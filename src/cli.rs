//! The command line: what clap parses, and nothing else. Flags that take no value are read into
//! the enum they stand for, so no boolean reaches the commands.

use clap::{Parser, Subcommand};
use remote_run::app;
use remote_run::domain::fwd::{Browser, LoopbackPort, Transport, Ttl, Wanted};
use remote_run::domain::pull::JobPolicy;
use remote_run::domain::slurm::{Follow, JobId};
use remote_run::domain::transfer::parse_size;
use remote_run::ports::Terminal;
use remote_run::trust::TrustAction;

#[derive(Parser)]
#[command(
    name = "rr",
    version,
    about = "Run commands, update a checkout and copy files on a remote Slurm cluster over ssh."
)]
pub struct Cli {
    /// ssh host or alias from ~/.ssh/config
    #[arg(long, env = "RR_HOST", global = true)]
    pub host: Option<String>,
    /// remote project directory, relative to the remote home or absolute
    #[arg(long, env = "RR_DIR", global = true)]
    pub dir: Option<String>,
    /// remote shell file sourced before every command (PATH, uv, Slurm settings)
    #[arg(long = "env", env = "RR_ENV", global = true)]
    pub env_file: Option<String>,
    #[command(subcommand)]
    pub command: Cmd,
}

/// What `rr` can be asked to do. Only [`Cmd::Remote`] needs a host.
#[derive(Subcommand)]
pub enum Cmd {
    /// Review and trust the project file (.rr.toml) that applies here
    Trust {
        /// record the file as reviewed; without it, only show what would be trusted
        #[arg(long = "yes", num_args = 0, default_value = "show", default_missing_value = "record", value_parser = parse_trust)]
        action: TrustAction,
    },
    #[command(flatten)]
    Remote(RemoteCmd),
}

