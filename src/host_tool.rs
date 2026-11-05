use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};

use bashkit::{CommandContext, CommandInput, CommandOutput, CommandStatus, async_trait};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

pub const FAILURE_EXIT_CODE: i32 = 1;

pub enum ScriptInput {
    Closed,
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptOutput {
    Capture(usize),
    Inherit(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Created,
    Updated,
}

pub struct HostOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct HostExecutable {
    pub path: PathBuf,

    pub args: Vec<String>,

    pub cwd: Option<PathBuf>,

    pub env: Vec<(String, String)>,

    pub clear_env: bool,
}

pub struct HostTool {
    name: String,
    executable: HostExecutable,
}

impl HostExecutable {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command.args(&self.args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(self.env.iter().cloned());
        command.kill_on_drop(true);
        command
    }

    pub async fn execute<I, S>(
        &self,
        args: I,
        stdin: Option<Vec<u8>>,
        output: ScriptOutput,
    ) -> std::io::Result<HostOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = self.command();
        command.args(args);
        run(command, stdin, output).await
    }

    pub async fn spawn_with(
        &self,
        args: &[String],
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> std::io::Result<ExitStatus> {
        let mut command = self.command();
        command.args(args);
        command.stdin(stdin).stdout(stdout).stderr(stderr);
        command.spawn()?.wait().await
    }
}

impl HostTool {
    #[must_use]
    pub const fn new(name: String, executable: HostExecutable) -> Self {
        Self { name, executable }
    }
}

type Sink<'a> = Box<dyn AsyncWrite + Unpin + Send + 'a>;

/// Returns whether output past the limit was dropped.
async fn copy_capped(
    pipe: Option<impl AsyncRead + Unpin>,
    mut sink: Sink<'_>,
    limit: u64,
) -> std::io::Result<bool> {
    let Some(pipe) = pipe else {
        return Ok(false);
    };
    let mut pipe = pipe.take(limit);
    match tokio::io::copy(&mut pipe, &mut sink).await {
        Ok(_) => {}
        // A closed reader ends the stream as in a shell pipeline; dropping the
        // pipe sends the child SIGPIPE.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(false),
        Err(error) => return Err(error),
    }
    // Draining past the limit keeps the child from blocking on a full pipe.
    let dropped = tokio::io::copy(&mut pipe.into_inner(), &mut tokio::io::sink()).await?;
    if let Err(error) = sink.flush().await
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(error);
    }
    Ok(dropped > 0)
}

async fn run(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    output: ScriptOutput,
) -> std::io::Result<HostOutput> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let pipe = child.stdin.take();
    let write_stdin = async move {
        if let (Some(bytes), Some(mut pipe)) = (stdin, pipe) {
            if let Err(error) = pipe.write_all(&bytes).await
                && error.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(error);
            }
            if let Err(error) = pipe.shutdown().await
                && error.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(error);
            }
        }
        Ok(())
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (limit, stdout_sink, stderr_sink): (usize, Sink<'_>, Sink<'_>) = match output {
        ScriptOutput::Capture(limit) => (limit, Box::new(&mut stdout), Box::new(&mut stderr)),
        ScriptOutput::Inherit(limit) => (
            limit,
            Box::new(tokio::io::stdout()),
            Box::new(tokio::io::stderr()),
        ),
    };
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    let (stdin_result, stdout_truncated, stderr_truncated, status) = tokio::join!(
        write_stdin,
        copy_capped(child.stdout.take(), stdout_sink, limit),
        copy_capped(child.stderr.take(), stderr_sink, limit),
        child.wait(),
    );
    let status = status?;
    let stdout_truncated = stdout_truncated?;
    let stderr_truncated = stderr_truncated?;
    stdin_result?;
    Ok(HostOutput {
        stdout,
        stderr,
        exit_code: status.code().unwrap_or(FAILURE_EXIT_CODE),
        stdout_truncated,
        stderr_truncated,
    })
}

enum Delivery {
    Sent,
    ReaderClosed,
}

/// A closed reader ends the stream as in a shell pipeline; other errors propagate.
fn delivery(written: bashkit::Result<()>) -> bashkit::Result<Delivery> {
    match written {
        Ok(()) => Ok(Delivery::Sent),
        Err(bashkit::Error::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            Ok(Delivery::ReaderClosed)
        }
        Err(error) => Err(error),
    }
}

/// Stops early when the child closes its stdin; dropping the pipe is its EOF.
async fn feed(input: CommandInput, mut pipe: impl AsyncWrite + Unpin) -> bashkit::Result<()> {
    while let Some(chunk) = input.read_chunk().await? {
        match pipe.write_all(&chunk).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(bashkit::Error::Io(error)),
        }
    }
    match pipe.shutdown().await {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(bashkit::Error::Io(error))
        }
        _ => Ok(()),
    }
}

/// Stops when the reader closes; dropping the pipe sends the child SIGPIPE.
async fn relay(mut pipe: impl AsyncRead + Unpin, output: CommandOutput) -> bashkit::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let read = match pipe.read(&mut chunk).await {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(bashkit::Error::Io(error)),
        };
        match delivery(output.write_all(&chunk[..read]).await)? {
            Delivery::Sent => {}
            Delivery::ReaderClosed => return Ok(()),
        }
    }
}

#[async_trait]
impl bashkit::Command for HostTool {
    async fn execute(&self, ctx: CommandContext<'_>) -> bashkit::Result<CommandStatus> {
        let mut command = self.executable.command();
        command
            .args(ctx.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("bxwrp: {}: {error}\n", self.name);
                delivery(ctx.stderr.write_all(message.as_bytes()).await)?;
                return Ok(CommandStatus::with_code(FAILURE_EXIT_CODE));
            }
        };
        let (Some(pipe), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            unreachable!("every child stream is piped");
        };
        let input = ctx.stdin;
        // An exited child reads no more input, so stop waiting on upstream.
        let wait = async {
            let fed = tokio::select! {
                status = child.wait() => return status.map_err(bashkit::Error::Io),
                fed = feed(input, pipe) => fed,
            };
            fed?;
            child.wait().await.map_err(bashkit::Error::Io)
        };
        let (status, (), ()) =
            tokio::try_join!(wait, relay(stdout, ctx.stdout), relay(stderr, ctx.stderr),)?;
        Ok(CommandStatus::with_code(
            status.code().unwrap_or(FAILURE_EXIT_CODE),
        ))
    }
}
