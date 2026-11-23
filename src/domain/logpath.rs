//! The log file of a job: `sacct` stores `--output` unexpanded (`%x-%j.out`), so it is expanded here.

use crate::domain::slurm::SACCT_SEPARATOR;
use anyhow::{Result, bail};

/// One job's record from `sacct`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRecord {
    pub id_raw: String,
    /// `123` or, for an array task, `123_4`.
    pub id: String,
    pub name: String,
    pub user: String,
    pub workdir: String,
    pub stdout: String,
}

/// Parse the one line of `sacct -n -X -P -o` [`crate::domain::slurm::SACCT_FIELDS`] (pure). Several
/// lines mean the id named more than one job (the parent id of an array): that is an error naming
/// them, because picking the first would show some other task's log.
pub fn parse_sacct(text: &str) -> Result<JobRecord> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(line) = lines.next() else {
        bail!("sacct knows no such job");
    };
    if lines.next().is_some() {
        let ids: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split(SACCT_SEPARATOR).nth(1))
            .collect();
        bail!(
            "that id names several jobs ({}): pick one, such as an array task `<id>_<n>`",
            ids.join(", ")
        );
    }
    let fields: Vec<&str> = line.splitn(6, SACCT_SEPARATOR).collect();
    let [id_raw, id, name, user, workdir, stdout] = fields[..] else {
        bail!("unexpected sacct line: {line:?}");
    };
    Ok(JobRecord {
        id_raw: id_raw.into(),
        id: id.into(),
        name: name.into(),
        user: user.into(),
        workdir: workdir.into(),
        stdout: stdout.into(),
    })
}

/// The absolute log path: Slurm's default when `--output` was not set, relative paths against
/// the job's directory, and the placeholders `%%`, `%A`, `%a`, `%J`, `%j`, `%u`, `%x` (pure).
pub fn log_path(job: &JobRecord) -> Result<String> {
    let array = job.id.split_once('_');
    let pattern = match (job.stdout.as_str(), array) {
        ("", Some(_)) => "slurm-%A_%a.out",
        ("", None) => "slurm-%j.out",
        (pattern, _) => pattern,
    };
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match (chars.next(), array) {
            (Some('%'), _) => out.push('%'),
            (Some('j' | 'J'), _) => out.push_str(&job.id_raw),
            (Some('x'), _) => out.push_str(&job.name),
            (Some('u'), _) => out.push_str(&job.user),
            (Some('A'), Some((master, _))) => out.push_str(master),
            (Some('a'), Some((_, task))) => out.push_str(task),
            (Some('A'), None) => out.push_str(&job.id_raw),
            (other, _) => bail!(
                "log pattern {pattern:?}: placeholder %{} is not supported",
                other.map(String::from).unwrap_or_default()
            ),
        }
    }
    if out.starts_with('/') {
        return Ok(out);
    }
    if job.workdir.is_empty() {
        bail!(
            "job {} has no working directory in sacct and a relative log path {out:?}",
            job.id
        );
    }
    Ok(format!("{}/{out}", job.workdir.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, stdout: &str) -> JobRecord {
        JobRecord {
            id_raw: "529727".into(),
            id: id.into(),
            name: "stats-s31of32".into(),
            user: "someone".into(),
            workdir: "/work/someone/proj".into(),
            stdout: stdout.into(),
        }
    }

    #[test]
    fn expands_the_placeholders() {
        let cases = [
            (
                "529727",
                "/out/slurm/%x-%j.out",
                "/out/slurm/stats-s31of32-529727.out",
            ),
            (
                "529700_31",
                "/out/%A_%a-%u.log",
                "/out/529700_31-someone.log",
            ),
            (
                "529727",
                "logs/100%%-%j",
                "/work/someone/proj/logs/100%-529727",
            ),
            ("529727", "", "/work/someone/proj/slurm-529727.out"),
            ("529700_31", "", "/work/someone/proj/slurm-529700_31.out"),
        ];
        for (id, pattern, expected) in cases {
            assert_eq!(
                log_path(&record(id, pattern)).unwrap(),
                expected,
                "{pattern:?}"
            );
        }
    }

    #[test]
    fn refuses_an_unknown_placeholder() {
        assert!(log_path(&record("1", "/o/%N.out")).is_err());
        assert!(log_path(&record("1", "/o/trailing%")).is_err());
    }

    #[test]
    fn parses_a_sacct_line() {
        assert!(parse_sacct("").is_err());
        let line = ["529727", "529727", "n", "someone", "/w", "/o/%x-%j.out"].join("\u{1f}");
        let job = parse_sacct(&format!("{line}\n")).unwrap();
        assert_eq!(job.stdout, "/o/%x-%j.out");
        assert!(parse_sacct("529727\u{1f}529727\u{1f}n\n").is_err());
    }

    fn line(id: &str) -> String {
        [id, id, "n", "someone", "/w", "/o/out"].join("\u{1f}")
    }

    #[test]
    fn an_id_that_names_several_jobs_is_an_error_naming_them() {
        let text = format!("{}\n{}\n\n{}\n", line("9_1"), line("9_2"), line("9_3"));
        let err = parse_sacct(&text).unwrap_err().to_string();
        assert!(err.contains("9_1, 9_2, 9_3"), "{err}");
        assert!(parse_sacct(&format!("{}\n\n", line("9_1"))).is_ok());
    }

    #[test]
    fn a_relative_log_path_needs_a_working_directory() {
        let mut job = record("1", "logs/%j.out");
        job.workdir = String::new();
        let err = log_path(&job).unwrap_err().to_string();
        assert!(err.contains("no working directory"), "{err}");
        job.stdout = String::new();
        assert!(log_path(&job).is_err(), "the default name is relative too");
        job.stdout = "/abs/%j.out".into();
        assert_eq!(log_path(&job).unwrap(), "/abs/529727.out");
    }

    #[test]
    fn a_job_name_with_a_pipe_does_not_shift_the_fields() {
        let line = ["1", "1", "a|b|c", "someone", "/w", "/o/out"].join("\u{1f}");
        let job = parse_sacct(&line).unwrap();
        assert_eq!(job.name, "a|b|c");
        assert_eq!(job.workdir, "/w");
        assert_eq!(job.stdout, "/o/out");
    }
}
