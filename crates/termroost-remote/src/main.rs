//! Terminal Rooster's helper on a remote host. Terminal Rooster runs it over
//! SSH (copying it to `~/.terminalrooster/bin` first), and it serves that
//! connection: files, git, and the host's terminals, each of which runs
//! under a helper of its own (this program again) that outlives the
//! connection. See `termroost_core::remote`.

use std::process::ExitCode;

use termroost_core::HostCommand;
use termroost_core::remote::REMOTE_ARG;

/// The argument that makes this program a terminal's helper.
const HOST_ARG: &str = "host";

const HELP: &str = "\
Usage: terminalrooster-remote remote

Terminal Rooster's helper on a remote host. Terminal Rooster starts it over
SSH; it isn't meant to be run by hand.
";

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some(REMOTE_ARG) => {
            init_logging();
            let Some(helper) = helper_command() else { return ExitCode::FAILURE };
            exit(termroost_core::remote::serve(helper))
        }
        Some(HOST_ARG) => exit(termroost_core::host::serve()),
        Some("-V" | "--version") => {
            println!("terminalrooster-remote {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{HELP}");
            ExitCode::from(2)
        }
    }
}

/// This program, as a terminal's helper.
fn helper_command() -> Option<HostCommand> {
    match termroost_core::remote::this_program() {
        Ok(program) => Some(HostCommand { program, args: vec![HOST_ARG.to_owned()], env: Vec::new() }),
        Err(e) => {
            eprintln!("terminalrooster-remote: can't find this program: {e}");
            None
        }
    }
}

/// Logs to standard error, which SSH carries to Terminal Rooster's log.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("TERMINALROOSTER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).try_init();
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
