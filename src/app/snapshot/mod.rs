//! Snapshot mode of `rr run`: build the working tree locally, check it, ship it and run in it.

pub mod build;
pub mod ship;

use super::exec;
use super::run::After;
use super::wait::after_command;
use super::{Output, Ports, stream};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::place::{Place, script};
use crate::domain::pull::{PullOutcome, STALE_EXIT, UV_MISSING_EXIT};
use crate::domain::sanitize::sanitize;
use crate::domain::shell::{Script, quote};
use crate::domain::slurm::SUBMITTED_PREFIX;
use crate::domain::snapshot::{Policy, ShippedSnapshot, run_prelude, stale_script};
use anyhow::{Result, bail};
use ship::{Delivery, Shipment, ship};

/// How many left-out files the note names before it says how many more there are.
const NAMED: usize = 5;

/// The line that says which files a snapshot leaves out, so a "no such file" has a visible cause.
/// File names are text from the user's disk, so they are sanitized.
fn skipped_note(skipped: &[String]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let named: Vec<String> = skipped
        .iter()
        .take(NAMED)
        .map(|name| sanitize(name))
        .collect();
    let more = skipped.len().saturating_sub(NAMED);
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Some(format!(
        "rr: not shipped, not in HEAD (--include-untracked ships them): {}{tail}",
        named.join(", ")
    ))
}

fn delivery_note(snapshot: &ShippedSnapshot, delivery: Delivery) -> String {
    let how = match delivery {
        Delivery::Uploaded => "uploaded",
        Delivery::AlreadyThere => "already on the remote",
    };
    format!("rr: snapshot {} ({how})", snapshot.id())
}

/// Whether the shared environment holds what the snapshot's `uv.lock` needs. A mismatch is the
/// same stop as in a pull (`Pulled(Stale)`), and a check that could not run is an error, never a
/// pass.
fn stale(session: &Session, snapshot: &ShippedSnapshot, ports: &Ports) -> Result<Option<Outcome>> {
    let checked = ports
        .remote
        .capture(&script(&session.place, stale_script(snapshot)))?;
    let stderr = checked.stderr.trim().to_string();
    match checked.code {
        0 => Ok(None),
        STALE_EXIT => Ok(Some(Outcome::Pulled(PullOutcome::Stale { stderr }))),
        UV_MISSING_EXIT => bail!(
            "cannot check the remote environment: uv is not on the remote PATH (set `env` to a \
             file that puts it there):\n{stderr}"
        ),
        code => bail!("checking the remote environment failed ({code}):\n{stderr}"),
    }
}

/// The command, run inside the snapshot after the prelude that sets its environment.
fn body(snapshot: &ShippedSnapshot, cmd: &[String]) -> Script {
    Script::lines([
        run_prelude(snapshot),
        Script::new().words(cmd.iter().map(|word| quote(word))),
    ])
}

fn run_in(
    session: &Session,
    snapshot: &ShippedSnapshot,
    command: exec::Args,
    after: After,
    ports: &Ports,
) -> Result<Outcome> {
    let body = body(snapshot, &command.cmd);
    match after {
        After::Return => stream(ports, &session.place, body, command.terminal, Output::Raw),
        After::Wait { limits, echo } => {
            let placed = script(&session.place, body);
            let teed =
                ports
                    .remote
                    .stream_tee(&placed, command.terminal, SUBMITTED_PREFIX, echo)?;
            after_command(session, teed, limits, ports)
        }
    }
}

/// The directory the snapshots live in is inside the project directory, so there must be one.
fn project_place(session: &Session) -> Result<Place> {
    match &session.place.dir {
        Some(dir) => Ok(Place {
            env: None,
            dir: Some(dir.clone()),
        }),
        None => bail!("--mode snapshot needs a project directory on the remote (set `dir`)"),
    }
}

