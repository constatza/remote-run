//! `rr log`: show a job's log.

use super::{Output, Ports, stream};
use crate::config::Session;
use crate::domain::logpath::{log_path, parse_sacct};
use crate::domain::outcome::{Outcome, log_path_note};
use crate::domain::place::script;
use crate::domain::slurm::{Follow, JobId, record_script, tail_script};
use crate::ports::Terminal;
use anyhow::Result;

pub struct Args {
    pub job: JobId,
    pub follow: Follow,
    pub lines: u32,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let home = session.place.at_home();
    let record = ports
        .remote
        .capture(&script(&home, record_script(&args.job)))?;
    let path = log_path(&parse_sacct(&record.into_stdout("sacct")?)?)?;
    ports.notes.note(&log_path_note(&path));
    // a followed log needs a terminal so Ctrl-C reaches tail
    let terminal = match args.follow {
        Follow::Once => Terminal::Plain,
        Follow::Follow => Terminal::Attached,
    };
    let body = tail_script(&path, args.lines, args.follow);
    stream(ports, &home, body, terminal, Output::Filtered)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, HOSTILE, HOSTILE_SHOWN, at_home, session};
    use super::*;
    use crate::domain::exit::RemoteExit;
    use crate::ports::fake::RemoteCall;

    /// `sacct -P` with the unit separator: ids, name, user, directory, `--output` pattern.
    const RECORD: &str = "9\u{1f}9\u{1f}n\u{1f}u\u{1f}/w\u{1f}/o/%j.out\n";

    fn args(follow: Follow) -> Args {
        Args {
            job: JobId::parse("9").unwrap(),
            follow,
            lines: 5,
        }
    }

    #[test]
    fn it_names_the_file_first_then_tails_it_and_a_followed_log_gets_a_terminal() {
        let fakes = Fakes::new(
            vec![],
            vec![],
            vec![(0, RECORD, ""), (0, RECORD, "")],
            vec![0, 0],
        );
        run(&session(), args(Follow::Once), &fakes.ports()).unwrap();
        run(&session(), args(Follow::Follow), &fakes.ports()).unwrap();
        let calls = fakes.remote.calls.borrow();
        assert_eq!(calls[0].1, RemoteCall::Capture);
        assert_eq!(
            calls[1],
            (
                at_home("tail -n 5 -- '/o/9.out'"),
                RemoteCall::StreamFiltered(Terminal::Plain)
            )
        );
        assert_eq!(
            calls[3],
            (
                at_home("tail -n 5 -F -- '/o/9.out'"),
                RemoteCall::StreamFiltered(Terminal::Attached)
            )
        );
        assert_eq!(*fakes.notes.0.borrow(), ["rr: /o/9.out", "rr: /o/9.out"]);
    }

    #[test]
    fn a_job_log_is_sanitized_whether_followed_or_not_and_the_exit_code_is_tails() {
        for follow in [Follow::Once, Follow::Follow] {
            let fakes = Fakes::new(vec![], vec![], vec![(0, RECORD, "")], vec![130]);
            fakes.remote.emit(HOSTILE);
            let outcome = run(&session(), args(follow), &fakes.ports()).unwrap();
            assert_eq!(*fakes.remote.shown.borrow(), HOSTILE_SHOWN.as_bytes());
            assert!(matches!(outcome, Outcome::Remote(code) if code == RemoteExit::new(130)));
        }
    }

    #[test]
    fn a_failed_or_unreadable_sacct_streams_nothing() {
        let failed = Fakes::new(
            vec![],
            vec![],
            vec![(1, "", "sacct: boom\u{1b}[2J\n")],
            vec![],
        );
        let err = run(&session(), args(Follow::Once), &failed.ports()).unwrap_err();
        assert_eq!(err.to_string(), "sacct failed (1): sacct: boom?[2J");
        let unknown = Fakes::new(vec![], vec![], vec![(0, "", "")], vec![]);
        assert!(run(&session(), args(Follow::Once), &unknown.ports()).is_err());
        for fakes in [failed, unknown] {
            assert_eq!(fakes.remote.calls.borrow().len(), 1, "only the sacct call");
            assert!(fakes.notes.0.borrow().is_empty());
        }
    }
}
