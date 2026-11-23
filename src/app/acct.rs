//! `rr acct`: your recent jobs.

use super::{Output, Ports, stream};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::slurm::{AcctColumns, AcctSelection, acct_script};
use crate::ports::Terminal;
use anyhow::Result;

pub struct Args {
    pub selection: AcctSelection,
    pub columns: AcctColumns,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let body = acct_script(&args.selection, args.columns);
    stream(
        ports,
        &session.place.at_home(),
        body,
        Terminal::Plain,
        Output::Filtered,
    )
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
        let selection = AcctSelection::Since("x'; id; '".into());
        let args = Args {
            selection: selection.clone(),
            columns: AcctColumns::Brief,
        };
        let outcome = run(&session(), args, &fakes.ports()).unwrap();
        let sent = fakes.remote.scripts();
        let expected = acct_script(&selection, AcctColumns::Brief).to_string();
        assert_eq!(sent, [at_home(&expected)]);
        assert!(sent[0].contains(r"-S 'x'\''; id; '\'''"), "{}", sent[0]);
        assert!(matches!(outcome, Outcome::Remote(code) if code == RemoteExit::new(4)));
    }

    #[test]
    fn what_sacct_prints_is_sanitized_and_streamed_filtered() {
        let fakes = Fakes::remote_streaming(vec![0]);
        fakes.remote.emit(HOSTILE);
        let args = Args {
            selection: AcctSelection::Since("now-1day".into()),
            columns: AcctColumns::Brief,
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
