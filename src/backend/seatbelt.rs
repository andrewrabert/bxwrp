use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use async_trait::async_trait;

use super::Backend;
use crate::bridge::{BRIDGE_ENV, Bridge};
use crate::config::{Http, MountMode, Settings, Source, ToolName};
use crate::host_tool::{HostExecutable, HostOutput, ScriptInput, Written};
use crate::mount::{Entry, Visibility};
use crate::view::{
    Fate, Filter, Location, Removal, Served, View, entry_of, read_located, scratch_dir, shape,
    write_located,
};

const SANDBOX_EXEC: &str = "sandbox-exec";

const SHELL: &str = "/bin/bash";

const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

const PREAMBLE: &[&str] = &[
    "(deny default)",
    "(allow process-exec* process-fork process-info* signal)",
    "(allow sysctl-read)",
    "(allow mach-lookup mach-task-name)",
    "(allow ipc-posix-shm)",
    "(allow file-read-metadata)",
    r#"(allow file-read* (literal "/"))"#,
    r#"(allow file-write-data (literal "/dev/null") (literal "/dev/tty"))"#,
    r#"(allow file-ioctl (literal "/dev/dtracehelper") (literal "/dev/tty"))"#,
];

const SYSTEM_READ: &[&str] = &[
    "/Applications",
    "/Library",
    "/System",
    "/bin",
    "/opt",
    "/private/etc",
    "/private/var/db",
    "/private/var/select",
    "/sbin",
    "/usr",
];

pub(super) struct Seatbelt {
    view: View,
    global: Filter,
    base: Vec<String>,
    tail: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    #[allow(dead_code)]
    tmp: tempfile::TempDir,
    #[allow(dead_code)]
    shim: tempfile::TempDir,
    #[allow(dead_code)]
    bridge: Bridge,
}

fn quote(path: &Path) -> anyhow::Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{}: only UTF-8 paths can be sandboxed", path.display()))?;
    Ok(text.replace('\\', r"\\").replace('"', "\\\""))
}

fn pattern(entry: Entry, path: &Path) -> anyhow::Result<String> {
    let keyword = match entry {
        Entry::Dir => "subpath",
        Entry::File => "literal",
    };
    Ok(format!("({keyword} \"{}\")", quote(path)?))
}

fn rule(action: &str, operations: &str, entry: Entry, path: &Path) -> anyhow::Result<String> {
    Ok(format!("({action} {operations} {})", pattern(entry, path)?))
}

fn allow_read(rules: &mut Vec<String>, entry: Entry, path: &Path) -> anyhow::Result<()> {
    rules.push(rule("allow", "file-read*", entry, path)?);
    rules.push(rule("deny", "file-write*", entry, path)?);
    Ok(())
}

fn allow_write(rules: &mut Vec<String>, entry: Entry, path: &Path) -> anyhow::Result<()> {
    rules.push(rule(
        "allow",
        "file-read* file-write* file-ioctl",
        entry,
        path,
    )?);
    Ok(())
}

fn allow(
    rules: &mut Vec<String>,
    mode: MountMode,
    entry: Entry,
    path: &Path,
) -> anyhow::Result<()> {
    match mode {
        MountMode::ReadOnly => allow_read(rules, entry, path),
        MountMode::ReadWrite => allow_write(rules, entry, path),
    }
}

fn deny_read(rules: &mut Vec<String>, entry: Entry, path: &Path) -> anyhow::Result<()> {
    rules.push(rule("deny", "file-read* file-write*", entry, path)?);
    Ok(())
}

fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_identity(target: &Path, host: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        resolve(target) == host,
        "the seatbelt backend cannot remap paths: {} cannot be served at {}, \
         so drop the vfs path or set it to the host path",
        host.display(),
        target.display()
    );
    Ok(())
}

