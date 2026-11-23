//! The local git commands that build and describe a snapshot, as argv (pure). Every command runs
//! with `-C <repository root>`, so what it prints does not depend on the directory `rr` was
//! started in, and the ones that touch an index use a temporary one named by `GIT_INDEX_FILE`: the
//! user's own index, branch, stash and refs are never written.

use super::guard::{ClearedTree, Inclusion};
use super::tree::{ObjectId, TreeId};
use crate::domain::argv::{Argv, EnvVar};
use crate::domain::ids::Commit;
use anyhow::{Result, bail};
use std::fmt;

/// Git's own text for a line that begins an lfs pointer file.
const LFS_POINTER_FIRST_LINE: &str = r"^version https://git-lfs\.github\.com/spec/v1$";

/// The path of a temporary index file, absolute, as the `GIT_INDEX_FILE` variable that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexFile(EnvVar);

impl fmt::Display for IndexFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.value())
    }
}

impl IndexFile {
    pub fn parse(path: &str) -> Result<Self> {
        if !path.starts_with('/') {
            bail!("the temporary index {path:?} is not an absolute path");
        }
        EnvVar::parse("GIT_INDEX_FILE", path).map(Self)
    }
}

fn git(args: Vec<String>) -> Argv {
    Argv::new("git", args)
}

/// Find the repository root from the current directory.
pub fn toplevel() -> Argv {
    git(vec!["rev-parse".into(), "--show-toplevel".into()])
}

/// The top directory of the working tree, absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo(String);

impl Repo {
    pub fn parse(stdout: &str) -> Result<Self> {
        let path = stdout.strip_suffix('\n').unwrap_or(stdout);
        if !path.starts_with('/') || path.contains('\0') {
            bail!("git printed no repository root: {stdout:?}");
        }
        Ok(Self(path.to_string()))
    }

    fn git(&self, args: &[&str]) -> Argv {
        let mut all = vec!["-C".to_string(), self.0.clone()];
        all.extend(args.iter().map(|arg| arg.to_string()));
        git(all)
    }

    /// The commit the working tree is based on.
    pub fn head(&self) -> Argv {
        self.git(&["rev-parse", "--verify", "HEAD^{commit}"])
    }

    /// The tree of `HEAD`.
    pub fn head_tree(&self) -> Argv {
        self.git(&["rev-parse", "--verify", "HEAD^{tree}"])
    }

    /// Fill the temporary index with `base`, what `git add` then changes.
    pub fn read_tree(&self, index: &IndexFile, base: &Commit) -> Argv {
        self.git(&["read-tree", base.as_str()])
            .with_env(index.0.clone())
    }

    /// Bring the working tree into the temporary index: tracked files only, or untracked ones too.
    pub fn add(&self, index: &IndexFile, inclusion: Inclusion) -> Argv {
        let flag = match inclusion {
            Inclusion::TrackedOnly => "-u",
            Inclusion::WithUntracked => "-A",
        };
        self.git(&["add", flag]).with_env(index.0.clone())
    }

    /// Write the temporary index as a tree object and print its id.
    pub fn write_tree(&self, index: &IndexFile) -> Argv {
        self.git(&["write-tree"]).with_env(index.0.clone())
    }

    /// Every file of `tree` with mode, object id and size, NUL-separated.
    pub fn ls_tree(&self, tree: &TreeId) -> Argv {
        self.git(&["ls-tree", "-r", "-l", "-z", "--full-tree", tree.as_str()])
    }

    /// The paths `tree` has and `base` does not, NUL-separated.
    pub fn added(&self, base: &Commit, tree: &TreeId) -> Argv {
        self.git(&[
            "diff-tree",
            "-r",
            "--no-renames",
            "--diff-filter=A",
            "--name-only",
            "-z",
            base.as_str(),
            tree.as_str(),
        ])
    }

    /// What a symlink points to: the content of its blob.
    pub fn link_target(&self, oid: &ObjectId) -> Argv {
        self.git(&["cat-file", "blob", oid.as_str()])
    }

    /// The files of `tree` with a git-lfs pointer's first line (`tree:path`, NUL-separated). Exit 1
    /// means there are none.
    pub fn lfs_pointers(&self, tree: &TreeId) -> Argv {
        self.git(&[
            "grep",
            "-l",
            "-z",
            "-I",
            "-E",
            "-e",
            LFS_POINTER_FIRST_LINE,
            tree.as_str(),
            "--",
        ])
    }

