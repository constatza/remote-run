//! `rr do`: run a recipe, the named command of the config.

use super::run::{After, Args as RunArgs};
use super::{Ports, exec, run};
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::pull::JobPolicy;
use crate::domain::recipe::{RecipeName, RecipePull, Values};
use crate::ports::Terminal;
use anyhow::{Result, anyhow, bail};

pub struct Args {
    pub name: RecipeName,
    pub values: Values,
    /// Words after `--`, appended to the recipe's command.
    pub extra: Vec<String>,
    pub policy: JobPolicy,
    pub terminal: Terminal,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let recipe = session.recipes.get(&args.name).ok_or_else(|| {
        let known: Vec<String> = session.recipes.keys().map(ToString::to_string).collect();
        anyhow!("no recipe {} (known: {})", args.name, known.join(", "))
    })?;
    let cmd = recipe.fill(&args.values, &args.extra)?;
    match recipe.pull() {
        RecipePull::Checkout => {
            let run_args = RunArgs::new(args.policy, args.terminal, cmd, After::Return);
            run::run(session, run_args, ports)
        }
        RecipePull::Skip => {
            // --force lifts a pull guard; with no pull there is nothing for it to lift
            if args.policy == JobPolicy::Override {
                bail!(
                    "--force has no effect on recipe {}: it does not pull",
                    args.name
                );
            }
            let terminal = args.terminal;
            exec::run(session, exec::Args { terminal, cmd }, ports)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, in_project, session};
    use super::*;
    use crate::domain::recipe::{Assignment, Recipe};
    use crate::ports::fake::{RemoteCall, output};

    fn with_recipe(pull: RecipePull) -> Session {
        let run = ["python", "train.py", "--cfg={cfg}"].map(String::from);
        let mut session = session();
        session.recipes = [(
            RecipeName::parse("train").unwrap(),
            Recipe::new(&run, pull).unwrap(),
        )]
        .into_iter()
        .collect();
        session
    }

    fn args(name: &str, sets: &[&str], extra: &[&str]) -> Args {
        let assignments = sets.iter().map(|s| Assignment::parse(s).unwrap()).collect();
        Args {
            name: RecipeName::parse(name).unwrap(),
            values: Values::new(assignments).unwrap(),
            extra: extra.iter().map(|word| word.to_string()).collect(),
            policy: JobPolicy::Respect,
            terminal: Terminal::Plain,
        }
    }

    #[test]
    fn a_recipe_without_a_pull_runs_the_quoted_words_in_the_project() {
        let fakes = Fakes::remote_streaming(vec![3]);
        let session = with_recipe(RecipePull::Skip);
        let outcome = run(
            &session,
            args("train", &["cfg=a b"], &["$(id)"]),
            &fakes.ports(),
        );
        assert!(matches!(outcome, Ok(Outcome::Remote(_))));
        let expected = in_project("'python' 'train.py' '--cfg=a b' '$(id)'");
        assert_eq!(fakes.remote.scripts(), [expected]);
        let calls = fakes.remote.calls.borrow();
        assert!(matches!(calls[0].1, RemoteCall::Stream(Terminal::Plain)));
    }

    #[test]
    fn force_on_a_recipe_that_does_not_pull_is_refused_not_ignored() {
        let fakes = Fakes::remote_streaming(vec![]);
        let session = with_recipe(RecipePull::Skip);
        let forced = Args {
            policy: JobPolicy::Override,
            ..args("train", &["cfg=a"], &[])
        };
        assert!(run(&session, forced, &fakes.ports()).is_err());
        assert!(fakes.remote.scripts().is_empty());
    }

    #[test]
    fn an_unknown_recipe_a_missing_value_and_a_bad_value_run_nothing() {
        let fakes = Fakes::remote_streaming(vec![]);
        let session = with_recipe(RecipePull::Skip);
        let ports = fakes.ports();
        let unknown = run(&session, args("nope", &[], &[]), &ports).unwrap_err();
        assert_eq!(unknown.to_string(), "no recipe nope (known: train)");
        assert!(run(&session, args("train", &[], &[]), &ports).is_err());
        assert!(run(&session, args("train", &["cfg=a", "x=b"], &[]), &ports).is_err());
        assert!(fakes.remote.scripts().is_empty());
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn state(extra: &str) -> String {
        format!(
            "rr-state\tdir\t/d\nrr-state\thead\t{A}\nrr-state\ttarget\t{B}\n\
             rr-state\tonorigin\t  origin/main\n{extra}"
        )
    }

    /// A clean local repository at `B` and a remote that answers `captures`.
    fn pulling(captures: Vec<(i32, &str, &str)>, streams: Vec<i32>) -> Fakes {
        let git = vec![output(0, &format!("{B}\n"), ""), output(0, "", "")];
        Fakes::new(git, vec![], captures, streams)
    }

    #[test]
    fn a_checkout_recipe_pulls_first_and_a_refused_pull_never_runs_it() {
        let behind = state("");
        let fakes = pulling(vec![(0, &behind, ""), (0, "", "")], vec![0]);
        let session = with_recipe(RecipePull::Checkout);
        run(&session, args("train", &["cfg=a"], &[]), &fakes.ports()).unwrap();
        let how: Vec<_> = fakes
            .remote
            .calls
            .borrow()
            .iter()
            .map(|(_, h)| *h)
            .collect();
        let capture = RemoteCall::Capture;
        let stream = RemoteCall::Stream(Terminal::Plain);
        assert_eq!(how, [capture, capture, stream], "state, merge, command");

        let dirty = state("rr-state\tdirty\t M x\n");
        let fakes = pulling(vec![(0, &dirty, "")], vec![]);
        let outcome = run(&session, args("train", &["cfg=a"], &[]), &fakes.ports()).unwrap();
        assert!(matches!(outcome, Outcome::Pulled(_)));
        let streamed = fakes
            .remote
            .calls
            .borrow()
            .iter()
            .any(|(_, h)| matches!(h, RemoteCall::Stream(_)));
        assert!(!streamed);
    }
}
