//! Settings from files, under flags and environment variables.
//!
//! Precedence, highest first: `--flag`, `RR_*` variable, the nearest `.rr.toml` from the current
//! directory up, the user file (`$XDG_CONFIG_HOME/rr/config.toml`, else `~/.config/rr/config.toml`).
//! The user file suits what belongs to you on a cluster (the host alias, the shell file that puts
//! `uv` on PATH); `.rr.toml` suits what belongs to a project (its remote directory).

use crate::domain::host::Host;
use crate::domain::ids::{Digest, digest};
use crate::domain::place::{EnvFile, Place, RemoteDir};
use crate::trust::{TrustStore, store_file};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::ffi::OsString;
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

/// What `rr` needs from the process, read once in `main` and passed down as a value so that
/// nothing below it touches the real environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    pub cwd: PathBuf,
    pub xdg_config_home: Option<String>,
    pub xdg_state_home: Option<String>,
    pub home: Option<String>,
    pub appdata: Option<String>,
}

impl Environment {
    /// The user file's path (see [`user_file`]).
    pub fn user_file(&self) -> Option<PathBuf> {
        user_file(
            self.xdg_config_home.as_deref(),
            self.home.as_deref(),
            self.appdata.as_deref(),
        )
    }

    /// The trust store's path (see [`store_file`]).
    pub fn trust_file(&self) -> Option<PathBuf> {
        store_file(
            self.xdg_state_home.as_deref(),
            self.home.as_deref(),
            self.appdata.as_deref(),
        )
    }
}

/// The value of environment variable `name` as text. A value that is not UTF-8 is an error that
/// names the variable: treating it as unset would silently fall back to another location (pure).
pub fn utf8_var(name: &str, value: Option<OsString>) -> Result<Option<String>> {
    value
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow!("environment variable {name} is not valid UTF-8"))
        })
        .transpose()
}

/// Everything a command needs to reach the remote: a valid host and where to run. Code below
/// `main` never sees a missing host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub host: Host,
    pub place: Place,
}

/// Validate the merged settings once, at the boundary (pure).
pub fn resolve(settings: Settings) -> Result<Session> {
    let Some(host) = settings.host else {
        bail!(
            "no host: pass --host, set RR_HOST, or set host in .rr.toml or ~/.config/rr/config.toml"
        );
    };
    Ok(Session {
        host: Host::parse(&host)?,
        place: Place {
            env: settings.env.as_deref().map(EnvFile::parse).transpose()?,
            dir: settings.dir.as_deref().map(RemoteDir::parse).transpose()?,
        },
    })
}

/// Read one file; a missing file is empty settings, a malformed one an error naming the file.
#[allow(clippy::disallowed_methods)] // file I/O is this function's job
pub fn read(path: &Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn parse(text: &str, path: &Path) -> Result<Settings> {
    toml::from_str(text).with_context(|| format!("in {}", path.display()))
}

/// A project file as read once from disk. Its content is not exposed: the settings come out only
/// through [`ProjectFile::trusted`], and what the user reviews through [`ProjectFile::review`].
/// The digest that is trusted and the text that is parsed come from the same bytes, so the file
/// cannot change in between.
///
/// ```compile_fail,E0616
/// fn peek(file: &remote_run::config::ProjectFile) -> &[u8] {
///     &file.bytes
/// }
/// ```
pub struct ProjectFile {
    path: PathBuf,
    bytes: Vec<u8>,
}

/// What `rr trust` shows and records: the file's canonical path, the digest of what was read, and
/// that text (lossily decoded; it is only displayed).
pub struct Review<'a> {
    pub path: &'a Path,
    pub digest: Digest,
    pub text: String,
}

/// Settings from a project file that [`ProjectFile::trusted`] accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedProject(Settings);

impl TrustedProject {
    pub fn into_settings(self) -> Settings {
        self.0
    }
}

