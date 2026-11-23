//! `rr`: parse the command line, build the environment and the session, hand one command to
//! [`app`](remote_run::app), render what it returns, print it and exit. The only place that
//! prints, reads the process environment or starts the real adapters.

// the shell reads the process environment once, and printing is its job
#![allow(clippy::disallowed_methods, clippy::print_stdout, clippy::print_stderr)]

mod cli;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Cmd, FwdArgs, JobCmd, RemoteCmd, StdoutUse};
use remote_run::adapters::{OsExec, OsNet, Ssh, SystemClock};
use remote_run::app::{self, Ports};
use remote_run::config::{Environment, Session, Settings, load, resolve, utf8_var};
use remote_run::domain::outcome::{Format, Outcome, render, render_error};
use remote_run::domain::slurm::JobSet;
use remote_run::ports::{Echo, Notes};
use remote_run::trust;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = cli.format;
    let rendered = match environment().and_then(|env| run(cli, &env)) {
        Ok(outcome) => render(&outcome, format),
        Err(err) => render_error(&err, format),
    };
    print!("{}", rendered.stdout);
    eprint!("{}", rendered.stderr);
    ExitCode::from(rendered.exit.code())
}

/// Prints each note at once, so it precedes the output of a command that runs after it.
struct StderrNotes;

impl Notes for StderrNotes {
    fn note(&self, line: &str) {
        eprintln!("{line}");
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
            "--json does not apply to a command that prints a program's own output (q, acct, log, \
             x, fwd, run without --wait): its text would not be one JSON document"
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