impl Seatbelt {
    pub(super) async fn new(settings: &Settings) -> anyhow::Result<Self> {
        let tmp = scratch_dir()?;
        let shim = scratch_dir()?;
        let mut view = View::default();

        for layer in &settings.layers {
            let served = match &layer.source {
                Source::HostDir { host, mode } => {
                    ensure_identity(&layer.target, host)?;
                    Served::Dir {
                        host: host.clone(),
                        mode: *mode,
                    }
                }
                Source::HostFile { host, mode } => {
                    ensure_identity(&layer.target, host)?;
                    Served::File {
                        host: host.clone(),
                        mode: *mode,
                    }
                }
                Source::Memory { .. } => anyhow::bail!(
                    "the seatbelt backend has no memory mounts: give the mount at {} a host path",
                    layer.target.display()
                ),
                Source::Remove => Served::Removed(match view.locate(None, &layer.target) {
                    Location::Host { path, .. } => match std::fs::metadata(&path) {
                        Ok(meta) if meta.is_dir() => Removal::Dir,
                        Ok(_) => Removal::File,
                        Err(_) => Removal::Nothing,
                    },
                    Location::Unmapped => Removal::Nothing,
                }),
            };
            view.push(layer.target.clone(), served, Filter::of_layer(layer));
        }

        let executable = std::env::current_exe().context("the bxwrp executable")?;
        let mut tools: BTreeMap<ToolName, HostExecutable> = BTreeMap::new();
        for (name, target) in settings.builtins() {
            std::os::unix::fs::symlink(&executable, shim.path().join(name.as_ref()))
                .with_context(|| format!("the {} shim", name.as_ref()))?;
            tools.insert(name.clone(), target.clone());
        }
        let bridge = Bridge::listen(tools)?;

        let mut base = vec![String::from("(version 1)")];
        base.extend(PREAMBLE.iter().map(|rule| (*rule).to_string()));
        for path in SYSTEM_READ {
            allow_read(&mut base, Entry::Dir, Path::new(path))?;
        }
        allow_write(&mut base, Entry::Dir, Path::new("/dev"))?;
        let scratch = resolve(tmp.path());
        allow_write(&mut base, Entry::Dir, &scratch)?;

        let mut env: Vec<(String, String)> = Vec::new();
        let mut path = None;
        for (name, value) in settings.env() {
            if name == "PATH" {
                path = Some(value.to_string());
                continue;
            }
            env.push((name.to_string(), value.to_string()));
        }
        env.push((
            String::from("PATH"),
            format!(
                "{}:{}",
                resolve(shim.path()).display(),
                path.as_deref().unwrap_or(DEFAULT_PATH)
            ),
        ));
        env.push((
            String::from("TMPDIR"),
            scratch.to_string_lossy().into_owned(),
        ));
        env.push((
            String::from(BRIDGE_ENV),
            bridge.socket().to_string_lossy().into_owned(),
        ));

        if matches!(view.locate(None, &settings.cwd), Location::Unmapped) {
            tracing::warn!("{}: cwd is under no mount", settings.cwd.display());
        }
        anyhow::ensure!(
            settings.cwd.is_dir(),
            "{}: cwd is not a directory on this host, and the seatbelt \
             backend cannot make one",
            settings.cwd.display()
        );

        let mut tail = vec![
            rule("allow", "file-read*", Entry::Dir, &resolve(shim.path()))?,
            rule("allow", "file-read*", Entry::File, &resolve(&executable))?,
        ];
        tail.push(match settings.http() {
            Http::Enabled => String::from("(allow network*)"),
            Http::Disabled => String::from("(deny network*)"),
        });
        tail.push(format!(
            "(allow network-outbound (literal \"{}\"))",
            quote(&resolve(bridge.socket()))?
        ));

        Ok(Self {
            view,
            global: Filter::of_view(settings),
            base,
            tail,
            env,
            cwd: resolve(&settings.cwd),
            tmp,
            shim,
            bridge,
        })
    }

    fn launcher(&self) -> anyhow::Result<HostExecutable> {
        let profile = self.profile()?;
        Ok(HostExecutable {
            path: PathBuf::from(SANDBOX_EXEC),
            args: vec![String::from("-p"), profile],
            cwd: Some(self.cwd.clone()),
            env: self.env.clone(),
            clear_env: true,
        })
    }

    fn profile(&self) -> anyhow::Result<String> {
        let mut rules = self.base.clone();
        for layer in &self.view.layers {
            match &layer.served {
                Served::Dir { host, mode } => {
                    allow(&mut rules, *mode, Entry::Dir, host)?;
                    if let Filter::By(filter) = &layer.filter {
                        shape(
                            |rel, entry| match filter
                                .own_visibility(&Path::new("/").join(rel), entry)
                            {
                                Visibility::Shown => Fate::Shown,
                                Visibility::Hidden => Fate::Hidden,
                            },
                            host,
                            host,
                            &mut |entry, path| deny_read(&mut rules, entry, path),
                        )?;
                    }
                }
                Served::File { host, mode } => {
                    allow(&mut rules, *mode, Entry::File, host)?;
                }
                Served::Removed(Removal::File) => {
                    deny_read(&mut rules, Entry::File, &resolve(&layer.target))?;
                }
                Served::Removed(Removal::Dir | Removal::Nothing) => {
                    deny_read(&mut rules, Entry::Dir, &resolve(&layer.target))?;
                }
            }
        }
        if let Filter::By(global) = &self.global {
            let judge = |own: &Filter, target: &Path, rel: &Path, entry: Entry| match own
                .own_visibility(&Path::new("/").join(rel), entry)
            {
                Visibility::Hidden => Fate::Absent,
                Visibility::Shown => match global.own_visibility(&target.join(rel), entry) {
                    Visibility::Shown => Fate::Shown,
                    Visibility::Hidden => Fate::Hidden,
                },
            };
            for layer in &self.view.layers {
                if let Served::Dir { host, .. } = &layer.served {
                    shape(
                        |rel, entry| judge(&layer.filter, &layer.target, rel, entry),
                        host,
                        host,
                        &mut |entry, path| deny_read(&mut rules, entry, path),
                    )?;
                }
            }
        }
        rules.extend(self.tail.iter().cloned());
        rules.push(String::new());
        Ok(rules.join("\n"))
    }

    fn locate(&self, vfs: &Path) -> Location {
        match self.view.locate(None, vfs) {
            Location::Host { path, mode } => match self.global.visibility(vfs, entry_of(&path)) {
                Visibility::Shown => Location::Host { path, mode },
                Visibility::Hidden => Location::Unmapped,
            },
            Location::Unmapped => Location::Unmapped,
        }
    }
}

#[async_trait]
impl Backend for Seatbelt {
    async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: ScriptInput,
    ) -> anyhow::Result<HostOutput> {
        let mut argv = vec![
            String::from(SHELL),
            String::from("-c"),
            script.to_string(),
            arg0.to_string(),
        ];
        argv.extend(args);
        let stdin = match stdin {
            ScriptInput::Closed => None,
            ScriptInput::Bytes(bytes) => Some(bytes),
        };
        Ok(self.launcher()?.execute(argv, stdin).await?)
    }

    async fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        read_located(self.locate(path), path).await
    }

    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<Written> {
        write_located(self.locate(path), path, content).await
    }
}