impl ProjectFile {
    pub fn review(&self) -> Review<'_> {
        Review {
            path: &self.path,
            digest: digest(&self.bytes),
            text: String::from_utf8_lossy(&self.bytes).into_owned(),
        }
    }

    /// The file's settings, if `store` holds this exact content; otherwise the refusal that says
    /// how to proceed. Nothing of an untrusted file is parsed.
    pub fn trusted(self, store: &TrustStore) -> Result<TrustedProject> {
        store.require(&self.path, &digest(&self.bytes))?;
        let text = std::str::from_utf8(&self.bytes)
            .with_context(|| format!("{} is not UTF-8", self.path.display()))?;
        parse(text, &self.path).map(TrustedProject)
    }
}

/// The nearest project file at or above `start`, read.
#[allow(clippy::disallowed_methods)] // file I/O is this function's job
pub fn read_project(start: &Path) -> Result<Option<ProjectFile>> {
    let Some(found) = project_file(start)? else {
        return Ok(None);
    };
    let path = found
        .canonicalize()
        .with_context(|| format!("resolving {}", found.display()))?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(ProjectFile { path, bytes }))
}

/// The nearest project file at or above `start`. Anything at a candidate path counts as found,
/// including a dangling symlink, and any error other than "does not exist" (a directory that
/// cannot be searched) is returned: walking on past either would apply a file further up that the
/// user did not mean, or none.
pub fn project_file(start: &Path) -> Result<Option<PathBuf>> {
    for dir in start.ancestors() {
        let candidate = dir.join(PROJECT_FILE);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(Some(candidate)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("looking for {}", candidate.display()));
            }
        }
    }
    Ok(None)
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

