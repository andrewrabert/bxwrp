use std::borrow::Borrow;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;

use rmcp::handler::server::common::schema_for_type;
use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, JsonObject, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt, tool_handler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::backend::Backend;
use crate::config::{McpTool, ParamSpec, RemainderSpec, Settings};
use crate::host_tool::HostExecutable;
use crate::host_tool::HostOutput;
use crate::host_tool::ScriptInput;
use crate::host_tool::Written;

const MAX_BYTES: usize = 256 * 1024;

pub async fn serve(sandbox: Box<dyn Backend>, settings: Settings) -> anyhow::Result<()> {
    let service = Server::new(sandbox, settings).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn run(sandbox: &dyn Backend, command: &str) -> anyhow::Result<String> {
    let result = sandbox
        .exec_script(command, "bash", Vec::new(), ScriptInput::Closed)
        .await?;
    let mut parts = Vec::new();
    if !result.stdout.is_empty() {
        parts.push(String::from_utf8_lossy(&result.stdout).into_owned());
    }
    if !result.stderr.is_empty() {
        parts.push(String::from_utf8_lossy(&result.stderr).into_owned());
    }
    if result.exit_code != 0 {
        parts.push(format!("(exit code {})", result.exit_code));
    }
    Ok(parts.join("\n"))
}

fn success(message: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(message.into())])
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn tool_output(result: HostOutput) -> CallToolResult {
    let mut parts = Vec::new();
    if !result.stdout.is_empty() {
        parts.push(String::from_utf8_lossy(&result.stdout).into_owned());
    }
    if !result.stderr.is_empty() {
        parts.push(String::from_utf8_lossy(&result.stderr).into_owned());
    }
    if result.exit_code != 0 {
        parts.push(format!("(exit code {})", result.exit_code));
    }
    let text = parts.join("\n");
    match result.exit_code {
        0 => success(text),
        _ => tool_error(text),
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(try_from = "String")]
#[schemars(with = "String")]
struct AbsolutePath(String);

impl TryFrom<String> for AbsolutePath {
    type Error = String;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        if path.starts_with('/') {
            Ok(Self(path))
        } else {
            Err(format!(
                "file_path must be an absolute path, not relative: {path:?}"
            ))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(try_from = "Option<u64>")]
#[schemars(with = "Option<u64>")]
struct LineOffset(NonZeroU64);

impl TryFrom<Option<u64>> for LineOffset {
    type Error = String;

    fn try_from(offset: Option<u64>) -> Result<Self, Self::Error> {
        NonZeroU64::new(offset.unwrap_or(1))
            .map(Self)
            .ok_or_else(|| String::from("offset: line numbers start at 1"))
    }
}

impl Default for LineOffset {
    fn default() -> Self {
        Self(NonZeroU64::MIN)
    }
}

impl LineOffset {
    fn is_start(&self) -> bool {
        self.0 == NonZeroU64::MIN
    }

    fn skip(&self) -> usize {
        usize::try_from(self.0.get() - 1).unwrap_or(usize::MAX)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(from = "Option<u64>")]
#[schemars(with = "Option<u64>")]
enum LineLimit {
    Rest,
    Lines(u64),
}

impl From<Option<u64>> for LineLimit {
    fn from(limit: Option<u64>) -> Self {
        limit.map_or(Self::Rest, Self::Lines)
    }
}

impl LineLimit {
    fn take(&self) -> usize {
        match self {
            Self::Rest => usize::MAX,
            Self::Lines(count) => usize::try_from(*count).unwrap_or(usize::MAX),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
struct OldText(String);

#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
struct NewText(String);

#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
struct ShellSource(String);

#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
struct FileContent(String);

#[derive(Deserialize, JsonSchema)]
struct BashArgs {
    command: ShellSource,
    #[allow(dead_code)]
    description: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    file_path: AbsolutePath,
    #[serde(default)]
    offset: LineOffset,
    #[serde(default)]
    limit: LineLimit,
}

impl Default for LineLimit {
    fn default() -> Self {
        Self::from(None)
    }
}

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    file_path: AbsolutePath,
    content: FileContent,
}

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    file_path: AbsolutePath,
    old_string: OldText,
    new_string: NewText,
    #[serde(default)]
    replace_all: bool,
}

enum ReplaceMode {
    Single,
    All,
}

#[derive(Deserialize, JsonSchema)]
#[serde(try_from = "EditArgs")]
#[schemars(with = "EditArgs")]
struct Replacement {
    file_path: AbsolutePath,
    old: OldText,
    new: NewText,
    mode: ReplaceMode,
}

impl TryFrom<EditArgs> for Replacement {
    type Error = String;

    fn try_from(args: EditArgs) -> Result<Self, Self::Error> {
        if args.old_string.0 == args.new_string.0 {
            return Err(String::from(
                "No changes to make: old_string and new_string are exactly the same.",
            ));
        }
        Ok(Self {
            file_path: args.file_path,
            old: args.old_string,
            new: args.new_string,
            mode: if args.replace_all {
                ReplaceMode::All
            } else {
                ReplaceMode::Single
            },
        })
    }
}

pub struct Server {
    sandbox: Box<dyn Backend>,
    settings: Settings,
    tool_router: ToolRouter<Self>,
}

type CallFuture<'a> = Pin<Box<dyn Future<Output = CallToolResult> + Send + 'a>>;

pub trait ToolDef: Send + Sync {
    fn name(&self) -> &str;
    fn attr(&self) -> Tool;
    fn call<'a>(&'a self, server: &'a Server, arguments: JsonObject) -> CallFuture<'a>;
}

#[derive(Clone)]
pub struct Def(Arc<dyn ToolDef>);

impl Def {
    pub fn new(def: impl ToolDef + 'static) -> Self {
        Self(Arc::new(def))
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.0.name()
    }

    fn route(self) -> ToolRoute<Server> {
        let attr = self.0.attr();
        ToolRoute::new_dyn(attr, move |context: ToolCallContext<'_, Server>| {
            let def = self.clone();
            Box::pin(async move {
                let arguments = context.arguments.clone().unwrap_or_default();
                Ok(def.0.call(context.service, arguments).await.into())
            })
        })
    }
}

impl PartialEq for Def {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for Def {}

impl Hash for Def {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name().hash(state);
    }
}

impl Borrow<str> for Def {
    fn borrow(&self) -> &str {
        self.name()
    }
}

fn parse_arguments<T: serde::de::DeserializeOwned>(arguments: JsonObject) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments)).map_err(|error| error.to_string())
}

