//! The one remote script behind `rr gc`, built in stages that only the right order can reach:
//!
//! 1. [`Listed`]: the root is checked, the queue read once, and every snapshot sorted into kept or
//!    chosen for removal;
//! 2. [`Trashed`]: every chosen entry renamed to `.trash-<name>`, so a job that starts after this
//!    cannot enter it through its old name;
//! 3. [`Rechecked`]: the queue read again, and every entry a job is in put back;
//! 4. [`Rechecked::remove`]: only what is still set aside and unused is deleted.
//!
//! The deleting text exists only inside [`Rechecked::remove`], and a [`Rechecked`] only comes from
//! [`Trashed::recheck`]:
//!
//! ```compile_fail,E0599
//! use remote_run::domain::snapshot::{Age, Keep, Listed};
//! let age = Age::parse("2h").unwrap();
//! let _ = Listed::new(age, Keep::NONE).remove();
//! ```
//!
//! ```compile_fail,E0599
//! use remote_run::domain::snapshot::{Age, Keep, Listed};
//! let age = Age::parse("2h").unwrap();
//! let _ = Listed::new(age, Keep::NONE).trash().remove();
//! ```
//!
//! ```compile_fail,E0423
//! use remote_run::domain::shell::Script;
//! use remote_run::domain::snapshot::Rechecked;
//! let _ = Rechecked(Script::new()).remove();
//! ```
//!
//! Built from source literals and numbers only, like every remote script: no text of the user's
//! reaches it. The report it prints is read by [`parse_report`](super::parse_report).

use super::gc::{AGE_FLOOR_SECONDS, Age, GC_REFUSED_EXIT, Keep};
use super::remote::{ROOT, ROOT_MODE, SAFE_FN};
use crate::domain::pull::job_loop;
use crate::domain::shell::{Script, lit, number};

/// Length of a snapshot's directory name: a full tree id.
const ID_LENGTH: i64 = 40;

/// Prints one report line: `rr-gc`, then its fields, tab separated.
const SAY: &str = r#"say() { printf 'rr-gc'; printf '\t%s' "$@"; printf '\n'; }"#;

/// A refusal ends the script with [`GC_REFUSED_EXIT`] before anything is deleted.
const REFUSE: &str = r#"refuse() { say refused "$1"; exit "#;

/// A snapshot's name: exactly the 40 lowercase hex digits of a tree id (`LC_ALL=C` keeps the
/// character class to ASCII).
const IS_ID: &str =
    r#"is_id() { case "$1" in *[!0123456789abcdef]*) return 1 ;; esac; [ "${#1}" -eq "#;

/// Leftovers are only read when their whole name is plain characters.
const PLAIN_NAME: &str = r#"plain_name() { case "$1" in *[!A-Za-z0-9._-]*) return 1 ;; esac; }"#;

/// `$1` is a real directory (not a symlink, never followed) of the user's own.
const MINE: &str =
    r#"mine() { [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c %u -- "$1")" = "$uid" ]; }"#;

/// When a snapshot was last run in: `.rr-used` (a regular file, never a symlink), else the
/// directory.
const USED_OF: &str = r#"used_of() { if [ -f "$1/.rr-used" ] && [ ! -L "$1/.rr-used" ]; then stat -c %Y -- "$1/.rr-used"; else stat -c %Y -- "$1"; fi; }"#;

/// The name an entry is set aside under; a leftover that is already `.trash-…` keeps its name.
const TRASH_OF: &str = r#"trash_of() { case "$1" in .trash-*) printf '%s' "$1" ;; *) printf '.trash-%s' "$1" ;; esac; }"#;

/// The snapshot name a job's directory would carry for an entry: a snapshot set aside by a run
/// that never finished (`.trash-<id>`) is still the directory its jobs were submitted from.
const HOME_OF: &str = r#"home_of() {
  case "$1" in
    .trash-*) if is_id "${1#.trash-}"; then printf '%s' "${1#.trash-}"; return 0; fi ;;
  esac
  printf '%s' "$1"
}"#;

/// Puts an entry back under its own name; nothing to do for one that was never renamed.
const RESTORE: &str = r#"restore() { t=$(trash_of "$1"); [ "$t" = "$1" ] || mv -T -- "$rootreal/$t" "$rootreal/$1"; }"#;

