//! A local command as data: what to run, what it reads, what it printed.

use crate::domain::sanitize::sanitize;
use anyhow::{Result, bail};

/// One environment variable for a child: a name that is a plain identifier and a value without a
/// NUL, so a value cannot cut the variable short or smuggle in a second one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVar {
    name: String,
    value: String,
}

impl EnvVar {
    pub fn parse(name: &str, value: &str) -> Result<Self> {
        let mut chars = name.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("not an environment variable name: {name:?}");
        }
        if value.contains('\0') {
            bail!("the value of {name} has a NUL");
        }
        Ok(Self {
            name: name.to_string(),
            value: value.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A program and its arguments. The program is a required field, so there is no empty argv and
/// no `argv[0]` to index. The child inherits `rr`'s environment, plus the variables given with
/// [`Argv::with_env`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argv {
    program: String,
    args: Vec<String>,
    envs: Vec<EnvVar>,
}

impl Argv {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            envs: Vec::new(),
        }
    }

    /// The same command with `var` set in the child's environment.
    #[must_use]
    pub fn with_env(mut self, var: EnvVar) -> Self {
        self.envs.push(var);
        self
    }

    pub fn envs(&self) -> &[EnvVar] {
        &self.envs
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

/// Bytes a child reads on standard input to their end (a script, an archive): the type says "this
/// is data for stdin" where a call takes one.
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

/// What a command printed, byte for byte on stdout (an archive is not text), and how it ended.
#[derive(Clone, Debug)]
pub struct CapturedBytes {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
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
    fn an_environment_variable_has_a_plain_name_and_a_value_without_nul() {
        assert!(EnvVar::parse("GIT_INDEX_FILE", "/tmp/a b/index").is_ok());
        assert!(EnvVar::parse("_x1", "").is_ok());
        for name in ["", "1A", "A=B", "A B", "A\0", "a-b", "A\n"] {
            assert!(EnvVar::parse(name, "v").is_err(), "{name:?}");
        }
        assert!(EnvVar::parse("A", "v\0w").is_err());
        let argv = Argv::new("git", vec![]).with_env(EnvVar::parse("A", "1").unwrap());
        assert_eq!(argv.envs()[0].name(), "A");
        assert_eq!(argv.envs()[0].value(), "1");
        assert_ne!(argv, Argv::new("git", vec![]));
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
