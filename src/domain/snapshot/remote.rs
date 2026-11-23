//! The scripts that put a snapshot on the remote host and run a command in it. Built from source
//! literals and quoted words only, like every remote script (`domain::shell`).
//!
//! The scripts run with the project directory as their working directory, so the snapshot root is
//! the relative `.rr-snapshots`: no path of the user's is joined into shell text.

use super::guard::{Cleanliness, ClearedTree, ShippedSnapshot};
use super::provenance::provenance_json;
use super::tree::TreeId;
use crate::domain::pull::{STALE_EXIT, UV_MISSING_EXIT};
use crate::domain::shell::{Script, lit, number, quote};
use anyhow::{Result, bail};

/// Where snapshots live, inside the project directory.
pub const ROOT: &str = ".rr-snapshots";

/// Exit code of the probe and upload scripts when the root or a snapshot directory is not a private
/// directory of the user's own (a symlink, another owner, a mode that others could write). Distinct
/// from [`STALE_EXIT`] (75), the pull's 76, and the shell's 127.
pub const UNSAFE_EXIT: i32 = 77;

/// What the probe and upload scripts print when the snapshot is there.
const PRESENT: &str = "rr-snapshot present";
const ABSENT: &str = "rr-snapshot absent";
const PUBLISHED: &str = "rr-snapshot published";

/// The owner and mode of a directory must be the user's and `$2`, and it must not be a symlink.
/// `stat -c` is GNU coreutils, as `realpath -m` already is for the pull guard.
pub(super) const SAFE_FN: &str = r#"safe() { [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%u %a' -- "$1")" = "$(id -u) $2" ]; }"#;

/// The mode of the snapshot root (`mkdir -m`), and of a published snapshot: the same minus the
/// write bits that `chmod -R a-w` removes.
pub(super) const ROOT_MODE: &str = "700";
const SNAPSHOT_MODE: &str = "500";

/// The names `rr` keeps in a snapshot. A tree that holds one (a file, or a symlink that would send
/// the write elsewhere in the tree) is refused before anything is written to it.
const RESERVED_CHECK: &str = "for reserved in .rr .rr-used; do\n  if [ -e \"$tmp/$reserved\" ] || [ -L \"$tmp/$reserved\" ]; then echo \"rr: the tree holds $reserved, a name rr keeps for itself\" >&2; exit 1; fi\ndone";

/// The lines both scripts start with: private files, the root created if it is missing and
/// checked, and the names used below.
fn head(id: &TreeId) -> Script {
    Script::lines([
        lit("umask 077"),
        lit("root=").lit(ROOT),
        lit("name=").word(quote(id.as_str())),
        lit(SAFE_FN),
        lit("refuse() { echo \"rr: $1 is not a private directory of yours\" >&2; exit ")
            .word(number(UNSAFE_EXIT))
            .lit("; }"),
        lit("mkdir -p -m 0700 -- \"$root\""),
        lit("safe \"$root\" ")
            .lit(ROOT_MODE)
            .lit(" || refuse \"$PWD/$root\""),
        lit("snap=\"$root/$name\""),
    ])
}

/// Refuse a snapshot directory that exists and is not safe, else say it is present.
fn present_lines() -> Script {
    Script::lines([
        lit("safe \"$snap\" ")
            .lit(SNAPSHOT_MODE)
            .lit(" || refuse \"$PWD/$snap\""),
        lit("echo ").lit(PRESENT),
    ])
}

/// Whether a snapshot is already on the remote host, checked before any byte is sent: a snapshot
/// that is there and private is `present`, one that is not there `absent`, and one that is there
/// but not safe makes the script exit [`UNSAFE_EXIT`]. It takes a [`ClearedTree`], so a tree that
/// was not checked is not even asked about:
///
/// ```compile_fail,E0308
/// use remote_run::domain::snapshot::{BuiltTree, probe_script};
/// fn ask(built: &BuiltTree) {
///     let _ = probe_script(built);
/// }
/// ```
pub fn probe_script(tree: &ClearedTree) -> Script {
    Script::lines([
        head(tree.id()),
        lit("if [ -e \"$snap\" ] || [ -L \"$snap\" ]; then"),
        present_lines(),
        lit("else\n  echo ").lit(ABSENT).lit("\nfi"),
    ])
}

