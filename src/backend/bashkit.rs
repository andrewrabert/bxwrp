use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;

use super::Backend;
use crate::config::{MountMode, Settings, Source};
use crate::host_tool::{HostOutput, HostTool, ScriptInput, ScriptOutput, Written};
use crate::mount::{FilterFs, Layer, MountTable, PathFilter, SingleFile};

const COMMAND_NOT_FOUND_EXIT_CODE: i32 = 127;

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

fn memory_targets(settings: &Settings) -> Vec<PathBuf> {
    settings
        .layers
        .iter()
        .filter(|layer| {
            matches!(
                layer.source,
                Source::Memory {
                    mode: MountMode::ReadWrite
                }
            )
        })
        .map(|layer| layer.target.clone())
        .collect()
}

async fn mkdir_missing(
    memory: &Arc<dyn bashkit::FileSystem>,
    targets: &[PathBuf],
) -> anyhow::Result<()> {
    for target in targets {
        if !memory.exists(target).await.with_context(view(target))? {
            memory
                .mkdir(target, true)
                .await
                .with_context(view(target))?;
        }
    }
    Ok(())
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
            Source::Memory { mode } => match mode {
                MountMode::ReadOnly => Layer::shared(target, Arc::clone(&readonly)),
                MountMode::ReadWrite => Layer::shared(target, Arc::clone(memory)),
            },
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
            COMMAND_NOT_FOUND_EXIT_CODE,
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

const BUILD_ONLY_ENV: [&str; 1] = ["BASHKIT_ALLOW_INPROCESS_SQLITE"];

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn session_parent(path: &Path) -> anyhow::Result<&Path> {
    let parent = path
        .parent()
        .with_context(|| format!("session {}", path.display()))?;
    anyhow::ensure!(
        parent.is_dir(),
        "session {}: parent directory does not exist",
        path.display()
    );
    Ok(parent)
}

fn lock_session(path: &Path) -> anyhow::Result<std::fs::File> {
    let context = || format!("session {}", path.display());
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(with_suffix(path, ".lock"))
        .with_context(context)?;
    lock.lock().with_context(context)?;
    session_parent(path)?;
    Ok(lock)
}

fn read_session(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                !meta.file_type().is_symlink(),
                "session {}: is a symlink",
                path.display()
            );
            Ok(Some(
                std::fs::read(path).with_context(|| format!("session {}", path.display()))?,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("session {}", path.display())),
    }
}