fn typed<'a, T, F, Fut>(server: &'a Server, arguments: JsonObject, handler: F) -> CallFuture<'a>
where
    T: serde::de::DeserializeOwned + Send + 'a,
    F: FnOnce(&'a Server, T) -> Fut + Send + 'a,
    Fut: Future<Output = CallToolResult> + Send + 'a,
{
    Box::pin(async move {
        match parse_arguments::<T>(arguments) {
            Ok(args) => handler(server, args).await,
            Err(message) => tool_error(message),
        }
    })
}

pub struct Bash;

impl ToolDef for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn attr(&self) -> Tool {
        Tool::new(
            self.name().to_owned(),
            "Run a shell command. Standard output and standard error are \
             returned together, followed by the exit code when it is nonzero. \
             The bash and file tools share the same filesystem.",
            schema_for_type::<BashArgs>(),
        )
    }

    fn call<'a>(&'a self, server: &'a Server, arguments: JsonObject) -> CallFuture<'a> {
        typed(server, arguments, Server::bash)
    }
}

pub struct Read;

impl ToolDef for Read {
    fn name(&self) -> &str {
        "read"
    }

    fn attr(&self) -> Tool {
        Tool::new(
            self.name().to_owned(),
            "Read a text file with numbered lines. The path must be absolute. \
             Use offset and limit to select a range of lines.",
            schema_for_type::<ReadArgs>(),
        )
    }

    fn call<'a>(&'a self, server: &'a Server, arguments: JsonObject) -> CallFuture<'a> {
        typed(server, arguments, Server::read)
    }
}

pub struct Write;

impl ToolDef for Write {
    fn name(&self) -> &str {
        "write"
    }

    fn attr(&self) -> Tool {
        Tool::new(
            self.name().to_owned(),
            "Write a text file. The path must be absolute, and missing parent \
             directories are created automatically.",
            schema_for_type::<WriteArgs>(),
        )
    }

    fn call<'a>(&'a self, server: &'a Server, arguments: JsonObject) -> CallFuture<'a> {
        typed(server, arguments, Server::write)
    }
}

