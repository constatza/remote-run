//! What `rr` knows about Slurm: the `squeue`, `sacct` and `scancel` command text and the field
//! layouts it reads back. Pure; every runtime value reaches the text through [`quote`].

use crate::domain::exit::{Exit, RrCode};
use crate::domain::shell::{Script, lit, number, quote};
use anyhow::{Context, Result, bail};

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

/// The `sacct` fields [`parse_states`](crate::domain::wait::parse_states) reads, in this order.
pub const STATE_FIELDS: &str = "JobID,State";

/// The `sacct -P` fields `rr status` reads: the job's progress, then [`SACCT_FIELDS`] last, so
/// that the rest of the line after the three progress fields is a line `parse_sacct` reads.
pub const STATUS_FIELDS: &str = "State,Elapsed,ExitCode,JobIDRaw,JobID,JobName,User,WorkDir,StdOut";

/// `sbatch` prints this before the job id; `rr run --wait` looks for it in a command's output.
pub const SUBMITTED_PREFIX: &str = "Submitted batch job ";

/// A job that is waiting or running; it can still change state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    Pending,
    Running,
    Completing,
    Suspended,
    Stopped,
    Resizing,
    Requeued,
    SpecialExit,
}

impl Active {
    const ALL: [Active; 8] = [
        Active::Pending,
        Active::Running,
        Active::Completing,
        Active::Suspended,
        Active::Stopped,
        Active::Resizing,
        Active::Requeued,
        Active::SpecialExit,
    ];

    /// The state as `sacct` spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Active::Pending => "PENDING",
            Active::Running => "RUNNING",
            Active::Completing => "COMPLETING",
            Active::Suspended => "SUSPENDED",
            Active::Stopped => "STOPPED",
            Active::Resizing => "RESIZING",
            Active::Requeued => "REQUEUED",
            Active::SpecialExit => "SPECIAL_EXIT",
        }
    }
}

/// A job that will not change state again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finished {
    Completed,
    Failed,
    Cancelled,
    Timeout,
    Deadline,
    OutOfMemory,
    NodeFail,
    BootFail,
    Preempted,
    Revoked,
}

impl Finished {
    const ALL: [Finished; 10] = [
        Finished::Completed,
        Finished::Failed,
        Finished::Cancelled,
        Finished::Timeout,
        Finished::Deadline,
        Finished::OutOfMemory,
        Finished::NodeFail,
        Finished::BootFail,
        Finished::Preempted,
        Finished::Revoked,
    ];

    /// The state as `sacct` spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Finished::Completed => "COMPLETED",
            Finished::Failed => "FAILED",
            Finished::Cancelled => "CANCELLED",
            Finished::Timeout => "TIMEOUT",
            Finished::Deadline => "DEADLINE",
            Finished::OutOfMemory => "OUT_OF_MEMORY",
            Finished::NodeFail => "NODE_FAIL",
            Finished::BootFail => "BOOT_FAIL",
            Finished::Preempted => "PREEMPTED",
            Finished::Revoked => "REVOKED",
        }
    }
}

/// What `sacct` says a job is doing. A state this table does not know is [`JobState::Unknown`]
/// and is never taken as the end of a job: only a known [`Finished`] state stops a wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Active(Active),
    Finished(Finished),
    /// The text `sacct` printed, for the user to read.
    Unknown(String),
}

impl JobState {
    /// Read a `sacct` state. `CANCELLED by 123` is `CANCELLED`, and a trailing `+` (a name cut to
    /// the column width) is dropped.
    pub fn parse(text: &str) -> Self {
        let word = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('+');
        if let Some(state) = Active::ALL.into_iter().find(|s| s.name() == word) {
            return JobState::Active(state);
        }
        match Finished::ALL.into_iter().find(|s| s.name() == word) {
            Some(state) => JobState::Finished(state),
            None => JobState::Unknown(text.trim().to_string()),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            JobState::Active(state) => state.name(),
            JobState::Finished(state) => state.name(),
            JobState::Unknown(raw) => raw,
        }
    }
}