/// Whether an active job runs from the entry or from inside it. The test is the shell form of
/// `inside` (equal, or below with a `/` between), applied to paths `realpath -m` already
/// normalised; a heredoc and not a pipe, so no `SIGPIPE` can turn "found" into "not found".
const IN_USE: &str = r#"in_use() {
  dir="$rootreal/$(home_of "$1")"
  while IFS= read -r workdir; do
    case "$workdir" in "$dir"|"$dir"/*) return 0 ;; esac
  done <<EOF
$queue
EOF
  return 1
}"#;

/// Every entry of `$1`'s list, one name per line.
fn each_name(body: Script, list: &'static str) -> Script {
    lit("while IFS= read -r name; do\n  [ -n \"$name\" ] || continue\n")
        .then(body)
        .lit("\ndone <<EOF\n")
        .lit(list)
        .lit("\nEOF")
}

/// Active jobs' directories, one `realpath -m` result per line, in `$queue`. A directory that
/// cannot be placed, a line that cannot be read and a failing `squeue` or `realpath` all make this
/// fail (`exit 1` inside the loop's subshell, `pipefail` for the rest), and the script then
/// refuses: unknown is "in use". It is the loop the pull guard uses.
fn read_queue() -> Script {
    let resolved = lit(r#"printf '%s\n' "$resolved""#);
    lit("read_queue() { queue=$(")
        .then(job_loop(resolved, lit("exit 1"), lit("exit 1")))
        .lit(") || return 1; }")
}

/// The functions, the root check and the first read of the queue.
fn head(age: Age) -> Script {
    Script::lines([
        lit("export LC_ALL=C"),
        lit("root=").lit(ROOT),
        lit(SAFE_FN),
        lit(SAY),
        lit(REFUSE).word(number(GC_REFUSED_EXIT)).lit("; }"),
        // nothing was ever uploaded: nothing to clean, and nothing is created
        lit("if [ ! -e \"$root\" ] && [ ! -L \"$root\" ]; then say done; exit 0; fi"),
        lit("safe \"$root\" ").lit(ROOT_MODE).lit(" || refuse root"),
        lit("rootreal=$(realpath -- \"$root\") || refuse root"),
        lit("[ -n \"$rootreal\" ] && [ \"$rootreal\" != / ] || refuse root"),
        lit("[ -n \"${HOME:-}\" ] || refuse root"),
        lit("homereal=$(realpath -- \"$HOME\") || refuse root"),
        lit("[ \"$rootreal\" != \"$homereal\" ] || refuse root"),
        lit("uid=$(id -u)"),
        lit(IS_ID).word(number(ID_LENGTH)).lit(" ]; }"),
        lit(PLAIN_NAME),
        lit(MINE),
        lit(USED_OF),
        lit(TRASH_OF),
        lit(HOME_OF),
        lit(RESTORE),
        read_queue(),
        lit(IN_USE),
        lit("now=$(date +%s)"),
        lit("cutoff=$((now - ")
            .word(number(age.seconds()))
            .lit("))"),
        lit("floor=$((now - ")
            .word(number(AGE_FLOOR_SECONDS))
            .lit("))"),
        lit("queue="),
        lit("read_queue || refuse queue"),
    ])
}

/// Sort every snapshot (newest first by last use) into kept and chosen, and every old leftover into
/// chosen. `$victims` holds the chosen names, one per line.
fn list(keep: Keep) -> Script {
    let snapshots = Script::lines([
        lit("list="),
        lit("for path in \"$rootreal\"/*; do"),
        lit("  name=${path##*/}"),
        lit("  is_id \"$name\" || continue"),
        lit(
            "  if mine \"$path\"; then\n    used=$(used_of \"$path\")\n    list=\"$list$used $name\n\"\n  else say kept not-yours \"$name\"; fi",
        ),
        lit("done"),
        lit("sorted=$(printf '%s' \"$list\" | sort -rn)"),
        lit("victims="),
        lit("index=0"),
        lit("while IFS=' ' read -r used name; do"),
        lit("  [ -n \"$name\" ] || continue"),
        lit("  index=$((index + 1))"),
        lit("  if [ \"$index\" -le ")
            .word(number(keep.count()))
            .lit(" ]; then say kept keep \"$name\""),
        lit("  elif [ \"$used\" -ge \"$cutoff\" ]; then say kept young \"$name\""),
        lit("  elif in_use \"$name\"; then say kept in-use \"$name\""),
        lit("  else victims=\"$victims$name\n\"; fi"),
        lit("done <<EOF\n$sorted\nEOF"),
    ]);
    let leftovers = Script::lines([
        lit("for path in \"$rootreal\"/.tmp-?* \"$rootreal\"/.trash-?*; do"),
        lit("  name=${path##*/}"),
        lit("  [ -e \"$path\" ] || [ -L \"$path\" ] || continue"),
        lit("  plain_name \"$name\" || continue"),
        lit("  mine \"$path\" || { say kept not-yours \"$name\"; continue; }"),
        lit("  touched=$(stat -c %Y -- \"$path\")"),
        lit("  if [ \"$touched\" -ge \"$floor\" ]; then say kept young \"$name\""),
        lit("  elif in_use \"$name\"; then say kept in-use \"$name\""),
        lit("  else victims=\"$victims$name\n\"; fi"),
        lit("done"),
    ]);
    snapshots.lit("\n").then(leftovers)
}

