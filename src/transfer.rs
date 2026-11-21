//! rsync between the desktop and the remote project, for small files only.

use crate::remote::SSH_OPTIONS;
use anyhow::{Context, Result, bail};

/// A remote path: absolute as given, relative ones inside the remote project directory.
pub fn remote_path(dir: Option<&str>, path: &str) -> String {
    match dir {
        Some(dir) if !path.starts_with('/') => {
            format!("{}/{path}", dir.trim_end_matches('/'))
        }
        _ => path.to_string(),
    }
}

/// The rsync argv; `probe` adds `--dry-run --stats`, which measures without copying (pure).
pub fn rsync_argv(source: &str, destination: &str, probe: bool) -> Vec<String> {
    let mut argv: Vec<String> = ["rsync", "-az", "--partial"].map(String::from).to_vec();
    argv.push("-e".into());
    argv.push(format!("ssh {}", SSH_OPTIONS.join(" ")));
    if probe {
        argv.extend(["--dry-run", "--stats"].map(String::from));
    }
    argv.extend([source.to_string(), destination.to_string()]);
    argv
}

/// Bytes rsync would copy, from its `--stats` output (pure).
pub fn parse_total(stats: &str) -> Result<u64> {
    let line = stats
        .lines()
        .find_map(|line| line.strip_prefix("Total transferred file size:"))
        .context("rsync --stats printed no transferred size")?;
    let digits: String = line
        .chars()
        .take_while(|c| !c.is_alphabetic())
        .filter(char::is_ascii_digit)
        .collect();
    digits
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

    #[test]
    fn relative_remote_paths_live_in_the_project() {
        assert_eq!(
            remote_path(Some("projects/proj/"), "bin/x"),
            "projects/proj/bin/x"
        );
        assert_eq!(remote_path(Some("projects/proj"), "/abs/x"), "/abs/x");
        assert_eq!(remote_path(None, "x"), "x");
    }

    #[test]
    fn reads_the_transferred_size() {
        let stats = "Number of files: 2\nTotal file size: 9 bytes\nTotal transferred file size: 123,456 bytes\n";
        assert_eq!(parse_total(stats).unwrap(), 123_456);
        assert!(parse_total("Total file size: 9 bytes").is_err());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("200K").unwrap(), 200 << 10);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert!(parse_size("1T").is_err());
        assert!(parse_size("G").is_err());
    }

    #[test]
    fn a_probe_copies_nothing() {
        let argv = rsync_argv("a", "h:b", true);
        assert!(argv.contains(&"--dry-run".to_string()));
        assert!(!argv.contains(&"--delete".to_string()));
    }
}
