//! `rr status`: one job's state and the end of its log.

use super::Ports;
use crate::config::Session;
use crate::domain::logpath::log_path;
use crate::domain::outcome::Outcome;
use crate::domain::place::script;
use crate::domain::slurm::{Follow, JobId, status_script, tail_script};
use crate::domain::status::{LogTail, StatusReport, parse_status};
use anyhow::{Result, bail};

/// What `tail` exits with when it cannot open the file (not there yet, or not readable): the one
/// failure that is a state of the job, not of the connection.
const TAIL_CANNOT_OPEN: i32 = 1;

pub struct Args {
    pub job: JobId,
    pub lines: u32,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let home = session.place.at_home();
    let record = ports
        .remote
        .capture(&script(&home, status_script(&args.job)))?;
    let status = parse_status(&record.into_stdout("sacct")?)?;
    // a job that has not started has no log yet: that is a state of the job, not a failure
    let log = match log_path(&status.record) {
        Err(err) => LogTail::Unavailable {
            why: format!("{err:#}"),
        },
        Ok(path) => {
            let tail = tail_script(&path, args.lines, Follow::Once);
            let shown = ports.remote.capture(&script(&home, tail))?;
            match shown.code {
                0 => LogTail::Lines {
                    path,
                    lines: shown.stdout.lines().map(String::from).collect(),
                },
                TAIL_CANNOT_OPEN => LogTail::Unavailable {
                    why: shown.stderr.trim().to_string(),
                },
                // ssh itself failing (255) or a missing program must not look like "no log yet"
                code => bail!(
                    "reading the log of job {} failed ({code}): {}",
                    status.record.id,
                    shown.stderr.trim()
                ),
            }
        }
    };
    Ok(Outcome::Status(StatusReport { status, log }))
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, at_home, session};
    use super::*;
    use crate::domain::exit::Exit;
    use crate::domain::outcome::{Format, render};

    const RECORD: &str =
        "RUNNING\u{1f}00:01:02\u{1f}0:0\u{1f}9\u{1f}9\u{1f}n\u{1f}u\u{1f}/w\u{1f}/o/%j.out\n";

    fn args() -> Args {
        Args {
            job: JobId::parse("9").unwrap(),
            lines: 3,
        }
    }

    #[test]
    fn the_status_reads_the_record_then_the_end_of_the_log() {
        let fakes = Fakes::new(
            vec![],
            vec![],
            vec![(0, RECORD, ""), (0, "one\ntwo\u{1b}[2J\n", "")],
            vec![],
        );
        let outcome = run(&session(), args(), &fakes.ports()).unwrap();
        let scripts = fakes.remote.scripts();
        assert!(
            scripts[0].starts_with(&at_home("sacct -n -X")),
            "{}",
            scripts[0]
        );
        assert_eq!(scripts[1], at_home("tail -n 3 -- '/o/9.out'"));
        let shown = render(&outcome, Format::Text);
        assert_eq!(shown.exit, Exit::Success);
        assert_eq!(
            shown.stdout,
            "job 9 (n)\nstate RUNNING\nelapsed 00:01:02\nexit 0:0\ndir /w\nlog /o/9.out\none\ntwo?[2J\n"
        );
    }

    /// A pending job has no log file: `tail` fails, and the status still shows the job.
    #[test]
    fn a_missing_log_is_part_of_the_status_not_an_error() {
        let record = RECORD.replace("RUNNING", "PENDING");
        let tail = (1, "", "tail: cannot open '/o/9.out'\n");
        let fakes = Fakes::new(vec![], vec![], vec![(0, &record, ""), tail], vec![]);
        let outcome = run(&session(), args(), &fakes.ports()).unwrap();
        let shown = render(&outcome, Format::Text);
        assert!(shown.stdout.contains("state PENDING"), "{}", shown.stdout);
        assert!(
            shown
                .stdout
                .ends_with("log unavailable: tail: cannot open '/o/9.out'\n"),
            "{}",
            shown.stdout
        );
    }

    #[test]
    fn an_unknown_job_and_a_failed_ssh_are_errors() {
        let fakes = Fakes::new(vec![], vec![], vec![(0, "", "")], vec![]);
        let err = run(&session(), args(), &fakes.ports()).unwrap_err();
        assert!(!format!("{err:#}").is_empty());
        let fakes = Fakes::new(vec![], vec![], vec![(255, "", "ssh: no route")], vec![]);
        let err = format!("{:#}", run(&session(), args(), &fakes.ports()).unwrap_err());
        assert!(
            err.contains("255") && err.contains("ssh: no route"),
            "{err}"
        );
        assert_eq!(fakes.remote.calls.borrow().len(), 1);
    }

    /// Only `tail` failing to open the file is "no log"; a dead link while reading the log is not.
    #[test]
    fn a_failed_ssh_while_reading_the_log_is_an_error_not_an_unavailable_log() {
        let tail = (255, "", "ssh: no route\n");
        let fakes = Fakes::new(vec![], vec![], vec![(0, RECORD, ""), tail], vec![]);
        let err = format!("{:#}", run(&session(), args(), &fakes.ports()).unwrap_err());
        assert!(
            err.contains("255") && err.contains("ssh: no route"),
            "{err}"
        );
        assert_eq!(fakes.remote.calls.borrow().len(), 2);
    }
}
