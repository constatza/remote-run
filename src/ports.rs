//! The seam to local processes: everything `rr` runs (ssh, rsync, git) goes through [`Exec`], so
//! tests can record the argv and replay output instead of starting anything.

// the vocabulary the ports speak is the core's
pub use crate::domain::argv::{Argv, Captured, Stdin};
use crate::domain::fwd::{Endpoint, LoopbackPort, SocketPath};
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

/// How a command that was shown raw and read at the same time ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Teed {
    pub code: i32,
    /// The output lines that began with the asked-for prefix, without their line ending. Only
    /// these are kept, so a long-running command's output is not held in memory.
    pub matched: Vec<String>,
}

/// Which of the user's streams a tee shows the child's output on. Under `--json` stdout is one
/// document, so the output of a command that is run and then waited for goes to stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    Stdout,
    Stderr,
}

/// A tee that stopped on an I/O error, with the lines that matched before it: they may be job ids
/// the user needs to find out about.
#[derive(Debug)]
pub struct TeeError {
    pub matched: Vec<String>,
    pub source: io::Error,
}

impl std::fmt::Display for TeeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for TeeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Copy `reader` to `writer` unchanged, a line at a time (flushed, like [`write_sanitized`]), and
/// return the lines that start with `prefix`. A line longer than [`MAX_LINE_BYTES`] is read in
/// pieces, so its later pieces are not seen as line starts. When the writer fails the reader is
/// still read to its end and matched, without echoing, so a job id printed after the failure is
/// not lost; the first write error is returned then, with every matched line.
pub fn tee_lines(
    reader: impl Read,
    writer: &mut impl Write,
    prefix: &str,
) -> Result<Vec<String>, TeeError> {
    let mut matched = Vec::new();
    match tee_into(reader, writer, prefix, &mut matched) {
        Ok(()) => Ok(matched),
        Err(source) => Err(TeeError { matched, source }),
    }
}

fn tee_into(
    reader: impl Read,
    writer: &mut impl Write,
    prefix: &str,
    matched: &mut Vec<String>,
) -> io::Result<()> {
    let mut reader = io::BufReader::new(reader);
    let mut write_failed: Option<io::Error> = None;
    let mut line = Vec::new();
    let mut at_line_start = true;
    loop {
        line.clear();
        if (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return write_failed.map_or(Ok(()), Err);
        }
        // recorded before the echo: a failing write on the matching line itself must still hand
        // back its job id
        if at_line_start && line.starts_with(prefix.as_bytes()) {
            let text = String::from_utf8_lossy(&line);
            matched.push(text.trim_end_matches(['\r', '\n']).to_string());
        }
        if write_failed.is_none()
            && let Err(source) = writer.write_all(&line).and_then(|()| writer.flush())
        {
            write_failed = Some(source);
        }
        at_line_start = line.ends_with(b"\n");
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
    /// Like [`Exec::stream`], but the child's stdout is shown unchanged on `echo` while it is also
    /// read: the lines that start with `prefix` come back in [`Teed`]. Stdin and the child's stderr
    /// stay inherited.
    fn stream_tee(&self, argv: &Argv, prefix: &str, echo: Echo) -> Result<Teed>;
    /// Start `argv` without waiting for it: no stdin, no stdout, its stderr filtered like
    /// [`Exec::stream_filtered`]. For a program that is meant to keep running (an ssh tunnel); the
    /// child ends when the handle is dropped, and when `rr` is killed or interrupted.
    fn spawn(&self, argv: &Argv) -> Result<Box<dyn Child>>;
    /// Start `argv` and let it go: no stdin, stdout or stderr, its own process group (so a Ctrl-C
    /// at `rr` does not reach it), and nobody waits for its result. For a program that is the
    /// user's, such as a browser, which must neither block `rr` nor end with it. An `Err` means it
    /// could not be started.
    fn launch(&self, argv: &Argv) -> Result<()>;
}

/// How a [`Child`] ended. An interrupt of `rr` is not an exit of the child: the child was ended
/// by `rr`, so its code is not a result worth passing on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The child ended with this code (a child killed by a signal reads as non-zero, never as
    /// success).
    Exited(i32),
    /// `rr` was interrupted (SIGINT, SIGTERM or SIGHUP) and has ended the child.
    Interrupted,
}

