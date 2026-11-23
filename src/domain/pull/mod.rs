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

pub use decide::{JobPolicy, Permit, PullDecision, Reason, decide};
pub use state::{Job, RemoteState, Workdir, parse_state, state_script};

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
/// moved or the tree is dirty. Distinct from [`STALE_EXIT`], from the shell's 127 and from the
/// codes `git` itself exits with (1, 128), so a failed merge cannot be read as this refusal.
pub const STATE_CHANGED_EXIT: i32 = 76;

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
/// `HEAD` is still `from` and the tree is still clean, and exits [`STATE_CHANGED_EXIT`] if not. A
/// `git` that fails inside the check fails the check: the merge only runs on a positive answer.
///
/// ```compile_fail,E0603
/// let _ = remote_run::domain::pull::apply_script;
/// ```
pub(crate) fn apply_script(permit: &Permit) -> Script {
    lit(r#"head=$(git rev-parse HEAD) && dirty=$(git status --porcelain) && [ "$head" = "#)
        .word(quote(permit.from().as_str()))
        .lit(r#" ] && [ -z "$dirty" ] || { echo 'the remote checkout changed since it was read' >&2; exit "#)
        .word(number(STATE_CHANGED_EXIT))
        .lit("; }\ngit merge -q --ff-only ")
        .word(quote(permit.to().as_str()))
        .lit("\n")
        .then(verify_script())
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

    /// The merge script run by bash, as `place::script` wraps it, against a stand-in `git` that
    /// reports `head`, a `status` output and an exit code for `git status`, and records merges.
    #[cfg(unix)]
    fn run_merge(head: &str, status: &str, status_code: i32) -> (i32, bool) {
        use crate::adapters::OsExec;
        use crate::domain::place::{Place, script};
        use crate::ports::{Argv, Exec, Stdin};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("merged");
        let git = format!(
            "#!/bin/sh\ncase \"$1\" in\n  rev-parse) echo {head} ;;\n  status) printf '{status}'; exit {status_code} ;;\n  merge) echo \"$@\" > {} ;;\nesac\n",
            quote(&log.display().to_string())
        );
        let path = dir.path().join("git");
        std::fs::write(&path, git).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let permit = Permit::between(commit(A), commit(B));
        let body = script(&Place::default(), apply_script(&permit)).to_string();
        // only the stand-in on PATH: no uv.lock here, so the environment check is skipped
        let cwd = format!("cd {} && {body}", quote(&dir.path().display().to_string()));
        let args = vec![
            format!("PATH={}:/usr/bin:/bin", dir.path().display()),
            "bash".into(),
            "-c".into(),
            cwd,
        ];
        let out = OsExec
            .capture(&Argv::new("env", args), Stdin::Null)
            .unwrap();
        (out.code, log.exists())
    }

    /// The merge runs only when `HEAD` is still the commit that was read and the tree is clean.
    #[cfg(unix)]
    #[test]
    fn the_merge_script_reasserts_the_state_it_was_judged_on() {
        assert_eq!(run_merge(A, "", 0), (0, true), "unchanged: merges");
        for (name, head, status, status_code) in [
            ("head moved", B, "", 0),
            ("head unreadable", "", "", 0),
            ("dirty tracked file", A, " M x.py\\n", 0),
            ("untracked file", A, "?? new\\n", 0),
            ("status fails", A, "", 128),
            ("status fails after output", A, " M x\\n", 1),
        ] {
            let (code, merged) = run_merge(head, status, status_code);
            assert_eq!(code, STATE_CHANGED_EXIT, "{name}");
            assert!(!merged, "{name}: a changed checkout must not be merged");
        }
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