/// The queue was read and the snapshots chosen; nothing has been moved.
#[derive(Debug)]
pub struct Listed(Script);

impl Listed {
    pub fn new(age: Age, keep: Keep) -> Self {
        Self(Script::lines([head(age), list(keep)]))
    }

    /// Set every chosen entry aside as `.trash-<name>`. One that cannot be renamed stays where it
    /// is and is reported.
    pub fn trash(self) -> Trashed {
        let rename = lit(
            "  trash=$(trash_of \"$name\")\n  if [ \"$trash\" = \"$name\" ] || mv -T -- \"$rootreal/$name\" \"$rootreal/$trash\"; then\n    moved=\"$moved$name\n\"\n  else say kept error \"$name\"; fi",
        );
        Trashed(Script::lines([
            self.0,
            lit("moved="),
            each_name(rename, "$victims"),
        ]))
    }
}

/// Every chosen entry is set aside; none has been deleted.
#[derive(Debug)]
pub struct Trashed(Script);

impl Trashed {
    /// Read the queue again. A job found inside an entry puts it back and keeps it; a queue that
    /// cannot be read puts everything back and refuses. What is left in `$clear` is set aside and
    /// unused.
    pub fn recheck(self) -> Rechecked {
        let put_back = each_name(lit("  restore \"$name\" || :"), "$moved");
        let decide = lit(
            "  if in_use \"$name\"; then\n    if restore \"$name\"; then say kept in-use \"$name\"; else say kept error \"$name\"; fi\n  else clear=\"$clear$name\n\"; fi",
        );
        Rechecked(Script::lines([
            self.0,
            lit("clear="),
            lit("if ! read_queue; then"),
            put_back,
            lit("  refuse queue\nfi"),
            each_name(decide, "$moved"),
        ]))
    }
}

/// The queue was read again after every entry was set aside, and what is in `$clear` is unused.
/// The private field is why only [`Trashed::recheck`] makes one.
#[derive(Debug)]
pub struct Rechecked(Script);

impl Rechecked {
    /// Delete what the recheck left: the read-only tree made writable, then `rm` that stays on its
    /// file system and follows no symlink. A directory that is no longer a plain one is not
    /// touched.
    pub fn remove(self) -> Script {
        let delete = lit(
            "  path=\"$rootreal/$(trash_of \"$name\")\"\n  if [ ! -L \"$path\" ] && [ -d \"$path\" ] && chmod -R u+w -- \"$path\" && rm -rf --one-file-system -- \"$path\"; then\n    say removed \"$name\"\n  else say failed \"$name\"; fi",
        );
        Script::lines([self.0, each_name(delete, "$clear"), lit("say done")])
    }
}

