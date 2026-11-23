//! Ask the local git which commit to send.

use crate::domain::argv::{Argv, Stdin};
use crate::domain::ids::Commit;
use crate::domain::local::{Local, Worktree};
use crate::ports::Exec;
use anyhow::Result;

fn git(args: &[&str]) -> Argv {
    Argv::new("git", args.iter().map(|arg| arg.to_string()).collect())
}

/// What `git` prints to stderr outside a repository.
const NOT_A_REPOSITORY: &str = "not a git repository";

/// Ask git for `HEAD` and whether tracked files are modified. Only "not a git repository" and a
/// git that cannot start fall back to `origin/HEAD`; any other git failure is an error, because
/// shipping a different commit than the user expects must never be silent.
pub fn locate(exec: &dyn Exec) -> Result<Local> {
    let Ok(head) = exec.capture(&git(&["rev-parse", "HEAD"]), Stdin::Null) else {
        return Ok(Local::GitDidNotRun);
    };
    if head.code != 0 && head.stderr.contains(NOT_A_REPOSITORY) {
        return Ok(Local::NotARepository);
    }
    let commit = Commit::parse(&head.into_stdout("git rev-parse HEAD")?)?;
    let status = exec.capture(
        &git(&["status", "--porcelain", "--untracked-files=no"]),
        Stdin::Null,
    )?;
    let worktree = if status.into_stdout("git status")?.is_empty() {
        Worktree::Clean
    } else {
        Worktree::Dirty
    };
    Ok(Local::Head(commit, worktree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pull::Target;
    use crate::ports::fake::{FakeExec, How, Reply, output};

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn commit() -> Commit {
        Commit::parse(A).unwrap()
    }

    #[test]
    fn a_clean_repository_sends_its_head_without_a_warning() {
        let exec = FakeExec::new(
            vec![output(0, &format!("{A}\n"), ""), output(0, "", "")],
            vec![],
        );
        let local = locate(&exec).unwrap();
        assert_eq!(local, Local::Head(commit(), Worktree::Clean));
        assert_eq!(local.target(), Target::Local(commit()));
        assert_eq!(local.warning(), None);
        assert_eq!(
            exec.words(),
            [
                vec!["git", "rev-parse", "HEAD"],
                vec!["git", "status", "--porcelain", "--untracked-files=no"]
            ]
        );
        assert!(
            exec.calls
                .borrow()
                .iter()
                .all(|(_, how)| *how == How::Capture(Stdin::Null))
        );
    }

    #[test]
    fn uncommitted_changes_still_send_head_with_a_warning() {
        let exec = FakeExec::new(vec![output(0, A, ""), output(0, " M x.py\n", "")], vec![]);
        let local = locate(&exec).unwrap();
        assert_eq!(local.target(), Target::Local(commit()));
        let warning = local.warning().unwrap();
        assert!(
            warning.starts_with("local uncommitted changes stay here"),
            "{warning}"
        );
    }

    #[test]
    fn git_failing_falls_back_to_origin_head_with_a_warning() {
        let broken = FakeExec::new(vec![Reply::WontStart], vec![]);
        let local = locate(&broken).unwrap();
        assert_eq!(local, Local::GitDidNotRun);
        assert_eq!(local.target(), Target::OriginHead);
        assert_eq!(
            local.warning().unwrap(),
            "git did not run; the remote gets origin/HEAD"
        );
        let not_repo = FakeExec::new(vec![output(128, "", "fatal: not a git repository")], vec![]);
        let local = locate(&not_repo).unwrap();
        assert_eq!(local, Local::NotARepository);
        assert_eq!(local.target(), Target::OriginHead);
        assert_eq!(
            local.warning().unwrap(),
            "not in a git repository; the remote gets origin/HEAD"
        );
        assert_eq!(not_repo.calls.borrow().len(), 1);
    }

    #[test]
    fn a_failed_git_status_is_an_error_never_a_clean_tree() {
        let exec = FakeExec::new(
            vec![
                output(0, A, ""),
                output(128, "", "fatal: unable to read index\n"),
            ],
            vec![],
        );
        let err = locate(&exec).unwrap_err().to_string();
        assert_eq!(err, "git status failed (128): fatal: unable to read index");
    }

    #[test]
    fn a_rev_parse_failure_is_not_a_repository_only_when_git_says_so() {
        for stderr in [
            "fatal: detected dubious ownership in repository at '/x'",
            "fatal: ambiguous argument 'HEAD': unknown revision",
            "",
        ] {
            let exec = FakeExec::new(vec![output(128, "", stderr)], vec![]);
            let err = locate(&exec).unwrap_err().to_string();
            assert!(err.starts_with("git rev-parse HEAD failed (128)"), "{err}");
        }
        let exec = FakeExec::new(
            vec![output(
                128,
                "",
                "fatal: not a git repository (or any parent up to mount point /)\n",
            )],
            vec![],
        );
        assert_eq!(locate(&exec).unwrap(), Local::NotARepository);
    }

    #[test]
    fn a_head_that_is_not_a_commit_is_an_error() {
        let exec = FakeExec::new(vec![output(0, "garbage", "")], vec![]);
        assert!(locate(&exec).is_err());
    }
}
