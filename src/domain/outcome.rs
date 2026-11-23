//! What a command produced, as data, and the one pure function that turns it into text and an exit
//! code. Commands never print and never pick an exit code; `main` prints what [`render`] returns.

use crate::domain::exit::{Exit, RemoteExit, RrCode};
use crate::domain::fwd::Closed;
use crate::domain::ids::Digest;
use crate::domain::json;
use crate::domain::pull::{Next, PullOutcome, next};
use crate::domain::sanitize::{error_line, sanitize};
use crate::domain::slurm::{JobStatus, WaitOutcome};
use crate::domain::snapshot::{GcOutcome, Refusal, gc_lines};
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
    /// A forward that `rr` closed (`rr fwd`): its time to live ran out, or `rr` was interrupted.
    Forwarded(Closed),
    /// A snapshot guard said no, so nothing was run (`rr run --mode snapshot`).
    SnapshotRefused(Refusal),
    /// `rr gc` cleaned the snapshot root, or refused to touch it.
    Gc(GcOutcome),
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
            match json::document(outcome, exit) {
                Ok(document) => Rendered {
                    exit,
                    stdout: format!("{document}\n"),
                    stderr: String::new(),
                },
                Err(err) => render_error(&anyhow::Error::new(err), Format::Json),
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
        Outcome::Forwarded(Closed::Expired) => Rendered::exiting(Exit::Success),
        Outcome::Forwarded(Closed::Interrupted) => Rendered::exiting(Exit::Rr(RrCode::Interrupted)),
        Outcome::Forwarded(Closed::SshEnded(code)) => Rendered::exiting(code.exit()),
        Outcome::SnapshotRefused(refusal) => Rendered {
            exit: Exit::Rr(RrCode::Refused),
            stdout: String::new(),
            stderr: lines(&[format!("rr: refused: {}", sanitize(&refusal.to_string()))]),
        },
        Outcome::Gc(outcome) => render_gc(outcome),
    }
}

/// The line that says some entries were set aside but their removal did not finish.
const GC_HALF_REMOVED: &str =
    "rr: some entries were set aside but not fully removed; run `rr gc` again";
/// The line that says some entries could not be set aside, or put back after a job appeared.
/// After a refusal: nothing was deleted, but an entry set aside could not be put back.
const GC_STRANDED: &str = "rr: nothing was deleted, but an entry could not be put back and may exist only as `.trash-<id>`; run `rr gc` again";

const GC_NOT_SET_ASIDE: &str = "rr: some entries could not be set aside or put back and were left as they are; run `rr gc` again";

