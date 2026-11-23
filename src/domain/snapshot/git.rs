//! The local git commands that build and describe a snapshot, as argv (pure). Every command runs
//! with `-C <repository root>`, so what it prints does not depend on the directory `rr` was
//! started in, and the ones that touch an index use a temporary one named by `GIT_INDEX_FILE`: the
//! user's own index, branch, stash and refs are never written. The commands that add, write or read
//! the built tree use a scratch object directory (`GIT_OBJECT_DIRECTORY`) with the repository's own
//! objects as an alternate, so what is built is stored only in the scratch directory and a refused
//! tree leaves nothing in the user's `.git/objects`.

use super::guard::{ClearedTree, Inclusion};
use super::tree::{ObjectId, TreeId};
use crate::domain::argv::{Argv, EnvVar};
use crate::domain::ids::Commit;
use anyhow::{Result, bail};
use std::fmt;

/// Git's own text for a line that begins an lfs pointer file. The file may use CRLF line ends. The
/// pattern is a POSIX extended one (`git grep -E`), where `\r` is not a carriage return but a
/// plain `r`, so the optional CR is the control-character class: it matches the CR and nothing a
/// pointer's first line would otherwise end in.
const LFS_POINTER_FIRST_LINE: &str = r"^version https://git-lfs\.github\.com/spec/v1[[:cntrl:]]?$";

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

/// The length of an object name in a SHA-256 repository, in hex digits.
const SHA256_LENGTH: usize = 64;

/// The commit `git rev-parse HEAD^{commit}` printed. A SHA-256 repository names objects with 64
/// digits, which the id types of `rr` (40 digits) cannot hold, so it is said plainly instead of
/// as a malformed id.
pub fn parse_head(stdout: &str) -> Result<Commit> {
    let name = stdout.trim_end();
    if name.len() == SHA256_LENGTH && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("SHA-256 repositories are not supported by snapshot mode");
    }
    Commit::parse(stdout)
}

/// A scratch directory for the objects of a built tree, absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScratchObjects(String);

impl ScratchObjects {
    pub fn parse(path: &str) -> Result<Self> {
        if !path.starts_with('/') {
            bail!("the scratch object directory {path:?} is not an absolute path");
        }
        // validated as a variable value now, so building the environment later cannot fail
        EnvVar::parse(OBJECT_DIRECTORY, path)?;
        Ok(Self(path.to_string()))
    }
}

/// Where a built tree's objects go: a scratch directory, with the repository's own object
/// directory as an alternate so git can still read every commit, tree and blob the tree builds on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectStore {
    scratch: EnvVar,
    shared: EnvVar,
}

const OBJECT_DIRECTORY: &str = "GIT_OBJECT_DIRECTORY";
const ALTERNATE_OBJECT_DIRECTORIES: &str = "GIT_ALTERNATE_OBJECT_DIRECTORIES";

impl ObjectStore {
    /// `shared` is the repository's object directory. The alternates variable separates its
    /// entries with `:`, so a path that holds one cannot be named in it.
    pub fn new(scratch: &ScratchObjects, shared: &str) -> Result<Self> {
        let shared = shared.trim_end_matches('/');
        if !shared.starts_with('/') {
            bail!("the repository's object directory {shared:?} is not an absolute path");
        }
        if shared.contains(':') {
            bail!(
                "the repository's object directory {shared:?} holds a `:`, which git's alternates \
                 variable cannot carry"
            );
        }
        Ok(Self {
            scratch: EnvVar::parse(OBJECT_DIRECTORY, &scratch.0)?,
            shared: EnvVar::parse(ALTERNATE_OBJECT_DIRECTORIES, shared)?,
        })
    }
}

fn git(args: Vec<String>) -> Argv {
    Argv::new("git", args)
}

/// Find the repository root from the current directory.
pub fn toplevel() -> Argv {
    git(vec!["rev-parse".into(), "--show-toplevel".into()])
}