/// Receive the tar of the tree on stdin into a private staging directory, add the provenance, make
/// it read-only and publish it with a rename that cannot replace or nest.
///
/// - stdin goes to descriptor 3 first, so nothing the script runs can read the archive;
/// - the staging directory is `mktemp -d` (random, 0700) inside the root, removed by a trap on
///   EXIT, HUP, INT and TERM (a SIGKILL leaves a `.tmp-*` directory for `rr gc` to sweep);
/// - `mv -T` renames onto the final name; when that fails because another upload won, the answer
///   is `present`, and only if the winner passes the same owner, mode and symlink check.
pub fn upload_script(tree: &ClearedTree) -> Result<Script> {
    let provenance = provenance_json(tree)?;
    Ok(Script::lines([
        head(tree.id()),
        lit("if [ -e \"$snap\" ] || [ -L \"$snap\" ]; then"),
        present_lines(),
        lit("  exit 0\nfi"),
        lit("exec 3<&0 0</dev/null"),
        lit("tmp=$(mktemp -d -- \"$root/.tmp-XXXXXXXX\")"),
        // `|| :` because `set -e` is on in a trap too: a failing chmod must not skip the rm
        lit("trap 'chmod -R u+w -- \"$tmp\" 2>/dev/null || :; rm -rf -- \"$tmp\"' EXIT"),
        lit("trap 'exit 129' HUP\ntrap 'exit 130' INT\ntrap 'exit 143' TERM"),
        lit("tar -x --no-same-owner --no-same-permissions -C \"$tmp\" -f - <&3"),
        lit(RESERVED_CHECK),
        lit("mkdir -- \"$tmp/.rr\""),
        lit("printf '%s\\n' ")
            .word(quote(&provenance))
            .lit(" > \"$tmp/.rr/provenance.json\""),
        lit(": > \"$tmp/.rr-used\""),
        lit("chmod -R a-w -- \"$tmp\""),
        lit("if mv -T -- \"$tmp\" \"$snap\"; then echo ")
            .lit(PUBLISHED)
            .lit("; else\n  safe \"$snap\" ")
            .lit(SNAPSHOT_MODE)
            .lit(" || refuse \"$PWD/$snap\"\n  echo ")
            .lit(PRESENT)
            .lit("\nfi"),
    ]))
}

/// What the probe or the upload said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    Published,
}

/// The last line of a probe's or upload's stdout. Anything else is an error: a script that did not
/// get to its answer must not read as "absent" and start a transfer.
pub fn parse_presence(stdout: &str) -> Result<Presence> {
    match stdout.trim_end() {
        PRESENT => Ok(Presence::Present),
        ABSENT => Ok(Presence::Absent),
        PUBLISHED => Ok(Presence::Published),
        other => bail!("the remote host answered {other:?} where the snapshot state was expected"),
    }
}

/// `cd` into the snapshot, remembering the project directory first (`$proj`, absolute: the shared
/// environment lives there).
fn enter(snapshot: &ShippedSnapshot) -> Script {
    Script::lines([
        lit("proj=$(pwd -P)"),
        lit("cd -- ")
            .lit(ROOT)
            .lit("/")
            .word(quote(snapshot.id().as_str())),
    ])
}

