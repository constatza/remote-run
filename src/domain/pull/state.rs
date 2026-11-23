//! The remote checkout as one script reports it, and the parser for that report.
//!
//! Every line the script prints starts with [`STATE_PREFIX`], so text the remote shell prints on
//! its own (a login banner, what the env file echoes) is ignored, while a prefixed line the parser
//! does not understand is an error: a guard must read what it was told or refuse.

use super::Target;
use crate::domain::ids::Commit;
use crate::domain::shell::{Script, lit, quote};
use crate::domain::slurm::{SqueueId, job_workdirs_command};
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
    /// The target resolved after `git fetch origin`; `None` when origin does not have it.
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

/// Every active job as a `job` field `<id> <normalised directory>`; a directory `realpath -m`
/// cannot normalise becomes `jobunresolved`, which blocks. A line with no directory is passed
/// through as it is so the parser refuses it.
fn jobs_script() -> Script {
    let resolved = field("job", lit(r#""$id $resolved""#));
    let unresolved = field("jobunresolved", lit(r#""$id""#));
    let malformed = field("job", lit(r#""$line""#));
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

/// The script that prints the remote state, one prefixed field per line.
pub fn state_script(target: &Target) -> Script {
    let revision = quote(&format!("{}^{{commit}}", target.revision()));
    let resolved = lit(r#""$(git rev-parse -q --verify "#)
        .word(revision)
        .lit(r#" || true)""#);
    Script::lines([
        field("dir", lit(r#""$(pwd -P)""#)),
        field("head", lit(r#""$(git rev-parse HEAD)""#)),
        lit("git fetch -q origin"),
        field("target", resolved),
        each_line(lit("git status --porcelain"), "dirty"),
        jobs_script(),
    ])
}

/// Parse the output of [`state_script`] (pure).
pub fn parse_state(text: &str) -> Result<RemoteState> {
    let (mut dir, mut head, mut target) = (None, None, None);
    let (mut dirty, mut jobs) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let Some(fields) = state_fields(line) else {
            continue;
        };
        let (key, value) = fields
            .split_once('\t')
            .with_context(|| format!("remote state line without a value: {line:?}"))?;
        match key {
            "dir" => set_once(&mut dir, key, value.to_string())?,
            "head" => set_once(&mut head, key, Commit::parse(value)?)?,
            "target" => set_once(&mut target, key, parse_target(value)?)?,
            "dirty" => dirty.push(value.to_string()),
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
            target,
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
        let mut text =
            format!("rr-state\tdir\t/work/u/proj\nrr-state\thead\t{A}\nrr-state\ttarget\t{B}\n");
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
        assert!(text.contains("squeue --me -h -o '%i %Z' | while"), "{text}");
        assert!(text.contains("realpath -m -- "), "{text}");
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
    use crate::ports::{Argv, Exec, Stdin};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    const GIT: &str = r#"#!/bin/sh
case "$1 $2" in
  "rev-parse HEAD") echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ;;
  "rev-parse -q") echo bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ;;
  "status --porcelain") printf ' M a b.py\n?? x\ty\n' ;;
esac
"#;

    fn install(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The state report of the checkout `cwd`, with `squeue` printing `queue`.
    fn run_state_script(stubs: &Path, cwd: &Path, queue: &str) -> String {
        install(stubs, "git", GIT);
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
        let args = [
            path.as_str(),
            "bash",
            "-c",
            &format!("set -euo pipefail\n{body}"),
        ];
        let env = Argv::new("env", args.map(String::from).to_vec());
        let out = OsExec.capture(&env, Stdin::Null).unwrap();
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.stdout
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
}
