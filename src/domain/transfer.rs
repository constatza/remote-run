//! rsync between the desktop and the remote project, for small files only.

use crate::domain::argv::Argv;
use crate::domain::exit::{Exit, RrCode};
use crate::domain::host::Host;
use crate::domain::place::RemotePath;
use crate::domain::shell::{lit, quote};
use crate::domain::ssh::SSH_OPTIONS;
use anyhow::{Context, Result, bail};
use std::fmt;
use std::path::PathBuf;

/// One side of a copy. Display is the only code that writes rsync's `host:path` form, so a local
/// name can never be read as a remote one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Local(PathBuf),
    Remote(Host, RemotePath),
}

impl Endpoint {
    /// A local path. rsync reads `name:rest` as a remote host (and `name::mod` as a daemon address
    /// over TCP), so a colon before the first `/` gets `./` in front and stays a file name.
    pub fn local(path: &str) -> Self {
        let before_slash = path.split('/').next().unwrap_or_default();
        if before_slash.contains(':') {
            Self::Local(PathBuf::from(format!("./{path}")))
        } else {
            Self::Local(PathBuf::from(path))
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Local(path) => write!(f, "{}", path.display()),
            Endpoint::Remote(host, path) => write!(f, "{host}:{path}"),
        }
    }
}

/// Which of the two rsync runs a copy makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// `--dry-run --stats`: measures what would be copied, copies nothing.
    Measure,
    /// The copy itself.
    Copy,
}

/// The rsync argv for one pass (pure).
pub fn rsync_argv(source: &Endpoint, destination: &Endpoint, pass: Pass) -> Argv {
    // -s sends the remote path as data instead of through the remote shell (older rsync versions)
    let mut args: Vec<String> = ["-azs", "--partial"].map(String::from).to_vec();
    args.push("-e".into());
    // only the base options: a copy may be quiet for long, so the user's own ssh configuration decides
    // when a silent link is given up (the liveness options are for polls, see `domain::ssh`)
    args.push(lit("ssh ").words(SSH_OPTIONS.map(quote)).to_string());
    match pass {
        Pass::Measure => args.extend(["--dry-run", "--stats"].map(String::from)),
        Pass::Copy => {}
    }
    // `--` so a path that starts with `-` is a path, not an option
    args.extend([
        "--".to_string(),
        source.to_string(),
        destination.to_string(),
    ]);
    Argv::new("rsync", args)
}

/// How a copy ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// rsync ran and exited with this code.
    Ran(i32),
    /// The probe measured more than the limit; nothing was copied.
    OverLimit { total: u64, limit: u64 },
}

impl Transfer {
    /// How the process exits: with rsync's own code, or refused.
    pub fn exit(&self) -> Exit {
        match self {
            Transfer::Ran(code) => Exit::remote(*code),
            Transfer::OverLimit { .. } => Exit::Rr(RrCode::Refused),
        }
    }
}

impl fmt::Display for Transfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transfer::Ran(code) => write!(f, "rsync exited with {code}"),
            Transfer::OverLimit { total, limit } => write!(
                f,
                "refused: {total} bytes to copy, over the limit of {limit} (--limit)"
            ),
        }
    }
}

/// Bytes rsync would copy, from its `--stats` output (pure).
pub fn parse_total(stats: &str) -> Result<u64> {
    let line = stats
        .lines()
        .find_map(|line| line.strip_prefix("Total transferred file size:"))
        .context("rsync --stats printed no transferred size")?;
    // rsync prints `123,456 bytes`; anything else (`1.5M`, a locale's separators) is not a count
    let number = line.trim().split(' ').next().unwrap_or_default();
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        bail!("unreadable size: {line:?}");
    }
    number
        .replace(',', "")
        .parse()
        .with_context(|| format!("unreadable size: {line:?}"))
}

