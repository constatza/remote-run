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

use crate::domain::fwd::{Endpoint, ForwardTarget, argv};
use crate::domain::host::Host;
use crate::ports::{Child, Clock, Deadline, Ended, Exec, Net};
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
    /// `rr` was interrupted and closed it.
    Interrupted,
    /// ssh ended by itself with this code.
    Exited(i32),
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

/// What waiting for a forward to listen came to.
pub enum Started {
    Listening(ListeningForward),
    /// `rr` was interrupted first; ssh has been ended.
    Interrupted,
}

/// Whether ssh is still running at a look.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Running,
    Interrupted,
}

impl StartingForward {
    /// Start ssh for `endpoint` -> `target` through `host`. The command line is built here from
    /// those three values, so the endpoint that is polled and announced is the one ssh was given.
    pub fn start(
        exec: &dyn Exec,
        host: &Host,
        target: &ForwardTarget,
        endpoint: Endpoint,
    ) -> Result<Self> {
        Ok(Self {
            child: exec.spawn(&argv(host, &endpoint, target))?,
            endpoint,
        })
    }

    /// Wait until `endpoint` accepts and ssh is still alive after [`SETTLE`]. On any failure ssh
    /// is killed first, so an error never leaves a tunnel behind; a kill that fails is part of the
    /// error returned.
    pub fn wait_listening(mut self, net: &dyn Net, clock: &dyn Clock) -> Result<Started> {
        match self.poll_listening(net, clock) {
            Ok(Look::Running) => Ok(Started::Listening(ListeningForward {
                child: self.child,
                endpoint: self.endpoint,
            })),
            Ok(Look::Interrupted) => Ok(Started::Interrupted),
            Err(err) => Err(match self.child.kill() {
                Ok(()) => err,
                Err(kill) => err.context(format!("ssh could not be stopped either: {kill:#}")),
            }),
        }
    }

    fn poll_listening(&mut self, net: &dyn Net, clock: &dyn Clock) -> Result<Look> {
        let give_up = Deadline::after(clock.now(), START_LIMIT);
        loop {
            if self.look()? == Look::Interrupted {
                return Ok(Look::Interrupted);
            }
            if net.accepts(&self.endpoint) {
                clock.sleep(SETTLE);
                return self.look();
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

    /// An exit before the forward listens is an error.
    fn look(&mut self) -> Result<Look> {
        match self.child.try_wait()? {
            None => Ok(Look::Running),
            Some(Ended::Interrupted) => Ok(Look::Interrupted),
            Some(Ended::Exited(code)) => {
                bail!("ssh ended ({code}) before the forward was listening")
            }
        }
    }
}

impl ListeningForward {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Keep the forward until ssh ends or `until` passes; then kill ssh. When this returns ssh has
    /// been reaped (also on an error, as the handle is dropped), so a socket file can be removed.
    pub fn hold(mut self, until: Deadline, clock: &dyn Clock) -> Result<End> {
        loop {
            match self.child.try_wait()? {
                Some(Ended::Exited(code)) => return Ok(End::Exited(code)),
                Some(Ended::Interrupted) => return Ok(End::Interrupted),
                None => {}
            }
            if until.expired(clock.now()) {
                self.child.kill()?;
                return Ok(End::Expired);
            }
            clock.sleep(POLL);
        }
    }
}
