use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use tokio::sync::Mutex;

use super::Backend;
use crate::config::{MountMode, Settings, Source};
use crate::host_tool::{HostOutput, HostTool, ScriptInput, Written};
use crate::mount::{FilterFs, Layer, MountTable, PathFilter, SingleFile};

fn filtered(
    fs: Arc<dyn bashkit::FileSystem>,
    filter: Option<&Arc<PathFilter>>,
) -> Arc<dyn bashkit::FileSystem> {
    match filter {
        Some(filter) => Arc::new(FilterFs::new(fs, Arc::clone(filter))),
        None => fs,
    }
}

async fn real_fs(host: &Path, mode: MountMode) -> anyhow::Result<Arc<dyn bashkit::FileSystem>> {
    let mode = match mode {
        MountMode::ReadOnly => bashkit::RealFsMode::ReadOnly,
        MountMode::ReadWrite => bashkit::RealFsMode::ReadWrite,
    };
    let real = bashkit::RealFs::open(host, mode).await?;
    Ok(Arc::new(bashkit::PosixFs::new(real)))
}

fn view(path: &Path) -> impl Fn() -> String + use<> {
    let path = path.to_path_buf();
    move || format!("view of {}", path.display())
}

async fn build_layers(
    settings: &Settings,
    memory: &Arc<dyn bashkit::FileSystem>,
) -> anyhow::Result<Vec<Layer>> {
    let readonly: Arc<dyn bashkit::FileSystem> =
        Arc::new(bashkit::ReadOnlyFs::new(Arc::clone(memory)));
    let mut layers: Vec<Layer> = Vec::new();
    for layer in &settings.layers {
        let target = &layer.target;
        let applied = |source: &'static str| {
            let target = target.clone();
            move || format!("{source} at {}", target.display())
        };
        layers.push(match &layer.source {
            Source::Remove => Layer::removed(target),
            Source::Memory { mode } => {
                if matches!(mode, MountMode::ReadWrite)
                    && !memory.exists(target).await.with_context(view(target))?
                {
                    memory
                        .mkdir(target, true)
                        .await
                        .with_context(view(target))?;
                }
                match mode {
                    MountMode::ReadOnly => Layer::shared(target, Arc::clone(&readonly)),
                    MountMode::ReadWrite => Layer::shared(target, Arc::clone(memory)),
                }
            }
            Source::HostDir { host, mode } => {
                let real = real_fs(host, *mode)
                    .await
                    .with_context(applied("mount of a host directory"))?;
                Layer::mounted(target, filtered(real, layer.filter.as_ref()))
            }
            Source::HostFile { host, mode } => {
                let parent = host
                    .parent()
                    .with_context(applied("mount of a host file"))?
                    .to_path_buf();
                let name = Path::new(
                    host.file_name()
                        .with_context(applied("mount of a host file"))?,
                );
                let real = real_fs(&parent, *mode)
                    .await
                    .with_context(applied("mount of a host file"))?;
                Layer::mounted(target, Arc::new(SingleFile::new(real, name)))
            }
        });
    }
    Ok(layers)
}

struct NotFound {
    name: String,
    suffix: Arc<str>,
}

#[bashkit::async_trait]
impl bashkit::Builtin for NotFound {
    async fn execute(
        &self,
        _ctx: bashkit::BuiltinContext<'_>,
    ) -> bashkit::Result<bashkit::ExecResult> {
        Ok(bashkit::ExecResult::err(
            format!("bash: {}: command not found\n{}", self.name, self.suffix),
            127,
        ))
    }
}

struct NotFoundResolver {
    suffix: Arc<str>,
}

impl bashkit::CommandResolver for NotFoundResolver {
    fn resolve(&self, name: &str) -> Option<Arc<dyn bashkit::Builtin>> {
        Some(Arc::new(NotFound {
            name: name.to_string(),
            suffix: Arc::clone(&self.suffix),
        }))
    }
}

fn not_found_suffix(settings: &Settings) -> String {
    let message = settings.command_not_found().trim_end();
    if message.is_empty() {
        String::new()
    } else {
        format!("{message}\n")
    }
}

pub(super) struct Bashkit {
    bash: Mutex<bashkit::Bash>,
    fs: Arc<dyn bashkit::FileSystem>,
}

