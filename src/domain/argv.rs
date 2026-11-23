//! A local command as data: what to run, what it reads, what it printed.

use crate::domain::sanitize::sanitize;
use anyhow::{Result, bail};

/// A program and its arguments. The program is a required field, so there is no empty argv and
/// no `argv[0]` to index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argv {
    program: String,
    args: Vec<String>,
}

impl Argv {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// What a child process reads on standard input while its output is captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stdin {
    /// Closed at once: the child can never wait for a keystroke nobody sees.
    Null,
    /// The terminal's.
    Inherit,
}

/// Bytes a child reads on standard input to their end (a script, an archive). A type of its own,
/// so a call that feeds a child cannot be confused with one that leaves stdin to the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload(Vec<u8>);

impl Payload {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// What a command printed and how it ended.
#[derive(Clone, Debug)]
pub struct Captured {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    /// What the command printed, if it exited 0; otherwise an error naming `what`, the code and
    /// the command's stderr (sanitized: it is remote or tool text).
    pub fn into_stdout(self, what: &str) -> Result<String> {
        if self.code != 0 {
            bail!(
                "{what} failed ({}): {}",
                self.code,
                sanitize(self.stderr.trim())
            );
        }
        Ok(self.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn into_stdout_gives_the_output_of_a_command_that_succeeded() {
        let out = captured(0, "data\n", "noise").into_stdout("sacct").unwrap();
        assert_eq!(out, "data\n");
    }

    #[test]
    fn into_stdout_turns_a_failure_into_an_error_with_the_sanitized_stderr() {
        let err = captured(23, "partial", " boom\u{1b}[2J\n")
            .into_stdout("rsync --dry-run")
            .unwrap_err()
            .to_string();
        assert_eq!(err, "rsync --dry-run failed (23): boom?[2J");
        assert!(
            captured(-1, "", "").into_stdout("x").is_err(),
            "a signal is a failure"
        );
    }
}
