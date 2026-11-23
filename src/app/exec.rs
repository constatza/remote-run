//! `rr x`: run a command in the remote project directory.

use super::{Output, Ports, stream};
use crate::config::Session;
use crate::domain::exit::RemoteExit;
use crate::domain::interp::Interpreter;
use crate::domain::outcome::Outcome;
use crate::domain::place::script;
use crate::domain::shell::{Script, quote};
use crate::ports::{Echo, Payload, Teed, Terminal};
use anyhow::Result;

pub struct Args {
    pub terminal: Terminal,
    pub cmd: Vec<String>,
}

/// The largest script `rr x -s` sends: a script is read whole, so a device or a dataset named by
/// mistake must not fill memory.
pub const SCRIPT_LIMIT: u64 = 1 << 20;

/// A script and the interpreter that reads it from stdin.
pub struct ScriptArgs {
    pub interp: Interpreter,
    pub source: Payload,
}

/// Run `args.source` with the interpreter in the remote home (no project directory, no pull: the
/// script is data that comes with the call). Raw output, like [`run`].
pub fn run_script(session: &Session, args: ScriptArgs, ports: &Ports) -> Result<Outcome> {
    let body = Script::new().words(args.interp.words().map(quote));
    let script = script(&session.place.at_home(), body);
    let code = ports.remote.send(&script, &args.source)?;
    Ok(Outcome::Remote(RemoteExit::new(code)))
}

fn body(args: &Args) -> Script {
    Script::new().words(args.cmd.iter().map(|word| quote(word)))
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    stream(
        ports,
        &session.place,
        body(&args),
        args.terminal,
        Output::Raw,
    )
}

/// Like [`run`], with the output shown on `echo` and also read for the lines that start with
/// `prefix`.
pub fn tee(session: &Session, args: Args, prefix: &str, echo: Echo, ports: &Ports) -> Result<Teed> {
    let script = script(&session.place, body(&args));
    ports
        .remote
        .stream_tee(&script, args.terminal, prefix, echo)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, at_home, in_project, session};
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

    #[test]
    fn a_script_goes_on_stdin_to_the_quoted_interpreter_in_the_home_directory() {
        let fakes = Fakes::remote_streaming(vec![5]);
        let args = ScriptArgs {
            interp: Interpreter::parse("python -u '$(id)'").unwrap(),
            source: Payload::new(b"print(1)\n".to_vec()),
        };
        let outcome = run_script(&session(), args, &fakes.ports()).unwrap();
        assert_eq!(fakes.remote.scripts(), [at_home("'python' '-u' '$(id)'")]);
        assert_eq!(*fakes.remote.sent.borrow(), [b"print(1)\n".to_vec()]);
        let exit = render(&outcome, Format::Text).exit;
        assert_eq!(exit, Exit::remote(5), "the remote's code, not an rr code");
        let calls = fakes.remote.calls.borrow();
        assert!(matches!(calls[0].1, RemoteCall::Send));
    }
}
