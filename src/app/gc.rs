//! `rr gc`: delete the snapshots no job uses, in one remote script (see
//! [`crate::domain::snapshot::gc_script`] for the order it deletes in).

use super::Ports;
use super::snapshot::project_place;
use crate::config::Session;
use crate::domain::outcome::Outcome;
use crate::domain::place::script;
use crate::domain::sanitize::sanitize;
use crate::domain::snapshot::{Age, GC_REFUSED_EXIT, GcOutcome, Keep, gc_script, parse_report};
use anyhow::{Result, bail};

pub struct Args {
    pub age: Age,
    pub keep: Keep,
}

pub fn run(session: &Session, args: Args, ports: &Ports) -> Result<Outcome> {
    let place = project_place(session)?;
    let captured = ports
        .remote
        .capture(&script(&place, gc_script(args.age, args.keep)))?;
    match (captured.code, parse_report(&captured.stdout)) {
        (0, Ok(outcome @ GcOutcome::Cleaned(_)))
        | (GC_REFUSED_EXIT, Ok(outcome @ GcOutcome::Refused(_))) => Ok(Outcome::Gc(outcome)),
        (0 | GC_REFUSED_EXIT, Ok(_)) => bail!("the clean-up's exit code and its report disagree"),
        (0 | GC_REFUSED_EXIT, Err(err)) => Err(err),
        (code, _) => bail!(
            "the clean-up failed ({code}): {}{}",
            sanitize(captured.stderr.trim()),
            reported_so_far(&captured.stdout)
        ),
    }
}

/// What a script that stopped half way had already printed, so a removal that did happen is not
/// lost with the error.
fn reported_so_far(stdout: &str) -> String {
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("rr-gc\t"))
        .collect();
    match lines.is_empty() {
        true => String::new(),
        false => format!("\nit had reported: {}", sanitize(&lines.join("; "))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Fakes, session};
    use super::*;
    use crate::domain::exit::{Exit, RrCode};
    use crate::domain::outcome::{Format, render};
    use crate::domain::snapshot::GcRefusal;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn args() -> Args {
        Args {
            age: Age::parse("48h").unwrap(),
            keep: Keep::parse("2").unwrap(),
        }
    }

    fn lines(report: &[String]) -> String {
        report
            .iter()
            .map(|line| format!("rr-gc\t{line}\n"))
            .collect()
    }

    fn remote(code: i32, stdout: &str, stderr: &str) -> Fakes {
        Fakes::new(vec![], vec![], vec![(code, stdout, stderr)], vec![])
    }

    #[test]
    fn one_script_runs_in_the_project_directory_with_the_age_and_the_count() {
        let out = lines(&[
            format!("kept\tin-use\t{A}"),
            format!("removed\t{B}"),
            "done".into(),
        ]);
        let fakes = remote(0, &out, "");
        let outcome = run(&session(), args(), &fakes.ports()).unwrap();
        let sent = fakes.remote.scripts();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].starts_with("set -euo pipefail\ncd 'proj'\nexport LC_ALL=C\n"),
            "{}",
            sent[0]
        );
        assert!(sent[0].contains("cutoff=$((now - 172800))"), "{}", sent[0]);
        assert!(sent[0].contains("-le 2 ]"), "{}", sent[0]);
        let shown = render(&outcome, Format::Text);
        assert_eq!(shown.exit, Exit::Success);
        assert!(
            shown.stdout.contains(&format!("removed {B}")),
            "{}",
            shown.stdout
        );
        assert!(
            shown
                .stdout
                .contains(&format!("kept {A} (an active job runs from it)"))
        );
    }

    /// The script says it deleted nothing and why; `rr` exits 3 like every refusal.
    #[test]
    fn a_refusal_is_exit_3_and_names_the_reason() {
        for (word, why) in [
            ("queue", GcRefusal::QueueUnreadable),
            ("root", GcRefusal::RootNotSafe),
        ] {
            let fakes = remote(GC_REFUSED_EXIT, &lines(&[format!("refused\t{word}")]), "");
            let outcome = run(&session(), args(), &fakes.ports()).unwrap();
            assert!(matches!(&outcome, Outcome::Gc(GcOutcome::Refused(got)) if *got == why));
            let shown = render(&outcome, Format::Text);
            assert_eq!(shown.exit, Exit::Rr(RrCode::Refused));
            assert!(
                shown.stderr.starts_with("rr: refused: "),
                "{}",
                shown.stderr
            );
            assert!(shown.stderr.contains("nothing was deleted"));
        }
    }

    #[test]
    fn a_script_that_failed_is_an_error_with_its_sanitized_stderr_and_what_it_had_done() {
        let done = lines(&[format!("removed\t{A}")]);
        let fakes = remote(1, &done, "\u{1b}[2Jstat: cannot stat\n");
        let err = run(&session(), args(), &fakes.ports())
            .unwrap_err()
            .to_string();
        assert!(err.contains("(1): ?[2Jstat: cannot stat"), "{err}");
        assert!(
            err.contains(&format!("it had reported: rr-gc\tremoved\t{A}")),
            "{err}"
        );
        assert!(!err.contains('\u{1b}'));
    }

    /// An exit code and a report that do not agree, or a report that is cut short or garbled, is
    /// never read as a clean-up.
    #[test]
    fn a_report_that_does_not_fit_the_exit_code_is_an_error() {
        let done = lines(&["done".into()]);
        let refused = lines(&["refused\tqueue".into()]);
        for (code, stdout) in [
            (0, refused.as_str()),
            (GC_REFUSED_EXIT, done.as_str()),
            (0, ""),
            (0, "rr-gc\tremoved\tnot-a-name\nrr-gc\tdone\n"),
            (0, "rr-gc\twhat\n"),
            (GC_REFUSED_EXIT, ""),
        ] {
            let fakes = remote(code, stdout, "");
            assert!(
                run(&session(), args(), &fakes.ports()).is_err(),
                "{code} {stdout:?}"
            );
        }
    }

    #[test]
    fn without_a_project_directory_nothing_is_sent() {
        let fakes = remote(0, "", "");
        let mut home = session();
        home.place.dir = None;
        let err = run(&home, args(), &fakes.ports()).unwrap_err();
        assert!(err.to_string().contains("project directory"), "{err}");
        assert!(fakes.remote.scripts().is_empty());
    }

    #[test]
    fn a_failed_connection_is_an_error() {
        let fakes = Fakes::new(
            vec![],
            vec![],
            vec![(255, "", "ssh: Could not resolve")],
            vec![],
        );
        let err = run(&session(), args(), &fakes.ports())
            .unwrap_err()
            .to_string();
        assert!(err.contains("(255)"), "{err}");
    }
}
