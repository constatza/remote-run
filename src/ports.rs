//! The seam to local processes: everything `rr` runs (ssh, rsync, git) goes through [`Exec`], so
//! tests can record the argv and replay output instead of starting anything.

// the vocabulary the ports speak is the core's
pub use crate::domain::argv::{Argv, Captured, Stdin};
use crate::domain::fwd::{Endpoint, LoopbackPort, SocketPath, Ttl};
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
    /// Start `argv` without waiting for it: no stdin, no stdout, its stderr filtered like
    /// [`Exec::stream_filtered`]. For a program that is meant to keep running (an ssh tunnel); the
    /// child ends when the handle is dropped, and when `rr` is killed or interrupted.
    fn spawn(&self, argv: &Argv) -> Result<Box<dyn Child>>;
}

/// A program started by [`Exec::spawn`].
pub trait Child {
    /// Its exit code once it has ended (a child killed by a signal reads as non-zero, never as
    /// success), else `None` while it runs.
    fn try_wait(&mut self) -> Result<Option<i32>>;
    /// End it and reap it. Ending a child that has already ended is not an error.
    fn kill(&mut self) -> Result<()>;
}

/// The local network and the files a forward needs: asking a question about the machine, never
/// changing it.
pub trait Net {
    /// A TCP port on loopback that nothing listens on: `want` when given (an error if taken),
    /// else any free one. Another process can still take it before ssh binds; ssh then fails
    /// (`ExitOnForwardFailure`), which `rr` reports.
    fn free_port(&self, want: Option<LoopbackPort>) -> Result<LoopbackPort>;
    /// An error when something already exists at `path`: a stale socket would make ssh fail to
    /// bind, and removing a file that is not ours is not `rr`'s call.
    fn socket_unused(&self, path: &SocketPath) -> Result<()>;
    /// Whether a connection to `endpoint` is accepted right now.
    fn accepts(&self, endpoint: &Endpoint) -> bool;
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

/// The point at which a forward must be over: its start plus its [`Ttl`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(start: Instant, ttl: Ttl) -> Self {
        Self(start.after(ttl.duration()))
    }

    pub fn expired(self, now: Instant) -> bool {
        now >= self.0
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
    use std::rc::Rc;

    /// How a recorded call ran.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum How {
        Capture(Stdin),
        Stream,
        StreamFiltered,
        Spawn,
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

    /// A child that answers `try_wait` from a script, repeating the last answer, and counts kills.
    pub struct FakeChild {
        waits: VecDeque<Option<i32>>,
        killed: Rc<Cell<u32>>,
    }

    impl FakeChild {
        /// `waits` are the answers of successive `try_wait` calls; once killed, it has ended.
        pub fn new(waits: Vec<Option<i32>>) -> (Self, Rc<Cell<u32>>) {
            let killed = Rc::new(Cell::new(0));
            let child = Self {
                waits: waits.into(),
                killed: Rc::clone(&killed),
            };
            (child, killed)
        }
    }

    /// The code a killed fake child reads as: 128 + SIGKILL.
    pub const KILLED_EXIT: i32 = 137;

    impl Child for FakeChild {
        fn try_wait(&mut self) -> Result<Option<i32>> {
            if self.killed.get() > 0 {
                return Ok(Some(KILLED_EXIT));
            }
            Ok(match self.waits.len() {
                0 => None,
                1 => self.waits[0],
                _ => self.waits.pop_front().flatten(),
            })
        }

        fn kill(&mut self) -> Result<()> {
            self.killed.set(self.killed.get() + 1);
            Ok(())
        }
    }

    /// A scripted [`Net`]: `free` is the port it hands out (`None`: every port is taken),
    /// `accepts` answers successive checks, repeating the last, and `socket_taken` says whether a
    /// file is in the way.
    pub struct FakeNet {
        pub free: Option<u16>,
        pub accepts: RefCell<VecDeque<bool>>,
        pub socket_taken: bool,
    }

    impl FakeNet {
        pub fn new(free: Option<u16>, accepts: Vec<bool>) -> Self {
            Self {
                free,
                accepts: RefCell::new(accepts.into()),
                socket_taken: false,
            }
        }
    }

    impl Net for FakeNet {
        fn free_port(&self, want: Option<LoopbackPort>) -> Result<LoopbackPort> {
            match (self.free, want) {
                (None, _) => anyhow::bail!("port in use"),
                (Some(_), Some(want)) => Ok(want),
                (Some(free), None) => LoopbackPort::from_u16(free),
            }
        }

        fn socket_unused(&self, path: &SocketPath) -> Result<()> {
            if self.socket_taken {
                anyhow::bail!("{} exists", path.as_str());
            }
            Ok(())
        }

        fn accepts(&self, _: &Endpoint) -> bool {
            let mut answers = self.accepts.borrow_mut();
            if answers.len() > 1 {
                answers.pop_front().unwrap_or_default()
            } else {
                answers.front().copied().unwrap_or_default()
            }
        }
    }

    #[derive(Default)]
    pub struct FakeExec {
        captures: RefCell<VecDeque<Reply>>,
        streams: RefCell<VecDeque<i32>>,
        children: RefCell<VecDeque<FakeChild>>,
        pub calls: RefCell<Vec<(Argv, How)>>,
    }

    impl FakeExec {
        pub fn new(captures: Vec<Reply>, streams: Vec<i32>) -> Self {
            Self {
                captures: RefCell::new(captures.into()),
                streams: RefCell::new(streams.into()),
                children: RefCell::default(),
                calls: RefCell::default(),
            }
        }

        /// The children that `spawn` hands out, in order.
        pub fn with_children(self, children: Vec<FakeChild>) -> Self {
            *self.children.borrow_mut() = children.into();
            self
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

        fn spawn(&self, argv: &Argv) -> Result<Box<dyn Child>> {
            self.calls.borrow_mut().push((argv.clone(), How::Spawn));
            let child = self.children.borrow_mut().pop_front();
            Ok(Box::new(child.expect("an unexpected spawn")))
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
    fn a_bidi_override_split_by_the_line_cap_does_not_survive() {
        let mut raw = vec![b'a'; MAX_LINE_BYTES as usize - 1];
        raw.extend_from_slice("\u{202E}b\n".as_bytes());
        let shown = sanitized(&raw);
        assert!(!shown.contains('\u{202E}'));
        assert!(shown.ends_with("\u{FFFD}\u{FFFD}\u{FFFD}b\n"), "{shown:?}");
    }

    /// Records the size of every `write_all` and counts the flushes.
    #[derive(Default)]
    struct Counting {
        writes: Vec<usize>,
        flushes: usize,
    }
    impl Write for Counting {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.push(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn every_write_is_bounded_and_each_line_is_flushed_once() {
        // a bad byte grows to the three bytes of U+FFFD, so a piece may be 3 times the cap
        let widest = 3 * MAX_LINE_BYTES as usize;
        let mut raw = b"one\ntwo\r\n".to_vec();
        raw.extend(vec![0xff; 2 * MAX_LINE_BYTES as usize + 1]);
        raw.extend_from_slice(b"\nlast");
        let mut counting = Counting::default();
        write_sanitized(raw.as_slice(), &mut counting).unwrap();
        // one piece per line, three pieces for the long run of bad bytes (the last holds the
        // single byte left over and the newline), and the unterminated tail
        assert_eq!(counting.writes.len(), 2 + 3 + 1);
        assert!(
            counting.writes.iter().all(|&n| n <= widest),
            "{:?}",
            counting.writes
        );
        assert_eq!(counting.flushes, counting.writes.len());
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
