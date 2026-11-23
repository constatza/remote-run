//! The real [`Exec`] and [`Remote`]: the only place that starts a process.
#![allow(clippy::disallowed_methods)]

use crate::domain::argv::{Argv, Captured, Stdin};
use crate::domain::host::Host;
use crate::domain::shell::Script;
use crate::domain::ssh::{Terminal, ssh_argv};
use crate::ports::{Clock, Exec, Instant, Remote, write_sanitized};
use anyhow::{Context, Result, anyhow};
use std::io::{self, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

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
}

/// Run `argv` with stdin inherited and its stdout and stderr read through [`write_sanitized`] into
/// `out` and `err`. The two pipes are read at once, so a child that fills one while the other is
/// idle cannot stall. When a write fails its reader is dropped, which closes the pipe and ends the
/// child; the error is reported after the child has been waited for, so no zombie is left.
fn stream_filtered_to(
    argv: &Argv,
    mut out: impl Write + Send,
    mut err: impl Write + Send,
) -> Result<i32> {
    let mut child = command(argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {}", argv.program()))?;
    let (Some(child_out), Some(child_err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(anyhow!("{} has no output pipes", argv.program()));
    };
    let (out_result, err_result) = std::thread::scope(|scope| {
        let err_pump = scope.spawn(move || write_sanitized(child_err, &mut err));
        let out_result = write_sanitized(child_out, &mut out);
        (out_result, err_pump.join())
    });
    let status = child.wait().context("could not wait for the child")?;
    out_result.context("could not write the output")?;
    err_result
        .map_err(|_| anyhow!("the stderr filter stopped"))?
        .context("could not write the error output")?;
    Ok(exit_code(status))
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

    fn sh(script: &str) -> Argv {
        Argv::new("sh", vec!["-c".into(), script.into()])
    }

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

    #[test]
    fn stream_returns_the_exit_code_and_a_missing_program_is_an_error() {
        assert_eq!(OsExec.stream(&sh("exit 7")).unwrap(), 7);
        let missing = Argv::new("rr-no-such-program", vec![]);
        let err = OsExec.stream(&missing).unwrap_err().to_string();
        assert_eq!(err, "could not start rr-no-such-program");
    }

    /// What the user's terminal would have received from `script`, and the child's exit code.
    fn filtered(script: &str) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = stream_filtered_to(&sh(script), &mut out, &mut err).unwrap();
        let text = |bytes| String::from_utf8(bytes).unwrap();
        (code, text(out), text(err))
    }

    #[test]
    fn a_child_cannot_put_control_sequences_on_the_terminal_through_either_stream() {
        let (code, out, err) = filtered(
            r"printf 'a\033]52;c;ZXZpbA==\007b\n'; printf 'x\342\200\256y\033[2J\n' >&2; exit 5",
        );
        assert_eq!(code, 5);
        assert_eq!(out, "a?]52;c;ZXZpbA==?b\n");
        assert_eq!(err, "x?y?[2J\n");
    }

    #[test]
    fn bad_utf8_is_cleaned_a_terminal_newline_is_kept_and_a_huge_line_does_not_stall() {
        let (_, out, _) = filtered(r"printf 'caf\351\r\n'");
        assert_eq!(out, "caf\u{FFFD}\r\n");
        let (code, out, _) = filtered("head -c 300000 /dev/zero | tr '\\000' a");
        assert_eq!((code, out.len()), (0, 300_000));
    }

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
    fn a_filtered_signal_killed_child_is_never_success_and_a_missing_program_is_an_error() {
        let (code, _, _) = filtered("kill -9 $$");
        assert_ne!(code, 0);
        let missing = Argv::new("rr-no-such-program", vec![]);
        let err = stream_filtered_to(&missing, io::sink(), io::sink()).unwrap_err();
        assert_eq!(err.to_string(), "could not start rr-no-such-program");
    }

    #[test]
    fn a_failing_output_stops_the_child_and_is_reported() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let forever = sh("while :; do echo line; done");
        assert!(stream_filtered_to(&forever, Closed, io::sink()).is_err());
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
}
