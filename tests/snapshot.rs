//! `rr run --mode snapshot` with the real binary, a real temporary git repository, and a stub `ssh`
//! that runs the script it is given in a local directory standing in for the cluster's home. Nothing
//! here touches the network, `~/.ssh`, the user's git configuration or the real `HOME`.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // starts processes and reads files by design

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const CONFIG: &str = "dir = \"proj\"\n";

/// What ssh does: the remote command is its last argument, run by bash in the stand-in home, with
/// ssh's stdin (the archive) passed on. Every script is kept, numbered, so a test can see what was
/// asked of the remote.
const STUB: &str = "for last; do :; done; n=$(ls \"$CALLS\" | wc -l); printf '%s' \"$last\" > \"$CALLS/$n\"; cd \"$REMOTE_HOME\" && exec bash -c \"$last\"";

/// Everything one test needs: a repository, the stand-in remote home and the stub `ssh`.
struct World {
    dir: tempfile::TempDir,
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

impl World {
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// A repository with two committed files, `a.py` and `b.py`, on branch `main`.
    fn new() -> Self {
        let world = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        for sub in ["bin", "calls", "repo", "remote/proj", "home/.config/rr"] {
            fs::create_dir_all(world.path(sub)).unwrap();
        }
        let ssh = world.path("bin/ssh");
        fs::write(&ssh, format!("#!/bin/sh\n{STUB}\n")).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(world.path("home/.config/rr/config.toml"), CONFIG).unwrap();
        world.git(&["init", "-q", "-b", "main"]);
        world.write("a.py", "print('a')\n");
        world.write("b.py", "print('b')\n");
        world.git(&["add", "."]);
        world.git(&["commit", "-q", "-m", "first"]);
        world
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.path("repo").join(name), text).unwrap();
    }

