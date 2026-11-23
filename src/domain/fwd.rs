//! `rr fwd`: the values of an ssh port forward and the argv that opens it. Everything that can
//! reach `ssh` is a validated type, and no type has a field for a bind address, so a forward can
//! only ever listen on the loopback interface.

use crate::domain::argv::Argv;
use crate::domain::host::Host;
use crate::domain::ssh::SSH_OPTIONS;
use anyhow::{Context, Result, bail};
use core::time::Duration;
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU16;

/// The one address a forward listens on. A wildcard bind would put an unauthenticated service on
/// the network, so the argv builder spells it out and nothing can change it.
const LOOPBACK: &str = "127.0.0.1";

/// How long a forward lives when `--ttl` is not given: a forgotten tunnel is the risk this bounds.
pub const DEFAULT_TTL: Duration = Duration::from_secs(8 * 3600);

/// The longest `--ttl` accepted, so "forever" is not one typo away.
pub const MAX_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// `sun_path` holds 108 bytes including the final NUL; a longer socket path cannot be bound.
const SUN_PATH_MAX: usize = 107;

/// The longest forward name: it ends up in a socket file name under the runtime directory.
const NAME_MAX: usize = 32;

/// A port from digits only: `u16::from_str` would also take a leading `+`.
fn parse_port(text: &str) -> Result<NonZeroU16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        bail!("not a port number");
    }
    Ok(text.parse()?)
}

/// `host:port` on the far side of the tunnel, as the ssh server resolves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardTarget {
    host: String,
    port: NonZeroU16,
}

impl ForwardTarget {
    /// `text` as `host:port`. The host is a plain name (letters, digits, `.`, `_`, `-`; no leading
    /// `-`), so it cannot be read as an option or carry the `:` that separates `-L` fields.
    pub fn parse(text: &str) -> Result<Self> {
        let Some((host, port)) = text.rsplit_once(':') else {
            bail!("not a forward target: {text:?} (expected host:port)");
        };
        let host_ok = !host.is_empty()
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !host_ok {
            bail!(
                "not a forward target: {text:?} (the host uses letters, digits, '.', '_' and '-', \
                 and does not start with '-')"
            );
        }
        let port = parse_port(port)
            .with_context(|| format!("not a forward target: {text:?} (port is 1-65535)"))?;
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }
}

impl fmt::Display for ForwardTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// A local TCP port, 1-65535.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopbackPort(NonZeroU16);

impl LoopbackPort {
    pub fn parse(text: &str) -> Result<Self> {
        parse_port(text)
            .map(Self)
            .with_context(|| format!("not a port: {text:?} (1-65535)"))
    }

    pub fn from_u16(port: u16) -> Result<Self> {
        NonZeroU16::new(port)
            .map(Self)
            .context("port 0 is not a port")
    }

    pub fn get(self) -> u16 {
        self.0.get()
    }
}

impl fmt::Display for LoopbackPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The name of a forward in the config table, and the stem of its socket file. `[A-Za-z0-9_-]+`
/// keeps `../x` and any separator out of that file name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForwardName(String);

