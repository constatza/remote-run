//! `rr x`: run a command in the remote project directory.

use super::{Output, Ports, stream};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::shell::{Script, quote};
use crate::ports::Terminal;
use anyhow::Result;

pub struct Args {
    pub terminal: Terminal,
    pub cmd: Vec<String>,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let body = Script::new().words(args.cmd.iter().map(|word| quote(word)));
    stream(ports, &session.place, body, args.terminal, Output::Raw)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, in_project, session};
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::outcome::{Format, render};
    use crate::ports::fake::RemoteCall;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn the_command_is_quoted_word_by_word_and_runs_in_the_project() {
        let fakes = Fakes::remote_streaming(vec![0, 0]);
        let cmd = words(&["echo", "a b", "$(id)"]);
        let args = Args {
            terminal: Terminal::Attached,
            cmd,
        };
        run(&session(), args, &fakes.ports()).unwrap();
        let plain = Args {
            terminal: Terminal::Plain,
            cmd: words(&["ls"]),
        };
        run(&session(), plain, &fakes.ports()).unwrap();
        let calls = fakes.remote.calls.borrow();
        assert_eq!(
            calls[0],
            (
                in_project("'echo' 'a b' '$(id)'"),
                RemoteCall::Stream(Terminal::Attached)
            )
        );
        assert_eq!(calls[1].1, RemoteCall::Stream(Terminal::Plain));
    }

    /// `x` shows the program's own bytes: its output never goes through the filter, whatever the
    /// terminal setting, so a tty session and `rr x -- cat file` keep working.
    #[test]
    fn the_output_of_x_is_raw() {
        let fakes = Fakes::remote_streaming(vec![0, 0]);
        fakes.remote.emit(b"\x1b[1mbold\x1b[0m\n");
        for terminal in [Terminal::Plain, Terminal::Attached] {
            let args = Args {
                terminal,
                cmd: words(&["cat", "log"]),
            };
            run(&session(), args, &fakes.ports()).unwrap();
        }
        let calls = fakes.remote.calls.borrow();
        assert!(
            calls
                .iter()
                .all(|(_, how)| matches!(how, RemoteCall::Stream(_)))
        );
        assert!(
            fakes.remote.shown.borrow().is_empty(),
            "nothing was filtered"
        );
    }

    /// The command's own exit code is passed through; it is never an `rr` code, even at 3.
    #[test]
    fn the_remote_exit_code_is_the_commands_not_rrs() {
        let fakes = Fakes::remote_streaming(vec![3]);
        let args = Args {
            terminal: Terminal::Plain,
            cmd: words(&["false"]),
        };
        let outcome = run(&session(), args, &fakes.ports()).unwrap();
        let exit = render(&outcome, Format::Text).exit;
        assert_eq!(exit, Exit::remote(3));
        assert_ne!(exit, Exit::Rr(RrCode::Refused));
    }
}