/// The whole clean-up, to run in the project directory.
pub fn gc_script(age: Age, keep: Keep) -> Script {
    Listed::new(age, keep).trash().recheck().remove()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(age: &str, keep: u32) -> String {
        gc_script(
            Age::parse(age).unwrap(),
            Keep::parse(&keep.to_string()).unwrap(),
        )
        .to_string()
    }

    #[test]
    fn the_numbers_are_the_only_runtime_text() {
        let text = script("90m", 3);
        assert!(text.contains("cutoff=$((now - 5400))\n"), "{text}");
        assert!(text.contains("floor=$((now - 3600))\n"), "{text}");
        assert!(text.contains("[ \"$index\" -le 3 ]"), "{text}");
        assert!(text.contains(&format!("-eq {ID_LENGTH} ]")), "{text}");
        assert!(
            text.contains(&format!("exit {GC_REFUSED_EXIT}; }}")),
            "{text}"
        );
        let other = script("2d", 0);
        assert_eq!(
            text.replace("5400", "172800").replace("-le 3 ", "-le 0 "),
            other
        );
    }

    #[test]
    fn the_queue_is_the_pull_guards_loop_and_it_is_read_twice() {
        let text = script("2h", 0);
        let squeue = "squeue --me -h -o '%i %Z'";
        assert_eq!(text.matches(squeue).count(), 1, "one function reads it");
        assert!(text.contains("realpath -m -- \"${line#* }\""), "{text}");
        assert_eq!(
            text.matches("read_queue").count(),
            3,
            "defined, first read, recheck"
        );
    }

    /// The order is the safety argument: check, set aside, check again, delete.
    #[test]
    fn the_stages_run_in_the_order_that_makes_deleting_safe() {
        let text = script("2h", 0);
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle}\n{text}"))
        };
        let first_check = at("read_queue || refuse queue");
        let chosen = at("victims=\"$victims$name");
        let rename = at("mv -T -- \"$rootreal/$name\"");
        let second_check = at("if ! read_queue; then");
        let put_back = at("restore \"$name\" || :");
        let rm = at("rm -rf --one-file-system");
        let chmod = at("chmod -R u+w");
        assert!(first_check < chosen && chosen < rename && rename < second_check);
        assert!(second_check < put_back && put_back < chmod && chmod < rm);
        assert_eq!(text.matches("rm -rf").count(), 1, "one place deletes");
        assert_eq!(text.matches("rm ").count(), 1);
        assert!(text.ends_with("say done"), "{text}");
        assert!(
            text.starts_with("export LC_ALL=C\nroot=.rr-snapshots\n"),
            "{text}"
        );
    }

    #[test]
    fn nothing_is_followed_or_created() {
        let text = script("2h", 0);
        assert!(!text.contains("mkdir") && !text.contains("mktemp") && !text.contains("rm -r "));
        assert!(
            !text.contains("flock"),
            "flock is unreliable on shared file systems"
        );
        assert!(text.contains("realpath -- \"$root\""));
        assert!(text.contains("[ ! -L \"$path\" ] && [ -d \"$path\" ] && chmod -R u+w --"));
    }
}

/// A change to the stand-in host before a run.
#[cfg(test)]
type SetUp<'a> = Box<dyn Fn() + 'a>;

/// The clean-up run by a real `bash` in a temporary project directory, with stub `squeue` and
/// `realpath` programs first on `PATH`. Nothing here reads the real `HOME` or the network.
#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build real files and run a real shell
mod shell {
    use super::super::{GC_REFUSED_EXIT, GcOutcome, GcRefusal, ROOT, RootEntry, Why, parse_report};
    use super::*;
    use crate::adapters::OsExec;
    use crate::domain::argv::{Argv, Captured, EnvVar};
    use crate::domain::place::{Place, RemoteDir, script};
    use crate::ports::{Exec, Stdin};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddd";

    const HOUR: u64 = 3600;
    const DAY: u64 = 24 * HOUR;

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A project directory on a pretend remote host, the programs that stand in for the host's, and a
    /// home directory of its own.
    struct Host {
        project: tempfile::TempDir,
        stubs: tempfile::TempDir,
        home: tempfile::TempDir,
    }

    impl Host {
        fn new() -> Self {
            let host = Self {
                project: tempfile::tempdir().unwrap(),
                stubs: tempfile::tempdir().unwrap(),
                home: tempfile::tempdir().unwrap(),
            };
            host.stub("squeue", "exit 0");
            host
        }

        /// The project directory as `realpath` writes it, so a stub `squeue` can name paths inside it.
        fn project(&self) -> PathBuf {
            self.project.path().canonicalize().unwrap()
        }

        fn root(&self) -> PathBuf {
            self.project().join(ROOT)
        }

        fn snap(&self, name: &str) -> PathBuf {
            self.root().join(name)
        }