/// A job and its state, as one `sacct` line gave them. The id is what `sacct` prints (`123`,
/// `123_4`, `123_[5-9]`), so it is a [`SqueueId`], not a [`JobId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStatus {
    pub id: SqueueId,
    pub state: JobState,
}

/// At least one job id: waiting for no job would end at once as "all completed".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSet(Vec<JobId>);

impl JobSet {
    pub fn new(jobs: Vec<JobId>) -> Result<Self> {
        if jobs.is_empty() {
            bail!("no job id given");
        }
        Ok(Self(jobs))
    }

    pub fn as_slice(&self) -> &[JobId] {
        &self.0
    }
}

/// The jobs `sbatch` reported in `lines` (the output lines that start with
/// [`SUBMITTED_PREFIX`]), each once. The text is a program's output: the id is the first word after
/// the prefix and must be a valid [`JobId`] (`Submitted batch job 12 on cluster x` is job 12). A
/// prefixed line with a bad id is an error, never skipped: waiting for the other jobs only would
/// report success for a submission that is not fully known. No such line is an error too.
pub fn submitted_jobs(lines: &[String]) -> Result<JobSet> {
    let mut jobs: Vec<JobId> = Vec::new();
    for line in lines {
        let Some(rest) = line.strip_prefix(SUBMITTED_PREFIX) else {
            continue;
        };
        let word = rest.split_whitespace().next().unwrap_or_default();
        let id = JobId::parse(word).with_context(|| format!("unexpected sbatch line: {line:?}"))?;
        if !jobs.contains(&id) {
            jobs.push(id);
        }
    }
    if jobs.is_empty() {
        bail!("the command printed no {SUBMITTED_PREFIX:?}<id> line");
    }
    JobSet::new(jobs)
}

/// How a wait ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Every job is COMPLETED.
    Completed(Vec<JobStatus>),
    /// A job ended in a state other than COMPLETED (the others may still be active).
    Failed(Vec<JobStatus>),
    /// `--timeout` passed with jobs still active; these are the states last read.
    TimedOut(Vec<JobStatus>),
    /// `rr` could not learn how the jobs ended; `why` is remote or program text.
    Unknown { why: String },
}

impl WaitOutcome {
    /// Success only when every job completed; each other ending has its own code, and none of
    /// them is a remote command's.
    pub fn exit(&self) -> Exit {
        match self {
            WaitOutcome::Completed(_) => Exit::Success,
            WaitOutcome::Failed(_) => Exit::Rr(RrCode::JobFailed),
            WaitOutcome::TimedOut(_) => Exit::Rr(RrCode::Timeout),
            WaitOutcome::Unknown { .. } => Exit::Rr(RrCode::Unknown),
        }
    }
}

/// How long one poll's `sacct` may run. ssh's keepalive only notices a dead link; a live link with
/// a stalled `slurmdbd` would leave `sacct` waiting, and `rr wait` with it, past `--timeout`.
const SACCT_POLL_LIMIT_SECS: u32 = 60;

/// One `JobID,State` line per job (`-X`), for
/// [`parse_states`](crate::domain::wait::parse_states). The comma-joined ids are one quoted word.
/// `timeout` ends a stalled `sacct` with exit 124, which the wait reads as an unreadable poll,
/// never as a job that failed.
pub fn states_script(jobs: &JobSet) -> Script {
    let ids: Vec<&str> = jobs.as_slice().iter().map(JobId::as_str).collect();
    lit("timeout ")
        .word(number(SACCT_POLL_LIMIT_SECS))
        .lit(" sacct -n -X ")
        .lit(SACCT_PARSABLE)
        .lit(" -j ")
        .word(quote(&ids.join(",")))
        .lit(" -o ")
        .lit(STATE_FIELDS)
}

