//! What `rr` knows about Slurm: the `squeue`, `sacct` and `scancel` command text and the field
//! layouts it reads back. Pure; every runtime value reaches the text through [`quote`].

use crate::domain::shell::{Script, lit, number, quote};
use anyhow::{Result, bail};

/// The `squeue` columns `rr q` shows.
pub const QUEUE_FORMAT: &str = "%.12i %.40j %.9T %.10M %.6D %R";

/// The `sacct` columns `rr acct` shows.
pub const ACCT_FIELDS: &str = "JobID,JobName%40,State,Elapsed,ExitCode";

/// The `sacct -P` fields [`crate::domain::logpath::parse_sacct`] reads, in this order.
pub const SACCT_FIELDS: &str = "JobIDRaw,JobID,JobName,User,WorkDir,StdOut";

/// Separates the fields. `sacct -P` uses `|`, which a job name may contain and which then shifts
/// every later field, so the ASCII unit separator is used instead.
pub const SACCT_SEPARATOR: char = '\u{1f}';

/// `squeue` prints a job as `<id> <submit directory>` with this format (`%Z` is the directory).
const JOB_WORKDIR_FORMAT: &str = "%i %Z";

/// A job id typed by the user: `123`, `123_4` (array task) or `123.batch` (step). The strict
/// shape keeps `-u`, whitespace and control characters out of `scancel` and `sacct` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobId(String);

