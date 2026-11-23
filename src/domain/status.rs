//! What `rr status` shows about one job: its record and progress from `sacct`, and the end of its
//! log. Pure; every field is remote text and is sanitized where it is shown.

use crate::domain::logpath::{JobRecord, parse_sacct};
use crate::domain::slurm::{JobState, SACCT_SEPARATOR};
use anyhow::{Result, bail};

/// One job as `sacct -X` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub record: JobRecord,
    pub state: JobState,
    /// `HH:MM:SS`, as `sacct` prints it.
    pub elapsed: String,
    /// `code:signal`, as `sacct` prints it.
    pub exit_code: String,
}

/// The end of a job's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogTail {
    Lines {
        path: String,
        lines: Vec<String>,
    },
    /// There is no log to show yet (the job has not started, or its path could not be worked
    /// out); `why` is what was found out.
    Unavailable {
        why: String,
    },
}

/// A job's status and the end of its log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub status: Status,
    pub log: LogTail,
}

/// The one line of `sacct -n -X -P -o` [`STATUS_FIELDS`](crate::domain::slurm::STATUS_FIELDS)
/// (pure). Several lines, none, or a line of another shape is an error; the rest of the line after
/// the progress fields is read by [`parse_sacct`], which names an id that matches several jobs.
pub fn parse_status(text: &str) -> Result<Status> {
    let mut heads = Vec::new();
    let mut rests = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.splitn(4, SACCT_SEPARATOR).collect();
        let [state, elapsed, exit_code, rest] = fields[..] else {
            bail!("unexpected sacct line: {line:?}");
        };
        heads.push((state, elapsed, exit_code));
        rests.push(rest);
    }
    let record = parse_sacct(&rests.join("\n"))?;
    let Some(&(state, elapsed, exit_code)) = heads.first() else {
        bail!("sacct knows no such job");
    };
    Ok(Status {
        record,
        state: JobState::parse(state),
        elapsed: elapsed.into(),
        exit_code: exit_code.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::slurm::{Active, Finished};

    const LINE: &str =
        "RUNNING\u{1f}00:01:02\u{1f}0:0\u{1f}9\u{1f}9\u{1f}n|x\u{1f}u\u{1f}/w\u{1f}/o/%j.out\n";

    #[test]
    fn a_status_line_gives_progress_and_the_record_the_log_path_needs() {
        let status = parse_status(LINE).unwrap();
        assert_eq!(status.state, JobState::Active(Active::Running));
        assert_eq!(
            (status.elapsed.as_str(), status.exit_code.as_str()),
            ("00:01:02", "0:0")
        );
        assert_eq!(status.record.name, "n|x");
        assert_eq!(status.record.stdout, "/o/%j.out");
        let done = LINE.replace("RUNNING", "COMPLETED");
        assert_eq!(
            parse_status(&done).unwrap().state,
            JobState::Finished(Finished::Completed)
        );
    }

    #[test]
    fn no_line_several_lines_and_a_short_line_are_errors() {
        assert!(parse_status("").is_err());
        assert!(parse_status(&format!("{LINE}{LINE}")).is_err());
        assert!(parse_status("RUNNING\u{1f}00:00:01\n").is_err());
        assert!(parse_status("a\u{1f}b\u{1f}c\u{1f}too\u{1f}few\n").is_err());
    }
}
