//! `rr fwd`: forward a port of the cluster to this machine over ssh, loopback only, for a bounded
//! time. The URL is printed only once ssh has stayed alive with the port accepting; `--ttl` ends
//! the forward at its deadline, and an interrupt or the death of `rr` ends ssh.

use super::Ports;
use super::tunnel::{End, ListeningForward, SshEnded, Started, StartingForward};
use crate::config::Session;
use crate::domain::exit::RemoteExit;
use crate::domain::fwd::{
    Browser, Closed, Endpoint, Listener, LoopbackPort, OpenUrl, Plan, SocketPath,
};
use crate::domain::fwd::{Transport, Ttl, Wanted};
use crate::domain::outcome::Outcome;
use crate::ports::{Deadline, Net};
use anyhow::{Context, Result};

pub struct Args {
    pub wanted: Wanted,
    pub local: Option<LoopbackPort>,
    pub transport: Transport,
    pub ttl: Ttl,
    pub browser: Browser,
}

/// The local machine as `fwd` sees it, besides the process ports: its network and files, and the
/// private directory (`XDG_RUNTIME_DIR`) a socket may live in.
pub struct Machine<'a> {
    pub net: &'a dyn Net,
    pub runtime_dir: Option<&'a str>,
}

pub fn run(session: &Session, args: Args, ports: &Ports, machine: &Machine) -> Result<Outcome> {
    let plan = session
        .forwards
        .plan(args.wanted, args.local, args.transport, args.browser)?;
    let endpoint = endpoint(&plan, machine)?;
    let served = serve(session, &plan, &endpoint, args.ttl, (ports, machine));
    // The path was free before ssh started (a taken one is refused above), so a file there is
    // ssh's: a SIGKILLed ssh leaves it behind and the next `rr fwd` of this name would be
    // refused. A socket file is removed only once ssh is known to be gone: one that could not be
    // stopped may still own it. (Two `rr fwd` of the same name at once is not supported: the one
    // that fails to bind would remove the other's socket.)
    if let Endpoint::Unix(path) = &endpoint {
        match ssh_after(&served) {
            SshAfter::Gone => {
                if let Err(err) = machine.net.remove_socket(path) {
                    ports.notes.note(&format!("rr: {err:#}"));
                }
            }
            SshAfter::MayRun => ports.notes.note(&format!(
                "rr: ssh may still be running, so {} is left in place",
                path.as_str()
            )),
        }
    }
    served
}

/// Whether ssh is known to have ended once `serve` has returned.
enum SshAfter {
    /// Ended and reaped, however `serve` ended (the handle also ends it when dropped).
    Gone,
    /// ssh is not known to be gone (no `SshEnded` in the error).
    MayRun,
}

fn ssh_after(served: &Result<Outcome>) -> SshAfter {
    match served {
        Ok(_) => SshAfter::Gone,
        Err(err) if err.downcast_ref::<SshEnded>().is_some() => SshAfter::Gone,
        // an error that does not prove ssh gone keeps the file
        Err(_) => SshAfter::MayRun,
    }
}

fn serve(
    session: &Session,
    plan: &Plan,
    endpoint: &Endpoint,
    ttl: Ttl,
    (ports, machine): (&Ports, &Machine),
) -> Result<Outcome> {
    let starting =
        StartingForward::start(ports.exec, &session.host, &plan.target, endpoint.clone())?;
    let listening = match starting.wait_listening(machine.net, ports.clock)? {
        Started::Listening(listening) => listening,
        Started::Interrupted => return closed(End::Interrupted, ports),
    };
    announce(&listening, plan, ttl, ports);
    let until = Deadline::after(ports.clock.now(), ttl.duration());
    closed(listening.hold(until, ports.clock)?, ports)
}

