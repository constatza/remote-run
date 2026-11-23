//! `rr fwd` with the real binary and a stub `ssh` on `PATH` that binds the forwarded port, as
//! ssh would: when `rr` is killed or interrupted while the forward listens, ssh must not outlive
//! it, and the forward must end by itself at its ttl. Linux only: it relies on `PR_SET_PDEATHSIG`
//! and `/proc`. The stub needs `python3` to hold the port; without it the listening tests say so
//! and pass without running. Nothing here touches the network, `~/.ssh` or the real `HOME`.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // starts processes and reads files by design

use std::fs;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Longest any wait may take; a hang fails the test instead of stalling the run.
const PATIENCE: Duration = Duration::from_secs(20);

/// What a shell reports for a command ended by Ctrl-C.
const INTERRUPTED: i32 = 130;

/// The stub: record the pid and the argv, then bind the `-L` port on loopback and hold it, as an
/// ssh tunnel would. Without `STUB_BIND` it only sleeps (a tunnel that never comes up).
const STUB: &str = r#"#!/bin/sh
echo $$ > "$STUB_PIDFILE"
printf '%s\n' "$@" > "$STUB_ARGS"
[ -n "$STUB_BIND" ] || exec sleep 60
while [ $# -gt 0 ]; do
    [ "$1" = -L ] && spec=$2
    shift
done
port=${spec#127.0.0.1:}
port=${port%%:*}
exec python3 -c 'import socket, sys, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen(5)
time.sleep(60)' "$port"
"#;

/// How `rr` is run against the stub.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tunnel {
    /// The stub binds the forwarded port.
    Listening,
    /// The stub never binds it.
    Silent,
}

/// A directory with a stub `ssh` on a `PATH` of its own.
struct Stub {
    dir: tempfile::TempDir,
}

impl Stub {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        fs::write(&ssh, STUB).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn pidfile(&self) -> PathBuf {
        self.dir.path().join("ssh.pid")
    }

    fn argv_file(&self) -> PathBuf {
        self.dir.path().join("ssh.argv")
    }

    fn stderr_file(&self) -> PathBuf {
        self.dir.path().join("rr.stderr")
    }

    /// `rr --host cluster fwd gpu-01:5000 <extra>`, with its stderr in a file.
    fn rr(&self, tunnel: Tunnel, extra: &[&str]) -> Child {
        let path = format!("{}:/usr/bin:/bin", self.dir.path().join("bin").display());
        let mut command = Command::new(env!("CARGO_BIN_EXE_rr"));
        command
            .args(["--host", "cluster", "fwd", "gpu-01:5000"])
            .args(extra)
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .env("STUB_PIDFILE", self.pidfile())
            .env("STUB_ARGS", self.argv_file());
        if tunnel == Tunnel::Listening {
            command.env("STUB_BIND", "1");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(self.stderr_file()).unwrap())
            .spawn()
            .unwrap()
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.stderr_file()).unwrap_or_default()
    }
}

fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The stub's pid, once it has started.
fn ssh_pid(stub: &Stub) -> u32 {
    until("the stub ssh to start", || {
        fs::read_to_string(stub.pidfile()).ok()?.trim().parse().ok()
    })
}