/// A clean-up prints its report on stdout. Entries left half removed, or that a step could not
/// move, make it a failure (the clean-up did not do what it was asked), and a refusal is a refusal
/// like any other (nothing was deleted).
fn render_gc(outcome: &GcOutcome) -> Rendered {
    match outcome {
        GcOutcome::Cleaned(report) => {
            let mut notes = Vec::new();
            if !report.failed.is_empty() {
                notes.push(GC_HALF_REMOVED.to_string());
            }
            if report.errored().next().is_some() {
                notes.push(GC_NOT_SET_ASIDE.to_string());
            }
            let exit = if notes.is_empty() {
                Exit::Success
            } else {
                Exit::Rr(RrCode::Failure)
            };
            Rendered {
                exit,
                stdout: lines(&gc_lines(report)),
                stderr: lines(&notes),
            }
        }
        GcOutcome::Refused(why) => Rendered {
            exit: Exit::Rr(RrCode::Refused),
            stdout: String::new(),
            stderr: lines(&[format!("rr: refused: {why}")]),
        },
        // a failure, not a refusal: an entry may be missing under its own name
        GcOutcome::Stranded { why, report } => Rendered {
            exit: Exit::Rr(RrCode::Failure),
            stdout: lines(&gc_lines(report)),
            stderr: lines(&[format!("rr: refused: {why}"), GC_STRANDED.to_string()]),
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
        WaitOutcome::TimedOut { rows, unlisted } => {
            let mut report = states(rows);
            report.extend(
                unlisted
                    .iter()
                    .map(|job| format!("rr: {} not listed by sacct", sanitize(job.as_str()))),
            );
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

/// What `main` prints when a command returned an error: every message may embed remote or file
/// text, so it passes through [`sanitize`] whole. In text, one line on stderr; in JSON, one error
/// document on stdout (and nothing on stderr), so a script that reads stdout always gets a document.
pub fn render_error(err: &anyhow::Error, format: Format) -> Rendered {
    render_error_as(err, format, RrCode::Failure)
}

/// [`render_error`] with the code `rr` exits with given, for an error that is not a plain failure
/// (a command line clap refused is [`RrCode::Usage`]).
pub fn render_error_as(err: &anyhow::Error, format: Format, code: RrCode) -> Rendered {
    let exit = Exit::Rr(code);
    match format {
        Format::Text => Rendered {
            exit,
            stdout: String::new(),
            stderr: lines(&[error_line(err)]),
        },
        Format::Json => Rendered {
            exit,
            stdout: format!("{}\n", json::error_document(exit, &format!("{err:#}"))),
            stderr: String::new(),
        },
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
                "rr: the environment is out of date or uv failed (the uv.lock check on the remote \
                 did not pass): if it is out of date, run `uv sync` there (it may download \
                 several GB)"
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
    fn a_refused_snapshot_is_an_rr_refusal_and_its_text_is_sanitized() {
        let refused = text(&Outcome::SnapshotRefused(Refusal::SecretNames(vec![
            ".env".into(),
            "k\u{1b}]0;x\u{7}ey.pem".into(),
        ])));
        assert_eq!(refused.exit, Exit::Rr(RrCode::Refused));
        assert_eq!(refused.stdout, "");
        assert!(
            refused
                .stderr
                .starts_with("rr: refused: untracked files with secret")
        );
        assert!(refused.stderr.contains(".env"));
        assert!(!refused.stderr.contains('\u{1b}') && !refused.stderr.contains('\u{7}'));
        assert_eq!(refused.stderr.lines().count(), 1);
        let remote = text(&Outcome::SnapshotRefused(Refusal::RemoteNotPrivate {
            stderr: "\u{1b}[2Jnot yours\n".into(),
        }));
        assert!(remote.stderr.contains("?[2Jnot yours"), "{}", remote.stderr);
    }

    #[test]
    fn a_clean_up_that_could_not_move_an_entry_fails() {
        use crate::domain::snapshot::{GcReport, Kept, RootEntry, Why};
        let stuck = GcReport {
            kept: vec![Kept {
                entry: RootEntry::parse(A).unwrap(),
                why: Why::Error,
            }],
            ..GcReport::default()
        };
        let stuck = text(&Outcome::Gc(GcOutcome::Cleaned(stuck)));
        assert_eq!(
            stuck.exit,
            Exit::Rr(RrCode::Failure),
            "an entry that could not be moved"
        );
        assert!(
            stuck.stdout.contains(&format!("kept {A}")),
            "{}",
            stuck.stdout
        );
        assert!(
            stuck.stderr.contains("could not be set aside"),
            "{}",
            stuck.stderr
        );
    }

    #[test]
    fn a_refusal_that_left_an_entry_unrestored_is_a_failure_naming_it() {
        use crate::domain::snapshot::{GcRefusal, GcReport, Kept, RootEntry, Why};
        let report = GcReport {
            kept: vec![Kept {
                entry: RootEntry::parse(A).unwrap(),
                why: Why::Error,
            }],
            ..GcReport::default()
        };
        let shown = text(&Outcome::Gc(GcOutcome::Stranded {
            why: GcRefusal::QueueUnreadable,
            report,
        }));
        assert_eq!(shown.exit, Exit::Rr(RrCode::Failure));
        assert!(
            shown.stdout.contains(&format!("kept {A}")),
            "{}",
            shown.stdout
        );
        assert!(
            shown.stderr.contains("refused: could not tell"),
            "{}",
            shown.stderr
        );
        assert!(shown.stderr.contains("put back"), "{}", shown.stderr);
    }

    #[test]
    fn a_clean_up_prints_its_report_and_a_refusal_prints_why_nothing_was_deleted() {
        use crate::domain::snapshot::{GcRefusal, GcReport, Kept, RootEntry, Why};
        let entry = |name: &str| RootEntry::parse(name).unwrap();
        let report = GcReport {
            removed: vec![entry(A)],
            kept: vec![Kept {
                entry: entry(B),
                why: Why::Newest,
            }],
            failed: vec![],
        };
        let done = text(&Outcome::Gc(GcOutcome::Cleaned(report.clone())));
        assert_eq!(done.exit, Exit::Success);
        assert_eq!(
            done.stdout,
            format!(
                "removed {A}\nkept {B} (one of the newest kept by --keep)\n1 removed, 1 kept, 0 failed\n"
            )
        );
        assert_eq!(done.stderr, "");
        let partly = GcReport {
            failed: vec![entry(".trash-x")],
            ..report
        };
        let partly = text(&Outcome::Gc(GcOutcome::Cleaned(partly)));
        assert_eq!(partly.exit, Exit::Rr(RrCode::Failure));
        assert!(
            partly.stdout.contains("failed .trash-x"),
            "{}",
            partly.stdout
        );
        assert!(partly.stderr.starts_with("rr: some entries were set aside"));
        let refused = text(&Outcome::Gc(GcOutcome::Refused(GcRefusal::QueueUnreadable)));
        assert_eq!(refused.exit, Exit::Rr(RrCode::Refused));
        assert_eq!(refused.stdout, "");
        assert!(
            refused
                .stderr
                .starts_with("rr: refused: could not tell which snapshots")
        );
        assert_eq!(refused.stderr.lines().count(), 1);
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
                .starts_with("rr: the environment is out of date or uv failed")
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
        let rendered = render_error(&err, Format::Text);
        assert_eq!(rendered.exit, Exit::Rr(RrCode::Failure));
        assert_eq!(rendered.stderr, "rr: sacct failed: remote said ?[2J\n");
        assert_eq!(rendered.stdout, "");
    }

    /// A script that reads stdout under `--json` always gets one document, error or not.
    #[test]
    fn under_json_an_error_is_one_sanitized_document_on_stdout() {
        let err = anyhow::anyhow!("remote said \u{1b}[2J").context("sacct failed");
        let rendered = render_error(&err, Format::Json);
        assert_eq!(rendered.exit, Exit::Rr(RrCode::Failure));
        assert_eq!(rendered.stderr, "");
        assert!(rendered.stdout.ends_with('\n'));
        assert_eq!(rendered.stdout.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(&rendered.stdout).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "source": "rr",
                "exit": 1,
                "kind": "error",
                "message": "sacct failed: remote said ?[2J",
            })
        );
    }

    #[test]
    fn a_usage_error_keeps_clap_s_code_in_both_formats() {
        let err = anyhow::anyhow!("unexpected argument '--bogus'");
        let text = render_error_as(&err, Format::Text, RrCode::Usage);
        assert_eq!(text.exit.code(), 2);
        let json = render_error_as(&err, Format::Json, RrCode::Usage);
        assert_eq!(json.exit.code(), 2);
        let value: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        assert_eq!(value["exit"], 2);
        assert_eq!(value["kind"], "error");
        assert_eq!(value["source"], "rr");
    }

    #[test]
    fn a_wait_prints_one_line_per_job_and_exits_with_its_own_code() {
        use crate::domain::slurm::{JobId, JobState, SqueueId};
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
        let late = waited(WaitOutcome::TimedOut {
            rows,
            unlisted: vec![JobId::parse("9").unwrap()],
        });
        assert_eq!(late.exit, Exit::Rr(RrCode::Timeout));
        assert!(
            late.stderr
                .ends_with("rr: 9 not listed by sacct\nrr: timed out; the jobs are still active\n")
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
    fn a_forward_that_rr_closed_is_an_rr_exit_not_a_remote_one() {
        let expired = text(&Outcome::Forwarded(Closed::Expired));
        assert_eq!(expired.exit, Exit::Success);
        let interrupted = text(&Outcome::Forwarded(Closed::Interrupted));
        assert_eq!(interrupted.exit, Exit::Rr(RrCode::Interrupted));
        assert_ne!(interrupted.exit, Exit::remote(130));
        let lost = text(&Outcome::Forwarded(Closed::SshEnded(RemoteExit::new(255))));
        assert_eq!(lost.exit, Exit::remote(255));
        assert_eq!((lost.stdout, lost.stderr), (String::new(), String::new()));
        assert_eq!(interrupted.exit.code(), 130);
        // the notes port already said why; nothing is written twice
        assert_eq!(
            (interrupted.stdout, interrupted.stderr),
            (String::new(), String::new())
        );
    }

    #[test]
    fn the_log_note_is_sanitized() {
        assert_eq!(log_path_note("/o/\u{1b}x.out"), "rr: /o/?x.out");
    }
}
