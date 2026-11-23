//! How `rr` reaches the remote host with `ssh`: the options every call uses and the argv that
//! runs one script.

use crate::domain::argv::Argv;
use crate::domain::host::Host;
use crate::domain::shell::{lit, quote};

/// ssh options every call uses: never prompt (a prompt nobody sees would hang), no banner.
pub const SSH_OPTIONS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "LogLevel=ERROR"];

/// Bounds on a link that has gone silent, for the polls `rr wait` and `rr status` make (not the
/// forwards, which set their own keepalive). Without them one black-holed poll blocks `rr wait` for
/// ever, so neither `--timeout` nor the bounded retry of unreadable polls ever fires: a connect
/// gives up after `ConnectTimeout` seconds, and an established link that answers no keepalive for
/// `ServerAliveInterval * ServerAliveCountMax` (60) seconds is closed. They are not put on a
/// command the user runs (`rr x`, `run`, `log -f`): those may sit silent for longer than that over
/// a healthy link, and the user's own `~/.ssh/config` values must apply to them. The copy through
/// rsync is not covered either.
pub const SSH_LIVENESS_OPTIONS: [&str; 6] = [
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
];

/// Whether a streamed command gets a terminal on the remote side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    /// No terminal: output is piped, there is no Ctrl-C to forward.
    Plain,
    /// A terminal is allocated (`ssh -t`), so interactive programs work and Ctrl-C reaches them.
    Attached,
    /// No terminal even when `~/.ssh/config` says `RequestTTY force` (`ssh -T`): for a call whose
    /// stdin is data, which a remote tty would echo, translate and end at a control byte.
    Refused,
}

/// Whether ssh gives up on a silent link by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The user's own ssh configuration decides: for commands that may be quiet for long.
    Configured,
    /// [`SSH_LIVENESS_OPTIONS`] apply: for the short polls that must come back.
    Bounded,
}

/// The `ssh` argv that runs `script` under bash on `host` (pure).
pub fn ssh_argv(host: &Host, script: &str, terminal: Terminal, liveness: Liveness) -> Argv {
    let mut args: Vec<String> = SSH_OPTIONS.iter().map(|s| s.to_string()).collect();
    match liveness {
        Liveness::Configured => {}
        Liveness::Bounded => args.extend(SSH_LIVENESS_OPTIONS.iter().map(|s| s.to_string())),
    }
    match terminal {
        Terminal::Attached => args.push("-t".into()),
        Terminal::Refused => args.push("-T".into()),
        Terminal::Plain => {}
    }
    // `--` ends the options; `Host` already refuses a leading `-`, this keeps ssh from ever reading it as one
    args.push("--".into());
    args.push(host.as_str().into());
    // ssh hands the rest to the login shell as one string, so it is quoted once here
    args.push(lit("bash -c ").word(quote(script)).to_string());
    Argv::new("ssh", args)
}

#[allow(clippy::disallowed_methods)] // tests build real files
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::OsExec;
    use crate::domain::argv::Stdin;
    use crate::ports::Exec;

    fn host() -> Host {
        Host::parse("cluster").unwrap()
    }

    /// Spelled out, so a change to either option list shows up here.
    const OPTIONS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "LogLevel=ERROR"];
    const BOUNDED: [&str; 10] = [
        "-o",
        "BatchMode=yes",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
    ];

    /// The whole ssh command line, spelled out: options, then `--`, then the host, then one word
    /// that starts with `bash -c `.
    #[test]
    fn the_host_follows_the_options_and_a_double_dash() {
        let argv = ssh_argv(&host(), "true", Terminal::Plain, Liveness::Configured);
        assert_eq!(argv.program(), "ssh");
        let expected: Vec<&str> = OPTIONS
            .into_iter()
            .chain(["--", "cluster", "bash -c 'true'"])
            .collect();
        assert_eq!(argv.args(), expected);
        let tty = ssh_argv(&host(), "true", Terminal::Attached, Liveness::Configured);
        let expected: Vec<&str> = OPTIONS
            .into_iter()
            .chain(["-t", "--", "cluster", "bash -c 'true'"])
            .collect();
        assert_eq!(tty.args(), expected);
        let none = ssh_argv(&host(), "true", Terminal::Refused, Liveness::Configured);
        let expected: Vec<&str> = OPTIONS
            .into_iter()
            .chain(["-T", "--", "cluster", "bash -c 'true'"])
            .collect();
        assert_eq!(none.args(), expected);
    }

    /// A streamed command must keep the user's own `ServerAlive*` and `ConnectTimeout`: a stall of
    /// more than a minute on a healthy link must not kill `rr x` or `rr log -f`. Only a poll is
    /// bounded.
    #[test]
    fn only_a_bounded_call_carries_the_liveness_options() {
        for terminal in [Terminal::Plain, Terminal::Attached] {
            let free = ssh_argv(&host(), "true", terminal, Liveness::Configured);
            assert!(!free.args().iter().any(|a| a.contains("ServerAlive")));
            assert!(!free.args().iter().any(|a| a.contains("ConnectTimeout")));
        }
        let bounded = ssh_argv(&host(), "true", Terminal::Plain, Liveness::Bounded);
        let expected: Vec<&str> = BOUNDED
            .into_iter()
            .chain(["--", "cluster", "bash -c 'true'"])
            .collect();
        assert_eq!(bounded.args(), expected);
    }

    #[test]
    fn the_remote_command_is_always_one_word_starting_with_bash_c() {
        for script in ["true", "echo 'a b'", "a\nb", ""] {
            let argv = ssh_argv(&host(), script, Terminal::Plain, Liveness::Configured);
            let last = argv.args().last().unwrap();
            assert!(last.starts_with("bash -c '"), "{last}");
            assert_eq!(argv.args().len(), OPTIONS.len() + 3);
        }
    }

    /// The remote login shell reads the last argument; the script must arrive as one word of a
    /// `bash -c`, unevaluated, whatever it contains.
    #[test]
    fn the_script_reaches_bash_as_one_unevaluated_word() {
        let marker = tempfile::tempdir().unwrap().keep().join("pwned");
        let script = format!("echo '$(touch {0})' `touch {0}`; it's", marker.display());
        let argv = ssh_argv(&host(), &script, Terminal::Plain, Liveness::Configured);
        let remote_line = argv.args().last().unwrap();
        let shell = Argv::new(
            "sh",
            vec![
                "-c".into(),
                format!("set -- {remote_line}; printf %s \"$3\""),
            ],
        );
        let out = OsExec.capture(&shell, Stdin::Null).unwrap();
        assert_eq!(out.stdout, script);
        assert!(!marker.exists());
    }
}