/// The project file's settings over the user file's. `read_store` runs only when a project file
/// exists, so a damaged trust store cannot break `rr` where no project file applies; with one, it
/// must read. A project file the user has not reviewed is refused, whatever it contains.
pub fn load(
    start: &Path,
    user: Option<&Path>,
    read_store: impl FnOnce() -> Result<TrustStore>,
) -> Result<Settings> {
    let project = match read_project(start)? {
        None => Settings::default(),
        Some(file) => file.trusted(&read_store()?)?.into_settings(),
    };
    let user = user.map(read).transpose()?.unwrap_or_default();
    Ok(project.over(user))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build real files
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
            "dir = \"projects/proj\"\nhost = \"b\"\n",
        )
        .unwrap();
        let user = root.path().join("user.toml");
        fs::write(&user, "host = \"a\"\nenv = \"hpc/env.sh\"\n").unwrap();
        let store = trusting(&root.path().join("repo"));
        let loaded = load(&nested, Some(&user), || Ok(store)).unwrap();
        assert_eq!(
            loaded,
            settings(Some("b"), Some("projects/proj"), Some("hpc/env.sh"))
        );
    }

    /// A store that trusts the project file in `dir` as it is now.
    fn trusting(dir: &Path) -> TrustStore {
        let file = read_project(dir).unwrap().expect("a project file");
        let review = file.review();
        TrustStore::default()
            .with(review.path, &review.digest)
            .unwrap()
    }

    #[test]
    fn an_unreviewed_or_changed_project_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(PROJECT_FILE);
        fs::write(&path, "host = \"evil\"\n").unwrap();
        let refusal = load(root.path(), None, || Ok(TrustStore::default())).unwrap_err();
        assert!(refusal.to_string().contains("not trusted"), "{refusal}");
        let store = trusting(root.path());
        assert!(load(root.path(), None, || Ok(store.clone())).is_ok());
        fs::write(&path, "host = \"evil\"\ndir = \"x;touch /tmp/p\"\n").unwrap();
        assert!(load(root.path(), None, || Ok(store)).is_err());
    }

    #[test]
    fn resolve_needs_a_host_and_validates_every_value() {
        let err = resolve(settings(None, Some("d"), None)).unwrap_err();
        assert!(err.to_string().starts_with("no host"), "{err}");
        assert!(resolve(settings(Some("-oProxyCommand=x"), None, None)).is_err());
        assert!(resolve(settings(Some("h"), Some(""), None)).is_err());
        assert!(resolve(settings(Some("h"), None, Some("a\nb"))).is_err());
        let session = resolve(settings(Some("h"), Some("proj"), Some("e.sh"))).unwrap();
        assert_eq!(session.host.as_str(), "h");
        assert_eq!(session.place.dir.unwrap().as_str(), "proj");
        assert_eq!(session.place.env.unwrap().as_str(), "e.sh");
        assert_eq!(
            resolve(settings(Some("h"), None, None)).unwrap().place,
            Place::default()
        );
    }

    #[test]
    fn the_environment_locates_the_user_file_and_the_trust_store() {
        let env = Environment {
            xdg_config_home: Some("/c".into()),
            xdg_state_home: Some("/s".into()),
            ..Environment::default()
        };
        assert_eq!(env.user_file(), Some(PathBuf::from("/c/rr/config.toml")));
        assert_eq!(env.trust_file(), Some(PathBuf::from("/s/rr/trusted")));
        assert_eq!(Environment::default().user_file(), None);
    }

    #[test]
    fn an_environment_variable_that_is_not_utf8_is_an_error_naming_it() {
        assert_eq!(utf8_var("HOME", None).unwrap(), None);
        assert_eq!(
            utf8_var("HOME", Some("/h".into())).unwrap().as_deref(),
            Some("/h")
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = OsString::from_vec(vec![b'/', 0xff]);
            let err = utf8_var("XDG_STATE_HOME", Some(bad)).unwrap_err();
            assert!(err.to_string().contains("XDG_STATE_HOME"), "{err}");
        }
    }

    #[test]
    fn an_untrusted_project_file_is_never_parsed() {
        let root = tempfile::tempdir().unwrap();
        // not even valid TOML: the refusal comes first, so nothing of the text is interpreted
        fs::write(root.path().join(PROJECT_FILE), "host = = =\n").unwrap();
        let file = read_project(root.path()).unwrap().unwrap();
        let err = file.trusted(&TrustStore::default()).unwrap_err();
        assert!(err.to_string().contains("not trusted"), "{err}");
    }

    #[test]
    fn a_trusted_file_yields_its_settings_and_review_shows_what_was_hashed() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(PROJECT_FILE), "host = \"b\"\n").unwrap();
        let store = trusting(root.path());
        let file = read_project(root.path()).unwrap().unwrap();
        let review = file.review();
        assert_eq!(review.text, "host = \"b\"\n");
        assert_eq!(review.digest, digest(b"host = \"b\"\n"));
        let settings = file.trusted(&store).unwrap().into_settings();
        assert_eq!(settings, self::settings(Some("b"), None, None));
    }

    #[test]
    fn the_trust_store_is_read_only_when_a_project_file_exists() {
        let root = tempfile::tempdir().unwrap();
        let broken = || -> Result<TrustStore> { bail!("the store is corrupt") };
        assert_eq!(
            load(root.path(), None, broken).unwrap(),
            Settings::default()
        );
        fs::write(root.path().join(PROJECT_FILE), "host = \"b\"\n").unwrap();
        let err = load(root.path(), None, broken).unwrap_err();
        assert!(err.to_string().contains("the store is corrupt"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_project_file_is_an_error_not_a_reason_to_look_further_up() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(PROJECT_FILE), "host = \"outer\"\n").unwrap();
        let inner = root.path().join("inner");
        fs::create_dir(&inner).unwrap();
        std::os::unix::fs::symlink("/nonexistent/rr.toml", inner.join(PROJECT_FILE)).unwrap();
        let err = format!("{:#}", read_project(&inner).err().expect("an error"));
        assert!(err.contains(PROJECT_FILE), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_searched_is_an_error_not_a_reason_to_look_further_up() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(PROJECT_FILE), "host = \"outer\"\n").unwrap();
        let locked = root.path().join("locked");
        let start = locked.join("work");
        fs::create_dir_all(&start).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // root can search anything, so there is nothing to see in that case
        let bypassed = fs::read_dir(&locked).is_ok();
        let result = project_file(&start);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !bypassed {
            assert!(result.is_err(), "{result:?}");
        }
    }

    #[test]
    fn missing_files_are_empty() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("none.toml");
        assert_eq!(
            load(root.path(), Some(&missing), || Ok(TrustStore::default())).unwrap(),
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
