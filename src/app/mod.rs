//! One function per command: it takes its arguments and the ports, talks to the remote through
//! them, and returns an [`Outcome`]. It never prints and never picks an exit code; `main` renders
//! the outcome. Commands that stream a child's output first say what they did through
//! [`Notes`], because that text must come before the output.

use crate::domain::exit::RemoteExit;
use crate::domain::outcome::Outcome;
use crate::domain::place::{Place, script};
use crate::domain::shell::Script;
use crate::ports::{Exec, Notes, Remote, Terminal};
use anyhow::Result;

pub mod acct;
pub mod cancel;
mod copy;
pub mod exec;
pub mod get;
mod git;
pub mod log;
pub mod pull;
pub mod put;
pub mod q;
pub mod run;
pub mod trust;

/// What a command may use besides its arguments.
pub struct Ports<'a> {
    pub exec: &'a dyn Exec,
    pub remote: &'a dyn Remote,
    pub notes: &'a dyn Notes,
}

/// Run `body` at `place` on the remote with the terminal attached, and report how it ended.
fn stream(ports: &Ports, place: &Place, body: Script, terminal: Terminal) -> Result<Outcome> {
    let code = ports.remote.stream(&script(place, body), terminal)?;
    Ok(Outcome::Remote(RemoteExit::new(code)))
}

/// Fakes for the command tests: a scripted exec, remote and notes, and a session to run them as.
#[cfg(test)]
mod testing {
    use super::*;
    use crate::config::Session;
    use crate::domain::host::Host;
    use crate::domain::place::{EnvFile, RemoteDir};
    use crate::ports::fake::{FakeExec, FakeRemote, RecordedNotes, Reply};

    pub struct Fakes {
        pub exec: FakeExec,
        pub remote: FakeRemote,
        pub notes: RecordedNotes,
    }

    impl Fakes {
        pub fn new(
            execs: Vec<Reply>,
            exec_streams: Vec<i32>,
            captures: Vec<(i32, &str, &str)>,
            streams: Vec<i32>,
        ) -> Self {
            Self {
                exec: FakeExec::new(execs, exec_streams),
                remote: FakeRemote::new(captures, streams),
                notes: RecordedNotes::default(),
            }
        }

        /// A remote that only streams: the commands that never read anything first.
        pub fn remote_streaming(streams: Vec<i32>) -> Self {
            Self::new(vec![], vec![], vec![], streams)
        }

        pub fn ports(&self) -> Ports<'_> {
            Ports {
                exec: &self.exec,
                remote: &self.remote,
                notes: &self.notes,
            }
        }
    }

    /// Host `cluster`, env file `env.sh`, project directory `proj`.
    pub fn session() -> Session {
        Session {
            host: Host::parse("cluster").unwrap(),
            place: Place {
                env: Some(EnvFile::parse("env.sh").unwrap()),
                dir: Some(RemoteDir::parse("proj").unwrap()),
            },
        }
    }

    /// The text sent for `body` run in the project directory of [`session`], spelled out so the
    /// tests check the wrapper too.
    pub fn in_project(body: &str) -> String {
        format!("set -euo pipefail\nsource 'env.sh'\ncd 'proj'\n{body}")
    }

    /// The same for the remote home.
    pub fn at_home(body: &str) -> String {
        format!("set -euo pipefail\nsource 'env.sh'\n{body}")
    }
}
