//! `rr q`: your queued and running jobs.

use super::{Ports, stream};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::slurm::queue_script;
use crate::ports::Terminal;
use anyhow::Result;

pub fn run(session: &Session, ports: &Ports) -> Result<Outcome> {
    stream(
        ports,
        &session.place.at_home(),
        queue_script(),
        Terminal::Plain,
    )
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, at_home, session};
    use super::*;
    use crate::domain::exit::Exit;
    use crate::domain::outcome::{Format, render};
    use crate::ports::fake::RemoteCall;

    #[test]
    fn it_streams_squeue_from_the_home_directory_without_a_terminal() {
        let fakes = Fakes::remote_streaming(vec![0]);
        let outcome = run(&session(), &fakes.ports()).unwrap();
        assert_eq!(
            *fakes.remote.calls.borrow(),
            [(
                at_home("squeue --me -o '%.12i %.40j %.9T %.10M %.6D %R'"),
                RemoteCall::Stream(Terminal::Plain)
            )]
        );
        assert_eq!(render(&outcome, Format::Text).exit, Exit::Success);
    }
}