impl ForwardName {
    pub fn parse(text: &str) -> Result<Self> {
        let ok = !text.is_empty()
            && !text.starts_with('-')
            && text.len() <= NAME_MAX
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
        if !ok {
            bail!(
                "not a forward name: {text:?} (1-{NAME_MAX} letters, digits, '_' and '-'; \
                 use host:port to forward a target directly)"
            );
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ForwardName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the user typed to say where to forward: a target, or the name of a configured forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wanted {
    Target(ForwardTarget),
    Named(ForwardName),
}

impl Wanted {
    /// A `:` makes it a target; a configured name has none.
    pub fn parse(text: &str) -> Result<Self> {
        if text.contains(':') {
            ForwardTarget::parse(text).map(Self::Target)
        } else {
            ForwardName::parse(text).map(Self::Named)
        }
    }
}

/// A forward from the config: where it goes, and the local port it insists on, if any. Without
/// `local` a free port is picked per run, so two forwards never share the cookies of one
/// `127.0.0.1:<port>` origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardSpec {
    pub target: ForwardTarget,
    pub local: Option<LoopbackPort>,
}

/// The forwards configured by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForwardTable(BTreeMap<ForwardName, ForwardSpec>);

impl FromIterator<(ForwardName, ForwardSpec)> for ForwardTable {
    fn from_iter<I: IntoIterator<Item = (ForwardName, ForwardSpec)>>(entries: I) -> Self {
        Self(entries.into_iter().collect())
    }
}

/// The local end one `rr fwd` listens on. A Unix socket has no port to choose and no browser to
/// open, so those cannot be combined with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listener {
    /// A TCP port on loopback: `local` when insisted on, else a free one.
    Tcp {
        local: Option<LoopbackPort>,
        browser: Browser,
    },
    /// A Unix socket in the runtime directory.
    Socket,
}

/// What one `rr fwd` will forward, once the table has been consulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Names the socket file. A forward given as `host:port` is named by its remote port.
    pub name: ForwardName,
    pub target: ForwardTarget,
    pub listener: Listener,
}

impl Plan {
    /// Whether to open the browser once the forward listens: only a TCP forward has a URL.
    pub fn browser(&self) -> Browser {
        match self.listener {
            Listener::Tcp { browser, .. } => browser,
            Listener::Socket => Browser::Leave,
        }
    }
}

impl ForwardTable {
    /// The plan for `wanted`; `local` from the command line wins over the table's. A socket takes
    /// neither a local port (from the command line or the table) nor `--open`.
    pub fn plan(
        &self,
        wanted: Wanted,
        local: Option<LoopbackPort>,
        transport: Transport,
        browser: Browser,
    ) -> Result<Plan> {
        let (name, target, configured) = match wanted {
            Wanted::Target(target) => (ForwardName::parse(&target.port.to_string())?, target, None),
            Wanted::Named(name) => {
                let Some(spec) = self.0.get(&name) else {
                    let known: Vec<&str> = self.0.keys().map(ForwardName::as_str).collect();
                    bail!("no forward named {name} in the config (configured: {known:?})");
                };
                (name, spec.target.clone(), spec.local)
            }
        };
        let local = local.or(configured);
        let listener = match (transport, local, browser) {
            (Transport::Tcp, local, browser) => Listener::Tcp { local, browser },
            (Transport::Socket, None, Browser::Leave) => Listener::Socket,
            (Transport::Socket, Some(_), _) => {
                bail!(
                    "--socket has no TCP port: drop --local, or the local port of {name} in the config"
                )
            }
            (Transport::Socket, None, Browser::Open) => {
                bail!("--open needs a TCP forward; drop --socket")
            }
        };
        Ok(Plan {
            name,
            target,
            listener,
        })
    }
}

/// Which kind of local listener the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A TCP port on loopback: what a browser can use.
    Tcp,
    /// A Unix socket in the private runtime directory: for command-line clients.
    Socket,
}

/// Whether to open the forward in the user's browser once it listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browser {
    Leave,
    Open,
}

/// A forward that `rr` itself closed, as opposed to one whose ssh ended by itself (that is a
/// remote exit code). Both are decisions of `rr`, so neither is reported as a remote result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    /// Its time to live ran out: the planned end, exit 0.
    Expired,
    /// `rr` was interrupted: exit 130.
    Interrupted,
}

/// How long a forward may live, 1 s up to [`MAX_TTL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ttl(Duration);

impl Ttl {
    pub const DEFAULT: Ttl = Ttl(DEFAULT_TTL);

    /// `90s`, `30m`, `8h` or `2d`.
    pub fn parse(text: &str) -> Result<Self> {
        let bad = || format!("not a time to live: {text:?} (use 90s, 30m, 8h or 2d, at most 7d)");
        let (digits, seconds) = [("s", 1_u64), ("m", 60), ("h", 3600), ("d", 86_400)]
            .into_iter()
            .find_map(|(unit, seconds)| Some((text.strip_suffix(unit)?, seconds)))
            .with_context(bad)?;
        // digits only: `u64::from_str` would also take a leading `+`
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            bail!(bad());
        }
        let count: u64 = digits.parse().with_context(bad)?;
        let ttl = Duration::from_secs(count.saturating_mul(seconds));
        if ttl.is_zero() || ttl > MAX_TTL {
            bail!(bad());
        }
        Ok(Self(ttl))
    }

    pub fn duration(self) -> Duration {
        self.0
    }
}

