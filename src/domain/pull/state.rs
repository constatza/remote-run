//! The remote checkout as one script reports it, and the parser for that report.
//!
//! Every line the script prints starts with [`STATE_PREFIX`], so text the remote shell prints on
//! its own (a login banner, what the env file echoes) is ignored, while a prefixed line the parser
//! does not understand is an error: a guard must read what it was told or refuse.

use super::Target;
use crate::domain::ids::Commit;
use crate::domain::shell::{Script, lit, quote};
use crate::domain::slurm::{SqueueId, job_workdirs_command};
use crate::domain::snapshot::ROOT as SNAPSHOT_ROOT;
use anyhow::{Context, Result, bail};

/// Starts every line of the state report; a tab separates the fields after it.
const STATE_PREFIX: &str = "rr-state";

/// Where an active job was submitted from, as far as the remote could tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Workdir {
    /// The path after `realpath -m`: symlinks, `//`, `..` and trailing slashes resolved.
    Resolved(String),
    /// The remote could not normalise it, so the job counts as inside every checkout.
    Unresolved,
}

/// An active job and the directory it was submitted from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: SqueueId,
    pub workdir: Workdir,
}

impl Job {
    /// Whether the job runs code from the checkout at `dir`: its directory is `dir` or below it,
    /// or is unknown.
    pub fn is_inside(&self, dir: &str) -> bool {
        match &self.workdir {
            Workdir::Resolved(workdir) => inside(workdir, dir),
            Workdir::Unresolved => true,
        }
    }
}

fn inside(workdir: &str, dir: &str) -> bool {
    workdir == dir || workdir.starts_with(&format!("{}/", dir.trim_end_matches('/')))
}

/// The remote checkout as read by [`state_script`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteState {
    /// `pwd -P` of the checkout.
    pub dir: String,
    pub head: Commit,
    /// The target resolved after `git fetch origin` and found in at least one `origin/*` branch
    /// (`git branch -r --contains`); `None` when origin does not have it. An object that is in
    /// the store but on no origin branch (an old fetch, a force-pushed branch) is `None` too.
    pub target: Option<Commit>,
    /// `git status --porcelain` lines.
    pub dirty: Vec<String>,
    pub jobs: Vec<Job>,
}

/// A shell line that prints `value` (a shell expression) as the state field `key`.
fn field(key: &'static str, value: Script) -> Script {
    lit("printf '")
        .lit(STATE_PREFIX)
        .lit("\\t")
        .lit(key)
        .lit("\\t%s\\n' ")
        .then(value)
}

/// Run `command`, then print each of its output lines as the state field `key`.
fn each_line(command: Script, key: &'static str) -> Script {
    let print = field(key, lit("\"$line\""));
    command
        .lit(" | while IFS= read -r line || [ -n \"$line\" ]; do ")
        .then(print)
        .lit("; done")
}

/// The loop over the active jobs, `<id> <submit directory>` lines of [`job_workdirs_command`].
/// `resolved` runs with `$id` and the `realpath -m` result `$resolved` set, `unresolved` when
/// `realpath -m` cannot normalise the directory, and `malformed` for a line with no directory
/// (`$line`). The state read, the merge-time check and `rr gc` all build on this loop, so the query
/// and the normalisation cannot differ between them.
pub(crate) fn job_loop(resolved: Script, unresolved: Script, malformed: Script) -> Script {
    job_workdirs_command()
        .lit(
            r#" | while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    *' '*)
      id=${line%% *}
      if resolved=$(realpath -m -- "${line#* }" 2>/dev/null) && [ -n "$resolved" ]; then
        "#,
        )
        .then(resolved)
        .lit("\n      else\n        ")
        .then(unresolved)
        .lit("\n      fi ;;\n    *) ")
        .then(malformed)
        .lit(" ;;\n  esac\ndone")
}

