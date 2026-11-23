//! Shell text that cannot carry an injection: a script is built from `'static` literals and
//! [`Word`]s, and a `Word` exists only if [`quote`] (or [`number`]) made it. Runtime text (a user's
//! argument, a job id, a path read from the remote) can therefore reach a script only quoted.

use std::fmt;

/// One argument for a POSIX shell, already quoted. Made only by [`quote`] and [`number`], so a
/// `String` cannot be passed off as one.
///
/// ```compile_fail
/// let _ = remote_run::domain::shell::Word("$(id)".to_string());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word(String);

impl fmt::Display for Word {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `text` as one shell argument: single quotes, with `'` written as `'\''`.
pub fn quote(text: &str) -> Word {
    Word(format!("'{}'", text.replace('\'', r"'\''")))
}

/// A whole number as a shell argument, unquoted (`tail -n 40`, `exit 75`). Digits and a sign
/// cannot change the meaning of a command line.
pub fn number(value: impl Into<i64>) -> Word {
    Word(value.into().to_string())
}

/// A script of source text only.
pub fn lit(text: &'static str) -> Script {
    Script::new().lit(text)
}

/// A piece of a script: text written in the source, or words that were quoted at runtime.
///
/// A literal must be `'static`, so text built at runtime cannot be one:
///
/// ```compile_fail
/// use remote_run::domain::shell::Fragment;
/// fn build(runtime: String) -> Fragment {
///     Fragment::Literal(&runtime)
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fragment {
    Literal(&'static str),
    /// Joined by single spaces.
    Words(Vec<Word>),
}

/// Shell text as a list of [`Fragment`]s. Nothing converts a `&str` or a `String` into one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Script(Vec<Fragment>);

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append text written in the source.
    #[must_use]
    pub fn lit(mut self, text: &'static str) -> Self {
        self.0.push(Fragment::Literal(text));
        self
    }

    /// Append one quoted word.
    #[must_use]
    pub fn word(self, word: Word) -> Self {
        self.words([word])
    }

    /// Append quoted words, separated by spaces.
    #[must_use]
    pub fn words(mut self, words: impl IntoIterator<Item = Word>) -> Self {
        self.0.push(Fragment::Words(words.into_iter().collect()));
        self
    }

    /// Append another script.
    #[must_use]
    pub fn then(mut self, other: Script) -> Self {
        self.0.extend(other.0);
        self
    }

    /// The scripts as lines, in order.
    pub fn lines(lines: impl IntoIterator<Item = Script>) -> Self {
        let mut script = Self::new();
        for (index, line) in lines.into_iter().enumerate() {
            if index > 0 {
                script = script.lit("\n");
            }
            script = script.then(line);
        }
        script
    }
}

impl fmt::Display for Script {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for fragment in &self.0 {
            match fragment {
                Fragment::Literal(text) => f.write_str(text)?,
                Fragment::Words(words) => {
                    for (index, word) in words.iter().enumerate() {
                        if index > 0 {
                            f.write_str(" ")?;
                        }
                        word.fmt(f)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::OsExec;
    use crate::ports::{Argv, Exec, Stdin};

    fn through_sh(quoted: &str) -> String {
        let sh = Argv::new("sh", vec!["-c".into(), format!("printf %s {quoted}")]);
        OsExec.capture(&sh, Stdin::Null).expect("sh runs").stdout
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
            assert_eq!(through_sh(&quote(text).to_string()), text, "{text:?}");
        }
    }

    #[test]
    fn a_number_is_not_quoted() {
        assert_eq!(number(40_u32).to_string(), "40");
        assert_eq!(number(-5_i32).to_string(), "-5");
    }

    #[test]
    fn a_script_is_its_literals_and_its_words_joined_by_spaces() {
        let script = Script::new()
            .lit("echo ")
            .words(["a b", "it's"].map(quote))
            .lit(" | wc -l");
        assert_eq!(script.to_string(), r"echo 'a b' 'it'\''s' | wc -l");
        assert_eq!(Script::new().words([]).to_string(), "");
    }

    #[test]
    fn lines_are_joined_by_a_newline_and_scripts_nest() {
        let inner = Script::new().lit("b ").word(quote("x"));
        let lines = Script::lines([Script::new().lit("a"), inner, Script::new()]);
        assert_eq!(lines.to_string(), "a\nb 'x'\n");
        assert_eq!(Script::lines([]).to_string(), "");
    }

    /// Hostile text stays one argument whatever it holds: run through bash, the program sees it
    /// verbatim and nothing else runs.
    #[test]
    fn hostile_words_stay_one_argument() {
        let hostile = "x'; echo pwned; '$(echo pwned)`echo pwned`";
        let script = Script::new().lit("printf %s ").word(quote(hostile));
        let bash = Argv::new("bash", vec!["-c".into(), script.to_string()]);
        let out = OsExec.capture(&bash, Stdin::Null).unwrap().stdout;
        assert_eq!(out, hostile);
    }
}
