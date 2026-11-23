//! Identifiers that are fixed-length hex digests, validated once at construction.

use anyhow::{Context, Result, bail};
use sha2::{Digest as _, Sha256};
use std::fmt;

/// How many leading digits [`HexId`] shows to people; the full id is for programs.
const SHORT_LEN: usize = 12;

/// Exactly `N` lowercase hex digits. One validating parser for every digest-like id (`Commit` now,
/// a tree id later), so each of them is a distinct type over the same check.
///
/// ```compile_fail
/// let _ = remote_run::domain::ids::HexId::<4>("zzzz".to_string());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexId<const N: usize>(String);

impl<const N: usize> HexId<N> {
    /// `text` without surrounding whitespace, as exactly `N` hex digits in either case. `what`
    /// names the id in the error.
    pub fn parse(what: &str, text: &str) -> Result<Self> {
        let text = text.trim();
        if text.len() != N || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("not a full {what}: {text:?}");
        }
        Ok(Self(text.to_ascii_lowercase()))
    }

    /// All `N` digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The leading digits only, as people write commit ids.
impl<const N: usize> fmt::Display for HexId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0[..SHORT_LEN.min(N)])
    }
}

/// A full 40-hex git commit id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit(HexId<40>);

impl Commit {
    pub fn parse(text: &str) -> Result<Self> {
        HexId::parse("commit id", text).map(Self)
    }

    /// All 40 digits.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A SHA-256 digest. A type of its own, so a digest cannot be confused with a path or any other
/// string, and a stored one is known to be well formed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest([u8; 32]);

impl Digest {
    /// 64 hex digits (either case).
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("not a SHA-256 digest: {text:?}");
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).context("not a SHA-256 digest")?;
            *byte = u8::from_str_radix(pair, 16).context("not a SHA-256 digest")?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// SHA-256 of `bytes`.
pub fn digest(bytes: &[u8]) -> Digest {
    Digest(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_hex_id_has_exactly_n_hex_digits_in_either_case_and_is_stored_lowercase() {
        let id = HexId::<4>::parse("id", " AbC1\n").unwrap();
        assert_eq!(id.as_str(), "abc1");
        for bad in ["", "abc", "abc12", "abcg", "ab c", "abc\u{661}"] {
            let err = HexId::<4>::parse("id", bad).unwrap_err().to_string();
            assert!(err.starts_with("not a full id: "), "{bad:?}: {err}");
        }
    }

    #[test]
    fn the_length_is_part_of_the_type() {
        assert!(HexId::<40>::parse("id", &"a".repeat(64)).is_err());
        assert!(HexId::<64>::parse("id", &"a".repeat(64)).is_ok());
        assert!(HexId::<64>::parse("id", &"a".repeat(40)).is_err());
    }

    #[test]
    fn people_see_twelve_digits_and_programs_all_of_them() {
        let commit = Commit::parse(A).unwrap();
        assert_eq!(commit.to_string(), "aaaaaaaaaaaa");
        assert_eq!(commit.as_str(), A);
        assert_eq!(HexId::<4>::parse("id", "abcd").unwrap().to_string(), "abcd");
    }

    #[test]
    fn a_commit_id_must_be_forty_digits() {
        assert!(Commit::parse("abc123").is_err());
        assert!(Commit::parse(&"g".repeat(40)).is_err());
        let err = Commit::parse("abc").unwrap_err().to_string();
        assert_eq!(err, "not a full commit id: \"abc\"");
        assert_eq!(
            Commit::parse(&format!("{}\n", A.to_uppercase())).unwrap(),
            Commit::parse(A).unwrap()
        );
    }
}
