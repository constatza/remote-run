//! Recipes: a named remote command, written once in the config as an argv with `{slot}`
//! placeholders and filled in at the call. The command is a list of words, never a shell string:
//! every filled word is quoted on its own, so a value cannot add a word or run anything.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The longest name of a recipe or a slot.
const NAME_MAX: usize = 32;
/// How `--set` separates a slot from its value.
pub const ASSIGN: char = '=';

/// `[A-Za-z0-9_-]+`, no leading `-`: a name that is safe in a message and cannot look like a flag.
fn plain_name(what: &str, text: &str) -> Result<String> {
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

macro_rules! name_type {
    ($(#[$doc:meta])* $type:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $type(String);

        impl $type {
            pub fn parse(text: &str) -> Result<Self> {
                plain_name($what, text).map(Self)
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_type!(
    /// What `rr do` is given to choose a recipe.
    RecipeName,
    "recipe"
);
name_type!(
    /// What a `{placeholder}` and a `--set` key call a value.
    SlotName,
    "placeholder"
);

/// Whether the remote checkout is brought up to this one before the recipe's command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipePull {
    /// The guarded fast-forward of `rr run`.
    Checkout,
    /// Run in the remote directory as it is, like `rr x`.
    Skip,
}

impl RecipePull {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "checkout" => Ok(Self::Checkout),
            "none" => Ok(Self::Skip),
            other => bail!("not a pull mode: {other:?} (use \"checkout\" or \"none\")"),
        }
    }
}

/// One piece of a word: text as written, or a value that is filled in at the call.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    Slot(SlotName),
}

/// One word of the command. `{{` and `}}` write a brace; any other `{` or `}` is an error, so a
/// typo cannot send its braces to the cluster unnoticed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Template(Vec<Piece>);

impl Template {
    fn parse(word: &str) -> Result<Self> {
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut chars = word.chars().peekable();
        while let Some(c) = chars.next() {
            match (c, chars.peek()) {
                ('{', Some('{')) | ('}', Some('}')) => {
                    text.push(c);
                    chars.next();
                }
                ('{', _) => {
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) if c != '{' => name.push(c),
                            _ => bail!("{word:?}: a '{{' is not closed"),
                        }
                    }
                    pieces.push(Piece::Slot(
                        SlotName::parse(&name)
                            .with_context(|| format!("{word:?}: not a placeholder"))?,
                    ));
                }
                ('}', _) => bail!("{word:?}: a '}}' has no '{{' (write '}}}}' for a brace)"),
                (c, _) => text.push(c),
            }
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Ok(Self(pieces))
    }

    fn slots(&self) -> impl Iterator<Item = &SlotName> {
        self.0.iter().filter_map(|piece| match piece {
            Piece::Slot(slot) => Some(slot),
            Piece::Text(_) => None,
        })
    }

    /// Each slot is replaced once, by its value as given: a value that contains `{x}` stays text.
    fn fill(&self, values: &Values) -> Result<String> {
        let mut word = String::new();
        for piece in &self.0 {
            match piece {
                Piece::Text(text) => word.push_str(text),
                Piece::Slot(slot) => match values.0.get(slot) {
                    Some(value) => word.push_str(value),
                    None => bail!("no value for {{{slot}}} (pass --set {slot}=VALUE)"),
                },
            }
        }
        Ok(word)
    }
}

/// A `--set` argument: which slot, and what to put in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    slot: SlotName,
    value: String,
}

impl Assignment {
    /// `slot=value`; the value is everything after the first `=` and may be empty.
    pub fn parse(text: &str) -> Result<Self, String> {
        let Some((slot, value)) = text.split_once(ASSIGN) else {
            return Err(format!("expected SLOT{ASSIGN}VALUE, got {text:?}"));
        };
        let slot = SlotName::parse(slot).map_err(|err| err.to_string())?;
        Ok(Self {
            slot,
            value: value.to_string(),
        })
    }
}

/// The values for a recipe's slots; a slot is given at most once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Values(BTreeMap<SlotName, String>);

impl Values {
    pub fn new(assignments: Vec<Assignment>) -> Result<Self> {
        let mut values = BTreeMap::new();
        for Assignment { slot, value } in assignments {
            if values.insert(slot.clone(), value).is_some() {
                bail!("{slot} is set twice");
            }
        }
        Ok(Self(values))
    }
}

/// A command: at least a program word, then argument words, and what to do about the checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    program: Template,
    args: Vec<Template>,
    pull: RecipePull,
}

impl Recipe {
    /// `run` is the command as words; the first is the program.
    pub fn new(run: &[String], pull: RecipePull) -> Result<Self> {
        let Some((program, args)) = run.split_first() else {
            bail!("run is empty (give the command as a list of words)");
        };
        Ok(Self {
            program: Template::parse(program)?,
            args: args
                .iter()
                .map(|word| Template::parse(word))
                .collect::<Result<_>>()?,
            pull,
        })
    }

