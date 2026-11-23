//! `rr get`: copy from the remote; a relative remote path is inside the project directory.

use super::Ports;
use super::copy::copy;
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::transfer::Endpoint;
use anyhow::Result;

pub struct Args {
    pub remote: String,
    pub local: String,
    /// Refuse a copy larger than this many bytes.
    pub limit: u64,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let path = session.place.remote_path(&args.remote);
    let source = Endpoint::Remote(session.host.clone(), path);
    copy(ports, &source, &Endpoint::local(&args.local), args.limit)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, session};
    use super::*;
    use crate::ports::fake::output;

    #[test]
    fn a_relative_remote_path_is_inside_the_project_and_the_copy_goes_to_the_local_path() {
        let fakes = Fakes::new(
            vec![output(0, "Total transferred file size: 5 bytes\n", "")],
            vec![0],
            vec![],
            vec![],
        );
        let args = Args {
            remote: "out/r.txt".into(),
            local: "here".into(),
            limit: 100,
        };
        run(&session(), args, &fakes.ports()).unwrap();
        let words = fakes.exec.words();
        assert_eq!(words.len(), 2, "measure, then copy");
        assert_eq!(
            words[1][words[1].len() - 2..],
            ["cluster:./proj/out/r.txt", "here"]
        );
        assert!(words[0].contains(&"--dry-run".to_string()));
    }

    #[test]
    fn a_local_path_that_looks_remote_stays_local() {
        let fakes = Fakes::new(
            vec![output(0, "Total transferred file size: 5 bytes\n", "")],
            vec![0],
            vec![],
            vec![],
        );
        let args = Args {
            remote: "x".into(),
            local: "evil::mod".into(),
            limit: 100,
        };
        run(&session(), args, &fakes.ports()).unwrap();
        let words = fakes.exec.words();
        assert_eq!(words[1].last().unwrap(), "./evil::mod");
    }
}