/// What the forward's end means for the exit code. ssh ending by itself with 0 is a lost forward,
/// not a success: nothing asked it to stop. A forward `rr` closed is `rr`'s own outcome; only a
/// code ssh ended with is a remote one, reported as a forward that ssh ended.
fn closed(end: End, ports: &Ports) -> Result<Outcome> {
    match end {
        End::Expired => {
            ports.notes.note("rr: time to live reached, forward closed");
            Ok(Outcome::Forwarded(Closed::Expired))
        }
        End::Interrupted => {
            ports.notes.note("rr: interrupted, forward closed");
            Ok(Outcome::Forwarded(Closed::Interrupted))
        }
        End::Exited(0) => Err(SshEnded::error(
            "ssh ended by itself before the time to live: the forward is gone",
        )),
        End::Exited(code) => {
            ports
                .notes
                .note(&format!("rr: ssh ended ({code}), forward closed"));
            Ok(Outcome::Forwarded(Closed::SshEnded(RemoteExit::new(code))))
        }
    }
}

/// The local end to listen on, checked free before ssh is started.
fn endpoint(plan: &Plan, machine: &Machine) -> Result<Endpoint> {
    match plan.listener {
        Listener::Tcp { local, .. } => machine.net.free_port(local).map(Endpoint::Tcp),
        Listener::Socket => {
            let dir = machine
                .runtime_dir
                .context("--socket needs a private directory: set XDG_RUNTIME_DIR")?;
            let path = SocketPath::new(dir, &plan.name)?;
            machine.net.socket_unused(&path)?;
            Ok(Endpoint::Unix(path))
        }
    }
}

