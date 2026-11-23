//! The two-pass copy `get` and `put` share: measure, then copy unless it is over the limit.

use super::Ports;
use crate::domain::outcome::Outcome;
use crate::domain::transfer::{Endpoint, Pass, Transfer, parse_total, rsync_argv};
use crate::ports::{Exec, Stdin};
use anyhow::Result;

pub(super) fn copy(
    ports: &Ports,
    source: &Endpoint,
    destination: &Endpoint,
    limit: u64,
) -> Result<Outcome> {
    let done = transfer(ports.exec, source, destination, limit)?;
    Ok(Outcome::Transferred(done))
}

/// Measure with a dry run, then copy unless it is over `limit` (bytes). A failed probe copies
/// nothing and is an error.
fn transfer(
    exec: &dyn Exec,
    source: &Endpoint,
    destination: &Endpoint,
    limit: u64,
) -> Result<Transfer> {
    let probe = exec.capture(&rsync_argv(source, destination, Pass::Measure), Stdin::Null)?;
    let total = parse_total(&probe.into_stdout("rsync --dry-run")?)?;
    if total > limit {
        return Ok(Transfer::OverLimit { total, limit });
    }
    let code = exec.stream(&rsync_argv(source, destination, Pass::Copy))?;
    Ok(Transfer::Ran(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::host::Host;
    use crate::domain::place::RemotePath;
    use crate::ports::fake::{FakeExec, How, output};

    fn local(path: &str) -> Endpoint {
        Endpoint::local(path)
    }

    fn remote(path: &str) -> Endpoint {
        Endpoint::Remote(Host::parse("h").unwrap(), RemotePath::in_home(path))
    }

    const STATS: &str = "Total transferred file size: 100 bytes\n";

    #[test]
    fn a_failed_probe_copies_nothing() {
        let exec = FakeExec::new(vec![output(23, "", "ssh: no route\n")], vec![]);
        let err = transfer(&exec, &local("a"), &remote("b"), 1000)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "rsync --dry-run failed (23): ssh: no route");
        assert_eq!(exec.calls.borrow().len(), 1);
        let unreadable = FakeExec::new(vec![output(0, "no stats", "")], vec![]);
        assert!(transfer(&unreadable, &local("a"), &remote("b"), 1000).is_err());
        assert_eq!(unreadable.calls.borrow().len(), 1);
    }

    #[test]
    fn over_the_limit_copies_nothing_and_is_a_refusal() {
        let exec = FakeExec::new(vec![output(0, STATS, "")], vec![]);
        let done = transfer(&exec, &local("a"), &remote("b"), 99).unwrap();
        assert_eq!(
            done,
            Transfer::OverLimit {
                total: 100,
                limit: 99
            }
        );
        assert_eq!(done.exit(), Exit::Rr(RrCode::Refused));
        assert_eq!(exec.calls.borrow().len(), 1, "only the probe ran");
        assert!(
            done.to_string()
                .contains("100 bytes to copy, over the limit of 99")
        );
    }

    #[test]
    fn within_the_limit_streams_the_copy_and_never_deletes() {
        let exec = FakeExec::new(vec![output(0, STATS, "")], vec![5]);
        let done = transfer(&exec, &local("local"), &remote("remote/"), 100).unwrap();
        assert_eq!(done, Transfer::Ran(5));
        assert_eq!(done.exit().code(), 5);
        let calls = exec.calls.borrow();
        assert_eq!(calls[0].1, How::Capture(Stdin::Null));
        assert_eq!(
            calls[0].0.args()[4..],
            ["--dry-run", "--stats", "--", "local", "h:./remote/"]
        );
        assert_eq!(calls[1].1, How::Stream);
        assert_eq!(calls[1].0.args()[4..], ["--", "local", "h:./remote/"]);
        assert!(
            calls
                .iter()
                .all(|(argv, _)| argv.args().iter().all(|a| !a.contains("delete")))
        );
    }
}
