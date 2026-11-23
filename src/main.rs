//! `rr`: parse the command line, build the environment and the session, hand one command to
//! [`app`](remote_run::app), render what it returns, print it and exit. The only place that
//! prints, reads the process environment or starts the real adapters.

// the shell reads the process environment once, and printing is its job
#![allow(clippy::disallowed_methods, clippy::print_stdout, clippy::print_stderr)]

mod cli;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Cmd, FwdArgs, JobCmd, RemoteCmd, StdoutUse, format_of_args};
use remote_run::adapters::{OsExec, OsNet, Ssh, SystemClock};
use remote_run::app::{self, Ports};
use remote_run::config::{Environment, Session, Settings, load, resolve, utf8_var};
use remote_run::domain::exit::{Exit, RrCode};
use remote_run::domain::outcome::{
    Format, Outcome, Rendered, render, render_error, render_error_as,
};
use remote_run::domain::slurm::JobSet;
use remote_run::ports::{Echo, Notes};
use remote_run::trust;
use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return usage_error(err),
    };
    let format = cli.format;
    let rendered = match environment().and_then(|env| run(cli, &env)) {
        Ok(outcome) => render(&outcome, format),
        Err(err) => render_error(&err, format),
    };
    emit(
        &rendered,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

/// A command line clap refused. `--help` and `--version` are not errors and clap prints them as it
/// always does; a real usage error is clap's text with exit 2, or, when `--json` was given, one
/// error document on stdout (and the same exit 2), so a script that asked for JSON gets JSON.
fn usage_error(err: clap::Error) -> ExitCode {
    let format = format_of_args(std::env::args_os());
    if !err.use_stderr() || format == Format::Text {
        err.exit();
    }
    let message = anyhow::anyhow!(err.to_string());
    let rendered = render_error_as(&message, format, RrCode::Usage);
    emit(
        &rendered,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

/// Write the result and return the exit code. A closed pipe (`rr --json ... | head -1`) is how a
/// reader says it has enough, not a failure to report, and `print!` would panic on it. Both
/// streams are always attempted: a closed stdout must not hide the notes on stderr. Any other
/// write error is told on stderr whatever the exit code, and turns a success into a failure: the
/// result was not delivered.
fn emit(rendered: &Rendered, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let wrote = [
        write_all(out, &rendered.stdout),
        write_all(err, &rendered.stderr),
    ];
    let failure = wrote
        .into_iter()
        .filter_map(Result::err)
        .find(|failure| failure.kind() != io::ErrorKind::BrokenPipe);
    let Some(failure) = failure else {
        return ExitCode::from(rendered.exit.code());
    };
    // stderr may be what failed; there is nowhere left to say so then
    let _ = writeln!(err, "rr: could not write the result: {failure}");
    match rendered.exit {
        Exit::Success => ExitCode::from(RrCode::Failure.code()),
        other => ExitCode::from(other.code()),
    }
}

fn write_all(sink: &mut dyn Write, text: &str) -> io::Result<()> {
    sink.write_all(text.as_bytes())?;
    sink.flush()
}

/// Prints each note at once, so it precedes the output of a command that runs after it.
struct StderrNotes;

impl Notes for StderrNotes {
    fn note(&self, line: &str) {
        // `eprintln!` panics when stderr is closed (`2>&1 | head`); a lost note is not worth that
        let _ = writeln!(io::stderr().lock(), "{line}");
    }
}

/// The only place the process environment and working directory are read.
fn environment() -> Result<Environment> {
    let var = |name: &str| utf8_var(name, std::env::var_os(name));
    Ok(Environment {
        cwd: std::env::current_dir()?,
        xdg_config_home: var("XDG_CONFIG_HOME")?,
        xdg_state_home: var("XDG_STATE_HOME")?,
        xdg_runtime_dir: var("XDG_RUNTIME_DIR")?,
        home: var("HOME")?,
        appdata: var("APPDATA")?,
    })
}

/// Flags and `RR_*` variables over the project and user files, validated into a session.
fn session(given: Settings, env: &Environment) -> Result<Session> {
    let read_store = || match env.trust_file() {
        Some(path) => trust::load(&path),
        None => Ok(trust::TrustStore::default()),
    };
    let files = load(&env.cwd, env.user_file().as_deref(), read_store)?;
    resolve(given.over(files))
}

/// `--json` promises one document on stdout; a command that shows a program's own text there
/// cannot keep it, so it is refused before anything starts.
fn refuse_json_for(command: &RemoteCmd, format: Format) -> Result<()> {
    match (format, command.stdout_use()) {
        (Format::Json, StdoutUse::ProgramOutput) => bail!(
            "--json does not apply to a command that writes to stdout before its result (q, acct, \
             log, x, fwd, cancel, get, put, run without --wait): that would not be one JSON \
             document"
        ),
        (Format::Json, StdoutUse::Result) | (Format::Text, _) => Ok(()),
    }
}

fn run(cli: Cli, env: &Environment) -> Result<Outcome> {
    let (format, echo) = (cli.format, cli.echo());
    let command = match cli.command {
        Cmd::Trust { action } => return app::trust::run(env, action),
        Cmd::Remote(command) => command,
    };
    refuse_json_for(&command, format)?;
    let given = Settings {
        host: cli.host,
        dir: cli.dir,
        env: cli.env_file,
        ..Settings::default()
    };
    let session = session(given, env)?;
    let exec = OsExec;
    let ssh = Ssh::new(&session.host, &exec);
    let clock = SystemClock::new();
    let ports = Ports {
        exec: &exec,
        remote: &ssh,
        notes: &StderrNotes,
        clock: &clock,
    };
    dispatch(command, &session, &ports, env, echo)
}

fn dispatch_job(command: JobCmd, session: &Session, ports: &Ports) -> Result<Outcome> {
    match command {
        JobCmd::Wait { jobs, limits } => {
            let args = app::wait::Args {
                jobs: JobSet::new(jobs)?,
                limits: limits.limits(),
            };
            app::wait::run(session, args, ports)
        }
        JobCmd::Status { job, lines } => {
            app::status::run(session, app::status::Args { job, lines }, ports)
        }
    }
}

fn dispatch(
    command: RemoteCmd,
    session: &Session,
    ports: &Ports,
    env: &Environment,
    echo: Echo,
) -> Result<Outcome> {
    match command {
        RemoteCmd::Job(command) => dispatch_job(command, session, ports),
        RemoteCmd::Q => app::q::run(session, ports),
        RemoteCmd::Acct { since } => app::acct::run(session, app::acct::Args { since }, ports),
        RemoteCmd::Log { job, follow, lines } => {
            app::log::run(session, app::log::Args { job, follow, lines }, ports)
        }
        RemoteCmd::Cancel { jobs } => app::cancel::run(session, app::cancel::Args { jobs }, ports),
        RemoteCmd::X { terminal, cmd } => {
            app::exec::run(session, app::exec::Args { terminal, cmd }, ports)
        }
        RemoteCmd::Pull { policy } => app::pull::run(session, app::pull::Args { policy }, ports),
        RemoteCmd::Run {
            policy,
            terminal,
            wait,
            cmd,
        } => {
            let args = app::run::Args::new(policy, terminal, cmd, wait.after(echo));
            app::run::run(session, args, ports)
        }
        RemoteCmd::Fwd(args) => fwd(session, args, ports, env),
        RemoteCmd::Get {
            remote,
            local,
            limit,
        } => {
            let args = app::get::Args {
                remote,
                local,
                limit,
            };
            app::get::run(session, args, ports)
        }
        RemoteCmd::Put {
            local,
            remote,
            limit,
        } => {
            let args = app::put::Args {
                local,
                remote,
                limit,
            };
            app::put::run(session, args, ports)
        }
    }
}

/// `rr fwd` is the one command that also needs the local network, clock and runtime directory.
fn fwd(session: &Session, args: FwdArgs, ports: &Ports, env: &Environment) -> Result<Outcome> {
    let machine = app::fwd::Machine {
        net: &OsNet,
        runtime_dir: env.xdg_runtime_dir.as_deref(),
    };
    app::fwd::run(session, args.into(), ports, &machine)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose every write fails with `kind`.
    struct Failing(io::ErrorKind);
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn rendered(exit: Exit) -> Rendered {
        Rendered {
            exit,
            stdout: "{}\n".into(),
            stderr: "rr: note\n".into(),
        }
    }

    #[test]
    fn a_closed_stdout_keeps_the_computed_exit_code() {
        let closed = || Failing(io::ErrorKind::BrokenPipe);
        let mut err = Vec::new();
        for exit in [Exit::Success, Exit::Rr(RrCode::Refused), Exit::remote(7)] {
            let code = emit(&rendered(exit), &mut closed(), &mut err);
            assert_eq!(code, ExitCode::from(exit.code()), "{exit:?}");
        }
        let shown = String::from_utf8(err).unwrap();
        assert!(
            !shown.contains("could not write"),
            "a closed pipe is not reported"
        );
    }

    #[test]
    fn a_write_that_failed_for_another_reason_is_no_success() {
        let mut err = Vec::new();
        let full = Failing(io::ErrorKind::StorageFull);
        let code = emit(&rendered(Exit::Success), &mut { full }, &mut err);
        assert_eq!(code, ExitCode::from(RrCode::Failure.code()));
        assert!(String::from_utf8(err).unwrap().contains("could not write"));
        // a result that already says failure keeps its own code
        let full = Failing(io::ErrorKind::StorageFull);
        let code = emit(&rendered(Exit::remote(7)), &mut { full }, &mut Vec::new());
        assert_eq!(code, ExitCode::from(7));
    }

    /// A failing stdout must not hide stderr, and the failure is told whatever the exit code.
    #[test]
    fn both_streams_are_attempted_and_a_write_failure_is_told_for_any_exit() {
        for exit in [Exit::Success, Exit::remote(7)] {
            let mut err = Vec::new();
            let full = Failing(io::ErrorKind::StorageFull);
            let _ = emit(&rendered(exit), &mut { full }, &mut err);
            let shown = String::from_utf8(err).unwrap();
            assert!(shown.starts_with("rr: note\n"), "{shown}");
            assert!(shown.contains("could not write"), "{shown}");
        }
        // a broken stdout hides no stderr either, and a broken stderr fails a success
        let mut err = Vec::new();
        let _ = emit(
            &rendered(Exit::Success),
            &mut Failing(io::ErrorKind::BrokenPipe),
            &mut err,
        );
        assert_eq!(err, b"rr: note\n");
        let code = emit(
            &rendered(Exit::Success),
            &mut Vec::new(),
            &mut Failing(io::ErrorKind::StorageFull),
        );
        assert_eq!(code, ExitCode::from(RrCode::Failure.code()));
    }

    #[test]
    fn what_is_rendered_is_written_in_order_to_its_streams() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = emit(&rendered(Exit::Success), &mut out, &mut err);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!((out, err), (b"{}\n".to_vec(), b"rr: note\n".to_vec()));
    }
}
