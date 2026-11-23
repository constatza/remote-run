//! The guarded fast-forward of the remote checkout to the commit seen locally.
//!
//! One script reads the remote state ([`state`]), a pure [`decide`] judges it, and only a
//! [`PullDecision::FastForward`] holding a [`Permit`] sends the merge. The guards: the remote tree
//! is clean (a job runs what the checkout holds), the commit is on `origin` (code arrives only as
//! a pushed commit: an `origin/*` branch contains it, not merely that the object exists), and no job queued or running from the checkout gets its code changed under it
//! ([`JobPolicy::Override`] lifts only that last guard). A job submitted from another directory
//! with `sbatch proj/run.sh` is not detected: `squeue` reports the submit directory only.

mod decide;
mod state;

pub use decide::{JobPolicy, Permit, PullDecision, Reason, decide, merge_refusal};
pub(crate) use state::job_loop;
pub use state::{Job, RemoteState, Workdir, parse_state, state_script};
use state::{read_blocking_jobs_script, status_command};

use crate::domain::exit::RrCode;
use crate::domain::ids::Commit;
use crate::domain::shell::{Script, lit, number, quote};
use std::fmt;

/// What the commit to reach is: the local `HEAD`, or `origin`'s default branch outside a repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Local(Commit),
    OriginHead,
}

impl Target {
    fn revision(&self) -> String {
        match self {
            Target::Local(commit) => commit.as_str().to_string(),
            Target::OriginHead => "origin/HEAD".into(),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Local(commit) => write!(f, "{commit}"),
            Target::OriginHead => f.write_str("origin/HEAD"),
        }
    }
}

/// Exit code of [`verify_script`] when the environment does not match `uv.lock`.
pub const STALE_EXIT: i32 = 75;

/// Exit code of [`verify_script`] when `uv` is not on the remote `PATH` (the shell's own
/// "command not found" code), kept apart from [`STALE_EXIT`] so a missing tool is not read as a
/// stale environment.
pub const UV_MISSING_EXIT: i32 = 127;

/// Exit code of [`apply_script`] when the checkout is no longer what [`state_script`] read: `HEAD`
/// moved, the tree is dirty or a job was submitted from it. Distinct from [`STALE_EXIT`], from the shell's 127 and from the
/// codes `git` itself exits with (1, 128), so a failed merge cannot be read as this refusal.
pub const STATE_CHANGED_EXIT: i32 = 76;

/// The line the merge script prints, before it exits [`STATE_CHANGED_EXIT`], when it could not
/// read the queue at all (`squeue`, `realpath` or `pwd` failed), as opposed to having found a job.
/// [`merge_refusal`] tells the two apart by it.
pub const QUEUE_UNREADABLE_MARKER: &str = "rr: could not re-check the queue";

/// The note the merge script prints when the checkout or its jobs are not what was read.
const CHANGED_NOTE: &str = "the remote checkout or its jobs changed since it was read";

/// Check a uv project's environment without changing it.
pub(crate) fn verify_script() -> Script {
    lit("if [ -f uv.lock ]; then\n  if ! command -v uv >/dev/null 2>&1; then echo 'uv is not on the remote PATH' >&2; exit ")
        .word(number(UV_MISSING_EXIT))
        .lit("; fi\n  uv sync --frozen --check || exit ")
        .word(number(STALE_EXIT))
        .lit("\nfi")
}