/// One job's record and progress, for `rr status`.
pub fn status_script(job: &JobId) -> Script {
    lit("sacct -n -X ")
        .lit(SACCT_PARSABLE)
        .lit(" -j ")
        .word(quote(job.as_str()))
        .lit(" -o ")
        .lit(STATUS_FIELDS)
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

    #[test]
    fn every_sacct_state_is_known_and_an_unknown_one_keeps_its_text() {
        let state = |text| JobState::parse(text);
        for name in [
            "PENDING",
            "RUNNING",
            "COMPLETING",
            "SUSPENDED",
            "STOPPED",
            "RESIZING",
            "REQUEUED",
            "SPECIAL_EXIT",
        ] {
            assert!(matches!(state(name), JobState::Active(_)), "{name}");
            assert_eq!(state(name).name(), name);
        }
        for name in [
            "COMPLETED",
            "FAILED",
            "CANCELLED",
            "TIMEOUT",
            "DEADLINE",
            "OUT_OF_MEMORY",
            "NODE_FAIL",
            "BOOT_FAIL",
            "PREEMPTED",
            "REVOKED",
        ] {
            assert!(matches!(state(name), JobState::Finished(_)), "{name}");
            assert_eq!(state(name).name(), name);
        }
        assert_eq!(
            state("CANCELLED by 123"),
            JobState::Finished(Finished::Cancelled)
        );
        assert_eq!(state("CANCELLED+"), JobState::Finished(Finished::Cancelled));
        assert_eq!(
            state(" RESV_DEL_HOLD \n"),
            JobState::Unknown("RESV_DEL_HOLD".into())
        );
        assert_eq!(state(""), JobState::Unknown(String::new()));
        assert_eq!(state("completed"), JobState::Unknown("completed".into()));
    }

    #[test]
    fn the_states_script_asks_for_all_jobs_in_one_quoted_word() {
        let jobs = JobSet::new(vec![id("1"), id("2_3")]).unwrap();
        assert_eq!(
            states_script(&jobs).to_string(),
            r#"timeout 60 sacct -n -X -P --delimiter=$'\x1f' -j '1,2_3' -o JobID,State"#
        );
        assert!(JobSet::new(vec![]).is_err());
    }

    #[test]
    fn the_status_script_reads_the_record_fields_first() {
        assert!(STATUS_FIELDS.ends_with(SACCT_FIELDS));
        assert_eq!(
            status_script(&id("9")).to_string(),
            format!(r#"sacct -n -X -P --delimiter=$'\x1f' -j '9' -o {STATUS_FIELDS}"#)
        );
    }

    /// The text is a program's output: only a well-formed id after the exact prefix counts.
    #[test]
    fn submitted_jobs_come_only_from_well_formed_sbatch_lines() {
        let lines = |text: &[&str]| text.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let found = submitted_jobs(&lines(&[
            "Submitted batch job 12",
            "Submitted batch job 13 on cluster x",
        ]))
        .unwrap();
        assert_eq!(found.as_slice(), [id("12"), id("13")]);
        for bad in [
            &[][..],
            &["submitted batch job 1"],
            &["Submitted batch job"],
            &["Submitted batch job -u"],
            &["Submitted batch job $(id)"],
            &[" Submitted batch job 1"],
        ] {
            assert!(submitted_jobs(&lines(bad)).is_err(), "{bad:?}");
        }
    }

    /// One bad prefixed line among good ones must not shrink the set: the wait would end without
    /// ever looking at that job.
    #[test]
    fn a_malformed_submit_line_among_good_ones_is_an_error_and_a_repeat_counts_once() {
        let lines = |text: &[&str]| text.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        for bad in ["1abc", "-u", "$(id)", ""] {
            let line = format!("Submitted batch job {bad}");
            let err = submitted_jobs(&lines(&["Submitted batch job 12", &line]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("unexpected sbatch line"), "{bad:?}: {err}");
        }
        let twice = submitted_jobs(&lines(&[
            "Submitted batch job 12",
            "noise",
            "Submitted batch job 12",
        ]))
        .unwrap();
        assert_eq!(twice.as_slice(), [id("12")]);
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

    /// `timeout` stands in front of `sacct` with the limit as its first word, so a stalled
    /// `slurmdbd` ends the call (exit 124) instead of hanging `rr wait`.
    #[cfg(unix)]
    #[test]
    fn the_poll_runs_sacct_under_a_time_limit() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobSet::new(vec![id("7")]).unwrap();
        let seen = arguments_seen_by("timeout", &states_script(&jobs).to_string(), dir.path());
        assert_eq!(seen[..3], ["60", "sacct", "-n"]);
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
