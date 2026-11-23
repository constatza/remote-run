//! What a command produced, as data, and the one pure function that turns it into text and an exit
//! code. Commands never print and never pick an exit code; `main` prints what [`render`] returns.

use crate::domain::exit::{Exit, RemoteExit, RrCode};
use crate::domain::ids::Digest;
use crate::domain::json;
use crate::domain::pull::{Next, PullOutcome, next};
use crate::domain::sanitize::{error_line, sanitize};
use crate::domain::slurm::{JobStatus, WaitOutcome};
use crate::domain::status::{LogTail, StatusReport};
use crate::domain::transfer::Transfer;
use std::path::PathBuf;

/// How a command ended, as data.
#[derive(Debug)]
pub enum Outcome {
    /// A command ran on the remote host (`q`, `acct`, `cancel`, `log`, `x`, `run`) and ended so.
    Remote(RemoteExit),
    /// A pull ended so, and nothing ran after it: `rr pull`, or `rr run` stopped by a refusal.
    Pulled(PullOutcome),
    /// A copy ended so.
    Transferred(Transfer),
    /// The project file `rr trust` showed or recorded.
    Trust(TrustReport),
    /// A wait ended so: `rr wait`, or `rr run --wait` after its command.
    Waited(WaitOutcome),
    /// One job's status, from `rr status`.
    Status(StatusReport),
}

/// What `rr trust` did with a project file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    /// Only shown: the user has not asked to trust it.
    Shown,
    /// Recorded as reviewed.
    Recorded,
}

/// A project file as `rr trust` shows it: its canonical path, the digest that is (or would be)
/// recorded, and the text that digest covers.
#[derive(Debug)]
pub struct TrustReport {
    pub path: PathBuf,
    pub digest: Digest,
    pub text: String,
    pub state: TrustState,
}

/// How the result is written. [`render`] matches it exhaustively, so a new format must be handled
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    /// One JSON object on stdout; the exit code is the same as in text.
    Json,
}

/// What `main` writes and exits with.
#[derive(Debug, PartialEq, Eq)]
pub struct Rendered {
    pub exit: Exit,
    pub stdout: String,
    pub stderr: String,
}

