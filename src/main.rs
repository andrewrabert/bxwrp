use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use bxwrp::bridge::{BRIDGE_ENV, shim};
use bxwrp::cli::{Cli, ConfigAction, Mode, Options};
use bxwrp::config::{Config, Host};
use bxwrp::host_tool::ScriptInput;
use bxwrp::mcp;

const SH: &str = "sh";

const SH_ALIASES: [&str; 2] = ["bxwrp-sh", SH];

enum Target {
    Mcp,
    Invoke { arg0: String, args: Vec<String> },
    StdinScript,
}

fn install_log_subscriber() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .try_init();
}

fn validate_config(host: Host, overlay: Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !host.paths().is_empty(),
        "this run names no config file to validate"
    );
    let paths = host.paths().to_vec();
    host.resolve(overlay)?;
    for path in paths {
        println!("{}: ok", path.display());
    }
    Ok(())
}

fn shim_main(socket: std::ffi::OsString) -> anyhow::Result<()> {
    use std::path::Path;

    use bxwrp::config::ToolName;

    let mut argv = std::env::args_os();
    let argv0 = argv.next().unwrap_or_default();
    let name = Path::new(&argv0)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = ToolName::try_from(name).map_err(|error| anyhow::anyhow!("bxwrp: {error}"))?;
    let exit_code = shim(Path::new(&socket), name, argv.collect())?;
    std::process::exit(exit_code);
}

fn main() -> anyhow::Result<()> {
    if let Some(socket) = std::env::var_os(BRIDGE_ENV) {
        return shim_main(socket);
    }
    let mut argv = std::env::args_os();
    let argv0 = argv.next().map(std::path::PathBuf::from);
    let sh_alias = argv0.is_some_and(|path| {
        path.file_name()
            .is_some_and(|name| SH_ALIASES.iter().any(|alias| name == *alias))
    });
    if sh_alias {
        return sh_alias_main(argv.map(|arg| arg.to_string_lossy().into_owned()).collect());
    }
    let cli = Cli::read();
    match cli.mode {
        Mode::Config { action } => config_main(action, cli.options),
        Mode::Exec { args } => shell_main(Some(args), cli.options),
        Mode::Mcp => shell_main(None, cli.options),
    }
}

fn config_main(action: ConfigAction, options: Options) -> anyhow::Result<()> {
    match action {
        ConfigAction::Export => {
            let host = Host::load(&options.config)?;
            install_log_subscriber();
            print!("{}", host.merge(options.into_config()?).render()?);
            Ok(())
        }
        ConfigAction::Schema => {
            println!("{}", Config::json_schema()?);
            Ok(())
        }
        ConfigAction::Validate => {
            let host = Host::load(&options.config)?;
            install_log_subscriber();
            validate_config(host, options.into_config()?)
        }
    }
}

fn sh_alias_main(raw: Vec<String>) -> anyhow::Result<()> {
    let host = Host::load(&[])?;
    install_log_subscriber();
    let settings = host.resolve(Config::default())?;
    let session = session_path(std::env::var_os("BXWRP_SESSION").map(PathBuf::from))?;
    let target = if raw.is_empty() {
        stdin_script_target()?
    } else {
        Target::Invoke {
            arg0: SH.to_string(),
            args: raw,
        }
    };
    run(target, settings, session)
}

fn shell_main(exec: Option<Vec<String>>, mut options: Options) -> anyhow::Result<()> {
    let host = Host::load(&options.config)?;
    install_log_subscriber();
    let session = session_path(options.session.take())?;
    let overlay = options.into_config()?;
    let settings = host.resolve(overlay)?;
    let target = match exec {
        None => Target::Mcp,
        Some(mut args) => {
            if args.is_empty() {
                stdin_script_target()?
            } else {
                let arg0 = args.remove(0);
                Target::Invoke { arg0, args }
            }
        }
    };
    run(target, settings, session)
}

fn session_path(path: Option<PathBuf>) -> anyhow::Result<Option<PathBuf>> {
    Ok(path.map(std::path::absolute).transpose()?)
}

fn stdin_script_target() -> anyhow::Result<Target> {
    anyhow::ensure!(
        !std::io::stdin().is_terminal(),
        "exec: pass a command or pipe a script"
    );
    Ok(Target::StdinScript)
}

fn run(
    target: Target,
    settings: bxwrp::config::Settings,
    session: Option<PathBuf>,
) -> anyhow::Result<()> {
    let stdin_is_pipe = !std::io::stdin().is_terminal();
    let invocation = match target {
        Target::Mcp => None,
        Target::StdinScript => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes)?;
            let script = String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("exec: script on stdin is not valid UTF-8"))?;
            Some((script, SH.to_string(), Vec::new(), false))
        }
        Target::Invoke { arg0, args } => Some((r#""$0" "$@""#.to_string(), arg0, args, true)),
    };
    let stdin = match &invocation {
        Some((_, _, _, true)) if stdin_is_pipe => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes)?;
            Some(bytes)
        }
        _ => None,
    };
    let stdin = match stdin {
        None => ScriptInput::Closed,
        Some(bytes) => ScriptInput::Bytes(bytes),
    };
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        let sandbox = bxwrp::backend::new(&settings, session).await?;
        if let Some((script, arg0, args, _)) = invocation {
            Ok::<_, anyhow::Error>(Some(
                sandbox.exec_script(&script, &arg0, args, stdin).await?,
            ))
        } else {
            mcp::serve(sandbox, settings).await?;
            Ok(None)
        }
    })?;
    if let Some(result) = result {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&result.stdout)?;
        stdout.flush()?;
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(&result.stderr)?;
        stderr.flush()?;
        if result.exit_code != 0 {
            std::process::exit(result.exit_code);
        }
    }
    Ok(())
}