    /// Git as the test wants it: no user or system configuration, a fixed identity.
    fn git_command(&self) -> Command {
        let mut git = Command::new("git");
        git.current_dir(self.path("repo"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid");
        git
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.git_command().args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out.stderr);
        String::from_utf8(out.stdout).unwrap()
    }

    fn rr(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rr"))
            .args(["--host", "cluster"])
            .args(args)
            .current_dir(self.path("repo"))
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .env("HOME", self.path("home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("CALLS", self.path("calls"))
            .env("REMOTE_HOME", self.path("remote"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `rr run --mode snapshot FLAGS -- CMD...`
    fn snapshot_run(&self, flags: &[&str], cmd: &[&str]) -> Output {
        let mut args = vec!["run", "--mode", "snapshot"];
        args.extend(flags);
        args.push("--");
        args.extend(cmd);
        self.rr(&args)
    }

    /// The snapshots on the stand-in remote, by directory name.
    fn snapshots(&self) -> Vec<String> {
        let root = self.path("remote/proj/.rr-snapshots");
        let mut names: Vec<String> = fs::read_dir(root)
            .map(|dir| {
                dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The scripts the stub ssh was given, in order.
    fn scripts(&self) -> Vec<String> {
        let count = fs::read_dir(self.path("calls")).unwrap().count();
        (0..count)
            .map(|n| fs::read_to_string(self.path("calls").join(n.to_string())).unwrap())
            .collect()
    }

    /// How many ssh calls carried an archive.
    fn uploads(&self) -> usize {
        self.scripts()
            .iter()
            .filter(|script| script.contains("tar -x --no-same-owner"))
            .count()
    }

    /// What the user's repository looks like to git, for "nothing changed" checks.
    fn repo_state(&self) -> Vec<String> {
        [
            &["ls-files", "-s"][..],
            &["rev-parse", "HEAD"],
            &["branch", "--list"],
            &["stash", "list"],
            &["for-each-ref"],
            &["status", "--porcelain", "-uall"],
            &["diff", "--cached"],
        ]
        .iter()
        .map(|args| self.git(args))
        .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_first_run_uploads_and_a_second_run_of_the_same_tree_transfers_nothing() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("a.py", "print('changed')\n");
    let first = world.snapshot_run(&[], &["cat", "a.py"]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));
    assert_eq!(
        text(&first.stdout),
        "print('changed')\n",
        "the working tree runs"
    );
    assert!(
        text(&first.stderr).contains("(uploaded)"),
        "{}",
        text(&first.stderr)
    );
    assert_eq!(world.uploads(), 1);
    let snapshots = world.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].len(), 40);

    let second = world.snapshot_run(&[], &["cat", "a.py"]);
    assert_eq!(second.status.code(), Some(0), "{}", text(&second.stderr));
    assert!(text(&second.stderr).contains("already on the remote"));
    assert_eq!(world.uploads(), 1, "the second run sent no archive");
    assert_eq!(world.snapshots(), snapshots, "and made no new snapshot");
}

#[test]
fn the_command_runs_in_the_snapshot_with_its_environment_and_its_exit_code_comes_back() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("a.py", "print('changed')\n");
    let probe =
        "echo \"$PWD|$PYTHONPATH|$UV_PROJECT_ENVIRONMENT|$RR_SNAPSHOT_ID|$RR_DIRTY\"; exit 7";
    let run = world.snapshot_run(&[], &["sh", "-c", probe]);
    assert_eq!(
        run.status.code(),
        Some(7),
        "a remote code is passed on as it is"
    );
    let id = &world.snapshots()[0];
    let out = text(&run.stdout);
    let fields: Vec<&str> = out.trim().split('|').collect();
    let root = fs::canonicalize(world.path("remote/proj")).unwrap();
    assert_eq!(fields[0], format!("{}/.rr-snapshots/{id}", root.display()));
    assert_eq!(
        fields[1],
        format!("{}/.rr-snapshots/{id}/src", root.display())
    );
    assert_eq!(fields[2], format!("{}/.venv", root.display()));
    assert_eq!(fields[3], id.as_str());
    assert_eq!(fields[4], "true", "the working tree differs from HEAD");
    let mode = fs::metadata(root.join(".rr-snapshots").join(id))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o500, "published read-only");
}

#[test]
fn an_untracked_file_is_not_shipped_unless_asked_and_then_it_is_a_different_snapshot() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("new.txt", "not in git\n");
    let tracked = world.snapshot_run(&[], &["ls"]);
    assert_eq!(tracked.status.code(), Some(0), "{}", text(&tracked.stderr));
    let listing = text(&tracked.stdout);
    assert!(
        listing.contains("a.py") && !listing.contains("new.txt"),
        "{listing}"
    );
    assert!(
        text(&tracked.stderr)
            .contains("not shipped, not in HEAD (--include-untracked ships them): new.txt"),
        "{}",
        text(&tracked.stderr)
    );
    let with = world.snapshot_run(&["--include-untracked"], &["ls"]);
    assert_eq!(with.status.code(), Some(0), "{}", text(&with.stderr));
    assert!(text(&with.stdout).contains("new.txt"));
    assert_eq!(world.snapshots().len(), 2, "another tree, another snapshot");
}

#[test]
fn a_secret_looking_untracked_file_is_refused_even_with_include_untracked() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write(".env", "TOKEN=hunter2\n");
    let refused = world.snapshot_run(&["--include-untracked"], &["ls"]);
    assert_eq!(refused.status.code(), Some(3), "{}", text(&refused.stderr));
    assert!(
        text(&refused.stderr).contains("refused"),
        "{}",
        text(&refused.stderr)
    );
    assert!(text(&refused.stderr).contains(".env"));
    assert!(world.scripts().is_empty(), "the remote was not even asked");
    assert!(world.snapshots().is_empty());
    let allowed = world.snapshot_run(
        &["--include-untracked", "--allow-path", ".env"],
        &["cat", ".env"],
    );
    assert_eq!(allowed.status.code(), Some(0), "{}", text(&allowed.stderr));
    assert_eq!(text(&allowed.stdout), "TOKEN=hunter2\n");
}

#[test]
fn the_guards_refuse_before_anything_is_sent() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("a.py", "dirty\n");
    let dirty = world.snapshot_run(&["--require-clean"], &["ls"]);
    assert_eq!(dirty.status.code(), Some(3), "{}", text(&dirty.stderr));
    world.git(&["checkout", "-q", "a.py"]);
    std::os::unix::fs::symlink("/etc/passwd", world.path("repo/link")).unwrap();
    world.git(&["add", "link"]);
    world.git(&["commit", "-q", "-m", "link"]);
    let link = world.snapshot_run(&[], &["ls"]);
    assert_eq!(link.status.code(), Some(3), "{}", text(&link.stderr));
    assert!(text(&link.stderr).contains("link -> /etc/passwd"));
    let big = world.snapshot_run(&["--snapshot-limit", "1"], &["ls"]);
    assert_eq!(big.status.code(), Some(3), "{}", text(&big.stderr));
    assert!(world.scripts().is_empty(), "no refusal reached the remote");
}

#[test]
fn a_refusal_under_json_is_one_document_with_a_reason() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("key.pem", "-----BEGIN-----\n");
    let out = world.rr(&[
        "--json",
        "run",
        "--wait",
        "--mode",
        "snapshot",
        "--include-untracked",
        "--",
        "true",
    ]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["kind"], "snapshot");
    assert_eq!(doc["result"], "refused");
    assert_eq!(doc["reason"], "secret_names");
    assert_eq!(doc["source"], "rr");
}

#[test]
fn the_users_index_branch_stash_and_refs_are_untouched() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("a.py", "stashed\n");
    world.git(&["stash", "push", "-q"]);
    world.write("b.py", "staged\n");
    world.git(&["add", "b.py"]);
    world.write("a.py", "unstaged\n");
    world.write("new.txt", "untracked\n");
    world.git(&["status"]); // lets git refresh its stat cache once, before the "before"
    let before = world.repo_state();
    for flags in [&[][..], &["--include-untracked"]] {
        let run = world.snapshot_run(flags, &["true"]);
        assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    }
    assert_eq!(world.repo_state(), before);
    assert_eq!(
        fs::read_to_string(world.path("repo/a.py")).unwrap(),
        "unstaged\n"
    );
    assert!(!Path::new(&world.path("repo/.git/rr-index")).exists());
}

