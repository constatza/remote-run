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
        Output::Raw,
    )
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, at_home, session};
    use super::*;

    #[test]
    fn every_job_id_is_one_quoted_argument_to_scancel() {
        let fakes = Fakes::remote_streaming(vec![0]);
        let jobs = ["1", "2_3"].map(|id| JobId::parse(id).unwrap()).to_vec();
        run(&session(), Args { jobs }, &fakes.ports()).unwrap();
        assert_eq!(fakes.remote.scripts(), [at_home("scancel '1' '2_3'")]);
    }
}
