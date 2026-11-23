//! Build the snapshot's tree locally and gather what the guards need to know about it. The user's
//! index, branch, stash, refs and object directory are not touched: the tree is made in a
//! temporary index and its objects are stored in a scratch object directory (the repository's own
//! objects are only an alternate to read from), so a tree the guards refuse leaves nothing behind
//! once the [`Workspace`] is dropped.

use crate::domain::argv::{Argv, Stdin};
use crate::domain::ids::Commit;
use crate::domain::sanitize::sanitize;
use crate::domain::snapshot::{
    BuiltTree, Entry, EntryKind, Facts, Inclusion, Link, ObjectStore, Policy, Repo, TreeId,
    lfs_pointer_paths, may_be_lfs_pointer, parse_head, parse_ls_tree, parse_names,
    parse_not_in_head, parse_unmerged, toplevel,
};
use crate::ports::{Exec, Scratch, Workspace};
use anyhow::{Result, bail};

/// What git said, ready for the guards, and what the snapshot leaves out.
pub struct Built {
    pub repo: Repo,
    pub tree: BuiltTree,
    /// Files the working tree has and `HEAD` does not, left out because untracked files are not
    /// included. Empty when they are.
    pub skipped: Vec<String>,
    /// Holds the scratch objects the tree is stored in, which `git archive` still reads: it goes
    /// when the caller drops it, once the tree is shipped or refused.
    pub workspace: Workspace,
}

/// What git prints to stderr outside a repository.
const NOT_A_REPOSITORY: &str = "not a git repository";

/// The exit code of `git grep` when nothing matched.
const GREP_NO_MATCH: i32 = 1;

fn read(exec: &dyn Exec, argv: &Argv, what: &str) -> Result<String> {
    exec.capture(argv, Stdin::Null)?.into_stdout(what)
}

fn locate_repo(exec: &dyn Exec) -> Result<Repo> {
    let found = exec.capture(&toplevel(), Stdin::Null)?;
    if found.code != 0 && found.stderr.contains(NOT_A_REPOSITORY) {
        bail!("snapshot mode needs a git repository: this directory is not in one");
    }
    Repo::parse(&found.into_stdout("git rev-parse --show-toplevel")?)
}

/// The tree of the working tree: `base` plus the changes of tracked files (and the untracked files
/// when asked), written from a temporary index into the scratch object directory.
fn write_tree(
    exec: &dyn Exec,
    workspace: &Workspace,
    repo: &Repo,
    base: &Commit,
    inclusion: Inclusion,
) -> Result<TreeId> {
    let index = workspace.index();
    read(exec, &repo.read_tree(index, base), "git read-tree")?;
    read(exec, &repo.add(index, inclusion), "git add")?;
    TreeId::parse(&read(exec, &repo.write_tree(index), "git write-tree")?)
}

/// The repository with a scratch object store for what the build stores, reading the repository's
/// own objects through an alternate.
fn in_scratch(exec: &dyn Exec, repo: Repo, workspace: &Workspace) -> Result<Repo> {
    let printed = read(exec, &repo.objects_directory(), "git rev-parse --git-path")?;
    let shared = repo.absolute_objects_directory(&printed)?;
    let store = ObjectStore::new(workspace.objects(), &shared)?;
    Ok(repo.in_scratch(store))
}

/// The target of every symlink, one `git cat-file` each: a repository has few of them.
fn read_links(exec: &dyn Exec, repo: &Repo, entries: &[Entry]) -> Result<Vec<Link>> {
    let mut links = Vec::new();
    for entry in entries.iter().filter(|e| e.kind == EntryKind::Symlink) {
        let target = exec
            .capture(&repo.link_target(&entry.oid), Stdin::Null)?
            .into_stdout("git cat-file")?;
        links.push(Link {
            path: entry.path.clone(),
            target,
        });
    }
    Ok(links)
}

/// The git-lfs pointer files: searched only when some file is small enough to be one.
fn find_lfs_pointers(
    exec: &dyn Exec,
    repo: &Repo,
    tree: &TreeId,
    entries: &[Entry],
) -> Result<Vec<String>> {
    if !entries.iter().any(may_be_lfs_pointer) {
        return Ok(vec![]);
    }
    let grep = exec.capture(&repo.lfs_pointers(tree), Stdin::Null)?;
    match grep.code {
        0 => lfs_pointer_paths(entries, tree, &grep.stdout),
        GREP_NO_MATCH => Ok(vec![]),
        code => bail!(
            "git grep for lfs pointers failed ({code}): {}",
            sanitize(grep.stderr.trim())
        ),
    }
}

