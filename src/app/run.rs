//! `rr run`: `pull`, then run a command in the remote project directory.

use super::wait::{Limits, run_then_wait};
use super::{Ports, exec, pull};
use crate::config::Session;
use crate::domain::outcome::{Outcome, pull_report};
use crate::domain::pull::{JobPolicy, Next, next};
use crate::ports::{Echo, Terminal};
use anyhow::Result;

/// What happens once the command has ended.
#[derive(Clone, Copy, Debug)]
pub enum After {
    Return,
    /// Wait for the jobs the command submitted, showing the command's output on `echo`.
    Wait {
        limits: Limits,
        echo: Echo,
    },
}

pub struct Args {
    pub policy: JobPolicy,
    pub terminal: Terminal,
    pub cmd: Vec<String>,
    pub after: After,
}

impl Args {
    pub fn new(policy: JobPolicy, terminal: Terminal, cmd: Vec<String>, after: After) -> Self {
        Self {
            policy,
            terminal,
            cmd,
            after,
        }
    }
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
            match args.after {
                After::Return => exec::run(session, command, ports),
                After::Wait { limits, echo } => {
                    run_then_wait(session, command, limits, echo, ports)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, in_project, session};
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::outcome::{Format, render};
    use crate::domain::wait::PollInterval;
    use crate::ports::fake::{RemoteCall, output};
    use std::time::Duration;

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
            after: After::Return,
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

    fn waiting() -> Args {
        let limits = Limits {
            timeout: Duration::from_secs(3600),
            poll: PollInterval::default(),
        };
        let after = After::Wait {
            limits,
            echo: Echo::Stdout,
        };
        Args { after, ..args() }
    }

    /// `After::Wait` is what turns `rr run` into `rr run --wait`: pull, run the command with its
    /// output read, then ask `sacct` for the job it printed.
    #[test]
    fn run_with_wait_pulls_then_runs_then_polls_the_submitted_job() {
        let behind = state(A, B, "");
        let done = "7\u{1f}COMPLETED\n";
        let captures = vec![(0, behind.as_str(), ""), (0, "", ""), (0, done, "")];
        let fakes = fakes(captures, vec![0]);
        fakes.remote.emit(b"Submitted batch job 7\n");
        let outcome = run(&session(), waiting(), &fakes.ports()).unwrap();
        let how: Vec<_> = fakes
            .remote
            .calls
            .borrow()
            .iter()
            .map(|(_, how)| *how)
            .collect();
        let tee = RemoteCall::StreamTee(Terminal::Plain, Echo::Stdout);
        let capture = RemoteCall::Capture;
        assert_eq!(
            how,
            [capture, capture, tee, RemoteCall::Poll],
            "state, merge, command, sacct"
        );
        assert!(matches!(outcome, Outcome::Waited(_)));
        assert_eq!(render(&outcome, Format::Text).exit, Exit::Success);
    }

    /// A pull that stopped is the result: the command is never run and nothing is waited for.
    #[test]
    fn a_refused_or_stale_pull_under_wait_makes_no_wait() {
        let dirty = state(A, B, "rr-state\tdirty\t M x\n");
        let behind = state(A, B, "");
        let stale = vec![
            (0, behind.as_str(), ""),
            (crate::domain::pull::STALE_EXIT, "", ""),
        ];
        for (name, captures) in [("dirty", vec![(0, dirty.as_str(), "")]), ("stale", stale)] {
            let asked = captures.len();
            let fakes = fakes(captures, vec![]);
            let outcome = run(&session(), waiting(), &fakes.ports()).unwrap();
            assert!(matches!(outcome, Outcome::Pulled(_)), "{name}");
            assert_eq!(fakes.remote.calls.borrow().len(), asked, "{name}");
            let waited = fakes
                .remote
                .scripts()
                .iter()
                .any(|script| script.contains("sacct"));
            assert!(!waited, "{name}");
        }
    }

    #[test]
    fn an_error_while_pulling_runs_nothing() {
        let fakes = fakes(vec![(255, "", "ssh: no route")], vec![]);
        assert!(run(&session(), args(), &fakes.ports()).is_err());
        assert_eq!(fakes.remote.calls.borrow().len(), 1);
    }
}
