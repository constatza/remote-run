//! The one seam to the remote host: run a bash script over `ssh`.

use crate::quote::quote;
use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// ssh options every call uses: never prompt (a prompt nobody sees would hang), no banner.
pub const SSH_OPTIONS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "LogLevel=ERROR"];

/// What a script printed and how it ended.
#[derive(Clone, Debug)]
pub struct Captured {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs scripts on the remote host; a trait so tests can record what would be sent.
pub trait Remote {
    /// Run `script`, collecting its output.
    fn capture(&self, script: &str) -> Result<Captured>;
    /// Run `script` with the terminal attached (`tty` allocates one, so Ctrl-C reaches it).
    fn stream(&self, script: &str, tty: bool) -> Result<i32>;
}

/// The real host, reached as `ssh <host>`, so `~/.ssh/config` and the agent apply.
pub struct Ssh {
    pub host: String,
}

/// The `ssh` argv that runs `script` under bash on `host` (pure).
pub fn ssh_argv(host: &str, script: &str, tty: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec!["ssh".into()];
    argv.extend(SSH_OPTIONS.iter().map(|s| s.to_string()));
    if tty {
        argv.push("-t".into());
    }
    argv.push(host.into());
    // ssh hands the rest to the login shell as one string, so it is quoted once here
    argv.push(format!("bash -c {}", quote(script)));
    argv
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

impl Remote for Ssh {
    fn capture(&self, script: &str) -> Result<Captured> {
        let argv = ssh_argv(&self.host, script, false);
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .context("could not start ssh")?;
        Ok(Captured {
            code: exit_code(out.status),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn stream(&self, script: &str, tty: bool) -> Result<i32> {
        let argv = ssh_argv(&self.host, script, tty);
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .context("could not start ssh")?;
        Ok(exit_code(status))
    }
}