/// Every active job as a `job` field `<id> <normalised directory>`; a directory `realpath -m`
/// cannot normalise becomes `jobunresolved`, which blocks. A line with no directory is passed
/// through as it is so the parser refuses it.
fn jobs_script() -> Script {
    job_loop(
        field("job", lit(r#""$id $resolved""#)),
        field("jobunresolved", lit(r#""$id""#)),
        field("job", lit(r#""$line""#)),
    )
}

/// Shell that sets `$blocked` to a line for every active job inside the current checkout, one
/// that cannot be placed, and every line it cannot read; the caller passes on an empty `$blocked`
/// only. The test for "inside" is an equivalent shell form of [`inside`], not the same code: the
/// behaviour tests of the merge script pin the two together, and only the `squeue` and `realpath`
/// loop is shared through [`job_loop`]. Each printed line starts with `job`, so an empty id or
/// line cannot print nothing. A failing `squeue`, `realpath` or `pwd` fails this command, which
/// is a refusal and never "no jobs".
pub(super) fn read_blocking_jobs_script() -> Script {
    let inside =
        lit(r#"case "$resolved" in "$checkout"|"${checkout%/}"/*) echo "job $id" ;; esac"#);
    lit("checkout=$(pwd -P) && blocked=$(")
        .then(job_loop(
            inside,
            lit(r#"echo "job $id""#),
            lit("echo 'job line with no directory'"),
        ))
        .lit(")")
}

/// `git status --porcelain` for the checkout, without the directory `rr run --mode snapshot` keeps
/// in it: it is untracked and would make every checkout look dirty after the first snapshot run.
/// An exclude pathspec avoids writing a rule into the remote's `.git`. The state read and the
/// merge-time check both use it, so they cannot disagree on what "clean" means.
pub(super) fn status_command() -> Script {
    lit("git status --porcelain -- . ").word(quote(&format!(":(exclude){SNAPSHOT_ROOT}")))
}

/// The `origin/*` branches that contain the resolved `$target`, one `onorigin` field each; prints
/// nothing when there is no target. A `git` that fails here fails the script (`pipefail`), so an
/// unreadable answer is an error and never an empty one.
fn origin_branches_script() -> Script {
    let branches = lit(r#"git branch -r --contains "$target" --list 'origin/*'"#);
    lit(r#"if [ -n "$target" ]; then "#)
        .then(each_line(branches, "onorigin"))
        .lit("; fi")
}

/// The script that prints the remote state, one prefixed field per line.
pub fn state_script(target: &Target) -> Script {
    let revision = quote(&format!("{}^{{commit}}", target.revision()));
    let resolved = lit(r#"target="$(git rev-parse -q --verify "#)
        .word(revision)
        .lit(r#" || true)""#);
    Script::lines([
        field("dir", lit(r#""$(pwd -P)""#)),
        field("head", lit(r#""$(git rev-parse HEAD)""#)),
        // --prune: a branch deleted on origin must stop counting as "on origin"
        lit("git fetch -q --prune origin"),
        resolved,
        field("target", lit(r#""$target""#)),
        origin_branches_script(),
        each_line(status_command(), "dirty"),
        jobs_script(),
    ])
}

/// Parse the output of [`state_script`] (pure).
pub fn parse_state(text: &str) -> Result<RemoteState> {
    let (mut dir, mut head, mut target) = (None, None, None);
    let (mut dirty, mut jobs, mut branches) = (Vec::new(), Vec::new(), Vec::new());
    for line in text.lines() {
        let Some(fields) = state_fields(line) else {
            continue;
        };
        let (key, value) = fields
            .split_once('\t')
            .with_context(|| format!("remote state line without a value: {line:?}"))?;
        match key {
            "dir" if value.is_empty() => bail!("the remote state has an empty dir"),
            "dir" => set_once(&mut dir, key, value.to_string())?,
            "head" => set_once(&mut head, key, Commit::parse(value)?)?,
            "target" => set_once(&mut target, key, parse_target(value)?)?,
            "dirty" => dirty.push(value.to_string()),
            "onorigin" => branches.push(parse_origin_branch(value)?),
            "job" => jobs.push(parse_job(value)?),
            "jobunresolved" => jobs.push(Job {
                id: SqueueId::parse(value)?,
                workdir: Workdir::Unresolved,
            }),
            other => bail!("unknown remote state key {other:?} in {line:?}"),
        }
    }
    match (dir, head, target) {
        (Some(dir), Some(head), Some(target)) => Ok(RemoteState {
            dir,
            head,
            target: confirm_on_origin(target, &branches)?,
            dirty,
            jobs,
        }),
        _ => bail!("incomplete remote state:\n{text}"),
    }
}

/// What follows the prefix on a state line; `None` for any other line.
fn state_fields(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(STATE_PREFIX)?;
    match rest.strip_prefix('\t') {
        Some(fields) => Some(fields),
        None if rest.is_empty() => Some(rest),
        None => None,
    }
}

fn set_once<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("the remote state names {key:?} twice");
    }
    Ok(())
}

/// One line of `git branch -r --contains`: a branch under `origin/`, with the padding git adds.
/// Anything else is not an answer to the question asked.
fn parse_origin_branch(value: &str) -> Result<String> {
    let branch = value.trim();
    if !branch.starts_with("origin/") {
        bail!("unexpected branch in the remote state: {value:?}");
    }
    Ok(branch.to_string())
}

/// Keep the resolved target only when an origin branch contains it. Branches named for a target
/// that did not resolve contradict each other, so that is an error rather than a guess.
fn confirm_on_origin(target: Option<Commit>, branches: &[String]) -> Result<Option<Commit>> {
    match (target, branches.is_empty()) {
        (Some(commit), false) => Ok(Some(commit)),
        (Some(_), true) | (None, true) => Ok(None),
        (None, false) => bail!("the remote state lists origin branches for a target it lacks"),
    }
}

fn parse_target(value: &str) -> Result<Option<Commit>> {
    if value.is_empty() {
        return Ok(None);
    }
    Commit::parse(value).map(Some)
}

fn parse_job(value: &str) -> Result<Job> {
    match value.split_once(' ') {
        Some((id, workdir)) if !id.is_empty() && !workdir.is_empty() => Ok(Job {
            id: SqueueId::parse(id)?,
            workdir: Workdir::Resolved(workdir.to_string()),
        }),
        _ => bail!("unexpected squeue line: {value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn commit(text: &str) -> Commit {
        Commit::parse(text).unwrap()
    }

    fn report(lines: &[&str]) -> String {
        let mut text = format!(
            "rr-state\tdir\t/work/u/proj\nrr-state\thead\t{A}\nrr-state\ttarget\t{B}\n\
             rr-state\tonorigin\t  origin/HEAD -> origin/main\nrr-state\tonorigin\t  origin/main\n"
        );
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    #[test]
    fn parse_state_reads_every_key() {
        let text = report(&[
            "rr-state\tdirty\t M a b.py",
            "rr-state\tjob\t12 /work/u/my dir",
            "rr-state\tjobunresolved\t13",
        ]);
        let state = parse_state(&text).unwrap();
        assert_eq!(state.dir, "/work/u/proj");
        assert_eq!((state.head, state.target), (commit(A), Some(commit(B))));
        assert_eq!(state.dirty, [" M a b.py"]);
        let workdirs: Vec<_> = state.jobs.iter().map(|j| j.workdir.clone()).collect();
        assert_eq!(
            workdirs,
            [
                Workdir::Resolved("/work/u/my dir".into()),
                Workdir::Unresolved
            ]
        );
        let missing = format!("rr-state\tdir\t/d\nrr-state\thead\t{A}\nrr-state\ttarget\t\n");
        assert_eq!(parse_state(&missing).unwrap().target, None);
        assert!(parse_state("rr-state\tdir\t/x\n").is_err());
    }

    #[test]
    fn an_empty_dir_is_an_error_like_an_empty_head() {
        for (name, text) in [
            (
                "dir",
                format!("rr-state\tdir\t\nrr-state\thead\t{A}\nrr-state\ttarget\t\n"),
            ),
            (
                "head",
                "rr-state\tdir\t/d\nrr-state\thead\t\nrr-state\ttarget\t\n".to_string(),
            ),
        ] {
            assert!(
                parse_state(&text).is_err(),
                "an empty {name} must not parse"
            );
        }
        let err = parse_state(&format!(
            "rr-state\tdir\t\nrr-state\thead\t{A}\nrr-state\ttarget\t\n"
        ));
        assert!(err.unwrap_err().to_string().contains("empty dir"));
    }

    #[test]
    fn a_target_no_origin_branch_contains_is_not_on_origin() {
        let none = format!("rr-state\tdir\t/d\nrr-state\thead\t{A}\nrr-state\ttarget\t{B}\n");
        assert_eq!(parse_state(&none).unwrap().target, None);
        assert_eq!(parse_state(&report(&[])).unwrap().target, Some(commit(B)));
    }

    #[test]
    fn an_unreadable_or_contradictory_branch_answer_is_an_error() {
        for bad in [
            "rr-state\tonorigin\t",
            "rr-state\tonorigin\t   ",
            "rr-state\tonorigin\tfeature",
            "rr-state\tonorigin\tupstream/main",
            "rr-state\tonorigin\tfatal: bad object",
        ] {
            assert!(parse_state(&report(&[bad])).is_err(), "{bad:?}");
        }
        let no_target = format!(
            "rr-state\tdir\t/d\nrr-state\thead\t{A}\nrr-state\ttarget\t\n\
             rr-state\tonorigin\torigin/main\n"
        );
        let err = parse_state(&no_target).unwrap_err().to_string();
        assert!(
            err.contains("origin branches for a target it lacks"),
            "{err}"
        );
    }

    #[test]
    fn only_unprefixed_lines_are_ignored() {
        let noisy = format!(
            "Welcome to the cluster\nrr-states\tx\ndir\t/evil\n{}rr-statement\n",
            report(&[])
        );
        assert_eq!(parse_state(&noisy).unwrap().dir, "/work/u/proj");
    }

    #[test]
    fn a_prefixed_line_that_cannot_be_read_is_an_error() {
        for bad in [
            "rr-state\tcolour\tred",
            "rr-state\tdirty",
            "rr-state",
            "rr-state\t",
            "rr-state\t\tx",
            "rr-state\tjobunresolved\t",
            "rr-state\tjobunresolved\ta b",
        ] {
            assert!(parse_state(&report(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_repeated_dir_head_or_target_is_an_error() {
        for dup in [
            "rr-state\tdir\t/other".to_string(),
            format!("rr-state\thead\t{B}"),
            format!("rr-state\ttarget\t{A}"),
            "rr-state\ttarget\t".to_string(),
        ] {
            let err = parse_state(&report(&[&dup])).unwrap_err().to_string();
            assert!(err.contains("twice"), "{dup:?}: {err}");
        }
    }

    #[test]
    fn a_malformed_job_line_is_an_error_never_skipped() {
        for bad in [
            "rr-state\tjob\tno-workdir",
            "rr-state\tjob\t /work",
            "rr-state\tjob\t12 ",
            "rr-state\tjob\t1\u{1b} /w",
        ] {
            assert!(parse_state(&report(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_guard_reads_pending_array_and_heterogeneous_ids() {
        let text = report(&["rr-state\tjob\t123_[1-5%2] /d", "rr-state\tjob\t124+1 /d"]);
        let ids: Vec<String> = parse_state(&text)
            .unwrap()
            .jobs
            .iter()
            .map(|job| job.id.as_str().to_string())
            .collect();
        assert_eq!(ids, ["123_[1-5%2]", "124+1"]);
    }

    #[test]
    fn inside_is_a_path_prefix_not_a_string_prefix() {
        assert!(inside("/w/proj", "/w/proj"));
        assert!(inside("/w/proj/bin", "/w/proj/"));
        assert!(!inside("/w/proj-runs", "/w/proj"));
        assert!(inside("/anything", "/"));
    }

    #[test]
    fn an_unresolved_job_is_inside_every_checkout() {
        let job = Job {
            id: SqueueId::parse("1").unwrap(),
            workdir: Workdir::Unresolved,
        };
        assert!(job.is_inside("/anywhere"));
    }

    #[test]
    fn the_state_script_uses_printf_only_and_one_squeue() {
        let text = state_script(&Target::OriginHead).to_string();
        assert!(text.contains("git fetch -q --prune origin"), "{text}");
        let query = format!("{} | while", job_workdirs_command());
        assert!(text.contains(&query), "{text}");
        assert!(
            text.contains(crate::domain::slurm::ACTIVE_JOB_STATES),
            "{text}"
        );
        assert!(text.contains("realpath -m -- "), "{text}");
        assert!(
            text.contains(r#"git branch -r --contains "$target" --list 'origin/*'"#),
            "{text}"
        );
        assert!(
            !text.contains("sed"),
            "BSD and busybox sed print a literal t for \\t: {text}"
        );
        for line in text.lines().filter(|l| l.contains("printf")) {
            assert!(line.contains("rr-state\\t"), "{line}");
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // builds real files and directories
mod shell_tests {
    //! The script itself, run by bash against stand-ins for `git` and `squeue`.

    use super::*;
    use crate::adapters::OsExec;
    use crate::domain::argv::Captured;
    use crate::ports::{Argv, Exec, Stdin};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    /// What the stand-in `git` prints for the target `rev-parse -q`.
    const TARGET_IS_B: &str = "echo bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A stand-in `git` with a head, a dirty tree, a target that `target_body` resolves (a shell
    /// snippet; nothing printed means unresolvable) and `branch_body` for what `git branch` does.
    fn git_stub(target_body: &str, branch_body: &str) -> String {
        format!(
            r#"#!/bin/sh
case "$1 $2" in
  "rev-parse HEAD") echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ;;
  "rev-parse -q") {target_body} ;;
  "status --porcelain") printf ' M a b.py\n?? x\ty\n' ;;
  "branch -r") {branch_body} ;;
esac
"#
        )
    }

    const ON_MAIN: &str = r"printf '  origin/HEAD -> origin/main\n  origin/main\n'";

    fn install(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The state report of the checkout `cwd`, with `squeue` printing `queue`.
    fn run_state_script(stubs: &Path, cwd: &Path, queue: &str) -> String {
        let out = run_with_git(stubs, cwd, queue, ON_MAIN);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.stdout
    }

    /// The script's whole result when `git branch` runs `branch_body`.
    fn run_with_git(stubs: &Path, cwd: &Path, queue: &str, branch_body: &str) -> Captured {
        run_with_stub(stubs, cwd, queue, &git_stub(TARGET_IS_B, branch_body))
    }

    /// The script's whole result when the `git` on `PATH` is the program text `git`.
    fn run_with_stub(stubs: &Path, cwd: &Path, queue: &str, git: &str) -> Captured {
        install(stubs, "git", git);
        install(
            stubs,
            "squeue",
            &format!("#!/bin/sh\ncat <<'EOF'\n{queue}\nEOF\n"),
        );
        let body = format!(
            "cd {} && {}",
            quote(&cwd.display().to_string()),
            state_script(&Target::OriginHead)
        );
        let path = format!("PATH={}:/usr/bin:/bin", stubs.display());
        let home = format!("HOME={}", stubs.display());
        let args = [
            path.as_str(),
            home.as_str(),
            "bash",
            "-c",
            &format!("set -euo pipefail\n{body}"),
        ];
        let env = Argv::new("env", args.map(String::from).to_vec());
        OsExec.capture(&env, Stdin::Null).unwrap()
    }

    #[test]
    fn every_line_is_prefixed_and_symlinked_or_odd_directories_are_normalised() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().canonicalize().unwrap().join("real");
        fs::create_dir_all(real.join("proj/bin")).unwrap();
        symlink(&real, root.path().join("home")).unwrap();
        let link = root.path().join("home");
        let queue = format!(
            "7 {l}/proj/\n8 {l}//proj/bin/../bin\n9 {l}/proj-runs\n10 {l}/gone/../proj\nbad-line",
            l = link.display()
        );
        let stubs = tempfile::tempdir().unwrap();
        let text = run_state_script(stubs.path(), &real.join("proj"), &queue);
        assert!(text.lines().all(|l| l.starts_with("rr-state\t")), "{text}");
        let state = parse_state(&text);
        assert!(
            state.is_err(),
            "the line without a directory must refuse: {text}"
        );
        let text = text
            .lines()
            .filter(|l| !l.ends_with("bad-line"))
            .collect::<Vec<_>>()
            .join("\n");
        let state = parse_state(&text).unwrap();
        assert_eq!(state.dirty, [" M a b.py", "?? x\ty"]);
        let dir = &state.dir;
        assert_eq!(dir, &real.join("proj").display().to_string());
        let blocked: Vec<&str> = state
            .jobs
            .iter()
            .filter(|job| job.is_inside(dir))
            .map(|job| job.id.as_str())
            .collect();
        assert_eq!(blocked, ["7", "8", "10"]);
    }

    /// What `git branch -r --contains` prints decides whether the target counts as on origin.
    #[test]
    fn the_target_is_on_origin_only_when_a_branch_contains_it() {
        let root = tempfile::tempdir().unwrap();
        let stubs = tempfile::tempdir().unwrap();
        let on = run_with_git(stubs.path(), root.path(), "5 /else", ON_MAIN);
        assert_eq!(on.code, 0, "{}", on.stderr);
        assert!(on.stdout.contains("rr-state\tonorigin\t  origin/main\n"));
        assert_eq!(
            parse_state(&on.stdout).unwrap().target,
            Some(Commit::parse(&"b".repeat(40)).unwrap())
        );
        let off = run_with_git(stubs.path(), root.path(), "5 /else", "true");
        assert_eq!(off.code, 0, "{}", off.stderr);
        assert!(!off.stdout.contains("onorigin"), "{}", off.stdout);
        assert_eq!(parse_state(&off.stdout).unwrap().target, None);
    }

    /// `git branch` failing is neither "no branch" nor a branch: the script fails, and `sync`
    /// turns a failed state call into an error.
    #[test]
    fn a_failing_branch_query_fails_the_script() {
        let root = tempfile::tempdir().unwrap();
        let stubs = tempfile::tempdir().unwrap();
        let failing = "echo 'fatal: bad' >&2; exit 129";
        let out = run_with_git(stubs.path(), root.path(), "5 /else", failing);
        assert_eq!(out.code, 129, "the stub's own code, not just any failure");
        assert!(out.stderr.contains("fatal: bad"), "{}", out.stderr);
    }

    /// `rev-parse -q` printing nothing is a target the remote lacks: the branch query must not
    /// even run, and the report still parses to "not on origin".
    #[test]
    fn an_unresolvable_target_asks_no_branch_question() {
        let root = tempfile::tempdir().unwrap();
        let stubs = tempfile::tempdir().unwrap();
        let log = stubs.path().join("branch-calls");
        let branch = format!("echo called >> {}", quote(&log.display().to_string()));
        let git = git_stub("true", &branch);
        let out = run_with_stub(stubs.path(), root.path(), "5 /else", &git);
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(!out.stdout.contains("onorigin"), "{}", out.stdout);
        assert!(!log.exists(), "git branch ran with no target");
        assert_eq!(parse_state(&out.stdout).unwrap().target, None);
    }

    #[test]
    fn a_directory_realpath_cannot_normalise_blocks_the_job() {
        let root = tempfile::tempdir().unwrap();
        let stubs = tempfile::tempdir().unwrap();
        install(stubs.path(), "realpath", "#!/bin/sh\nexit 1\n");
        let text = run_state_script(stubs.path(), root.path(), "5 /somewhere/else");
        let state = parse_state(&text).unwrap();
        assert_eq!(state.jobs[0].workdir, Workdir::Unresolved);
        assert!(state.jobs[0].is_inside(&state.dir));
    }

    /// Real `git` against a bare origin in a temporary directory, with no user or system
    /// configuration, so the guard is checked against what git does and not against a stand-in.
    struct Repos {
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
    }

    impl Repos {
        /// A bare `origin.git` and a clone `work` of it, `main` pushed with one commit.
        /// `None` (with a message) when `git` is not installed.
        fn new() -> Option<Self> {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let repos = Self { _tmp: tmp, root };
            if repos.try_git(&repos.root, &["--version"]).is_none() {
                eprintln!("skipped: `git` is not on PATH");
                return None;
            }
            let (origin, work) = (repos.origin(), repos.work());
            // `init -b` needs git >= 2.28; naming the unborn branch afterwards works everywhere.
            repos.git(&repos.root, &["init", "-q", "--bare", &origin]);
            repos.git(
                &repos.root,
                &["-C", &origin, "symbolic-ref", "HEAD", "refs/heads/main"],
            );
            repos.git(&repos.root, &["clone", "-q", &origin, &work]);
            repos.commit("pushed");
            repos.git(&repos.work_path(), &["push", "-q", "origin", "HEAD:main"]);
            Some(repos)
        }

        fn origin(&self) -> String {
            self.root.join("origin.git").display().to_string()
        }

        fn work(&self) -> String {
            self.work_path().display().to_string()
        }

        fn work_path(&self) -> std::path::PathBuf {
            self.root.join("work")
        }

        /// `git` run in `cwd` with a fixed identity and no configuration files; `None` when it
        /// cannot be started at all.
        fn try_git(&self, cwd: &Path, args: &[&str]) -> Option<Captured> {
            let fixed = [
                format!("HOME={}", self.root.display()),
                "GIT_CONFIG_GLOBAL=/dev/null".into(),
                "GIT_CONFIG_SYSTEM=/dev/null".into(),
                "GIT_AUTHOR_NAME=rr".into(),
                "GIT_AUTHOR_EMAIL=rr@example.invalid".into(),
                "GIT_COMMITTER_NAME=rr".into(),
                "GIT_COMMITTER_EMAIL=rr@example.invalid".into(),
                "git".into(),
                "-C".into(),
                cwd.display().to_string(),
            ];
            let rest = args.iter().map(ToString::to_string);
            let argv = Argv::new("env", fixed.into_iter().chain(rest).collect());
            OsExec
                .capture(&argv, Stdin::Null)
                .ok()
                .filter(|out| out.code != 127)
        }

        fn git(&self, cwd: &Path, args: &[&str]) -> String {
            let out = self.try_git(cwd, args).expect("git started a moment ago");
            assert_eq!(out.code, 0, "git {args:?}: {}", out.stderr);
            out.stdout.trim().to_string()
        }

        /// A new empty commit in `work`; returns its id.
        fn commit(&self, message: &str) -> Commit {
            let work = self.work_path();
            self.git(&work, &["commit", "-q", "--allow-empty", "-m", message]);
            Commit::parse(&self.git(&work, &["rev-parse", "HEAD"])).unwrap()
        }

        /// The target of the state report of `work` for `target`, using the real `git`.
        fn target_on_origin(&self, target: &Commit) -> Option<Commit> {
            let stubs = tempfile::tempdir().unwrap();
            install(stubs.path(), "squeue", "#!/bin/sh\necho '5 /else'\n");
            let body = format!(
                "cd {} && {}",
                quote(&self.work()),
                state_script(&Target::Local(target.clone()))
            );
            let args = vec![
                format!("PATH={}:/usr/bin:/bin", stubs.path().display()),
                format!("HOME={}", self.root.display()),
                "GIT_CONFIG_GLOBAL=/dev/null".into(),
                "GIT_CONFIG_SYSTEM=/dev/null".into(),
                "bash".into(),
                "-c".into(),
                format!("set -euo pipefail\n{body}"),
            ];
            let out = OsExec
                .capture(&Argv::new("env", args), Stdin::Null)
                .unwrap();
            assert_eq!(out.code, 0, "{}", out.stderr);
            parse_state(&out.stdout).unwrap().target
        }
    }

    /// The dirty lines the state script reports for `work`, using the real `git`.
    fn dirty_lines(repos: &Repos) -> Vec<String> {
        let stubs = tempfile::tempdir().unwrap();
        install(stubs.path(), "squeue", "#!/bin/sh\necho '5 /else'\n");
        let body = format!(
            "cd {} && {}",
            quote(&repos.work()),
            state_script(&Target::OriginHead)
        );
        let args = vec![
            format!("PATH={}:/usr/bin:/bin", stubs.path().display()),
            format!("HOME={}", repos.root.display()),
            "GIT_CONFIG_GLOBAL=/dev/null".into(),
            "GIT_CONFIG_SYSTEM=/dev/null".into(),
            "bash".into(),
            "-c".into(),
            format!("set -euo pipefail\n{body}"),
        ];
        let out = OsExec
            .capture(&Argv::new("env", args), Stdin::Null)
            .unwrap();
        assert_eq!(out.code, 0, "{}", out.stderr);
        parse_state(&out.stdout).unwrap().dirty
    }

    /// `rr run --mode snapshot` keeps its directory inside the checkout. It must not make the
    /// checkout dirty, and any other untracked file must still do so.
    #[test]
    fn with_real_git_the_snapshot_directory_alone_is_clean_and_other_files_are_dirty() {
        let Some(repos) = Repos::new() else { return };
        let snapshots = repos.work_path().join(SNAPSHOT_ROOT);
        std::fs::create_dir_all(snapshots.join(".tmp-x")).unwrap();
        std::fs::write(snapshots.join(".tmp-x/a.py"), "x").unwrap();
        assert_eq!(dirty_lines(&repos), Vec::<String>::new());
        std::fs::write(repos.work_path().join("stray.txt"), "x").unwrap();
        assert_eq!(dirty_lines(&repos), ["?? stray.txt"]);
        std::fs::remove_file(repos.work_path().join("stray.txt")).unwrap();
        std::fs::create_dir(repos.work_path().join(format!("{SNAPSHOT_ROOT}-other"))).unwrap();
        std::fs::write(
            repos.work_path().join(format!("{SNAPSHOT_ROOT}-other/f")),
            "x",
        )
        .unwrap();
        assert_eq!(
            dirty_lines(&repos).len(),
            1,
            "a sibling name is not the root"
        );
    }

    #[test]
    fn with_real_git_only_a_pushed_commit_is_on_origin() {
        let Some(repos) = Repos::new() else { return };
        let pushed = Commit::parse(&repos.git(&repos.work_path(), &["rev-parse", "HEAD"])).unwrap();
        let local_only = repos.commit("local only");
        assert_eq!(repos.target_on_origin(&pushed), Some(pushed.clone()));
        assert_eq!(repos.target_on_origin(&local_only), None);
    }

    /// A commit that only a branch since deleted on origin held: the object is still in the
    /// clone's store, but the fetch must prune the branch so it no longer counts as on origin.
    #[test]
    fn with_real_git_a_commit_on_a_deleted_origin_branch_is_not_on_origin() {
        let Some(repos) = Repos::new() else { return };
        let work = repos.work_path();
        let feature = repos.commit("feature work");
        repos.git(&work, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
        repos.git(&work, &["reset", "-q", "--hard", "HEAD~1"]);
        assert_eq!(repos.target_on_origin(&feature), Some(feature.clone()));
        let origin = repos.origin();
        repos.git(
            &repos.root,
            &["--git-dir", &origin, "branch", "-D", "feature"],
        );
        assert_eq!(repos.target_on_origin(&feature), None);
        let kept = repos
            .try_git(&work, &["cat-file", "-e", feature.as_str()])
            .unwrap();
        assert_eq!(kept.code, 0, "the object is still in the store");
    }
}