impl fmt::Display for Ttl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.as_secs();
        match () {
            () if s.is_multiple_of(86_400) => write!(f, "{}d", s / 86_400),
            () if s.is_multiple_of(3600) => write!(f, "{}h", s / 3600),
            () if s.is_multiple_of(60) => write!(f, "{}m", s / 60),
            () => write!(f, "{s}s"),
        }
    }
}

/// An absolute path for a forward's local Unix socket: short enough for `sun_path`, and free of
/// the `:` that separates `-L` fields and of anything a shell or a terminal would treat specially.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketPath(String);

impl SocketPath {
    /// `rr-<name>.sock` in `dir`. The socket is private only if `dir` is: `rr` relies on `dir` being
    /// the runtime directory (`XDG_RUNTIME_DIR`, which the system makes private) and does not check.
    pub fn new(dir: &str, name: &ForwardName) -> Result<Self> {
        let path = format!("{}/rr-{}.sock", dir.trim_end_matches('/'), name.as_str());
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-');
        if !dir.starts_with('/') || !path.chars().all(plain) {
            bail!(
                "not a usable socket directory: {dir:?} (an absolute path of letters, digits, '/', '.', '_', '-')"
            );
        }
        if path.len() > SUN_PATH_MAX {
            bail!("socket path too long for a Unix socket ({SUN_PATH_MAX} bytes at most): {path}");
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the local end of a forward listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `127.0.0.1:<port>`: what a browser can use.
    Tcp(LoopbackPort),
    /// A Unix socket, in the private runtime directory: for command-line clients.
    Unix(SocketPath),
}

/// The `ssh` argv that forwards `endpoint` to `target` through `host` and runs nothing else.
///
/// `ExitOnForwardFailure` makes a failed bind end ssh instead of leaving a tunnel-less session.
/// The master connection is switched off: through a shared master a forward outlives the ssh
/// process that asked for it, and killing that process would not close the tunnel.
pub fn argv(host: &Host, endpoint: &Endpoint, target: &ForwardTarget) -> Argv {
    let local = match endpoint {
        Endpoint::Tcp(port) => format!("{LOOPBACK}:{port}"),
        Endpoint::Unix(path) => path.as_str().to_string(),
    };
    let mut args: Vec<String> = SSH_OPTIONS.iter().map(|s| s.to_string()).collect();
    for option in [
        "ExitOnForwardFailure=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "ServerAliveInterval=30",
    ] {
        args.extend(["-o".to_string(), option.to_string()]);
    }
    args.extend(["-N".into(), "-L".into(), format!("{local}:{target}")]);
    // `--` ends the options; `Host` already refuses a leading `-`
    args.extend(["--".into(), host.as_str().into()]);
    Argv::new("ssh", args)
}

/// The `st` column of `/proc/net/tcp` for a socket in LISTEN state.
const PROC_LISTEN: &str = "0A";

/// The column of `/proc/net/tcp` that holds the socket's inode.
const PROC_INODE_COLUMN: usize = 9;

/// Whether a listener on `127.0.0.1:port` belongs to a process, from the text of its
/// `/proc/net/tcp` (`table`) and the link texts of its `/proc/<pid>/fd/*` (`links`, each
/// `socket:[inode]` for a socket). A connect to the port succeeds against any listener, so this is
/// what ties the port to ssh: a process that took the port between `rr` choosing it and ssh
/// binding it holds a different inode. Anything unreadable or unexpected is "not held".
pub fn listener_held(table: &str, links: &[String], port: LoopbackPort) -> bool {
    // the kernel prints the address as the raw u32 in host byte order
    let local = format!(
        "{:08X}:{:04X}",
        u32::from_ne_bytes([127, 0, 0, 1]),
        port.get()
    );
    let held: Vec<u64> = links.iter().filter_map(|l| socket_inode(l)).collect();
    table
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|cols| cols.get(1) == Some(&local.as_str()) && cols.get(3) == Some(&PROC_LISTEN))
        .filter_map(|cols| cols.get(PROC_INODE_COLUMN)?.parse::<u64>().ok())
        .any(|inode| held.contains(&inode))
}

/// The inode in a `/proc/<pid>/fd` link text `socket:[inode]`, digits only.
fn socket_inode(link: &str) -> Option<u64> {
    let digits = link.strip_prefix("socket:[")?.strip_suffix(']')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The URL `--open` hands to a browser: `http`, on the loopback host, at the forward's own port,
/// without user information. It can only be made from a [`LoopbackPort`], so those parts hold by
/// construction and no text is parsed: no input path reaches a URL, so a parser has nothing to
/// check. A user-supplied URL would need one back, with its scheme, host, port and login compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenUrl(String);

impl OpenUrl {
    /// The URL of the service behind a TCP forward at `port`.
    pub fn of_forward(port: LoopbackPort) -> Self {
        Self(format!("http://{LOOPBACK}:{port}/"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The program that opens it in the user's browser; the URL is one argument, never shell text.
    pub fn opener(&self) -> Argv {
        let program = match () {
            () if cfg!(target_os = "macos") => "open",
            () if cfg!(windows) => "explorer",
            () => "xdg-open",
        };
        Argv::new(program, vec![self.as_str().to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(n: u16) -> LoopbackPort {
        LoopbackPort::from_u16(n).unwrap()
    }

    #[test]
    fn a_target_is_host_colon_port() {
        for (text, shown) in [
            ("gpu-01:5000", "gpu-01:5000"),
            ("node.example.org:65535", "node.example.org:65535"),
            ("localhost:1", "localhost:1"),
            ("a_b:80", "a_b:80"),
        ] {
            assert_eq!(ForwardTarget::parse(text).unwrap().to_string(), shown);
        }
        for text in [
            "",
            "host",
            ":80",
            "host:",
            "host:0",
            "host:65536",
            "host:-1",
            "host:+80",
            "-h:80",
            "-oProxyCommand=x:80",
            "a b:80",
            "a:b:80",
            "[::1]:80",
            "h;id:80",
            "h\n:80",
            "h:80 ",
            "user@h:80",
            "127.0.0.1:80:90",
        ] {
            assert!(ForwardTarget::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_name_cannot_walk_out_of_the_socket_directory() {
        for text in ["mlflow", "a-b_c", "A1", &"x".repeat(32)] {
            assert_eq!(ForwardName::parse(text).unwrap().as_str(), text);
        }
        for text in [
            "",
            "../x",
            "a/b",
            "a.b",
            ".",
            "a b",
            "a:b",
            "-",
            &"x".repeat(33),
            "é",
        ] {
            assert!(ForwardName::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn what_has_a_colon_is_a_target_and_what_has_none_is_a_name() {
        assert!(matches!(Wanted::parse("n:1").unwrap(), Wanted::Target(_)));
        assert!(matches!(Wanted::parse("n").unwrap(), Wanted::Named(_)));
        assert!(Wanted::parse("n:").is_err());
        assert!(Wanted::parse("-x").is_err());
    }

    #[test]
    fn a_ttl_has_a_unit_and_a_ceiling() {
        for (text, secs, shown) in [
            ("90s", 90, "90s"),
            ("30m", 1800, "30m"),
            ("8h", 28_800, "8h"),
            ("2d", 172_800, "2d"),
            ("120m", 7200, "2h"),
        ] {
            let ttl = Ttl::parse(text).unwrap();
            assert_eq!(
                (ttl.duration().as_secs(), ttl.to_string()),
                (secs, shown.into())
            );
        }
        for text in [
            "",
            "8",
            "h",
            "0s",
            "8x",
            "-1h",
            "1.5h",
            "8 h",
            "8H",
            "8d1h",
            "8d ",
            "+5m",
            "8d",
            "8é",
            "é",
            "8\u{661}h",
            "169h",
            "99999999999999999999h",
        ] {
            assert!(Ttl::parse(text).is_err(), "{text:?}");
        }
        assert!(Ttl::parse("7d").is_ok() && Ttl::parse("168h").is_ok());
        assert_eq!(Ttl::DEFAULT.to_string(), "8h");
    }

    #[test]
    fn ports_are_1_to_65535() {
        assert_eq!(LoopbackPort::parse("5051").unwrap().get(), 5051);
        for text in ["", "0", "65536", "-1", "x", "80 "] {
            assert!(LoopbackPort::parse(text).is_err(), "{text:?}");
        }
        assert!(LoopbackPort::from_u16(0).is_err());
    }

    #[test]
    fn a_socket_path_is_absolute_plain_and_short() {
        let name = ForwardName::parse("mlflow").unwrap();
        let path = SocketPath::new("/run/user/1000", &name).unwrap();
        assert_eq!(path.as_str(), "/run/user/1000/rr-mlflow.sock");
        assert_eq!(SocketPath::new("/run/user/1000/", &name).unwrap(), path);
        for dir in ["", "run/user", "/run/a:b", "/run/a b", "/run/$x", "/run/é"] {
            assert!(SocketPath::new(dir, &name).is_err(), "{dir:?}");
        }
        let long = format!("/{}", "d".repeat(100));
        assert!(SocketPath::new(&long, &name).is_err());
    }

    /// `sun_path` is 108 bytes with its NUL: 107 bind, 108 do not.
    #[test]
    fn a_socket_path_of_107_bytes_is_the_longest() {
        let name = ForwardName::parse("m").unwrap();
        // "/rr-m.sock" is 10 bytes
        let dir = |len: usize| format!("/{}", "d".repeat(len - 1));
        assert_eq!(
            SocketPath::new(&dir(97), &name).unwrap().as_str().len(),
            107
        );
        assert!(SocketPath::new(&dir(98), &name).is_err());
    }

    /// A `/proc/net/tcp` as the kernel writes it: 127.0.0.1:8080 and 127.0.0.1:8081 listening, a
    /// connected socket (state 01) with 8080 as local port, and a wildcard listener on 8082.
    const TCP_TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0000000000000000 100 0 0 10 0
   2: 0100007F:1F90 0100007F:D2E4 01 00000000:00000000 00:00000000 00000000  1000        0 333 1 0000000000000000 100 0 0 10 0
   3: 00000000:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 444 1 0000000000000000 100 0 0 10 0
";

    fn links(inodes: &[&str]) -> Vec<String> {
        inodes.iter().map(|n| format!("socket:[{n}]")).collect()
    }

    /// The table with the address column in the host's byte order, as the kernel prints it.
    fn proc_table() -> String {
        let loopback = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
        TCP_TABLE.replace("0100007F", &loopback)
    }

    #[test]
    fn a_listener_is_held_when_one_of_the_childs_sockets_is_the_loopback_listener() {
        assert!(listener_held(
            &proc_table(),
            &links(&["5", "111"]),
            port(8080)
        ));
        assert!(listener_held(&proc_table(), &links(&["222"]), port(8081)));
    }

    /// The foreign process that took the port first: the child holds no listener on it.
    #[test]
    fn a_listener_of_another_process_is_not_held() {
        assert!(!listener_held(
            &proc_table(),
            &links(&["5", "6"]),
            port(8080)
        ));
        assert!(!listener_held(&proc_table(), &[], port(8080)));
        // the child's own listener on another port does not vouch for this one
        assert!(!listener_held(&proc_table(), &links(&["222"]), port(8080)));
    }

    /// Only a socket in LISTEN state on 127.0.0.1 counts: a connection of the child to the port
    /// (state 01) and a listener on another address are not the forward.
    #[test]
    fn only_a_listening_loopback_socket_counts() {
        let connected = listener_held(&proc_table(), &links(&["333"]), port(8080));
        assert!(!connected, "a connected socket is not a listener");
        let wildcard = listener_held(&proc_table(), &links(&["444"]), port(0x1F92));
        assert!(!wildcard, "a wildcard listener is not the loopback one");
    }

    #[test]
    fn what_cannot_be_read_is_not_held() {
        assert!(!listener_held("", &links(&["111"]), port(8080)));
        assert!(!listener_held(
            "garbage\nmore",
            &links(&["111"]),
            port(8080)
        ));
        // a link that is not exactly `socket:[digits]` names no socket
        for link in [
            "socket:[111",
            "socket:[+111]",
            "socket:[]",
            "pipe:[111]",
            "socket:[111]x",
        ] {
            let held = listener_held(&proc_table(), &[link.to_string()], port(8080));
            assert!(!held, "{link}");
        }
    }

    fn argv_of(endpoint: &Endpoint) -> Vec<String> {
        let host = Host::parse("cluster").unwrap();
        let target = ForwardTarget::parse("gpu-01:5000").unwrap();
        let argv = argv(&host, endpoint, &target);
        assert_eq!(argv.program(), "ssh");
        argv.args().to_vec()
    }

    /// The whole command line: no wildcard bind, no shell, `--` before the host.
    #[test]
    fn the_argv_binds_loopback_only_and_ends_options_before_the_host() {
        let expected = [
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
        assert_eq!(argv_of(&Endpoint::Tcp(port(5051))), expected);
        let name = ForwardName::parse("m").unwrap();
        let socket = Endpoint::Unix(SocketPath::new("/run/user/1", &name).unwrap());
        let words = argv_of(&socket);
        assert_eq!(words[14], "/run/user/1/rr-m.sock:gpu-01:5000");
        assert_eq!(&words[15..], ["--", "cluster"]);
    }

    #[test]
    fn the_url_is_plain_http_on_loopback_at_the_forwards_port() {
        assert_eq!(
            OpenUrl::of_forward(port(5051)).as_str(),
            "http://127.0.0.1:5051/"
        );
        assert_eq!(OpenUrl::of_forward(port(1)).as_str(), "http://127.0.0.1:1/");
    }

    fn table(local: Option<u16>) -> ForwardTable {
        let spec = ForwardSpec {
            target: ForwardTarget::parse("gpu-01:5000").unwrap(),
            local: local.map(port),
        };
        [(ForwardName::parse("mlflow").unwrap(), spec)]
            .into_iter()
            .collect()
    }

    fn plan_of(
        table: &ForwardTable,
        (wanted, local): (&str, Option<u16>),
        (transport, browser): (Transport, Browser),
    ) -> Result<Plan> {
        table.plan(
            Wanted::parse(wanted).unwrap(),
            local.map(port),
            transport,
            browser,
        )
    }

    #[test]
    fn a_socket_has_no_port_and_no_browser() {
        let tcp = (Transport::Tcp, Browser::Open);
        let socket = (Transport::Socket, Browser::Leave);
        let t = table(None);
        let plan = plan_of(&t, ("mlflow", Some(7000)), tcp).unwrap();
        let (local, browser) = (Some(port(7000)), Browser::Open);
        assert_eq!(plan.listener, Listener::Tcp { local, browser });
        assert_eq!(
            plan_of(&t, ("mlflow", None), socket).unwrap().listener,
            Listener::Socket
        );
        assert_eq!(
            plan_of(&t, ("h:1", None), socket).unwrap().name.as_str(),
            "1"
        );
        let refused = [
            plan_of(&t, ("mlflow", Some(7000)), socket),
            plan_of(&t, ("mlflow", None), (Transport::Socket, Browser::Open)),
            // a local port in the config is a port too
            plan_of(&table(Some(6000)), ("mlflow", None), socket),
        ];
        for result in refused {
            assert!(result.is_err());
        }
    }

    #[test]
    fn the_opener_gets_the_url_as_one_argument() {
        let url = OpenUrl::of_forward(port(5051));
        let opener = url.opener();
        assert_eq!(opener.args(), ["http://127.0.0.1:5051/"]);
        assert!(["xdg-open", "open", "explorer"].contains(&opener.program()));
    }
}