impl JobId {
    pub fn parse(text: &str) -> Result<Self> {
        let (job, step) = match text.split_once('.') {
            Some((job, step)) => (job, Some(step)),
            None => (text, None),
        };
        let (job, task) = match job.split_once('_') {
            Some((job, task)) => (job, Some(task)),
            None => (job, None),
        };
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        let name = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric());
        if !(digits(job) && task.is_none_or(digits) && step.is_none_or(name)) {
            bail!("not a job id: {text:?} (expected 123, 123_4 or 123.batch)");
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A job id as `squeue` prints it (`123`, `123_4`, `123_[1-5%2]` for pending array tasks, `124+1`
/// for heterogeneous jobs). Any non-empty token without whitespace or control characters is
/// accepted: the pull guard must read whatever `squeue` prints, because refusing to parse a job
/// line is safe but skipping it is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqueueId(String);

impl SqueueId {
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() || text.contains(|c: char| c.is_whitespace() || c.is_control()) {
            bail!("not a job id: {text:?}");
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `tail` stops at the end of the file or keeps following it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follow {
    Once,
    Follow,
}

/// The `sacct` options that make the output `parse_sacct` reads: parsable (`-P`, which
/// `--delimiter` needs) with [`SACCT_SEPARATOR`] written as bash `$'\x1f'`, so the script text
/// stays printable. It spells the same character as [`SACCT_SEPARATOR`];
/// a test keeps the two equal.
const SACCT_PARSABLE: &str = "-P --delimiter=$'\\x1f'";

/// Your queued and running jobs.
pub fn queue_script() -> Script {
    lit("squeue --me -o ").word(quote(QUEUE_FORMAT))
}

/// Your jobs since `since` (a `sacct -S` time, user input).
pub fn acct_script(since: &str) -> Script {
    lit("sacct -X -u \"$USER\" -S ")
        .word(quote(since))
        .lit(" -o ")
        .lit(ACCT_FIELDS)
}

/// One job's record, for [`crate::domain::logpath::parse_sacct`].
pub fn record_script(job: &JobId) -> Script {
    lit("sacct -n -X ")
        .lit(SACCT_PARSABLE)
        .lit(" -j ")
        .word(quote(job.as_str()))
        .lit(" -o ")
        .lit(SACCT_FIELDS)
}

/// Cancel `jobs`.
pub fn cancel_script(jobs: &[JobId]) -> Script {
    lit("scancel ").words(jobs.iter().map(|job| quote(job.as_str())))
}

/// The last `lines` lines of the file at `path`, and with [`Follow::Follow`] the lines appended
/// later (`-F` keeps following through a rotated or not-yet-created file).
pub fn tail_script(path: &str, lines: u32, follow: Follow) -> Script {
    let flag = match follow {
        Follow::Once => "",
        Follow::Follow => " -F",
    };
    lit("tail -n ")
        .word(number(lines))
        .lit(flag)
        .lit(" -- ")
        .word(quote(path))
}

/// Every active job as `<id> <submit directory>` lines, for the pull guard.
pub fn job_workdirs_command() -> Script {
    lit("squeue --me -h -o ").word(quote(JOB_WORKDIR_FORMAT))
}

#[allow(clippy::disallowed_methods)] // tests build real files
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::OsExec;
    use crate::ports::{Argv, Exec, Stdin};
    use std::fs;
    use std::path::Path;

    #[test]
    fn the_queue_script_is_pinned() {
        assert_eq!(
            queue_script().to_string(),
            "squeue --me -o '%.12i %.40j %.9T %.10M %.6D %R'"
        );
    }

    #[test]
    fn the_acct_script_quotes_since() {
        assert_eq!(
            acct_script("now-1days").to_string(),
            r#"sacct -X -u "$USER" -S 'now-1days' -o JobID,JobName%40,State,Elapsed,ExitCode"#
        );
        assert_eq!(
            acct_script("x'; touch /tmp/p; '").to_string(),
            r#"sacct -X -u "$USER" -S 'x'\''; touch /tmp/p; '\''' -o JobID,JobName%40,State,Elapsed,ExitCode"#
        );
    }

    fn id(text: &str) -> JobId {
        JobId::parse(text).unwrap()
    }

    #[test]
    fn the_record_script_is_pinned() {
        assert_eq!(
            record_script(&id("123_4.batch")).to_string(),
            r#"sacct -n -X -P --delimiter=$'\x1f' -j '123_4.batch' -o JobIDRaw,JobID,JobName,User,WorkDir,StdOut"#
        );
    }

    /// The delimiter spelled in the script is the character `parse_sacct` splits on.
    #[test]
    fn the_script_delimiter_is_the_separator_the_parser_splits_on() {
        let spelled = format!("$'\\x{:02x}'", u32::from(SACCT_SEPARATOR));
        assert!(SACCT_PARSABLE.ends_with(&spelled), "{SACCT_PARSABLE}");
    }

    #[test]
    fn the_cancel_script_names_every_job() {
        assert_eq!(
            cancel_script(&[id("1"), id("2_3")]).to_string(),
            "scancel '1' '2_3'"
        );
    }

    #[test]
    fn a_user_job_id_is_digits_with_an_optional_task_and_step() {
        for text in ["1", "123", "123_4", "123.batch", "123_4.0", "123.extern"] {
            assert_eq!(JobId::parse(text).unwrap().as_str(), text);
        }
        for text in [
            "",
            "-u",
            "-1",
            "1 2",
            "1\n",
            "1\0",
            "1\u{1b}",
            "1;id",
            "$(id)",
            "a",
            "1_",
            "_1",
            "1_a",
            "1_2_3",
            "1.",
            ".1",
            "1.a.b",
            "1.b-c",
            "123_[1-5]",
            "124+1",
            "\u{0661}",
        ] {
            assert!(JobId::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn the_squeue_id_reads_what_squeue_prints_and_refuses_what_could_hide() {
        for text in ["123", "123_4", "123_[1-5%2]", "124+1"] {
            assert_eq!(SqueueId::parse(text).unwrap().as_str(), text);
        }
        for text in ["", " ", "1 2", "1\t2", "1\n", "1\0", "1\u{1b}[2J", "\u{9b}"] {
            assert!(SqueueId::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn the_tail_script_quotes_the_path_and_follows_on_request() {
        assert_eq!(
            tail_script("/o/9.out", 40, Follow::Once).to_string(),
            "tail -n 40 -- '/o/9.out'"
        );
        assert_eq!(
            tail_script("/o/a b's.out", 7, Follow::Follow).to_string(),
            r"tail -n 7 -F -- '/o/a b'\''s.out'"
        );
    }

    #[test]
    fn the_pull_guard_reads_the_same_squeue_it_always_did() {
        assert_eq!(
            job_workdirs_command().to_string(),
            "squeue --me -h -o '%i %Z'"
        );
    }

    /// Run `command` under bash with a stand-in for `program` that prints its arguments one per
    /// line, so a test sees what the program would receive.
    fn arguments_seen_by(program: &str, command: &str, dir: &Path) -> Vec<String> {
        let stub = dir.join(program);
        fs::write(&stub, "#!/bin/sh\nfor a; do printf '%s\\n' \"$a\"; done\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("PATH={}:/usr/bin:/bin", dir.display());
        let args = [path.as_str(), "USER=tester", "bash", "-c", command];
        let env = Argv::new("env", args.map(String::from).to_vec());
        OsExec
            .capture(&env, Stdin::Null)
            .unwrap()
            .stdout
            .lines()
            .map(String::from)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn hostile_input_stays_one_argument_and_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        let hostile = format!("x'; touch {0}; echo '$(touch {0})", marker.display());
        let seen = arguments_seen_by("sacct", &acct_script(&hostile).to_string(), dir.path());
        assert_eq!(seen[..4], ["-X", "-u", "tester", "-S"]);
        assert_eq!(seen[4], hostile);
        let seen = arguments_seen_by("sacct", &record_script(&id("7_1")).to_string(), dir.path());
        assert_eq!(seen[..4], ["-n", "-X", "-P", "--delimiter=\u{1f}"]);
        assert_eq!(seen[5], "7_1");
        let seen = arguments_seen_by(
            "scancel",
            &cancel_script(&[id("1"), id("2_3")]).to_string(),
            dir.path(),
        );
        assert_eq!(seen, ["1", "2_3"]);
        assert!(!marker.exists());
    }
}