    /// What the working tree has that `HEAD` does not, to tell the user what was left out. It
    /// takes no lock, so it does not rewrite the user's index.
    pub fn status(&self) -> Argv {
        git(vec![
            "--no-optional-locks".into(),
            "-C".into(),
            self.0.clone(),
            "status".into(),
            "--porcelain=v1".into(),
            "-z".into(),
            "--untracked-files=all".into(),
        ])
    }

    /// The tar of a checked tree, on stdout. It takes a [`ClearedTree`], so nothing is archived
    /// before the guards have passed.
    pub fn archive(&self, tree: &ClearedTree) -> Argv {
        self.git(&["archive", "--format=tar", tree.id().as_str()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn repo() -> Repo {
        Repo::parse("/work/proj\n").unwrap()
    }

    fn words(argv: &Argv) -> Vec<&str> {
        argv.args().iter().map(String::as_str).collect()
    }

    #[test]
    fn the_repository_root_must_be_an_absolute_path() {
        assert_eq!(Repo::parse("/a b/c\n").unwrap(), Repo("/a b/c".into()));
        for bad in ["", "\n", "rel/path", "-C", "/a\0b"] {
            assert!(Repo::parse(bad).is_err(), "{bad:?}");
        }
        assert!(IndexFile::parse("tmp/index").is_err());
    }

    #[test]
    fn only_the_three_index_commands_get_the_temporary_index() {
        let index = IndexFile::parse("/tmp/x y/index").unwrap();
        let (base, tree) = (Commit::parse(A).unwrap(), TreeId::parse(B).unwrap());
        let with_index = [
            repo().read_tree(&index, &base),
            repo().add(&index, Inclusion::TrackedOnly),
            repo().write_tree(&index),
        ];
        for argv in &with_index {
            assert_eq!(argv.envs().len(), 1);
            assert_eq!(argv.envs()[0].name(), "GIT_INDEX_FILE");
            assert_eq!(argv.envs()[0].value(), "/tmp/x y/index");
        }
        let without = [
            repo().head(),
            repo().head_tree(),
            repo().ls_tree(&tree),
            repo().added(&base, &tree),
            repo().lfs_pointers(&tree),
            repo().status(),
            toplevel(),
        ];
        assert!(without.iter().all(|argv| argv.envs().is_empty()));
    }

    #[test]
    fn every_command_runs_in_the_repository_root() {
        let tree = TreeId::parse(B).unwrap();
        for argv in [repo().head(), repo().ls_tree(&tree), repo().status()] {
            assert_eq!(argv.program(), "git");
            let args = words(&argv);
            let at = args.iter().position(|a| *a == "-C").unwrap();
            assert_eq!(args[at + 1], "/work/proj");
        }
        assert_eq!(words(&toplevel()), ["rev-parse", "--show-toplevel"]);
    }

    #[test]
    fn the_index_is_filled_from_the_base_commit_and_updated_by_the_inclusion() {
        let index = IndexFile::parse("/i").unwrap();
        let base = Commit::parse(A).unwrap();
        assert_eq!(
            words(&repo().read_tree(&index, &base)),
            ["-C", "/work/proj", "read-tree", A]
        );
        assert_eq!(
            words(&repo().add(&index, Inclusion::TrackedOnly)),
            ["-C", "/work/proj", "add", "-u"]
        );
        assert_eq!(
            words(&repo().add(&index, Inclusion::WithUntracked)),
            ["-C", "/work/proj", "add", "-A"]
        );
    }

    #[test]
    fn the_description_commands_are_spelled_out() {
        let (base, tree) = (Commit::parse(A).unwrap(), TreeId::parse(B).unwrap());
        assert_eq!(
            words(&repo().ls_tree(&tree)),
            [
                "-C",
                "/work/proj",
                "ls-tree",
                "-r",
                "-l",
                "-z",
                "--full-tree",
                B
            ]
        );
        assert_eq!(
            words(&repo().added(&base, &tree)),
            [
                "-C",
                "/work/proj",
                "diff-tree",
                "-r",
                "--no-renames",
                "--diff-filter=A",
                "--name-only",
                "-z",
                A,
                B
            ]
        );
        assert_eq!(
            words(&repo().status()),
            [
                "--no-optional-locks",
                "-C",
                "/work/proj",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all"
            ]
        );
        let grep = repo().lfs_pointers(&tree);
        let args = words(&grep);
        assert_eq!(&args[2..8], ["grep", "-l", "-z", "-I", "-E", "-e"]);
        assert_eq!(&args[9..], [B, "--"]);
    }
}