/// Tell the user where the forward is, and open the browser if asked. Everything printed is built
/// from validated values, so none of it needs `sanitize`.
fn announce(listening: &ListeningForward, plan: &Plan, ttl: Ttl, ports: &Ports) {
    let target = &plan.target;
    match listening.endpoint() {
        Endpoint::Tcp(port) => {
            let url = OpenUrl::of_forward(*port);
            ports
                .notes
                .note(&format!("rr: {} -> {target} (closes in {ttl})", url.as_str()));
            // launched and left: waiting for the browser would hide the ttl and an ssh exit
            if plan.browser() == Browser::Open
                && let Err(err) = ports.exec.launch(&url.opener())
            {
                ports.notes.note(&format!(
                    "rr: could not open a browser ({err:#}); open the URL yourself"
                ));
            }
        }
        Endpoint::Unix(path) => ports.notes.note(&format!(
            "rr: socket {} -> {target} (closes in {ttl}); try curl --unix-socket {} http://localhost/",
            path.as_str(),
            path.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, session};
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::fwd::{ForwardName, ForwardSpec, ForwardTarget};
    use crate::domain::outcome::{Format, render};
    use crate::ports::Ended;
    use crate::ports::fake::{FAKE_CHILD_PID, FakeChild, FakeExec, FakeNet, How};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    const RUNTIME: &str = "/run/user/1";
    const SOCKET: &str = "/run/user/1/rr-5000.sock";

    struct World {
        fakes: Fakes,
        net: FakeNet,
        killed: Rc<Cell<u32>>,
    }

    /// A world whose ssh child answers `waits`, and whose port answers `accepts`.
    fn world(waits: Vec<Option<i32>>, accepts: Vec<bool>) -> World {
        let (child, killed) = FakeChild::new(waits);
        world_of(child, killed, accepts)
    }

    fn world_of(child: FakeChild, killed: Rc<Cell<u32>>, accepts: Vec<bool>) -> World {
        let mut fakes = Fakes::remote_streaming(vec![]);
        fakes.exec = FakeExec::new(vec![], vec![]).with_children(vec![child]);
        World {
            fakes,
            net: FakeNet::new(Some(5051), accepts),
            killed,
        }
    }

    fn args(wanted: &str) -> Args {
        Args {
            wanted: Wanted::parse(wanted).unwrap(),
            local: None,
            transport: Transport::Tcp,
            ttl: Ttl::parse("30m").unwrap(),
            browser: Browser::Leave,
        }
    }

    fn socket_args() -> Args {
        Args {
            transport: Transport::Socket,
            ..args("n:5000")
        }
    }

    fn go(world: &World, session: &Session, args: Args, runtime: Option<&str>) -> Result<Outcome> {
        let machine = Machine {
            net: &world.net,
            runtime_dir: runtime,
        };
        run(session, args, &world.fakes.ports(), &machine)
    }

    fn notes(world: &World) -> Vec<String> {
        world.fakes.notes.0.borrow().clone()
    }

    fn exit(outcome: &Outcome) -> Exit {
        render(outcome, Format::Text).exit
    }

    fn slept(world: &World) -> Duration {
        world.fakes.clock.slept.borrow().iter().sum()
    }

    fn removed(world: &World) -> Vec<String> {
        world.net.removed.borrow().clone()
    }

    #[test]
    fn it_starts_ssh_waits_for_the_port_prints_the_url_and_closes_at_the_ttl() {
        let w = world(vec![None], vec![false, false, true]);
        let outcome = go(&w, &session(), args("gpu-01:5000"), None).unwrap();
        let want = [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ServerAliveInterval=30",
            "-N",
            "-L",
            "127.0.0.1:5051:gpu-01:5000",
            "--",
            "cluster",
        ];
        assert_eq!(w.fakes.exec.words(), [want.map(String::from).to_vec()]);
        assert_eq!(w.fakes.exec.calls.borrow()[0].1, How::Spawn);
        assert_eq!(
            notes(&w),
            [
                "rr: http://127.0.0.1:5051/ -> gpu-01:5000 (closes in 30m)",
                "rr: time to live reached, forward closed"
            ]
        );
        assert_eq!(w.killed.get(), 1, "the ttl kills ssh");
        // the ttl counts from the announcement, not from the start: the two polls and the settle
        // before it (under a second) are the only slack
        let ttl = Duration::from_secs(30 * 60);
        assert!(slept(&w) >= ttl && slept(&w) < ttl + Duration::from_secs(2));
        assert_eq!(exit(&outcome), Exit::Success);
        assert!(matches!(outcome, Outcome::Forwarded(Closed::Expired)));
    }

    #[test]
    fn without_a_ttl_the_forward_lasts_eight_hours() {
        let w = world(vec![None], vec![true]);
        let given = Args {
            ttl: Ttl::DEFAULT,
            ..args("n:1")
        };
        go(&w, &session(), given, None).unwrap();
        let eight_hours = Duration::from_secs(8 * 3600);
        assert!(slept(&w) >= eight_hours && slept(&w) < eight_hours + Duration::from_secs(5));
    }

    #[test]
    fn an_ssh_that_ends_by_itself_ends_the_forward_with_its_code() {
        let w = world(vec![None, None, None, Some(255)], vec![true]);
        let outcome = go(&w, &session(), args("n:1"), None).unwrap();
        assert_eq!(exit(&outcome), Exit::remote(255));
        assert!(
            matches!(outcome, Outcome::Forwarded(Closed::SshEnded(_))),
            "a lost forward, not a remote command"
        );
        assert_eq!(w.killed.get(), 0);
        let last = notes(&w).pop().unwrap();
        assert_eq!(last, "rr: ssh ended (255), forward closed");
    }

    #[test]
    fn an_ssh_that_ends_by_itself_with_zero_is_a_lost_forward() {
        let w = world(vec![None, None, None, Some(0)], vec![true]);
        let err = go(&w, &session(), args("n:1"), None).unwrap_err();
        assert!(err.to_string().contains("the forward is gone"), "{err}");
    }

    #[test]
    fn an_interrupt_closes_the_forward_with_130_not_an_ssh_code() {
        let (child, killed) = FakeChild::scripted(vec![None, None, None, Some(Ended::Interrupted)]);
        let w = world_of(child, killed, vec![true]);
        let outcome = go(&w, &session(), args("n:1"), None).unwrap();
        assert_eq!(exit(&outcome), Exit::Rr(RrCode::Interrupted));
        assert!(matches!(outcome, Outcome::Forwarded(Closed::Interrupted)));
        assert_eq!(notes(&w).pop().unwrap(), "rr: interrupted, forward closed");
        // an interrupt before the forward listens is an interrupt too, and prints no URL
        let (child, killed) = FakeChild::scripted(vec![Some(Ended::Interrupted)]);
        let w = world_of(child, killed, vec![false]);
        let outcome = go(&w, &session(), args("n:1"), None).unwrap();
        assert_eq!(exit(&outcome), Exit::Rr(RrCode::Interrupted));
        assert_eq!(notes(&w), ["rr: interrupted, forward closed"]);
    }

    #[test]
    fn a_taken_port_fails_before_ssh_starts() {
        let mut w = world(vec![None], vec![true]);
        w.net.free = None;
        let err = go(&w, &session(), args("n:1"), None).unwrap_err();
        assert_eq!(err.to_string(), "port in use");
        assert!(w.fakes.exec.calls.borrow().is_empty());
    }

    #[test]
    fn ssh_that_exits_right_after_the_port_opened_prints_no_url() {
        // a foreign listener accepts while ssh is still failing to bind
        let w = world(vec![None, Some(255)], vec![true]);
        let err = go(&w, &session(), args("n:1"), None).unwrap_err();
        assert!(err.to_string().contains("ssh ended (255)"), "{err}");
        assert!(notes(&w).is_empty());
    }

    /// The race the ownership check closes: another process took the port after `rr` picked it.
    /// Its listener accepts, ssh is alive (still authenticating), and nothing may be announced.
    #[test]
    fn a_foreign_listener_is_never_announced_and_ssh_is_stopped() {
        for browser in [Browser::Leave, Browser::Open] {
            let mut w = world(vec![None], vec![true]);
            w.net.holder = None;
            let given = Args {
                browser,
                ..args("n:1")
            };
            let err = go(&w, &session(), given, None).unwrap_err();
            assert!(err.to_string().contains("not ssh's"), "{err}");
            assert!(
                notes(&w).is_empty(),
                "no URL for a listener that is not ssh's"
            );
            assert_eq!(w.killed.get(), 1, "ssh is stopped");
            assert_eq!(
                w.fakes.exec.calls.borrow().len(),
                1,
                "no browser is launched"
            );
        }
    }

    /// A listener of some other process, even one that is itself a child of nobody known, does not
    /// count: only the pid of the ssh `rr` started does.
    #[test]
    fn the_listener_must_be_held_by_the_ssh_child_on_the_forwarded_port() {
        let w = world(vec![None], vec![true]);
        go(&w, &session(), args("n:1"), None).unwrap();
        assert_eq!(*w.net.asked.borrow(), [(5051, FAKE_CHILD_PID)]);
        let mut w = world(vec![None], vec![true]);
        w.net.holder = Some(FAKE_CHILD_PID + 1);
        assert!(go(&w, &session(), args("n:1"), None).is_err());
        assert!(notes(&w).is_empty());
    }

    /// A socket in the private runtime directory has no port to take: it is checked as before.
    #[test]
    fn a_socket_forward_needs_no_port_owner() {
        let mut w = world(vec![None], vec![true]);
        w.net.holder = None;
        go(&w, &session(), socket_args(), Some(RUNTIME)).unwrap();
        assert!(w.net.asked.borrow().is_empty());
    }

    #[test]
    fn ssh_that_exits_before_the_port_opens_fails_and_a_silent_port_times_out() {
        let w = world(vec![Some(255)], vec![false]);
        assert!(go(&w, &session(), args("n:1"), None).is_err());
        assert!(notes(&w).is_empty());
        let w = world(vec![None], vec![false]);
        let err = go(&w, &session(), args("n:1"), None).unwrap_err();
        assert!(err.to_string().contains("did not open"), "{err}");
        assert_eq!(w.killed.get(), 1, "a failed start leaves no ssh behind");
        assert!(notes(&w).is_empty());
    }

    #[test]
    fn a_kill_that_fails_after_a_failed_start_is_part_of_the_error() {
        let (child, killed) = FakeChild::new(vec![None]);
        let w = world_of(child.with_failing_kill(), killed, vec![false]);
        let err = go(&w, &session(), args("n:1"), None).unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("did not open"), "{shown}");
        assert!(shown.contains("could not be stopped"), "{shown}");
    }

    fn with_forward(local: Option<u16>) -> Session {
        let mut session = session();
        let spec = ForwardSpec {
            target: ForwardTarget::parse("gpu-02:5000").unwrap(),
            local: local.map(|p| LoopbackPort::from_u16(p).unwrap()),
        };
        session.forwards = [(ForwardName::parse("mlflow").unwrap(), spec)]
            .into_iter()
            .collect();
        session
    }

    #[test]
    fn a_configured_name_supplies_the_target_and_the_command_line_port_wins() {
        let listens_at =
            |w: &World, spec: &str| w.fakes.exec.words()[0].contains(&spec.to_string());
        let w = world(vec![None], vec![true]);
        go(&w, &with_forward(Some(6000)), args("mlflow"), None).unwrap();
        assert!(listens_at(&w, "127.0.0.1:6000:gpu-02:5000"));
        let w = world(vec![None], vec![true]);
        let given = Args {
            local: Some(LoopbackPort::from_u16(7000).unwrap()),
            ..args("mlflow")
        };
        go(&w, &with_forward(Some(6000)), given, None).unwrap();
        assert!(listens_at(&w, "127.0.0.1:7000:gpu-02:5000"));
        let w = world(vec![None], vec![true]);
        let err = go(&w, &with_forward(None), args("other"), None).unwrap_err();
        assert!(err.to_string().contains("no forward named other"), "{err}");
        assert!(w.fakes.exec.calls.borrow().is_empty());
    }

    #[test]
    fn a_socket_forward_listens_in_the_runtime_directory() {
        let w = world(vec![None], vec![true]);
        go(&w, &session(), socket_args(), Some(RUNTIME)).unwrap();
        let spec = format!("{SOCKET}:n:5000");
        assert!(w.fakes.exec.words()[0].contains(&spec));
        assert!(notes(&w)[0].starts_with(&format!("rr: socket {SOCKET} -> n:5000")));
    }

    #[test]
    fn a_socket_forward_that_cannot_start_starts_nothing_and_removes_nothing() {
        let mut w = world(vec![None], vec![true]);
        w.net.socket_taken = true;
        assert!(go(&w, &session(), socket_args(), Some(RUNTIME)).is_err());
        let err = go(&w, &session(), socket_args(), None).unwrap_err();
        assert!(err.to_string().contains("XDG_RUNTIME_DIR"), "{err}");
        let browser = Args {
            browser: Browser::Open,
            ..socket_args()
        };
        assert!(go(&w, &session(), browser, Some(RUNTIME)).is_err());
        let configured = Args {
            transport: Transport::Socket,
            ..args("mlflow")
        };
        assert!(go(&w, &with_forward(Some(6000)), configured, Some(RUNTIME)).is_err());
        assert!(w.fakes.exec.calls.borrow().is_empty(), "nothing started");
        assert!(
            removed(&w).is_empty(),
            "a file that was there before is not ours"
        );
    }

    /// ssh is gone however the forward ends, and then its socket file is too: a SIGKILLed ssh
    /// cannot unlink it, and the next `rr fwd` of the name would be refused.
    #[test]
    fn the_socket_file_is_removed_when_the_forward_ends_however_it_ends() {
        let ended = |waits: Vec<Option<i32>>, accepts: Vec<bool>| {
            let w = world(waits, accepts);
            let _ = go(&w, &session(), socket_args(), Some(RUNTIME));
            removed(&w)
        };
        let path = vec![SOCKET.to_string()];
        assert_eq!(ended(vec![None], vec![true]), path, "time to live reached");
        assert_eq!(ended(vec![None, Some(255)], vec![true]), path, "ssh ended");
        assert_eq!(ended(vec![None], vec![false]), path, "ssh never listened");
        assert_eq!(
            ended(vec![Some(255)], vec![false]),
            path,
            "ssh failed to start"
        );
    }

    /// ssh that could not be stopped may still own its socket: the file stays, and the user is
    /// told. Every place a stop can fail is covered, and where the child could be stopped the
    /// same run removes the file.
    #[test]
    fn a_socket_file_stays_unless_ssh_is_known_to_be_stopped() {
        let cases = vec![
            ("never listened", FakeChild::new(vec![None]), vec![false]),
            ("ttl reached", FakeChild::new(vec![None]), vec![true]),
            (
                "interrupted before listening",
                FakeChild::scripted(vec![Some(Ended::Interrupted)]),
                vec![false],
            ),
            (
                "interrupted while held",
                FakeChild::scripted(vec![None, None, None, Some(Ended::Interrupted)]),
                vec![true],
            ),
            (
                "poll fails while held",
                {
                    let (child, killed) = FakeChild::new(vec![None]);
                    (child.with_failing_wait_from(2), killed)
                },
                vec![true],
            ),
        ];
        for (what, (child, killed), accepts) in cases {
            let w = world_of(child.with_failing_kill(), killed, accepts);
            let err = go(&w, &session(), socket_args(), Some(RUNTIME)).unwrap_err();
            assert!(
                format!("{err:#}").contains("could not be stopped"),
                "{what}: {err:#}"
            );
            assert!(removed(&w).is_empty(), "{what}: the socket may be ssh's");
            assert!(
                notes(&w).pop().unwrap().contains("left in place"),
                "{what}: {:?}",
                notes(&w)
            );
        }
    }

    /// The same ends with a kill that works: ssh is gone, so the file goes.
    #[test]
    fn a_socket_file_goes_when_an_interrupt_or_a_failed_poll_ended_ssh() {
        let (child, killed) = FakeChild::scripted(vec![None, None, None, Some(Ended::Interrupted)]);
        let w = world_of(child, killed, vec![true]);
        go(&w, &session(), socket_args(), Some(RUNTIME)).unwrap();
        assert_eq!(removed(&w), [SOCKET]);
        assert_eq!(w.killed.get(), 1, "an interrupt ends ssh explicitly");
        let (child, killed) = FakeChild::new(vec![None]);
        let w = world_of(child.with_failing_wait_from(2), killed, vec![true]);
        assert!(go(&w, &session(), socket_args(), Some(RUNTIME)).is_err());
        assert_eq!(removed(&w), [SOCKET]);
        assert_eq!(w.killed.get(), 1);
    }

    /// An error that does not say whether ssh was reaped must not take the socket away.
    #[test]
    fn an_error_of_unknown_cause_keeps_the_socket() {
        let unknown: Result<Outcome> = Err(anyhow::anyhow!("something else"));
        assert!(matches!(ssh_after(&unknown), SshAfter::MayRun));
    }

    #[test]
    fn a_tcp_forward_removes_no_file() {
        let w = world(vec![None], vec![true]);
        go(&w, &session(), args("n:1"), None).unwrap();
        assert!(removed(&w).is_empty());
    }

    #[test]
    fn open_launches_the_browser_detached_with_the_url_as_one_argument() {
        let open = || Args {
            browser: Browser::Open,
            ..args("n:1")
        };
        let w = world(vec![None], vec![true]);
        go(&w, &session(), open(), None).unwrap();
        let calls = w.fakes.exec.calls.borrow();
        assert_eq!(calls[1].1, How::Launch);
        assert_eq!(calls[1].0.args(), ["http://127.0.0.1:5051/"]);
        drop(calls);
        let mut w = world(vec![None], vec![true]);
        let (child, _) = FakeChild::new(vec![None]);
        w.fakes.exec = FakeExec::new(vec![], vec![])
            .with_children(vec![child])
            .with_failing_launch();
        go(&w, &session(), open(), None).unwrap();
        let note = &notes(&w)[1];
        assert!(
            note.starts_with("rr: could not open a browser (could not start"),
            "{note}"
        );
    }
}