/// The commands that act on the remote host.
#[derive(Subcommand)]
pub enum RemoteCmd {
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
        #[arg(value_parser = JobId::parse)]
        job: JobId,
        /// follow it (tail -f; Ctrl-C stops)
        #[arg(short, long = "follow", num_args = 0, default_value = "once", default_missing_value = "follow", value_parser = parse_follow)]
        follow: Follow,
        /// lines to show
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: u32,
    },
    /// Cancel jobs (scancel)
    Cancel {
        #[arg(required = true, value_parser = JobId::parse)]
        jobs: Vec<JobId>,
    },
    /// Run a command in the remote project directory
    X {
        /// allocate a terminal (interactive programs)
        #[arg(short, long = "tty", num_args = 0, default_value = "plain", default_missing_value = "attached", value_parser = parse_terminal)]
        terminal: Terminal,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Fast-forward the remote checkout to the local HEAD, with guards
    Pull {
        /// fast-forward even while jobs submitted from the checkout are active
        #[arg(long = "force", num_args = 0, default_value = "respect", default_missing_value = "override", value_parser = parse_policy)]
        policy: JobPolicy,
    },
    /// `pull`, then run a command in the remote project directory
    Run {
        /// fast-forward even while jobs submitted from the checkout are active
        #[arg(long = "force", num_args = 0, default_value = "respect", default_missing_value = "override", value_parser = parse_policy)]
        policy: JobPolicy,
        #[arg(short, long = "tty", num_args = 0, default_value = "plain", default_missing_value = "attached", value_parser = parse_terminal)]
        terminal: Terminal,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Forward a cluster port to localhost over ssh (127.0.0.1 only; closes after --ttl)
    Fwd(FwdArgs),
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

/// `rr fwd`'s arguments.
#[derive(clap::Args)]
pub struct FwdArgs {
    /// host:port on the cluster side, or the name of a forward in the config
    #[arg(value_parser = Wanted::parse)]
    pub target: Wanted,
    /// local port (default: a free one)
    #[arg(long, value_parser = LoopbackPort::parse)]
    pub local: Option<LoopbackPort>,
    /// listen on a Unix socket in $XDG_RUNTIME_DIR that only you can open, not on TCP
    #[arg(long = "socket", num_args = 0, default_value = "tcp", default_missing_value = "socket", value_parser = parse_transport, conflicts_with = "local")]
    pub transport: Transport,
    /// open the URL in your browser once it works
    #[arg(long = "open", num_args = 0, default_value = "leave", default_missing_value = "open", value_parser = parse_browser)]
    pub browser: Browser,
    /// close the forward after this long (90s, 30m, 8h, up to 7d)
    #[arg(long, default_value = "8h", value_parser = Ttl::parse)]
    pub ttl: Ttl,
}

impl From<FwdArgs> for app::fwd::Args {
    fn from(args: FwdArgs) -> Self {
        Self {
            wanted: args.target,
            local: args.local,
            transport: args.transport,
            ttl: args.ttl,
            browser: args.browser,
        }
    }
}

/// A flag that takes no value reads as one of two variants: the first when it is absent, the
/// second when given. clap hands the chosen spelling to the parser, so the boolean never exists.
fn flag<T>(name: &str, text: &str, absent: (&str, T), present: (&str, T)) -> Result<T, String> {
    match text {
        _ if text == absent.0 => Ok(absent.1),
        _ if text == present.0 => Ok(present.1),
        other => Err(format!("{name} takes no value (got {other:?})")),
    }
}

/// `--force` is `override`; absent it is `respect`.
fn parse_policy(text: &str) -> Result<JobPolicy, String> {
    flag(
        "--force",
        text,
        ("respect", JobPolicy::Respect),
        ("override", JobPolicy::Override),
    )
}

/// `--tty` is `attached`; absent it is `plain`.
fn parse_terminal(text: &str) -> Result<Terminal, String> {
    flag(
        "--tty",
        text,
        ("plain", Terminal::Plain),
        ("attached", Terminal::Attached),
    )
}

/// `--follow` is `follow`; absent it is `once`.
fn parse_follow(text: &str) -> Result<Follow, String> {
    flag(
        "--follow",
        text,
        ("once", Follow::Once),
        ("follow", Follow::Follow),
    )
}

/// `--socket` is `socket`; absent it is `tcp`.
fn parse_transport(text: &str) -> Result<Transport, String> {
    flag(
        "--socket",
        text,
        ("tcp", Transport::Tcp),
        ("socket", Transport::Socket),
    )
}

/// `--open` is `open`; absent it is `leave`.
fn parse_browser(text: &str) -> Result<Browser, String> {
    flag(
        "--open",
        text,
        ("leave", Browser::Leave),
        ("open", Browser::Open),
    )
}

/// `--yes` is `record`; absent it is `show`.
fn parse_trust(text: &str) -> Result<TrustAction, String> {
    flag(
        "--yes",
        text,
        ("show", TrustAction::Show),
        ("record", TrustAction::Record),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tagline names only what is built: `submit` and `watch` are not commands yet.
    #[test]
    fn the_tagline_promises_no_unbuilt_command() {
        use clap::CommandFactory;
        let about = Cli::command().get_about().map(|text| text.to_string());
        let about = about.unwrap_or_default().to_lowercase();
        assert!(!about.is_empty());
        assert!(
            !about.contains("submit") && !about.contains("watch"),
            "{about}"
        );
    }

    fn refused(args: &[&str]) -> bool {
        let argv = ["rr", "--host", "h"]
            .into_iter()
            .chain(args.iter().copied());
        Cli::try_parse_from(argv).is_err()
    }

    fn policy_of(args: &[&str]) -> JobPolicy {
        let argv = ["rr", "--host", "h"]
            .into_iter()
            .chain(args.iter().copied());
        match Cli::try_parse_from(argv).unwrap().command {
            Cmd::Remote(RemoteCmd::Pull { policy } | RemoteCmd::Run { policy, .. }) => policy,
            _ => panic!("not a pull or a run"),
        }
    }

    #[test]
    fn force_is_the_only_way_to_override_the_jobs_guard() {
        assert_eq!(policy_of(&["pull"]), JobPolicy::Respect);
        assert_eq!(policy_of(&["pull", "--force"]), JobPolicy::Override);
        assert_eq!(policy_of(&["run", "--", "ls"]), JobPolicy::Respect);
        assert_eq!(
            policy_of(&["run", "--force", "--", "ls"]),
            JobPolicy::Override
        );
        assert_eq!(
            policy_of(&["run", "--force", "ls", "-la"]),
            JobPolicy::Override
        );
        assert!(refused(&["pull", "--force", "extra"]));
    }

    #[test]
    fn job_ids_typed_on_the_command_line_are_validated() {
        assert!(!refused(&["cancel", "12", "3_4"]));
        assert!(!refused(&["log", "12.batch"]));
        for bad in ["-u", "1;id", "a\u{1b}b", "$(id)", "1 2", ""] {
            assert!(refused(&["cancel", "--", bad]), "cancel {bad:?}");
            assert!(refused(&["log", "--", bad]), "log {bad:?}");
        }
        assert!(refused(&["cancel", "1", "--", "-u"]));
    }

    fn command_of(args: &[&str]) -> Cmd {
        let argv = ["rr", "--host", "h"]
            .into_iter()
            .chain(args.iter().copied());
        Cli::try_parse_from(argv).unwrap().command
    }

    #[test]
    fn a_flag_without_a_value_picks_one_of_two_variants() {
        let terminal = |args: &[&str]| match command_of(args) {
            Cmd::Remote(RemoteCmd::X { terminal, .. } | RemoteCmd::Run { terminal, .. }) => {
                terminal
            }
            _ => panic!("not an x or a run"),
        };
        assert_eq!(terminal(&["x", "ls"]), Terminal::Plain);
        assert_eq!(terminal(&["x", "-t", "ls"]), Terminal::Attached);
        assert_eq!(terminal(&["x", "--tty", "--", "ls"]), Terminal::Attached);
        assert_eq!(terminal(&["run", "--", "ls"]), Terminal::Plain);
        assert_eq!(terminal(&["run", "-t", "--", "ls"]), Terminal::Attached);
        let follow = |args: &[&str]| match command_of(args) {
            Cmd::Remote(RemoteCmd::Log { follow, .. }) => follow,
            _ => panic!("not a log"),
        };
        assert_eq!(follow(&["log", "1"]), Follow::Once);
        assert_eq!(follow(&["log", "-f", "1"]), Follow::Follow);
        assert_eq!(follow(&["log", "--follow", "1"]), Follow::Follow);
        let action = |args: &[&str]| match command_of(args) {
            Cmd::Trust { action } => action,
            _ => panic!("not a trust"),
        };
        assert_eq!(action(&["trust"]), TrustAction::Show);
        assert_eq!(action(&["trust", "--yes"]), TrustAction::Record);
        assert!(
            refused(&["x", "--tty=attached", "ls"]),
            "a flag takes no value"
        );
    }

    fn fwd_of(args: &[&str]) -> FwdArgs {
        let argv: Vec<&str> = ["fwd"].into_iter().chain(args.iter().copied()).collect();
        match command_of(&argv) {
            Cmd::Remote(RemoteCmd::Fwd(args)) => args,
            _ => panic!("not a fwd"),
        }
    }

    #[test]
    fn fwd_defaults_to_tcp_for_eight_hours_and_takes_flags_without_values() {
        let plain = fwd_of(&["gpu-01:5000"]);
        assert_eq!(plain.transport, Transport::Tcp);
        assert_eq!(plain.browser, Browser::Leave);
        assert_eq!(plain.ttl, Ttl::DEFAULT);
        assert_eq!(plain.local, None);
        let all = fwd_of(&["mlflow", "--open", "--ttl", "30m", "--local", "5051"]);
        assert_eq!(all.browser, Browser::Open);
        assert_eq!(all.ttl.to_string(), "30m");
        assert_eq!(all.local.map(|p| p.get()), Some(5051));
        assert_eq!(fwd_of(&["mlflow", "--socket"]).transport, Transport::Socket);
    }

    #[test]
    fn fwd_refuses_what_ssh_would_misread_before_anything_starts() {
        for bad in [
            &["fwd", "-oProxyCommand=x:1"][..],
            &["fwd", "--", "-h:80"],
            &["fwd", "h:0"],
            &["fwd", "../x"],
            &["fwd", "h:1", "--local", "0"],
            &["fwd", "h:1", "--ttl", "0s"],
            &["fwd", "h:1", "--ttl", "9d"],
            &["fwd", "h:1", "--socket", "--local", "5051"],
            &["fwd", "h:1", "--open=yes"],
            &["fwd"],
        ] {
            assert!(refused(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_flag_parser_refuses_a_spelling_that_is_neither_variant() {
        assert!(parse_terminal("tty").is_err());
        assert!(parse_follow("yes").is_err());
        assert!(parse_trust("true").is_err());
        assert!(parse_policy("1").is_err());
        assert!(parse_transport("unix").is_err());
        assert!(parse_browser("true").is_err());
        assert_eq!(
            parse_follow("x").unwrap_err(),
            "--follow takes no value (got \"x\")"
        );
    }
}
