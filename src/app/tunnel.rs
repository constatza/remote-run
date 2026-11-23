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

/// How long ssh must stay alive once it holds the listener before it counts as listening: it
/// exits at once when it fails after binding (`ExitOnForwardFailure`).
const SETTLE: Duration = Duration::from_millis(500);

/// ssh could not be stopped: it may still be running and may still own its socket file. A value
/// in the error's chain, so the caller can tell this failure from the others by type.
#[derive(Debug)]
pub struct NotStopped(String);

impl NotStopped {
    fn new(kill: &anyhow::Error) -> Self {
        Self(format!("{kill:#}"))
    }
}

impl std::fmt::Display for NotStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ssh could not be stopped ({})", self.0)
    }
}

impl std::error::Error for NotStopped {}

/// An error after which ssh is known not to run: it was reaped, it ended by itself, or it never
/// started. The caller keeps a socket file on any error without this marker, so a new failure
/// path is safe until it proves ssh gone.
#[derive(Debug)]
pub struct SshEnded(String);

impl SshEnded {
    pub fn error(message: impl std::fmt::Display) -> anyhow::Error {
        anyhow::Error::new(Self(message.to_string()))
    }
}

impl std::fmt::Display for SshEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SshEnded {}

/// Kill `child` for the sake of `err`: `err` comes back marked [`SshEnded`] when the kill worked,
/// and with [`NotStopped`] on top when it did not.
fn stop_after(child: &mut dyn Child, err: &anyhow::Error) -> anyhow::Error {
    match child.kill() {
        Ok(()) => SshEnded::error(format!("{err:#}")),
        Err(kill) => anyhow::anyhow!("{err:#}").context(NotStopped::new(&kill)),
    }
}

/// Kill `child`, then report `end`; a kill that fails is [`NotStopped`].
fn stop_for(child: &mut dyn Child, end: End) -> Result<End> {
    child
        .kill()
        .map(|()| end)
        .map_err(|kill| anyhow::Error::new(NotStopped::new(&kill)))
}

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
        let child = exec
            .spawn(&argv(host, &endpoint, target))
            .map_err(|err| SshEnded::error(format!("{err:#}")))?;
        Ok(Self { child, endpoint })
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
            // an interrupt has ended ssh through the child's own handler, but only a kill that
            // succeeds here says it is gone
            Ok(Look::Interrupted) => {
                stop_for(&mut *self.child, End::Interrupted).map(|_| Started::Interrupted)
            }
            Err(err) => Err(stop_after(&mut *self.child, &err)),
        }
    }

    fn poll_listening(&mut self, net: &dyn Net, clock: &dyn Clock) -> Result<Look> {
        let give_up = Deadline::after(clock.now(), START_LIMIT);
        loop {
            if self.look()? == Look::Interrupted {
                return Ok(Look::Interrupted);
            }
            if self.is_ssh_listening(net) {
                clock.sleep(SETTLE);
                return self.look();
            }
            if give_up.expired(clock.now()) {
                // a port that answers without being ssh's is the race this check closes
                let foreign = if net.accepts(&self.endpoint) {
                    ": the port answers, but its listener is not ssh's"
                } else {
                    ""
                };
                bail!(
                    "ssh did not open the forward within {}s{foreign}",
                    START_LIMIT.as_secs()
                );
            }
            clock.sleep(POLL);
        }
    }

    /// Whether the endpoint accepts and, for a TCP port, the listener is ssh's own: a connect
    /// succeeds against any listener, including one that took the port between `rr` choosing it
    /// and ssh binding it, so only a listener held by ssh's pid counts. A socket path was absent
    /// before ssh started and lies in the private runtime directory, so it needs no owner.
    fn is_ssh_listening(&self, net: &dyn Net) -> bool {
        if !net.accepts(&self.endpoint) {
            return false;
        }
        match &self.endpoint {
            Endpoint::Tcp(port) => net.held_by(*port, self.child.pid()),
            Endpoint::Unix(_) => true,
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
    /// been reaped, except in an error that has [`NotStopped`] in its chain.
    pub fn hold(mut self, until: Deadline, clock: &dyn Clock) -> Result<End> {
        loop {
            match self.child.try_wait() {
                Ok(Some(Ended::Exited(code))) => return Ok(End::Exited(code)),
                Ok(Some(Ended::Interrupted)) => {
                    return stop_for(&mut *self.child, End::Interrupted);
                }
                Ok(None) => {}
                Err(err) => return Err(stop_after(&mut *self.child, &err)),
            }
            if until.expired(clock.now()) {
                return stop_for(&mut *self.child, End::Expired);
            }
            clock.sleep(POLL);
        }
    }
}
