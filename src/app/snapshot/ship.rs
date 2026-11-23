//! Put a checked tree on the remote host, and send nothing when it is already there.

use crate::app::Ports;
use crate::domain::argv::{Captured, Payload};
use crate::domain::place::{Place, script};
use crate::domain::sanitize::sanitize;
use crate::domain::snapshot::{
    ClearedTree, Presence, Refusal, Repo, ShippedSnapshot, UNSAFE_EXIT, count_files,
    parse_presence, probe_script, upload_script,
};
use anyhow::{Result, bail};

/// Whether the archive went over the link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Uploaded,
    /// The remote host had the tree already, so nothing was sent.
    AlreadyThere,
}

/// How shipping ended: the snapshot is on the remote host, or a guard said no and the remote was
/// left as it was.
#[derive(Debug)]
pub enum Shipment {
    Shipped {
        snapshot: ShippedSnapshot,
        delivery: Delivery,
    },
    Refused(Refusal),
}

/// What a probe or an upload script said.
enum Answer {
    State(Presence),
    /// It exited [`UNSAFE_EXIT`]: a directory is not a private one of ours (the remote's stderr).
    NotPrivate(String),
}

impl Answer {
    /// Any other failure is an error that names the remote's sanitized stderr. A script that did
    /// not get to its answer is never read as "absent".
    fn of(captured: Captured, what: &str) -> Result<Self> {
        if captured.code == UNSAFE_EXIT {
            return Ok(Self::NotPrivate(captured.stderr));
        }
        let stdout = captured.into_stdout(what)?;
        Ok(Self::State(parse_presence(&stdout)?))
    }
}

fn not_private(stderr: String) -> Shipment {
    Shipment::Refused(Refusal::RemoteNotPrivate { stderr })
}

/// Ask the remote whether the snapshot is there; upload it when it is not. The archive is built
/// (and compared with the tree) only after the remote said "absent", so a tree that is there
/// transfers nothing, and a tree whose archive does not match sends nothing either.
pub fn ship(place: &Place, tree: ClearedTree, repo: &Repo, ports: &Ports) -> Result<Shipment> {
    let probed = ports.remote.capture(&script(place, probe_script(&tree)))?;
    let presence = match Answer::of(probed, "the snapshot probe")? {
        Answer::State(presence) => presence,
        Answer::NotPrivate(stderr) => return Ok(not_private(stderr)),
    };
    match presence {
        Presence::Present => Ok(Shipment::Shipped {
            snapshot: tree.shipped(),
            delivery: Delivery::AlreadyThere,
        }),
        Presence::Absent => upload(place, tree, repo, ports),
        Presence::Published => bail!("the snapshot probe answered \"published\", which it cannot"),
    }
}

