//! `--json` with the real binary and a stub `ssh` on `PATH`: stdout must be exactly one JSON
//! document whatever happens, and the exit code must stay the one the text mode gives. Nothing here
//! touches the network, `~/.ssh` or the real `HOME`.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // starts processes and reads files by design

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

/// What a run of `rr` left behind.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// The one line `--json` promises on stdout, parsed.
    fn document(&self) -> Value {
        assert_eq!(self.stdout.lines().count(), 1, "stdout: {:?}", self.stdout);
        serde_json::from_str(&self.stdout).expect("stdout is JSON")
    }
}

const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The remote state of a checkout that is already at the target: `rr run` pulls nothing and goes on
/// to run its command.
fn up_to_date() -> String {
    format!(
        "printf 'rr-state\\tdir\\t/d\\nrr-state\\thead\\t{COMMIT}\\nrr-state\\ttarget\\t{COMMIT}\\n\
         rr-state\\tonorigin\\t  origin/main\\n'"
    )
}

/// Run `rr --host cluster ARGS` in an empty directory with `ssh_body` as the whole of `ssh`.
fn rr(ssh_body: &str, args: &[&str]) -> (Ran, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh, format!("#!/bin/sh\n{ssh_body}\n")).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out = Command::new(env!("CARGO_BIN_EXE_rr"))
        .args(["--host", "cluster"])
        .args(args)
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir.path())
        .env("STUB_DIR", dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let ran = Ran {
        code: out.status.code(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    };
    (ran, dir)
}

#[test]
fn a_failed_job_is_one_document_and_exit_5() {
    let (ran, _dir) = rr("printf '7\\037FAILED\\n'", &["--json", "wait", "7"]);
    assert_eq!(ran.code, Some(5), "{}", ran.stderr);
    assert_eq!(
        ran.document(),
        json!({
            "source": "rr", "exit": 5, "kind": "wait", "result": "failed",
            "jobs": [{"id": "7", "state": "FAILED"}],
        })
    );
    assert_eq!(ran.stderr, "");
}

#[test]
fn an_error_under_json_is_one_error_document_and_exit_1() {
    let ssh = "echo 'ssh: no route' >&2; exit 255";
    let (ran, _dir) = rr(ssh, &["--json", "status", "9"]);
    assert_eq!(ran.code, Some(1));
    let document = ran.document();
    assert_eq!(
        (&document["source"], &document["exit"], &document["kind"]),
        (&json!("rr"), &json!(1), &json!("error"))
    );
    let message = document["message"].as_str().unwrap();
    assert!(message.contains("ssh: no route"), "{message}");
    assert_eq!(ran.stderr, "", "the error is in the document");
}

/// `x`, `q`, `cancel`, `get`, `put` and the like write to stdout themselves: `--json` is refused
/// before anything starts.
#[test]
fn a_streaming_command_refuses_json_before_it_touches_ssh() {
    for args in [
        &["--json", "q"][..],
        &["--json", "acct"],
        &["--json", "log", "7"],
        &["--json", "x", "ls"],
        &["--json", "run", "--", "ls"],
        &["--json", "fwd", "gpu-01:5000"],
        &["--json", "cancel", "7"],
        &["--json", "get", "out.txt"],
        &["--json", "put", "in.txt"],
    ] {
        let (ran, dir) = rr("touch \"$STUB_DIR/ssh-was-run\"", args);
        assert_eq!(ran.code, Some(1), "{args:?}");
        assert_eq!(ran.document()["kind"], "error", "{args:?}");
        assert!(!dir.path().join("ssh-was-run").exists(), "{args:?}");
    }
}

/// The command's own output goes to stderr, so stdout is the one document.
#[test]
fn run_wait_under_json_shows_the_command_on_stderr_and_one_document_on_stdout() {
    let ssh = format!(
        r#"case "$*" in
  *"git fetch"*) {state} ;;
  *"'sbatch'"*) echo 'Submitted batch job 7' ;;
  *sacct*) printf '7\037COMPLETED\n' ;;
esac"#,
        state = up_to_date()
    );
    let args = ["--json", "run", "--wait", "--", "sbatch", "job.sh"];
    let (ran, _dir) = rr(&ssh, &args);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(
        ran.document(),
        json!({
            "source": "rr", "exit": 0, "kind": "wait", "result": "completed",
            "jobs": [{"id": "7", "state": "COMPLETED"}],
        })
    );
    assert!(
        ran.stderr.contains("Submitted batch job 7"),
        "{}",
        ran.stderr
    );
}

/// clap's usage error keeps its exit code 2; under `--json` it is the one document, on stdout.
#[test]
fn a_usage_error_under_json_is_one_document_with_exit_2() {
    let (ran, _dir) = rr("true", &["--json", "--bogus"]);
    assert_eq!(ran.code, Some(2));
    let document = ran.document();
    assert_eq!(
        (&document["source"], &document["exit"], &document["kind"]),
        (&json!("rr"), &json!(2), &json!("error"))
    );
    assert!(document["message"].as_str().unwrap().contains("--bogus"));
    assert_eq!(ran.stderr, "");
    // without --json clap prints its own text, and --help is still a success
    let (ran, _dir) = rr("true", &["--bogus"]);
    assert_eq!((ran.code, ran.stdout.as_str()), (Some(2), ""));
    assert!(ran.stderr.contains("--bogus"));
    let (ran, _dir) = rr("true", &["--json", "--help"]);
    assert_eq!(ran.code, Some(0));
    assert!(ran.stdout.contains("Usage"));
}
