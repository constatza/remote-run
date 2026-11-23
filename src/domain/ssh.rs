//! How `rr` reaches the remote host with `ssh`: the options every call uses and the argv that
//! runs one script.

use crate::domain::argv::Argv;
use crate::domain::host::Host;
use crate::domain::shell::{lit, quote};

/// ssh options every call uses: never prompt (a prompt nobody sees would hang), no banner.
pub const SSH_OPTIONS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "LogLevel=ERROR"];

/// Whether a streamed command gets a terminal on the remote side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    /// No terminal: output is piped, there is no Ctrl-C to forward.
    Plain,
    /// A terminal is allocated (`ssh -t`), so interactive programs work and Ctrl-C reaches them.
    Attached,
}

/// The `ssh` argv that runs `script` under bash on `host` (pure).
pub fn ssh_argv(host: &Host, script: &str, terminal: Terminal) -> Argv {
    let mut args: Vec<String> = SSH_OPTIONS.iter().map(|s| s.to_string()).collect();
    match terminal {
        Terminal::Attached => args.push("-t".into()),
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

    const OPTIONS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "LogLevel=ERROR"];

    /// The whole ssh command line, spelled out: options, then `--`, then the host, then one word
    /// that starts with `bash -c `.
    #[test]
    fn the_host_follows_the_options_and_a_double_dash() {
        let argv = ssh_argv(&host(), "true", Terminal::Plain);
        assert_eq!(argv.program(), "ssh");
        let expected: Vec<&str> = OPTIONS
            .into_iter()
            .chain(["--", "cluster", "bash -c 'true'"])
            .collect();
        assert_eq!(argv.args(), expected);
        let tty = ssh_argv(&host(), "true", Terminal::Attached);
        let expected: Vec<&str> = OPTIONS
            .into_iter()
            .chain(["-t", "--", "cluster", "bash -c 'true'"])
            .collect();
        assert_eq!(tty.args(), expected);
    }

    #[test]
    fn the_remote_command_is_always_one_word_starting_with_bash_c() {
        for script in ["true", "echo 'a b'", "a\nb", ""] {
            let argv = ssh_argv(&host(), script, Terminal::Plain);
            let last = argv.args().last().unwrap();
            assert!(last.starts_with("bash -c '"), "{last}");
            assert_eq!(argv.args().len(), 7);
        }
    }

    /// The remote login shell reads the last argument; the script must arrive as one word of a
    /// `bash -c`, unevaluated, whatever it contains.
    #[test]
    fn the_script_reaches_bash_as_one_unevaluated_word() {
        let marker = tempfile::tempdir().unwrap().keep().join("pwned");
        let script = format!("echo '$(touch {0})' `touch {0}`; it's", marker.display());
        let argv = ssh_argv(&host(), &script, Terminal::Plain);
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