/// Whether the shared environment holds what the snapshot's `uv.lock` needs, without writing to it:
/// `uv sync --frozen --check --inexact --no-install-project` (plain `--check` always reports the
/// editable project's path). Exits [`STALE_EXIT`] when it does not and [`UV_MISSING_EXIT`] when
/// `uv` is not on the PATH; a snapshot without a `uv.lock` has nothing to check. It takes a
/// [`ShippedSnapshot`], so no command runs in a tree that was never uploaded:
///
/// ```compile_fail,E0308
/// use remote_run::domain::snapshot::{ClearedTree, stale_script};
/// fn run_in(cleared: &ClearedTree) {
///     let _ = stale_script(cleared);
/// }
/// ```
pub fn stale_script(snapshot: &ShippedSnapshot) -> Script {
    Script::lines([
        enter(snapshot),
        lit("if [ -f uv.lock ]; then\n  if ! command -v uv >/dev/null 2>&1; then echo 'uv is not on the remote PATH' >&2; exit ")
            .word(number(UV_MISSING_EXIT))
            .lit("; fi\n  UV_PROJECT_ENVIRONMENT=\"$proj/.venv\" uv sync --frozen --check --inexact --no-install-project || exit ")
            .word(number(STALE_EXIT))
            .lit("\nfi"),
    ])
}

fn dirty_word(cleanliness: Cleanliness) -> &'static str {
    match cleanliness {
        Cleanliness::Clean => "false",
        Cleanliness::Dirty => "true",
    }
}

