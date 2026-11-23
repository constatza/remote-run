//! `rr run`: `pull`, then run a command in the remote project directory.

use super::{Ports, exec, pull};
use crate::config::Session;
use crate::domain::outcome::{Outcome, pull_report};
use crate::domain::pull::{JobPolicy, Next, next};
use crate::ports::Terminal;
use anyhow::Result;

pub struct Args {
    pub policy: JobPolicy,
    pub terminal: Terminal,
    pub cmd: Vec<String>,
}

/// The command runs only when the pull reached the target: [`next`] is a closed match, so a
/// refusal or a stale environment can never fall through to running code.
pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let pulled = pull::to_local_head(session, args.policy, ports)?;
    match next(&pulled) {
        Next::Stop(_) => Ok(Outcome::Pulled(pulled)),
        Next::RunCommand => {
            pull_report(&pulled)
                .iter()
                .for_each(|line| ports.notes.note(line));
            let command = exec::Args {
                terminal: args.terminal,
                cmd: args.cmd,
            };
            exec::run(session, command, ports)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, in_project, session};
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::outcome::{Format, render};
    use crate::ports::fake::{RemoteCall, output};

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn state(head: &str, target: &str, extra: &str) -> String {
        format!(
            "rr-state\tdir\t/d\nrr-state\thead\t{head}\nrr-state\ttarget\t{target}\n\
             rr-state\tonorigin\t  origin/main\n{extra}"
        )
    }

    /// A clean local repository at `B`, and a remote that answers `captures`.
    fn fakes(captures: Vec<(i32, &str, &str)>, streams: Vec<i32>) -> Fakes {
        let git = vec![output(0, &format!("{B}\n"), ""), output(0, "", "")];
        Fakes::new(git, vec![], captures, streams)
    }

    fn args() -> Args {
        Args {
            policy: JobPolicy::Respect,
            terminal: Terminal::Plain,
            cmd: vec!["python".into(), "train.py".into()],
        }
    }

    #[test]
    fn a_pull_that_advanced_reports_first_then_runs_the_command_and_returns_its_code() {
        let behind = state(A, B, "");
        let fakes = fakes(vec![(0, &behind, ""), (0, "", "")], vec![7]);
        let outcome = run(&session(), args(), &fakes.ports()).unwrap();
        assert_eq!(
            *fakes.notes.0.borrow(),
            ["rr: remote checkout aaaaaaaaaaaa -> bbbbbbbbbbbb"]
        );
        let calls = fakes.remote.calls.borrow();
        assert_eq!(calls.len(), 3, "state, merge, command");
        assert_eq!(
            calls[2],
            (
                in_project("'python' 'train.py'"),
                RemoteCall::Stream(Terminal::Plain)
            )
        );
        assert_eq!(render(&outcome, Format::Text).exit, Exit::remote(7));
    }

    /// Every pull that did not reach the target stops before the command, with a refusal.
    #[test]
    fn a_refusal_or_a_stale_environment_never_runs_the_command() {
        let dirty = state(A, B, "rr-state\tdirty\t M x\n");
        let jobs = state(A, B, "rr-state\tjob\t7 /d\n");
        let behind = state(A, B, "");
        let stale = vec![
            (0, behind.as_str(), ""),
            (crate::domain::pull::STALE_EXIT, "", ""),
        ];
        for (name, captures) in [
            ("dirty", vec![(0, dirty.as_str(), "")]),
            ("jobs", vec![(0, jobs.as_str(), "")]),
            ("stale", stale),
        ] {
            let fakes = fakes(captures, vec![]);
            let outcome = run(&session(), args(), &fakes.ports()).unwrap();
            assert!(matches!(outcome, Outcome::Pulled(_)), "{name}");
            let exit = render(&outcome, Format::Text).exit;
            assert_eq!(exit, Exit::Rr(RrCode::Refused), "{name}");
            let streamed = fakes
                .remote
                .calls
                .borrow()
                .iter()
                .any(|(_, how)| matches!(how, RemoteCall::Stream(_)));
            assert!(!streamed, "{name} must not run the command");
        }
    }

    #[test]
    fn an_error_while_pulling_runs_nothing() {
        let fakes = fakes(vec![(255, "", "ssh: no route")], vec![]);
        assert!(run(&session(), args(), &fakes.ports()).is_err());
        assert_eq!(fakes.remote.calls.borrow().len(), 1);
    }
}
