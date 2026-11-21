//! POSIX shell quoting and the script every remote command runs in.

/// One argument for a POSIX shell: single quotes, with `'` written as `'\''`.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Several arguments, each quoted, joined by spaces.
pub fn join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|arg| quote(arg.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where a remote command runs: an optional shell file sourced first, then an optional directory.
/// Relative paths are relative to the remote home, where `ssh` starts.
#[derive(Clone, Debug, Default)]
pub struct Place {
    pub env: Option<String>,
    pub dir: Option<String>,
}

/// The bash script for `body` at `place`: strict mode, then `source`, then `cd`.
pub fn script(place: &Place, body: &str) -> String {
    let mut parts = vec!["set -euo pipefail".to_string()];
    if let Some(env) = &place.env {
        parts.push(format!("source {}", quote(env)));
    }
    if let Some(dir) = &place.dir {
        parts.push(format!("cd {}", quote(dir)));
    }
    parts.push(body.to_string());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn through_sh(quoted: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("sh runs");
        String::from_utf8(out.stdout).expect("utf-8")
    }

    #[test]
    fn quote_survives_a_shell() {
        for text in [
            "plain",
            "it's",
            "a b",
            "$HOME `id` $(id)",
            "line\nbreak",
            "''",
            "\\",
            "",
        ] {
            assert_eq!(through_sh(&quote(text)), text, "{text:?}");
        }
    }

    #[test]
    fn script_sources_then_changes_directory() {
        let place = Place {
            env: Some("hpc/env.sh".into()),
            dir: Some("my dir".into()),
        };
        assert_eq!(
            script(&place, "true"),
            "set -euo pipefail\nsource 'hpc/env.sh'\ncd 'my dir'\ntrue"
        );
        assert_eq!(script(&Place::default(), "true"), "set -euo pipefail\ntrue");
    }
}