        fn stub(&self, name: &str, text: &str) {
            let path = self.stubs.path().join(name);
            std::fs::write(&path, format!("#!/bin/bash\n{text}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// `squeue` that lists these `<id> <directory>` lines.
        fn queue(&self, lines: &[String]) {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(self.stubs.path().join("queue.txt"), text).unwrap();
            let file = self.stubs.path().join("queue.txt");
            self.stub("squeue", &format!("cat '{}'", file.display()));
        }

        /// A `realpath` that fails for any argument that holds `needle`, and is the real one otherwise.
        fn realpath_fails_for(&self, needle: &str) {
            let real = Command::new("sh")
                .args(["-c", "command -v realpath"])
                .output()
                .unwrap()
                .stdout;
            let real = String::from_utf8(real).unwrap();
            self.stub(
            "realpath",
            &format!(
                "for a in \"$@\"; do case \"$a\" in *{needle}*) exit 1 ;; esac; done\nexec {} \"$@\"",
                real.trim()
            ),
        );
        }

        fn make_root(&self) {
            std::fs::create_dir_all(self.root()).unwrap();
            std::fs::set_permissions(self.root(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        /// A published-looking snapshot, last run in `used_ago` seconds ago: read-only all through,
        /// with the nested directory and the files a tree has.
        fn snapshot(&self, name: &str, used_ago: u64) {
            self.make_root();
            let dir = self.snap(name);
            std::fs::create_dir_all(dir.join("src/pkg")).unwrap();
            std::fs::write(dir.join("src/pkg/mod.py"), "x = 1\n").unwrap();
            std::fs::write(dir.join(".rr-used"), "").unwrap();
            self.age(&dir.join(".rr-used"), used_ago);
            self.freeze(&dir, used_ago);
        }

        fn age(&self, path: &Path, seconds_ago: u64) {
            let stamp = format!("@{}", now() - seconds_ago);
            let status = Command::new("touch")
                .args(["-h", "-d", &stamp])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
        }

        /// `chmod -R a-w`, then the directory's own time (what a leftover is judged by).
        fn freeze(&self, dir: &Path, dir_ago: u64) {
            let status = Command::new("chmod")
                .args(["-R", "a-w"])
                .arg(dir)
                .status()
                .unwrap();
            assert!(status.success());
            self.age(dir, dir_ago);
        }

        /// Run the clean-up for `age` and `keep` and return what it printed.
        fn gc(&self, age: &str, keep: u32) -> Captured {
            self.run_with_home(age, keep, self.home.path().to_str().unwrap())
        }

        fn run_with_home(&self, age: &str, keep: u32, home: &str) -> Captured {
            let place = Place {
                env: None,
                dir: Some(RemoteDir::parse(self.project().to_str().unwrap()).unwrap()),
            };
            let age = Age::parse(age).unwrap();
            let body = script(
                &place,
                gc_script(age, Keep::parse(&keep.to_string()).unwrap()),
            );
            let path = format!(
                "{}:{}",
                self.stubs.path().display(),
                std::env::var("PATH").unwrap()
            );
            let argv = Argv::new("/bin/bash", vec!["-c".into(), body.to_string()])
                .with_env(EnvVar::parse("PATH", &path).unwrap())
                .with_env(EnvVar::parse("HOME", home).unwrap());
            OsExec.capture(&argv, Stdin::Null).unwrap()
        }

        /// The names under the root, sorted.
        fn left(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.root())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    /// What a finished clean-up reported, as names.
    struct Done {
        removed: Vec<String>,
        kept: Vec<(String, Why)>,
        failed: Vec<String>,
    }

    fn cleaned(captured: &Captured) -> Done {
        assert_eq!(captured.code, 0, "{}", captured.stderr);
        let GcOutcome::Cleaned(report) = parse_report(&captured.stdout).unwrap() else {
            panic!("a refusal: {}", captured.stdout);
        };
        let names = |entries: &[RootEntry]| {
            let mut names: Vec<String> = entries.iter().map(|e| e.name().to_string()).collect();
            names.sort();
            names
        };
        let mut kept: Vec<(String, Why)> = report
            .kept
            .iter()
            .map(|k| (k.entry.name().to_string(), k.why))
            .collect();
        kept.sort_by(|a, b| a.0.cmp(&b.0));
        Done {
            removed: names(&report.removed),
            kept,
            failed: names(&report.failed),
        }
    }

    fn refused(captured: &Captured) -> GcRefusal {
        assert_eq!(
            captured.code, GC_REFUSED_EXIT,
            "{:?} {}",
            captured.stdout, captured.stderr
        );
        let GcOutcome::Refused(why) = parse_report(&captured.stdout).unwrap() else {
            panic!("a clean-up: {}", captured.stdout);
        };
        why
    }

    #[test]
    fn an_old_unused_snapshot_is_removed_read_only_files_and_all_and_a_young_one_is_not_touched() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        host.snapshot(B, 600);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [A]);
        assert_eq!(done.kept, [(B.to_string(), Why::Young)]);
        assert!(done.failed.is_empty());
        assert_eq!(host.left(), [B], "no .trash-* is left behind");
        assert!(
            host.snap(B).join("src/pkg/mod.py").is_file(),
            "the young one is whole"
        );
    }

    #[test]
    fn the_age_is_judged_by_the_last_run_not_by_when_the_directory_was_made() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        // run again an hour ago: `.rr-used` is new, the directory is old
        let used = host.snap(A).join(".rr-used");
        host.age(&used, 600);
        assert_eq!(
            cleaned(&host.gc("2h", 0)).kept,
            [(A.to_string(), Why::Young)]
        );
        // no `.rr-used` at all: the directory's own time decides
        host.snapshot(B, 3 * DAY);
        let gone = host.snap(B).join(".rr-used");
        std::fs::set_permissions(host.snap(B), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(gone).unwrap();
        host.freeze(&host.snap(B), 3 * DAY);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [B]);
        assert_eq!(host.left(), [A]);
    }

    #[test]
    fn a_snapshot_exactly_as_old_as_the_age_is_not_older_than_it() {
        let host = Host::new();
        host.snapshot(A, 2 * HOUR + 60);
        host.snapshot(B, 2 * HOUR - 60);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [A]);
        assert_eq!(done.kept, [(B.to_string(), Why::Young)]);
    }

    #[test]
    fn the_newest_n_stay_whatever_their_age() {
        let host = Host::new();
        host.snapshot(A, 10 * DAY);
        host.snapshot(B, 20 * DAY);
        host.snapshot(C, 30 * DAY);
        host.snapshot(D, 40 * DAY);
        let done = cleaned(&host.gc("2h", 2));
        assert_eq!(done.removed, [C, D]);
        assert_eq!(
            done.kept,
            [(A.to_string(), Why::Newest), (B.to_string(), Why::Newest)]
        );
        assert_eq!(host.left(), [A, B]);
        let all = cleaned(&host.gc("2h", 5));
        assert!(
            all.removed.is_empty(),
            "--keep larger than the count keeps all"
        );
    }

    /// Where a job's directory is, relative to the snapshot's own path, and whether that is in it.
    const JOB_DIRECTORIES: [(&str, bool); 7] = [
        ("", true),
        ("/src", true),
        ("/src/pkg/deeper/missing", true),
        ("x", false),
        ("-old", false),
        ("/../other", false),
        ("/..", false),
    ];

    #[test]
    fn a_job_in_a_snapshot_keeps_it_and_a_job_next_to_it_does_not() {
        for (suffix, kept) in JOB_DIRECTORIES {
            let host = Host::new();
            host.snapshot(A, 3 * DAY);
            let workdir = format!("{}{suffix}", host.snap(A).display());
            host.queue(&[format!("77 {workdir}")]);
            let done = cleaned(&host.gc("2h", 0));
            let expected_kept = if kept {
                vec![(A.to_string(), Why::InUse)]
            } else {
                vec![]
            };
            assert_eq!(done.kept, expected_kept, "{workdir}");
            assert_eq!(done.removed.len(), usize::from(!kept), "{workdir}");
            assert_eq!(host.snap(A).exists(), kept, "{workdir}");
        }
    }

    #[test]
    fn a_job_reached_through_a_symlink_or_dots_still_counts() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        let link = host.project().join("shortcut");
        std::os::unix::fs::symlink(host.snap(A), &link).unwrap();
        host.queue(&[format!("5 {}/src/../src", link.display())]);
        assert_eq!(
            cleaned(&host.gc("2h", 0)).kept,
            [(A.to_string(), Why::InUse)]
        );
    }

    #[test]
    fn a_queue_that_cannot_be_read_deletes_nothing() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        let snap = host.snap(A).display().to_string();
        let cases: Vec<(&str, SetUp)> = vec![
            (
                "squeue fails",
                Box::new(|| host.stub("squeue", "echo 'slurm down' >&2; exit 1")),
            ),
            (
                "squeue fails after listing a job",
                Box::new(|| host.stub("squeue", "echo '1 /elsewhere'; exit 1")),
            ),
            (
                "a line with no directory",
                Box::new(|| host.queue(&["12".into()])),
            ),
            ("an empty line", Box::new(|| host.queue(&["".into()]))),
            (
                "a directory realpath cannot place",
                Box::new(|| {
                    host.queue(&["3 /home/u/unplaceable".into()]);
                    host.realpath_fails_for("unplaceable");
                }),
            ),
        ];
        for (what, set_up) in cases {
            set_up();
            let out = host.gc("2h", 0);
            assert_eq!(refused(&out), GcRefusal::QueueUnreadable, "{what}");
            assert_eq!(host.left(), [A], "{what}");
            assert!(
                host.snap(A).join("src/pkg/mod.py").is_file(),
                "{what} {snap}"
            );
            host.stub("squeue", "exit 0");
            std::fs::remove_file(host.stubs.path().join("realpath")).ok();
        }
    }