pub struct Edit;

impl ToolDef for Edit {
    fn name(&self) -> &str {
        "edit"
    }

    fn attr(&self) -> Tool {
        Tool::new(
            self.name().to_owned(),
            "Replace exact text in a file. By default the old text must occur \
             exactly once; set replace_all to replace every occurrence.",
            schema_for_type::<Replacement>(),
        )
    }

    fn call<'a>(&'a self, server: &'a Server, arguments: JsonObject) -> CallFuture<'a> {
        typed(server, arguments, Server::edit)
    }
}

pub struct Invocation(McpTool);

impl Invocation {
    #[must_use]
    pub fn new(tool: McpTool) -> Self {
        Self(tool)
    }
}

impl ToolDef for Invocation {
    fn name(&self) -> &str {
        self.0.name.as_ref()
    }

    fn attr(&self) -> Tool {
        Tool::new(
            self.name().to_owned(),
            self.0.description.clone().unwrap_or_default(),
            Arc::new(tool_schema(&self.0)),
        )
    }

    fn call<'a>(&'a self, _server: &'a Server, arguments: JsonObject) -> CallFuture<'a> {
        Box::pin(async move {
            let result = match Call::parse(&self.0, &arguments) {
                Ok(call) => call.execute().await,
                Err(message) => Err(message),
            };
            match result {
                Ok(output) => tool_output(output),
                Err(message) => tool_error(message),
            }
        })
    }
}

fn tool_schema(tool: &McpTool) -> JsonObject {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();
    let with_description = |mut property: Value, description: &Option<String>| {
        if let (Some(text), Some(object)) = (description, property.as_object_mut()) {
            object.insert(String::from("description"), json!(text));
        }
        property
    };
    if let Some(stdin) = &tool.stdin {
        properties.insert(
            String::from("stdin"),
            with_description(json!({"type": "string"}), &stdin.description),
        );
        if stdin.required {
            required.push(String::from("stdin"));
        }
    }
    for param in &tool.params {
        let property = if param.args.takes_value() {
            json!({"type": "string"})
        } else {
            json!({"type": "boolean", "default": false})
        };
        properties.insert(
            param.name.clone(),
            with_description(property, &param.description),
        );
    }
    if let Some(remainder) = &tool.remainder {
        properties.insert(
            String::from("args"),
            with_description(
                json!({"type": "array", "items": {"type": "string"}}),
                &remainder.description,
            ),
        );
    }
    let mut schema = serde_json::Map::new();
    schema.insert(String::from("type"), json!("object"));
    schema.insert(String::from("properties"), Value::Object(properties));
    schema.insert(String::from("required"), json!(required));
    schema.insert(String::from("additionalProperties"), json!(false));
    schema
}

fn scalar(name: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        other => Err(format!("{name}: expected a string, got {other}")),
    }
}

fn present<'a>(arguments: &'a JsonObject, name: &str) -> Option<&'a Value> {
    arguments.get(name).filter(|value| !value.is_null())
}

fn param_argv(param: &ParamSpec, value: Option<&Value>) -> Result<Vec<String>, String> {
    if param.args.takes_value() {
        return match value {
            None => Ok(param.default.elements().to_vec()),
            Some(value) => param
                .args
                .fill(&scalar(&param.name, value)?)
                .map_err(|error| format!("{error:#}")),
        };
    }
    let enabled = match value {
        None => false,
        Some(Value::Bool(enabled)) => *enabled,
        Some(other) => {
            return Err(format!("{}: expected a boolean, got {other}", param.name));
        }
    };
    let fragment = if enabled { &param.args } else { &param.default };
    Ok(fragment.elements().to_vec())
}

fn remainder_argv(spec: &RemainderSpec, value: Option<&Value>) -> Result<Vec<String>, String> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(other) => return Err(format!("args: expected an array, got {other}")),
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut argv = spec.prefix.clone();
    for item in items {
        argv.push(scalar("args", item)?);
    }
    Ok(argv)
}

struct Call<'a> {
    target: &'a HostExecutable,
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
}