/// Fast-forward to what the permit allows, then [`verify_script`]. Crate-private: its only caller
/// is `app::pull`, after [`decide`], and a [`Permit`] is the proof of that.
///
/// The state was read by an earlier ssh call, so the script first re-asserts, on the remote, that
/// `HEAD` is still `from`, the tree is still clean and, unless the permit's policy is
/// [`JobPolicy::Override`], that no job was submitted from the checkout since (the same query and
/// path normalisation as [`state_script`], with an equivalent shell form of its "inside" test). Any mismatch exits [`STATE_CHANGED_EXIT`]. A `git`, `squeue` or
/// `realpath` that fails inside the check fails the check: the merge only runs on a positive
/// answer. A queue that could not be read prints [`QUEUE_UNREADABLE_MARKER`] first, so it is not
/// told as a job that landed. What remains is the time between this check and `git merge`, inside
/// one script.
///
/// ```compile_fail,E0603
/// let _ = remote_run::domain::pull::apply_script;
/// ```
pub(crate) fn apply_script(permit: &Permit) -> Script {
    let checks = lit("head=$(git rev-parse HEAD) && dirty=$(")
        .then(status_command())
        .lit(r#") && [ "$head" = "#)
        .word(quote(permit.from().as_str()))
        .lit(r#" ] && [ -z "$dirty" ]"#);
    let checks = checks.then(refuse_unless(CHANGED_NOTE));
    let checks = match permit.policy() {
        JobPolicy::Respect => checks
            .then(read_blocking_jobs_script())
            .then(refuse_unless(QUEUE_UNREADABLE_MARKER))
            .lit(r#"[ -z "$blocked" ]"#)
            .then(refuse_unless(CHANGED_NOTE)),
        JobPolicy::Override => checks,
    };
    checks
        .lit("git merge -q --ff-only ")
        .word(quote(permit.to().as_str()))
        .lit("\n")
        .then(verify_script())
}

/// The tail that ends a check line: when the check before it failed, say `note` on stderr and
/// exit [`STATE_CHANGED_EXIT`], so the merge that follows never runs.
fn refuse_unless(note: &'static str) -> Script {
    lit(" || { echo '")
        .lit(note)
        .lit("' >&2; exit ")
        .word(number(STATE_CHANGED_EXIT))
        .lit("; }\n")
}

/// How a pull ended.
#[derive(Debug, PartialEq, Eq)]
pub enum PullOutcome {
    UpToDate,
    Advanced {
        from: Commit,
        to: Commit,
    },
    Refused(Reason),
    /// The checkout is at the target, but its environment does not match `uv.lock`; `stderr` is
    /// what `uv` said.
    Stale {
        stderr: String,
    },
}

/// What a command that follows a pull does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// The checkout is at the target with a matching environment: run the command.
    RunCommand,
    /// Stop with this exit code; the command must not run.
    Stop(RrCode),
}

/// The only way from a pull to running code: a closed match, so a new outcome must choose.
pub fn next(outcome: &PullOutcome) -> Next {
    match outcome {
        PullOutcome::UpToDate | PullOutcome::Advanced { .. } => Next::RunCommand,
        PullOutcome::Refused(_) | PullOutcome::Stale { .. } => Next::Stop(RrCode::Refused),
    }
}

#[allow(clippy::disallowed_methods)] // tests build real files
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::slurm::job_workdirs_command;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn commit(text: &str) -> Commit {
        Commit::parse(text).unwrap()
    }

    /// The environment check, run by sh against a stand-in `uv`: 127 only when `uv` is missing,
    /// 75 only when `uv sync --frozen --check` itself fails.
    #[cfg(unix)]
    #[test]
    fn the_environment_check_tells_a_missing_uv_from_a_stale_environment() {
        use crate::adapters::OsExec;
        use crate::ports::{Argv, Exec, Stdin};
        use std::os::unix::fs::PermissionsExt;
        let project = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let run = |lock: bool, uv: Option<&str>| {
            let _ = std::fs::remove_file(bin.path().join("uv"));
            let _ = std::fs::remove_file(project.path().join("uv.lock"));
            if lock {
                std::fs::write(project.path().join("uv.lock"), "").unwrap();
            }
            if let Some(body) = uv {
                let path = bin.path().join("uv");
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let script = format!(
                "cd {} && {}",
                quote(&project.path().display().to_string()),
                verify_script()
            );
            // only the stand-in directory on PATH, so a real `uv` on this machine cannot answer
            let path = format!("PATH={}", bin.path().display());
            let sh = vec![path, "/bin/sh".into(), "-c".into(), script];
            let argv = Argv::new("env", sh);
            let out = OsExec.capture(&argv, Stdin::Null).unwrap();
            (out.code, out.stderr)
        };
        assert_eq!(run(false, None).0, 0, "no uv.lock, nothing to check");
        let (code, stderr) = run(true, None);
        assert_eq!(code, UV_MISSING_EXIT);
        assert!(stderr.contains("not on the remote PATH"), "{stderr}");
        assert_eq!(run(true, Some("exit 0")).0, 0);
        let (code, stderr) = run(true, Some("echo 'lock out of date' >&2; exit 2"));
        assert_eq!(code, STALE_EXIT);
        assert!(stderr.contains("lock out of date"), "{stderr}");
    }

    /// What a merge-time run of the script saw: its exit code, stderr and whether `git merge` ran.
    #[cfg(unix)]
    struct Merge {
        code: i32,
        stderr: String,
        merged: bool,
    }

    /// The job part of a merge-time run: what the stand-in `squeue` does (`$DIR` in it is the
    /// checkout), an optional stand-in `realpath` (the real one otherwise), and the permit's
    /// policy.
    #[cfg(unix)]
    struct Queue<'a> {
        squeue: &'a str,
        realpath: Option<&'a str>,
        policy: JobPolicy,
    }

    #[cfg(unix)]
    impl<'a> Queue<'a> {
        fn printing(squeue: &'a str) -> Self {
            Self {
                squeue,
                realpath: None,
                policy: JobPolicy::Respect,
            }
        }
    }

    /// The merge script run by bash, as `place::script` wraps it, against a stand-in `git` whose
    /// `rev-parse` and `status` run the shell snippets `head` and `status`, and which records
    /// merges, and the stand-in `squeue` of `queue`. `HOME` is the temporary directory, so no
    /// real git configuration is read.
    #[cfg(unix)]
    fn run_merge_with(head: &str, status: &str, queue: &Queue) -> Merge {
        use crate::adapters::OsExec;
        use crate::domain::place::{Place, script};
        use crate::ports::{Argv, Exec, Stdin};
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let log = dir.join("merged");
        let install = |name: &str, body: String| {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        install(
            "git",
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  rev-parse) {head} ;;\n  status) {status} ;;\n  merge) echo \"$@\" > {} ;;\nesac\n",
                quote(&log.display().to_string())
            ),
        );
        let squeue = queue.squeue.replace("$DIR", &dir.display().to_string());
        install("squeue", format!("#!/bin/sh\n{squeue}\n"));
        if let Some(body) = queue.realpath {
            install("realpath", format!("#!/bin/sh\n{body}\n"));
        }
        let permit = Permit::between(commit(A), commit(B), queue.policy);
        let body = script(&Place::default(), apply_script(&permit)).to_string();
        // the stand-ins first on PATH: no uv.lock here, so the environment check is skipped
        let cwd = format!("cd {} && {body}", quote(&dir.display().to_string()));
        let args = vec![
            format!("PATH={}:/usr/bin:/bin", dir.display()),
            format!("HOME={}", dir.display()),
            "bash".into(),
            "-c".into(),
            cwd,
        ];
        let out = OsExec
            .capture(&Argv::new("env", args), Stdin::Null)
            .unwrap();
        Merge {
            code: out.code,
            stderr: out.stderr,
            merged: log.exists(),
        }
    }

    /// An empty queue: the head and status checks alone decide.
    #[cfg(unix)]
    fn run_merge(head: &str, status: &str) -> (i32, bool) {
        let merge = run_merge_with(head, status, &Queue::printing("true"));
        (merge.code, merge.merged)
    }

    /// The merge runs only when `HEAD` is still the commit that was read and the tree is clean.
    #[cfg(unix)]
    #[test]
    fn the_merge_script_reasserts_the_state_it_was_judged_on() {
        let at = |sha: &str| format!("echo {sha}");
        assert_eq!(run_merge(&at(A), "true"), (0, true), "unchanged: merges");
        for (name, head, status) in [
            ("head moved", at(B), "true"),
            ("head unreadable", "echo".to_string(), "true"),
            ("git rev-parse fails", "exit 128".to_string(), "true"),
            (
                "git rev-parse fails after output",
                format!("echo {A}; exit 128"),
                "true",
            ),
            ("dirty tracked file", at(A), "printf ' M x.py\\n'"),
            ("untracked file", at(A), "printf '?? new\\n'"),
            ("status fails", at(A), "exit 128"),
            (
                "status fails after output",
                at(A),
                "printf ' M x\\n'; exit 1",
            ),
        ] {
            let (code, merged) = run_merge(&head, status);
            assert_eq!(code, STATE_CHANGED_EXIT, "{name}");
            assert!(!merged, "{name}: a changed checkout must not be merged");
        }
    }

    /// A merge of an unchanged checkout under the stand-in queue `queue`.
    #[cfg(unix)]
    fn merge_with_queue(queue: &Queue) -> Merge {
        run_merge_with(&format!("echo {A}"), "true", queue)
    }

    /// A job that appeared after the state was read, from inside the checkout, stops the merge.
    #[cfg(unix)]
    #[test]
    fn a_job_that_appears_at_merge_time_inside_the_checkout_blocks_the_merge() {
        for (name, squeue) in [
            ("the checkout itself", "echo '7 $DIR'"),
            ("below it", "echo '7 $DIR/sub/../sub'"),
            ("a trailing slash", "echo '7 $DIR/'"),
            ("a path with a space", "echo '7 $DIR/my dir'"),
            ("among others", "echo '6 /elsewhere'; echo '7 $DIR/x'"),
        ] {
            let merge = merge_with_queue(&Queue::printing(squeue));
            assert_eq!(merge.code, STATE_CHANGED_EXIT, "{name}: {}", merge.stderr);
            assert!(!merge.merged, "{name}: merged under a job");
            assert!(merge.stderr.contains("job"), "{name}: {}", merge.stderr);
            assert!(
                matches!(merge_refusal(&merge.stderr), Reason::StateChanged { .. }),
                "{name}: a found job is not an unreadable queue: {}",
                merge.stderr
            );
        }
    }

    /// `--force` lifts the jobs guard and nothing else: the merge runs, a moved head still stops.
    #[cfg(unix)]
    #[test]
    fn an_overridden_permit_merges_under_a_job_but_still_checks_head_and_tree() {
        let force = Queue {
            policy: JobPolicy::Override,
            ..Queue::printing("echo '7 $DIR'; exit 5")
        };
        let merge = merge_with_queue(&force);
        assert_eq!((merge.code, merge.merged), (0, true), "{}", merge.stderr);
        let moved = run_merge_with(&format!("echo {B}"), "true", &force);
        assert_eq!((moved.code, moved.merged), (STATE_CHANGED_EXIT, false));
    }

    /// A job from a sibling directory whose name starts with the checkout's, or no job, is no
    /// reason to refuse.
    #[cfg(unix)]
    #[test]
    fn jobs_outside_the_checkout_and_an_empty_queue_let_the_merge_run() {
        for (name, squeue) in [
            ("empty queue", "true"),
            ("a name that only starts alike", "echo \"7 $DIR-runs\""),
            ("another directory", "echo '7 /elsewhere/proj'"),
            ("the parent", "echo \"7 $(dirname \"$DIR\")\""),
        ] {
            let merge = merge_with_queue(&Queue::printing(squeue));
            assert_eq!(
                (merge.code, merge.merged),
                (0, true),
                "{name}: {}",
                merge.stderr
            );
        }
    }

    /// An unreadable job answer is never "no jobs": a failing `squeue`, a line with no directory
    /// and a directory `realpath` cannot normalise all stop the merge.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_job_answer_fails_closed() {
        let no_realpath = Queue {
            realpath: Some("exit 1"),
            ..Queue::printing("echo '7 /elsewhere'")
        };
        for (name, queue) in [
            (
                "squeue fails",
                Queue::printing("echo 'squeue: down' >&2; exit 1"),
            ),
            (
                "squeue fails after output",
                Queue::printing("echo '6 /elsewhere'; exit 1"),
            ),
            (
                "a line without a directory",
                Queue::printing("echo no-workdir"),
            ),
            ("realpath fails", no_realpath),
        ] {
            let merge = merge_with_queue(&queue);
            assert_eq!(merge.code, STATE_CHANGED_EXIT, "{name}: {}", merge.stderr);
            assert!(!merge.merged, "{name}: merged on an unreadable answer");
        }
    }

    /// A query that failed says so, apart from a job that was found: `squeue` itself failing or
    /// `pwd` failing is "could not re-check", a job inside the checkout or a line that cannot be
    /// placed is a change.
    #[cfg(unix)]
    #[test]
    fn a_failed_queue_query_is_told_apart_from_a_job_that_landed() {
        for squeue in [
            "echo 'squeue: down' >&2; exit 1",
            "echo '6 /elsewhere'; exit 1",
        ] {
            let merge = merge_with_queue(&Queue::printing(squeue));
            assert_eq!(merge.code, STATE_CHANGED_EXIT, "{}", merge.stderr);
            assert!(!merge.merged);
            let Reason::QueueUnreadable { stderr } = merge_refusal(&merge.stderr) else {
                panic!("not marked as an unreadable queue: {}", merge.stderr);
            };
            assert!(!stderr.contains(QUEUE_UNREADABLE_MARKER), "{stderr}");
        }
        let down = merge_with_queue(&Queue::printing("echo 'squeue: down' >&2; exit 1"));
        let shown = merge_refusal(&down.stderr).to_string();
        assert!(shown.contains("squeue: down"), "{shown}");
        assert!(!shown.contains("landed"), "{shown}");
        let unplaced = Queue {
            realpath: Some("exit 1"),
            ..Queue::printing("echo '7 /elsewhere'")
        };
        let found = merge_with_queue(&unplaced);
        assert!(matches!(
            merge_refusal(&found.stderr),
            Reason::StateChanged { .. }
        ));
    }

    /// The merge-time check and the state read build the query and the path normalisation from
    /// the same loop, so the squeue format and `realpath -m` cannot differ between them.
    /// The merge-time check must not call a checkout dirty that the state read called clean.
    #[test]
    fn the_merge_script_reads_the_tree_the_way_the_state_script_does() {
        let permit = Permit::between(commit(A), commit(B), JobPolicy::Override);
        let status = state::status_command().to_string();
        assert!(status.contains("rr-snapshots"), "{status}");
        assert!(apply_script(&permit).to_string().contains(&status));
    }

    #[test]
    fn the_merge_script_asks_the_same_job_question_as_the_state_script() {
        let permit = Permit::between(commit(A), commit(B), JobPolicy::Respect);
        let merge = apply_script(&permit).to_string();
        let state = state_script(&Target::OriginHead).to_string();
        let query = format!("{} | while", job_workdirs_command());
        for piece in [query.as_str(), "realpath -m -- "] {
            assert!(merge.contains(piece), "{merge}");
            assert!(state.contains(piece), "{state}");
        }
        let forced = Permit::between(commit(A), commit(B), JobPolicy::Override);
        assert!(!apply_script(&forced).to_string().contains("squeue"));
    }

    #[test]
    fn the_state_changed_code_is_not_one_of_the_other_pull_codes() {
        let codes = [STATE_CHANGED_EXIT, STALE_EXIT, UV_MISSING_EXIT, 0, 1, 128];
        for (i, a) in codes.iter().enumerate() {
            assert!(!codes[i + 1..].contains(a), "{a} repeats");
        }
    }

    #[test]
    fn only_an_up_to_date_or_advanced_checkout_runs_the_command() {
        let advanced = PullOutcome::Advanced {
            from: commit(A),
            to: commit(B),
        };
        assert_eq!(next(&PullOutcome::UpToDate), Next::RunCommand);
        assert_eq!(next(&advanced), Next::RunCommand);
        let dirty = PullOutcome::Refused(Reason::RemoteDirty(vec![]));
        assert_eq!(next(&dirty), Next::Stop(RrCode::Refused));
        let stale = PullOutcome::Stale {
            stderr: String::new(),
        };
        assert_eq!(next(&stale), Next::Stop(RrCode::Refused));
    }
}