/// A program started by [`Exec::spawn`].
pub trait Child {
    /// Its process id, which names the sockets it holds (see [`Net::held_by`]).
    fn pid(&self) -> u32;
    /// How it ended, or `None` while it runs.
    fn try_wait(&mut self) -> Result<Option<Ended>>;
    /// End it and reap it. Ending a child that has already ended is not an error.
    fn kill(&mut self) -> Result<()>;
}

/// Whose a TCP listener on the forwarded port is, as far as `rr` could establish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// The process asked about holds the listening socket.
    Held,
    /// The tables were read, and another process holds it (or none does).
    Foreign,
    /// The tables could not be read or the system cannot tell; the text says why. Not a proof of
    /// either: a caller that announces only on [`Ownership::Held`] fails closed.
    Unknown(String),
}

/// The local network and the files a forward needs.
pub trait Net {
    /// A TCP port on loopback that nothing listens on: `want` when given (an error if taken),
    /// else any free one. Another process can still take it before ssh binds, so a listener on
    /// it proves nothing until [`Net::held_by`] says ssh holds it.
    fn free_port(&self, want: Option<LoopbackPort>) -> Result<LoopbackPort>;
    /// An error when something already exists at `path`: a stale socket would make ssh fail to
    /// bind, and removing a file that is not ours is not `rr`'s call.
    fn socket_unused(&self, path: &SocketPath) -> Result<()>;
    /// Remove the socket file at `path` (none there is fine). Only for a path that
    /// [`Net::socket_unused`] found free before ssh was started, once ssh has been reaped: the
    /// file is then ssh's, and a SIGKILLed ssh cannot unlink it.
    fn remove_socket(&self, path: &SocketPath) -> Result<()>;
    /// Whether a connection to `endpoint` is accepted right now. Any listener answers, so for a TCP
    /// port this alone does not say whose it is: see [`Net::held_by`].
    fn accepts(&self, endpoint: &Endpoint) -> bool;
    /// Whether the process `pid` holds the listening socket on `127.0.0.1:port`. A read error is
    /// [`Ownership::Unknown`] with its cause, never [`Ownership::Held`], so a caller that
    /// announces only on `Held` fails closed and can still say why.
    fn held_by(&self, port: LoopbackPort, pid: u32) -> Ownership;
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
    /// Like [`Remote::capture`], for a short read that must come back (`rr wait` and `rr status`):
    /// a silent link ends the call instead of blocking it for ever.
    fn poll(&self, script: &Script) -> Result<Captured>;
    /// Run `script` with the local terminal attached; `terminal` says whether the remote side gets
    /// one too.
    fn stream(&self, script: &Script, terminal: Terminal) -> Result<i32>;
    /// Like [`Remote::stream`], with the output filtered as in [`Exec::stream_filtered`]. For
    /// commands whose output is the remote's text rather than a program the user chose to run.
    fn stream_filtered(&self, script: &Script, terminal: Terminal) -> Result<i32>;
    /// Like [`Remote::stream`], with the output also read as in [`Exec::stream_tee`].
    fn stream_tee(
        &self,
        script: &Script,
        terminal: Terminal,
        prefix: &str,
        echo: Echo,
    ) -> Result<Teed>;
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

/// The point at which something must be over: its start plus how long it may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(start: Instant, limit: Duration) -> Self {
        Self(start.after(limit))
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
        StreamTee,
        Spawn,
        Launch,
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
        Poll,
        Stream(Terminal),
        StreamFiltered(Terminal),
        StreamTee(Terminal, Echo),
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

