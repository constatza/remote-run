//! Which project files (`.rr.toml`) the user has reviewed.
//!
//! A project file can name the host, the remote directory, a shell file the remote sources and the
//! commands of recipes, so
//! a file from a cloned repository must not apply on its own. `rr trust` records the SHA-256 of the
//! reviewed bytes next to the file's canonical path; any change makes the file untrusted again.

use crate::domain::ids::Digest;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What `rr trust` was asked to do with the project file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustAction {
    /// Only show the file and its digest.
    Show,
    /// Record the digest as reviewed.
    Record,
}

/// The reviewed files: canonical path to the digest that was reviewed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustStore(BTreeMap<PathBuf, Digest>);

impl TrustStore {
    /// Parse the store's text: one `<digest> <path>` per line; malformed lines are an error, so a
    /// damaged store never silently trusts less or more than it says.
    pub fn parse(text: &str) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let (hash, path) = line
                .split_once(' ')
                .with_context(|| format!("malformed trust entry: {line:?}"))?;
            let hash =
                Digest::parse(hash).with_context(|| format!("malformed trust entry: {line:?}"))?;
            entries.insert(PathBuf::from(path), hash);
        }
        Ok(Self(entries))
    }

    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(|(path, hash)| format!("{hash} {}\n", path.display()))
            .collect()
    }

    /// `path` holds exactly the bytes whose digest is `hash` (pure).
    pub fn trusts(&self, path: &Path, hash: &Digest) -> bool {
        self.0.get(path) == Some(hash)
    }

    /// The store with `path` trusted at `hash`, replacing an older digest (pure). A path the
    /// store's line format cannot hold (a line break, or bytes that are not UTF-8) is refused:
    /// written out, it would come back as another path or none.
    pub fn with(mut self, path: &Path, hash: &Digest) -> Result<Self> {
        match path.to_str() {
            Some(text) if !text.contains(['\n', '\r']) => {}
            _ => bail!("cannot record {path:?}: its name has a line break or is not UTF-8"),
        }
        self.0.insert(path.to_path_buf(), *hash);
        Ok(self)
    }

    /// Ok when `path` is trusted at `hash`; otherwise the refusal that says how to proceed.
    pub fn require(&self, path: &Path, hash: &Digest) -> Result<()> {
        if self.trusts(path, hash) {
            return Ok(());
        }
        bail!(
            "{} is not trusted (a project file can choose the host, the remote directory, a \
             shell file the remote sources and the commands of recipes): review it with `rr trust`, then run `rr trust --yes`",
            path.display()
        )
    }
}

/// The store's path from the variables that locate a state directory (pure).
pub fn store_file(
    xdg_state_home: Option<&str>,
    home: Option<&str>,
    appdata: Option<&str>,
) -> Option<PathBuf> {
    let base = match (xdg_state_home, home, appdata) {
        (Some(xdg), _, _) if !xdg.is_empty() => PathBuf::from(xdg),
        (_, Some(home), _) if !home.is_empty() => Path::new(home).join(".local/state"),
        (_, _, Some(appdata)) if !appdata.is_empty() => PathBuf::from(appdata),
        _ => return None,
    };
    Some(base.join("rr").join("trusted"))
}

/// Read the store; a missing file is an empty store.
#[allow(clippy::disallowed_methods)] // file I/O is this function's job
pub fn load(path: &Path) -> Result<TrustStore> {
    match std::fs::read_to_string(path) {
        Ok(text) => TrustStore::parse(&text).with_context(|| format!("in {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the store, private to the user (directory 0700, file 0600 on Unix), by rename so a crash
/// leaves the old store.
#[allow(clippy::disallowed_methods)] // file I/O is this function's job
pub fn save(path: &Path, store: &TrustStore) -> Result<()> {
    let dir = path.parent().context("the trust store has no directory")?;
    create_private_dir(dir)?;
    let temp = path.with_extension("tmp");
    write_private(&temp, store.render().as_bytes())?;
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[allow(clippy::disallowed_methods)] // file I/O is this function's job
#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))
}

#[allow(clippy::disallowed_methods)] // file I/O is this function's job
#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build real files
mod tests {
    use super::*;
    use crate::domain::ids::digest;

    #[test]
    fn digest_is_sha256() {
        assert_eq!(
            digest(b"abc").to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_file_is_trusted_only_at_the_reviewed_digest() {
        let path = Path::new("/p/.rr.toml");
        let store = TrustStore::default()
            .with(path, &digest(b"host = \"a\"\n"))
            .unwrap();
        assert!(store.trusts(path, &digest(b"host = \"a\"\n")));
        assert!(!store.trusts(path, &digest(b"host = \"evil\"\n")));
        assert!(!store.trusts(Path::new("/other/.rr.toml"), &digest(b"host = \"a\"\n")));
        let refusal = store.require(path, &digest(b"x")).unwrap_err().to_string();
        assert!(refusal.contains("/p/.rr.toml") && refusal.contains("rr trust"));
    }

    #[test]
    fn the_store_round_trips_and_refuses_damage() {
        let store = TrustStore::default()
            .with(Path::new("/a b/.rr.toml"), &digest(b"1"))
            .unwrap()
            .with(Path::new("/c/.rr.toml"), &digest(b"2"))
            .unwrap();
        assert_eq!(TrustStore::parse(&store.render()).unwrap(), store);
        assert!(TrustStore::parse("nothex /x\n").is_err());
        assert!(TrustStore::parse("abc\n").is_err());
    }

    #[test]
    fn a_digest_parses_either_case_and_prints_lowercase() {
        let upper = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        let parsed = Digest::parse(upper).unwrap();
        assert_eq!(parsed, digest(b"abc"));
        assert_eq!(parsed.to_string(), upper.to_lowercase());
        for bad in [
            "",
            "abc",
            &"g".repeat(64),
            &"a".repeat(63),
            &"a".repeat(65),
            &"é".repeat(32),
        ] {
            assert!(Digest::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_path_the_line_format_cannot_hold_is_refused_so_render_and_parse_round_trip() {
        let hash = digest(b"1");
        for bad in ["/a\nb/.rr.toml", "/a\rb/.rr.toml", "/x\n0000/.rr.toml"] {
            let err = TrustStore::default().with(Path::new(bad), &hash);
            assert!(err.is_err(), "{bad:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = Path::new(std::ffi::OsStr::from_bytes(b"/caf\xe9/.rr.toml"));
            assert!(TrustStore::default().with(raw, &hash).is_err());
        }
        for odd in [" /lead", "/a  b/ ", "/tab\there/.rr.toml", "/é/.rr.toml"] {
            let store = TrustStore::default().with(Path::new(odd), &hash).unwrap();
            assert_eq!(
                TrustStore::parse(&store.render()).unwrap(),
                store,
                "{odd:?}"
            );
        }
    }

    #[test]
    fn store_location() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(store_file(Some("/s"), Some("/h"), None), p("/s/rr/trusted"));
        assert_eq!(
            store_file(None, Some("/h"), None),
            p("/h/.local/state/rr/trusted")
        );
        assert_eq!(store_file(None, None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_store_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/rr/trusted");
        save(
            &path,
            &TrustStore::default()
                .with(Path::new("/x"), &digest(b"1"))
                .unwrap(),
        )
        .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(load(&path).unwrap().trusts(Path::new("/x"), &digest(b"1")));
    }
}
