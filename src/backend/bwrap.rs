use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use async_trait::async_trait;

use super::Backend;
use crate::bridge::{BRIDGE_ENV, Bridge};
use crate::config::{Http, MountMode, Settings, Source, ToolName};
use crate::host_tool::{HostExecutable, HostOutput, ScriptInput, ScriptOutput, Written};
use crate::mount::{Entry, Visibility};
use crate::view::{
    Fate, Filter, Location, Removal, Served, TMP, View, entry_of, read_located, scratch_dir, shape,
    text, write_located,
};

pub const SHIM_DIR: &str = "/.bxwrp/bin";

pub const SHIM_EXE: &str = "/.bxwrp/bxwrp";

pub const BRIDGE_SOCKET: &str = "/run/bxwrp/bridge.sock";

const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

const BWRAP: &str = "bwrap";

const SHELL: &str = "/bin/bash";

const SYSTEM_READ: &[&str] = &[
    "/bin",
    "/opt",
    "/usr",
    "/etc",
    "/lib",
    "/lib64",
    "/run/systemd/resolve",
];

pub(super) struct Bwrap {
    head: Vec<String>,
    view: View,
    global: Filter,
    tail: Vec<String>,
    tmp: tempfile::TempDir,
    #[allow(dead_code)]
    scratch: Vec<tempfile::TempDir>,
    #[allow(dead_code)]
    bridge: Bridge,
}

const fn bind_flag(mode: MountMode) -> &'static str {
    match mode {
        MountMode::ReadOnly => "--ro-bind",
        MountMode::ReadWrite => "--bind",
    }
}

fn bind(args: &mut Vec<String>, mode: MountMode, host: &Path, target: &str) -> anyhow::Result<()> {
    args.push(String::from(bind_flag(mode)));
    args.push(text(host)?);
    args.push(target.to_string());
    Ok(())
}

fn base(args: &mut Vec<String>) -> anyhow::Result<()> {
    args.extend(
        [
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run",
        ]
        .map(String::from),
    );
    args.push(String::from("--dir"));
    args.push(format!("/run/user/{}", rustix::process::getuid().as_raw()));
    let mut bound = BTreeSet::new();
    let mut links = Vec::new();
    for path in SYSTEM_READ {
        let path = Path::new(path);
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue;
        };
        let host = if meta.is_symlink() {
            let target = std::fs::canonicalize(path)
                .with_context(|| format!("{}: resolving the system path", path.display()))?;
            links.push((text(&target)?, text(path)?));
            target
        } else {
            path.to_path_buf()
        };
        if bound.insert(host.clone()) {
            bind(args, MountMode::ReadOnly, &host, &text(&host)?)?;
        }
    }
    for (target, link) in links {
        args.extend([String::from("--symlink"), target, link]);
    }
    Ok(())
}

fn hide(args: &mut Vec<String>, entry: Entry, target: &str) {
    match entry {
        Entry::Dir => args.push(String::from("--tmpfs")),
        Entry::File => args.extend(["--ro-bind", "/dev/null"].map(String::from)),
    }
    args.push(target.to_string());
}

