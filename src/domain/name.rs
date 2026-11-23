//! The one rule for a name that is typed by the user and ends up in a message, a table key or a
//! file name.

use anyhow::{Result, bail};

/// The longest name: a forward name ends up in a socket file name under the runtime directory.
pub const NAME_MAX: usize = 32;

/// `[A-Za-z0-9_-]+`, no leading `-`: a name that is safe in a message and cannot look like a flag.
pub fn plain_name(what: &str, text: &str) -> Result<String> {
    let ok = !text.is_empty()
        && !text.starts_with('-')
        && text.len() <= NAME_MAX
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if !ok {
        bail!("not a {what} name: {text:?} (1-{NAME_MAX} letters, digits, '_' and '-')");
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_names() {
        assert_eq!(plain_name("x", "a_b-9").unwrap(), "a_b-9");
        assert!(plain_name("x", &"a".repeat(NAME_MAX)).is_ok());
    }

    #[test]
    fn refuses_the_rest() {
        for bad in [
            "",
            "-a",
            "a b",
            "a/b",
            "../x",
            "é",
            &"a".repeat(NAME_MAX + 1),
        ] {
            let err = plain_name("slot", bad).unwrap_err();
            assert!(err.to_string().starts_with("not a slot name: "), "{err}");
        }
    }
}
