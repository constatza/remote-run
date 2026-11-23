//! The decisions of `rr wait`, as pure functions: what `sacct` printed, what to do about it, and how
//! often to ask. The loop that asks and sleeps is in `app::wait`.

use crate::domain::argv::Captured;
use crate::domain::sanitize::sanitize;
use crate::domain::slurm::{
    Finished, JobId, JobSet, JobState, JobStatus, SACCT_SEPARATOR, SqueueId, WaitOutcome,
};
use anyhow::{Result, bail};
// `core`, not `std`: a Duration is a plain value; the clock itself lives in `ports`
use core::time::Duration;

/// The shortest time between two `sacct` calls. A tight loop of ssh calls against a shared login
/// node is abusive and can trip the cluster's rate limits.
pub const MIN_POLL: Duration = Duration::from_secs(5);

/// The default time between two `sacct` calls.
pub const DEFAULT_POLL: Duration = Duration::from_secs(30);

/// The default `--timeout`. A finite value, so a state the table does not know cannot hang a wait.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// How many polls in a row may fail to tell the jobs' states (ssh down, `sacct` failing or not
/// yet listing a job) before the wait gives up with "unknown". A just-submitted job can be missing
/// from `sacct` for a short while; a dead link must not wait for the whole timeout.
pub const MAX_UNREADABLE_POLLS: u32 = 5;

/// The time between two polls: never shorter than [`MIN_POLL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollInterval(Duration);

impl PollInterval {
    pub fn from_secs(secs: u64) -> Result<Self> {
        let interval = Duration::from_secs(secs);
        if interval < MIN_POLL {
            bail!(
                "a poll interval of {secs}s is too short: the minimum is {}s",
                MIN_POLL.as_secs()
            );
        }
        Ok(Self(interval))
    }

    pub fn duration(self) -> Duration {
        self.0
    }
}

impl Default for PollInterval {
    fn default() -> Self {
        Self(DEFAULT_POLL)
    }
}

/// The `JobID,State` lines of `sacct -n -X -P` (pure). A line that does not have exactly the two
/// fields is an error: the wait must not guess at a state.
pub fn parse_states(text: &str) -> Result<Vec<JobStatus>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(SACCT_SEPARATOR).collect();
            let [id, state] = fields[..] else {
                bail!("unexpected sacct line: {line:?}");
            };
            Ok(JobStatus {
                id: SqueueId::parse(id)?,
                state: JobState::parse(state),
            })
        })
        .collect()
}

/// What one poll found out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    Statuses(Vec<JobStatus>),
    /// The poll could not tell; `why` is the cause, as text from the remote.
    Unreadable(String),
}

impl Reading {
    /// An ssh failure, a non-zero `sacct` and a line that does not parse are all "cannot ask",
    /// never "the job failed".
    pub fn from_capture(result: Result<Captured>) -> Self {
        let stdout = result.and_then(|captured| captured.into_stdout("sacct"));
        match stdout.and_then(|text| parse_states(&text)) {
            Ok(rows) => Reading::Statuses(rows),
            Err(err) => Reading::Unreadable(format!("{err:#}")),
        }
    }
}

/// Where a wait stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Time since the wait began.
    pub elapsed: Duration,
    pub timeout: Duration,
    /// Polls in a row, this one included, that could not tell the states.
    pub unreadable_polls: u32,
}

/// What the loop does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Stop with this result.
    Done(WaitOutcome),
    /// Sleep one interval and ask again.
    Poll,
}

/// Whether `row` is a state of `job`: the job itself, one of its array tasks (`123_4`) or a
/// heterogeneous component (`123+1`). A step suffix on the requested id is ignored because `sacct
/// -X` lists whole allocations.
fn is_row_of(job: &JobId, row: &SqueueId) -> bool {
    let job = job.as_str().split('.').next().unwrap_or_default();
    let row = row.as_str();
    row == job
        || row
            .strip_prefix(job)
            .is_some_and(|rest| rest.starts_with(['_', '+']))
}

fn covers(rows: &[JobStatus], jobs: &JobSet) -> bool {
    jobs.as_slice()
        .iter()
        .all(|job| rows.iter().any(|row| is_row_of(job, &row.id)))
}

fn ended_badly(row: &JobStatus) -> bool {
    matches!(&row.state, JobState::Finished(state) if *state != Finished::Completed)
}

fn completed(row: &JobStatus) -> bool {
    row.state == JobState::Finished(Finished::Completed)
}

/// Decide what to do after a poll (pure). A job that ended in any state but COMPLETED ends the wait
/// at once, because a job that depends on it may never start. A job missing from `sacct` (accounting
/// lags a new submission) is not a failure, and a state the table does not know is not an end:
/// both are retried, the first a bounded number of times, the second until the timeout.
pub fn next_action(reading: &Reading, jobs: &JobSet, progress: Progress) -> Action {
    let timed_out = progress.elapsed >= progress.timeout;
    let rows = match reading {
        Reading::Statuses(rows) if covers(rows, jobs) => rows,
        Reading::Statuses(rows) => {
            let why = "sacct does not list every job yet";
            return unreadable(why, rows, progress);
        }
        Reading::Unreadable(why) => return unreadable(&sanitize(why), &[], progress),
    };
    if rows.iter().any(ended_badly) {
        Action::Done(WaitOutcome::Failed(rows.clone()))
    } else if rows.iter().all(completed) {
        Action::Done(WaitOutcome::Completed(rows.clone()))
    } else if timed_out {
        Action::Done(WaitOutcome::TimedOut(rows.clone()))
    } else {
        Action::Poll
    }
}