impl Bashkit {
    pub(super) async fn new(settings: &Settings) -> anyhow::Result<Self> {
        let memory: Arc<dyn bashkit::FileSystem> = Arc::new(bashkit::InMemoryFs::new());
        if !memory
            .exists(&settings.cwd)
            .await
            .with_context(view(&settings.cwd))?
        {
            memory
                .mkdir(&settings.cwd, true)
                .await
                .with_context(view(&settings.cwd))?;
        }

        let layers = build_layers(settings, &memory).await?;
        let table: Arc<dyn bashkit::FileSystem> =
            Arc::new(MountTable::new(Arc::clone(&memory), layers));
        let table = filtered(table, settings.filter.as_ref());

        let profile = bashkit::ExecutionProfile::named(bashkit::ExecutionProfileName::Interactive);
        let limits = profile
            .execution_limits()
            .clone()
            .max_aggregate_input_bytes(settings.max_input_bytes())
            .max_work_units(u64::MAX);
        let mut builder = bashkit::Bash::builder()
            .profile(profile)
            .limits(limits)
            .hostname(settings.hostname())
            .username(settings.username())
            .cwd(settings.cwd.clone())
            .fs(table)
            .rootfs(false);
        for (key, value) in settings.env() {
            builder = builder.env(key, value);
        }
        for (name, target) in settings.builtins() {
            let host_tool = HostTool::new(name.as_ref().to_string(), target.clone());
            builder = builder.builtin(name.as_ref(), Box::new(host_tool));
        }
        if let Some(network) = settings.network() {
            builder = builder.network(network);
        }
        builder = builder.command_resolver(Arc::new(NotFoundResolver {
            suffix: not_found_suffix(settings).into(),
        }));
        let mut build_only_env = Vec::new();
        builder = builder.cpython();
        builder = builder.sqlite().env("BASHKIT_ALLOW_INPROCESS_SQLITE", "1");
        build_only_env.push("BASHKIT_ALLOW_INPROCESS_SQLITE");
        builder = builder.typescript();
        let mut bash = builder.build();

        let mut state = bash.shell_state();
        state.env.clear();
        for (key, value) in settings.env() {
            state.env.insert(key.to_string(), value.to_string());
        }
        for key in build_only_env {
            state.variables.remove(key);
        }
        bash.restore_shell_state(&state);

        let fs = bash.fs();
        Ok(Self {
            bash: Mutex::new(bash),
            fs,
        })
    }

    pub async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> bashkit::Result<bashkit::ExecResult> {
        let mut options = bashkit::ExecOptions::new().arg0(arg0).positional(args);
        if let Some(stdin) = stdin {
            options = options.stdin(stdin);
        }
        self.bash
            .lock()
            .await
            .exec_with_options(script, options)
            .await
    }

    pub fn path(&self, path: &str) -> BashkitPath {
        BashkitPath {
            fs: Arc::clone(&self.fs),
            path: bashkit::normalize_path(Path::new(path)),
        }
    }
}

fn vfs(kit: &Bashkit, path: &Path) -> BashkitPath {
    kit.path(&path.to_string_lossy())
}

#[async_trait]
impl Backend for Bashkit {
    async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: ScriptInput,
    ) -> anyhow::Result<HostOutput> {
        let stdin = match stdin {
            ScriptInput::Closed => None,
            ScriptInput::Bytes(bytes) => Some(bytes),
        };
        let result = Self::exec_script(self, script, arg0, args, stdin).await?;
        let exit_code = match result.control_flow {
            bashkit::ControlFlow::Exit(code) => code,
            _ => result.exit_code,
        };
        Ok(HostOutput {
            stdout: result.stdout.into_bytes(),
            stderr: result.stderr.into_bytes(),
            exit_code,
        })
    }

    async fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        vfs(self, path).read_text().await
    }

    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<Written> {
        let path = vfs(self, path);
        let existed = path.exists().await?;
        path.parent().mkdir_parents().await?;
        path.write_text(content).await?;
        Ok(if existed {
            Written::Updated
        } else {
            Written::Created
        })
    }
}

pub struct BashkitPath {
    fs: Arc<dyn bashkit::FileSystem>,
    pub path: PathBuf,
}

impl BashkitPath {
    #[must_use]
    pub fn parent(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            path: self.path.parent().unwrap_or(&self.path).to_path_buf(),
        }
    }

    pub async fn exists(&self) -> anyhow::Result<bool> {
        Ok(self.fs.exists(&self.path).await?)
    }

    pub async fn read_text(&self) -> anyhow::Result<String> {
        self.require_file().await?;
        let bytes = self.fs.read_file(&self.path).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn write_text(&self, content: &str) -> anyhow::Result<()> {
        if self.exists().await? && !self.is_file().await? {
            anyhow::bail!("Illegal operation on a directory: {}", self.path.display());
        }
        Ok(self.fs.write_file(&self.path, content.as_bytes()).await?)
    }

    pub async fn mkdir_parents(&self) -> anyhow::Result<()> {
        Ok(self.fs.mkdir(&self.path, true).await?)
    }

    async fn is_file(&self) -> anyhow::Result<bool> {
        let meta = self.fs.stat(&self.path).await?;
        Ok(meta.file_type.is_file())
    }

    async fn require_file(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.exists().await?, "File does not exist.");
        anyhow::ensure!(
            self.is_file().await?,
            "Illegal operation on a directory: {}",
            self.path.display()
        );
        Ok(())
    }
}