/// The top directory of the working tree, absolute, and (once the build has made one) the scratch
/// object store its built tree lives in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    root: String,
    objects: Option<ObjectStore>,
}

impl Repo {
    pub fn parse(stdout: &str) -> Result<Self> {
        let path = stdout.strip_suffix('\n').unwrap_or(stdout);
        if !path.starts_with('/') || path.contains('\0') {
            bail!("git printed no repository root: {stdout:?}");
        }
        Ok(Self {
            root: path.to_string(),
            objects: None,
        })
    }

    /// The same repository, with the commands that touch the built tree stored in `objects`.
    pub fn in_scratch(self, objects: ObjectStore) -> Self {
        Self {
            objects: Some(objects),
            ..self
        }
    }

    fn git(&self, args: &[&str]) -> Argv {
        let mut all = vec!["-C".to_string(), self.root.clone()];
        all.extend(args.iter().map(|arg| arg.to_string()));
        git(all)
    }

    /// A command that adds, writes or reads the built tree.
    fn on_built_tree(&self, args: &[&str]) -> Argv {
        let argv = self.git(args);
        match &self.objects {
            Some(store) => argv
                .with_env(store.scratch.clone())
                .with_env(store.shared.clone()),
            None => argv,
        }
    }

    /// The repository's object directory, as git names it: relative to the root, or absolute (a
    /// linked worktree shares its main repository's).
    pub fn objects_directory(&self) -> Argv {
        self.git(&["rev-parse", "--git-path", "objects"])
    }

    /// The absolute form of what [`Repo::objects_directory`] printed.
    pub fn absolute_objects_directory(&self, stdout: &str) -> Result<String> {
        let path = stdout.strip_suffix('\n').unwrap_or(stdout);
        match path {
            "" => bail!("git printed no object directory"),
            absolute if absolute.starts_with('/') => Ok(absolute.to_string()),
            relative => Ok(format!("{}/{relative}", self.root.trim_end_matches('/'))),
        }
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
        self.on_built_tree(&["add", flag]).with_env(index.0.clone())
    }

    /// Write the temporary index as a tree object and print its id.
    pub fn write_tree(&self, index: &IndexFile) -> Argv {
        self.on_built_tree(&["write-tree"])
            .with_env(index.0.clone())
    }

    /// Every file of `tree` with mode, object id and size, NUL-separated.
    pub fn ls_tree(&self, tree: &TreeId) -> Argv {
        self.on_built_tree(&["ls-tree", "-r", "-l", "-z", "--full-tree", tree.as_str()])
    }

