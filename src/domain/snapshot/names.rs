//! Which file names look like secrets, and the paths a user may name to let one through.

use anyhow::{Result, bail};

/// A path the user names with `--allow-path`: relative to the repository root, written as git
/// writes it (`a/b.pem`), no `.` or `..` parts. It is compared whole with a path of the tree, so
/// it lets exactly that file through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedPath(String);

impl AllowedPath {
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() || text.contains(['\0', '\n']) {
            bail!("an --allow-path is empty or has a NUL or a newline: {text:?}");
        }
        let plain = !text.starts_with('/')
            && !text.ends_with('/')
            && text.split('/').all(|part| !matches!(part, "" | "." | ".."));
        if !plain {
            bail!(
                "--allow-path {text:?} is not a path inside the repository \
                 (write it relative to the repository root, like `config/app.pem`)"
            );
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Names that are exactly this.
const SECRET_NAMES: [&str; 3] = [".netrc", ".npmrc", ".pypirc"];
/// Names that start with this: `.env`, `.env.local`, an ssh key `id_ed25519`.
const SECRET_PREFIXES: [&str; 2] = [".env", "id_"];
/// Names that end with this: certificates and keys.
const SECRET_SUFFIXES: [&str; 4] = [".pem", ".key", ".p12", ".kdbx"];
/// Names that hold this anywhere: cloud credential files.
const SECRET_PART: &str = "credentials";

/// Whether one path component looks like a secret (ASCII case ignored). A deny list is a net with
/// holes (`secrets.yaml` passes), which is why untracked files are opt-in at all; this only stops
/// the common mistakes once the user has opted in.
fn component_is_secret(component: &str) -> bool {
    let name = component.to_ascii_lowercase();
    SECRET_NAMES.contains(&name.as_str())
        || SECRET_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || SECRET_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || name.contains(SECRET_PART)
}

/// Whether any part of `path` looks like a secret: a file named so, or one inside a directory
/// named so.
pub fn looks_secret(path: &str) -> bool {
    path.split('/').any(component_is_secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secret_looking_names_are_caught_anywhere_in_a_path() {
        for path in [
            ".env",
            ".env.local",
            ".envrc",
            "app/.env",
            "certs/server.pem",
            "k.KEY",
            "id_rsa",
            "keys/id_ed25519",
            "aws/credentials",
            "my-credentials.json",
            "vault.kdbx",
            ".netrc",
            "home/.npmrc",
            ".pypirc",
            "bundle.p12",
            ".env/anything.txt",
        ] {
            assert!(looks_secret(path), "{path}");
        }
    }

    #[test]
    fn ordinary_names_pass() {
        for path in [
            "src/main.py",
            "environment.yml",
            "env.sh",
            "pem/readme.txt",
            "monkey.py",
            "idle.py",
            "secrets.yaml",
            "src/id.py",
        ] {
            assert!(!looks_secret(path), "{path}");
        }
    }

    #[test]
    fn an_allowed_path_is_a_plain_relative_path_of_the_repository() {
        assert_eq!(AllowedPath::parse("a/b.pem").unwrap().as_str(), "a/b.pem");
        for bad in [
            "", "/etc/x", "a/../b", "./a", "a//b", "a/", "..", "a\nb", "a\0b", ".",
        ] {
            assert!(AllowedPath::parse(bad).is_err(), "{bad:?}");
        }
    }
}
