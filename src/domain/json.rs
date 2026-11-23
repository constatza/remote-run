//! The machine-readable form of an [`Outcome`], one JSON object. `serde_json` does the escaping,
//! so remote text cannot break the document; it still goes through [`sanitize`] first, because JSON
//! leaves bidi controls and the like as they are and a person may read the output.
//!
//! Every object says which program decided the exit code (`"source"`), because a remote command's
//! own code can be 1, 3 or 6 as well: `"rr"` means `rr` stopped by itself, `"remote"` that this is
//! what the command it ran returned.

use crate::domain::exit::Exit;
use crate::domain::outcome::{Outcome, TrustState};
use crate::domain::pull::{PullOutcome, Reason};
use crate::domain::sanitize::sanitize;
use crate::domain::slurm::{JobStatus, WaitOutcome};
use crate::domain::status::{LogTail, StatusReport};
use crate::domain::transfer::Transfer;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Rr,
    Remote,
}

#[derive(Serialize)]
struct Document {
    source: Source,
    exit: u8,
    #[serde(flatten)]
    body: Body,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Body {
    /// `x`, `run`, `log` and the like: only how it ended.
    Command,
    Pull(PullBody),
    Copy(CopyBody),
    Trust {
        path: String,
        sha256: String,
        state: TrustJson,
        text: String,
    },
    Wait(WaitBody),
    Status(StatusBody),
}

#[derive(Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum PullBody {
    UpToDate,
    Advanced {
        from: String,
        to: String,
    },
    Refused {
        reason: RefusalReason,
        detail: String,
    },
    Stale {
        detail: String,
    },
}

/// Why a pull was refused, as a name a script can match on.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RefusalReason {
    NotOnOrigin,
    RemoteDirty,
    JobsFromCheckout,
    StateChanged,
}

#[derive(Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum CopyBody {
    Ran,
    OverLimit { total: u64, limit: u64 },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum TrustJson {
    Shown,
    Recorded,
}

#[derive(Serialize)]
struct JobJson {
    id: String,
    state: String,
}

#[derive(Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum WaitBody {
    Completed { jobs: Vec<JobJson> },
    Failed { jobs: Vec<JobJson> },
    TimedOut { jobs: Vec<JobJson> },
    Unknown { why: String },
}

