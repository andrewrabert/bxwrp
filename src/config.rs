use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;

use crate::host_tool::HostExecutable;
use crate::mcp::{Bash, Def, Edit, Invocation, Read, Write};
use crate::mount::PathFilter;
use crate::template::ArgsTemplate;

const GLOBAL_CONFIG_FILE_NAME: &str = "config.yaml";

#[must_use]
fn global_config_dir() -> Option<PathBuf> {
    use etcetera::BaseStrategy as _;

    etcetera::choose_base_strategy()
        .ok()
        .map(|strategy| strategy.config_dir().join("bxwrp"))
}

const LOCAL_CONFIG_FILE_NAME: &str = ".bxwrp.yaml";

pub struct Host {
    cwd: PathBuf,
    paths: Vec<PathBuf>,
    config: Config,
}

impl Host {
    pub fn load(cli_paths: &[PathBuf]) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().context("the directory bxwrp runs in is unreadable")?;
        let paths: Vec<PathBuf> = if cli_paths.is_empty() {
            match std::env::var_os("BXWRP_CONFIG") {
                Some(value) if !value.is_empty() => vec![PathBuf::from(value)],
                Some(_) => Vec::new(),
                None => global_config_dir()
                    .map(|dir| dir.join(GLOBAL_CONFIG_FILE_NAME))
                    .filter(|path| path.is_file())
                    .into_iter()
                    .chain(
                        cwd.ancestors()
                            .map(|dir| dir.join(LOCAL_CONFIG_FILE_NAME))
                            .find(|path| path.is_file()),
                    )
                    .collect(),
            }
        } else {
            cli_paths
                .iter()
                .filter(|path| !path.as_os_str().is_empty())
                .cloned()
                .collect()
        };
        let mut config = Config::default();
        for path in &paths {
            config = config.merge(Config::load(path)?);
        }
        Ok(Self { cwd, paths, config })
    }

    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    #[must_use]
    pub fn merge(self, overlay: Config) -> Config {
        self.config.merge(overlay)
    }

    pub fn resolve(self, overlay: Config) -> anyhow::Result<Settings> {
        let Self { cwd, config, .. } = self;
        Settings::resolve(config.merge(overlay), &cwd)
    }
}

pub const SANDBOX_USERNAME: &str = "sandbox";

pub const SANDBOX_HOSTNAME: &str = "sandbox";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(from = "String", into = "String")]
#[schemars(with = "String")]
pub enum ToolPath {
    Program(String),
    Path(PathBuf),
}

impl From<String> for ToolPath {
    fn from(value: String) -> Self {
        if value.contains('/') {
            Self::Path(PathBuf::from(value))
        } else {
            Self::Program(value)
        }
    }
}