fn write_session(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let context = || format!("session {}", path.display());
    let parent = session_parent(path)?;
    let tmp = with_suffix(path, ".tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(context)?;
    file.write_all(bytes).with_context(context)?;
    file.sync_all().with_context(context)?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(context)?;
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .with_context(context)?;
    Ok(())
}

pub(super) struct Bashkit {
    bash: Mutex<bashkit::Bash>,
    fs: Arc<dyn bashkit::FileSystem>,
    memory: Arc<dyn bashkit::FileSystem>,
    memory_targets: Vec<PathBuf>,
    env: Vec<(String, String)>,
    session: Option<PathBuf>,
    session_guard: Mutex<()>,
}

impl Bashkit {
    pub(super) async fn new(settings: &Settings, session: Option<PathBuf>) -> anyhow::Result<Self> {
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
        let memory_targets = memory_targets(settings);
        mkdir_missing(&memory, &memory_targets).await?;

        let layers = build_layers(settings, &memory).await?;
        let table: Arc<dyn bashkit::FileSystem> =
            Arc::new(MountTable::new(Arc::clone(&memory), layers));
        let table = filtered(table, settings.filter.as_ref());

        let base = bashkit::ExecutionProfile::named(bashkit::ExecutionProfileName::Interactive);
        let limits = base
            .execution_limits()
            .clone()
            .max_aggregate_input_bytes(settings.max_input_bytes())
            .max_stdout_bytes(settings.max_output_bytes())
            .max_stderr_bytes(settings.max_output_bytes())
            .max_work_units(u64::MAX);
        let profile =
            bashkit::ExecutionProfile::builder(bashkit::ExecutionProfileName::Interactive)
                .execution_limits(limits)
                .build()?;
        let mut builder = bashkit::Bash::builder()
            .profile(profile)
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
            builder = builder.command(name.as_ref(), host_tool);
        }
        if let Some(network) = settings.network() {
            builder = builder.network(network);
        }
        builder = builder.command_resolver(Arc::new(NotFoundResolver {
            suffix: not_found_suffix(settings).into(),
        }));
        builder = builder.cpython();
        builder = builder.sqlite().env("BASHKIT_ALLOW_INPROCESS_SQLITE", "1");
        builder = builder.typescript();
        let mut bash = builder.build();

        let mut state = bash.shell_state();
        state.env.clear();
        for (key, value) in settings.env() {
            state.env.insert(key.to_string(), value.to_string());
        }
        for key in BUILD_ONLY_ENV {
            state.variables.remove(key);
        }
        bash.restore_shell_state(&state);

        let fs = bash.fs();
        Ok(Self {
            bash: Mutex::new(bash),
            fs,
            memory,
            memory_targets,
            env: settings
                .env()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            session,
            session_guard: Mutex::new(()),
        })
    }

    /// Normal setup on top of a restored snapshot, keeping its env and cwd.
    async fn resume(&self, bash: &mut bashkit::Bash, snapshot: &[u8]) -> anyhow::Result<()> {
        bash.restore_snapshot(snapshot)?;
        mkdir_missing(&self.memory, &self.memory_targets).await?;
        for (key, value) in &self.env {
            bash.set_env(key, value);
        }
        let mut state = bash.shell_state();
        for key in BUILD_ONLY_ENV {
            state.variables.remove(key);
        }
        bash.restore_shell_state(&state);
        Ok(())
    }

    async fn session<T>(&self, op: impl AsyncFnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let Some(path) = &self.session else {
            return op().await;
        };
        let _guard = self.session_guard.lock().await;
        let lock = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || lock_session(&path)).await??
        };
        let snapshot = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || read_session(&path)).await??
        };
        if let Some(snapshot) = snapshot {
            self.resume(&mut *self.bash.lock().await, &snapshot).await?;
        }
        let result = op().await?;
        let bytes = self.bash.lock().await.snapshot()?;
        let path = path.clone();
        // The blocking task owns the lock so a cancelled caller cannot release it mid-write.
        tokio::task::spawn_blocking(move || {
            let written = write_session(&path, &bytes);
            drop(lock);
            written
        })
        .await??;
        Ok(result)
    }

    pub async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
        output: ScriptOutput,
    ) -> anyhow::Result<bashkit::ExecResult> {
        let mut options = bashkit::ExecOptions::new().arg0(arg0).positional(args);
        if let Some(stdin) = stdin {
            options = options.stdin(stdin);
        }
        let mut bash = self.bash.lock().await;
        let execution = bash.execute(script).options(options);
        match output {
            ScriptOutput::Capture(capture) => Ok(execution.capture(capture).await?),
            ScriptOutput::Inherit(_) => {
                let mut stdout = tokio::io::stdout();
                let mut stderr = tokio::io::stderr();
                let result = execution.stdout(&mut stdout).stderr(&mut stderr).await;
                stdout.flush().await?;
                stderr.flush().await?;
                Ok(result?)
            }
        }
    }

    pub fn path(&self, path: &Path) -> BashkitPath {
        BashkitPath {
            fs: Arc::clone(&self.fs),
            path: bashkit::normalize_path(path),
        }
    }
}

#[async_trait]
impl Backend for Bashkit {
    async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: ScriptInput,
        output: ScriptOutput,
    ) -> anyhow::Result<HostOutput> {
        let stdin = match stdin {
            ScriptInput::Closed => None,
            ScriptInput::Bytes(bytes) => Some(bytes),
        };
        let result = self
            .session(async || Self::exec_script(self, script, arg0, args, stdin, output).await)
            .await?;
        Ok(HostOutput {
            stdout: result.stdout.into_bytes(),
            stderr: result.stderr.into_bytes(),
            exit_code: result.exit_code,
            stdout_truncated: result.stdout_truncated,
            stderr_truncated: result.stderr_truncated,
        })
    }

    async fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        let path = self.path(path);
        self.session(async || path.read_text().await).await
    }

    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<Written> {
        let path = self.path(path);
        self.session(async || {
            let existed = path.exists().await?;
            path.parent().mkdir_parents().await?;
            path.write_text(content).await?;
            Ok(if existed {
                Written::Updated
            } else {
                Written::Created
            })
        })
        .await
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
        if matches!(self.kind().await?, Kind::Directory) {
            anyhow::bail!("Illegal operation on a directory: {}", self.path.display());
        }
        Ok(self.fs.write_file(&self.path, content.as_bytes()).await?)
    }

    pub async fn mkdir_parents(&self) -> anyhow::Result<()> {
        Ok(self.fs.mkdir(&self.path, true).await?)
    }

    async fn kind(&self) -> anyhow::Result<Kind> {
        if !self.exists().await? {
            return Ok(Kind::Missing);
        }
        let meta = self.fs.stat(&self.path).await?;
        Ok(if meta.file_type.is_file() {
            Kind::File
        } else {
            Kind::Directory
        })
    }

    async fn require_file(&self) -> anyhow::Result<()> {
        match self.kind().await? {
            Kind::Missing => anyhow::bail!("File does not exist."),
            Kind::Directory => {
                anyhow::bail!("Illegal operation on a directory: {}", self.path.display())
            }
            Kind::File => Ok(()),
        }
    }
}

enum Kind {
    Missing,
    File,
    Directory,
}