impl Bwrap {
    pub(super) async fn new(settings: &Settings) -> anyhow::Result<Self> {
        let tmp = scratch_dir()?;
        let mut scratch = Vec::new();
        let mut view = View::default();
        let mut head: Vec<String> = vec![String::from("--unshare-all")];
        if matches!(settings.http(), Http::Enabled) {
            head.push(String::from("--share-net"));
        }
        head.push(String::from("--new-session"));
        head.push(String::from("--die-with-parent"));
        base(&mut head)?;

        for layer in &settings.layers {
            let served = match &layer.source {
                Source::HostDir { host, mode } => Served::Dir {
                    host: host.clone(),
                    mode: *mode,
                },
                Source::HostFile { host, mode } => Served::File {
                    host: host.clone(),
                    mode: *mode,
                },
                Source::Memory { mode } => {
                    let dir = scratch_dir()?;
                    let host = dir.path().to_path_buf();
                    scratch.push(dir);
                    Served::Dir { host, mode: *mode }
                }
                Source::Remove => {
                    Served::Removed(match view.locate(Some(tmp.path()), &layer.target) {
                        Location::Host { path, .. } => match std::fs::metadata(&path) {
                            Ok(meta) if meta.is_dir() => Removal::Dir,
                            Ok(_) => Removal::File,
                            Err(_) => Removal::Nothing,
                        },
                        Location::Unmapped => Removal::Nothing,
                    })
                }
            };
            view.push(layer.target.clone(), served, Filter::of_layer(layer));
        }

        let executable = std::env::current_exe().context("the bxwrp executable")?;
        let mut tail = Vec::new();
        tail.push(String::from("--ro-bind"));
        tail.push(text(&executable)?);
        tail.push(String::from(SHIM_EXE));
        let mut tools: BTreeMap<ToolName, HostExecutable> = BTreeMap::new();
        for (name, target) in settings.builtins() {
            tail.push(String::from("--symlink"));
            tail.push(String::from(SHIM_EXE));
            tail.push(format!("{SHIM_DIR}/{}", name.as_ref()));
            tools.insert(name.clone(), target.clone());
        }
        let bridge = Bridge::listen(tools)?;
        tail.push(String::from("--bind"));
        tail.push(text(bridge.socket())?);
        tail.push(String::from(BRIDGE_SOCKET));

        let cwd = text(&settings.cwd)?;
        tail.push(String::from("--dir"));
        tail.push(cwd.clone());
        tail.push(String::from("--chdir"));
        tail.push(cwd);
        tail.push(String::from("--hostname"));
        tail.push(settings.hostname().to_string());
        tail.push(String::from("--clearenv"));
        let mut path = None;
        for (name, value) in settings.env() {
            if name == "PATH" {
                path = Some(value.to_string());
                continue;
            }
            tail.push(String::from("--setenv"));
            tail.push(name.to_string());
            tail.push(value.to_string());
        }
        tail.push(String::from("--setenv"));
        tail.push(String::from("PATH"));
        tail.push(format!(
            "{SHIM_DIR}:{}",
            path.as_deref().unwrap_or(DEFAULT_PATH)
        ));
        tail.push(String::from("--setenv"));
        tail.push(String::from(BRIDGE_ENV));
        tail.push(String::from(BRIDGE_SOCKET));

        if matches!(
            view.locate(Some(tmp.path()), &settings.cwd),
            Location::Unmapped
        ) {
            tracing::warn!("{}: cwd is under no mount", settings.cwd.display());
        }

        Ok(Self {
            head,
            view,
            global: Filter::of_view(settings),
            tail,
            tmp,
            scratch,
            bridge,
        })
    }

    fn launcher(&self) -> anyhow::Result<HostExecutable> {
        let mut args = self.head.clone();
        for layer in &self.view.layers {
            let target = text(&layer.target)?;
            match &layer.served {
                Served::Dir { host, mode } => {
                    bind(&mut args, *mode, host, &target)?;
                    if let Filter::By(filter) = &layer.filter {
                        shape(
                            |rel, entry| match filter
                                .own_visibility(&Path::new("/").join(rel), entry)
                            {
                                Visibility::Shown => Fate::Shown,
                                Visibility::Hidden => Fate::Hidden,
                            },
                            host,
                            &layer.target,
                            &mut |entry, path| {
                                hide(&mut args, entry, &text(path)?);
                                Ok(())
                            },
                        )?;
                    }
                }
                Served::File { host, mode } => bind(&mut args, *mode, host, &target)?,
                Served::Removed(Removal::Dir) => hide(&mut args, Entry::Dir, &target),
                Served::Removed(Removal::File) => hide(&mut args, Entry::File, &target),
                Served::Removed(Removal::Nothing) => {}
            }
        }
        args.extend(["--proc", "/proc"].map(String::from));
        args.extend(["--dev", "/dev"].map(String::from));
        args.extend(["--tmpfs", "/run"].map(String::from));
        bind(&mut args, MountMode::ReadWrite, self.tmp.path(), TMP)?;
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
                        &layer.target,
                        &mut |entry, path| {
                            hide(&mut args, entry, &text(path)?);
                            Ok(())
                        },
                    )?;
                }
            }
            shape(
                |rel, entry| judge(&Filter::Unfiltered, Path::new(TMP), rel, entry),
                self.tmp.path(),
                Path::new(TMP),
                &mut |entry, path| {
                    hide(&mut args, entry, &text(path)?);
                    Ok(())
                },
            )?;
        }
        args.extend(self.tail.iter().cloned());
        Ok(HostExecutable {
            path: PathBuf::from(BWRAP),
            args,
            cwd: None,
            env: Vec::new(),
            clear_env: false,
        })
    }

    fn locate(&self, vfs: &Path) -> Location {
        match self.view.locate(Some(self.tmp.path()), vfs) {
            Location::Host { path, mode } => match self.global.visibility(vfs, entry_of(&path)) {
                Visibility::Shown => Location::Host { path, mode },
                Visibility::Hidden => Location::Unmapped,
            },
            Location::Unmapped => Location::Unmapped,
        }
    }
}

#[async_trait]
impl Backend for Bwrap {
    async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: ScriptInput,
        output: ScriptOutput,
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
        Ok(self.launcher()?.execute(argv, stdin, output).await?)
    }

    async fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        read_located(self.locate(path), path).await
    }

    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<Written> {
        write_located(self.locate(path), path, content).await
    }
}