fn alive(pid: u32) -> bool {
    let stat = fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat"));
    // a zombie is dead: it only waits for its parent to collect it
    stat.is_ok_and(|text| !text.contains(") Z "))
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The announcement line, once `rr` has seen the forward listening.
fn url_line(stub: &Stub) -> String {
    until("the URL line", || {
        stub.stderr()
            .lines()
            .find(|l| l.starts_with("rr: http://"))
            .map(String::from)
    })
}

/// The forward's tunnel in the stub's world, or `None` (after saying why) when it cannot run.
fn listening_stub() -> Option<Stub> {
    if !python_available() {
        eprintln!("skipped: python3 is needed to hold the forwarded port");
        return None;
    }
    Some(Stub::new())
}

fn signal(name: &str, pid: u32) {
    let sent = Command::new("kill")
        .args([name, &pid.to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
}

fn wait_for(rr: &mut Child) -> ExitStatus {
    until("rr to exit", || rr.try_wait().unwrap())
}

#[test]
fn killing_rr_with_sigkill_before_the_forward_listens_kills_ssh() {
    let stub = Stub::new();
    let mut rr = stub.rr(Tunnel::Silent, &[]);
    let pid = ssh_pid(&stub);
    assert!(alive(pid));
    rr.kill().unwrap();
    rr.wait().unwrap();
    until("ssh to die with rr", || (!alive(pid)).then_some(()));
}

#[test]
fn killing_rr_with_sigkill_while_the_forward_listens_kills_ssh() {
    let Some(stub) = listening_stub() else { return };
    let mut rr = stub.rr(Tunnel::Listening, &[]);
    url_line(&stub);
    let pid = ssh_pid(&stub);
    assert!(alive(pid));
    rr.kill().unwrap();
    rr.wait().unwrap();
    until("ssh to die with rr", || (!alive(pid)).then_some(()));
}

#[test]
fn sigint_and_sigterm_while_the_forward_listens_close_it_and_exit_130() {
    for name in ["-INT", "-TERM"] {
        let Some(stub) = listening_stub() else { return };
        let mut rr = stub.rr(Tunnel::Listening, &[]);
        url_line(&stub);
        let pid = ssh_pid(&stub);
        signal(name, rr.id());
        let status = wait_for(&mut rr);
        assert_eq!(status.code(), Some(INTERRUPTED), "{name}: {status:?}");
        assert!(!alive(pid), "{name}: ssh outlived rr");
        assert!(
            stub.stderr().contains("interrupted, forward closed"),
            "{name}"
        );
    }
}

#[test]
fn sigterm_before_the_forward_listens_stops_ssh_and_is_not_a_success() {
    let stub = Stub::new();
    let mut rr = stub.rr(Tunnel::Silent, &[]);
    let pid = ssh_pid(&stub);
    signal("-TERM", rr.id());
    let status = wait_for(&mut rr);
    assert!(!status.success(), "an interrupted forward is not a success");
    assert!(status.signal().is_none(), "rr ended itself: {status:?}");
    until("ssh to die", || (!alive(pid)).then_some(()));
}

#[test]
fn the_ttl_ends_the_forward_with_success_and_no_ssh_left() {
    let Some(stub) = listening_stub() else { return };
    let mut rr = stub.rr(Tunnel::Listening, &["--ttl", "1s"]);
    let status = wait_for(&mut rr);
    assert!(status.success(), "{status:?}\n{}", stub.stderr());
    assert!(!alive(ssh_pid(&stub)), "ssh outlived the ttl");
    assert!(stub.stderr().contains("time to live reached"));
}

#[test]
fn the_url_is_announced_once_the_port_accepts_and_ssh_gets_options_ended_before_the_host() {
    let Some(stub) = listening_stub() else { return };
    let mut rr = stub.rr(Tunnel::Listening, &[]);
    let line = url_line(&stub);
    let port: u16 = line
        .strip_prefix("rr: http://127.0.0.1:")
        .and_then(|rest| rest.split('/').next())
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no port in {line:?}"));
    assert!(line.ends_with("-> gpu-01:5000 (closes in 8h)"), "{line}");
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
    let argv = fs::read_to_string(stub.argv_file()).unwrap();
    let words: Vec<&str> = argv.lines().collect();
    let [.., forward, dashes, host] = words[..] else {
        panic!("short argv: {words:?}")
    };
    assert_eq!(forward, format!("127.0.0.1:{port}:gpu-01:5000"));
    assert_eq!((dashes, host), ("--", "cluster"));
    rr.kill().unwrap();
    rr.wait().unwrap();
}