#[derive(Serialize)]
struct StatusBody {
    id: String,
    name: String,
    state: String,
    elapsed: String,
    exit_code: String,
    workdir: String,
    log: LogBody,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LogBody {
    Shown { path: String, lines: Vec<String> },
    Unavailable { why: String },
}

fn clean(text: &str) -> String {
    sanitize(text)
}

fn jobs(rows: &[JobStatus]) -> Vec<JobJson> {
    rows.iter()
        .map(|row| JobJson {
            id: clean(row.id.as_str()),
            state: clean(row.state.name()),
        })
        .collect()
}

fn pull(outcome: &PullOutcome) -> PullBody {
    match outcome {
        PullOutcome::UpToDate => PullBody::UpToDate,
        PullOutcome::Advanced { from, to } => PullBody::Advanced {
            from: from.as_str().into(),
            to: to.as_str().into(),
        },
        PullOutcome::Refused(reason) => PullBody::Refused {
            reason: match reason {
                Reason::NotOnOrigin(_) => RefusalReason::NotOnOrigin,
                Reason::RemoteDirty(_) => RefusalReason::RemoteDirty,
                Reason::JobsFromCheckout(_) => RefusalReason::JobsFromCheckout,
                Reason::StateChanged { .. } => RefusalReason::StateChanged,
            },
            detail: clean(&reason.to_string()),
        },
        PullOutcome::Stale { stderr } => PullBody::Stale {
            detail: clean(stderr),
        },
    }
}

fn wait(outcome: &WaitOutcome) -> WaitBody {
    match outcome {
        WaitOutcome::Completed(rows) => WaitBody::Completed { jobs: jobs(rows) },
        WaitOutcome::Failed(rows) => WaitBody::Failed { jobs: jobs(rows) },
        WaitOutcome::TimedOut(rows) => WaitBody::TimedOut { jobs: jobs(rows) },
        WaitOutcome::Unknown { why } => WaitBody::Unknown { why: clean(why) },
    }
}

fn status(report: &StatusReport) -> StatusBody {
    let (status, record) = (&report.status, &report.status.record);
    StatusBody {
        id: clean(&record.id),
        name: clean(&record.name),
        state: clean(status.state.name()),
        elapsed: clean(&status.elapsed),
        exit_code: clean(&status.exit_code),
        workdir: clean(&record.workdir),
        log: match &report.log {
            LogTail::Lines { path, lines } => LogBody::Shown {
                path: clean(path),
                lines: lines.iter().map(|line| clean(line)).collect(),
            },
            LogTail::Unavailable { why } => LogBody::Unavailable { why: clean(why) },
        },
    }
}

fn parts(outcome: &Outcome) -> (Source, Body) {
    match outcome {
        Outcome::Remote(_) => (Source::Remote, Body::Command),
        Outcome::Pulled(outcome) => (Source::Rr, Body::Pull(pull(outcome))),
        Outcome::Transferred(Transfer::Ran(_)) => (Source::Remote, Body::Copy(CopyBody::Ran)),
        Outcome::Transferred(Transfer::OverLimit { total, limit }) => (
            Source::Rr,
            Body::Copy(CopyBody::OverLimit {
                total: *total,
                limit: *limit,
            }),
        ),
        Outcome::Trust(report) => (
            Source::Rr,
            Body::Trust {
                path: clean(&report.path.display().to_string()),
                sha256: report.digest.to_string(),
                state: match report.state {
                    TrustState::Shown => TrustJson::Shown,
                    TrustState::Recorded => TrustJson::Recorded,
                },
                text: clean(&report.text),
            },
        ),
        Outcome::Waited(outcome) => (Source::Rr, Body::Wait(wait(outcome))),
        Outcome::Status(report) => (Source::Rr, Body::Status(status(report))),
    }
}

/// `outcome` as one line of JSON, with `exit` as the process will exit. A serialization error is
/// returned, never replaced by a stand-in document: a script reading stdout must be able to tell a
/// result from a failure to write one.
pub fn document(outcome: &Outcome, exit: Exit) -> serde_json::Result<String> {
    let (source, body) = parts(outcome);
    let document = Document {
        source,
        exit: exit.code(),
        body,
    };
    serde_json::to_string(&document)
}

/// The document for an error `rr` itself ended with, as one line. Built from a `Value`, whose text
/// form cannot fail, because this is what is written when writing the result failed.
pub fn error_document(exit: Exit, message: &str) -> String {
    serde_json::json!({
        "source": "rr",
        "exit": exit.code(),
        "kind": "error",
        "message": clean(message),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exit::{RemoteExit, RrCode};
    use crate::domain::ids::{Commit, digest};
    use crate::domain::outcome::{Format, TrustReport, render};
    use crate::domain::pull::Target;
    use crate::domain::slurm::{JobState, SqueueId};
    use crate::domain::status::Status;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// What the process prints and exits with in `--json` mode, parsed back.
    fn json_of(outcome: &Outcome) -> (Value, Exit) {
        let rendered = render(outcome, Format::Json);
        assert_eq!(rendered.stdout.lines().count(), 1, "one line");
        assert!(rendered.stdout.ends_with('\n'));
        assert_eq!(rendered.stderr, "");
        let value = serde_json::from_str(&rendered.stdout).expect("valid JSON");
        (value, rendered.exit)
    }

    fn row(id: &str, state: &str) -> JobStatus {
        JobStatus {
            id: SqueueId::parse(id).unwrap(),
            state: JobState::parse(state),
        }
    }

    #[test]
    fn a_remote_command_and_an_rr_stop_with_the_same_number_say_who_decided() {
        let (remote, exit) = json_of(&Outcome::Remote(RemoteExit::new(3)));
        assert_eq!(
            remote,
            json!({"source": "remote", "exit": 3, "kind": "command"})
        );
        assert_eq!(exit, Exit::remote(3));
        let refused = PullOutcome::Refused(Reason::RemoteDirty(vec!["?? x".into()]));
        let (rr, exit) = json_of(&Outcome::Pulled(refused));
        assert_eq!(rr["source"], "rr");
        assert_eq!(rr["exit"], 3);
        assert_eq!(exit, Exit::Rr(RrCode::Refused));
    }

    #[test]
    fn every_pull_ending_is_named_and_a_refusal_carries_its_reason() {
        let advanced = PullOutcome::Advanced {
            from: Commit::parse(A).unwrap(),
            to: Commit::parse(B).unwrap(),
        };
        let (value, _) = json_of(&Outcome::Pulled(advanced));
        assert_eq!(
            value,
            json!({"source": "rr", "exit": 0, "kind": "pull", "result": "advanced", "from": A, "to": B})
        );
        let (value, _) = json_of(&Outcome::Pulled(PullOutcome::UpToDate));
        assert_eq!(value["result"], "up_to_date");
        let stale = PullOutcome::Stale {
            stderr: "x\u{1b}[2J".into(),
        };
        let (value, _) = json_of(&Outcome::Pulled(stale));
        assert_eq!(
            (&value["result"], &value["detail"]),
            (&json!("stale"), &json!("x?[2J"))
        );
        let reasons = [
            (Reason::RemoteDirty(vec![]), "remote_dirty"),
            (Reason::JobsFromCheckout(vec![]), "jobs_from_checkout"),
            (
                Reason::StateChanged {
                    stderr: String::new(),
                },
                "state_changed",
            ),
            (Reason::NotOnOrigin(Target::OriginHead), "not_on_origin"),
        ];
        for (reason, name) in reasons {
            let (value, exit) = json_of(&Outcome::Pulled(PullOutcome::Refused(reason)));
            assert_eq!(value["result"], "refused");
            assert_eq!(value["reason"], name);
            assert_eq!(exit, Exit::Rr(RrCode::Refused));
        }
    }

    #[test]
    fn a_wait_names_its_result_and_every_job_with_its_state() {
        let jobs = vec![row("7", "COMPLETED"), row("8_1", "CANCELLED by 3")];
        let wanted =
            json!([{"id": "7", "state": "COMPLETED"}, {"id": "8_1", "state": "CANCELLED"}]);
        let cases = [
            (WaitOutcome::Completed(jobs.clone()), "completed", 0),
            (WaitOutcome::Failed(jobs.clone()), "failed", 5),
            (WaitOutcome::TimedOut(jobs.clone()), "timed_out", 4),
        ];
        for (outcome, name, code) in cases {
            let (value, exit) = json_of(&Outcome::Waited(outcome));
            assert_eq!(value["result"], name);
            assert_eq!(value["exit"], code);
            assert_eq!(value["source"], "rr");
            assert_eq!(u64::from(exit.code()), code);
            assert_eq!(value["jobs"], wanted, "{name}");
        }
        let unknown = WaitOutcome::Unknown {
            why: "ssh: no route\u{1b}[2J".into(),
        };
        let (value, exit) = json_of(&Outcome::Waited(unknown));
        assert_eq!(value["why"], "ssh: no route?[2J");
        assert_eq!(exit, Exit::Rr(RrCode::Unknown));
        let odd = WaitOutcome::TimedOut(vec![row("1", "BRAND\u{202E}NEW")]);
        let (value, _) = json_of(&Outcome::Waited(odd));
        assert_eq!(value["jobs"][0]["state"], "BRAND?NEW", "bidi controls go");
    }

    #[test]
    fn a_copy_and_a_trust_report_have_their_own_shape() {
        let (ran, _) = json_of(&Outcome::Transferred(Transfer::Ran(23)));
        assert_eq!(
            ran,
            json!({"source": "remote", "exit": 23, "kind": "copy", "result": "ran"})
        );
        let over = Transfer::OverLimit { total: 5, limit: 4 };
        let (over, _) = json_of(&Outcome::Transferred(over));
        assert_eq!(
            over,
            json!({"source": "rr", "exit": 3, "kind": "copy", "result": "over_limit", "total": 5, "limit": 4})
        );
        let report = |state| TrustReport {
            path: PathBuf::from("/p/.rr.toml"),
            digest: digest(b"x"),
            text: "host = \"h\"\n".into(),
            state,
        };
        let (shown, _) = json_of(&Outcome::Trust(report(TrustState::Shown)));
        assert_eq!(shown["state"], "shown");
        let (trust, _) = json_of(&Outcome::Trust(report(TrustState::Recorded)));
        assert_eq!(trust["state"], "recorded");
        assert_eq!(trust["sha256"], digest(b"x").to_string());
        assert_eq!(trust["text"], "host = \"h\"\n");
    }

    #[test]
    fn a_status_carries_the_record_and_the_log_or_why_there_is_none() {
        let record = "9\u{1f}9\u{1f}n\u{1f}u\u{1f}/w\u{1f}/o/%j.out";
        let status = Status {
            record: crate::domain::logpath::parse_sacct(record).unwrap(),
            state: JobState::parse("RUNNING"),
            elapsed: "00:01:02".into(),
            exit_code: "0:0".into(),
        };
        let shown = LogTail::Lines {
            path: "/o/9.out".into(),
            lines: vec!["a\u{1b}[2J".into()],
        };
        let report = StatusReport {
            status: status.clone(),
            log: shown,
        };
        let (value, exit) = json_of(&Outcome::Status(report));
        assert_eq!(exit, Exit::Success);
        assert_eq!(
            (&value["kind"], &value["state"]),
            (&json!("status"), &json!("RUNNING"))
        );
        assert_eq!(
            value["log"],
            json!({"state": "shown", "path": "/o/9.out", "lines": ["a?[2J"]})
        );
        let none = LogTail::Unavailable {
            why: "no file".into(),
        };
        let (value, _) = json_of(&Outcome::Status(StatusReport { status, log: none }));
        assert_eq!(
            value["log"],
            json!({"state": "unavailable", "why": "no file"})
        );
    }
}
