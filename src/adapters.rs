//! The real [`Exec`] and [`Remote`]: the only place that starts a process.
#![allow(clippy::disallowed_methods)]

use crate::domain::argv::{Argv, Captured, Stdin};
use crate::domain::fwd::{Endpoint, LoopbackPort, SocketPath};
use crate::domain::host::Host;
use crate::domain::shell::Script;
use crate::domain::ssh::{Terminal, ssh_argv};
use crate::ports::{Child as PortChild, Clock, Exec, Instant, Net, Remote, write_sanitized};
use anyhow::{Context, Result, anyhow, bail};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as WallClock};

/// Runs programs with `std::process`.
pub struct OsExec;

fn command(argv: &Argv) -> Command {
    let mut command = Command::new(argv.program());
    command.args(argv.args());
    command
}

/// A child killed by a signal has no exit code; that must never read as success.
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

impl Exec for OsExec {
    fn capture(&self, argv: &Argv, stdin: Stdin) -> Result<Captured> {
        let stdin = match stdin {
            Stdin::Null => Stdio::null(),
            Stdin::Inherit => Stdio::inherit(),
        };
        let out = command(argv)
            .stdin(stdin)
            .output()
            .with_context(|| format!("could not start {}", argv.program()))?;
        Ok(Captured {
            code: exit_code(out.status),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn stream(&self, argv: &Argv) -> Result<i32> {
        let status = command(argv)
            .status()
            .with_context(|| format!("could not start {}", argv.program()))?;
        Ok(exit_code(status))
    }

    fn stream_filtered(&self, argv: &Argv) -> Result<i32> {
        stream_filtered_to(argv, io::stdout(), io::stderr())
    }

    fn spawn(&self, argv: &Argv) -> Result<Box<dyn PortChild>> {
        spawn_child(argv).map(|child| Box::new(child) as Box<dyn PortChild>)
    }
}

/// The exit code a child reads as after `rr` was interrupted (SIGINT, SIGTERM or SIGHUP): 128 plus
/// SIGINT, the shell's convention for Ctrl-C.
const INTERRUPTED_EXIT: i32 = 130;

/// A child started by [`OsExec::spawn`]. Dropping it ends the child, because dropping a
/// `std::process::Child` leaves the process running, and an orphaned tunnel is the failure `rr fwd`
/// exists to prevent.
struct OsChild {
    child: Child,
    /// Set by a SIGINT, SIGTERM or SIGHUP to `rr`: the signal handler only sets a flag, and the
    /// owner of the handle sees it on its next `try_wait`.
    interrupted: Arc<AtomicBool>,
}

impl PortChild for OsChild {
    fn try_wait(&mut self) -> Result<Option<i32>> {
        if self.interrupted.load(Ordering::SeqCst) {
            self.kill()?;
            return Ok(Some(INTERRUPTED_EXIT));
        }
        let status = self.child.try_wait().context("could not poll the child")?;
        Ok(status.map(exit_code))
    }

    fn kill(&mut self) -> Result<()> {
        // `kill` on a child that has already been reaped is `Ok`; only a real failure is an error
        self.child.kill().context("could not kill the child")?;
        self.child.wait().context("could not wait for the child")?;
        Ok(())
    }
}

impl Drop for OsChild {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

fn spawn_child(argv: &Argv) -> Result<OsChild> {
    let interrupted = Arc::new(AtomicBool::new(false));
    flag_interrupts(&interrupted)?;
    let mut command = command(argv);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    die_with_parent(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", argv.program()))?;
    if let Some(stderr) = child.stderr.take() {
        // ends by itself when the child closes its stderr
        std::thread::spawn(move || write_sanitized(stderr, &mut io::stderr()));
    }
    Ok(OsChild { child, interrupted })
}

/// Set `flag` on SIGINT, SIGTERM or SIGHUP, so `rr` can end its child instead of dying with it
/// still running. Without Unix signals there is nothing to install.
#[cfg(unix)]
fn flag_interrupts(flag: &Arc<AtomicBool>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(flag))
            .context("could not install a signal handler")?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn flag_interrupts(_: &Arc<AtomicBool>) -> Result<()> {
    Ok(())
}

/// Have the kernel send the child SIGTERM when `rr` dies, however it dies (SIGKILL included, where
/// no handler and no `Drop` runs). The signal is tied to the thread that started the child, so
/// `spawn` must run on the thread that lives as long as `rr` does: the main thread.
#[cfg(target_os = "linux")]
fn die_with_parent(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs between fork and exec and calls only `prctl` and `getppid`, which
    // are async-signal-safe, and allocates nothing.
    unsafe {
        let parent = libc::getpid();
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            // `rr` may have died between fork and prctl, when the signal is no longer coming
            if libc::getppid() != parent {
                return Err(io::Error::other("rr ended while its child was starting"));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_: &mut Command) {}

/// The real local network: loopback sockets and the file system.
pub struct OsNet;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const LOOPBACK_ADDR: Ipv4Addr = Ipv4Addr::LOCALHOST;

impl Net for OsNet {
    fn free_port(&self, want: Option<LoopbackPort>) -> Result<LoopbackPort> {
        let port = want.map_or(0, LoopbackPort::get);
        let listener = TcpListener::bind((LOOPBACK_ADDR, port))
            .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
        let bound = listener.local_addr().context("no local address")?.port();
        LoopbackPort::from_u16(bound)
    }

    fn socket_unused(&self, path: &SocketPath) -> Result<()> {
        match std::fs::symlink_metadata(path.as_str()) {
            Ok(_) => bail!(
                "{} already exists; remove it if no rr fwd is using it",
                path.as_str()
            ),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("checking {}", path.as_str())),
        }
    }

    fn accepts(&self, endpoint: &Endpoint) -> bool {
        match endpoint {
            Endpoint::Tcp(port) => {
                let addr = SocketAddr::from((LOOPBACK_ADDR, port.get()));
                TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok()
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => std::os::unix::net::UnixStream::connect(path.as_str()).is_ok(),
            #[cfg(not(unix))]
            Endpoint::Unix(_) => false,
        }
    }
}

/// The exit code reported when the user's stdout closed early (`rr q | head -1`) and the child
/// itself had ended with success: the output was cut short, so that is not a success. It is the
/// shell's code for death by SIGPIPE.
const OUTPUT_CLOSED_EXIT: i32 = 141;

/// Take both output pipes of `child`.
fn take_pipes(child: &mut Child) -> Option<(ChildStdout, ChildStderr)> {
    Some((child.stdout.take()?, child.stderr.take()?))
}

/// How long a child gets to leave after SIGTERM before it is killed. `ssh -t` puts the local tty
/// in raw mode and restores it only when it ends on its own, so a kill is the last resort.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// How often a stopping child is checked for having ended.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Whether `child` has ended (and is now reaped). A handle that cannot be asked counts as ended:
/// there is nothing more that could be done to it.
fn ended(child: &Mutex<Child>) -> bool {
    child
        .lock()
        .map_or(true, |mut child| !matches!(child.try_wait(), Ok(None)))
}

/// Ask `child` to end with SIGTERM, and kill it when it is still there after `grace`. Std has no
/// SIGTERM, so the `kill` program sends it; when that cannot run, the kill after `grace` still ends
/// the child. Reaping here (through `try_wait`) leaves the final `wait` its cached status.
fn terminate(child: &Mutex<Child>, grace: Duration) {
    let Ok(pid) = child.lock().map(|child| child.id()) else {
        return;
    };
    let _ = Command::new("kill")
        .args(["-TERM", "--", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = WallClock::now() + grace;
    while !ended(child) {
        if WallClock::now() >= deadline {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
            }
            return;
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// `result`, after asking the stopper thread to end the child when it is an error: a child writing
/// into a pipe nobody reads any more would otherwise run on, or sit idle, and the other pump would
/// wait for it. Only a message is sent, so the pump never waits for the child.
fn stop_on_error(result: io::Result<()>, stop: &Sender<()>) -> io::Result<()> {
    if result.is_err() {
        let _ = stop.send(());
    }
    result
}

/// Read both pipes of `child` into `out` and `err` at once. A failed write on either side has the
/// child stopped by a third thread ([`terminate`]), which also lets the other pump finish.
fn pump(
    child: &Mutex<Child>,
    (child_out, child_err): (ChildStdout, ChildStderr),
    (mut out, mut err): (impl Write + Send, impl Write + Send),
    grace: Duration,
) -> (io::Result<()>, std::thread::Result<io::Result<()>>) {
    std::thread::scope(|scope| {
        let (stop, stopped) = mpsc::channel::<()>();
        scope.spawn(move || {
            // `recv` fails when both pumps ended without needing a stop
            if stopped.recv().is_ok() {
                terminate(child, grace);
            }
        });
        let err_stop = stop.clone();
        let err_pump =
            scope.spawn(move || stop_on_error(write_sanitized(child_err, &mut err), &err_stop));
        let out_result = stop_on_error(write_sanitized(child_out, &mut out), &stop);
        drop(stop);
        (out_result, err_pump.join())
    })
}

/// Run `argv` with stdin inherited and its stdout and stderr read through [`write_sanitized`] into
/// `out` and `err`. The two pipes are read at once, so a child that fills one while the other is
/// idle cannot stall.
///
/// A failed write to either side stops the child (closing the pipe alone would not stop one that
/// never writes there again), first with SIGTERM and after [`STOP_GRACE`] with SIGKILL, and waits
/// for it, so no zombie is left. A `BrokenPipe` on `out` or `err` is how `rr q | head -1` ends: it
/// is not an error to report, and the result is the child's code, or [`OUTPUT_CLOSED_EXIT`] when
/// that was success. Any other write error is returned.
fn stream_filtered_to(argv: &Argv, out: impl Write + Send, err: impl Write + Send) -> Result<i32> {
    stream_filtered_with(argv, out, err, STOP_GRACE)
}

/// [`stream_filtered_to`] with the grace period given, so a test need not wait the real one.
fn stream_filtered_with(
    argv: &Argv,
    out: impl Write + Send,
    err: impl Write + Send,
    grace: Duration,
) -> Result<i32> {
    let mut child = command(argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {}", argv.program()))?;
    let Some(pipes) = take_pipes(&mut child) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!("{} has no output pipes", argv.program()));
    };
    let child = Mutex::new(child);
    let (out_result, err_result) = pump(&child, pipes, (out, err), grace);
    let status = child
        .into_inner()
        .map_err(|_| anyhow!("the child handle was poisoned"))?
        .wait()
        .context("could not wait for the child")?;
    let code = exit_code(status);
    let hard = |e: &io::Error| e.kind() != io::ErrorKind::BrokenPipe;
    match (out_result, err_result) {
        (Err(e), _) if hard(&e) => Err(e).context("could not write the output"),
        (_, Err(_)) => Err(anyhow!("the stderr filter stopped")),
        (_, Ok(Err(e))) if hard(&e) => Err(e).context("could not write the error output"),
        (Ok(()), Ok(Ok(()))) => Ok(code),
        // a closed stdout or stderr cut the output short, so a clean exit is no success
        (_, Ok(_)) if code == 0 => Ok(OUTPUT_CLOSED_EXIT),
        (_, Ok(_)) => Ok(code),
    }
}

/// The real host, reached as `ssh <host>`, so `~/.ssh/config` and the agent apply.
pub struct Ssh<'a> {
    host: &'a Host,
    exec: &'a dyn Exec,
}

impl<'a> Ssh<'a> {
    pub fn new(host: &'a Host, exec: &'a dyn Exec) -> Self {
        Self { host, exec }
    }
}

impl Remote for Ssh<'_> {
    fn capture(&self, script: &Script) -> Result<Captured> {
        let argv = ssh_argv(self.host, &script.to_string(), Terminal::Plain);
        self.exec.capture(&argv, Stdin::Null)
    }

    fn stream(&self, script: &Script, terminal: Terminal) -> Result<i32> {
        self.exec
            .stream(&ssh_argv(self.host, &script.to_string(), terminal))
    }

    fn stream_filtered(&self, script: &Script, terminal: Terminal) -> Result<i32> {
        self.exec
            .stream_filtered(&ssh_argv(self.host, &script.to_string(), terminal))
    }
}

/// The real clock: time since it was built, and a real sleep.
pub struct SystemClock {
    started: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::from_start(self.started.elapsed())
    }

    fn sleep(&self, delay: Duration) {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shell::lit;

    fn host() -> Host {
        Host::parse("cluster").unwrap()
    }

    use crate::ports::fake::{FakeExec, How, output};

    /// Longest a stream test may take; a regression that hangs fails instead of stalling the run.
    #[cfg(unix)]
    const WATCHDOG: Duration = Duration::from_secs(30);

    #[cfg(unix)]
    fn sh(script: &str) -> Argv {
        Argv::new("sh", vec!["-c".into(), script.into()])
    }

    /// Run `work` on its own thread and fail the test when it does not finish within [`WATCHDOG`].
    #[cfg(unix)]
    fn within<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(work());
        });
        result
            .recv_timeout(WATCHDOG)
            .expect("the stream did not finish: a deadlock or a child that was not stopped")
    }

    /// A writer whose every write fails with `kind`.
    struct Failing(io::ErrorKind);
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn capture_collects_output_and_the_exit_code() {
        let out = OsExec
            .capture(&sh("printf out; printf err >&2; exit 4"), Stdin::Null)
            .unwrap();
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (4, "out", "err")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_null_stdin_is_closed_at_once() {
        let out = OsExec.capture(&sh("cat; echo done"), Stdin::Null).unwrap();
        assert_eq!(out.stdout, "done\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_killed_child_is_never_success() {
        let killed = sh("kill -9 $$");
        assert_ne!(OsExec.capture(&killed, Stdin::Null).unwrap().code, 0);
        assert_ne!(OsExec.stream(&killed).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn stream_returns_the_exit_code_and_a_missing_program_is_an_error() {
        assert_eq!(OsExec.stream(&sh("exit 7")).unwrap(), 7);
        let missing = Argv::new("rr-no-such-program", vec![]);
        let err = OsExec.stream(&missing).unwrap_err().to_string();
        assert_eq!(err, "could not start rr-no-such-program");
    }

    /// What the user's terminal would have received from `script`, and the child's exit code.
    #[cfg(unix)]
    fn filtered(script: &str) -> (i32, String, String) {
        let script = script.to_owned();
        within(move || {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = stream_filtered_to(&sh(&script), &mut out, &mut err).unwrap();
            let text = |bytes| String::from_utf8(bytes).unwrap();
            (code, text(out), text(err))
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_child_cannot_put_control_sequences_on_the_terminal_through_either_stream() {
        let (code, out, err) = filtered(
            r"printf 'a\033]52;c;ZXZpbA==\007b\n'; printf 'x\342\200\256y\033[2J\n' >&2; exit 5",
        );
        assert_eq!(code, 5);
        assert_eq!(out, "a?]52;c;ZXZpbA==?b\n");
        assert_eq!(err, "x?y?[2J\n");
    }

    #[cfg(unix)]
    #[test]
    fn bad_utf8_is_cleaned_a_terminal_newline_is_kept_and_a_huge_line_does_not_stall() {
        let (_, out, _) = filtered(r"printf 'caf\351\r\n'");
        assert_eq!(out, "caf\u{FFFD}\r\n");
        let (code, out, _) = filtered("head -c 300000 /dev/zero | tr '\\000' a");
        assert_eq!((code, out.len()), (0, 300_000));
    }

    #[cfg(unix)]
    #[test]
    fn a_lot_of_output_on_both_streams_at_once_does_not_deadlock() {
        let (code, out, err) = filtered(
            "i=0; while [ $i -lt 4000 ]; do echo out-$i; echo err-$i >&2; i=$((i+1)); done",
        );
        assert_eq!(code, 0);
        assert_eq!((out.lines().count(), err.lines().count()), (4000, 4000));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_fills_stderr_while_stdout_is_idle_does_not_deadlock() {
        // far more than a 64 KiB pipe holds: stderr must be drained while stdout is still open
        let (code, out, err) = filtered("head -c 300000 /dev/zero | tr '\\000' e >&2; echo done");
        assert_eq!((code, out.as_str(), err.len()), (0, "done\n", 300_000));
    }

    #[cfg(unix)]
    #[test]
    fn a_filtered_signal_killed_child_is_never_success_and_a_missing_program_is_an_error() {
        let (code, _, _) = filtered("kill -9 $$");
        assert_ne!(code, 0);
        let missing = Argv::new("rr-no-such-program", vec![]);
        let err = stream_filtered_to(&missing, io::sink(), io::sink()).unwrap_err();
        assert_eq!(err.to_string(), "could not start rr-no-such-program");
    }

    /// Run `script` filtered into `out` and `err` under the watchdog, with the time it took.
    #[cfg(unix)]
    fn filtered_into(
        script: &'static str,
        out: impl Write + Send + 'static,
        err: impl Write + Send + 'static,
    ) -> (Result<i32>, Duration) {
        within(move || {
            let started = WallClock::now();
            let result = stream_filtered_to(&sh(script), out, err);
            (result, started.elapsed())
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_stdout_ends_quietly_and_stops_a_child_that_never_writes_again() {
        // `head -1` closes the pipe after the first line; this child then just sits there
        let sits_idle = "echo first; exec sleep 25";
        let (result, took) =
            filtered_into(sits_idle, Failing(io::ErrorKind::BrokenPipe), io::sink());
        assert_ne!(result.unwrap(), 0, "a cut-short run is not a success");
        assert!(
            took < Duration::from_secs(20),
            "the child was waited for, not stopped"
        );
        let chatty = "while :; do echo line; done";
        let (result, _) = filtered_into(chatty, Failing(io::ErrorKind::BrokenPipe), io::sink());
        assert_ne!(result.unwrap(), 0);
        // a child that had already succeeded still reports the cut-short output as a failure
        let (result, _) = filtered_into("echo hi", Failing(io::ErrorKind::BrokenPipe), io::sink());
        assert_ne!(result.unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn any_other_stdout_error_stops_the_child_and_is_reported() {
        let chatty = "while :; do echo line; done";
        let (result, _) = filtered_into(chatty, Failing(io::ErrorKind::Other), io::sink());
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "could not write the output");
    }

    /// Run `script` (which gets `marker` as `$1`) with stdout already closed, under a grace period
    /// of `grace`; the child's result and the time it took.
    #[cfg(unix)]
    fn stopped(
        script: &'static str,
        marker: &std::path::Path,
        grace: Duration,
    ) -> (Result<i32>, Duration) {
        let args = ["-c", script, "sh", &marker.display().to_string()];
        let argv = Argv::new("sh", args.map(String::from).to_vec());
        within(move || {
            let started = WallClock::now();
            let broken = Failing(io::ErrorKind::BrokenPipe);
            let result = stream_filtered_with(&argv, broken, io::sink(), grace);
            (result, started.elapsed())
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_child_gets_sigterm_first_so_it_can_clean_up() {
        // like `ssh -t` restoring the tty: the trap runs only when TERM, not KILL, arrives
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cleaned-up");
        let script =
            r#"trap 'echo done > "$1"; exit 0' TERM; echo first; while :; do sleep 0.05; done"#;
        let (result, took) = stopped(script, &marker, Duration::from_secs(20));
        assert_ne!(result.unwrap(), 0, "a cut-short run is not a success");
        assert!(marker.exists(), "the child never saw SIGTERM");
        assert!(took < Duration::from_secs(10), "waited for the grace");
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_ignores_sigterm_is_killed_after_the_grace_and_never_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let script = r"trap '' TERM; echo first; while :; do sleep 0.05; done";
        let grace = Duration::from_millis(300);
        let (result, took) = stopped(script, &dir.path().join("unused"), grace);
        assert_ne!(result.unwrap(), 0, "a signal-killed child is not a success");
        assert!(took >= grace, "killed before the grace was over");
        assert!(took < Duration::from_secs(10), "the child was not killed");
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_stderr_ends_quietly_like_a_closed_stdout() {
        let noisy = "while :; do echo line >&2; done";
        let (result, _) = filtered_into(noisy, io::sink(), Failing(io::ErrorKind::BrokenPipe));
        assert_ne!(result.unwrap(), 0, "a cut-short run is not a success");
        let (result, _) = filtered_into(
            "echo hi >&2; exit 3",
            io::sink(),
            Failing(io::ErrorKind::BrokenPipe),
        );
        assert_eq!(result.unwrap(), 3, "the child's own failure code is kept");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_stderr_stops_the_child_and_is_reported() {
        let noisy = "while :; do echo line >&2; done";
        let (result, _) = filtered_into(noisy, io::sink(), Failing(io::ErrorKind::Other));
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "could not write the error output");
        // stdout is idle and open: only killing the child lets the stdout reader finish
        let idle = "echo err >&2; exec sleep 25";
        let (result, took) = filtered_into(idle, io::sink(), Failing(io::ErrorKind::Other));
        assert!(result.is_err());
        assert!(
            took < Duration::from_secs(20),
            "the child was waited for, not stopped"
        );
    }

    #[test]
    fn filtered_streams_go_through_ssh_with_the_same_argv_as_plain_ones() {
        let exec = FakeExec::new(vec![], vec![0, 9]);
        let cluster = host();
        let ssh = Ssh::new(&cluster, &exec);
        assert_eq!(
            ssh.stream_filtered(&lit("true"), Terminal::Plain).unwrap(),
            0
        );
        let attached = ssh.stream_filtered(&lit("true"), Terminal::Attached);
        assert_eq!(attached.unwrap(), 9);
        let calls = exec.calls.borrow();
        assert_eq!(calls[0].1, How::StreamFiltered);
        assert_eq!(calls[0].0.args()[4..], ["--", "cluster", "bash -c 'true'"]);
        assert_eq!(
            calls[1].0.args()[4..],
            ["-t", "--", "cluster", "bash -c 'true'"]
        );
    }

    #[test]
    fn capture_sends_the_script_with_a_null_stdin() {
        let exec = FakeExec::new(vec![output(0, "ok", "")], vec![]);
        let got = Ssh::new(&host(), &exec).capture(&lit("true")).unwrap();
        assert_eq!(got.stdout, "ok");
        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.program(), "ssh");
        assert_eq!(calls[0].0.args()[4..], ["--", "cluster", "bash -c 'true'"]);
        assert_eq!(calls[0].1, How::Capture(Stdin::Null));
    }

    #[test]
    fn stream_attaches_the_terminal_and_allocates_one_only_on_request() {
        let exec = FakeExec::new(vec![], vec![0, 9]);
        let cluster = host();
        let ssh = Ssh::new(&cluster, &exec);
        assert_eq!(ssh.stream(&lit("true"), Terminal::Plain).unwrap(), 0);
        assert_eq!(ssh.stream(&lit("true"), Terminal::Attached).unwrap(), 9);
        let calls = exec.calls.borrow();
        assert_eq!(calls[0].1, How::Stream);
        assert_eq!(calls[0].0.args()[4..], ["--", "cluster", "bash -c 'true'"]);
        assert_eq!(calls[1].1, How::Stream);
        assert_eq!(
            calls[1].0.args()[4..],
            ["-t", "--", "cluster", "bash -c 'true'"]
        );
    }

    #[test]
    fn the_system_clock_never_runs_backwards_and_sleeps_at_least_as_asked() {
        let clock = SystemClock::new();
        let before = clock.now();
        clock.sleep(Duration::from_millis(20));
        let after = clock.now();
        assert!(after >= before);
        assert!(after.since(before) >= Duration::from_millis(20));
    }

    /// Poll `child` until it has ended, failing the test after [`WATCHDOG`].
    #[cfg(unix)]
    fn ended_with(child: &mut dyn PortChild) -> i32 {
        let deadline = WallClock::now() + WATCHDOG;
        loop {
            if let Some(code) = child.try_wait().unwrap() {
                return code;
            }
            assert!(WallClock::now() < deadline, "the child never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_spawned_child_reports_its_code_and_a_killed_one_is_never_success() {
        assert_eq!(ended_with(&mut spawn_child(&sh("exit 3")).unwrap()), 3);
        let mut sleeper = spawn_child(&sh("sleep 30")).unwrap();
        assert_eq!(sleeper.try_wait().unwrap(), None);
        sleeper.kill().unwrap();
        assert_ne!(sleeper.try_wait().unwrap(), Some(0));
        sleeper.kill().unwrap();
        let missing = Argv::new("rr-no-such-program", vec![]);
        assert!(OsExec.spawn(&missing).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_spawned_child_ends_it() {
        let child = spawn_child(&sh("sleep 30")).unwrap();
        let pid = child.child.id().to_string();
        drop(child);
        let alive = Command::new("kill").args(["-0", &pid]).output().unwrap();
        assert!(!alive.status.success(), "the child outlived its handle");
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_ends_the_child_and_reads_as_interrupted() {
        let mut child = spawn_child(&sh("sleep 30")).unwrap();
        child.interrupted.store(true, Ordering::SeqCst);
        assert_eq!(child.try_wait().unwrap(), Some(INTERRUPTED_EXIT));
    }

    #[test]
    fn the_net_hands_out_loopback_ports_and_sees_who_listens() {
        // the listener keeps its port for the whole test: a port freed and asked for again could
        // meanwhile be taken by any other test's connection
        let listener = TcpListener::bind((LOOPBACK_ADDR, 0)).unwrap();
        let port = LoopbackPort::from_u16(listener.local_addr().unwrap().port()).unwrap();
        assert_ne!(OsNet.free_port(None).unwrap(), port);
        let endpoint = Endpoint::Tcp(port);
        assert!(OsNet.accepts(&endpoint));
        assert!(
            OsNet.free_port(Some(port)).is_err(),
            "a taken port is refused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_path_that_exists_is_refused() {
        use crate::domain::fwd::ForwardName;
        let dir = tempfile::tempdir().unwrap();
        let name = ForwardName::parse("t").unwrap();
        let path = SocketPath::new(dir.path().to_str().unwrap(), &name).unwrap();
        OsNet.socket_unused(&path).unwrap();
        assert!(!OsNet.accepts(&Endpoint::Unix(path.clone())));
        let listener = std::os::unix::net::UnixListener::bind(path.as_str()).unwrap();
        assert!(OsNet.socket_unused(&path).is_err());
        assert!(OsNet.accepts(&Endpoint::Unix(path)));
        drop(listener);
    }
}
