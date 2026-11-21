//! `rr`: the command line. Every remote action goes through `remote_run::remote::Ssh`.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use remote_run::config::{Settings, load, user_file};
use remote_run::logpath::{SACCT_FIELDS, log_path, parse_sacct};
use remote_run::pull::{Commit, PullOutcome, Target, pull};
use remote_run::quote::{Place, join, quote, script};
use remote_run::remote::{Remote, Ssh};
use remote_run::transfer::{parse_size, parse_total, remote_path, rsync_argv};
use std::process::{Command, ExitCode};

/// Exit code for a refusal (a guard said no) or a stale environment.
const REFUSED: u8 = 3;
const QUEUE_FORMAT: &str = "%.12i %.40j %.9T %.10M %.6D %R";
const ACCT_FIELDS: &str = "JobID,JobName%40,State,Elapsed,ExitCode";

#[derive(Parser)]
#[command(
    name = "rr",
    version,
    about = "Run, submit, watch and copy on a remote Slurm cluster over ssh."
)]
struct Cli {
    /// ssh host or alias from ~/.ssh/config
    #[arg(long, env = "RR_HOST", global = true)]
    host: Option<String>,
    /// remote project directory, relative to the remote home or absolute
    #[arg(long, env = "RR_DIR", global = true)]
    dir: Option<String>,
    /// remote shell file sourced before every command (PATH, uv, Slurm settings)
    #[arg(long = "env", env = "RR_ENV", global = true)]
    env_file: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Your queued and running jobs (squeue --me)
    Q,
    /// Your recent jobs (sacct)
    Acct {
        /// sacct start time, e.g. now-3days or 2026-11-20
        #[arg(long, default_value = "now-1days")]
        since: String,
    },
    /// Show a job's log
    Log {
        job: String,
        /// follow it (tail -f; Ctrl-C stops)
        #[arg(short, long)]
        follow: bool,
        /// lines to show
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: u32,
    },
    /// Cancel jobs (scancel)
    Cancel {
        #[arg(required = true)]
        jobs: Vec<String>,
    },
    /// Run a command in the remote project directory
    X {
        /// allocate a terminal (interactive programs)
        #[arg(short, long)]
        tty: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Fast-forward the remote checkout to the local HEAD, with guards
    Pull {
        /// fast-forward even while jobs submitted from the checkout are active
        #[arg(long)]
        force: bool,
    },
    /// `pull`, then run a command in the remote project directory
    Run {
        #[arg(long)]
        force: bool,
        #[arg(short, long)]
        tty: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Copy from the remote (rsync); relative remote paths are inside the project directory
    Get {
        remote: String,
        #[arg(default_value = ".")]
        local: String,
        /// refuse a transfer larger than this (K, M, G)
        #[arg(long, default_value = "1G", value_parser = parse_size)]
        limit: u64,
    },
    /// Copy to the remote (rsync); never deletes
    Put {
        local: String,
        /// destination, default the project directory
        remote: Option<String>,
        #[arg(long, default_value = "1G", value_parser = parse_size)]
        limit: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("rr: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn exit(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

fn run(cli: Cli) -> Result<u8> {
    let given = Settings {
        host: cli.host,
        dir: cli.dir,
        env: cli.env_file,
    };
    let var = |name: &str| std::env::var(name).ok();
    let user = user_file(
        var("XDG_CONFIG_HOME").as_deref(),
        var("HOME").as_deref(),
        var("APPDATA").as_deref(),
    );
    let settings = given.over(load(&std::env::current_dir()?, user.as_deref())?);
    let host = settings.host.context(
        "no host: pass --host, set RR_HOST, or set host in .rr.toml or ~/.config/rr/config.toml",
    )?;
    let place = Place {
        env: settings.env,
        dir: settings.dir,
    };
    let ssh = Ssh { host: host.clone() };
    let home = Place {
        env: place.env.clone(),
        dir: None,
    };
    match cli.command {
        Cmd::Q => Ok(exit(ssh.stream(
            &script(&home, &format!("squeue --me -o {}", quote(QUEUE_FORMAT))),
            false,
        )?)),
        Cmd::Acct { since } => {
            let body = format!(
                "sacct -X -u \"$USER\" -S {} -o {ACCT_FIELDS}",
                quote(&since)
            );
            Ok(exit(ssh.stream(&script(&home, &body), false)?))
        }
        Cmd::Log { job, follow, lines } => show_log(&ssh, &home, &job, follow, lines),
        Cmd::Cancel { jobs } => Ok(exit(
            ssh.stream(&script(&home, &format!("scancel {}", join(&jobs))), false)?,
        )),
        Cmd::X { tty, cmd } => Ok(exit(ssh.stream(&script(&place, &join(&cmd)), tty)?)),
        Cmd::Pull { force } => report(pull(&ssh, &place, &local_target()?, force)?),
        Cmd::Run { force, tty, cmd } => match report(pull(&ssh, &place, &local_target()?, force)?)?
        {
            0 => Ok(exit(ssh.stream(&script(&place, &join(&cmd)), tty)?)),
            code => Ok(code),
        },
        Cmd::Get {
            remote,
            local,
            limit,
        } => transfer(
            &format!("{host}:{}", remote_path(place.dir.as_deref(), &remote)),
            &local,
            limit,
        ),
        Cmd::Put {
            local,
            remote,
            limit,
        } => {
            let target = match (remote, place.dir.as_deref()) {
                (Some(path), dir) => remote_path(dir, &path),
                (None, Some(dir)) => format!("{}/", dir.trim_end_matches('/')),
                (None, None) => bail!("no destination: pass one or set RR_DIR"),
            };
            transfer(&local, &format!("{host}:{target}"), limit)
        }
    }
}

/// The commit to reach: the local HEAD (warning about uncommitted work), else origin's default.
fn local_target() -> Result<Target> {
    let head = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let Ok(head) = head.as_ref().map(|out| (out.status.success(), &out.stdout)) else {
        return Ok(Target::OriginHead);
    };
    if !head.0 {
        return Ok(Target::OriginHead);
    }
    let commit = Commit::parse(&String::from_utf8_lossy(head.1))?;
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()?;
    if !status.stdout.is_empty() {
        eprintln!("rr: local uncommitted changes stay here; the remote gets {commit}");
    }
    Ok(Target::Local(commit))
}

fn report(outcome: PullOutcome) -> Result<u8> {
    match outcome {
        PullOutcome::UpToDate => Ok(0),
        PullOutcome::Advanced { from, to } => {
            eprintln!("rr: remote checkout {from} -> {to}");
            Ok(0)
        }
        PullOutcome::Refused(reason) => {
            eprintln!("rr: refused: {reason}");
            Ok(REFUSED)
        }
        PullOutcome::Stale => {
            eprintln!(
                "rr: the remote environment does not match uv.lock: run `uv sync` on the remote \
                 (it may download several GB)"
            );
            Ok(REFUSED)
        }
    }
}

fn show_log(ssh: &Ssh, home: &Place, job: &str, follow: bool, lines: u32) -> Result<u8> {
    let body = format!("sacct -n -X -P -j {} -o {SACCT_FIELDS}", quote(job));
    let record = ssh.capture(&script(home, &body))?;
    if record.code != 0 {
        bail!("sacct failed: {}", record.stderr.trim());
    }
    let path = log_path(&parse_sacct(&record.stdout)?)?;
    eprintln!("rr: {path}");
    let follow_flag = if follow { " -F" } else { "" };
    let tail = format!("tail -n {lines}{follow_flag} -- {}", quote(&path));
    Ok(exit(ssh.stream(&script(home, &tail), follow)?))
}

fn transfer(source: &str, destination: &str, limit: u64) -> Result<u8> {
    let probe = rsync_argv(source, destination, true);
    let out = Command::new(&probe[0])
        .args(&probe[1..])
        .output()
        .context("could not start rsync")?;
    if !out.status.success() {
        bail!(
            "rsync --dry-run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let total = parse_total(&String::from_utf8_lossy(&out.stdout))?;
    if total > limit {
        eprintln!("rr: refused: {total} bytes to copy, over the limit of {limit} (--limit)");
        return Ok(REFUSED);
    }
    let argv = rsync_argv(source, destination, false);
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("could not start rsync")?;
    Ok(exit(status.code().unwrap_or(1)))
}
