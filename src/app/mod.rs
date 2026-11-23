//! One function per command: it takes its arguments and the ports, talks to the remote through
//! them, and returns an [`Outcome`]. It never prints and never picks an exit code; `main` renders
//! the outcome. Commands that stream a child's output first say what they did through
//! [`Notes`], because that text must come before the output.

use crate::domain::exit::RemoteExit;
use crate::domain::outcome::Outcome;
use crate::domain::place::{Place, script};
use crate::domain::shell::Script;
use crate::ports::{Clock, Exec, Notes, Remote, Terminal};
use anyhow::Result;

pub mod acct;
pub mod cancel;
mod copy;
pub mod exec;
pub mod fwd;
pub mod get;
mod git;
pub mod log;
pub mod pull;
pub mod put;
pub mod q;
pub mod recipe;
pub mod run;
pub mod status;
pub mod trust;
pub mod tunnel;
pub mod wait;

/// What a command may use besides its arguments.
pub struct Ports<'a> {
    pub exec: &'a dyn Exec,
    pub remote: &'a dyn Remote,
    pub notes: &'a dyn Notes,
    pub clock: &'a dyn Clock,
}

/// How a streamed command's output reaches the user.
#[derive(Clone, Copy)]
enum Output {
    /// The child's own bytes: `x` and `run`. The user chose that program, and a terminal session
    /// needs its raw bytes.
    Raw,
    /// The remote's own text (`q`, `acct`, `log`, `cancel`): it comes back line by line through `sanitize`
    /// and cannot carry a terminal control sequence.
    Filtered,
}

/// Run `body` at `place` on the remote with the terminal attached, and report how it ended.
fn stream(
    ports: &Ports,
    place: &Place,
    body: Script,
    terminal: Terminal,
    output: Output,
) -> Result<Outcome> {
    let script = script(place, body);
    let code = match output {
        Output::Raw => ports.remote.stream(&script, terminal),
        Output::Filtered => ports.remote.stream_filtered(&script, terminal),
    }?;
    Ok(Outcome::Remote(RemoteExit::new(code)))
}

/// Fakes for the command tests: a scripted exec, remote and notes, and a session to run them as.
#[cfg(test)]
mod testing {
    use super::*;
    use crate::config::Session;
    use crate::domain::host::Host;
    use crate::domain::place::{EnvFile, RemoteDir};
    use crate::ports::fake::{FakeClock, FakeExec, FakeRemote, RecordedNotes, Reply};

    pub struct Fakes {
        pub exec: FakeExec,
        pub remote: FakeRemote,
        pub notes: RecordedNotes,
        pub clock: FakeClock,
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
                clock: FakeClock::default(),
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
                clock: &self.clock,
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
            forwards: Default::default(),
            recipes: Default::default(),
        }
    }

    /// What a hostile job or file name prints: a title change, an OSC 52 clipboard write, a bidi
    /// override, a screen clear.
    pub const HOSTILE: &[u8] =
        "\u{1b}]0;t\u{7}ok\n\u{1b}]52;c;ZXZpbA==\u{7}x\nfi\u{202E}gnp\n\u{1b}[2J\n".as_bytes();

    /// What the user sees of [`HOSTILE`]: the text stays, every control character is a `?`.
    pub const HOSTILE_SHOWN: &str = "?]0;t?ok\n?]52;c;ZXZpbA==?x\nfi?gnp\n?[2J\n";

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
