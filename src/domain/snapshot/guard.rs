//! The guards that run before a snapshot leaves the machine, as pure functions over facts git
//! reported, and the types that make their order a compile-time fact:
//! [`BuiltTree`] -> [`ClearedTree`] (only [`BuiltTree::clear`] makes one) -> [`ShippedSnapshot`]
//! (only a successful upload makes one).

use super::names::{AllowedPath, looks_secret};
use super::tree::{Entry, EntryKind, ObjectId, TreeId};
use crate::domain::ids::Commit;
use anyhow::{Result, bail};
use serde::{Serialize, Serializer};
use std::fmt;

/// Whether the snapshot is the committed tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanliness {
    /// The tree is the one `HEAD` has.
    Clean,
    /// The working tree differs from `HEAD`: changes or added files are in the snapshot.
    Dirty,
}

/// Serialized as the flag a reader of `provenance.json` expects (`"dirty": true`).
impl Serialize for Cleanliness {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(matches!(self, Cleanliness::Dirty))
    }
}

/// What `--require-clean` decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyPolicy {
    Allow,
    Refuse,
}

/// Whether files that `HEAD` does not have are part of the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inclusion {
    /// Tracked files and their changes only. A reviewed set: nothing new reaches the cluster.
    TrackedOnly,
    /// Untracked files (not ignored ones) too, with the secret-name check.
    WithUntracked,
}

/// Whether gitlinks and git-lfs pointers may be shipped (as the nothing they are).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmodulePolicy {
    Refuse,
    Allow,
}

/// What `clear` checks a tree against, from the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub inclusion: Inclusion,
    /// Secret-looking untracked files that may go anyway, each named whole.
    pub allowed: Vec<AllowedPath>,
    pub submodules: SubmodulePolicy,
    /// The most bytes of file content the tree may hold.
    pub limit: u64,
    pub dirty: DirtyPolicy,
}

/// A symlink of the tree and where it points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub path: String,
    pub target: String,
}

/// Everything git said about the tree, gathered by the caller.
#[derive(Debug)]
pub struct Facts {
    pub base: Commit,
    /// The tree of `base`: the snapshot is clean exactly when it is this one.
    pub head_tree: TreeId,
    pub tree: TreeId,
    pub entries: Vec<Entry>,
    /// The paths the tree has and `base` does not.
    pub added: Vec<String>,
    /// The paths the working tree has that `HEAD` does not and the tree leaves out (untracked files
    /// and staged additions when only tracked files are shipped). The working tree is not the
    /// snapshot while there are any.
    pub left_out: Vec<String>,
    /// The paths with an unresolved merge conflict; the files hold conflict markers.
    pub unmerged: Vec<String>,
    /// The target of every symlink entry.
    pub links: Vec<Link>,
    /// The paths of git-lfs pointer files among the entries.
    pub lfs_pointers: Vec<String>,
}

/// Whether the repository has a `uv.lock`, by its blob id: what a run recorded about the
/// environment it expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockFile {
    Absent,
    Blob(ObjectId),
}

/// A tree that was built locally and has not been checked. It cannot be shipped: the scripts and
/// `git archive` take a [`ClearedTree`].
#[derive(Debug)]
pub struct BuiltTree {
    facts: Facts,
    cleanliness: Cleanliness,
}

/// A tree that passed every guard of a [`Policy`]. Only [`BuiltTree::clear`] makes one: its fields
/// are private, so nothing can claim a tree was checked.
///
/// ```compile_fail,E0451
/// use remote_run::domain::snapshot::{Cleanliness, ClearedTree, LockFile};
/// fn forge() -> ClearedTree {
///     ClearedTree { cleanliness: Cleanliness::Clean, lock: LockFile::Absent }
/// }
/// ```
#[derive(Debug)]
pub struct ClearedTree {
    id: TreeId,
    base: Commit,
    cleanliness: Cleanliness,
    lock: LockFile,
    files: usize,
    bytes: u64,
}

/// A snapshot that is on the remote host: the only thing a command can run in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShippedSnapshot {
    id: TreeId,
    base: Commit,
    cleanliness: Cleanliness,
}

