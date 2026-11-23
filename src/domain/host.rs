//! The ssh host: a validated name, so it can never be read as an option or a rsync daemon address.

use anyhow::{Result, bail};
use std::fmt;

/// An ssh host or alias, optionally `user@host`.
///
/// ssh and rsync read a leading `-` as an option (`-oProxyCommand=...` runs a local command), and
/// rsync reads `host::path` as its daemon protocol over TCP. The host can come from a file in a
/// cloned repository, so both are refused here, once, at the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host(String);

impl Host {
    pub fn parse(text: &str) -> Result<Self> {
        let (user, name) = match text.split_once('@') {
            Some((user, name)) => (Some(user), name),
            None => (None, text),
        };
        for part in user.into_iter().chain([name]) {
            check_part(part, text)?;
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn check_part(part: &str, whole: &str) -> Result<()> {
    if part.is_empty() {
        bail!("not an ssh host: {whole:?} (empty name)");
    }
    if part.starts_with('-') {
        bail!("not an ssh host: {whole:?} (starts with '-', which ssh reads as an option)");
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    if !part.chars().all(allowed) {
        bail!(
            "not an ssh host: {whole:?} (use letters, digits, '.', '_' and '-'; \
             put anything else in an alias in ~/.ssh/config)"
        );
    }
    Ok(())
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_names_aliases_and_users() {
        for text in ["cluster", "my-cluster.example.org", "me@cluster", "a_b.c"] {
            assert_eq!(Host::parse(text).unwrap().as_str(), text);
        }
    }

    #[test]
    fn refuses_what_ssh_or_rsync_would_misread() {
        for text in [
            "",
            "-oProxyCommand=touch /tmp/x",
            "-a@b",
            "me@-b",
            "evil.example:",
            "evil.example::mod",
            "[::1]",
            "a b",
            "a;b",
            "a\nb",
            "@host",
            "me@",
            "a@b@c",
        ] {
            assert!(Host::parse(text).is_err(), "{text:?}");
        }
    }
}