/// Build, check, ship, then run `command` in the snapshot. Every guard that says no is an
/// [`Outcome::SnapshotRefused`] and nothing runs; the run happens only for a
/// [`ShippedSnapshot`], which only a successful upload (or a snapshot already there) makes.
pub fn run(
    session: &Session,
    policy: &Policy,
    command: exec::Args,
    after: After,
    ports: &Ports,
) -> Result<Outcome> {
    let place = project_place(session)?;
    let built = build::build(ports.exec, ports.scratch, policy)?;
    if let Some(note) = skipped_note(&built.skipped) {
        ports.notes.note(&note);
    }
    let cleared = match built.tree.clear(policy) {
        Ok(cleared) => cleared,
        Err(refusal) => return Ok(Outcome::SnapshotRefused(refusal)),
    };
    let (snapshot, delivery) = match ship(&place, cleared, &built.repo, ports)? {
        Shipment::Shipped { snapshot, delivery } => (snapshot, delivery),
        Shipment::Refused(refusal) => return Ok(Outcome::SnapshotRefused(refusal)),
    };
    ports.notes.note(&delivery_note(&snapshot, delivery));
    if let Some(stopped) = stale(session, &snapshot, ports)? {
        return Ok(stopped);
    }
    run_in(session, &snapshot, command, after, ports)
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, in_project, session};
    use super::*;
    use crate::app::wait::Limits;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::outcome::{Format, render};
    use crate::domain::place::RemoteDir;
    use crate::domain::snapshot::{DirtyPolicy, Inclusion, Refusal, SubmodulePolicy, tar_of};
    use crate::domain::wait::PollInterval;
    use crate::ports::fake::{RemoteCall, Reply, output};
    use crate::ports::{Echo, Terminal};
    use std::time::Duration;

    const BASE: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const HEAD_TREE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const BLOB: &str = "dddddddddddddddddddddddddddddddddddddddd";

    fn policy() -> Policy {
        Policy {
            inclusion: Inclusion::TrackedOnly,
            allowed: vec![],
            submodules: SubmodulePolicy::Refuse,
            limit: 1 << 20,
            dirty: DirtyPolicy::Allow,
        }
    }

    fn command() -> exec::Args {
        exec::Args {
            terminal: Terminal::Plain,
            cmd: vec!["python".into(), "train it.py".into()],
        }
    }

    /// What git says about a tree of two files that differs from HEAD, with `added` new to HEAD
    /// and `status` listing what the working tree has beyond it.
    fn git(added: &str, status: &str) -> Vec<Reply> {
        let listing =
            format!("100644 blob {BLOB}    5000\ta.py\0100644 blob {BLOB}    5000\tb.py\0");
        vec![
            output(0, "/work/proj\n", ""),
            output(0, &format!("{BASE}\n"), ""),
            output(0, &format!("{HEAD_TREE}\n"), ""),
            output(0, "", ""),
            output(0, "", ""),
            output(0, &format!("{TREE}\n"), ""),
            output(0, &listing, ""),
            output(0, added, ""),
            output(0, status, ""),
        ]
    }

    fn archive() -> Vec<u8> {
        tar_of(&[(b'0', "a.py", b"x"), (b'0', "b.py", b"y")])
    }

    fn fakes(captures: Vec<(i32, &str, &str)>, streams: Vec<i32>) -> Fakes {
        let mut fakes = Fakes::new(git("", "?? new.py\0"), vec![], captures, streams);
        fakes.exec = fakes.exec.with_bytes(vec![(0, archive(), "")]);
        fakes
    }

    fn after_return() -> After {
        After::Return
    }

    fn how(fakes: &Fakes) -> Vec<RemoteCall> {
        fakes.remote.calls.borrow().iter().map(|c| c.1).collect()
    }

    const ABSENT: &str = "rr-snapshot absent\n";
    const PUBLISHED: &str = "rr-snapshot published\n";
    const PRESENT: &str = "rr-snapshot present\n";

    #[test]
    fn a_first_run_builds_uploads_checks_the_environment_and_runs_in_the_snapshot() {
        let captures = vec![(0, ABSENT, ""), (0, PUBLISHED, ""), (0, "", "")];
        let fakes = fakes(captures, vec![7]);
        let outcome = run(
            &session(),
            &policy(),
            command(),
            after_return(),
            &fakes.ports(),
        )
        .unwrap();
        assert_eq!(render(&outcome, Format::Text).exit, Exit::remote(7));
        assert_eq!(
            *fakes.notes.0.borrow(),
            [
                "rr: not shipped, not in HEAD (--include-untracked ships them): new.py",
                "rr: snapshot bbbbbbbbbbbb (uploaded)"
            ]
        );
        assert_eq!(
            how(&fakes),
            [
                RemoteCall::Capture,
                RemoteCall::Upload,
                RemoteCall::Capture,
                RemoteCall::Stream(Terminal::Plain)
            ]
        );
        let scripts = fakes.remote.scripts();
        assert!(scripts[2].starts_with("set -euo pipefail\nsource 'env.sh'\ncd 'proj'\n"));
        assert!(scripts[2].contains("uv sync --frozen --check --inexact --no-install-project"));
        let last = scripts.last().unwrap();
        assert!(last.starts_with(&in_project("proj=$(pwd -P)\ncd -- .rr-snapshots/'bbbb")));
        assert!(
            last.ends_with("RR_DIRTY='true'\n'python' 'train it.py'"),
            "the command is quoted and runs after the prelude: {last}"
        );
        assert!(last.contains("export PYTHONPATH=\"$snap/src${PYTHONPATH:+:$PYTHONPATH}\""));
        assert_eq!(fakes.scratch.dropped.get(), 1);
    }

    #[test]
    fn the_second_run_of_the_same_tree_transfers_nothing() {
        let fakes = fakes(vec![(0, PRESENT, ""), (0, "", "")], vec![0]);
        let outcome = run(
            &session(),
            &policy(),
            command(),
            after_return(),
            &fakes.ports(),
        )
        .unwrap();
        assert_eq!(render(&outcome, Format::Text).exit, Exit::Success);
        assert_eq!(
            how(&fakes),
            [
                RemoteCall::Capture,
                RemoteCall::Capture,
                RemoteCall::Stream(Terminal::Plain)
            ]
        );
        assert!(fakes.remote.sent.borrow().is_empty());
        assert_eq!(
            fakes.notes.0.borrow().last().unwrap(),
            "rr: snapshot bbbbbbbbbbbb (already on the remote)"
        );
        assert!(
            !fakes
                .exec
                .words()
                .iter()
                .any(|call| call.contains(&"archive".to_string())),
            "no archive is built for a tree that is there"
        );
    }

    /// A guard that says no ends the run before the remote host is asked anything.
    #[test]
    fn a_refused_tree_touches_nothing_on_the_remote_and_exits_refused() {
        let strict = Policy {
            dirty: DirtyPolicy::Refuse,
            ..policy()
        };
        let secret = Fakes::new(git(".env\0", ""), vec![], vec![], vec![]);
        for (name, policy, fakes) in [
            ("dirty", strict, fakes(vec![], vec![])),
            ("secret", policy(), secret),
        ] {
            let outcome = run(
                &session(),
                &policy,
                command(),
                after_return(),
                &fakes.ports(),
            )
            .unwrap();
            assert!(matches!(outcome, Outcome::SnapshotRefused(_)), "{name}");
            let rendered = render(&outcome, Format::Text);
            assert_eq!(rendered.exit, Exit::Rr(RrCode::Refused), "{name}");
            assert!(rendered.stderr.starts_with("rr: refused: "), "{name}");
            assert!(fakes.remote.calls.borrow().is_empty(), "{name}");
        }
    }

    #[test]
    fn a_remote_directory_that_is_not_private_refuses_and_runs_nothing() {
        let fakes = fakes(
            vec![(77, "", "rr: x is not a private directory of yours\n")],
            vec![],
        );
        let outcome = run(
            &session(),
            &policy(),
            command(),
            after_return(),
            &fakes.ports(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            Outcome::SnapshotRefused(Refusal::RemoteNotPrivate { .. })
        ));
        assert_eq!(how(&fakes), [RemoteCall::Capture]);
    }

    fn go(fakes: &Fakes) -> Result<Outcome> {
        run(
            &session(),
            &policy(),
            command(),
            after_return(),
            &fakes.ports(),
        )
    }

    fn never_ran(fakes: &Fakes) -> bool {
        !how(fakes)
            .iter()
            .any(|call| matches!(call, RemoteCall::Stream(_)))
    }

    #[test]
    fn a_stale_environment_stops_the_run_like_a_stale_pull() {
        let stale = (crate::domain::pull::STALE_EXIT, "", "Would install idna\n");
        let fakes = fakes(vec![(0, PRESENT, ""), stale], vec![]);
        let outcome = go(&fakes).unwrap();
        assert!(matches!(
            outcome,
            Outcome::Pulled(PullOutcome::Stale { ref stderr }) if stderr == "Would install idna"
        ));
        let exit = render(&outcome, Format::Text).exit;
        assert_eq!(exit, Exit::Rr(RrCode::Refused));
        assert!(never_ran(&fakes));
    }

    #[test]
    fn a_check_that_could_not_run_is_an_error_never_a_pass() {
        let missing = (127, "", "uv is not on the remote PATH\n");
        let fakes_missing = fakes(vec![(0, PRESENT, ""), missing], vec![]);
        let err = go(&fakes_missing).unwrap_err();
        assert!(
            err.to_string().contains("uv is not on the remote PATH"),
            "{err}"
        );
        let fakes_broken = fakes(vec![(0, PRESENT, ""), (1, "", "boom\n")], vec![]);
        assert!(go(&fakes_broken).is_err());
        assert!(never_ran(&fakes_missing) && never_ran(&fakes_broken));
    }

    #[test]
    fn wait_shows_the_command_output_and_polls_the_jobs_it_submitted() {
        let done = "7\u{1f}COMPLETED\n";
        let captures = vec![(0, PRESENT, ""), (0, "", ""), (0, done, "")];
        let fakes = fakes(captures, vec![0]);
        fakes.remote.emit(b"Submitted batch job 7\n");
        let after = After::Wait {
            limits: Limits {
                timeout: Duration::from_secs(3600),
                poll: PollInterval::default(),
            },
            echo: Echo::Stdout,
        };
        let outcome = run(&session(), &policy(), command(), after, &fakes.ports()).unwrap();
        assert!(matches!(outcome, Outcome::Waited(_)));
        assert_eq!(render(&outcome, Format::Text).exit, Exit::Success);
        assert_eq!(
            how(&fakes),
            [
                RemoteCall::Capture,
                RemoteCall::Capture,
                RemoteCall::StreamTee(Terminal::Plain, Echo::Stdout),
                RemoteCall::Poll
            ]
        );
        assert!(fakes.remote.scripts()[2].contains("RR_SNAPSHOT_ID='bbbb"));
    }

    #[test]
    fn a_snapshot_needs_a_project_directory_and_asks_nothing_without_one() {
        let mut home = session();
        home.place.dir = None;
        let fakes = fakes(vec![], vec![]);
        let err = run(&home, &policy(), command(), after_return(), &fakes.ports()).unwrap_err();
        assert!(err.to_string().contains("project directory"), "{err}");
        assert!(fakes.exec.calls.borrow().is_empty());
        let with_dir = session();
        assert_eq!(with_dir.place.dir, Some(RemoteDir::parse("proj").unwrap()));
    }

    #[test]
    fn what_is_left_out_is_named_with_a_count_and_sanitized() {
        assert_eq!(skipped_note(&[]), None);
        let many: Vec<String> = (0..8).map(|n| format!("f{n}")).collect();
        assert_eq!(
            skipped_note(&many).unwrap(),
            "rr: not shipped, not in HEAD (--include-untracked ships them): f0, f1, f2, f3, f4 and 3 more"
        );
        let hostile = skipped_note(&["a\u{1b}[2Jb".to_string()]).unwrap();
        assert!(!hostile.contains('\u{1b}'));
    }
}