/// A symlink that points out of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsafeLink {
    pub path: String,
    pub target: String,
}

impl fmt::Display for UnsafeLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} -> {}", self.path, self.target)
    }
}

/// Why a snapshot was not shipped. A refusal is a result, not an error: nothing was sent, or
/// nothing that exists was trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `--require-clean` and the working tree differs from `HEAD`.
    Dirty,
    TooLarge {
        total: u64,
        limit: u64,
    },
    Submodules(Vec<String>),
    LfsPointers(Vec<String>),
    UnsafeSymlinks(Vec<UnsafeLink>),
    SecretNames(Vec<String>),
    /// A merge is in progress: these paths have conflicts, so their files hold conflict markers.
    Unmerged(Vec<String>),
    /// `git archive` left files out (`export-ignore`) or added some.
    ArchiveMismatch {
        expected: usize,
        found: usize,
    },
    /// The snapshot directory on the remote exists and is not a private directory of ours.
    RemoteNotPrivate {
        stderr: String,
    },
}

/// How many names a refusal lists before it says how many more there are.
const LISTED: usize = 10;

fn listed(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(LISTED).map(String::as_str).collect();
    match names.len().checked_sub(LISTED).filter(|more| *more > 0) {
        Some(more) => format!("{} and {more} more", shown.join(", ")),
        None => shown.join(", "),
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Dirty => f.write_str(
                "the working tree differs from HEAD and --require-clean is set; commit or stash",
            ),
            Refusal::TooLarge { total, limit } => write!(
                f,
                "{total} bytes in the snapshot, over the limit of {limit} (--snapshot-limit)"
            ),
            Refusal::Submodules(paths) => write!(
                f,
                "submodules are not archived as content ({}); --allow-submodules ships them as empty",
                listed(paths)
            ),
            Refusal::LfsPointers(paths) => write!(
                f,
                "git-lfs pointer files stand for content the archive does not hold ({}); \
                 --allow-submodules ships the pointers",
                listed(paths)
            ),
            Refusal::UnsafeSymlinks(links) => write!(
                f,
                "symlinks that point out of the tree (an absolute target or `..`): {}",
                listed(&links.iter().map(UnsafeLink::to_string).collect::<Vec<_>>())
            ),
            Refusal::SecretNames(paths) => write!(
                f,
                "untracked files with secret-looking names: {}; name each one with --allow-path \
                 to ship it anyway",
                listed(paths)
            ),
            Refusal::Unmerged(paths) => write!(
                f,
                "a merge is in progress and these paths have conflicts: {}; resolve them (or abort \
                 the merge) first",
                listed(paths)
            ),
            Refusal::ArchiveMismatch { expected, found } => write!(
                f,
                "git archive holds {found} files but the tree has {expected} (export-ignore in \
                 .gitattributes?)"
            ),
            Refusal::RemoteNotPrivate { stderr } => write!(
                f,
                "the snapshot directory on the remote host is not a private directory of yours: {}",
                stderr.trim()
            ),
        }
    }
}

/// A link target that stays inside the tree: relative, with no `..` part.
fn target_is_inside(target: &str) -> bool {
    !target.is_empty() && !target.starts_with('/') && !target.split('/').any(|part| part == "..")
}

impl BuiltTree {
    /// The tree behind `facts`. Facts that do not add up are an error: a symlink whose target was
    /// not read would escape the symlink guard.
    pub fn new(facts: Facts) -> Result<Self> {
        for entry in &facts.entries {
            let read = facts.links.iter().any(|link| link.path == entry.path);
            if entry.kind == EntryKind::Symlink && !read {
                bail!("the target of the symlink {:?} was not read", entry.path);
            }
        }
        // the tree of a working tree with files `HEAD` lacks that were left out is `HEAD`'s tree,
        // but the working tree is not that tree
        let cleanliness = if facts.tree == facts.head_tree && facts.left_out.is_empty() {
            Cleanliness::Clean
        } else {
            Cleanliness::Dirty
        };
        Ok(Self { facts, cleanliness })
    }