/// What runs before the user's command: the snapshot is the working directory, its `src` comes
/// first on `PYTHONPATH` (it precedes the editable install's `.pth`), the shared environment is
/// `$proj/.venv`, and the snapshot says which tree it is. Touching `.rr-used` tells `rr gc` the
/// snapshot is in use; the file exists from the upload, so no directory write is needed.
pub fn run_prelude(snapshot: &ShippedSnapshot) -> Script {
    Script::lines([
        enter(snapshot),
        lit("snap=$(pwd -P)"),
        lit("touch -- \"$snap/.rr-used\""),
        lit("export PYTHONPATH=\"$snap/src${PYTHONPATH:+:$PYTHONPATH}\""),
        lit("export UV_PROJECT_ENVIRONMENT=\"$proj/.venv\""),
        lit("export RR_SNAPSHOT_ID=")
            .word(quote(snapshot.id().as_str()))
            .lit(" RR_BASE_COMMIT=")
            .word(quote(snapshot.base().as_str()))
            .lit(" RR_DIRTY=")
            .word(quote(dirty_word(snapshot.cleanliness()))),
    ])
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build real files and run a real shell
mod tests {
    use super::*;
    use crate::adapters::OsExec;
    use crate::domain::argv::{Argv, Captured, EnvVar, Payload};
    use crate::domain::place::{Place, RemoteDir, script};
    use crate::ports::Exec;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const BASH: &str = "/bin/bash";

    fn cleared() -> ClearedTree {
        ClearedTree::fake(TREE, 3)
    }

    /// A project directory on a pretend remote host, and programs that stand in for the host's.
    struct Remote {
        project: tempfile::TempDir,
        stubs: tempfile::TempDir,
    }

    impl Remote {
        fn new() -> Self {
            Self {
                project: tempfile::tempdir().unwrap(),
                stubs: tempfile::tempdir().unwrap(),
            }
        }

        fn root(&self) -> PathBuf {
            self.project.path().join(ROOT)
        }

        fn snap(&self) -> PathBuf {
            self.root().join(TREE)
        }

        fn stub(&self, name: &str, text: &str) {
            let path = self.stubs.path().join(name);
            std::fs::write(&path, format!("#!/bin/bash\n{text}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn argv(&self, body: Script, path: &str) -> Argv {
            let place = Place {
                env: None,
                dir: Some(RemoteDir::parse(self.project.path().to_str().unwrap()).unwrap()),
            };
            let text = script(&place, body).to_string();
            let path = format!("{}:{path}", self.stubs.path().display());
            Argv::new(BASH, vec!["-c".into(), text]).with_env(EnvVar::parse("PATH", &path).unwrap())
        }

        fn run_with(&self, body: Script, input: &[u8], path: &str) -> Captured {
            let argv = self.argv(body, path);
            OsExec
                .capture_input(&argv, &Payload::new(input.to_vec()))
                .unwrap()
        }

        fn run(&self, body: Script, input: &[u8]) -> Captured {
            self.run_with(body, input, &std::env::var("PATH").unwrap())
        }

        fn probe(&self) -> Captured {
            self.run(probe_script(&cleared()), b"")
        }

        fn upload(&self, archive: &[u8]) -> Captured {
            self.run(upload_script(&cleared()).unwrap(), archive)
        }

        /// Everything under the root, as names relative to it.
        fn left(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.root())
                .map(|dir| {
                    dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    /// A tar of a small tree made by the real `tar`, with modes a hostile or careless tree could
    /// carry: a world-writable file and a world-writable executable.
    fn archive() -> Vec<u8> {
        let src = tempfile::tempdir().unwrap();
        let at = |name: &str| src.path().join(name);
        std::fs::create_dir_all(at("src/pkg")).unwrap();
        std::fs::write(at("src/pkg/mod.py"), "WHERE = 'snapshot'\n").unwrap();
        std::fs::write(at("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(at("loose.txt"), "x").unwrap();
        std::fs::set_permissions(at("run.sh"), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(at("loose.txt"), std::fs::Permissions::from_mode(0o666)).unwrap();
        let tar = Argv::new(
            "tar",
            ["-cf", "-", "-C", src.path().to_str().unwrap(), "."]
                .map(String::from)
                .into(),
        );
        OsExec.capture_bytes(&tar).unwrap().stdout
    }

    fn walk(path: &Path, found: &mut Vec<PathBuf>) {
        found.push(path.to_path_buf());
        let is_dir = std::fs::symlink_metadata(path).unwrap().is_dir();
        if is_dir {
            for entry in std::fs::read_dir(path).unwrap() {
                walk(&entry.unwrap().path(), found);
            }
        }
    }

    fn said(captured: &Captured) -> &str {
        assert_eq!(captured.code, 0, "{}", captured.stderr);
        captured.stdout.trim()
    }

    #[test]
    fn a_probe_creates_a_private_root_and_says_absent_then_present() {
        let remote = Remote::new();
        assert_eq!(said(&remote.probe()), "rr-snapshot absent");
        assert_eq!(mode(&remote.root()), 0o700);
        assert_eq!(said(&remote.upload(&archive())), "rr-snapshot published");
        assert_eq!(said(&remote.probe()), "rr-snapshot present");
    }

    #[test]
    fn an_upload_publishes_a_read_only_tree_with_its_provenance() {
        let remote = Remote::new();
        assert_eq!(said(&remote.upload(&archive())), "rr-snapshot published");
        assert_eq!(remote.left(), [TREE], "no staging directory is left");
        assert_eq!(mode(&remote.snap()), 0o500);
        let mut all = Vec::new();
        walk(&remote.snap(), &mut all);
        for path in &all {
            assert_eq!(
                mode(path) & 0o277,
                0,
                "{} is writable or open",
                path.display()
            );
        }
        let content = std::fs::read_to_string(remote.snap().join("src/pkg/mod.py")).unwrap();
        assert_eq!(content, "WHERE = 'snapshot'\n");
        assert_eq!(
            mode(&remote.snap().join("run.sh")),
            0o500,
            "kept executable, group and other dropped"
        );
        assert_eq!(mode(&remote.snap().join("loose.txt")), 0o400);
        let provenance =
            std::fs::read_to_string(remote.snap().join(".rr/provenance.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&provenance).unwrap();
        assert_eq!(json["tree"], TREE);
        assert!(remote.snap().join(".rr-used").is_file());
    }

    #[test]
    fn a_second_upload_of_the_same_tree_changes_nothing_and_succeeds() {
        let remote = Remote::new();
        remote.upload(&archive());
        let before = std::fs::read_to_string(remote.snap().join(".rr/provenance.json")).unwrap();
        let again = remote.upload(b"not even an archive");
        assert_eq!(
            said(&again),
            "rr-snapshot present",
            "the archive is never read"
        );
        assert_eq!(remote.left(), [TREE]);
        let after = std::fs::read_to_string(remote.snap().join(".rr/provenance.json")).unwrap();
        assert_eq!(before, after);
    }

    /// An existing directory that is not ours and private is refused by the probe (so no archive
    /// is sent) and by the upload (so a race cannot slip one in).
    #[test]
    fn a_directory_with_the_wrong_mode_is_refused_by_both_scripts() {
        for bad in [0o755, 0o700, 0o770, 0o555] {
            let remote = Remote::new();
            std::fs::create_dir_all(remote.snap()).unwrap();
            std::fs::set_permissions(remote.snap(), std::fs::Permissions::from_mode(bad)).unwrap();
            for run in [remote.probe(), remote.upload(&archive())] {
                assert_eq!(run.code, UNSAFE_EXIT, "mode {bad:o}: {}", run.stderr);
                assert!(
                    run.stderr.contains("not a private directory"),
                    "{}",
                    run.stderr
                );
            }
            std::fs::set_permissions(remote.snap(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
    }

    #[test]
    fn a_symlink_is_refused_whatever_it_points_to() {
        let remote = Remote::new();
        let target = remote.project.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o500)).unwrap();
        for to in [target.as_path(), Path::new("/nonexistent")] {
            std::fs::create_dir_all(remote.root()).unwrap();
            std::fs::set_permissions(remote.root(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let _ = std::fs::remove_file(remote.snap());
            std::os::unix::fs::symlink(to, remote.snap()).unwrap();
            for run in [remote.probe(), remote.upload(&archive())] {
                assert_eq!(run.code, UNSAFE_EXIT, "{}: {}", to.display(), run.stderr);
            }
        }
        let remote = Remote::new();
        let real = remote.project.path().join("real-root");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, remote.root()).unwrap();
        assert_eq!(remote.probe().code, UNSAFE_EXIT, "a symlinked root");
        assert_eq!(remote.upload(&archive()).code, UNSAFE_EXIT);
        assert!(
            std::fs::read_dir(&real).unwrap().next().is_none(),
            "nothing was written through it"
        );
    }

    #[test]
    fn a_directory_of_another_owner_is_refused() {
        let remote = Remote::new();
        remote.upload(&archive());
        remote.stub("id", "echo 4242");
        for body in [probe_script(&cleared()), upload_script(&cleared()).unwrap()] {
            let run = remote.run(body, b"");
            assert_eq!(run.code, UNSAFE_EXIT, "{}", run.stderr);
        }
    }

    /// Another upload publishes the tree while this one extracts. `mv -T` must not move the
    /// staging directory inside the winner (a plain `mv` would), and the winner is trusted only if
    /// it passes the check.
    #[test]
    fn losing_the_race_does_not_nest_and_succeeds_only_for_a_private_winner() {
        for (winner_mode, code) in [(0o500, 0), (0o755, UNSAFE_EXIT)] {
            let remote = Remote::new();
            let make_winner = format!(
                "mkdir -m 700 \"{snap}\" && touch \"{snap}/winner\" && chmod {winner_mode:o} \"{snap}\"",
                snap = remote.snap().display()
            );
            remote.stub("tar", &format!("/usr/bin/tar \"$@\"\n{make_winner}"));
            let run = remote.upload(&archive());
            assert_eq!(run.code, code, "{}", run.stderr);
            if code == 0 {
                assert_eq!(run.stdout.trim(), "rr-snapshot present");
            }
            let mut inside = Vec::new();
            walk(&remote.snap(), &mut inside);
            assert_eq!(inside.len(), 2, "the winner and its file only: {inside:?}");
            assert_eq!(remote.left(), [TREE], "the staging directory is gone");
            std::fs::set_permissions(remote.snap(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
    }

    #[test]
    fn a_failed_extraction_publishes_nothing_and_leaves_no_staging_directory() {
        let remote = Remote::new();
        let run = remote.upload(b"this is not a tar archive");
        assert_ne!(run.code, 0);
        assert!(run.stdout.is_empty());
        assert!(remote.left().is_empty(), "{:?}", remote.left());
    }

    #[test]
    fn a_tree_that_holds_a_name_rr_keeps_is_refused() {
        for (name, kind) in [(".rr-used", "file"), (".rr", "dir"), (".rr-used", "link")] {
            let remote = Remote::new();
            let src = tempfile::tempdir().unwrap();
            match kind {
                "file" => std::fs::write(src.path().join(name), "x").unwrap(),
                "dir" => std::fs::create_dir(src.path().join(name)).unwrap(),
                _ => std::os::unix::fs::symlink("a.py", src.path().join(name)).unwrap(),
            }
            let tar = Argv::new(
                "tar",
                ["-cf", "-", "-C", src.path().to_str().unwrap(), "."]
                    .map(String::from)
                    .into(),
            );
            let bytes = OsExec.capture_bytes(&tar).unwrap().stdout;
            let run = remote.upload(&bytes);
            assert_eq!(run.code, 1, "{name} {kind}: {}", run.stderr);
            assert!(run.stderr.contains("keeps for itself"), "{}", run.stderr);
            assert!(
                remote.left().is_empty(),
                "{name} {kind}: {:?}",
                remote.left()
            );
        }
    }

    /// A process that is not given its whole input yet: the upload is stuck in `tar`.
    fn stuck_upload(remote: &Remote) -> std::process::Child {
        use std::io::Write;
        let argv = remote.argv(
            upload_script(&cleared()).unwrap(),
            &std::env::var("PATH").unwrap(),
        );
        let mut command = std::process::Command::new(argv.program());
        command
            .args(argv.args())
            .stdin(std::process::Stdio::piped());
        for var in argv.envs() {
            command.env(var.name(), var.value());
        }
        let mut child = command.spawn().unwrap();
        let head = &archive()[..1024];
        child.stdin.as_mut().unwrap().write_all(head).unwrap();
        child.stdin.as_mut().unwrap().flush().unwrap();
        let staged = (0..200).any(|_| {
            let found = remote.left().iter().any(|name| name.starts_with(".tmp-"));
            if !found {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            found
        });
        if !staged {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the upload never made its staging directory");
        }
        child
    }

    #[test]
    fn a_killed_upload_leaves_only_a_staging_directory_never_the_snapshot() {
        let remote = Remote::new();
        let mut child = stuck_upload(&remote);
        child.kill().unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let left = remote.left();
        assert!(
            !left.is_empty() && left.iter().all(|name| name.starts_with(".tmp-")),
            "{left:?}"
        );
        assert!(!remote.snap().exists());
    }

    #[test]
    fn an_interrupted_upload_cleans_up_after_itself() {
        for signal in ["TERM", "INT", "HUP"] {
            let remote = Remote::new();
            let mut child = stuck_upload(&remote);
            let kill = Argv::new("kill", vec![format!("-{signal}"), child.id().to_string()]);
            assert_eq!(
                OsExec
                    .capture(&kill, crate::domain::argv::Stdin::Null)
                    .unwrap()
                    .code,
                0
            );
            drop(child.stdin.take());
            let status = child.wait().unwrap();
            assert!(!status.success(), "{signal}");
            assert!(remote.left().is_empty(), "{signal}: {:?}", remote.left());
        }
    }

    fn shipped() -> crate::domain::snapshot::ShippedSnapshot {
        cleared().shipped()
    }

    #[test]
    fn running_in_a_snapshot_sets_the_environment_and_marks_it_used() {
        let remote = Remote::new();
        remote.upload(&archive());
        let used = remote.snap().join(".rr-used");
        std::fs::set_permissions(&used, std::fs::Permissions::from_mode(0o600)).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&used)
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::set_permissions(&used, std::fs::Permissions::from_mode(0o400)).unwrap();
        let body = run_prelude(&shipped())
            .lit("\nprintf '%s\\n' \"$PWD\" \"$PYTHONPATH\" \"$UV_PROJECT_ENVIRONMENT\" \"$RR_SNAPSHOT_ID\" \"$RR_BASE_COMMIT\" \"$RR_DIRTY\"");
        let argv = remote.argv(body, &std::env::var("PATH").unwrap());
        let argv = argv.with_env(EnvVar::parse("PYTHONPATH", "/already").unwrap());
        let run = OsExec
            .capture(&argv, crate::domain::argv::Stdin::Null)
            .unwrap();
        let lines: Vec<&str> = run.stdout.lines().collect();
        let project = std::fs::canonicalize(remote.project.path()).unwrap();
        let snap = project.join(ROOT).join(TREE);
        assert_eq!(lines[0], snap.to_str().unwrap(), "{}", run.stderr);
        assert_eq!(lines[1], format!("{}/src:/already", snap.display()));
        assert_eq!(lines[2], format!("{}/.venv", project.display()));
        assert_eq!(
            &lines[3..],
            [TREE, "cccccccccccccccccccccccccccccccccccccccc", "false"]
        );
        let touched = std::fs::metadata(&used).unwrap().modified().unwrap();
        assert!(touched > old, "the snapshot is marked as used");
    }

    #[test]
    fn the_stale_check_asks_uv_in_the_snapshot_and_maps_its_answer() {
        let remote = Remote::new();
        remote.upload(&archive());
        let env = std::env::var("PATH").unwrap();
        let no_lock = remote.run(stale_script(&shipped()), b"");
        assert_eq!(
            no_lock.code, 0,
            "a snapshot without uv.lock has nothing to check"
        );
        let lock = remote.snap().join("uv.lock");
        std::fs::set_permissions(remote.snap(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&lock, "version = 1\n").unwrap();
        remote.stub(
            "uv",
            "printf '%s|' \"$@\" > \"$UV_LOG\"; exit ${UV_EXIT:-0}",
        );
        let log = remote.project.path().join("uv.log");
        let ask = |exit: &str| {
            let argv = remote
                .argv(stale_script(&shipped()), &env)
                .with_env(EnvVar::parse("UV_LOG", log.to_str().unwrap()).unwrap())
                .with_env(EnvVar::parse("UV_EXIT", exit).unwrap());
            OsExec
                .capture(&argv, crate::domain::argv::Stdin::Null)
                .unwrap()
                .code
        };
        assert_eq!(ask("0"), 0);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "sync|--frozen|--check|--inexact|--no-install-project|"
        );
        assert_eq!(ask("1"), STALE_EXIT);
        std::fs::remove_file(remote.stubs.path().join("uv")).unwrap();
        let bare = remote.run_with(stale_script(&shipped()), b"", "/nonexistent");
        assert_eq!(bare.code, UV_MISSING_EXIT, "{}", bare.stderr);
    }

    #[test]
    fn presence_is_read_from_the_last_line_and_anything_else_is_an_error() {
        assert_eq!(
            parse_presence("rr-snapshot present\n").unwrap(),
            Presence::Present
        );
        assert_eq!(
            parse_presence("rr-snapshot absent").unwrap(),
            Presence::Absent
        );
        assert_eq!(
            parse_presence("rr-snapshot published\n").unwrap(),
            Presence::Published
        );
        for bad in [
            "",
            "present",
            "rr-snapshot present\nrr-snapshot absent\n",
            "rr-snapshot  absent",
        ] {
            assert!(parse_presence(bad).is_err(), "{bad:?}");
        }
    }
}