fn upload(place: &Place, tree: ClearedTree, repo: &Repo, ports: &Ports) -> Result<Shipment> {
    let archive = ports.exec.capture_bytes(&repo.archive(&tree))?;
    if archive.code != 0 {
        bail!(
            "git archive failed ({}): {}",
            archive.code,
            sanitize(archive.stderr.trim())
        );
    }
    let found = count_files(&archive.stdout)?;
    if found != tree.files() {
        let expected = tree.files();
        return Ok(Shipment::Refused(Refusal::ArchiveMismatch {
            expected,
            found,
        }));
    }
    let body = script(place, upload_script(&tree)?);
    let sent = ports.remote.upload(&body, &Payload::new(archive.stdout))?;
    let delivery = match Answer::of(sent, "the snapshot upload")? {
        Answer::NotPrivate(stderr) => return Ok(not_private(stderr)),
        Answer::State(Presence::Published) => Delivery::Uploaded,
        // another upload won between the probe and this one; it passed the same checks
        Answer::State(Presence::Present) => Delivery::AlreadyThere,
        Answer::State(Presence::Absent) => bail!("the snapshot upload ended without publishing"),
    };
    Ok(Shipment::Shipped {
        snapshot: tree.shipped(),
        delivery,
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::testing::Fakes;
    use super::*;
    use crate::domain::place::RemoteDir;
    use crate::domain::snapshot::tar_of;
    use crate::ports::fake::{How, RemoteCall};

    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn place() -> Place {
        Place {
            env: None,
            dir: Some(RemoteDir::parse("proj").unwrap()),
        }
    }

    fn repo() -> Repo {
        Repo::parse("/work/proj\n").unwrap()
    }

    fn tar() -> Vec<u8> {
        tar_of(&[(b'0', "a.py", b"print(1)\n"), (b'0', "b.py", b"")])
    }

    /// A tree of two files, a remote that answers `captures`, and `git archive` giving `archive`.
    fn fakes(captures: Vec<(i32, &str, &str)>, archive: Vec<(i32, Vec<u8>, &str)>) -> Fakes {
        let mut fakes = Fakes::new(vec![], vec![], captures, vec![]);
        fakes.exec = fakes.exec.with_bytes(archive);
        fakes
    }

    fn ship_it(fakes: &Fakes, files: usize) -> Result<Shipment> {
        ship(
            &place(),
            ClearedTree::fake(TREE, files),
            &repo(),
            &fakes.ports(),
        )
    }

    fn hows(fakes: &Fakes) -> Vec<RemoteCall> {
        fakes.remote.calls.borrow().iter().map(|c| c.1).collect()
    }

    #[test]
    fn a_tree_that_is_absent_is_archived_checked_and_uploaded() {
        let fakes = fakes(
            vec![
                (0, "rr-snapshot absent\n", ""),
                (0, "rr-snapshot published\n", ""),
            ],
            vec![(0, tar(), "")],
        );
        let shipment = ship_it(&fakes, 2).unwrap();
        let Shipment::Shipped { snapshot, delivery } = shipment else {
            panic!("{shipment:?}");
        };
        assert_eq!(snapshot.id().as_str(), TREE);
        assert_eq!(delivery, Delivery::Uploaded);
        assert_eq!(hows(&fakes), [RemoteCall::Capture, RemoteCall::Upload]);
        assert_eq!(
            *fakes.remote.sent.borrow(),
            [tar()],
            "the archive, byte for byte"
        );
        let scripts = fakes.remote.scripts();
        assert!(scripts[0].starts_with("set -euo pipefail\ncd 'proj'\numask 077\n"));
        assert!(scripts[0].contains("echo rr-snapshot absent"));
        assert!(scripts[1].contains("tar -x --no-same-owner"));
        let git = fakes.exec.words();
        assert_eq!(git[0][3..], ["archive", "--format=tar", TREE]);
        assert_eq!(fakes.exec.calls.borrow()[0].1, How::CaptureBytes);
    }

    #[test]
    fn a_tree_that_is_already_there_transfers_nothing() {
        let fakes = fakes(vec![(0, "rr-snapshot present\n", "")], vec![]);
        let Shipment::Shipped { delivery, .. } = ship_it(&fakes, 2).unwrap() else {
            panic!("refused");
        };
        assert_eq!(delivery, Delivery::AlreadyThere);
        assert_eq!(hows(&fakes), [RemoteCall::Capture], "the probe only");
        assert!(fakes.exec.calls.borrow().is_empty(), "no archive was built");
    }

    #[test]
    fn a_remote_directory_that_is_not_ours_is_a_refusal_at_the_probe_or_the_upload() {
        let probe = fakes(
            vec![(
                77,
                "",
                "rr: /h/proj/x is not a private directory of yours\n",
            )],
            vec![],
        );
        let refused = ship_it(&probe, 2).unwrap();
        assert!(
            matches!(&refused, Shipment::Refused(Refusal::RemoteNotPrivate { stderr }) if stderr.contains("not a private")),
            "{refused:?}"
        );
        assert!(probe.exec.calls.borrow().is_empty());
        let raced = fakes(
            vec![(0, "rr-snapshot absent\n", ""), (77, "", "rr: swapped\n")],
            vec![(0, tar(), "")],
        );
        let refused = ship_it(&raced, 2).unwrap();
        assert!(matches!(
            refused,
            Shipment::Refused(Refusal::RemoteNotPrivate { .. })
        ));
    }

    /// `export-ignore` leaves a file out of `git archive`: nothing is sent.
    #[test]
    fn an_archive_that_does_not_match_the_tree_is_refused_before_anything_is_sent() {
        let fakes = fakes(vec![(0, "rr-snapshot absent\n", "")], vec![(0, tar(), "")]);
        let refused = ship_it(&fakes, 3).unwrap();
        assert!(matches!(
            refused,
            Shipment::Refused(Refusal::ArchiveMismatch {
                expected: 3,
                found: 2
            })
        ));
        assert_eq!(hows(&fakes), [RemoteCall::Capture], "no upload");
        assert!(fakes.remote.sent.borrow().is_empty());
    }

    #[test]
    fn failures_are_errors_that_show_sanitized_text_and_never_read_as_shipped() {
        let hostile = "\u{1b}]52;c;ZXZpbA==\u{7}disk full";
        for (name, captures, archive) in [
            ("probe", vec![(1, "", hostile)], vec![]),
            ("probe garbage", vec![(0, "hello\n", "")], vec![]),
            ("probe empty", vec![(0, "", "")], vec![]),
            (
                "upload",
                vec![(0, "rr-snapshot absent\n", ""), (1, "", hostile)],
                vec![(0, tar(), "")],
            ),
            (
                "upload without an answer",
                vec![(0, "rr-snapshot absent\n", ""), (0, "", "")],
                vec![(0, tar(), "")],
            ),
            (
                "git archive",
                vec![(0, "rr-snapshot absent\n", "")],
                vec![(128, vec![], hostile)],
            ),
            (
                "truncated archive",
                vec![(0, "rr-snapshot absent\n", "")],
                vec![(0, tar()[..600].to_vec(), "")],
            ),
        ] {
            let fakes = fakes(captures, archive);
            let err = ship_it(&fakes, 2).unwrap_err().to_string();
            assert!(
                !err.contains('\u{1b}') && !err.contains('\u{7}'),
                "{name}: {err:?}"
            );
        }
        let fakes = fakes(vec![(1, "", hostile)], vec![]);
        let err = ship_it(&fakes, 2).unwrap_err().to_string();
        assert!(err.contains("disk full"), "{err}");
    }
}