    /// Check the tree against `policy`, in a fixed order: a merge in progress, dirtiness, size,
    /// submodules and lfs pointers, symlinks, secret names. The first guard that fails is the
    /// refusal.
    pub fn clear(self, policy: &Policy) -> Result<ClearedTree, Refusal> {
        let Self { facts, cleanliness } = self;
        if !facts.unmerged.is_empty() {
            return Err(Refusal::Unmerged(facts.unmerged));
        }
        match (cleanliness, policy.dirty) {
            (Cleanliness::Dirty, DirtyPolicy::Refuse) => return Err(Refusal::Dirty),
            (Cleanliness::Dirty | Cleanliness::Clean, DirtyPolicy::Allow)
            | (Cleanliness::Clean, DirtyPolicy::Refuse) => {}
        }
        let bytes = facts
            .entries
            .iter()
            .fold(0u64, |sum, entry| sum.saturating_add(entry.size));
        if bytes > policy.limit {
            return Err(Refusal::TooLarge {
                total: bytes,
                limit: policy.limit,
            });
        }
        check_submodules(&facts, policy)?;
        check_symlinks(&facts.links)?;
        check_secrets(&facts.added, &policy.allowed)?;
        Ok(ClearedTree {
            lock: lock_of(&facts.entries),
            files: facts
                .entries
                .iter()
                .filter(|e| e.is_archived_file())
                .count(),
            id: facts.tree,
            base: facts.base,
            cleanliness,
            bytes,
        })
    }
}

fn check_submodules(facts: &Facts, policy: &Policy) -> Result<(), Refusal> {
    match policy.submodules {
        SubmodulePolicy::Allow => Ok(()),
        SubmodulePolicy::Refuse => {
            let gitlinks: Vec<String> = facts
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Submodule)
                .map(|entry| entry.path.clone())
                .collect();
            if !gitlinks.is_empty() {
                return Err(Refusal::Submodules(gitlinks));
            }
            if !facts.lfs_pointers.is_empty() {
                return Err(Refusal::LfsPointers(facts.lfs_pointers.clone()));
            }
            Ok(())
        }
    }
}

fn check_symlinks(links: &[Link]) -> Result<(), Refusal> {
    let unsafe_links: Vec<UnsafeLink> = links
        .iter()
        .filter(|link| !target_is_inside(&link.target))
        .map(|link| UnsafeLink {
            path: link.path.clone(),
            target: link.target.clone(),
        })
        .collect();
    if unsafe_links.is_empty() {
        Ok(())
    } else {
        Err(Refusal::UnsafeSymlinks(unsafe_links))
    }
}

fn check_secrets(added: &[String], allowed: &[AllowedPath]) -> Result<(), Refusal> {
    let secret: Vec<String> = added
        .iter()
        .filter(|path| looks_secret(path))
        .filter(|path| !allowed.iter().any(|ok| ok.as_str() == path.as_str()))
        .cloned()
        .collect();
    if secret.is_empty() {
        Ok(())
    } else {
        Err(Refusal::SecretNames(secret))
    }
}

fn lock_of(entries: &[Entry]) -> LockFile {
    entries
        .iter()
        .find(|entry| entry.path == "uv.lock")
        .map_or(LockFile::Absent, |entry| LockFile::Blob(entry.oid.clone()))
}

impl ClearedTree {
    pub fn id(&self) -> &TreeId {
        &self.id
    }

    pub fn base(&self) -> &Commit {
        &self.base
    }

    pub fn cleanliness(&self) -> Cleanliness {
        self.cleanliness
    }

    pub fn lock(&self) -> &LockFile {
        &self.lock
    }

    /// How many files (and links) `git archive` must hold for this tree.
    pub fn files(&self) -> usize {
        self.files
    }