    pub fn pull(&self) -> RecipePull {
        self.pull
    }

    fn slots(&self) -> BTreeSet<&SlotName> {
        std::iter::once(&self.program)
            .chain(&self.args)
            .flat_map(Template::slots)
            .collect()
    }

    /// The command's words with every slot filled, then `extra` words as the caller typed them. A
    /// slot without a value is an error, and so is a value for a slot the recipe does not have: a
    /// typo must not run the command with a default nobody chose.
    pub fn fill(&self, values: &Values, extra: &[String]) -> Result<Vec<String>> {
        let slots = self.slots();
        if let Some(unused) = values.0.keys().find(|key| !slots.contains(key)) {
            bail!("the recipe has no placeholder {{{unused}}}");
        }
        let mut words = vec![self.program.fill(values)?];
        for arg in &self.args {
            words.push(arg.fill(values)?);
        }
        words.extend(extra.iter().cloned());
        Ok(words)
    }
}

/// The recipes of the merged config, by name.
pub type RecipeTable = BTreeMap<RecipeName, Recipe>;

#[cfg(test)]
mod tests {
    use super::*;

    fn words(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| text.to_string()).collect()
    }

    fn recipe(run: &[&str]) -> Recipe {
        Recipe::new(&words(run), RecipePull::Checkout).unwrap()
    }

    fn values(pairs: &[&str]) -> Values {
        Values::new(
            pairs
                .iter()
                .map(|p| Assignment::parse(p).unwrap())
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn slots_are_filled_word_by_word_and_extra_words_follow() {
        let train = recipe(&["python", "-m", "train", "--cfg={config}.yaml", "{seed}"]);
        let filled = train
            .fill(&values(&["config=base", "seed=7"]), &words(&["--fast"]))
            .unwrap();
        assert_eq!(
            filled,
            words(&["python", "-m", "train", "--cfg=base.yaml", "7", "--fast"])
        );
    }

    #[test]
    fn a_value_is_inserted_as_given_and_never_expanded_again() {
        let echo = recipe(&["echo", "{a}", "{b}"]);
        let filled = echo.fill(&values(&["a={b}", "b=$(id); x'y"]), &[]).unwrap();
        assert_eq!(filled, words(&["echo", "{b}", "$(id); x'y"]));
    }

    #[test]
    fn a_missing_value_and_a_value_for_no_slot_are_refused() {
        let echo = recipe(&["echo", "{a}"]);
        let missing = echo.fill(&Values::default(), &[]).unwrap_err().to_string();
        assert_eq!(missing, "no value for {a} (pass --set a=VALUE)");
        let extra = echo.fill(&values(&["a=1", "b=2"]), &[]).unwrap_err();
        assert_eq!(extra.to_string(), "the recipe has no placeholder {b}");
    }

    #[test]
    fn braces_are_written_twice_and_a_stray_one_is_an_error() {
        let literal = recipe(&["python", "-c", "print({{1}})"]);
        let filled = literal.fill(&Values::default(), &[]).unwrap();
        assert_eq!(filled[2], "print({1})");
        for bad in ["{a", "a}", "{}", "{a b}", "{-x}", "{{a}", "{a{b}", "}}{a"] {
            let parsed = Recipe::new(&words(&["echo", bad]), RecipePull::Checkout);
            assert!(parsed.is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn a_recipe_needs_a_program_and_the_program_can_be_a_slot() {
        assert!(Recipe::new(&[], RecipePull::Skip).is_err());
        let run = recipe(&["{python}", "-V"]);
        let filled = run.fill(&values(&["python=python3"]), &[]).unwrap();
        assert_eq!(filled, words(&["python3", "-V"]));
    }

    #[test]
    fn names_assignments_and_pull_modes_are_validated() {
        assert!(RecipeName::parse("train-2").is_ok());
        for bad in ["", "-x", "a b", "../x", "a;b"] {
            assert!(RecipeName::parse(bad).is_err(), "{bad:?}");
            assert!(SlotName::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Assignment::parse("a=").unwrap().value, "");
        assert_eq!(Assignment::parse("a=b=c").unwrap().value, "b=c");
        assert!(Assignment::parse("novalue").is_err());
        assert!(Assignment::parse("-a=1").is_err());
        assert_eq!(RecipePull::parse("none").unwrap(), RecipePull::Skip);
        assert!(RecipePull::parse("snapshot").is_err());
        assert!(
            Values::new(vec![
                Assignment::parse("a=1").unwrap(),
                Assignment::parse("a=2").unwrap()
            ])
            .is_err()
        );
    }
}
