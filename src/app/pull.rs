//! `rr pull`: fast-forward the remote checkout to the local `HEAD`, with the guards in
//! [`crate::domain::pull`].

use super::Ports;
use super::git::locate;
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::place::{Place, script};
use crate::domain::pull::{
    JobPolicy, PullDecision, PullOutcome, Reason, STALE_EXIT, STATE_CHANGED_EXIT, Target,
    UV_MISSING_EXIT, apply_script, decide, parse_state, state_script, verify_script,
};
use crate::ports::Remote;
use anyhow::{Result, bail};

pub struct Args {
    pub policy: JobPolicy,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    Ok(Outcome::Pulled(to_local_head(session, args.policy, ports)?))
}

/// Pull to the local target, noting a fallback to `origin/HEAD` first. `rr run` shares this.
pub(super) fn to_local_head(
    session: &Session,
    policy: JobPolicy,
    ports: &Ports,
) -> Result<PullOutcome> {
    let local = locate(ports.exec)?;
    if let Some(warning) = local.warning() {
        ports.notes.note(&format!("rr: {warning}"));
    }
    sync(ports.remote, &session.place, &local.target(), policy)
}

/// Read the state, decide, and apply only what the decision allows. The merge script is built
/// only here, from the [`Permit`](crate::domain::pull::Permit) that [`decide`] hands out.
fn sync(
    remote: &dyn Remote,
    place: &Place,
    target: &Target,
    policy: JobPolicy,
) -> Result<PullOutcome> {
    let read = remote.capture(&script(place, state_script(target)))?;
    let state = parse_state(&read.into_stdout("reading the remote checkout")?)?;
    let (body, permit) = match decide(target, &state, policy) {
        PullDecision::Refused(reason) => return Ok(PullOutcome::Refused(reason)),
        PullDecision::UpToDate => (verify_script(), None),
        PullDecision::FastForward(permit) => (apply_script(&permit), Some(permit)),
    };
    let applied = remote.capture(&script(place, body))?;
    let stderr = applied.stderr.trim().to_string();
    match (applied.code, permit) {
        (STALE_EXIT, _) => Ok(PullOutcome::Stale { stderr }),
        (STATE_CHANGED_EXIT, Some(_)) => Ok(PullOutcome::Refused(Reason::StateChanged)),
        (UV_MISSING_EXIT, _) => bail!(
            "cannot check the remote environment: uv is not on the remote PATH (set `env` to a \
             file that puts it there):\n{stderr}"
        ),
        (0, Some(permit)) => Ok(PullOutcome::Advanced {
            from: permit.from().clone(),
            to: permit.to().clone(),
        }),
        (0, None) => Ok(PullOutcome::UpToDate),
        (code, _) => bail!("updating the remote checkout failed ({code}):\n{stderr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, session};
    use super::*;
    use crate::domain::exit::RrCode;
    use crate::domain::ids::Commit;
    use crate::domain::pull::{Next, next};
    use crate::ports::fake::{Reply, output};

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn commit(text: &str) -> Commit {
        Commit::parse(text).unwrap()
    }

    fn report(head: &str, target: &str, extra: &str) -> String {
        format!(
            "rr-state\tdir\t/d\nrr-state\thead\t{head}\nrr-state\ttarget\t{target}\n\
             rr-state\tonorigin\t  origin/main\n{extra}"
        )
    }

    /// A remote that answers `capture` from a queue and never streams: the commands that follow a
    /// pull are not run by `sync` itself.
    struct Stub(Fakes);

    impl Stub {
        fn new(replies: Vec<(i32, &str, &str)>) -> Self {
            Self(Fakes::new(vec![], vec![], replies, vec![]))
        }

        fn pull(&self, policy: JobPolicy) -> Result<PullOutcome> {
            sync(
                &self.0.remote,
                &Place::default(),
                &Target::Local(commit(B)),
                policy,
            )
        }

        fn sent(&self) -> Vec<String> {
            self.0.remote.scripts()
        }
    }

    #[test]
    fn a_refused_pull_sends_no_merge_even_when_overridden() {
        let dirty = report(A, B, "rr-state\tdirty\t M x\n");
        let stub = Stub::new(vec![(0, &dirty, "")]);
        let outcome = stub.pull(JobPolicy::Override).unwrap();
        assert!(matches!(
            outcome,
            PullOutcome::Refused(Reason::RemoteDirty(_))
        ));
        assert_eq!(stub.sent().len(), 1);
        assert!(!stub.sent()[0].contains("merge"));
    }

    #[test]
    fn a_fast_forward_merges_exactly_the_target_then_checks_the_environment() {
        let stub = Stub::new(vec![(0, &report(A, B, ""), ""), (0, "", "")]);
        let outcome = stub.pull(JobPolicy::Respect).unwrap();
        let advanced = PullOutcome::Advanced {
            from: commit(A),
            to: commit(B),
        };
        assert_eq!(outcome, advanced);
        let sent = stub.sent();
        assert_eq!(sent.len(), 2);
        let merge = format!("git merge -q --ff-only '{B}'\n");
        assert!(sent[1].contains(&merge), "{}", sent[1]);
        assert!(sent[1].contains("uv sync --frozen --check"));
        // the re-assertion comes before the merge, so a changed checkout is never merged
        let guard = sent[1].find(&format!("\"$head\" = '{A}'")).unwrap();
        assert!(guard < sent[1].find("git merge").unwrap(), "{}", sent[1]);
    }

    #[test]
    fn a_stale_environment_is_matched_before_success_and_carries_uv_stderr() {
        let stub = Stub::new(vec![
            (0, &report(A, B, ""), ""),
            (STALE_EXIT, "", "Would update 3 packages\n"),
        ]);
        let outcome = stub.pull(JobPolicy::Respect).unwrap();
        let stale = PullOutcome::Stale {
            stderr: "Would update 3 packages".into(),
        };
        assert_eq!(outcome, stale);
    }

    #[test]
    fn a_missing_uv_is_an_error_with_the_remote_stderr_not_a_stale_environment() {
        let stub = Stub::new(vec![
            (0, &report(B, B, ""), ""),
            (UV_MISSING_EXIT, "", "uv is not on the remote PATH\n"),
        ]);
        let err = stub.pull(JobPolicy::Respect).unwrap_err().to_string();
        assert!(err.contains("uv is not on the remote PATH"), "{err}");
        assert!(!err.contains("does not match"), "{err}");
    }

    #[test]
    fn up_to_date_still_checks_the_environment_without_merging() {
        let stub = Stub::new(vec![(0, &report(B, B, ""), ""), (0, "", "")]);
        assert_eq!(
            stub.pull(JobPolicy::Respect).unwrap(),
            PullOutcome::UpToDate
        );
        assert_eq!(stub.sent().len(), 2);
        let second = stub.sent()[1].clone();
        assert!(!second.contains("merge"));
        assert!(second.contains("uv sync --frozen --check"));
    }

    /// The commit exists in the remote store but no `origin/*` branch contains it: refused, and
    /// nothing but the state read is sent.
    #[test]
    fn a_target_no_origin_branch_contains_is_refused_before_any_merge() {
        let unpushed = "rr-state\tdir\t/d\nrr-state\thead\t".to_string()
            + A
            + "\nrr-state\ttarget\t"
            + B
            + "\n";
        let stub = Stub::new(vec![(0, &unpushed, "")]);
        let outcome = stub.pull(JobPolicy::Override).unwrap();
        let reason = Reason::NotOnOrigin(Target::Local(commit(B)));
        assert_eq!(outcome, PullOutcome::Refused(reason));
        assert_eq!(stub.sent().len(), 1);
    }

    /// The merge script found `HEAD` or the tree changed since the read: a refusal, not an error
    /// and not an advance, and the command that follows does not run.
    #[test]
    fn a_checkout_that_changed_between_read_and_merge_is_refused() {
        let stub = Stub::new(vec![
            (0, &report(A, B, ""), ""),
            (
                STATE_CHANGED_EXIT,
                "",
                "the remote checkout changed since it was read\n",
            ),
        ]);
        let outcome = stub.pull(JobPolicy::Respect).unwrap();
        assert_eq!(outcome, PullOutcome::Refused(Reason::StateChanged));
        assert_eq!(next(&outcome), Next::Stop(RrCode::Refused));
    }

    /// The state-changed code is only meaningful from the merge script. From the environment
    /// check of an up-to-date checkout it is an unexpected failure, so it is an error.
    #[test]
    fn the_state_changed_code_without_a_merge_is_an_error() {
        let stub = Stub::new(vec![
            (0, &report(B, B, ""), ""),
            (STATE_CHANGED_EXIT, "", ""),
        ]);
        assert!(stub.pull(JobPolicy::Respect).is_err());
    }

    #[test]
    fn a_failed_state_call_is_an_error_and_sends_nothing_more() {
        let stub = Stub::new(vec![(255, "", "ssh: no route")]);
        assert!(stub.pull(JobPolicy::Respect).is_err());
        assert_eq!(stub.sent().len(), 1);
    }

    #[test]
    fn another_failure_while_applying_is_an_error_with_stderr() {
        let stub = Stub::new(vec![
            (0, &report(A, B, ""), ""),
            (128, "", "fatal: Not possible to fast-forward\n"),
        ]);
        let err = stub.pull(JobPolicy::Respect).unwrap_err().to_string();
        assert!(
            err.contains("(128)") && err.contains("fast-forward"),
            "{err}"
        );
    }

    /// The gate end to end, over the stub remote: whatever the remote says, a refusal or a stale
    /// environment never reaches `RunCommand`, and a failed call is an error, not an outcome.
    #[test]
    fn the_gate_stops_every_pull_that_did_not_reach_the_target() {
        let dirty = report(A, B, "rr-state\tdirty\t M x\n");
        let jobs = report(A, B, "rr-state\tjob\t7 /d\n");
        let (behind, same) = (report(A, B, ""), report(B, B, ""));
        let cases = [
            (
                "refused dirty",
                vec![(0, dirty.as_str(), "")],
                Next::Stop(RrCode::Refused),
            ),
            (
                "refused jobs",
                vec![(0, jobs.as_str(), "")],
                Next::Stop(RrCode::Refused),
            ),
            (
                "stale",
                vec![(0, behind.as_str(), ""), (STALE_EXIT, "", "")],
                Next::Stop(RrCode::Refused),
            ),
            (
                "advanced",
                vec![(0, behind.as_str(), ""), (0, "", "")],
                Next::RunCommand,
            ),
            (
                "up to date",
                vec![(0, same.as_str(), ""), (0, "", "")],
                Next::RunCommand,
            ),
        ];
        for (name, replies, expected) in cases {
            let outcome = Stub::new(replies).pull(JobPolicy::Respect).unwrap();
            assert_eq!(next(&outcome), expected, "{name}");
        }
        assert!(
            Stub::new(vec![(255, "", "")])
                .pull(JobPolicy::Respect)
                .is_err()
        );
    }

    /// `rr pull` end to end over the fakes: what git says locally picks the target, and the note
    /// about a fallback is said before the remote is asked anything.
    fn pulled(git: Vec<Reply>, remote: Vec<(i32, &str, &str)>) -> (Fakes, Outcome) {
        let fakes = Fakes::new(git, vec![], remote, vec![]);
        let args = Args {
            policy: JobPolicy::Respect,
        };
        let outcome = run(&session(), args, &fakes.ports()).unwrap();
        (fakes, outcome)
    }

    #[test]
    fn a_clean_local_head_is_the_target_and_needs_no_note() {
        let git = vec![output(0, &format!("{B}\n"), ""), output(0, "", "")];
        let (fakes, outcome) = pulled(git, vec![(0, &report(B, B, ""), ""), (0, "", "")]);
        assert!(fakes.notes.0.borrow().is_empty());
        assert!(fakes.remote.scripts()[0].contains(&format!("'{B}^{{commit}}'")));
        assert!(matches!(outcome, Outcome::Pulled(PullOutcome::UpToDate)));
    }

    #[test]
    fn a_dirty_local_tree_is_noted_and_the_head_still_goes() {
        let git = vec![output(0, B, ""), output(0, " M x.py\n", "")];
        let (fakes, _) = pulled(git, vec![(0, &report(B, B, ""), ""), (0, "", "")]);
        assert_eq!(
            *fakes.notes.0.borrow(),
            ["rr: local uncommitted changes stay here; the remote gets bbbbbbbbbbbb"]
        );
    }

    #[test]
    fn without_git_the_remote_gets_origin_head_and_the_note_comes_first() {
        let (fakes, _) = pulled(
            vec![Reply::WontStart],
            vec![(0, &report(B, B, ""), ""), (0, "", "")],
        );
        assert_eq!(
            *fakes.notes.0.borrow(),
            ["rr: git did not run; the remote gets origin/HEAD"]
        );
        assert!(fakes.remote.scripts()[0].contains("'origin/HEAD^{commit}'"));
    }

    #[test]
    fn a_refusal_is_data_with_the_reason_not_a_printed_line() {
        let dirty = report(A, B, "rr-state\tdirty\t M x\n");
        let git = vec![output(0, B, ""), output(0, "", "")];
        let (fakes, outcome) = pulled(git, vec![(0, &dirty, "")]);
        assert!(matches!(
            outcome,
            Outcome::Pulled(PullOutcome::Refused(Reason::RemoteDirty(_)))
        ));
        assert!(
            fakes.notes.0.borrow().is_empty(),
            "the report is for render"
        );
    }
}