    /// Bytes of file content.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The tree is on the remote host. Only the upload step calls this, once the remote says so;
    /// outside the crate nothing can claim a tree was shipped:
    ///
    /// ```compile_fail,E0624
    /// use remote_run::domain::snapshot::ClearedTree;
    /// fn claim(tree: ClearedTree) {
    ///     let _ = tree.shipped();
    /// }
    /// ```
    pub(crate) fn shipped(self) -> ShippedSnapshot {
        ShippedSnapshot {
            id: self.id,
            base: self.base,
            cleanliness: self.cleanliness,
        }
    }

    /// A cleared tree for the tests of the layers above, which cannot reach the private fields.
    #[cfg(test)]
    pub(crate) fn fake(tree: &str, files: usize) -> Self {
        Self {
            id: TreeId::parse(tree).unwrap(),
            base: Commit::parse("cccccccccccccccccccccccccccccccccccccccc").unwrap(),
            cleanliness: Cleanliness::Clean,
            lock: LockFile::Absent,
            files,
            bytes: 0,
        }
    }
}

impl ShippedSnapshot {
    pub fn id(&self) -> &TreeId {
        &self.id
    }

    pub fn base(&self) -> &Commit {
        &self.base
    }

    pub fn cleanliness(&self) -> Cleanliness {
        self.cleanliness
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn entry(kind: EntryKind, size: u64, path: &str) -> Entry {
        Entry {
            kind,
            oid: ObjectId::parse(A).unwrap(),
            size,
            path: path.into(),
        }
    }

    fn file(path: &str) -> Entry {
        entry(EntryKind::Regular, 10, path)
    }

    fn facts(entries: Vec<Entry>) -> Facts {
        Facts {
            base: Commit::parse(C).unwrap(),
            head_tree: TreeId::parse(A).unwrap(),
            tree: TreeId::parse(A).unwrap(),
            entries,
            added: vec![],
            left_out: vec![],
            unmerged: vec![],
            links: vec![],
            lfs_pointers: vec![],
        }
    }

    fn policy() -> Policy {
        Policy {
            inclusion: Inclusion::TrackedOnly,
            allowed: vec![],
            submodules: SubmodulePolicy::Refuse,
            limit: 1000,
            dirty: DirtyPolicy::Allow,
        }
    }

    fn clear(facts: Facts, policy: &Policy) -> Result<ClearedTree, Refusal> {
        BuiltTree::new(facts).unwrap().clear(policy)
    }

    #[test]
    fn a_plain_tree_clears_and_remembers_what_it_holds() {
        let cleared = clear(facts(vec![file("a.py"), file("uv.lock")]), &policy()).unwrap();
        assert_eq!(cleared.files(), 2);
        assert_eq!(cleared.bytes(), 20);
        assert_eq!(cleared.cleanliness(), Cleanliness::Clean);
        assert_eq!(cleared.id().as_str(), A);
        assert_eq!(cleared.base().as_str(), C);
        assert_eq!(cleared.lock(), &LockFile::Blob(ObjectId::parse(A).unwrap()));
        let none = clear(facts(vec![file("a.py")]), &policy()).unwrap();
        assert_eq!(none.lock(), &LockFile::Absent);
    }

    #[test]
    fn a_tree_that_differs_from_head_is_dirty_and_require_clean_refuses_it() {
        let mut dirty = facts(vec![file("a.py")]);
        dirty.tree = TreeId::parse(B).unwrap();
        let cleared = clear(dirty, &policy()).unwrap();
        assert_eq!(cleared.cleanliness(), Cleanliness::Dirty);
        let strict = Policy {
            dirty: DirtyPolicy::Refuse,
            ..policy()
        };
        let mut dirty = facts(vec![file("a.py")]);
        dirty.tree = TreeId::parse(B).unwrap();
        assert_eq!(clear(dirty, &strict).unwrap_err(), Refusal::Dirty);
        assert!(clear(facts(vec![file("a.py")]), &strict).is_ok());
    }

    /// `git add new.py` or `git mv a b`, or a plain untracked file, with only tracked files
    /// shipped: the snapshot is `HEAD`'s tree but the working tree is not.
    #[test]
    fn files_the_snapshot_leaves_out_make_it_dirty_even_when_the_tree_is_heads() {
        for left_out in [vec!["new.py".to_string()], vec!["b.py".to_string()]] {
            let mut f = facts(vec![file("a.py")]);
            f.left_out = left_out;
            let cleared = clear(f, &policy()).unwrap();
            assert_eq!(cleared.cleanliness(), Cleanliness::Dirty);
            let mut f = facts(vec![file("a.py")]);
            f.left_out = vec!["new.py".into()];
            let strict = Policy {
                dirty: DirtyPolicy::Refuse,
                ..policy()
            };
            assert_eq!(clear(f, &strict).unwrap_err(), Refusal::Dirty);
        }
    }

    #[test]
    fn a_merge_in_progress_is_refused_before_anything_else() {
        let mut f = facts(vec![file("a.py")]);
        f.unmerged = vec!["a.py".into()];
        f.tree = TreeId::parse(B).unwrap();
        let strict = Policy {
            dirty: DirtyPolicy::Refuse,
            limit: 0,
            ..policy()
        };
        let refusal = clear(f, &strict).unwrap_err();
        assert_eq!(refusal, Refusal::Unmerged(vec!["a.py".into()]));
        assert!(refusal.to_string().contains("a merge is in progress"));
    }

    #[test]
    fn the_size_limit_is_on_the_content_of_the_tree() {
        let big = vec![
            entry(EntryKind::Regular, 600, "a"),
            entry(EntryKind::Executable, 401, "b"),
        ];
        assert_eq!(
            clear(facts(big), &policy()).unwrap_err(),
            Refusal::TooLarge {
                total: 1001,
                limit: 1000
            }
        );
        let exact = vec![
            entry(EntryKind::Regular, 600, "a"),
            entry(EntryKind::Executable, 400, "b"),
        ];
        assert!(clear(facts(exact), &policy()).is_ok());
        let huge = vec![entry(EntryKind::Regular, u64::MAX, "a"), file("b")];
        assert!(clear(facts(huge), &policy()).is_err(), "no wrap-around");
    }

    #[test]
    fn submodules_and_lfs_pointers_are_refused_unless_allowed() {
        let gitlink = || {
            facts(vec![
                file("a"),
                entry(EntryKind::Submodule, 0, "vendor/lib"),
            ])
        };
        assert_eq!(
            clear(gitlink(), &policy()).unwrap_err(),
            Refusal::Submodules(vec!["vendor/lib".into()])
        );
        let mut lfs = facts(vec![file("model.bin")]);
        lfs.lfs_pointers = vec!["model.bin".into()];
        assert_eq!(
            clear(lfs, &policy()).unwrap_err(),
            Refusal::LfsPointers(vec!["model.bin".into()])
        );
        let allow = Policy {
            submodules: SubmodulePolicy::Allow,
            ..policy()
        };
        let cleared = clear(gitlink(), &allow).unwrap();
        assert_eq!(cleared.files(), 1, "a gitlink is not an archived file");
    }

    #[test]
    fn symlinks_must_point_inside_the_tree() {
        let links = |target: &str| {
            let mut f = facts(vec![entry(EntryKind::Symlink, 3, "l")]);
            f.links = vec![Link {
                path: "l".into(),
                target: target.into(),
            }];
            f
        };
        for ok in ["a/b", "b", "./b", "a.b/c", "a..b"] {
            assert!(clear(links(ok), &policy()).is_ok(), "{ok}");
        }
        for bad in ["/etc/passwd", "../x", "a/../../x", "a/..", "..", "", "/"] {
            let err = clear(links(bad), &policy()).unwrap_err();
            assert_eq!(
                err,
                Refusal::UnsafeSymlinks(vec![UnsafeLink {
                    path: "l".into(),
                    target: bad.into()
                }]),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_symlink_whose_target_was_not_read_is_an_error() {
        let f = facts(vec![entry(EntryKind::Symlink, 3, "l")]);
        assert!(BuiltTree::new(f).is_err());
    }

    #[test]
    fn secret_named_additions_are_refused_unless_named_whole() {
        let with = |added: &[&str]| {
            let mut f = facts(added.iter().map(|p| file(p)).collect());
            f.added = added.iter().map(|p| p.to_string()).collect();
            f
        };
        assert_eq!(
            clear(with(&["ok.py", ".env", "k/server.pem"]), &policy()).unwrap_err(),
            Refusal::SecretNames(vec![".env".into(), "k/server.pem".into()])
        );
        let allow = |paths: &[&str]| Policy {
            allowed: paths
                .iter()
                .map(|p| AllowedPath::parse(p).unwrap())
                .collect(),
            ..policy()
        };
        assert!(clear(with(&[".env"]), &allow(&[".env"])).is_ok());
        assert!(clear(with(&[".env", "x/.env"]), &allow(&[".env"])).is_err());
        assert!(clear(with(&["x/.env"]), &allow(&[".env"])).is_err());
        // a secret-named file that HEAD already has is tracked: it is not an addition
        let tracked = facts(vec![file(".env.example")]);
        assert!(clear(tracked, &policy()).is_ok());
    }

    #[test]
    fn the_first_failing_guard_is_the_refusal() {
        let mut f = facts(vec![entry(EntryKind::Submodule, 0, "s"), file(".env")]);
        f.added = vec![".env".into()];
        f.tree = TreeId::parse(B).unwrap();
        let strict = Policy {
            dirty: DirtyPolicy::Refuse,
            limit: 0,
            ..policy()
        };
        let mut again = facts(vec![entry(EntryKind::Submodule, 0, "s"), file(".env")]);
        again.added = vec![".env".into()];
        again.tree = TreeId::parse(B).unwrap();
        assert_eq!(clear(f, &strict).unwrap_err(), Refusal::Dirty);
        let sized = Policy {
            dirty: DirtyPolicy::Allow,
            ..strict
        };
        assert!(matches!(
            clear(again, &sized),
            Err(Refusal::TooLarge { .. })
        ));
    }

    #[test]
    fn a_refusal_lists_ten_names_and_counts_the_rest() {
        let names: Vec<String> = (0..13).map(|n| format!("f{n}")).collect();
        let text = Refusal::SecretNames(names).to_string();
        assert!(
            text.ends_with("f9 and 3 more; name each one with --allow-path to ship it anyway"),
            "{text}"
        );
        assert!(!text.contains("f10"), "{text}");
        let few = Refusal::Submodules(vec!["a".into(), "b".into()]).to_string();
        assert!(few.contains("(a, b)"), "{few}");
    }

    #[test]
    fn a_refusal_tells_the_user_what_to_do() {
        let link = UnsafeLink {
            path: "a".into(),
            target: "/etc".into(),
        };
        let cases = [
            (
                Refusal::Dirty,
                "the working tree differs from HEAD and --require-clean is set; commit or stash",
            ),
            (
                Refusal::TooLarge { total: 9, limit: 5 },
                "9 bytes in the snapshot, over the limit of 5 (--snapshot-limit)",
            ),
            (
                Refusal::LfsPointers(vec!["m.bin".into()]),
                "git-lfs pointer files stand for content the archive does not hold (m.bin); \
                 --allow-submodules ships the pointers",
            ),
            (
                Refusal::UnsafeSymlinks(vec![link]),
                "symlinks that point out of the tree (an absolute target or `..`): a -> /etc",
            ),
            (
                Refusal::ArchiveMismatch {
                    expected: 3,
                    found: 2,
                },
                "git archive holds 2 files but the tree has 3 (export-ignore in .gitattributes?)",
            ),
            (
                Refusal::RemoteNotPrivate {
                    stderr: "rr: x is not a private directory of yours\n".into(),
                },
                "the snapshot directory on the remote host is not a private directory of yours: \
                 rr: x is not a private directory of yours",
            ),
        ];
        for (refusal, text) in cases {
            assert_eq!(refusal.to_string(), text);
        }
    }

    #[test]
    fn the_dirty_flag_serializes_as_a_json_bool() {
        let json = |c: Cleanliness| serde_json::to_string(&c).unwrap();
        assert_eq!(json(Cleanliness::Dirty), "true");
        assert_eq!(json(Cleanliness::Clean), "false");
    }
}