impl Rendered {
    fn exiting(exit: Exit) -> Self {
        Self {
            exit,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Text and exit code of `outcome` (pure). Every piece of remote or file text passes through
/// [`sanitize`] here, so nothing a remote host or a file wrote reaches the terminal as a control
/// sequence.
pub fn render(outcome: &Outcome, format: Format) -> Rendered {
    match format {
        Format::Text => render_text(outcome),
        Format::Json => {
            let exit = render_text(outcome).exit;
            Rendered {
                exit,
                stdout: format!("{}\n", json::document(outcome, exit)),
                stderr: String::new(),
            }
        }
    }
}

fn render_text(outcome: &Outcome) -> Rendered {
    match outcome {
        Outcome::Remote(exit) => Rendered::exiting(exit.exit()),
        Outcome::Pulled(pulled) => Rendered {
            exit: match next(pulled) {
                Next::RunCommand => Exit::Success,
                Next::Stop(code) => Exit::Rr(code),
            },
            stdout: String::new(),
            stderr: lines(&pull_report(pulled)),
        },
        Outcome::Transferred(transfer) => Rendered {
            exit: transfer.exit(),
            stdout: String::new(),
            stderr: match transfer {
                Transfer::Ran(_) => String::new(),
                Transfer::OverLimit { .. } => lines(&[format!("rr: {transfer}")]),
            },
        },
        Outcome::Trust(report) => render_trust(report),
        Outcome::Waited(waited) => Rendered {
            exit: waited.exit(),
            stdout: String::new(),
            stderr: lines(&wait_report(waited)),
        },
        Outcome::Status(report) => Rendered {
            exit: Exit::Success,
            stdout: lines(&status_report(report)),
            stderr: String::new(),
        },
    }
}

/// What `rr status` prints: the job's progress, then the end of its log. All remote text.
pub fn status_report(report: &StatusReport) -> Vec<String> {
    let (status, record) = (&report.status, &report.status.record);
    let mut out = vec![
        format!("job {} ({})", sanitize(&record.id), sanitize(&record.name)),
        format!("state {}", sanitize(status.state.name())),
        format!("elapsed {}", sanitize(&status.elapsed)),
        format!("exit {}", sanitize(&status.exit_code)),
        format!("dir {}", sanitize(&record.workdir)),
    ];
    match &report.log {
        LogTail::Lines { path, lines } => {
            out.push(format!("log {}", sanitize(path)));
            out.extend(lines.iter().map(|line| sanitize(line)));
        }
        LogTail::Unavailable { why } => out.push(format!("log unavailable: {}", sanitize(why))),
    }
    out
}

/// One line per job (`rr: 123 RUNNING`), then why the wait stopped when that is not plain from the
/// states. Job ids and states are remote text.
pub fn wait_report(outcome: &WaitOutcome) -> Vec<String> {
    let states = |rows: &[JobStatus]| -> Vec<String> {
        let line = |row: &JobStatus| {
            let state = row.state.name();
            format!("rr: {} {}", sanitize(row.id.as_str()), sanitize(state))
        };
        rows.iter().map(line).collect()
    };
    match outcome {
        WaitOutcome::Completed(rows) | WaitOutcome::Failed(rows) => states(rows),
        WaitOutcome::TimedOut(rows) => {
            let mut report = states(rows);
            report.push("rr: timed out; the jobs are still active".to_string());
            report
        }
        WaitOutcome::Unknown { why } => vec![format!(
            "rr: could not tell how the jobs ended: {}",
            sanitize(why)
        )],
    }
}

fn render_trust(report: &TrustReport) -> Rendered {
    let shown = sanitize(&report.path.display().to_string());
    let stderr = match report.state {
        TrustState::Shown => {
            "rr: not trusted yet; if the text above is right, run `rr trust --yes`".to_string()
        }
        TrustState::Recorded => format!("rr: trusted {shown}"),
    };
    Rendered {
        exit: Exit::Success,
        stdout: format!(
            "{shown}\nsha256 {}\n---\n{}",
            report.digest,
            sanitize(&report.text)
        ),
        stderr: lines(&[stderr]),
    }
}

/// The failure line `main` prints when a command returned an error: every message may embed
/// remote or file text, so it passes through [`sanitize`] whole.
pub fn render_error(err: &anyhow::Error) -> Rendered {
    Rendered {
        exit: Exit::Rr(RrCode::Failure),
        stdout: String::new(),
        stderr: lines(&[error_line(err)]),
    }
}

/// The lines that tell the user how a pull ended; what it means for a command that follows is
/// [`next`]. Empty when the checkout was already current.
pub fn pull_report(outcome: &PullOutcome) -> Vec<String> {
    match outcome {
        PullOutcome::UpToDate => vec![],
        PullOutcome::Advanced { from, to } => vec![format!("rr: remote checkout {from} -> {to}")],
        PullOutcome::Refused(reason) => {
            vec![format!("rr: refused: {}", sanitize(&reason.to_string()))]
        }
        PullOutcome::Stale { stderr } => {
            let mut report = vec![
                "rr: the remote environment does not match uv.lock: run `uv sync` on the remote \
                 (it may download several GB)"
                    .to_string(),
            ];
            if !stderr.is_empty() {
                report.push(sanitize(stderr));
            }
            report
        }
    }
}

/// The line that names the log file before it is shown.
pub fn log_path_note(path: &str) -> String {
    format!("rr: {}", sanitize(path))
}

fn lines(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::Commit;
    use crate::domain::pull::Reason;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn text(outcome: &Outcome) -> Rendered {
        render(outcome, Format::Text)
    }

    fn pulled(outcome: PullOutcome) -> Rendered {
        text(&Outcome::Pulled(outcome))
    }

    #[test]
    fn a_remote_code_passes_through_and_is_not_an_rr_code() {
        let ran = |code| text(&Outcome::Remote(RemoteExit::new(code)));
        assert_eq!(ran(0).exit, Exit::Success);
        assert_eq!(ran(7).exit.code(), 7);
        assert_eq!(ran(3).exit, Exit::remote(3));
        assert_ne!(ran(3).exit, Exit::Rr(RrCode::Refused));
        assert_ne!(ran(1).exit, Exit::Rr(RrCode::Failure));
        assert_eq!(ran(-1).exit.code(), 1, "a signal-killed child is a failure");
        assert_eq!(
            (ran(7).stdout, ran(7).stderr),
            (String::new(), String::new())
        );
    }

    #[test]
    fn a_pull_that_reached_the_target_exits_zero_and_one_that_did_not_is_refused() {
        let advanced = PullOutcome::Advanced {
            from: Commit::parse(A).unwrap(),
            to: Commit::parse(B).unwrap(),
        };
        let done = pulled(advanced);
        assert_eq!(done.exit, Exit::Success);
        assert_eq!(
            done.stderr,
            "rr: remote checkout aaaaaaaaaaaa -> bbbbbbbbbbbb\n"
        );
        let current = pulled(PullOutcome::UpToDate);
        assert_eq!((current.exit, current.stderr.as_str()), (Exit::Success, ""));
        let stale = pulled(PullOutcome::Stale {
            stderr: "Would update\u{1b}[2J 3 packages".into(),
        });
        assert_eq!(stale.exit, Exit::Rr(RrCode::Refused));
        assert!(
            stale
                .stderr
                .starts_with("rr: the remote environment does not match uv.lock")
        );
        assert!(
            stale.stderr.ends_with("Would update?[2J 3 packages\n"),
            "{}",
            stale.stderr
        );
        let dirty = pulled(PullOutcome::Refused(Reason::RemoteDirty(vec![
            "?? \u{1b}[2Jx".into(),
        ])));
        assert_eq!(dirty.exit, Exit::Rr(RrCode::Refused));
        assert!(
            dirty
                .stderr
                .starts_with("rr: refused: the remote checkout has uncommitted")
        );
        assert!(!dirty.stderr.contains('\u{1b}'));
    }

    #[test]
    fn a_checkout_that_changed_mid_pull_is_a_refusal_with_its_reason() {
        let stderr = "fatal: bad object\u{1b}[2J\n".to_string();
        let changed = pulled(PullOutcome::Refused(Reason::StateChanged { stderr }));
        assert_eq!(changed.exit, Exit::Rr(RrCode::Refused));
        assert!(
            changed
                .stderr
                .starts_with("rr: refused: the remote checkout changed")
        );
        assert!(
            changed
                .stderr
                .contains("the remote said:\nfatal: bad object?[2J"),
            "the remote's stderr is shown, sanitized: {:?}",
            changed.stderr
        );
        assert!(!changed.stderr.contains('\u{1b}'));
    }

    #[test]
    fn a_stale_report_without_uv_output_is_one_line() {
        let stale = pulled(PullOutcome::Stale {
            stderr: String::new(),
        });
        assert_eq!(stale.stderr.lines().count(), 1);
    }

    #[test]
    fn a_copy_over_the_limit_is_refused_and_says_so() {
        let over = text(&Outcome::Transferred(Transfer::OverLimit {
            total: 100,
            limit: 99,
        }));
        assert_eq!(over.exit, Exit::Rr(RrCode::Refused));
        assert!(over.stderr.starts_with("rr: refused: 100 bytes to copy"));
        let ran = text(&Outcome::Transferred(Transfer::Ran(5)));
        assert_eq!((ran.exit.code(), ran.stderr.as_str()), (5, ""));
        assert_ne!(ran.exit, Exit::Rr(RrCode::Failure));
        assert_eq!(
            text(&Outcome::Transferred(Transfer::Ran(0))).exit,
            Exit::Success
        );
    }

    #[test]
    fn trust_prints_the_sanitized_file_and_says_what_happened() {
        let report = |state| TrustReport {
            path: PathBuf::from("/p/.rr.toml"),
            digest: crate::domain::ids::digest(b"x"),
            text: "host = \"h\"\u{1b}[2J\n".into(),
            state,
        };
        let shown = text(&Outcome::Trust(report(TrustState::Shown)));
        let hash = crate::domain::ids::digest(b"x");
        assert_eq!(
            shown.stdout,
            format!("/p/.rr.toml\nsha256 {hash}\n---\nhost = \"h\"?[2J\n")
        );
        assert_eq!(
            shown.stderr,
            "rr: not trusted yet; if the text above is right, run `rr trust --yes`\n"
        );
        let recorded = text(&Outcome::Trust(report(TrustState::Recorded)));
        assert_eq!(recorded.stdout, shown.stdout);
        assert_eq!(recorded.stderr, "rr: trusted /p/.rr.toml\n");
        assert_eq!((shown.exit, recorded.exit), (Exit::Success, Exit::Success));
    }

    #[test]
    fn an_error_is_one_sanitized_line_and_a_failure() {
        let err = anyhow::anyhow!("remote said \u{1b}[2J").context("sacct failed");
        let rendered = render_error(&err);
        assert_eq!(rendered.exit, Exit::Rr(RrCode::Failure));
        assert_eq!(rendered.stderr, "rr: sacct failed: remote said ?[2J\n");
        assert_eq!(rendered.stdout, "");
    }

    #[test]
    fn a_wait_prints_one_line_per_job_and_exits_with_its_own_code() {
        use crate::domain::slurm::{JobState, SqueueId};
        let row = |id: &str, state: &str| JobStatus {
            id: SqueueId::parse(id).unwrap(),
            state: JobState::parse(state),
        };
        let rows = vec![row("7", "COMPLETED"), row("8", "FAILED\u{1b}[2J")];
        let waited = |outcome| text(&Outcome::Waited(outcome));
        let done = waited(WaitOutcome::Completed(rows.clone()));
        assert_eq!(done.exit, Exit::Success);
        let failed = waited(WaitOutcome::Failed(rows.clone()));
        assert_eq!(failed.exit, Exit::Rr(RrCode::JobFailed));
        assert_eq!(
            failed.stderr,
            "rr: 7 COMPLETED\nrr: 8 ?[2J\n".replace("?[2J", "FAILED?[2J")
        );
        let late = waited(WaitOutcome::TimedOut(rows));
        assert_eq!(late.exit, Exit::Rr(RrCode::Timeout));
        assert!(
            late.stderr
                .ends_with("rr: timed out; the jobs are still active\n")
        );
        let unknown = waited(WaitOutcome::Unknown {
            why: "no\u{1b}[2J".into(),
        });
        assert_eq!(unknown.exit, Exit::Rr(RrCode::Unknown));
        assert_eq!(
            unknown.stderr,
            "rr: could not tell how the jobs ended: no?[2J\n"
        );
        assert_eq!(unknown.stdout, "");
    }

    #[test]
    fn the_log_note_is_sanitized() {
        assert_eq!(log_path_note("/o/\u{1b}x.out"), "rr: /o/?x.out");
    }
}
