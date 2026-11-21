//! The log file of a job: `sacct` stores `--output` unexpanded (`%x-%j.out`), so it is expanded here.

use anyhow::{Result, bail};

/// The `sacct -P` fields [`parse_sacct`] reads, in this order.
pub const SACCT_FIELDS: &str = "JobIDRaw,JobID,JobName,User,WorkDir,StdOut";

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

/// Parse the first line of `sacct -n -X -P -o` [`SACCT_FIELDS`] (pure).
pub fn parse_sacct(text: &str) -> Result<JobRecord> {
    let Some(line) = text.lines().find(|line| !line.trim().is_empty()) else {
        bail!("sacct knows no such job");
    };
    let fields: Vec<&str> = line.splitn(6, '|').collect();
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
        Ok(out)
    } else {
        Ok(format!("{}/{out}", job.workdir.trim_end_matches('/')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, stdout: &str) -> JobRecord {
        JobRecord {
            id_raw: "529727".into(),
            id: id.into(),
            name: "ustutt-stats-s31of32".into(),
            user: "kat".into(),
            workdir: "/home/kat/piaif".into(),
            stdout: stdout.into(),
        }
    }

    #[test]
    fn expands_the_placeholders() {
        let cases = [
            (
                "529727",
                "/out/slurm/%x-%j.out",
                "/out/slurm/ustutt-stats-s31of32-529727.out",
            ),
            ("529700_31", "/out/%A_%a-%u.log", "/out/529700_31-kat.log"),
            (
                "529727",
                "logs/100%%-%j",
                "/home/kat/piaif/logs/100%-529727",
            ),
            ("529727", "", "/home/kat/piaif/slurm-529727.out"),
            ("529700_31", "", "/home/kat/piaif/slurm-529700_31.out"),
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
        let job = parse_sacct("529727|529727|n|kat|/w|/o/%x-%j.out\n").unwrap();
        assert_eq!(job.stdout, "/o/%x-%j.out");
        assert!(parse_sacct("529727|529727|n\n").is_err());
    }
}