        fn poll(&self, script: &Script) -> Result<Captured> {
            self.calls
                .borrow_mut()
                .push((script.to_string(), RemoteCall::Poll));
            Ok(self
                .captures
                .borrow_mut()
                .pop_front()
                .expect("an unexpected remote poll"))
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

        /// What the command prints is what [`FakeRemote::emit`] queued; `shown` gets it unchanged.
        fn stream_tee(
            &self,
            script: &Script,
            terminal: Terminal,
            prefix: &str,
            echo: Echo,
        ) -> Result<Teed> {
            self.calls
                .borrow_mut()
                .push((script.to_string(), RemoteCall::StreamTee(terminal, echo)));
            let raw = self.emitted.borrow_mut().pop_front().unwrap_or_default();
            let matched = tee_lines(raw.as_slice(), &mut *self.shown.borrow_mut(), prefix)?;
            let code = self
                .streams
                .borrow_mut()
                .pop_front()
                .expect("an unexpected remote stream");
            Ok(Teed { code, matched })
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
        waits: VecDeque<Option<Ended>>,
        killed: Rc<Cell<u32>>,
        kill_fails: bool,
        /// `try_wait` fails from this call on (counted from 0), until the child is killed.
        wait_fails_from: Option<u32>,
        waited: u32,
    }

    impl FakeChild {
        /// `waits` are the answers of successive `try_wait` calls; once killed, it has ended.
        pub fn new(waits: Vec<Option<i32>>) -> (Self, Rc<Cell<u32>>) {
            Self::scripted(waits.into_iter().map(|w| w.map(Ended::Exited)).collect())
        }

        /// Like [`FakeChild::new`], for answers that are not an exit code.
        pub fn scripted(waits: Vec<Option<Ended>>) -> (Self, Rc<Cell<u32>>) {
            let killed = Rc::new(Cell::new(0));
            let child = Self {
                waits: waits.into(),
                killed: Rc::clone(&killed),
                kill_fails: false,
                wait_fails_from: None,
                waited: 0,
            };
            (child, killed)
        }

        /// Make `try_wait` fail from its `call`th call on (counted from 0).
        pub fn with_failing_wait_from(mut self, call: u32) -> Self {
            self.wait_fails_from = Some(call);
            self
        }

        /// Make `kill` fail (after it has been counted), as for a child that cannot be signalled.
        pub fn with_failing_kill(mut self) -> Self {
            self.kill_fails = true;
            self
        }
    }

    /// The code a killed fake child reads as: 128 + SIGKILL.
    const KILLED_EXIT: i32 = 137;

    /// The pid every [`FakeChild`] has.
    pub const FAKE_CHILD_PID: u32 = 4242;

    impl Child for FakeChild {
        fn pid(&self) -> u32 {
            FAKE_CHILD_PID
        }

        fn try_wait(&mut self) -> Result<Option<Ended>> {
            if self.killed.get() > 0 {
                return Ok(Some(Ended::Exited(KILLED_EXIT)));
            }
            self.waited += 1;
            if self.wait_fails_from.is_some_and(|from| self.waited > from) {
                anyhow::bail!("cannot poll the child");
            }
            Ok(match self.waits.len() {
                0 => None,
                1 => self.waits[0],
                _ => self.waits.pop_front().flatten(),
            })
        }

        fn kill(&mut self) -> Result<()> {
            self.killed.set(self.killed.get() + 1);
            if self.kill_fails {
                anyhow::bail!("not allowed to signal the child");
            }
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
        /// The pid that holds the listener on the port (`None`: some other process does).
        pub holder: Option<u32>,
        /// When set, the socket tables cannot be read and `held_by` says why.
        pub unreadable: Option<String>,
        /// The `(port, pid)` pairs `held_by` was asked about.
        pub asked: RefCell<Vec<(u16, u32)>>,
        /// The socket paths `remove_socket` was asked to remove.
        pub removed: RefCell<Vec<String>>,
    }

    impl FakeNet {
        pub fn new(free: Option<u16>, accepts: Vec<bool>) -> Self {
            Self {
                free,
                accepts: RefCell::new(accepts.into()),
                socket_taken: false,
                holder: Some(FAKE_CHILD_PID),
                unreadable: None,
                asked: RefCell::default(),
                removed: RefCell::default(),
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

        fn remove_socket(&self, path: &SocketPath) -> Result<()> {
            self.removed.borrow_mut().push(path.as_str().to_string());
            Ok(())
        }

        fn held_by(&self, port: LoopbackPort, pid: u32) -> Ownership {
            self.asked.borrow_mut().push((port.get(), pid));
            match (&self.unreadable, self.holder == Some(pid)) {
                (Some(why), _) => Ownership::Unknown(why.clone()),
                (None, true) => Ownership::Held,
                (None, false) => Ownership::Foreign,
            }
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
        launch_fails: bool,
        pub calls: RefCell<Vec<(Argv, How)>>,
    }

    impl FakeExec {
        pub fn new(captures: Vec<Reply>, streams: Vec<i32>) -> Self {
            Self {
                captures: RefCell::new(captures.into()),
                streams: RefCell::new(streams.into()),
                children: RefCell::default(),
                launch_fails: false,
                calls: RefCell::default(),
            }
        }

        /// The children that `spawn` hands out, in order.
        pub fn with_children(self, children: Vec<FakeChild>) -> Self {
            *self.children.borrow_mut() = children.into();
            self
        }

        /// Make `launch` fail, as for a program that is not installed.
        pub fn with_failing_launch(mut self) -> Self {
            self.launch_fails = true;
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

        fn stream_tee(&self, argv: &Argv, _prefix: &str, _echo: Echo) -> Result<Teed> {
            self.calls.borrow_mut().push((argv.clone(), How::StreamTee));
            let code = self
                .streams
                .borrow_mut()
                .pop_front()
                .expect("an unexpected stream");
            Ok(Teed {
                code,
                matched: vec![],
            })
        }

        fn spawn(&self, argv: &Argv) -> Result<Box<dyn Child>> {
            self.calls.borrow_mut().push((argv.clone(), How::Spawn));
            let child = self.children.borrow_mut().pop_front();
            Ok(Box::new(child.expect("an unexpected spawn")))
        }

        fn launch(&self, argv: &Argv) -> Result<()> {
            self.calls.borrow_mut().push((argv.clone(), How::Launch));
            if self.launch_fails {
                anyhow::bail!("could not start {}", argv.program());
            }
            Ok(())
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
    fn a_tee_shows_every_byte_unchanged_and_keeps_only_the_lines_with_the_prefix() {
        let raw = b"\x1b[1mbold\x1b[0m\nSubmitted batch job 7\r\n not Submitted batch job 8\nSubmitted batch job 9";
        let mut shown = Vec::new();
        let matched = tee_lines(raw.as_slice(), &mut shown, "Submitted batch job ").unwrap();
        assert_eq!(shown, raw, "the user sees the program's own bytes");
        assert_eq!(
            matched,
            ["Submitted batch job 7", "Submitted batch job 9"],
            "the line must start with the prefix"
        );
        assert!(tee_lines(&b""[..], &mut shown, "x").unwrap().is_empty());
    }

    /// The cap cuts a long line into pieces; only the first piece starts a line, so text that
    /// merely follows the cut must not read as a submit line.
    #[test]
    fn text_after_a_cut_in_a_long_line_is_not_a_line_start() {
        let prefix = "Submitted batch job ";
        let mut raw = vec![b'a'; MAX_LINE_BYTES as usize];
        raw.extend_from_slice(b"Submitted batch job 99\nSubmitted batch job 7\n");
        let matched = tee_lines(raw.as_slice(), &mut Vec::new(), prefix).unwrap();
        assert_eq!(
            matched,
            ["Submitted batch job 7"],
            "only after a real newline"
        );
    }

    #[test]
    fn a_read_error_while_teeing_is_an_error() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        assert!(tee_lines(Broken, &mut Vec::new(), "x").is_err());
    }

    /// A write that fails after a matching line was shown still returns that line.
    #[test]
    fn a_write_error_while_teeing_returns_the_lines_matched_so_far() {
        struct FailsOnSecondWrite(u32);
        impl Write for FailsOnSecondWrite {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                if self.0 > 1 {
                    return Err(io::Error::other("closed"));
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let raw = b"Submitted batch job 9\nnoise\nSubmitted batch job 10\n";
        let err =
            tee_lines(&raw[..], &mut FailsOnSecondWrite(0), "Submitted batch job ").unwrap_err();
        assert_eq!(
            err.matched,
            ["Submitted batch job 9", "Submitted batch job 10"],
            "a job id after the failed write is still recorded"
        );
        assert_eq!(err.to_string(), "closed");
    }

    /// The echo fails on the very line that carries the job id: the job is submitted all the same.
    #[test]
    fn a_write_error_on_the_matching_line_still_returns_that_line() {
        struct AlwaysFails;
        impl Write for AlwaysFails {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let raw = b"Submitted batch job 9\n";
        let err = tee_lines(&raw[..], &mut AlwaysFails, "Submitted batch job ").unwrap_err();
        assert_eq!(err.matched, ["Submitted batch job 9"]);
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