#[test]
fn checkout_stays_the_default_and_a_snapshot_flag_without_the_mode_is_refused() {
    if !git_available() {
        return;
    }
    let world = World::new();
    let out = world.rr(&["run", "--include-untracked", "--", "true"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("need --mode snapshot"));
    assert!(world.scripts().is_empty());
}

impl World {
    /// A `squeue` on the stand-in remote's `PATH` that lists these `<id> <directory>` lines.
    fn queue(&self, lines: &[String]) {
        let squeue = self.path("bin/squeue");
        let body: String = lines
            .iter()
            .map(|line| format!("echo '{line}'\n"))
            .collect();
        fs::write(&squeue, format!("#!/bin/sh\n{body}exit 0\n")).unwrap();
        fs::set_permissions(&squeue, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Pretend the snapshot was last run in `days` days ago.
    fn last_used(&self, name: &str, days: u64) {
        let used = self
            .path("remote/proj/.rr-snapshots")
            .join(name)
            .join(".rr-used");
        let status = Command::new("touch")
            .arg("-d")
            .arg(format!("{days} days ago"))
            .arg(used)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[test]
fn gc_removes_an_old_snapshot_and_keeps_one_a_job_runs_from() {
    if !git_available() {
        return;
    }
    let world = World::new();
    world.write("a.py", "print('one')\n");
    assert_eq!(world.snapshot_run(&[], &["true"]).status.code(), Some(0));
    world.write("a.py", "print('two')\n");
    assert_eq!(world.snapshot_run(&[], &["true"]).status.code(), Some(0));
    let names = world.snapshots();
    assert_eq!(names.len(), 2);
    for name in &names {
        world.last_used(name, 30);
    }
    let busy = fs::canonicalize(world.path("remote/proj/.rr-snapshots").join(&names[0])).unwrap();
    world.queue(&[format!("41 {}/src", busy.display())]);

    let out = world.rr(&["gc", "--older-than", "7d"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown = text(&out.stdout);
    assert!(shown.contains(&format!("removed {}", names[1])), "{shown}");
    assert!(
        shown.contains(&format!("kept {} (an active job runs from it)", names[0])),
        "{shown}"
    );
    assert_eq!(world.snapshots(), [names[0].clone()]);

    let json = world.rr(&["--json", "gc", "--older-than", "7d"]);
    let document: serde_json::Value = serde_json::from_str(text(&json.stdout).trim()).unwrap();
    assert_eq!(document["kind"], "gc");
    assert_eq!(document["result"], "cleaned");
    assert_eq!(document["kept"][0]["why"], "in_use");
    assert_eq!(document["exit"], 0);
}

#[test]
fn gc_with_a_queue_it_cannot_read_deletes_nothing_and_exits_3() {
    if !git_available() {
        return;
    }
    let world = World::new();
    assert_eq!(world.snapshot_run(&[], &["true"]).status.code(), Some(0));
    let names = world.snapshots();
    world.last_used(&names[0], 30);
    let squeue = world.path("bin/squeue");
    fs::write(&squeue, "#!/bin/sh\necho 'slurm down' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&squeue, fs::Permissions::from_mode(0o755)).unwrap();

    let out = world.rr(&["gc", "--older-than", "1h"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("nothing was deleted"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(world.snapshots(), names);

    let young = world.rr(&["gc", "--older-than", "59m"]);
    assert_eq!(
        young.status.code(),
        Some(2),
        "below the floor is a usage error"
    );
}