    /// The paths `tree` has and `base` does not, NUL-separated.
    pub fn added(&self, base: &Commit, tree: &TreeId) -> Argv {
        self.on_built_tree(&[
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
        self.on_built_tree(&["cat-file", "blob", oid.as_str()])
    }

    /// The files of `tree` with a git-lfs pointer's first line (`tree:path`, NUL-separated). Exit 1
    /// means there are none.
    pub fn lfs_pointers(&self, tree: &TreeId) -> Argv {
        self.on_built_tree(&[
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
            self.root.clone(),
            "status".into(),
            "--porcelain=v1".into(),
            "-z".into(),
            "--untracked-files=all".into(),
        ])
    }

    /// The tar of a checked tree, on stdout. It takes a [`ClearedTree`], so nothing is archived
    /// before the guards have passed.
    pub fn archive(&self, tree: &ClearedTree) -> Argv {
        self.on_built_tree(&["archive", "--format=tar", tree.id().as_str()])
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
        assert_eq!(Repo::parse("/a b/c\n").unwrap().root, "/a b/c");
        for bad in ["", "\n", "rel/path", "-C", "/a\0b"] {
            assert!(Repo::parse(bad).is_err(), "{bad:?}");
        }
        assert!(IndexFile::parse("tmp/index").is_err());
    }

    fn names(argv: &Argv) -> Vec<&str> {
        argv.envs().iter().map(EnvVar::name).collect()
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
            assert_eq!(argv.envs()[0].name(), "GIT_INDEX_FILE", "{argv:?}");
            assert_eq!(argv.envs()[0].value(), "/tmp/x y/index");
        }
        let without = [
            repo().head(),
            repo().head_tree(),
            repo().ls_tree(&tree),
            repo().added(&base, &tree),
            repo().lfs_pointers(&tree),
            repo().status(),
            repo().objects_directory(),
            toplevel(),
        ];
        assert!(without.iter().all(|argv| argv.envs().is_empty()));
    }

    /// What the build stores goes to the scratch directory, and the repository's own objects are
    /// only an alternate to read from: no command that adds, writes or reads the built tree can
    /// reach the user's `.git/objects` for writing.
    #[test]
    fn every_command_on_the_built_tree_uses_the_scratch_objects() {
        let scratch = ScratchObjects::parse("/tmp/rr x/objects").unwrap();
        let store = ObjectStore::new(&scratch, "/work/proj/.git/objects/").unwrap();
        let repo = repo().in_scratch(store);
        let index = IndexFile::parse("/tmp/rr x/index").unwrap();
        let (base, tree) = (Commit::parse(A).unwrap(), TreeId::parse(B).unwrap());
        let blob = ObjectId::parse(A).unwrap();
        let on_built = [
            repo.add(&index, Inclusion::WithUntracked),
            repo.write_tree(&index),
            repo.ls_tree(&tree),
            repo.added(&base, &tree),
            repo.link_target(&blob),
            repo.lfs_pointers(&tree),
            repo.archive(&ClearedTree::fake(B, 1)),
        ];
        for argv in &on_built {
            let values: Vec<(&str, &str)> = argv
                .envs()
                .iter()
                .filter(|var| var.name() != "GIT_INDEX_FILE")
                .map(|var| (var.name(), var.value()))
                .collect();
            assert_eq!(
                values,
                [
                    ("GIT_OBJECT_DIRECTORY", "/tmp/rr x/objects"),
                    (
                        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                        "/work/proj/.git/objects"
                    ),
                ],
                "{argv:?}"
            );
        }
        for argv in [
            repo.head(),
            repo.head_tree(),
            repo.status(),
            repo.objects_directory(),
        ] {
            assert!(argv.envs().is_empty(), "{argv:?}");
        }
        assert_eq!(names(&repo.read_tree(&index, &base)), ["GIT_INDEX_FILE"]);
    }

    #[test]
    fn a_sha256_repository_is_named_as_unsupported_not_as_a_bad_id() {
        let sha256 = format!("{}\n", "a".repeat(64));
        let err = parse_head(&sha256).unwrap_err().to_string();
        assert!(
            err.contains("SHA-256 repositories are not supported"),
            "{err}"
        );
        assert_eq!(parse_head(&format!("{A}\n")).unwrap().as_str(), A);
        let err = parse_head("zz\n").unwrap_err().to_string();
        assert!(!err.contains("SHA-256"), "{err}");
    }

    #[test]
    fn the_scratch_store_needs_absolute_paths_and_an_alternate_without_a_colon() {
        assert!(ScratchObjects::parse("objects").is_err());
        let scratch = ScratchObjects::parse("/tmp/o").unwrap();
        for bad in ["", ".git/objects", "/a:b/objects"] {
            assert!(ObjectStore::new(&scratch, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_object_directory_git_names_becomes_absolute() {
        let repo = repo();
        assert_eq!(
            repo.absolute_objects_directory(".git/objects\n").unwrap(),
            "/work/proj/.git/objects"
        );
        assert_eq!(
            repo.absolute_objects_directory("/main/.git/objects\n")
                .unwrap(),
            "/main/.git/objects"
        );
        assert!(repo.absolute_objects_directory("\n").is_err());
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
