//! `rr fwd` with the real binary and a stub `ssh` on `PATH`: when `rr` is killed or interrupted
//! while ssh runs, ssh must not outlive it. Linux only: it relies on `PR_SET_PDEATHSIG` and
//! `/proc`. Nothing here touches the network, `~/.ssh` or the real `HOME`.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // starts processes and reads files by design

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Longest any wait may take; a hang fails the test instead of stalling the run.
const PATIENCE: Duration = Duration::from_secs(20);

/// A directory with a stub `ssh` that records its pid and then sleeps, as an ssh tunnel would.
struct Stub {
    dir: tempfile::TempDir,
}

impl Stub {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\necho $$ > \"$STUB_PIDFILE\"\nexec sleep 60\n",
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn pidfile(&self) -> PathBuf {
        self.dir.path().join("ssh.pid")
    }

    fn rr(&self) -> Child {
        let path = format!("{}:/usr/bin:/bin", self.dir.path().join("bin").display());
        Command::new(env!("CARGO_BIN_EXE_rr"))
            .args(["--host", "cluster", "fwd", "gpu-01:5000"])
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .env("STUB_PIDFILE", self.pidfile())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
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

#[test]
fn killing_rr_with_sigkill_kills_ssh() {
    let stub = Stub::new();
    let mut rr = stub.rr();
    let pid = ssh_pid(&stub);
    assert!(alive(pid));
    rr.kill().unwrap();
    rr.wait().unwrap();
    until("ssh to die with rr", || (!alive(pid)).then_some(()));
}

#[test]
fn sigterm_to_rr_stops_ssh_and_rr_exits_without_success() {
    let stub = Stub::new();
    let mut rr = stub.rr();
    let pid = ssh_pid(&stub);
    let sent = Command::new("kill")
        .args(["-TERM", &rr.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let status = until("rr to exit", || rr.try_wait().unwrap());
    assert!(!status.success(), "an interrupted forward is not a success");
    until("ssh to die", || (!alive(pid)).then_some(()));
}
