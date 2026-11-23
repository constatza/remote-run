//! Where a remote command runs, and the script that gets it there.

use crate::domain::shell::{Script, lit, quote};
use anyhow::{Result, bail};
use std::fmt;

/// A directory on the remote host, relative to the remote home or absolute.
///
/// Refused: empty, NUL and newline (they would break the one-line `cd '<dir>'` it ends up in) and
/// a leading `-` (a remote rsync or `cd` reads it as an option). A leading `~/` (or `~`) means the
/// remote home and is stored as the relative path it stands for, because neither a quoted `cd` nor
/// `rsync -s` expands `~`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDir(String);

/// A path on the remote host as rsync receives it: absolute, or starting with `./` (relative to
/// the remote home), so the remote side can never read it as an option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePath(String);

/// A shell file on the remote host that every command sources first; same rules as [`RemoteDir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvFile(String);

fn check_remote_text(what: &str, text: &str) -> Result<()> {
    if text.is_empty() {
        bail!("{what} is empty");
    }
    if text.contains(['\0', '\n']) {
        bail!("{what} {text:?} has a NUL or a newline");
    }
    Ok(())
}

/// What follows a leading `~/` (`~` alone is the home itself).
fn home_relative(path: &str) -> Option<&str> {
    if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/")
    }
}