/// A size such as `512`, `200K`, `50M` or `1G` (binary units) (pure).
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let shift = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KIB" => 10,
        "M" | "MIB" => 20,
        "G" | "GIB" => 30,
        _ => bail!("unknown size unit in {text:?} (use K, M or G)"),
    };
    let number: u64 = number
        .parse()
        .with_context(|| format!("not a size: {text:?}"))?;
    number
        .checked_shl(shift)
        .filter(|n| n >> shift == number)
        .context("size too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(path: &str) -> Endpoint {
        Endpoint::Remote(Host::parse("h").unwrap(), RemotePath::in_home(path))
    }

    #[test]
    fn a_remote_endpoint_is_written_as_host_colon_path() {
        assert_eq!(remote("x").to_string(), "h:./x");
        assert_eq!(remote("/abs/x").to_string(), "h:/abs/x");
        let user = Host::parse("me@cluster").unwrap();
        let endpoint = Endpoint::Remote(user, RemotePath::in_home("-rf"));
        assert_eq!(endpoint.to_string(), "me@cluster:./-rf");
    }

    #[test]
    fn a_local_path_is_never_read_as_a_remote_one() {
        for (given, shown) in [
            ("evil::mod", "./evil::mod"),
            ("evil:dir", "./evil:dir"),
            ("a:b/c", "./a:b/c"),
            ("rsync://host/mod", "./rsync://host/mod"),
            (":x", "./:x"),
            ("-x", "-x"),
            ("/abs:colon", "/abs:colon"),
            ("dir/a:b", "dir/a:b"),
            ("./a:b", "./a:b"),
            ("plain", "plain"),
            (".", "."),
        ] {
            assert_eq!(Endpoint::local(given).to_string(), shown, "{given}");
        }
    }

    #[test]
    fn the_copy_is_one_exact_rsync_argv() {
        let argv = rsync_argv(&Endpoint::local("evil::mod"), &remote("-x"), Pass::Copy);
        assert_eq!(argv.program(), "rsync");
        assert_eq!(
            argv.args(),
            [
                "-azs",
                "--partial",
                "-e",
                "ssh '-o' 'BatchMode=yes' '-o' 'LogLevel=ERROR'",
                "--",
                "./evil::mod",
                "h:./-x",
            ]
        );
    }

    #[test]
    fn a_probe_measures_and_never_deletes() {
        let argv = rsync_argv(&Endpoint::local("a"), &remote("b"), Pass::Measure);
        assert_eq!(
            argv.args()[4..],
            ["--dry-run", "--stats", "--", "a", "h:./b"]
        );
        assert!(argv.args().iter().all(|a| !a.starts_with("--delete")));
    }

    #[test]
    fn a_dash_path_sits_after_the_double_dash() {
        let argv = rsync_argv(&Endpoint::local("-x"), &remote("y"), Pass::Copy);
        assert_eq!(argv.args()[4..], ["--", "-x", "h:./y"]);
    }

    #[test]
    fn reads_the_transferred_size() {
        let stats = "Number of files: 2\nTotal file size: 9 bytes\nTotal transferred file size: 123,456 bytes\n";
        assert_eq!(parse_total(stats).unwrap(), 123_456);
        assert!(parse_total("Total file size: 9 bytes").is_err());
        assert_eq!(
            parse_total("Total transferred file size: 0 bytes").unwrap(),
            0
        );
        assert_eq!(
            parse_total("Total transferred file size: 1,234 bytes").unwrap(),
            1234
        );
        for bad in [
            "1.5M bytes",
            "1.5 bytes",
            "1e3 bytes",
            "bytes",
            "",
            "-5 bytes",
            "1,2.3 bytes",
            "12K",
        ] {
            let stats = format!("Total transferred file size: {bad}\n");
            assert!(parse_total(&stats).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("200K").unwrap(), 200 << 10);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert!(parse_size("1T").is_err());
        assert!(parse_size("G").is_err());
    }
}
