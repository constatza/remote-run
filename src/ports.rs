//! The seam to local processes: everything `rr` runs (ssh, rsync, git) goes through [`Exec`], so
//! tests can record the argv and replay output instead of starting anything.

// the vocabulary the ports speak is the core's
pub use crate::domain::argv::{Argv, Captured, Stdin};
use crate::domain::shell::Script;
pub use crate::domain::ssh::Terminal;
use anyhow::Result;
use std::time::Duration;

/// Runs local programs. A child killed by a signal has no exit code; implementations report it as
/// a non-zero `code`, never as success.
pub trait Exec {
    /// Run `argv` to the end, collecting its output.
    fn capture(&self, argv: &Argv, stdin: Stdin) -> Result<Captured>;
    /// Run `argv` with the terminal attached (stdin, stdout and stderr inherited).
    fn stream(&self, argv: &Argv) -> Result<i32>;
}

/// Runs scripts on the remote host; a trait so tests can record what would be sent. It takes a
/// [`Script`], never text, so nothing but quoted words and source literals reaches the remote
/// shell:
///
/// ```compile_fail,E0308
/// fn send(remote: &dyn remote_run::ports::Remote) {
///     let _ = remote.capture("rm -rf $HOME");
/// }
/// ```
pub trait Remote {
    /// Run `script`, collecting its output.
    fn capture(&self, script: &Script) -> Result<Captured>;
    /// Run `script` with the local terminal attached; `terminal` says whether the remote side gets
    /// one too.
    fn stream(&self, script: &Script, terminal: Terminal) -> Result<i32>;
}

/// A point on a monotonic clock: the time since that clock started. Only a [`Clock`] hands one out,
/// so two of them are comparable only when they come from the same clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(Duration);

