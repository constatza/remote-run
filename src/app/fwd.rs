//! `rr fwd`: forward a port of the cluster to this machine over ssh, loopback only, for a bounded
//! time. The URL is printed only once ssh has stayed alive with the port accepting; `--ttl` ends
//! the forward at its deadline, and an interrupt or the death of `rr` ends ssh.

use super::Ports;
use super::tunnel::{End, ListeningForward, StartingForward};
use crate::config::Session;
use crate::domain::exit::RemoteExit;
use crate::domain::fwd::{Browser, Endpoint, OpenUrl, Plan, SocketPath, Transport, Ttl, Wanted};
use crate::domain::fwd::{LoopbackPort, argv};
use crate::domain::outcome::Outcome;
use crate::ports::{Clock, Deadline, Net};
use anyhow::{Context, Result, bail};

pub struct Args {
    pub wanted: Wanted,
    pub local: Option<LoopbackPort>,
    pub transport: Transport,
    pub ttl: Ttl,
    pub browser: Browser,
}

/// The local machine as `fwd` sees it, besides the process ports: its network and files, its
/// clock, and the private directory (`XDG_RUNTIME_DIR`) a socket may live in.
pub struct Machine<'a> {
    pub net: &'a dyn Net,
    pub clock: &'a dyn Clock,
    pub runtime_dir: Option<&'a str>,
}

pub fn run(session: &Session, args: Args, ports: &Ports, machine: &Machine) -> Result<Outcome> {
    let plan = session.forwards.plan(args.wanted, args.local)?;
    let endpoint = endpoint(&plan, args.transport, args.browser, machine)?;
    let ssh = argv(&session.host, &endpoint, &plan.target);
    let starting = StartingForward::start(ports.exec, &ssh, endpoint)?;
    let listening = starting.wait_listening(machine.net, machine.clock)?;
    announce(&listening, &plan, (args.ttl, args.browser), ports)?;
    let until = Deadline::after(machine.clock.now(), args.ttl.duration());
    let code = match listening.hold(until, machine.clock)? {
        End::Expired => {
            ports.notes.note("rr: time to live reached, forward closed");
            0
        }
        End::Ssh(code) => {
            ports
                .notes
                .note(&format!("rr: ssh ended ({code}), forward closed"));
            code
        }
    };
    Ok(Outcome::Remote(RemoteExit::new(code)))
}

/// The local end to listen on, checked free before ssh is started.
fn endpoint(
    plan: &Plan,
    transport: Transport,
    browser: Browser,
    machine: &Machine,
) -> Result<Endpoint> {
    match (transport, browser) {
        (Transport::Tcp, _) => machine.net.free_port(plan.local).map(Endpoint::Tcp),
        (Transport::Socket, Browser::Open) => bail!("--open needs a TCP forward; drop --socket"),
        (Transport::Socket, Browser::Leave) => {
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
fn announce(
    listening: &ListeningForward,
    plan: &Plan,
    (ttl, browser): (Ttl, Browser),
    ports: &Ports,
) -> Result<()> {
    let target = &plan.target;
    match listening.endpoint() {
        Endpoint::Tcp(port) => {
            let url = OpenUrl::of_forward(*port);
            ports.notes.note(&format!("rr: {} -> {target} (closes in {ttl})", url.as_str()));
            if browser == Browser::Open && !matches!(ports.exec.stream(&url.opener()), Ok(0)) {
                ports.notes.note("rr: could not open a browser; open the URL yourself");
            }
        }
        Endpoint::Unix(path) => ports.notes.note(&format!(
            "rr: socket {} -> {target} (closes in {ttl}); try curl --unix-socket {} http://localhost/",
            path.as_str(),
            path.as_str()
        )),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, session};
    use super::*;
    use crate::domain::exit::Exit;
    use crate::domain::fwd::{ForwardName, ForwardSpec, ForwardTarget};
    use crate::domain::outcome::{Format, render};
    use crate::ports::Child;
    use crate::ports::fake::{FakeChild, FakeClock, FakeExec, FakeNet, How, KILLED_EXIT};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    struct World {
        fakes: Fakes,
        net: FakeNet,
        clock: FakeClock,
        killed: Rc<Cell<u32>>,
    }

    /// A world whose ssh child answers `waits`, and whose port answers `accepts`.
    fn world(waits: Vec<Option<i32>>, accepts: Vec<bool>) -> World {
        let (child, killed) = FakeChild::new(waits);
        let mut fakes = Fakes::remote_streaming(vec![]);
        fakes.exec = FakeExec::new(vec![], vec![0]).with_children(vec![child]);
        World {
            fakes,
            net: FakeNet::new(Some(5051), accepts),
            clock: FakeClock::default(),
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

    fn go(world: &World, session: &Session, args: Args, runtime: Option<&str>) -> Result<Outcome> {
        let machine = Machine {
            net: &world.net,
            clock: &world.clock,
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
        world.clock.slept.borrow().iter().sum()
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
        assert!(slept(&w) >= Duration::from_secs(30 * 60));
        assert_eq!(exit(&outcome), Exit::Success);
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
        assert_eq!(w.killed.get(), 0);
        let last = notes(&w).pop().unwrap();
        assert_eq!(last, "rr: ssh ended (255), forward closed");
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
    fn a_socket_forward_listens_in_the_runtime_directory_and_never_over_a_stale_file() {
        let socket = || Args {
            transport: Transport::Socket,
            ..args("n:5000")
        };
        let w = world(vec![None], vec![true]);
        go(&w, &session(), socket(), Some("/run/user/1")).unwrap();
        let spec = "/run/user/1/rr-5000.sock:n:5000".to_string();
        assert!(w.fakes.exec.words()[0].contains(&spec));
        assert!(notes(&w)[0].starts_with("rr: socket /run/user/1/rr-5000.sock -> n:5000"));

        let mut w = world(vec![None], vec![true]);
        w.net.socket_taken = true;
        assert!(go(&w, &session(), socket(), Some("/run/user/1")).is_err());
        let err = go(&w, &session(), socket(), None).unwrap_err();
        assert!(err.to_string().contains("XDG_RUNTIME_DIR"), "{err}");
        let browser = Args {
            browser: Browser::Open,
            ..socket()
        };
        assert!(go(&w, &session(), browser, Some("/run/user/1")).is_err());
        assert!(w.fakes.exec.calls.borrow().is_empty(), "nothing started");
    }

    #[test]
    fn open_launches_the_browser_with_the_url_as_one_argument() {
        let open = || Args {
            browser: Browser::Open,
            ..args("n:1")
        };
        let w = world(vec![None], vec![true]);
        go(&w, &session(), open(), None).unwrap();
        let calls = w.fakes.exec.calls.borrow();
        assert_eq!(calls[1].1, How::Stream);
        assert_eq!(calls[1].0.args(), ["http://127.0.0.1:5051/"]);
        drop(calls);
        let mut w = world(vec![None], vec![true]);
        let (child, _) = FakeChild::new(vec![None]);
        w.fakes.exec = FakeExec::new(vec![], vec![1]).with_children(vec![child]);
        go(&w, &session(), open(), None).unwrap();
        assert!(notes(&w).iter().any(|n| n.contains("could not open")));
    }

    #[test]
    fn a_killed_fake_child_reads_as_ended() {
        let (mut child, _) = FakeChild::new(vec![None]);
        child.kill().unwrap();
        assert_eq!(child.try_wait().unwrap(), Some(KILLED_EXIT));
    }
}
