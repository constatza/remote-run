//! `rr trust`: review the project file that applies here, and record it as reviewed.
//!
//! It needs no host, so it takes the [`Environment`] instead of a session.

use crate::config::{Environment, PROJECT_FILE, read_project};
use crate::domain::outcome::{Outcome, TrustReport, TrustState};
use crate::trust::{self, TrustAction};
use anyhow::{Context, Result, bail};

pub fn run(env: &Environment, action: TrustAction) -> Result<Outcome> {
    let Some(file) = read_project(&env.cwd)? else {
        bail!("no {PROJECT_FILE} in this directory or above");
    };
    let review = file.review();
    let state = match action {
        TrustAction::Show => TrustState::Shown,
        TrustAction::Record => {
            let store = env
                .trust_file()
                .context("no state directory: set XDG_STATE_HOME or HOME")?;
            let trusted = trust::load(&store)?.with(review.path, &review.digest)?;
            trust::save(&store, &trusted)?;
            TrustState::Recorded
        }
    };
    Ok(Outcome::Trust(TrustReport {
        path: review.path.to_path_buf(),
        digest: review.digest,
        text: review.text,
        state,
    }))
}

#[allow(clippy::disallowed_methods)] // tests build real files
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A project directory with a `.rr.toml`, and an environment whose state directory is empty.
    fn project(text: &str) -> (tempfile::TempDir, Environment) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join(PROJECT_FILE), text).unwrap();
        let env = Environment {
            cwd: root.clone(),
            xdg_state_home: Some(root.join("state").display().to_string()),
            ..Environment::default()
        };
        (dir, env)
    }

    fn state_of(outcome: Outcome) -> TrustState {
        match outcome {
            Outcome::Trust(report) => report.state,
            other => panic!("not a trust report: {other:?}"),
        }
    }

    #[test]
    fn showing_records_nothing_and_recording_makes_the_file_trusted() {
        let (_dir, env) = project("host = \"h\"\n");
        let shown = run(&env, TrustAction::Show).unwrap();
        assert_eq!(state_of(shown), TrustState::Shown);
        let store = env.trust_file().unwrap();
        assert!(!store.exists(), "showing must not write the store");
        let recorded = run(&env, TrustAction::Record).unwrap();
        assert_eq!(state_of(recorded), TrustState::Recorded);
        let file = read_project(&env.cwd).unwrap().unwrap();
        let trusted = trust::load(&store).unwrap();
        assert!(file.trusted(&trusted).is_ok());
    }

    #[test]
    fn no_project_file_is_an_error_and_recording_needs_a_state_directory() {
        let empty = tempfile::tempdir().unwrap();
        let env = Environment {
            cwd: empty.path().canonicalize().unwrap(),
            ..Environment::default()
        };
        let err = run(&env, TrustAction::Show).unwrap_err().to_string();
        assert!(err.contains("no .rr.toml"), "{err}");
        let (_dir, mut env) = project("");
        env.xdg_state_home = None;
        let err = run(&env, TrustAction::Record).unwrap_err().to_string();
        assert!(err.contains("no state directory"), "{err}");
    }
}
