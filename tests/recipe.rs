//! `rr do` and `rr x -s` with the real binary and a stub `ssh` on `PATH` that records the script it is given.
//! Nothing here touches the network, `~/.ssh` or the real `HOME`.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // starts processes and reads files by design

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

const CONFIG: &str = "\
[recipe.train]
run = [\"python\", \"--cfg={cfg}\"]
pull = \"none\"
";

/// The last argument ssh gets is the remote command; the stub keeps it and exits 3.
const STUB: &str = "for last; do :; done; printf '%s' \"$last\" > \"$STUB_DIR/sent\"; cat > \"$STUB_DIR/stdin\"; exit 3";

/// Run `rr --host cluster ARGS` with `CONFIG` as the user file; the dir holds what the stub saw.
fn rr(args: &[&str]) -> (std::process::Output, tempfile::TempDir) {
    rr_with(args, |_| {})
}

/// Like [`rr`], after `prepare` has put files in the empty working directory.
fn rr_with(
    args: &[&str],
    prepare: impl FnOnce(&std::path::Path),
) -> (std::process::Output, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh, format!("#!/bin/sh\n{STUB}\n")).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join(".config/rr");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), CONFIG).unwrap();
    prepare(dir.path());
    let out = Command::new(env!("CARGO_BIN_EXE_rr"))
        .args(["--host", "cluster"])
        .args(args)
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", dir.path())
        .env("STUB_DIR", dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (out, dir)
}

#[test]
fn a_recipe_sends_its_words_quoted_and_returns_the_remote_exit_code() {
    let hostile = "$(touch pwned)";
    let (out, dir) = rr(&["do", "train", "--set", "cfg=a b", "--", hostile]);
    assert_eq!(out.status.code(), Some(3), "{:?}", out.stderr);
    let sent = fs::read_to_string(dir.path().join("sent")).unwrap();
    // ssh gets `bash -c '<script>'`, so the script's own quotes show as `'\''`
    let words = r"'\''python'\'' '\''--cfg=a b'\'' '\''$(touch pwned)'\''";
    assert!(sent.contains(words), "{sent}");
    assert!(!dir.path().join("pwned").exists());
}

#[test]
fn a_recipe_that_cannot_be_filled_never_reaches_ssh() {
    for args in [
        &["do", "train"][..],
        &["do", "nope"],
        &["do", "train", "--set", "cfg=1", "--set", "other=2"],
    ] {
        let (out, dir) = rr(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(!dir.path().join("sent").exists(), "{args:?}");
    }
}

#[test]
fn a_script_reaches_the_interpreter_on_stdin_and_never_runs_locally() {
    let body = "print(1)\n$(touch pwned)\n";
    let (out, dir) = rr_with(&["x", "-s", "probe.py", "--interp", "python -u -"], |dir| {
        fs::write(dir.join("probe.py"), body).unwrap();
    });
    assert_eq!(out.status.code(), Some(3), "{:?}", out.stderr);
    assert_eq!(fs::read_to_string(dir.path().join("stdin")).unwrap(), body);
    let sent = fs::read_to_string(dir.path().join("sent")).unwrap();
    assert!(
        sent.contains(r"'\''python'\'' '\''-u'\'' '\''-'\''"),
        "{sent}"
    );
    assert!(!dir.path().join("pwned").exists());
}

#[test]
fn a_script_that_cannot_be_sent_never_reaches_ssh() {
    for args in [
        &["x", "-s", "missing.py", "--interp", "python -"][..],
        &["x", "-s", "probe.py"],
        &["x", "-s", "probe.py", "--interp", "python -", "ls"],
        &["x", "-s", "probe.py", "--interp", "'python -"],
    ] {
        let (out, dir) = rr_with(args, |dir| fs::write(dir.join("probe.py"), "x").unwrap());
        assert!(!out.status.success(), "{args:?}");
        assert!(!dir.path().join("sent").exists(), "{args:?}");
    }
}
