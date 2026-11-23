//! `rr wait`: poll `sacct` until the jobs end, and `rr run --wait`: run a command, then wait for
//! the jobs it submitted.

use super::{Ports, exec};
use crate::config::Session;
use crate::domain::exit::RemoteExit;
use crate::domain::outcome::Outcome;
use crate::domain::place::script;
use crate::domain::slurm::{JobSet, SUBMITTED_PREFIX, WaitOutcome, states_script, submitted_jobs};
use crate::domain::wait::{Action, PollInterval, Progress, Reading, next_action};
use anyhow::Result;
use std::time::Duration;

/// How long to wait and how often to ask.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub poll: PollInterval,
}

pub struct Args {
    pub jobs: JobSet,
    pub limits: Limits,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let poll = script(&session.place.at_home(), states_script(&args.jobs));
    let start = ports.clock.now();
    let mut unreadable_polls = 0;
    loop {
        let reading = Reading::from_capture(ports.remote.capture(&poll))?;
        let progress = Progress {
            elapsed: ports.clock.now().since(start),
            timeout: args.limits.timeout,
            unreadable_polls,
        };
        match next_action(&reading, &args.jobs, progress) {
            Action::Done(outcome) => return Ok(Outcome::Waited(outcome)),
            Action::Poll {
                unreadable_polls: n,
            } => {
                unreadable_polls = n;
                ports.clock.sleep(args.limits.poll.duration());
            }
        }
    }
}

