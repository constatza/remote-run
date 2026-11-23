//! `rr put`: copy to the remote; never deletes.

use super::Ports;
use super::copy::copy;
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::transfer::Endpoint;
use anyhow::{Result, bail};

pub struct Args {
    pub local: String,
    /// Where to write; the project directory when absent.
    pub remote: Option<String>,
    /// Refuse a copy larger than this many bytes.
    pub limit: u64,
}

/// Where `rr put` writes: the given path, else the project directory.
fn destination(session: &Session, remote: Option<&str>) -> Result<Endpoint> {
    let path = match (remote, &session.place.dir) {
        (Some(path), _) => session.place.remote_path(path),
        (None, Some(dir)) => dir.join(""),
        (None, None) => bail!("no destination: pass one or set RR_DIR"),
    };
    Ok(Endpoint::Remote(session.host.clone(), path))
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let to = destination(session, args.remote.as_deref())?;
    copy(ports, &Endpoint::local(&args.local), &to, args.limit)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, session};
    use super::*;
    use crate::config::Session;
    use crate::ports::fake::output;

    fn put(session: &Session, remote: Option<&str>) -> (Fakes, Result<Outcome>) {
        let stats = output(0, "Total transferred file size: 5 bytes\n", "");
        let fakes = Fakes::new(vec![stats], vec![0], vec![], vec![]);
        let args = Args {
            local: "f.txt".into(),
            remote: remote.map(String::from),
            limit: 100,
        };
        let done = run(session, args, &fakes.ports());
        (fakes, done)
    }

    #[test]
    fn it_writes_to_the_project_directory_unless_told_otherwise_and_never_deletes() {
        let (fakes, done) = put(&session(), None);
        done.unwrap();
        let words = fakes.exec.words();
        assert_eq!(
            words[1][words[1].len() - 3..],
            ["--", "f.txt", "cluster:./proj/"]
        );
        assert!(words.iter().flatten().all(|word| !word.contains("delete")));
        let (fakes, done) = put(&session(), Some("/abs/there"));
        done.unwrap();
        assert_eq!(fakes.exec.words()[1].last().unwrap(), "cluster:/abs/there");
    }

    #[test]
    fn without_a_directory_or_a_destination_nothing_is_copied() {
        let mut bare = session();
        bare.place.dir = None;
        let (fakes, done) = put(&bare, None);
        assert_eq!(
            done.unwrap_err().to_string(),
            "no destination: pass one or set RR_DIR"
        );
        assert!(fakes.exec.calls.borrow().is_empty());
        let (_, done) = put(&bare, Some("x"));
        done.unwrap();
    }
}
