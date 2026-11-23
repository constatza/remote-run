//! What the local git checkout says about the commit to send.

use crate::domain::ids::Commit;
use crate::domain::pull::Target;

/// Whether the local tracked files match `HEAD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Worktree {
    Clean,
    /// Tracked files have uncommitted changes; they never reach the remote.
    Dirty,
}

/// What git said about the current directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Local {
    Head(Commit, Worktree),
    GitDidNotRun,
    NotARepository,
}

impl Local {
    /// The commit to reach: the local `HEAD`, else origin's default branch.
    pub fn target(&self) -> Target {
        match self {
            Local::Head(commit, _) => Target::Local(commit.clone()),
            Local::GitDidNotRun | Local::NotARepository => Target::OriginHead,
        }
    }

    /// What the user should be told about the choice, if anything.
    pub fn warning(&self) -> Option<String> {
        match self {
            Local::Head(_, Worktree::Clean) => None,
            Local::Head(commit, Worktree::Dirty) => Some(format!(
                "local uncommitted changes stay here; the remote gets {commit}"
            )),
            Local::GitDidNotRun => Some("git did not run; the remote gets origin/HEAD".into()),
            Local::NotARepository => {
                Some("not in a git repository; the remote gets origin/HEAD".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn what_git_said_picks_the_target_and_the_warning() {
        let commit = Commit::parse(A).unwrap();
        let clean = Local::Head(commit.clone(), Worktree::Clean);
        assert_eq!(clean.target(), Target::Local(commit.clone()));
        assert_eq!(clean.warning(), None);
        let dirty = Local::Head(commit.clone(), Worktree::Dirty);
        assert_eq!(dirty.target(), Target::Local(commit));
        assert!(
            dirty
                .warning()
                .unwrap()
                .starts_with("local uncommitted changes")
        );
        for local in [Local::GitDidNotRun, Local::NotARepository] {
            assert_eq!(local.target(), Target::OriginHead);
            assert!(
                local
                    .warning()
                    .unwrap()
                    .ends_with("the remote gets origin/HEAD")
            );
        }
    }
}
