//! How a run ends, as a type: `rr`'s own codes and a remote command's code are different variants,
//! so a pass-through code can never be mistaken for a refusal (or the reverse) inside the program.
//! They share the byte the process exits with, so only the process boundary ([`Exit::code`])
//! sees one number.

use std::num::NonZeroU8;

/// The failures `rr` itself reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RrCode {
    /// The command line was not understood. clap's own code, kept so a usage error reads the same
    /// with and without `--json`.
    Usage,
    /// `rr` could not do what was asked (an error, not a guard).
    Failure,
    /// A guard said no (or the remote environment is stale): nothing was done.
    Refused,
    /// `rr wait` gave up: the jobs were still running when `--timeout` ended.
    Timeout,
    /// A job ended in a state other than COMPLETED.
    JobFailed,
    /// `rr` could not learn how a job ended (ssh or sacct unreachable, no job found). Never read
    /// as a failure of the job.
    Unknown,
    /// `rr` was interrupted (Ctrl-C, SIGTERM, SIGHUP) and ended what it had started. 128 plus
    /// SIGINT, the code a shell reports for Ctrl-C.
    Interrupted,
}

impl RrCode {
    pub const fn code(self) -> u8 {
        match self {
            RrCode::Usage => 2,
            RrCode::Failure => 1,
            RrCode::Refused => 3,
            RrCode::Timeout => 4,
            RrCode::JobFailed => 5,
            RrCode::Unknown => 6,
            RrCode::Interrupted => 130,
        }
    }
}

/// What the process exits with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Everything `rr` set out to do was done, and a command it ran (if any) exited 0.
    Success,
    /// `rr` stopped by itself.
    Rr(RrCode),
    /// A command `rr` ran (ssh, rsync, the user's program) exited with this non-zero code.
    Remote(NonZeroU8),
}

impl Exit {
    /// A child's exit code. 0 is success; a code that does not fit in a byte (and a child killed by
    /// a signal, which has none) is a plain failure, never success.
    pub fn remote(code: i32) -> Self {
        match u8::try_from(code).ok().map(NonZeroU8::new) {
            Some(None) => Exit::Success,
            Some(Some(code)) => Exit::Remote(code),
            None => Exit::Remote(NonZeroU8::MIN),
        }
    }

    /// The byte the process exits with.
    pub fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Rr(code) => code.code(),
            Exit::Remote(code) => code.get(),
        }
    }
}

/// The exit code of a command that ran on the remote host, as `ssh` reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteExit(i32);

impl RemoteExit {
    pub fn new(code: i32) -> Self {
        Self(code)
    }

    pub fn exit(self) -> Exit {
        Exit::remote(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_outside_a_byte_is_a_failure_and_zero_is_success() {
        let code = |n| Exit::remote(n).code();
        assert_eq!((code(0), code(75), code(255)), (0, 75, 255));
        assert_eq!((code(256), code(-1)), (1, 1));
        assert_eq!(Exit::remote(0), Exit::Success);
    }

    /// A remote command that exits 3 (or 1) shares the number with an `rr` refusal (or failure) but
    /// is a different value, so code that branches on the variant cannot confuse them.
    #[test]
    fn a_remote_code_is_never_an_rr_code() {
        assert_eq!(Exit::remote(3).code(), RrCode::Refused.code());
        assert_ne!(Exit::remote(3), Exit::Rr(RrCode::Refused));
        assert_ne!(Exit::remote(1), Exit::Rr(RrCode::Failure));
        assert_eq!((RrCode::Failure.code(), RrCode::Refused.code()), (1, 3));
    }

    /// 2 is clap's usage error (`RrCode::Usage`); 126, 127, 128+n and 255 belong to shells and ssh.
    #[test]
    fn the_wait_codes_avoid_every_other_meaning() {
        let codes = [RrCode::Timeout, RrCode::JobFailed, RrCode::Unknown].map(RrCode::code);
        assert_eq!(codes, [4, 5, 6]);
    }

    #[test]
    fn an_interrupt_is_an_rr_code_that_shares_the_number_with_a_remote_130() {
        assert_eq!(Exit::Rr(RrCode::Interrupted).code(), 130);
        assert_ne!(Exit::Rr(RrCode::Interrupted), Exit::remote(130));
    }

    #[test]
    fn the_remote_exit_keeps_what_ssh_said() {
        assert_eq!(RemoteExit::new(0).exit(), Exit::Success);
        assert_eq!(RemoteExit::new(9).exit().code(), 9);
    }
}
