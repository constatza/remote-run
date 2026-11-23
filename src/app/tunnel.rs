//! The life of one ssh forward as types: started, then listening, then over. Only a
//! [`ListeningForward`] can be announced or held, and only [`StartingForward::wait_listening`]
//! makes one, so a URL cannot be printed for a tunnel that was never seen working.
//!
//! ```compile_fail,E0599
//! use remote_run::app::tunnel::StartingForward;
//! fn announce(starting: StartingForward) {
//!     let _ = starting.endpoint();
//! }
//! ```
//!
//! ```compile_fail,E0599
//! use remote_run::app::tunnel::StartingForward;
//! use remote_run::ports::{Clock, Deadline};
//! fn hold(starting: StartingForward, until: Deadline, clock: &dyn Clock) {
//!     let _ = starting.hold(until, clock);
//! }
//! ```

use crate::domain::fwd::Endpoint;
use crate::ports::{Argv, Child, Clock, Deadline, Exec, Net};
use anyhow::{Result, bail};
use std::time::Duration;

/// How often a starting or running forward is looked at.
const POLL: Duration = Duration::from_millis(200);

/// How long ssh gets to authenticate and bind before `rr` gives up on it.
const START_LIMIT: Duration = Duration::from_secs(30);

/// How long a port that accepts must keep ssh alive before it counts as listening. A foreign
/// process listening on the port makes the connection succeed while ssh is still failing to bind,
/// and ssh then exits (`ExitOnForwardFailure`).
const SETTLE: Duration = Duration::from_millis(500);

/// How a held forward ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Its time to live ran out and `rr` closed it.
    Expired,
    /// ssh ended by itself (or `rr` was interrupted) with this code.
    Ssh(i32),
}

/// ssh is running; nothing is known about the forward yet.
pub struct StartingForward {
    child: Box<dyn Child>,
    endpoint: Endpoint,
}

/// ssh has stayed alive with `endpoint` accepting connections.
pub struct ListeningForward {
    child: Box<dyn Child>,
    endpoint: Endpoint,
}

impl StartingForward {
    pub fn start(exec: &dyn Exec, argv: &Argv, endpoint: Endpoint) -> Result<Self> {
        Ok(Self {
            child: exec.spawn(argv)?,
            endpoint,
        })
    }

    /// Wait until `endpoint` accepts and ssh is still alive after [`SETTLE`]. On any failure ssh
    /// is killed first, so an error never leaves a tunnel behind.
    pub fn wait_listening(mut self, net: &dyn Net, clock: &dyn Clock) -> Result<ListeningForward> {
        match self.poll_listening(net, clock) {
            Ok(()) => Ok(ListeningForward {
                child: self.child,
                endpoint: self.endpoint,
            }),
            Err(err) => {
                let _ = self.child.kill();
                Err(err)
            }
        }
    }

    fn poll_listening(&mut self, net: &dyn Net, clock: &dyn Clock) -> Result<()> {
        let give_up = Deadline::after(clock.now(), START_LIMIT);
        loop {
            self.require_alive()?;
            if net.accepts(&self.endpoint) {
                clock.sleep(SETTLE);
                return self.require_alive();
            }
            if give_up.expired(clock.now()) {
                bail!(
                    "ssh did not open the forward within {}s",
                    START_LIMIT.as_secs()
                );
            }
            clock.sleep(POLL);
        }
    }

    fn require_alive(&mut self) -> Result<()> {
        match self.child.try_wait()? {
            None => Ok(()),
            Some(code) => bail!("ssh ended ({code}) before the forward was listening"),
        }
    }
}

impl ListeningForward {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Keep the forward until ssh ends or `until` passes; then kill ssh.
    pub fn hold(mut self, until: Deadline, clock: &dyn Clock) -> Result<End> {
        loop {
            if let Some(code) = self.child.try_wait()? {
                return Ok(End::Ssh(code));
            }
            if until.expired(clock.now()) {
                self.child.kill()?;
                return Ok(End::Expired);
            }
            clock.sleep(POLL);
        }
    }
}