fn unreadable(why: &str, seen: &[JobStatus], progress: Progress) -> Action {
    if progress.unreadable_polls >= MAX_UNREADABLE_POLLS {
        Action::Done(WaitOutcome::Unknown { why: why.into() })
    } else if progress.elapsed >= progress.timeout {
        Action::Done(WaitOutcome::TimedOut(seen.to_vec()))
    } else {
        Action::Poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::slurm::Active;

    fn jobs(ids: &[&str]) -> JobSet {
        JobSet::new(ids.iter().map(|id| JobId::parse(id).unwrap()).collect()).unwrap()
    }

    fn rows(rows: &[(&str, &str)]) -> Vec<JobStatus> {
        rows.iter()
            .map(|(id, state)| JobStatus {
                id: SqueueId::parse(id).unwrap(),
                state: JobState::parse(state),
            })
            .collect()
    }

    fn seen(rows_: &[(&str, &str)]) -> Reading {
        Reading::Statuses(rows(rows_))
    }

    const HOUR: Duration = Duration::from_secs(3600);

    fn at(elapsed: Duration, unreadable_polls: u32) -> Progress {
        Progress {
            elapsed,
            timeout: HOUR,
            unreadable_polls,
        }
    }

    fn early() -> Progress {
        at(Duration::from_secs(60), 0)
    }

    #[test]
    fn a_poll_interval_below_the_minimum_is_refused() {
        assert!(PollInterval::from_secs(4).is_err());
        assert!(PollInterval::from_secs(0).is_err());
        assert_eq!(PollInterval::from_secs(5).unwrap().duration(), MIN_POLL);
        assert_eq!(PollInterval::default().duration(), DEFAULT_POLL);
    }

    #[test]
    fn active_jobs_are_polled_again() {
        for state in ["PENDING", "RUNNING", "COMPLETING", "REQUEUED", "SUSPENDED"] {
            let action = next_action(&seen(&[("1", state)]), &jobs(&["1"]), early());
            assert_eq!(action, Action::Poll, "{state}");
        }
    }

    #[test]
    fn completed_jobs_end_the_wait_successfully() {
        let action = next_action(&seen(&[("1", "COMPLETED")]), &jobs(&["1"]), early());
        assert_eq!(
            action,
            Action::Done(WaitOutcome::Completed(rows(&[("1", "COMPLETED")])))
        );
    }

    #[test]
    fn every_finished_state_but_completed_is_a_failure() {
        for state in [
            "FAILED",
            "TIMEOUT",
            "CANCELLED by 5",
            "DEADLINE",
            "OUT_OF_MEMORY",
            "NODE_FAIL",
            "BOOT_FAIL",
            "PREEMPTED",
            "REVOKED",
        ] {
            let action = next_action(&seen(&[("1", state)]), &jobs(&["1"]), early());
            assert!(
                matches!(action, Action::Done(WaitOutcome::Failed(_))),
                "{state}"
            );
        }
    }

    /// A job that depends on the failed one can stay pending for ever.
    #[test]
    fn one_failure_ends_the_wait_even_while_another_job_runs() {
        let reading = seen(&[("1", "RUNNING"), ("2", "FAILED")]);
        let action = next_action(&reading, &jobs(&["1", "2"]), early());
        assert!(matches!(action, Action::Done(WaitOutcome::Failed(_))));
    }

    #[test]
    fn a_requeued_job_is_not_the_end() {
        let jobs = jobs(&["1"]);
        let first = next_action(&seen(&[("1", "REQUEUED")]), &jobs, early());
        let second = next_action(&seen(&[("1", "RUNNING")]), &jobs, early());
        let third = next_action(&seen(&[("1", "COMPLETED")]), &jobs, early());
        assert_eq!(
            (first, second),
            (Action::Poll, Action::Poll),
            "requeue is active"
        );
        assert!(matches!(third, Action::Done(WaitOutcome::Completed(_))));
    }

    #[test]
    fn active_jobs_past_the_timeout_end_as_timed_out_with_their_states() {
        let reading = seen(&[("1", "RUNNING")]);
        let action = next_action(&reading, &jobs(&["1"]), at(HOUR, 0));
        assert_eq!(
            action,
            Action::Done(WaitOutcome::TimedOut(rows(&[("1", "RUNNING")])))
        );
    }

    #[test]
    fn an_unknown_state_is_never_the_end_and_is_reported_at_the_timeout() {
        let reading = seen(&[("1", "BRAND_NEW")]);
        assert_eq!(next_action(&reading, &jobs(&["1"]), early()), Action::Poll);
        let Action::Done(WaitOutcome::TimedOut(last)) =
            next_action(&reading, &jobs(&["1"]), at(HOUR, 0))
        else {
            panic!("an unknown state past the timeout is a timeout");
        };
        assert_eq!(last[0].state, JobState::Unknown("BRAND_NEW".into()));
    }

    #[test]
    fn a_failure_wins_over_a_timeout_that_passed_at_the_same_moment() {
        let reading = seen(&[("1", "FAILED")]);
        let action = next_action(&reading, &jobs(&["1"]), at(HOUR, 0));
        assert!(matches!(action, Action::Done(WaitOutcome::Failed(_))));
    }

    #[test]
    fn a_poll_that_cannot_tell_is_retried_a_bounded_number_of_times_then_unknown() {
        let reading = Reading::Unreadable("ssh: no route".into());
        let jobs = jobs(&["1"]);
        for polls in 1..MAX_UNREADABLE_POLLS {
            let action = next_action(&reading, &jobs, at(early().elapsed, polls));
            assert_eq!(action, Action::Poll, "{polls}");
        }
        let action = next_action(&reading, &jobs, at(early().elapsed, MAX_UNREADABLE_POLLS));
        assert_eq!(
            action,
            Action::Done(WaitOutcome::Unknown {
                why: "ssh: no route".into()
            })
        );
    }

    /// `sacct` lags a new submission: an empty answer, or one without all the jobs, is not a result.
    #[test]
    fn missing_jobs_are_retried_and_never_read_as_all_completed() {
        let jobs = jobs(&["1", "2"]);
        let empty = seen(&[]);
        let partial = seen(&[("1", "COMPLETED")]);
        for reading in [&empty, &partial] {
            assert_eq!(
                next_action(reading, &jobs, at(Duration::ZERO, 1)),
                Action::Poll
            );
            let last = next_action(reading, &jobs, at(Duration::ZERO, MAX_UNREADABLE_POLLS));
            assert!(matches!(last, Action::Done(WaitOutcome::Unknown { .. })));
        }
    }

    #[test]
    fn remote_text_in_the_reason_is_sanitized() {
        let reading = Reading::Unreadable("down\u{1b}[2J".into());
        let action = next_action(
            &reading,
            &jobs(&["1"]),
            at(Duration::ZERO, MAX_UNREADABLE_POLLS),
        );
        assert_eq!(
            action,
            Action::Done(WaitOutcome::Unknown {
                why: "down?[2J".into()
            })
        );
    }

    #[test]
    fn array_tasks_and_hetjob_components_answer_for_their_job() {
        let reading = seen(&[
            ("5_1", "COMPLETED"),
            ("5_2", "COMPLETED"),
            ("6+0", "COMPLETED"),
        ]);
        let action = next_action(&reading, &jobs(&["5", "6"]), early());
        assert!(matches!(action, Action::Done(WaitOutcome::Completed(_))));
        // `55` is not a task of `5`
        let other = seen(&[("55", "COMPLETED")]);
        let action = next_action(&other, &jobs(&["5"]), at(Duration::ZERO, 1));
        assert_eq!(action, Action::Poll);
        // a step on the requested id still finds the allocation
        let step = seen(&[("7", "RUNNING")]);
        assert_eq!(
            next_action(&step, &jobs(&["7.batch"]), early()),
            Action::Poll
        );
    }

    #[test]
    fn states_are_read_from_the_unit_separated_lines() {
        let parsed = parse_states("1\u{1f}RUNNING\n\n2_3\u{1f}CANCELLED by 9\n").unwrap();
        assert_eq!(parsed, rows(&[("1", "RUNNING"), ("2_3", "CANCELLED by 9")]));
        assert_eq!(
            parsed[0].state,
            JobState::Active(Active::Running),
            "known states are typed"
        );
        assert!(parse_states("").unwrap().is_empty());
        for bad in ["1", "1\u{1f}A\u{1f}B", "1 2\u{1f}RUNNING", "\u{1f}RUNNING"] {
            assert!(parse_states(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_failed_call_or_a_bad_line_is_unreadable_never_a_job_failure() {
        let captured = |code, stdout: &str, stderr: &str| {
            Ok(Captured {
                code,
                stdout: stdout.into(),
                stderr: stderr.into(),
            })
        };
        assert!(matches!(
            Reading::from_capture(captured(0, "1\u{1f}FAILED\n", "")),
            Reading::Statuses(_)
        ));
        let Reading::Unreadable(why) = Reading::from_capture(captured(255, "", "ssh: no route"))
        else {
            panic!("a failed ssh is unreadable");
        };
        assert!(
            why.contains("255") && why.contains("ssh: no route"),
            "{why}"
        );
        let bad = Reading::from_capture(captured(0, "garbage\n", ""));
        assert!(matches!(bad, Reading::Unreadable(_)));
        let wont_start = Reading::from_capture(Err(anyhow::anyhow!("could not start ssh")));
        assert!(matches!(wont_start, Reading::Unreadable(_)));
    }
}
