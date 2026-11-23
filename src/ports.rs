//! The seam to local processes: everything `rr` runs (ssh, rsync, git) goes through [`Exec`], so
//! tests can record the argv and replay output instead of starting anything.

// the vocabulary the ports speak is the core's
pub use crate::domain::argv::{Argv, Captured, Stdin};
use crate::domain::sanitize::sanitize_bytes;
use crate::domain::shell::Script;
pub use crate::domain::ssh::Terminal;
use anyhow::Result;
use std::io::{self, BufRead, Read, Write};
use std::time::Duration;

/// The most bytes of one line handled at once. A child that never prints a newline would otherwise
/// grow the buffer without bound; its text is then passed on in pieces of this size.
pub const MAX_LINE_BYTES: u64 = 64 * 1024;

/// Copy `reader` to `writer` one line at a time, each through
/// [`sanitize_bytes`], and flush after every piece so a followed log appears as it grows. A line
/// longer than [`MAX_LINE_BYTES`] is passed on in pieces; a cut can split a multi-byte character,
/// which then shows as U+FFFD.
pub fn write_sanitized(reader: impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut reader = io::BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        if (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        writer.write_all(sanitize_bytes(&line).as_bytes())?;
        writer.flush()?;
    }
}

/// Runs local programs. A child killed by a signal has no exit code; implementations report it as
/// a non-zero `code`, never as success.
pub trait Exec {
    /// Run `argv` to the end, collecting its output.
    fn capture(&self, argv: &Argv, stdin: Stdin) -> Result<Captured>;
    /// Run `argv` with the terminal attached (stdin, stdout and stderr inherited).
    fn stream(&self, argv: &Argv) -> Result<i32>;
    /// Like [`Exec::stream`], but the child's stdout and stderr come back through
    /// [`write_sanitized`] to the user's stdout and stderr, so nothing it prints can carry a
    /// terminal control sequence. Stdin stays inherited.
    fn stream_filtered(&self, argv: &Argv) -> Result<i32>;
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
    /// Like [`Remote::stream`], with the output filtered as in [`Exec::stream_filtered`]. For
    /// commands whose output is the remote's text rather than a program the user chose to run.
    fn stream_filtered(&self, script: &Script, terminal: Terminal) -> Result<i32>;
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
        StreamFiltered,
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
        StreamFiltered(Terminal),
    }

    /// A scripted [`Remote`]: it records every script and answers from queues. A call with nothing
    /// queued panics, so a command that must not reach the remote fails loudly.
    #[derive(Default)]
    pub struct FakeRemote {
        captures: RefCell<VecDeque<Captured>>,
        streams: RefCell<VecDeque<i32>>,
        emitted: RefCell<VecDeque<Vec<u8>>>,
        /// What the user would have seen from the filtered streams, after the filter.
        pub shown: RefCell<Vec<u8>>,
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
                emitted: RefCell::default(),
                shown: RefCell::default(),
                calls: RefCell::default(),
            }
        }

        /// The raw bytes the next filtered stream's child prints; a call without one prints
        /// nothing.
        pub fn emit(&self, raw: &[u8]) {
            self.emitted.borrow_mut().push_back(raw.to_vec());
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

        fn stream_filtered(&self, script: &Script, terminal: Terminal) -> Result<i32> {
            self.calls
                .borrow_mut()
                .push((script.to_string(), RemoteCall::StreamFiltered(terminal)));
            let raw = self.emitted.borrow_mut().pop_front().unwrap_or_default();
            write_sanitized(raw.as_slice(), &mut *self.shown.borrow_mut())?;
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

        fn stream_filtered(&self, argv: &Argv) -> Result<i32> {
            self.calls
                .borrow_mut()
                .push((argv.clone(), How::StreamFiltered));
            Ok(self
                .streams
                .borrow_mut()
                .pop_front()
                .expect("an unexpected stream"))
        }
    }

    fn sanitized(raw: &[u8]) -> String {
        let mut shown = Vec::new();
        write_sanitized(raw, &mut shown).unwrap();
        String::from_utf8(shown).unwrap()
    }

    #[test]
    fn escape_sequences_clipboard_writes_and_bidi_controls_never_get_through() {
        let title = "\u{1b}]0;owned\u{7}";
        let clipboard = "\u{1b}]52;c;ZXZpbA==\u{1b}\\";
        let reorder = "fi\u{202E}gnp.exe\u{2066}";
        let raw = format!("{title}one\n{clipboard}two\n{reorder}\n");
        let shown = sanitized(raw.as_bytes());
        assert_eq!(shown.lines().count(), 3, "the lines stay lines");
        for c in ['\u{1b}', '\u{7}', '\u{202E}', '\u{2066}'] {
            assert!(!shown.contains(c), "{c:?} in {shown:?}");
        }
        assert!(shown.contains("one") && shown.contains("two"));
    }

    #[test]
    fn bad_utf8_and_a_missing_final_newline_are_handled() {
        assert_eq!(
            sanitized(b"caf\xe9\nna\xc3\xafve ok\ntail"),
            "caf\u{FFFD}\nna\u{ef}ve ok\ntail"
        );
        assert_eq!(sanitized(b""), "");
    }

    #[test]
    fn a_long_line_is_passed_on_in_bounded_pieces_and_a_read_error_is_an_error() {
        let long = vec![b'a'; 3 * MAX_LINE_BYTES as usize + 5];
        assert_eq!(sanitized(&long).len(), long.len());
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        assert!(write_sanitized(Broken, &mut Vec::new()).is_err());
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
