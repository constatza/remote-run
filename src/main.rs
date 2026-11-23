//! `rr`: parse the command line, build the environment and the session, hand one command to
//! [`app`](remote_run::app), render what it returns, print it and exit. The only place that
//! prints, reads the process environment or starts the real adapters.

// the shell reads the process environment once, and printing is its job
#![allow(clippy::disallowed_methods, clippy::print_stdout, clippy::print_stderr)]

mod cli;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Cmd, RemoteCmd};
use remote_run::adapters::OsExec;
use remote_run::adapters::Ssh;
use remote_run::app::{self, Ports};
use remote_run::config::{Environment, Session, Settings, load, resolve, utf8_var};
use remote_run::domain::outcome::{Format, Outcome, render, render_error};
use remote_run::ports::Notes;
use remote_run::trust;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rendered = match environment().and_then(|env| run(cli, &env)) {
        Ok(outcome) => render(&outcome, Format::Text),
        Err(err) => render_error(&err),
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

fn run(cli: Cli, env: &Environment) -> Result<Outcome> {
    let command = match cli.command {
        Cmd::Trust { action } => return app::trust::run(env, action),
        Cmd::Remote(command) => command,
    };
    let given = Settings {
        host: cli.host,
        dir: cli.dir,
        env: cli.env_file,
    };
    let session = session(given, env)?;
    let exec = OsExec;
    let ssh = Ssh::new(&session.host, &exec);
    let ports = Ports {
        exec: &exec,
        remote: &ssh,
        notes: &StderrNotes,
    };
    dispatch(command, &session, &ports)
}

fn dispatch(command: RemoteCmd, session: &Session, ports: &Ports) -> Result<Outcome> {
    match command {
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
            cmd,
        } => {
            let args = app::run::Args {
                policy,
                terminal,
                cmd,
            };
            app::run::run(session, args, ports)
        }
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
