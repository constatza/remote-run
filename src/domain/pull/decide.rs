//! The pure judgement of a remote state, and the permit that is the only way to a merge.

use super::Target;
use super::state::RemoteState;
use crate::domain::ids::Commit;
use crate::domain::sanitize::sanitize;
use crate::domain::slurm::SqueueId;
use std::fmt;

/// Whether jobs submitted from the checkout hold a pull back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobPolicy {
    /// Refuse while a queued or running job was submitted from the checkout.
    Respect,
    /// Fast-forward anyway (`--force`); lifts only the jobs guard.
    Override,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    NotOnOrigin(Target),
    RemoteDirty(Vec<String>),
    JobsFromCheckout(Vec<SqueueId>),
    /// The remote checkout moved or became dirty between reading it and merging: the merge script
    /// found `HEAD` or the tree different from what [`decide`] judged, and did not merge.
    StateChanged,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NotOnOrigin(target) => {
                write!(f, "{target} is not on the remote's origin: push it first")
            }
            // `git status` prints file names, which are remote text
            Reason::RemoteDirty(lines) => write!(
                f,
                "the remote checkout has uncommitted changes, which a job would run:\n  {}",
                sanitize(&lines.join("\n  "))
            ),
            Reason::JobsFromCheckout(ids) => {
                let ids: Vec<&str> = ids.iter().map(SqueueId::as_str).collect();
                write!(
                    f,
                    "jobs {} were submitted from this checkout; a requeue would run the new code \
                     (wait, or --force)",
                    ids.join(", ")
                )
            }
            Reason::StateChanged => f.write_str(
                "the remote checkout changed while rr was working (a commit or an edit landed); \
                 nothing was merged, run the command again",
            ),
        }
    }
}

/// Proof that [`decide`] allowed fast-forwarding from `from` to `to`. The fields are private to
/// this module and only [`decide`] builds one, so a merge script cannot be made from a refused or
/// unread state.
///
/// ```compile_fail,E0451
/// use remote_run::domain::pull::{Commit, Permit};
/// fn forge(from: Commit, to: Commit) -> Permit {
///     Permit { from, to }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permit {
    from: Commit,
    to: Commit,
}

impl Permit {
    pub fn from(&self) -> &Commit {
        &self.from
    }

    pub fn to(&self) -> &Commit {
        &self.to
    }

