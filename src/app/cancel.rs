//! `rr cancel`: cancel jobs.

use super::{Output, Ports, stream};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::slurm::{JobId, cancel_script};
use crate::ports::Terminal;
use anyhow::Result;

pub struct Args {
    pub jobs: Vec<JobId>,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let body = cancel_script(&args.jobs);
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
    use crate::ports::fake::RemoteCall;

    #[test]
    fn every_job_id_is_one_quoted_argument_to_scancel() {
        let fakes = Fakes::remote_streaming(vec![0]);
        let jobs = ["1", "2_3"].map(|id| JobId::parse(id).unwrap()).to_vec();
        run(&session(), Args { jobs }, &fakes.ports()).unwrap();
        assert_eq!(fakes.remote.scripts(), [at_home("scancel '1' '2_3'")]);
    }

    #[test]
    fn what_scancel_prints_is_sanitized_and_streamed_filtered() {
        let fakes = Fakes::remote_streaming(vec![0]);
        fakes.remote.emit(HOSTILE);
        let jobs = vec![JobId::parse("1").unwrap()];
        run(&session(), Args { jobs }, &fakes.ports()).unwrap();
        assert_eq!(*fakes.remote.shown.borrow(), HOSTILE_SHOWN.as_bytes());
        let calls = fakes.remote.calls.borrow();
        assert!(matches!(
            calls[0].1,
            RemoteCall::StreamFiltered(Terminal::Plain)
        ));
    }
}