/// What the working tree has beyond `HEAD`, from one `git status`.
struct Status {
    /// Left out of the snapshot when only tracked files are shipped, so empty when untracked files
    /// are included.
    left_out: Vec<String>,
    unmerged: Vec<String>,
}

fn read_status(exec: &dyn Exec, repo: &Repo, inclusion: Inclusion) -> Result<Status> {
    let text = read(exec, &repo.status(), "git status")?;
    let left_out = match inclusion {
        Inclusion::WithUntracked => vec![],
        Inclusion::TrackedOnly => parse_not_in_head(&text),
    };
    Ok(Status {
        left_out,
        unmerged: parse_unmerged(&text),
    })
}

/// Make the tree of the working tree and read the facts the guards judge it by.
pub fn build(exec: &dyn Exec, scratch: &dyn Scratch, policy: &Policy) -> Result<Built> {
    let repo = locate_repo(exec)?;
    let base = parse_head(&read(exec, &repo.head(), "git rev-parse HEAD")?)?;
    let head_tree = TreeId::parse(&read(exec, &repo.head_tree(), "git rev-parse HEAD^{tree}")?)?;
    let workspace = scratch.workspace()?;
    let repo = in_scratch(exec, repo, &workspace)?;
    let tree = write_tree(exec, &workspace, &repo, &base, policy.inclusion)?;
    let entries = parse_ls_tree(&read(exec, &repo.ls_tree(&tree), "git ls-tree")?)?;
    let added = parse_names(&read(exec, &repo.added(&base, &tree), "git diff-tree")?);
    let links = read_links(exec, &repo, &entries)?;
    let lfs_pointers = find_lfs_pointers(exec, &repo, &tree, &entries)?;
    let Status { left_out, unmerged } = read_status(exec, &repo, policy.inclusion)?;
    let facts = Facts {
        base,
        head_tree,
        tree,
        entries,
        added,
        left_out: left_out.clone(),
        unmerged,
        links,
        lfs_pointers,
    };
    Ok(Built {
        repo,
        tree: BuiltTree::new(facts)?,
        skipped: left_out,
        workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::snapshot::{Cleanliness, DirtyPolicy, SubmodulePolicy};
    use crate::ports::fake::{FakeExec, FakeScratch, Reply, output};

    const BASE: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const HEAD_TREE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const BLOB: &str = "dddddddddddddddddddddddddddddddddddddddd";

    fn policy(inclusion: Inclusion) -> Policy {
        Policy {
            inclusion,
            allowed: vec![],
            submodules: SubmodulePolicy::Refuse,
            limit: 1 << 20,
            dirty: DirtyPolicy::Allow,
        }
    }

    /// The replies of a repository with one big file and, when `links` is set, a symlink.
    fn replies(links: bool, tail: Vec<Reply>) -> Vec<Reply> {
        let mut listing = format!("100644 blob {BLOB}    5000\tbig.py\0");
        if links {
            listing.push_str(&format!("120000 blob {BLOB}       3\tlink\0"));
        }
        let mut all = vec![
            output(0, "/work/proj\n", ""),
            output(0, &format!("{BASE}\n"), ""),
            output(0, &format!("{HEAD_TREE}\n"), ""),
            output(0, ".git/objects\n", ""),
            output(0, "", ""),
            output(0, "", ""),
            output(0, &format!("{TREE}\n"), ""),
            output(0, &listing, ""),
            output(0, "new.py\0", ""),
        ];
        if links {
            all.push(output(0, "a/b", ""));
        }
        all.extend(tail);
        all
    }

    fn words(exec: &FakeExec) -> Vec<String> {
        exec.words()
            .iter()
            .map(|call| call[1..].join(" "))
            .collect()
    }

    #[test]
    fn the_tree_is_made_in_a_temporary_index_and_nothing_else_is_written() {
        let status = output(0, "?? new.py\0 M big.py\0", "");
        let exec = FakeExec::new(replies(true, vec![status]), vec![]);
        let scratch = FakeScratch::default();
        let built = build(&exec, &scratch, &policy(Inclusion::TrackedOnly)).unwrap();
        assert_eq!(built.skipped, ["new.py"]);
        assert_eq!(scratch.taken.get(), 1);
        assert_eq!(
            scratch.dropped.get(),
            0,
            "the objects stay until the caller is done with the tree"
        );
        let calls = words(&exec);
        assert_eq!(calls[0], "rev-parse --show-toplevel");
        assert!(
            calls[3].ends_with("rev-parse --git-path objects"),
            "{}",
            calls[3]
        );
        assert!(
            calls[4].starts_with("-C /work/proj read-tree "),
            "{}",
            calls[4]
        );
        assert!(calls[5].ends_with("add -u"), "{}", calls[5]);
        assert!(calls[6].ends_with("write-tree"), "{}", calls[6]);
        assert!(
            calls[9].ends_with(&format!("cat-file blob {BLOB}")),
            "{}",
            calls[9]
        );
        assert_scratch_envs(&exec);
        drop(built);
        assert_eq!(scratch.dropped.get(), 1, "and go with it");
    }

    /// Which calls carry which of git's variables, by their place in the sequence of `replies`.
    fn assert_scratch_envs(exec: &FakeExec) {
        let names = |at: usize| -> Vec<String> {
            let calls = exec.calls.borrow();
            calls[at]
                .0
                .envs()
                .iter()
                .map(|v| v.name().to_string())
                .collect()
        };
        let store = ["GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"];
        assert!(
            names(4).iter().eq(["GIT_INDEX_FILE"]),
            "read-tree: index only"
        );
        for at in [5, 6] {
            assert!(
                names(at).iter().eq([store[0], store[1], "GIT_INDEX_FILE"]),
                "{at}: {:?}",
                names(at)
            );
        }
        for at in [7, 8, 9] {
            assert!(names(at).iter().eq(store), "{at}: {:?}", names(at));
        }
        for at in [0, 1, 2, 3, 10] {
            assert!(names(at).is_empty(), "{at} must not touch the objects");
        }
        let calls = exec.calls.borrow();
        let value = |at: usize, name: &str| {
            calls[at]
                .0
                .envs()
                .iter()
                .find(|v| v.name() == name)
                .unwrap()
                .value()
                .to_string()
        };
        assert_eq!(value(4, "GIT_INDEX_FILE"), "/scratch/index");
        assert_eq!(value(5, "GIT_OBJECT_DIRECTORY"), "/scratch/objects");
        assert_eq!(
            value(5, "GIT_ALTERNATE_OBJECT_DIRECTORIES"),
            "/work/proj/.git/objects"
        );
    }

    /// The tree git built is `HEAD`'s (`HEAD_TREE`) while the working tree has a file staged as new,
    /// or one renamed away, or a plain untracked one: that is not a clean snapshot.
    #[test]
    fn a_tree_equal_to_heads_is_dirty_when_the_working_tree_has_files_it_leaves_out() {
        for status in [
            "A  new.py\0",
            "R  b.py\0a.py\0",
            "?? scratch.txt\0",
            "AM new.py\0",
        ] {
            let mut all = replies(false, vec![output(0, status, "")]);
            all[6] = output(0, &format!("{HEAD_TREE}\n"), "");
            let exec = FakeExec::new(all, vec![]);
            let built = build(
                &exec,
                &FakeScratch::default(),
                &policy(Inclusion::TrackedOnly),
            )
            .unwrap();
            let cleared = built.tree.clear(&policy(Inclusion::TrackedOnly)).unwrap();
            assert_eq!(cleared.cleanliness(), Cleanliness::Dirty, "{status:?}");
        }
        let mut all = replies(false, vec![output(0, " M big.py\0", "")]);
        all[6] = output(0, &format!("{HEAD_TREE}\n"), "");
        let built = build(
            &FakeExec::new(all, vec![]),
            &FakeScratch::default(),
            &policy(Inclusion::TrackedOnly),
        )
        .unwrap();
        let cleared = built.tree.clear(&policy(Inclusion::TrackedOnly)).unwrap();
        assert_eq!(cleared.cleanliness(), Cleanliness::Clean);
    }

    #[test]
    fn a_merge_in_progress_is_a_refusal_whatever_is_included() {
        for inclusion in [Inclusion::TrackedOnly, Inclusion::WithUntracked] {
            let exec = FakeExec::new(
                replies(false, vec![output(0, "UU conflict.py\0", "")]),
                vec![],
            );
            let built = build(&exec, &FakeScratch::default(), &policy(inclusion)).unwrap();
            assert_eq!(
                built.tree.clear(&policy(inclusion)).unwrap_err(),
                crate::domain::snapshot::Refusal::Unmerged(vec!["conflict.py".into()])
            );
        }
    }

    #[test]
    fn untracked_files_are_added_with_a_dash_a_and_nothing_is_reported_skipped() {
        let exec = FakeExec::new(replies(false, vec![output(0, "?? new.py\0", "")]), vec![]);
        let scratch = FakeScratch::default();
        let built = build(&exec, &scratch, &policy(Inclusion::WithUntracked)).unwrap();
        assert!(built.skipped.is_empty());
        assert!(words(&exec)[5].ends_with("add -A"));
    }

    #[test]
    fn a_small_file_is_searched_for_an_lfs_pointer_and_a_big_one_alone_is_not() {
        let small = format!("100644 blob {BLOB}     130\tmodel.bin\0");
        let exec = FakeExec::new(
            {
                let mut r = replies(false, vec![output(0, "", "")]);
                r[7] = output(0, &small, "");
                r.insert(9, output(0, &format!("{TREE}:model.bin\0"), ""));
                r
            },
            vec![],
        );
        let built = build(
            &exec,
            &FakeScratch::default(),
            &policy(Inclusion::TrackedOnly),
        )
        .unwrap();
        assert!(words(&exec).iter().any(|c| c.contains("grep -l -z")));
        let err = built
            .tree
            .clear(&policy(Inclusion::TrackedOnly))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::domain::snapshot::Refusal::LfsPointers(_)
        ));
        let big = FakeExec::new(replies(false, vec![output(0, "", "")]), vec![]);
        build(
            &big,
            &FakeScratch::default(),
            &policy(Inclusion::TrackedOnly),
        )
        .unwrap();
        assert!(
            !words(&big).iter().any(|c| c.contains(" grep ")),
            "no candidates, no grep"
        );
    }

    #[test]
    fn a_grep_that_fails_is_an_error_and_one_that_matched_nothing_is_not() {
        let small = format!("100644 blob {BLOB}     130\tx\0");
        let with_grep = |reply: Reply| {
            let mut r = replies(false, vec![output(0, "", "")]);
            r[7] = output(0, &small, "");
            r.insert(9, reply);
            FakeExec::new(r, vec![])
        };
        let p = policy(Inclusion::TrackedOnly);
        assert!(build(&with_grep(output(1, "", "")), &FakeScratch::default(), &p).is_ok());
        let err = build(
            &with_grep(output(2, "", "boom\u{1b}[2J")),
            &FakeScratch::default(),
            &p,
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(err, "git grep for lfs pointers failed (2): boom?[2J");
    }

    #[test]
    fn outside_a_repository_and_with_a_failing_git_there_is_an_error_and_no_tree() {
        let p = policy(Inclusion::TrackedOnly);
        let outside = FakeExec::new(
            vec![output(
                128,
                "",
                "fatal: not a git repository (or any parent)",
            )],
            vec![],
        );
        let err = build(&outside, &FakeScratch::default(), &p)
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.starts_with("snapshot mode needs a git repository"),
            "{err}"
        );
        for failing in 1..10 {
            let mut all = replies(false, vec![output(0, "", "")]);
            all.truncate(failing);
            all.push(output(128, "", "fatal: broken"));
            let exec = FakeExec::new(all, vec![]);
            let scratch = FakeScratch::default();
            assert!(build(&exec, &scratch, &p).is_err(), "step {failing}");
            assert_eq!(scratch.taken.get(), scratch.dropped.get(), "step {failing}");
        }
    }

    #[test]
    fn a_missing_head_is_not_a_tree() {
        let exec = FakeExec::new(
            vec![
                output(0, "/work/proj\n", ""),
                output(128, "", "fatal: Needed a single revision"),
            ],
            vec![],
        );
        let p = policy(Inclusion::TrackedOnly);
        assert!(build(&exec, &FakeScratch::default(), &p).is_err());
    }
}
