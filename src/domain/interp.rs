//! The program that reads a script sent over stdin (`rr x -s probe.py --interp "python -"`).

use anyhow::{Result, bail};

/// A program and its arguments, split from the text of `--interp` without a shell: words are
/// separated by blanks, `'...'` and `"..."` group a word and stand for exactly their text (no
/// escapes, no expansion: every word is quoted again before it reaches the remote shell).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpreter {
    program: String,
    args: Vec<String>,
}

impl Interpreter {
    pub fn parse(text: &str) -> Result<Self> {
        let mut words = split(text)?.into_iter();
        let Some(program) = words.next() else {
            bail!("an interpreter needs a program (for example \"python -\")");
        };
        if program.is_empty() {
            bail!("the interpreter's program is empty ({text:?})");
        }
        Ok(Self {
            program,
            args: words.collect(),
        })
    }

    /// The program, then its arguments.
    pub fn words(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.program.as_str()).chain(self.args.iter().map(String::as_str))
    }
}

fn split(text: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), c) => word.get_or_insert_with(String::new).push(c),
            (None, '\\') => {
                bail!("{text:?}: a backslash is not understood (quote the word instead)")
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (None, c) if c.is_whitespace() => words.extend(word.take()),
            (None, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    if quote.is_some() {
        bail!("{text:?}: a quote is not closed");
    }
    words.extend(word);
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        let interp = Interpreter::parse(text).unwrap();
        interp.words().map(str::to_string).collect()
    }

    #[test]
    fn words_split_on_blanks_and_quotes_group_without_expanding() {
        assert_eq!(words("python -"), ["python", "-"]);
        assert_eq!(words("  python   -u  - "), ["python", "-u", "-"]);
        assert_eq!(
            words("\"my python\" '-c' \"a b\""),
            ["my python", "-c", "a b"]
        );
        assert_eq!(
            words("sh -s -- $HOME $(id)"),
            ["sh", "-s", "--", "$HOME", "$(id)"]
        );
        assert_eq!(words("env X='a b' python"), ["env", "X=a b", "python"]);
        assert_eq!(words("prog ''"), ["prog", ""]);
    }

    #[test]
    fn nothing_to_run_a_backslash_and_an_open_quote_are_errors() {
        for bad in ["", "   ", "a\\b", "'a", "\"a b", "a 'b", "''", "\"\" -"] {
            assert!(Interpreter::parse(bad).is_err(), "{bad:?}");
        }
    }
}