impl<'a> Call<'a> {
    fn parse(tool: &'a McpTool, arguments: &JsonObject) -> Result<Self, String> {
        let mut argv: Vec<String> = Vec::new();
        for param in &tool.params {
            argv.extend(param_argv(param, present(arguments, &param.name))?);
        }
        if let Some(spec) = &tool.remainder {
            argv.extend(remainder_argv(spec, present(arguments, "args"))?);
        }
        let stdin = match (&tool.stdin, present(arguments, "stdin")) {
            (Some(spec), None) if spec.required => return Err(String::from("stdin is required")),
            (Some(_), Some(value)) => Some(scalar("stdin", value)?.into_bytes()),
            _ => None,
        };
        Ok(Self {
            target: &tool.target,
            argv,
            stdin,
        })
    }

    async fn execute(self) -> Result<HostOutput, String> {
        self.target
            .execute(self.argv, self.stdin)
            .await
            .map_err(|error| error.to_string())
    }
}

impl Server {
    fn resolve(&self, path: &str) -> std::path::PathBuf {
        bashkit::normalize_path(&self.settings.cwd.join(path))
    }
}

impl Server {
    fn new(sandbox: Box<dyn Backend>, mut settings: Settings) -> Self {
        let mut tool_router = ToolRouter::new();
        for def in settings.take_mcp_tools() {
            tool_router.add_route(def.route());
        }
        Self {
            sandbox,
            settings,
            tool_router,
        }
    }

    async fn bash(&self, args: BashArgs) -> CallToolResult {
        match run(&*self.sandbox, &args.command.0).await {
            Ok(output) => success(output),
            Err(e) => tool_error(e.to_string()),
        }
    }

    async fn read(&self, args: ReadArgs) -> CallToolResult {
        let path = self.resolve(&args.file_path.0);
        let text = match self.sandbox.read_text(&path).await {
            Ok(text) => text,
            Err(error) => return tool_error(error.to_string()),
        };
        if text.is_empty() {
            return tool_error("File exists but has empty contents.");
        }
        let whole_file = args.offset.is_start() && matches!(args.limit, LineLimit::Rest);
        if whole_file {
            let size = text.len();
            if size > MAX_BYTES {
                return tool_error(format!(
                    "File content ({:.1}KB) exceeds maximum allowed size \
                     ({}KB). Use offset and limit parameters to read \
                     specific portions of the file, or search for specific \
                     content instead of reading the whole file.",
                    size as f64 / 1024.0,
                    MAX_BYTES / 1024,
                ));
            }
        }
        let numbered: Vec<String> = text
            .lines()
            .enumerate()
            .skip(args.offset.skip())
            .take(args.limit.take())
            .map(|(i, line)| format!("{:>6}\t{}", i + 1, line))
            .collect();
        success(numbered.join("\n"))
    }

    async fn write(&self, args: WriteArgs) -> CallToolResult {
        let path = self.resolve(&args.file_path.0);
        match self.sandbox.write_text(&path, &args.content.0).await {
            Ok(Written::Updated) => success(format!(
                "The file {} has been updated successfully.",
                path.display()
            )),
            Ok(Written::Created) => {
                success(format!("File created successfully at: {}", path.display()))
            }
            Err(error) => tool_error(error.to_string()),
        }
    }

    async fn edit(&self, edit: Replacement) -> CallToolResult {
        let path = self.resolve(&edit.file_path.0);
        let text = match self.sandbox.read_text(&path).await {
            Ok(text) => text,
            Err(error) => return tool_error(error.to_string()),
        };
        let count = text.matches(&edit.old.0).count();
        if count == 0 {
            return tool_error("String to replace not found in file.");
        }
        if let (ReplaceMode::Single, 2..) = (&edit.mode, count) {
            return tool_error(format!(
                "Found {count} matches of the string to replace, but \
                 replace_all is false. To replace all occurrences, set \
                 replace_all to true. To replace only one occurrence, \
                 please provide more context to uniquely identify the \
                 instance."
            ));
        }
        let updated = text.replace(&edit.old.0, &edit.new.0);
        match self.sandbox.write_text(&path, &updated).await {
            Ok(_) => success(format!(
                "The file {} has been updated successfully.",
                path.display()
            )),
            Err(error) => tool_error(error.to_string()),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("bxwrp", env!("CARGO_PKG_VERSION")));
        if let Some(instructions) = self.settings.instructions() {
            info = info.with_instructions(instructions);
        }
        info
    }
}