    /// A permit for a test's expected value; production code gets one only from [`decide`].
    #[cfg(test)]
    pub(super) fn between(from: Commit, to: Commit) -> Self {
        Self { from, to }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullDecision {
    UpToDate,
    FastForward(Permit),
    Refused(Reason),
}

/// Judge the remote state against the target (pure).
pub fn decide(target: &Target, state: &RemoteState, policy: JobPolicy) -> PullDecision {
    let Some(to) = &state.target else {
        return PullDecision::Refused(Reason::NotOnOrigin(target.clone()));
    };
    if !state.dirty.is_empty() {
        return PullDecision::Refused(Reason::RemoteDirty(state.dirty.clone()));
    }
    if &state.head == to {
        return PullDecision::UpToDate;
    }
    let blocking: Vec<SqueueId> = state
        .jobs
        .iter()
        .filter(|job| job.is_inside(&state.dir))
        .map(|job| job.id.clone())
        .collect();
    match (blocking.is_empty(), policy) {
        (false, JobPolicy::Respect) => PullDecision::Refused(Reason::JobsFromCheckout(blocking)),
        (true, _) | (false, JobPolicy::Override) => PullDecision::FastForward(Permit {
            from: state.head.clone(),
            to: to.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{Job, Workdir};
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn commit(text: &str) -> Commit {
        Commit::parse(text).unwrap()
    }

    fn job(id: &str, workdir: &str) -> Job {
        Job {
            id: SqueueId::parse(id).unwrap(),
            workdir: Workdir::Resolved(workdir.into()),
        }
    }

    fn state(head: &str, target: Option<&str>, dirty: &[&str], jobs: Vec<Job>) -> RemoteState {
        RemoteState {
            dir: "/work/u/proj".into(),
            head: commit(head),
            target: target.map(commit),
            dirty: dirty.iter().map(|s| s.to_string()).collect(),
            jobs,
        }
    }

    fn ff() -> PullDecision {
        PullDecision::FastForward(Permit::between(commit(A), commit(B)))
    }

    #[test]
    fn what_is_allowed() {
        use JobPolicy::{Override, Respect};
        let target = Target::Local(commit(B));
        let here = || vec![job("7", "/work/u/proj"), job("8_1", "/work/u/proj/bin")];
        let elsewhere = || vec![job("9", "/work/u/proj-runs"), job("10", "/work/u")];
        let cases = [
            (
                "up to date",
                state(B, Some(B), &[], here()),
                Respect,
                PullDecision::UpToDate,
            ),
            (
                "fast-forward",
                state(A, Some(B), &[], vec![]),
                Respect,
                ff(),
            ),
            (
                "jobs elsewhere",
                state(A, Some(B), &[], elsewhere()),
                Respect,
                ff(),
            ),
            (
                "overridden jobs",
                state(A, Some(B), &[], here()),
                Override,
                ff(),
            ),
        ];
        for (name, remote, policy, expected) in cases {
            assert_eq!(decide(&target, &remote, policy), expected, "{name}");
        }
    }

    #[test]
    fn what_is_refused_and_in_which_order() {
        use JobPolicy::{Override, Respect};
        let target = Target::Local(commit(B));
        let here = || vec![job("7", "/work/u/proj"), job("8_1", "/work/u/proj/bin")];
        let refused = |reason| PullDecision::Refused(reason);
        let jobs = Reason::JobsFromCheckout(vec![
            SqueueId::parse("7").unwrap(),
            SqueueId::parse("8_1").unwrap(),
        ]);
        let cases = [
            (
                "not on origin",
                state(A, None, &[], vec![]),
                Respect,
                refused(Reason::NotOnOrigin(target.clone())),
            ),
            (
                "not on origin before dirty",
                state(A, None, &[" M x"], vec![]),
                Override,
                refused(Reason::NotOnOrigin(target.clone())),
            ),
            (
                "dirty even when at the target",
                state(B, Some(B), &[" M x.py"], vec![]),
                Respect,
                refused(Reason::RemoteDirty(vec![" M x.py".into()])),
            ),
            (
                "override keeps dirty refused",
                state(A, Some(B), &["?? y"], vec![]),
                Override,
                refused(Reason::RemoteDirty(vec!["?? y".into()])),
            ),
            (
                "jobs from checkout",
                state(A, Some(B), &[], here()),
                Respect,
                refused(jobs),
            ),
        ];
        for (name, remote, policy, expected) in cases {
            assert_eq!(decide(&target, &remote, policy), expected, "{name}");
        }
    }

    #[test]
    fn an_unresolved_job_blocks_until_overridden() {
        let target = Target::Local(commit(B));
        let unknown = Job {
            id: SqueueId::parse("3").unwrap(),
            workdir: Workdir::Unresolved,
        };
        let remote = state(A, Some(B), &[], vec![unknown]);
        assert!(matches!(
            decide(&target, &remote, JobPolicy::Respect),
            PullDecision::Refused(Reason::JobsFromCheckout(_))
        ));
        assert!(matches!(
            decide(&target, &remote, JobPolicy::Override),
            PullDecision::FastForward(_)
        ));
    }

    #[test]
    fn dirty_file_names_cannot_smuggle_terminal_controls() {
        let reason = Reason::RemoteDirty(vec!["?? \u{1b}[2Jevil\u{202E}gnp".into()]);
        let shown = reason.to_string();
        assert!(shown.ends_with("?? ?[2Jevil?gnp"), "{shown}");
    }
}