    /// `squeue` that answers `first` the first time it runs and `then` after that.
    fn two_answers(host: &Host, first: &str, then: &str) {
        let counter = host.stubs.path().join("count");
        host.stub(
        "squeue",
        &format!(
            "n=$(cat '{c}' 2>/dev/null || echo 0); echo $((n+1)) > '{c}'\nif [ \"$n\" = 0 ]; then {first}; else {then}; fi",
            c = counter.display()
        ),
    );
    }

    #[test]
    fn a_job_that_appears_after_the_first_check_puts_the_snapshot_back() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        host.snapshot(B, 3 * DAY);
        let inside = format!("echo '9 {}/src'", host.snap(A).display());
        two_answers(&host, "exit 0", &inside);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [B]);
        assert_eq!(done.kept, [(A.to_string(), Why::InUse)]);
        assert_eq!(
            host.left(),
            [A],
            "put back under its own name, nothing in the trash"
        );
        assert!(host.snap(A).join("src/pkg/mod.py").is_file());
    }

    #[test]
    fn an_entry_is_set_aside_before_the_second_check_so_a_new_job_cannot_enter_it() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        let seen = host.stubs.path().join("seen");
        let second = format!("ls -A '{}' > '{}'", host.root().display(), seen.display());
        two_answers(&host, "exit 0", &second);
        cleaned(&host.gc("2h", 0));
        let listing = std::fs::read_to_string(seen).unwrap();
        assert_eq!(listing.trim(), format!(".trash-{A}"), "{listing}");
    }

    #[test]
    fn a_queue_that_fails_the_second_time_puts_everything_back_and_deletes_nothing() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        host.snapshot(B, 3 * DAY);
        two_answers(&host, "exit 0", "exit 1");
        assert_eq!(refused(&host.gc("2h", 0)), GcRefusal::QueueUnreadable);
        assert_eq!(host.left(), [A, B]);
        assert!(host.snap(B).join("src/pkg/mod.py").is_file());
    }

    #[test]
    fn only_a_whole_lowercase_tree_id_names_a_snapshot() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        let upper = A.to_uppercase();
        let odd = [
            upper.as_str(),
            &A[..39],
            &format!("{A}0"),
            &format!("{A}x"),
            &format!("{B}.bak"),
            "snapshots",
            "provenance",
            "0123456789abcdef0123456789abcdef0123456g",
            "a b",
        ];
        for name in odd {
            let dir = host.snap(name);
            std::fs::create_dir_all(dir.join("keep")).unwrap();
            host.age(&dir, 30 * DAY);
        }
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [A]);
        assert!(
            done.kept.is_empty(),
            "names that are not snapshots are not even reported"
        );
        let mut expected: Vec<String> = odd.iter().map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(host.left(), expected);
    }

    #[test]
    fn a_symlink_with_a_snapshots_name_is_never_followed_or_removed() {
        let host = Host::new();
        host.make_root();
        let outside = host.project().join("outside");
        std::fs::create_dir_all(outside.join("data")).unwrap();
        std::fs::write(outside.join("data/precious"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, host.snap(A)).unwrap();
        std::fs::write(host.snap(B), "a file").unwrap();
        let done = cleaned(&host.gc("2h", 0));
        assert!(done.removed.is_empty());
        assert_eq!(
            done.kept,
            [
                (A.to_string(), Why::NotYours),
                (B.to_string(), Why::NotYours)
            ]
        );
        assert!(outside.join("data/precious").is_file());
        assert!(host.snap(A).is_symlink());
        assert!(host.snap(B).is_file());
    }

    #[test]
    fn a_symlink_among_the_leftovers_is_left_alone_too() {
        let host = Host::new();
        host.make_root();
        let outside = host.project().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, host.root().join(".tmp-link")).unwrap();
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.kept, [(".tmp-link".to_string(), Why::NotYours)]);
        assert!(outside.is_dir());
    }

    #[test]
    fn old_staging_and_trash_directories_are_swept_and_young_ones_are_not() {
        let host = Host::new();
        host.make_root();
        for (name, ago) in [
            (".tmp-OLD12345", 3 * HOUR),
            (".trash-leftover", 5 * DAY),
            (".tmp-LIVE1234", 600),
        ] {
            let dir = host.snap(name);
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub/f"), "x").unwrap();
            host.freeze(&dir, ago);
        }
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [".tmp-OLD12345", ".trash-leftover"]);
        assert_eq!(done.kept, [(".tmp-LIVE1234".to_string(), Why::Young)]);
        assert_eq!(host.left(), [".tmp-LIVE1234"]);
    }

    #[test]
    fn the_sweep_ignores_the_age_flag_and_uses_the_floor() {
        let host = Host::new();
        host.make_root();
        let dir = host.snap(".tmp-ONEDAY12");
        std::fs::create_dir_all(&dir).unwrap();
        host.age(&dir, DAY);
        let done = cleaned(&host.gc("30d", 0));
        assert_eq!(done.removed, [".tmp-ONEDAY12"]);
    }

    #[test]
    fn a_snapshot_set_aside_by_a_run_that_died_is_still_protected_by_the_jobs_in_it() {
        let host = Host::new();
        host.make_root();
        for name in [A, B] {
            let dir = host.snap(&format!(".trash-{name}"));
            std::fs::create_dir_all(dir.join("src")).unwrap();
            host.freeze(&dir, 3 * DAY);
        }
        // the job was submitted from the snapshot's own path, which no longer exists
        host.queue(&[format!("4 {}/src", host.snap(A).display())]);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [format!(".trash-{B}")]);
        assert_eq!(done.kept, [(format!(".trash-{A}"), Why::InUse)]);
        assert_eq!(host.left(), [format!(".trash-{A}")]);
    }

    #[test]
    fn a_snapshot_that_cannot_be_set_aside_stays_and_is_reported() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        // a non-empty `.trash-<id>` that is not a leftover to sweep (it is young) blocks the rename
        let blocker = host.snap(&format!(".trash-{A}"));
        std::fs::create_dir_all(blocker.join("x")).unwrap();
        host.freeze(&blocker, 60);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(
            done.kept,
            [
                (format!(".trash-{A}"), Why::Young),
                (A.to_string(), Why::Error)
            ]
        );
        assert!(done.removed.is_empty());
        assert!(host.snap(A).join("src/pkg/mod.py").is_file());
    }

    #[test]
    fn a_root_that_is_a_symlink_is_refused_and_what_it_points_at_is_untouched() {
        let host = Host::new();
        let elsewhere = host.project().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join(A)).unwrap();
        host.age(&elsewhere.join(A), 30 * DAY);
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, host.root()).unwrap();
        assert_eq!(refused(&host.gc("2h", 0)), GcRefusal::RootNotSafe);
        assert!(elsewhere.join(A).is_dir());
    }

    #[test]
    fn a_root_that_is_not_private_is_refused() {
        for mode in [0o755, 0o770, 0o777, 0o500] {
            let host = Host::new();
            host.snapshot(A, 3 * DAY);
            std::fs::set_permissions(host.root(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                refused(&host.gc("2h", 0)),
                GcRefusal::RootNotSafe,
                "{mode:o}"
            );
            std::fs::set_permissions(host.root(), std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(host.left(), [A], "{mode:o}");
        }
    }

    #[test]
    fn a_root_that_resolves_to_the_filesystem_root_the_home_or_nothing_is_refused() {
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        for answer in ["/", ""] {
            host.stub("realpath", &format!("echo '{answer}'"));
            assert_eq!(
                refused(&host.gc("2h", 0)),
                GcRefusal::RootNotSafe,
                "{answer:?}"
            );
        }
        std::fs::remove_file(host.stubs.path().join("realpath")).unwrap();
        let root = host.root();
        let as_home = host.run_with_home("2h", 0, root.to_str().unwrap());
        assert_eq!(
            refused(&as_home),
            GcRefusal::RootNotSafe,
            "the root is the home"
        );
        let unset = host.run_with_home("2h", 0, "");
        assert_eq!(
            refused(&unset),
            GcRefusal::RootNotSafe,
            "no home to compare with"
        );
        host.stub("realpath", "exit 1");
        assert_eq!(
            refused(&host.gc("2h", 0)),
            GcRefusal::RootNotSafe,
            "realpath failing"
        );
        assert_eq!(host.left(), [A], "nothing was deleted");
    }

    #[test]
    fn without_a_root_there_is_nothing_to_do_and_nothing_is_created() {
        let host = Host::new();
        let done = cleaned(&host.gc("2h", 0));
        assert!(done.removed.is_empty() && done.kept.is_empty() && done.failed.is_empty());
        assert!(!host.root().exists());
    }

    #[test]
    fn what_is_in_the_snapshot_never_decides_anything() {
        // a snapshot whose own files are named like the script's variables or like report lines
        let host = Host::new();
        host.snapshot(A, 3 * DAY);
        let dir = host.snap(A);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(dir.join("src"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.join("src/rr-gc\tdone"), "x").unwrap();
        std::fs::write(dir.join("src/$(touch pwned)"), "x").unwrap();
        host.freeze(&dir, 3 * DAY);
        let done = cleaned(&host.gc("2h", 0));
        assert_eq!(done.removed, [A]);
        assert!(!host.project().join("pwned").exists());
    }
}
