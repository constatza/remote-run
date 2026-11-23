//! Build the snapshot's tree locally and gather what the guards need to know about it. The user's
//! index, branch, stash and refs are not touched: the tree is made in a temporary index, and
//! `git write-tree` only stores objects, which nothing refers to.

use crate::domain::argv::{Argv, Stdin};
use crate::domain::ids::Commit;
use crate::domain::sanitize::sanitize;
use crate::domain::snapshot::{
    BuiltTree, Entry, EntryKind, Facts, Inclusion, Link, Policy, Repo, TreeId, lfs_pointer_paths,
    may_be_lfs_pointer, parse_ls_tree, parse_names, parse_not_in_head, toplevel,
};
use crate::ports::{Exec, Scratch};
use anyhow::{Result, bail};

/// What git said, ready for the guards, and what the snapshot leaves out.
pub struct Built {
    pub repo: Repo,
    pub tree: BuiltTree,
    /// Files the working tree has and `HEAD` does not, left out because untracked files are not
    /// included. Empty when they are.
    pub skipped: Vec<String>,
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
/// when asked), written from a temporary index.
fn write_tree(
    exec: &dyn Exec,
    scratch: &dyn Scratch,
    repo: &Repo,
    base: &Commit,
    inclusion: Inclusion,
) -> Result<TreeId> {
    let index = scratch.index()?;
    let file = index.file();
    read(exec, &repo.read_tree(file, base), "git read-tree")?;
    read(exec, &repo.add(file, inclusion), "git add")?;
    TreeId::parse(&read(exec, &repo.write_tree(file), "git write-tree")?)
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

/// What a snapshot of the working tree would leave out, for the user to see.
fn find_skipped(exec: &dyn Exec, repo: &Repo, inclusion: Inclusion) -> Result<Vec<String>> {
    match inclusion {
        Inclusion::WithUntracked => Ok(vec![]),
        Inclusion::TrackedOnly => {
            let status = read(exec, &repo.status(), "git status")?;
            Ok(parse_not_in_head(&status))
        }
    }
}

/// Make the tree of the working tree and read the facts the guards judge it by.
pub fn build(exec: &dyn Exec, scratch: &dyn Scratch, policy: &Policy) -> Result<Built> {
    let repo = locate_repo(exec)?;
    let base = Commit::parse(&read(exec, &repo.head(), "git rev-parse HEAD")?)?;
    let head_tree = TreeId::parse(&read(exec, &repo.head_tree(), "git rev-parse HEAD^{tree}")?)?;
    let tree = write_tree(exec, scratch, &repo, &base, policy.inclusion)?;
    let entries = parse_ls_tree(&read(exec, &repo.ls_tree(&tree), "git ls-tree")?)?;
    let added = parse_names(&read(exec, &repo.added(&base, &tree), "git diff-tree")?);
    let links = read_links(exec, &repo, &entries)?;
    let lfs_pointers = find_lfs_pointers(exec, &repo, &tree, &entries)?;
    let skipped = find_skipped(exec, &repo, policy.inclusion)?;
    let facts = Facts {
        base,
        head_tree,
        tree,
        entries,
        added,
        links,
        lfs_pointers,
    };
    Ok(Built {
        repo,
        tree: BuiltTree::new(facts)?,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::snapshot::{DirtyPolicy, SubmodulePolicy};
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
            1,
            "the index is gone when build returns"
        );
        let calls = words(&exec);
        assert_eq!(calls[0], "rev-parse --show-toplevel");
        assert!(
            calls[3].starts_with("-C /work/proj read-tree "),
            "{}",
            calls[3]
        );
        assert!(calls[4].ends_with("add -u"), "{}", calls[4]);
        assert!(calls[5].ends_with("write-tree"), "{}", calls[5]);
        assert!(
            calls[8].ends_with(&format!("cat-file blob {BLOB}")),
            "{}",
            calls[8]
        );
        let with_index: Vec<usize> = exec
            .calls
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, (argv, _))| !argv.envs().is_empty())
            .map(|(index, _)| index)
            .collect();
        assert_eq!(with_index, [3, 4, 5], "only read-tree, add and write-tree");
        let argv = exec.calls.borrow()[3].0.clone();
        assert_eq!(argv.envs()[0].value(), "/scratch/index");
    }

    #[test]
    fn untracked_files_are_added_with_a_dash_a_and_nothing_is_reported_skipped() {
        let exec = FakeExec::new(replies(false, vec![]), vec![]);
        let scratch = FakeScratch::default();
        let built = build(&exec, &scratch, &policy(Inclusion::WithUntracked)).unwrap();
        assert!(built.skipped.is_empty());
        assert!(words(&exec)[4].ends_with("add -A"));
        assert!(
            !words(&exec).iter().any(|call| call.contains("status")),
            "no status call when nothing is left out"
        );
    }

    #[test]
    fn a_small_file_is_searched_for_an_lfs_pointer_and_a_big_one_alone_is_not() {
        let small = format!("100644 blob {BLOB}     130\tmodel.bin\0");
        let exec = FakeExec::new(
            {
                let mut r = replies(false, vec![output(0, "", "")]);
                r[6] = output(0, &small, "");
                r.insert(8, output(0, &format!("{TREE}:model.bin\0"), ""));
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
            r[6] = output(0, &small, "");
            r.insert(8, reply);
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
        for failing in 1..8 {
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