/// Run the command with its output shown as it comes, then wait for the jobs it submitted. A
/// command that failed is its own result. One that succeeded without a `Submitted batch job N`
/// line is "unknown", never success: nothing says what to wait for.
pub fn run_then_wait(
    session: &Session,
    command: exec::Args,
    limits: Limits,
    ports: &Ports,
) -> Result<Outcome> {
    let teed = exec::tee(session, command, SUBMITTED_PREFIX, ports)?;
    if teed.code != 0 {
        return Ok(Outcome::Remote(RemoteExit::new(teed.code)));
    }
    match submitted_jobs(&teed.matched) {
        Some(jobs) => run(session, Args { jobs, limits }, ports),
        None => Ok(Outcome::Waited(WaitOutcome::Unknown {
            why: format!("the command printed no {SUBMITTED_PREFIX:?}<id> line"),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, at_home, session};
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::outcome::{Format, render};
    use crate::domain::slurm::JobId;
    use crate::domain::wait::MAX_UNREADABLE_POLLS;
    use crate::ports::Terminal;
    use crate::ports::fake::RemoteCall;

    const HOUR: Duration = Duration::from_secs(3600);

    fn reply(id: &str, state: &str) -> String {
        format!("{id}\u{1f}{state}\n")
    }

    fn limits(timeout: Duration) -> Limits {
        Limits {
            timeout,
            poll: PollInterval::default(),
        }
    }

    fn args(timeout: Duration) -> Args {
        let jobs = JobSet::new(vec![JobId::parse("7").unwrap()]).unwrap();
        Args {
            jobs,
            limits: limits(timeout),
        }
    }

    /// What `rr wait 7` exits with, the lines it prints, and how long it slept.
    fn wait(captures: &[(i32, &str, &str)]) -> (Exit, String, Vec<Duration>) {
        let fakes = Fakes::new(vec![], vec![], captures.to_vec(), vec![]);
        let outcome = run(&session(), args(HOUR), &fakes.ports()).unwrap();
        let rendered = render(&outcome, Format::Text);
        assert!(fakes.remote.calls.borrow().len() == captures.len());
        (
            rendered.exit,
            rendered.stderr,
            fakes.clock.slept.borrow().clone(),
        )
    }

    #[test]
    fn each_poll_asks_sacct_at_home_for_the_states() {
        let done = reply("7", "COMPLETED");
        let fakes = Fakes::new(vec![], vec![], vec![(0, &done, "")], vec![]);
        run(&session(), args(HOUR), &fakes.ports()).unwrap();
        let expected = at_home(r#"sacct -n -X -P --delimiter=$'\x1f' -j '7' -o JobID,State"#);
        assert_eq!(
            *fakes.remote.calls.borrow(),
            [(expected, RemoteCall::Capture)]
        );
    }

    #[test]
    fn a_job_is_polled_while_pending_and_running_until_it_completes() {
        let (pending, running) = (reply("7", "PENDING"), reply("7", "RUNNING"));
        let done = reply("7", "COMPLETED");
        let (exit, report, slept) = wait(&[(0, &pending, ""), (0, &running, ""), (0, &done, "")]);
        assert_eq!(exit, Exit::Success);
        assert_eq!(report, "rr: 7 COMPLETED\n");
        assert_eq!(slept, [Duration::from_secs(30); 2]);
    }

    #[test]
    fn a_job_that_ended_in_any_other_state_exits_5() {
        let cases = [
            ("FAILED", "FAILED"),
            ("TIMEOUT", "TIMEOUT"),
            ("CANCELLED by 9", "CANCELLED"),
            ("OUT_OF_MEMORY", "OUT_OF_MEMORY"),
        ];
        for (state, shown) in cases {
            let text = reply("7", state);
            let (exit, report, slept) = wait(&[(0, &text, "")]);
            assert_eq!(exit, Exit::Rr(RrCode::JobFailed), "{state}");
            assert_eq!(exit.code(), 5);
            assert_eq!(report, format!("rr: 7 {shown}\n"));
            assert!(slept.is_empty());
        }
    }

    #[test]
    fn a_requeued_job_keeps_the_wait_going_until_it_completes() {
        let (requeued, running) = (reply("7", "REQUEUED"), reply("7", "RUNNING"));
        let done = reply("7", "COMPLETED");
        let (exit, _, slept) = wait(&[(0, &requeued, ""), (0, &running, ""), (0, &done, "")]);
        assert_eq!((exit, slept.len()), (Exit::Success, 2));
    }

    #[test]
    fn a_wait_that_outlasts_the_timeout_exits_4_with_the_states_it_saw() {
        let running = reply("7", "RUNNING");
        let captures = vec![(0, running.as_str(), ""); 4];
        let fakes = Fakes::new(vec![], vec![], captures, vec![]);
        let timeout = Duration::from_secs(90);
        let outcome = run(&session(), args(timeout), &fakes.ports()).unwrap();
        let rendered = render(&outcome, Format::Text);
        assert_eq!(rendered.exit, Exit::Rr(RrCode::Timeout));
        assert_eq!(rendered.exit.code(), 4);
        assert!(rendered.stderr.contains("rr: 7 RUNNING"), "{rendered:?}");
        assert_eq!(
            fakes.clock.slept.borrow().len(),
            3,
            "polls at 0, 30, 60, 90 s"
        );
    }

    /// Exit 6 and never 5: the link was down, the job may be fine.
    #[test]
    fn an_ssh_failure_mid_wait_is_retried_then_unknown_never_a_job_failure() {
        let running = reply("7", "RUNNING");
        let mut captures = vec![(0, running.as_str(), "")];
        captures.extend(vec![
            (255, "", "ssh: no route");
            MAX_UNREADABLE_POLLS as usize
        ]);
        let (exit, report, slept) = wait(&captures);
        assert_eq!(exit, Exit::Rr(RrCode::Unknown));
        assert_eq!(exit.code(), 6);
        assert!(report.contains("ssh: no route"), "{report}");
        assert_eq!(slept.len(), MAX_UNREADABLE_POLLS as usize);
    }

    #[test]
    fn a_blip_that_recovers_does_not_count_against_the_next_one() {
        let (running, done) = (reply("7", "RUNNING"), reply("7", "COMPLETED"));
        let blip = (255, "", "ssh: no route");
        let mut captures = vec![];
        for _ in 0..MAX_UNREADABLE_POLLS - 1 {
            captures.push(blip);
        }
        captures.push((0, running.as_str(), ""));
        for _ in 0..MAX_UNREADABLE_POLLS - 1 {
            captures.push(blip);
        }
        captures.push((0, done.as_str(), ""));
        assert_eq!(wait(&captures).0, Exit::Success);
    }

    /// `sacct` can lag behind a new submission; an empty answer is not "all jobs completed".
    #[test]
    fn an_empty_sacct_is_retried_then_unknown() {
        let captures = vec![(0, "", ""); MAX_UNREADABLE_POLLS as usize];
        let (exit, report, _) = wait(&captures);
        assert_eq!(exit, Exit::Rr(RrCode::Unknown));
        assert!(report.contains("could not tell"), "{report}");
    }

    #[test]
    fn an_empty_sacct_that_fills_in_is_followed_to_the_end() {
        let done = reply("7", "COMPLETED");
        let (exit, _, slept) = wait(&[(0, "", ""), (0, &done, "")]);
        assert_eq!((exit, slept.len()), (Exit::Success, 1));
    }

    fn command() -> exec::Args {
        exec::Args {
            terminal: Terminal::Plain,
            cmd: vec!["sbatch".into(), "job.sh".into()],
        }
    }

    #[test]
    fn run_then_wait_waits_for_every_job_the_command_submitted() {
        let done = "7\u{1f}COMPLETED\n8\u{1f}COMPLETED\n";
        let fakes = Fakes::new(vec![], vec![], vec![(0, done, "")], vec![0]);
        fakes
            .remote
            .emit(b"Submitted batch job 7\nSubmitted batch job 8\n");
        let outcome = run_then_wait(&session(), command(), limits(HOUR), &fakes.ports()).unwrap();
        assert_eq!(render(&outcome, Format::Text).exit, Exit::Success);
        let calls = fakes.remote.calls.borrow();
        assert_eq!(calls[0].1, RemoteCall::StreamTee(Terminal::Plain));
        assert!(calls[1].0.contains("-j '7,8'"), "{}", calls[1].0);
        assert_eq!(
            fakes.remote.shown.borrow().as_slice(),
            b"Submitted batch job 7\nSubmitted batch job 8\n",
            "the command's output is shown as it is"
        );
    }

    /// No match is unknown, never success; and a command's own failure is its own code.
    #[test]
    fn a_command_with_no_submit_line_is_unknown_and_a_failed_one_keeps_its_code() {
        let fakes = Fakes::remote_streaming(vec![0]);
        fakes.remote.emit(b"hello\n");
        let outcome = run_then_wait(&session(), command(), limits(HOUR), &fakes.ports()).unwrap();
        let rendered = render(&outcome, Format::Text);
        assert_eq!(rendered.exit, Exit::Rr(RrCode::Unknown));
        assert_eq!(fakes.remote.calls.borrow().len(), 1, "nothing to poll");

        let fakes = Fakes::remote_streaming(vec![3]);
        fakes.remote.emit(b"Submitted batch job 7\n");
        let outcome = run_then_wait(&session(), command(), limits(HOUR), &fakes.ports()).unwrap();
        let exit = render(&outcome, Format::Text).exit;
        assert_eq!(exit, Exit::remote(3));
        assert_ne!(exit, Exit::Rr(RrCode::Refused));
        assert_eq!(
            fakes.remote.calls.borrow().len(),
            1,
            "no wait after a failure"
        );
    }
}