impl RemoteDir {
    pub fn parse(text: &str) -> Result<Self> {
        check_remote_text("the remote directory", text)?;
        let text = match home_relative(text) {
            Some("") => ".",
            Some(rest) => rest,
            None => text,
        };
        if text.starts_with('-') {
            bail!("the remote directory {text:?} starts with '-', which reads as an option");
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `path` inside this directory; an absolute `path` or one starting `~/` ignores it.
    pub fn join(&self, path: &str) -> RemotePath {
        if path.starts_with('/') || home_relative(path).is_some() {
            return RemotePath::in_home(path);
        }
        RemotePath::in_home(&format!("{}/{path}", self.0.trim_end_matches('/')))
    }
}

impl RemotePath {
    /// `path` as the user typed it: absolute as given, `~/x` and relative paths inside the remote
    /// home.
    pub fn in_home(path: &str) -> Self {
        if path.starts_with('/') {
            return Self(path.to_string());
        }
        match home_relative(path).unwrap_or(path) {
            "" | "." => Self("./".into()),
            rest if rest.starts_with("./") => Self(rest.to_string()),
            rest => Self(format!("./{rest}")),
        }
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl EnvFile {
    pub fn parse(text: &str) -> Result<Self> {
        check_remote_text("the remote env file", text)?;
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where a remote command runs: an optional shell file sourced first, then an optional directory.
/// Relative paths are relative to the remote home, where `ssh` starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub env: Option<EnvFile>,
    pub dir: Option<RemoteDir>,
}

impl Place {
    /// The same environment file, but the remote home instead of the project directory.
    pub fn at_home(&self) -> Place {
        Place {
            env: self.env.clone(),
            dir: None,
        }
    }

    /// `path` as the user typed it, resolved against the project directory (or the home).
    pub fn remote_path(&self, path: &str) -> RemotePath {
        match &self.dir {
            Some(dir) => dir.join(path),
            None => RemotePath::in_home(path),
        }
    }
}

/// The bash script for `body` at `place`: strict mode, then `source`, then `cd`. `body` is a
/// [`Script`], so only quoted words reach the remote shell.
///
/// ```compile_fail,E0308
/// use remote_run::domain::place::{Place, script};
/// let _ = script(&Place::default(), "rm -rf $HOME");
/// ```
pub fn script(place: &Place, body: Script) -> Script {
    let mut lines = vec![lit("set -euo pipefail")];
    if let Some(env) = &place.env {
        lines.push(lit("source ").word(quote(env.as_str())));
    }
    if let Some(dir) = &place.dir {
        lines.push(lit("cd ").word(quote(dir.as_str())));
    }
    lines.push(body);
    Script::lines(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_sources_then_changes_directory() {
        let place = Place {
            env: Some(EnvFile::parse("hpc/env.sh").unwrap()),
            dir: Some(RemoteDir::parse("my dir").unwrap()),
        };
        assert_eq!(
            script(&place, lit("true")).to_string(),
            "set -euo pipefail\nsource 'hpc/env.sh'\ncd 'my dir'\ntrue"
        );
        let bare = script(&Place::default(), lit("true"));
        assert_eq!(bare.to_string(), "set -euo pipefail\ntrue");
    }

    #[test]
    fn at_home_keeps_the_env_file_and_drops_the_directory() {
        let place = Place {
            env: Some(EnvFile::parse("e.sh").unwrap()),
            dir: Some(RemoteDir::parse("d").unwrap()),
        };
        let home = place.at_home();
        assert_eq!(home.env, place.env);
        assert_eq!(home.dir, None);
    }

    #[test]
    fn relative_and_absolute_paths_both_parse() {
        for text in ["proj", "/abs/proj", "a b", "./x/", "a~b"] {
            assert_eq!(RemoteDir::parse(text).unwrap().as_str(), text);
            assert_eq!(EnvFile::parse(text).unwrap().as_str(), text);
        }
        assert_eq!(EnvFile::parse("~/x").unwrap().as_str(), "~/x");
        assert_eq!(EnvFile::parse("-lead").unwrap().as_str(), "-lead");
    }

    #[test]
    fn what_breaks_the_script_line_is_refused() {
        for text in ["", "a\nb", "a\0b", "\n"] {
            assert!(RemoteDir::parse(text).is_err(), "{text:?}");
            assert!(EnvFile::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_directory_never_starts_with_a_dash() {
        for text in ["-rf", "-oProxyCommand=x", "~/-x"] {
            assert!(RemoteDir::parse(text).is_err(), "{text:?}");
        }
        assert_eq!(RemoteDir::parse("a/-b").unwrap().as_str(), "a/-b");
    }

    #[test]
    fn a_leading_tilde_slash_is_the_remote_home() {
        for (text, stored) in [
            ("~/proj", "proj"),
            ("~", "."),
            ("~/", "."),
            ("~/a/b", "a/b"),
        ] {
            assert_eq!(RemoteDir::parse(text).unwrap().as_str(), stored, "{text}");
        }
        assert_eq!(RemoteDir::parse("~user/x").unwrap().as_str(), "~user/x");
    }

    #[test]
    fn join_is_absolute_or_dot_slash_and_never_a_dash() {
        let dir = |text: &str| RemoteDir::parse(text).unwrap();
        let cases = [
            ("projects/proj/", "bin/x", "./projects/proj/bin/x"),
            ("projects/proj", "/abs/x", "/abs/x"),
            ("/work/proj", "bin/x", "/work/proj/bin/x"),
            ("/work/proj/", "", "/work/proj/"),
            ("proj", "", "./proj/"),
            ("proj", "-rf", "./proj/-rf"),
            (".", "-rf", "./-rf"),
            ("./a", "b", "./a/b"),
            ("/", "x", "/x"),
            ("proj", "~/x", "./x"),
            ("proj", "~", "./"),
            ("/work", "~/-x", "./-x"),
        ];
        for (base, path, expected) in cases {
            assert_eq!(
                dir(base).join(path).to_string(),
                expected,
                "{base} + {path}"
            );
        }
    }

    #[test]
    fn a_path_without_a_directory_is_absolute_or_in_the_home() {
        for (path, expected) in [
            ("x", "./x"),
            ("-rf", "./-rf"),
            ("/abs", "/abs"),
            ("~/x", "./x"),
            ("~", "./"),
            ("./y", "./y"),
            (".", "./"),
        ] {
            assert_eq!(RemotePath::in_home(path).to_string(), expected, "{path}");
        }
    }
}