impl Instant {
    /// Time from `earlier` to `self`; zero when `earlier` is not earlier.
    pub fn since(self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// The point `delay` after this one.
    pub fn after(self, delay: Duration) -> Instant {
        Instant(self.0.saturating_add(delay))
    }

    /// For the clocks: `elapsed` since the clock started.
    pub fn from_start(elapsed: Duration) -> Self {
        Self(elapsed)
    }
}

/// Time and sleeping, injected so that waiting runs in virtual time under test: a test that really
/// slept 30 seconds is a test nobody runs.
pub trait Clock {
    fn now(&self) -> Instant;
    /// Block for `delay`.
    fn sleep(&self, delay: Duration);
}

/// Where lines for the user go while a command is still running. A command that streams a child's
/// output (`run`, `log`) must say what it did first, before the child's output, so those lines
/// cannot wait for the [`Outcome`](crate::domain::outcome::Outcome) the command returns when it ends.
pub trait Notes {
    /// One line for the user's standard error.
    fn note(&self, line: &str);
}

/// A scripted [`Exec`] for tests: it records every call and answers from queues.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    /// How a recorded call ran.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum How {
        Capture(Stdin),
        Stream,
    }

    /// A scripted answer to `capture`.
    pub enum Reply {
        Output(Captured),
        /// The program could not be started.
        WontStart,
    }

    pub fn output(code: i32, stdout: &str, stderr: &str) -> Reply {
        Reply::Output(Captured {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    /// How a [`FakeRemote`] was called.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RemoteCall {
        Capture,
        Stream(Terminal),
    }

    /// A scripted [`Remote`]: it records every script and answers from queues. A call with nothing
    /// queued panics, so a command that must not reach the remote fails loudly.
    #[derive(Default)]
    pub struct FakeRemote {
        captures: RefCell<VecDeque<Captured>>,
        streams: RefCell<VecDeque<i32>>,
        pub calls: RefCell<Vec<(String, RemoteCall)>>,
    }

    impl FakeRemote {
        /// Answers `captures` as `(code, stdout, stderr)` in order, and `streams` as exit codes.
        pub fn new(captures: Vec<(i32, &str, &str)>, streams: Vec<i32>) -> Self {
            let captures = captures
                .into_iter()
                .map(|(code, stdout, stderr)| Captured {
                    code,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                })
                .collect();
            Self {
                captures: RefCell::new(captures),
                streams: RefCell::new(streams.into()),
                calls: RefCell::default(),
            }
        }

        /// The script of every call, in order.
        pub fn scripts(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(script, _)| script.clone())
                .collect()
        }
    }

    impl Remote for FakeRemote {
        fn capture(&self, script: &Script) -> Result<Captured> {
            self.calls
                .borrow_mut()
                .push((script.to_string(), RemoteCall::Capture));
            Ok(self
                .captures
                .borrow_mut()
                .pop_front()
                .expect("an unexpected remote capture"))
        }

        fn stream(&self, script: &Script, terminal: Terminal) -> Result<i32> {
            self.calls
                .borrow_mut()
                .push((script.to_string(), RemoteCall::Stream(terminal)));
            Ok(self
                .streams
                .borrow_mut()
                .pop_front()
                .expect("an unexpected remote stream"))
        }
    }

    /// A [`Clock`] that moves only when something sleeps on it, by exactly that long.
    #[derive(Default)]
    pub struct FakeClock {
        elapsed: Cell<Duration>,
        pub slept: RefCell<Vec<Duration>>,
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            Instant::from_start(self.elapsed.get())
        }

        fn sleep(&self, delay: Duration) {
            self.elapsed.set(self.elapsed.get() + delay);
            self.slept.borrow_mut().push(delay);
        }
    }

    /// A [`Notes`] that keeps the lines.
    #[derive(Default)]
    pub struct RecordedNotes(pub RefCell<Vec<String>>);

    impl Notes for RecordedNotes {
        fn note(&self, line: &str) {
            self.0.borrow_mut().push(line.to_string());
        }
    }

    #[derive(Default)]
    pub struct FakeExec {
        captures: RefCell<VecDeque<Reply>>,
        streams: RefCell<VecDeque<i32>>,
        pub calls: RefCell<Vec<(Argv, How)>>,
    }

    impl FakeExec {
        pub fn new(captures: Vec<Reply>, streams: Vec<i32>) -> Self {
            Self {
                captures: RefCell::new(captures.into()),
                streams: RefCell::new(streams.into()),
                calls: RefCell::default(),
            }
        }

        /// The program and arguments of every call, as words.
        pub fn words(&self) -> Vec<Vec<String>> {
            self.calls
                .borrow()
                .iter()
                .map(|(argv, _)| {
                    let mut words = vec![argv.program().to_string()];
                    words.extend(argv.args().iter().cloned());
                    words
                })
                .collect()
        }
    }

    impl Exec for FakeExec {
        fn capture(&self, argv: &Argv, stdin: Stdin) -> Result<Captured> {
            self.calls
                .borrow_mut()
                .push((argv.clone(), How::Capture(stdin)));
            match self.captures.borrow_mut().pop_front() {
                Some(Reply::Output(captured)) => Ok(captured),
                Some(Reply::WontStart) => anyhow::bail!("could not start {}", argv.program()),
                None => panic!("an unexpected capture of {}", argv.program()),
            }
        }

        fn stream(&self, argv: &Argv) -> Result<i32> {
            self.calls.borrow_mut().push((argv.clone(), How::Stream));
            Ok(self
                .streams
                .borrow_mut()
                .pop_front()
                .expect("an unexpected stream"))
        }
    }

    #[test]
    fn a_fake_clock_moves_only_by_sleeping_and_by_exactly_that_long() {
        let clock = fake::FakeClock::default();
        let start = clock.now();
        assert_eq!(clock.now(), start, "time does not pass by itself");
        clock.sleep(Duration::from_secs(30));
        clock.sleep(Duration::from_secs(5));
        let later = clock.now();
        assert_eq!(later.since(start), Duration::from_secs(35));
        assert_eq!(start.since(later), Duration::ZERO);
        assert_eq!(start.after(Duration::from_secs(35)), later);
        assert_eq!(
            *clock.slept.borrow(),
            [Duration::from_secs(30), Duration::from_secs(5)]
        );
    }
}
