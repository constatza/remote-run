//! Settings from files, under flags and environment variables.
//!
//! Precedence, highest first: `--flag`, `RR_*` variable, the nearest `.rr.toml` from the current
//! directory up, the user file (`$XDG_CONFIG_HOME/rr/config.toml`, else `~/.config/rr/config.toml`).
//! The user file suits what belongs to you on a cluster (the host alias, the shell file that puts
//! `uv` on PATH); `.rr.toml` suits what belongs to a project (its remote directory).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The project file's name, looked up from the current directory to the root.
pub const PROJECT_FILE: &str = ".rr.toml";

/// One file's settings; every key optional, unknown keys refused.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// ssh host or alias.
    pub host: Option<String>,
    /// Remote project directory, relative to the remote home or absolute.
    pub dir: Option<String>,
    /// Remote shell file sourced before every command.
    pub env: Option<String>,
}

impl Settings {
    /// `self` where set, else `lower` (pure).
    pub fn over(self, lower: Settings) -> Settings {
        Settings {
            host: self.host.or(lower.host),
            dir: self.dir.or(lower.dir),
            env: self.env.or(lower.env),
        }
    }
}

/// Read one file; a missing file is empty settings, a malformed one an error naming the file.
pub fn read(path: &Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("in {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// The nearest project file at or above `start`.
pub fn project_file(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(PROJECT_FILE))
        .find(|path| path.is_file())
}

/// The user file's path from the variables that locate a config directory (pure).
pub fn user_file(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
    appdata: Option<&str>,
) -> Option<PathBuf> {
    let base = match (xdg_config_home, home, appdata) {
        (Some(xdg), _, _) if !xdg.is_empty() => PathBuf::from(xdg),
        (_, Some(home), _) if !home.is_empty() => Path::new(home).join(".config"),
        (_, _, Some(appdata)) if !appdata.is_empty() => PathBuf::from(appdata),
        _ => return None,
    };
    Some(base.join("rr").join("config.toml"))
}

/// The project file's settings over the user file's.
pub fn load(start: &Path, user: Option<&Path>) -> Result<Settings> {
    let project = project_file(start)
        .map(|path| read(&path))
        .transpose()?
        .unwrap_or_default();
    let user = user.map(read).transpose()?.unwrap_or_default();
    Ok(project.over(user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn settings(host: Option<&str>, dir: Option<&str>, env: Option<&str>) -> Settings {
        Settings {
            host: host.map(Into::into),
            dir: dir.map(Into::into),
            env: env.map(Into::into),
        }
    }

    #[test]
    fn project_file_wins_key_by_key_over_the_user_file() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("repo/src/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.path().join("repo").join(PROJECT_FILE),
            "dir = \"projects/piaif\"\nhost = \"b\"\n",
        )
        .unwrap();
        let user = root.path().join("user.toml");
        fs::write(&user, "host = \"a\"\nenv = \"hpc/env.sh\"\n").unwrap();
        let loaded = load(&nested, Some(&user)).unwrap();
        assert_eq!(
            loaded,
            settings(Some("b"), Some("projects/piaif"), Some("hpc/env.sh"))
        );
    }

    #[test]
    fn missing_files_are_empty() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("none.toml");
        assert_eq!(
            load(root.path(), Some(&missing)).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn a_misspelt_key_is_refused_with_the_file_named() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(PROJECT_FILE);
        fs::write(&path, "hots = \"a\"\n").unwrap();
        let err = format!("{:#}", read(&path).unwrap_err());
        assert!(err.contains(&path.display().to_string()), "{err}");
    }

    #[test]
    fn user_file_location() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            user_file(Some("/x"), Some("/h"), None),
            p("/x/rr/config.toml")
        );
        assert_eq!(
            user_file(Some(""), Some("/h"), None),
            p("/h/.config/rr/config.toml")
        );
        assert_eq!(
            user_file(None, None, Some("C:/A")),
            p("C:/A/rr/config.toml")
        );
        assert_eq!(user_file(None, None, None), None);
    }
}