impl From<ToolPath> for String {
    fn from(path: ToolPath) -> Self {
        match path {
            ToolPath::Program(program) => program,
            ToolPath::Path(path) => path.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Http {
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<crate::backend::Kind>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_not_found: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<LimitsConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<PathsConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<BTreeMap<ToolName, ToolConfig>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpConfig>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<MountConfig>>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Vec<ToolName>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<BTreeMap<ToolName, McpToolConfig>>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpToolConfig {
    pub path: McpToolPath,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<StdinConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg: Option<indexmap::IndexMap<ToolName, ArgConfig>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<RemainderConfig>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum McpToolPath {
    Program(ToolPath),
    Argv(Vec<String>),
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StdinConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArgConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    pub args: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemainderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<Vec<String>>,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String", extend("pattern" = "^[^/]+$"))]
pub struct ToolName(String);

impl TryFrom<String> for ToolName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        if name.is_empty() {
            return Err(String::from("a tool name is empty"));
        }
        if name.contains('/') {
            return Err(format!("{name}: a tool name holds no /"));
        }
        Ok(Self(name))
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<ToolName> for String {
    fn from(name: ToolName) -> Self {
        name.0
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct EnvConfig(BTreeMap<String, String>);

impl<'de> serde::Deserialize<'de> for EnvConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Names;

        impl<'de> serde::de::Visitor<'de> for Names {
            type Value = EnvConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of environment names to string values")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<EnvConfig, M::Error> {
                let mut names = BTreeMap::new();
                while let Some(name) = map.next_key::<String>()? {
                    let value = map.next_value::<serde_json::Value>()?;
                    match value {
                        serde_json::Value::String(value) => {
                            names.insert(name, value);
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "{name}: expected a string, found {other}"
                            )));
                        }
                    }
                }
                Ok(EnvConfig(names))
            }
        }

        deserializer.deserialize_map(Names)
    }
}

impl EnvConfig {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl IntoIterator for EnvConfig {
    type Item = (String, String);
    type IntoIter = std::collections::btree_map::IntoIter<String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<(String, String)> for EnvConfig {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(pairs: I) -> Self {
        Self(pairs.into_iter().collect())
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub enabled: bool,
}

pub const DEFAULT_MAX_INPUT_BYTES: u64 = 100_000_000_000;

pub const DEFAULT_COMMAND_NOT_FOUND: &str = "{% if config.tools is defined %}\
{% for name, tool in config.tools | items %}\
{{ name }}{% if tool.description is defined %}: {{ tool.description }}{% endif %}\n\
{% endfor %}{% endif %}\
help: list builtin commands";

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_bytes: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub vfs: Option<PathBuf>,

    pub mode: MountModeConfig,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub enum MountModeConfig {
    #[serde(rename = "ro")]
    ReadOnly,

    #[serde(rename = "rw")]
    ReadWrite,

    #[serde(rename = "rm")]
    Remove,
}

impl std::str::FromStr for MountModeConfig {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        use serde::de::IntoDeserializer as _;

        let deserializer: serde::de::value::StrDeserializer<serde::de::value::Error> =
            text.into_deserializer();
        serde::Deserialize::deserialize(deserializer).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub path: ToolPath,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvConfig>,
}

fn merge_env(base: Option<EnvConfig>, overlay: Option<EnvConfig>) -> Option<EnvConfig> {
    match (base, overlay) {
        (None, other) | (other, None) => other,
        (Some(base), Some(overlay)) => {
            let mut merged: BTreeMap<String, String> = base.into_iter().collect();
            merged.extend(overlay);
            Some(merged.into_iter().collect())
        }
    }
}

fn merge_lists<T>(base: Option<Vec<T>>, overlay: Option<Vec<T>>) -> Option<Vec<T>> {
    match (base, overlay) {
        (None, other) | (other, None) => other,
        (Some(mut base), Some(overlay)) => {
            base.extend(overlay);
            Some(base)
        }
    }
}

fn merge_tools(
    base: Option<BTreeMap<ToolName, ToolConfig>>,
    overlay: Option<BTreeMap<ToolName, ToolConfig>>,
) -> Option<BTreeMap<ToolName, ToolConfig>> {
    match (base, overlay) {
        (None, other) | (other, None) => other,
        (Some(mut base), Some(overlay)) => {
            base.extend(overlay);
            Some(base)
        }
    }
}

fn merge_paths(base: Option<PathsConfig>, overlay: Option<PathsConfig>) -> Option<PathsConfig> {
    match (base, overlay) {
        (None, other) | (other, None) => other,
        (Some(base), Some(overlay)) => Some(PathsConfig {
            exclude: merge_lists(base.exclude, overlay.exclude),
            include: merge_lists(base.include, overlay.include),
            mounts: merge_lists(base.mounts, overlay.mounts),
        }),
    }
}

fn merge_limits(base: Option<LimitsConfig>, overlay: Option<LimitsConfig>) -> Option<LimitsConfig> {
    match (base, overlay) {
        (None, other) | (other, None) => other,
        (Some(base), Some(overlay)) => Some(LimitsConfig {
            max_input_bytes: overlay.max_input_bytes.or(base.max_input_bytes),
        }),
    }
}

fn merge_mcp(base: Option<McpConfig>, overlay: Option<McpConfig>) -> Option<McpConfig> {
    match (base, overlay) {
        (None, other) | (other, None) => other,
        (Some(base), Some(overlay)) => {
            let instructions = overlay.instructions.or(base.instructions);
            let enabled = overlay.enabled.or(base.enabled);
            let mut tools = base.tools.unwrap_or_default();
            for (name, tool) in overlay.tools.unwrap_or_default() {
                let description = tool
                    .description
                    .or_else(|| tools.get(&name).and_then(|kept| kept.description.clone()));
                tools.insert(
                    name,
                    McpToolConfig {
                        description,
                        ..tool
                    },
                );
            }
            Some(McpConfig {
                instructions,
                enabled,
                tools: Some(tools),
            })
        }
    }
}

impl Config {
    #[must_use]
    fn merge(self, overlay: Self) -> Self {
        Self {
            include: None,
            backend: overlay.backend.or(self.backend),
            cwd: overlay.cwd.or(self.cwd),
            username: overlay.username.or(self.username),
            hostname: overlay.hostname.or(self.hostname),
            command_not_found: overlay.command_not_found.or(self.command_not_found),
            env: merge_env(self.env, overlay.env),
            http: overlay.http.or(self.http),
            limits: merge_limits(self.limits, overlay.limits),
            paths: merge_paths(self.paths, overlay.paths),
            tools: merge_tools(self.tools, overlay.tools),
            mcp: merge_mcp(self.mcp, overlay.mcp),
        }
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(serde_saphyr::from_str(text)?)
    }

    fn load(path: &Path) -> anyhow::Result<Self> {
        let mut loading = Vec::new();
        Self::load_inner(path, &mut loading)
    }

    fn load_inner(path: &Path, loading: &mut Vec<PathBuf>) -> anyhow::Result<Self> {
        let canonical = path
            .canonicalize()
            .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
        if loading.contains(&canonical) {
            let cycle: Vec<String> = loading
                .iter()
                .chain(std::iter::once(&canonical))
                .map(|path| path.display().to_string())
                .collect();
            anyhow::bail!("config includes form a cycle: {}", cycle.join(" -> "));
        }
        loading.push(canonical.clone());
        let text = std::fs::read_to_string(&canonical)
            .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
        let mut parsed =
            Self::parse(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
        let included = parsed.include.take();
        let base_dir = canonical.parent().unwrap_or(Path::new("."));
        let mut config = Self::default();
        for entry in included.into_iter().flatten() {
            let entry = bashkit::normalize_path(&base_dir.join(entry));
            let layer = Self::load_inner(&entry, loading)
                .with_context(|| format!("{}: include", path.display()))?;
            config = config.merge(layer);
        }
        loading.pop();
        Ok(config.merge(parsed))
    }

    pub fn render(&self) -> anyhow::Result<String> {
        let mut text = serde_saphyr::to_string(self)?;
        text.truncate(text.trim_end_matches('\n').len());
        text.push('\n');
        Ok(text)
    }

    pub fn json_schema() -> anyhow::Result<String> {
        let schema = schemars::schema_for!(Self);
        Ok(serde_json::to_string_pretty(&schema)?)
    }
}

pub struct Layer {
    pub target: PathBuf,
    pub source: Source,
    pub filter: Option<Arc<PathFilter>>,
}

pub enum Source {
    HostDir { host: PathBuf, mode: MountMode },
    HostFile { host: PathBuf, mode: MountMode },
    Memory { mode: MountMode },
    Remove,
}

#[derive(Clone)]
pub struct McpTool {
    pub name: ToolName,
    pub target: HostExecutable,
    pub description: Option<String>,
    pub stdin: Option<StdinSpec>,
    pub params: Vec<ParamSpec>,
    pub remainder: Option<RemainderSpec>,
}

#[derive(Clone)]
pub struct StdinSpec {
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Clone)]
pub struct ParamSpec {
    pub name: String,
    pub description: Option<String>,
    pub args: ArgsTemplate,
    pub default: ArgsTemplate,
}

#[derive(Clone)]
pub struct RemainderSpec {
    pub description: Option<String>,
    pub prefix: Vec<String>,
}

fn mcp_tools(config: Option<&McpConfig>) -> anyhow::Result<Vec<Def>> {
    let mut available: HashSet<Def> = [
        Def::new(Bash),
        Def::new(Read),
        Def::new(Write),
        Def::new(Edit),
    ]
    .into_iter()
    .collect();
    let stated = config.and_then(|mcp| mcp.tools.as_ref());
    for (name, entry) in stated.into_iter().flatten() {
        let def = Def::new(Invocation::new(resolve_tool(name, entry)?));
        anyhow::ensure!(
            available.insert(def),
            "mcp.tools: {} shadows a built-in tool",
            name.as_ref()
        );
    }
    let Some(enabled) = config.and_then(|mcp| mcp.enabled.as_ref()) else {
        return Ok(available.into_iter().collect());
    };
    enabled
        .iter()
        .map(|name| {
            available
                .take(name.as_ref())
                .with_context(|| format!("mcp.enabled: unknown tool `{}`", name.as_ref()))
        })
        .collect()
}

fn resolve_tool(name: &ToolName, entry: &McpToolConfig) -> anyhow::Result<McpTool> {
    let (path, fixed_args) = match entry.path.clone() {
        McpToolPath::Program(path) => (path, Vec::new()),
        McpToolPath::Argv(argv) => {
            let mut argv = argv.into_iter();
            let program = argv
                .next()
                .with_context(|| format!("tool {}: path names no program", name.as_ref()))?;
            (ToolPath::from(program), argv.collect())
        }
    };
    let executable = host_executable(
        name,
        ToolConfig {
            path,
            description: None,
            args: Some(fixed_args),
            cwd: entry.cwd.clone(),
            env: entry.env.clone(),
        },
    )?;
    let mut params = Vec::new();
    for (param_name, param) in entry.arg.iter().flatten() {
        let param_name = param_name.as_ref();
        anyhow::ensure!(
            param_name != "stdin" && param_name != "args",
            "tool {}: arg {param_name} shadows the {param_name} input",
            name.as_ref(),
        );
        let context = || format!("tool {}: arg {param_name}", name.as_ref());
        let default =
            ArgsTemplate::new(param.default.clone().unwrap_or_default()).with_context(context)?;
        anyhow::ensure!(
            !default.takes_value(),
            "tool {}: arg {param_name}: default holds {{{{ value }}}} \
             but no value exists when unset",
            name.as_ref(),
        );
        let args = ArgsTemplate::new(param.args.clone()).with_context(context)?;
        params.push(ParamSpec {
            name: param_name.to_string(),
            description: param.description.clone(),
            args,
            default,
        });
    }
    Ok(McpTool {
        name: name.clone(),
        target: executable,
        description: entry.description.clone(),
        stdin: entry.stdin.as_ref().map(|stdin| StdinSpec {
            description: stdin.description.clone(),
            required: stdin.required,
        }),
        params,
        remainder: entry.args.as_ref().map(|remainder| RemainderSpec {
            description: remainder.description.clone(),
            prefix: remainder.prefix.clone().unwrap_or_default(),
        }),
    })
}

fn host_executable(name: &ToolName, entry: ToolConfig) -> anyhow::Result<HostExecutable> {
    let ToolConfig {
        path,
        description: _,
        args,
        cwd,
        env,
    } = entry;
    let executable = match path {
        ToolPath::Program(program) => PathBuf::from(program),
        ToolPath::Path(path) => path,
    };
    let cwd = match cwd {
        None => None,
        Some(cwd) => {
            let resolved = cwd
                .canonicalize()
                .with_context(|| format!("tool {}: cwd {}", name.as_ref(), cwd.display()))?;
            let meta = std::fs::metadata(&resolved)
                .with_context(|| format!("tool {}: cwd {}", name.as_ref(), resolved.display()))?;
            anyhow::ensure!(
                meta.is_dir(),
                "tool {}: cwd {} is not a directory",
                name.as_ref(),
                resolved.display()
            );
            Some(resolved)
        }
    };
    Ok(HostExecutable {
        path: executable,
        args: args.unwrap_or_default(),
        cwd,
        env: env
            .map(EnvConfig::into_iter)
            .into_iter()
            .flatten()
            .collect(),
        clear_env: false,
    })
}

fn host_tools(
    config: Option<&BTreeMap<ToolName, ToolConfig>>,
) -> anyhow::Result<BTreeMap<ToolName, HostExecutable>> {
    let mut host_tools: BTreeMap<ToolName, HostExecutable> = BTreeMap::new();
    for (name, entry) in config.into_iter().flatten() {
        let executable = host_executable(name, entry.clone())?;
        host_tools.insert(name.clone(), executable);
    }
    Ok(host_tools)
}

fn mount_layers(
    paths: Option<&PathsConfig>,
    vfs_path: impl Fn(&Path) -> PathBuf + Copy,
) -> anyhow::Result<Vec<Layer>> {
    let host_layer = |host: &PathBuf,
                      vfs: Option<&PathBuf>,
                      mode: MountMode,
                      filter: Option<Arc<PathFilter>>|
     -> anyhow::Result<Layer> {
        let unapplied = || {
            format!(
                "mount of {} at {}",
                host.display(),
                vfs.unwrap_or(host).display()
            )
        };
        let canonical = host
            .canonicalize()
            .with_context(|| format!("{} was not applied: unresolvable", unapplied()))?;
        let meta = std::fs::metadata(&canonical)
            .with_context(|| format!("{} was not applied: unreadable", unapplied()))?;
        let target = vfs_path(vfs.unwrap_or(host));
        let source = if meta.is_dir() {
            Source::HostDir {
                host: canonical,
                mode,
            }
        } else {
            anyhow::ensure!(
                filter.is_none(),
                "{}: include/exclude apply to a directory mount, not a file",
                unapplied()
            );
            Source::HostFile {
                host: canonical,
                mode,
            }
        };
        Ok(Layer {
            target,
            source,
            filter,
        })
    };

    let mut layers: Vec<Layer> = Vec::new();
    for entry in paths
        .and_then(|paths| paths.mounts.as_ref())
        .into_iter()
        .flatten()
    {
        let filter = PathFilter::new(
            entry.exclude.as_deref().unwrap_or_default(),
            entry.include.as_deref().unwrap_or_default(),
        )
        .context("a mounts entry holds an unparsable include/exclude pattern")?
        .map(Arc::new);
        let memory_layer = |vfs: &PathBuf, mode: MountMode| -> anyhow::Result<Layer> {
            anyhow::ensure!(
                filter.is_none(),
                "include/exclude apply to a host mount, not the memory mount at {}",
                vfs.display()
            );
            Ok(Layer {
                target: vfs_path(vfs),
                source: Source::Memory { mode },
                filter: None,
            })
        };
        let layer = match (&entry.host, entry.vfs.as_ref(), entry.mode) {
            (Some(host), _, MountModeConfig::Remove) => {
                anyhow::bail!("mode rm names a vfs path, not host {}", host.display());
            }
            (Some(host), vfs, MountModeConfig::ReadOnly) => {
                host_layer(host, vfs, MountMode::ReadOnly, filter.clone())?
            }
            (Some(host), vfs, MountModeConfig::ReadWrite) => {
                host_layer(host, vfs, MountMode::ReadWrite, filter.clone())?
            }
            (None, None, _) => {
                anyhow::bail!("a mounts entry names neither host nor vfs");
            }
            (None, Some(vfs), MountModeConfig::ReadOnly) => memory_layer(vfs, MountMode::ReadOnly)?,
            (None, Some(vfs), MountModeConfig::ReadWrite) => {
                memory_layer(vfs, MountMode::ReadWrite)?
            }
            (None, Some(vfs), MountModeConfig::Remove) => {
                anyhow::ensure!(
                    filter.is_none(),
                    "include/exclude apply to a host mount, not the removal of {}",
                    vfs.display()
                );
                Layer {
                    target: vfs_path(vfs),
                    source: Source::Remove,
                    filter: None,
                }
            }
        };
        layers.push(layer);
    }
    Ok(layers)
}

pub struct Settings {
    backend: crate::backend::Kind,

    #[cfg(not(target_os = "android"))]
    http: Http,

    username: String,
    hostname: String,
    env: EnvConfig,
    network: Option<bashkit::NetworkAllowlist>,
    max_input_bytes: u64,
    instructions: Option<String>,
    command_not_found: String,
    host_tools: BTreeMap<ToolName, HostExecutable>,
    mcp_tools: Vec<Def>,
    pub(crate) cwd: PathBuf,
    pub(crate) layers: Vec<Layer>,
    pub(crate) filter: Option<Arc<PathFilter>>,
}

impl Settings {
    #[must_use]
    pub fn network(&self) -> Option<bashkit::NetworkAllowlist> {
        self.network.clone()
    }

    fn resolve(config: Config, host_cwd: &Path) -> anyhow::Result<Self> {
        let config_value = minijinja::Value::from(minijinja::value::Serde(&config));
        let command_not_found = crate::template::render_message(
            config
                .command_not_found
                .as_deref()
                .unwrap_or(DEFAULT_COMMAND_NOT_FOUND),
            &config_value,
        )
        .context("command_not_found")?;
        let vfs_path = |path: &Path| bashkit::normalize_path(&host_cwd.join(path));
        let cwd = vfs_path(config.cwd.as_deref().unwrap_or(Path::new(".")));
        let paths = config.paths.as_ref();
        let layers = mount_layers(paths, vfs_path)?;

        let filter = PathFilter::new(
            paths
                .and_then(|paths| paths.exclude.as_deref())
                .unwrap_or_default(),
            paths
                .and_then(|paths| paths.include.as_deref())
                .unwrap_or_default(),
        )
        .context("an include/exclude pattern is unparsable")?
        .map(Arc::new);

        let host_tools = host_tools(config.tools.as_ref())?;
        let mcp_tools = mcp_tools(config.mcp.as_ref())?;

        let network = match config.http {
            Some(HttpConfig { enabled: false }) => None,
            Some(HttpConfig { enabled: true }) => {
                Some(bashkit::NetworkAllowlist::allow_all().block_private_ips(true))
            }
            None => Some(bashkit::NetworkAllowlist::new().block_private_ips(true)),
        };

        #[cfg(not(target_os = "android"))]
        let http = match config.http {
            Some(HttpConfig { enabled: true }) => Http::Enabled,
            Some(HttpConfig { enabled: false }) | None => Http::Disabled,
        };

        Ok(Self {
            backend: config.backend.unwrap_or_default(),
            #[cfg(not(target_os = "android"))]
            http,
            username: config.username.unwrap_or_else(|| SANDBOX_USERNAME.into()),
            hostname: config.hostname.unwrap_or_else(|| SANDBOX_HOSTNAME.into()),
            env: config.env.unwrap_or_default(),
            network,
            max_input_bytes: config
                .limits
                .and_then(|limits| limits.max_input_bytes)
                .unwrap_or(DEFAULT_MAX_INPUT_BYTES),
            instructions: config.mcp.and_then(|mcp| mcp.instructions),
            command_not_found,
            host_tools,
            mcp_tools,
            cwd,
            layers,
            filter,
        })
    }

    pub(crate) const fn backend(&self) -> crate::backend::Kind {
        self.backend
    }

    #[cfg(not(target_os = "android"))]
    pub(crate) const fn http(&self) -> Http {
        self.http
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn hostname(&self) -> &str {
        &self.hostname
    }

    pub(crate) fn env(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub(crate) const fn max_input_bytes(&self) -> u64 {
        self.max_input_bytes
    }

    pub(crate) fn builtins(&self) -> impl Iterator<Item = (&ToolName, &HostExecutable)> {
        self.host_tools.iter()
    }

    pub fn host_tool_names(&self) -> impl Iterator<Item = &str> {
        self.host_tools.keys().map(std::convert::AsRef::as_ref)
    }

    pub fn take_mcp_tools(&mut self) -> Vec<Def> {
        std::mem::take(&mut self.mcp_tools)
    }

    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    #[must_use]
    pub fn command_not_found(&self) -> &str {
        &self.command_not_found
    }
}
