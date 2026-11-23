//! `rr acct`: your recent jobs.

use super::{Ports, stream};
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
    stream(ports, &session.place.at_home(), body, Terminal::Plain)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, at_home, session};
    use super::*;
    use crate::domain::exit::RemoteExit;

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
}
