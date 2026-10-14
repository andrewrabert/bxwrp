use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use clap::builder::{OsStringValueParser, TypedValueParser};

use crate::config::{
    Config, McpConfig, McpToolConfig, McpToolPath, MountConfig, MountModeConfig, PathsConfig,
    ToolConfig, ToolName, ToolPath,
};

#[derive(clap::Parser)]
#[command(
    name = crate::APP_NAME,
    version = crate::APP_VERSION,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub mode: Mode,

    #[command(flatten)]
    pub options: Options,
}

#[derive(clap::Subcommand)]
pub enum Mode {
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    Exec {
        #[arg(
            value_name = "ARG",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
    Mcp,
}

#[derive(clap::Subcommand)]
pub enum ConfigAction {
    Export,
    Schema,
    Validate,
}

#[derive(clap::Args)]
pub struct Options {
    #[arg(
        long,
        global = true,
        value_parser = OsStringValueParser::new().map(PathBuf::from),
        value_name = "PATH"
    )]
    pub config: Vec<PathBuf>,

    #[arg(long, global = true, value_enum, value_name = "BACKEND")]
    pub backend: Option<crate::backend::Kind>,

    #[arg(long, global = true, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    #[arg(long, global = true, value_name = "PATTERN")]
    pub include: Vec<String>,

    #[arg(long, global = true, value_name = "VFS_PATH")]
    pub cwd: Option<PathBuf>,

    #[arg(long, global = true, value_name = "NAME")]
    pub username: Option<String>,

    #[arg(long, global = true, value_name = "NAME")]
    pub hostname: Option<String>,

    #[arg(long, global = true, value_name = "NAME=VALUE")]
    pub env: Vec<EnvAssignment>,

    #[arg(
        short = 'm',
        long,
        global = true,
        value_name = "[HOST_PATH][:VFS_PATH[:rm|ro|rw]]"
    )]
    pub mount: Vec<MountSpec>,

    #[arg(long, global = true, value_name = "NAME[=PATH]")]
    pub tool: Vec<ToolSpec>,

    #[arg(
        long,
        global = true,
        value_name = "NAME,...",
        value_delimiter = ',',
        value_parser = clap::builder::TypedValueParser::try_map(
            clap::builder::StringValueParser::new(),
            ToolName::try_from,
        ),
    )]
    pub mcp_enabled: Option<Vec<ToolName>>,
}

impl Cli {
    #[must_use]
    pub fn read() -> Self {
        <Self as clap::Parser>::parse()
    }
}

impl Options {
    pub fn into_config(self) -> anyhow::Result<Config> {
        let mounts: Vec<MountConfig> = self.mount.into_iter().map(MountSpec::into_config).collect();

        let mut host_tools = BTreeMap::new();
        let mut mcp_tools = BTreeMap::new();
        for spec in self.tool {
            if host_tools.contains_key(&spec.name) {
                anyhow::bail!("tool {} is named twice", spec.name.as_ref());
            }
            mcp_tools.insert(
                spec.name.clone(),
                McpToolConfig {
                    path: McpToolPath::Program(spec.host.clone()),
                    cwd: None,
                    env: None,
                    description: None,
                    stdin: None,
                    arg: None,
                    args: None,
                },
            );
            host_tools.insert(spec.name, stated(spec.host));
        }

        Ok(Config {
            include: None,
            backend: self.backend,
            cwd: self.cwd,
            command_not_found: None,
            username: self.username,
            hostname: self.hostname,
            env: (!self.env.is_empty()).then(|| {
                self.env
                    .into_iter()
                    .map(|assignment| (assignment.key, assignment.value))
                    .collect()
            }),
            http: None,
            limits: None,
            paths: (!self.exclude.is_empty() || !self.include.is_empty() || !mounts.is_empty())
                .then(|| PathsConfig {
                    exclude: (!self.exclude.is_empty()).then_some(self.exclude),
                    include: (!self.include.is_empty()).then_some(self.include),
                    mounts: (!mounts.is_empty()).then_some(mounts),
                }),
            tools: (!host_tools.is_empty()).then_some(host_tools),
            mcp: (!mcp_tools.is_empty() || self.mcp_enabled.is_some()).then_some(McpConfig {
                instructions: None,
                enabled: self.mcp_enabled,
                tools: (!mcp_tools.is_empty()).then_some(mcp_tools),
            }),
        })
    }
}

const fn stated(path: ToolPath) -> ToolConfig {
    ToolConfig {
        path,
        description: None,
        args: None,
        cwd: None,
        env: None,
    }
}

#[derive(Clone)]
pub struct EnvAssignment {
    pub key: String,
    pub value: String,
}

impl FromStr for EnvAssignment {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.split_once('=') {
            Some((key, value)) if !key.is_empty() => Ok(Self {
                key: key.to_string(),
                value: value.to_string(),
            }),
            _ => Err(format!("{text}: expected <name>=<value>")),
        }
    }
}

#[derive(Clone)]
pub struct MountSpec {
    pub host: Option<PathBuf>,
    pub vfs: Option<PathBuf>,
    pub mode: MountModeConfig,
}

impl FromStr for MountSpec {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let [host, vfs, mode] = mount_fields(text)?;
        if host.is_empty() && vfs.is_empty() {
            return Err(format!("{text}: expected a host or vfs path"));
        }
        Ok(Self {
            host: (!host.is_empty()).then(|| PathBuf::from(host)),
            vfs: (!vfs.is_empty()).then(|| PathBuf::from(vfs)),
            mode: if mode.is_empty() {
                MountModeConfig::ReadWrite
            } else {
                mode.parse()?
            },
        })
    }
}

impl MountSpec {
    fn into_config(self) -> MountConfig {
        match self.mode {
            MountModeConfig::Remove => MountConfig {
                host: None,
                vfs: self.vfs.or(self.host),
                mode: MountModeConfig::Remove,
                exclude: None,
                include: None,
            },
            mode => MountConfig {
                host: self.host,
                vfs: self.vfs,
                mode,
                exclude: None,
                include: None,
            },
        }
    }
}

fn mount_fields(value: &str) -> Result<[String; 3], String> {
    let mut fields: [String; 3] = Default::default();
    let mut field = 0;
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        match character {
            ':' if field == fields.len() - 1 => {
                return Err(format!(
                    "{value}: expected [<host-path>][:<vfs-path>[:rm|ro|rw]]"
                ));
            }
            ':' => field += 1,
            '\\' => match chars.next() {
                Some(escaped @ (':' | '\\')) => fields[field].push(escaped),
                Some(escaped) => {
                    return Err(format!(
                        "{value}: unsupported escape \\{escaped}; expected \\: or \\\\"
                    ));
                }
                None => return Err(format!("{value}: trailing escape")),
            },
            character => fields[field].push(character),
        }
    }
    Ok(fields)
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: ToolName,
    pub host: ToolPath,
}

impl FromStr for ToolSpec {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (name, host) = match text.split_once('=') {
            Some((name, host)) if !host.is_empty() => (name, host),
            Some(_) => return Err(format!("{text}: expected <name>[=<path>]")),
            None => (text, text),
        };
        Ok(Self {
            name: ToolName::try_from(name.to_string())?,
            host: ToolPath::from(host.to_string()),
        })
    }
}
