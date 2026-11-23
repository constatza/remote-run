//! `rr acct`: your recent jobs.

use super::{Ports, stream_filtered};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::slurm::acct_script;
use crate::ports::Terminal;
use anyhow::Result;

pub struct Args {
    /// A `sacct -S` time, as typed.
    pub since: String,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let body = acct_script(&args.since);
    stream_filtered(ports, &session.place.at_home(), body, Terminal::Plain)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, HOSTILE, HOSTILE_SHOWN, at_home, session};
    use super::*;
    use crate::domain::exit::RemoteExit;
    use crate::ports::fake::RemoteCall;

    #[test]
    fn the_time_is_quoted_into_one_sacct_argument() {
        let fakes = Fakes::remote_streaming(vec![4]);
        let args = Args {
            since: "x'; id; '".into(),
        };
        let outcome = run(&session(), args, &fakes.ports()).unwrap();
        let sent = fakes.remote.scripts();
        assert_eq!(sent, [at_home(&acct_script("x'; id; '").to_string())]);
        assert!(sent[0].contains(r"-S 'x'\''; id; '\'''"), "{}", sent[0]);
        assert!(matches!(outcome, Outcome::Remote(code) if code == RemoteExit::new(4)));
    }

    #[test]
    fn what_sacct_prints_is_sanitized_and_streamed_filtered() {
        let fakes = Fakes::remote_streaming(vec![0]);
        fakes.remote.emit(HOSTILE);
        let args = Args {
            since: "now-1day".into(),
        };
        run(&session(), args, &fakes.ports()).unwrap();
        assert_eq!(*fakes.remote.shown.borrow(), HOSTILE_SHOWN.as_bytes());
        let calls = fakes.remote.calls.borrow();
        assert!(matches!(
            calls[0].1,
            RemoteCall::StreamFiltered(Terminal::Plain)
        ));
    }
}
