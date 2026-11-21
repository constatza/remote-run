//! The guarded fast-forward of the remote checkout to the commit seen locally.
//!
//! One script reads the remote state, a pure [`decide`] judges it, and only a
//! [`PullDecision::FastForward`] sends the merge. The guards: the remote tree is clean (a job runs
//! what the checkout holds), the commit is on `origin` (code arrives only as a pushed commit), and
//! no job queued or running from the checkout gets its code changed under it (`--force` lifts
//! only that last guard).

use crate::quote::{Place, quote, script};
use crate::remote::Remote;
use anyhow::{Result, bail};
use std::fmt;

/// A full 40-hex commit id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit(String);

impl Commit {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.len() != 40 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("not a full commit id: {text:?}");
        }
        Ok(Self(text.to_ascii_lowercase()))
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0[..12])
    }
}

/// A Slurm job id as `squeue` prints it (`123`, or `123_4` for an array task).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobId(pub String);

/// An active job and the directory it was submitted from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: JobId,
    pub workdir: String,
}

/// What the commit to reach is: the local `HEAD`, or `origin`'s default branch outside a repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Local(Commit),
    OriginHead,
}

impl Target {
    fn revision(&self) -> String {
        match self {
            Target::Local(commit) => commit.0.clone(),
            Target::OriginHead => "origin/HEAD".into(),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Local(commit) => write!(f, "{commit}"),
            Target::OriginHead => f.write_str("origin/HEAD"),
        }
    }
}

/// The remote checkout as read by [`state_script`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteState {
    /// `pwd -P` of the checkout.
    pub dir: String,
    pub head: Commit,
    /// The target resolved after `git fetch origin`; `None` when origin does not have it.
    pub target: Option<Commit>,
    /// `git status --porcelain` lines.
    pub dirty: Vec<String>,
    pub jobs: Vec<Job>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    NotOnOrigin(Target),
    RemoteDirty(Vec<String>),
    JobsFromCheckout(Vec<JobId>),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NotOnOrigin(target) => {
                write!(f, "{target} is not on the remote's origin: push it first")
            }
            Reason::RemoteDirty(lines) => write!(
                f,
                "the remote checkout has uncommitted changes, which a job would run:\n  {}",
                lines.join("\n  ")
            ),
            Reason::JobsFromCheckout(ids) => {
                let ids: Vec<&str> = ids.iter().map(|id| id.0.as_str()).collect();
                write!(
                    f,
                    "jobs {} were submitted from this checkout; a requeue would run the new code \
                     (wait, or --force)",
                    ids.join(", ")
                )
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullDecision {
    UpToDate,
    FastForward { from: Commit, to: Commit },
    Refused(Reason),
}

fn inside(workdir: &str, dir: &str) -> bool {
    workdir == dir || workdir.starts_with(&format!("{}/", dir.trim_end_matches('/')))
}

/// Judge the remote state against the target (pure).
pub fn decide(target: &Target, state: &RemoteState, force: bool) -> PullDecision {
    let Some(to) = &state.target else {
        return PullDecision::Refused(Reason::NotOnOrigin(target.clone()));
    };
    if !state.dirty.is_empty() {
        return PullDecision::Refused(Reason::RemoteDirty(state.dirty.clone()));
    }
    if &state.head == to {
        return PullDecision::UpToDate;
    }
    let blocking: Vec<JobId> = state
        .jobs
        .iter()
        .filter(|job| inside(&job.workdir, &state.dir))
        .map(|job| job.id.clone())
        .collect();
    if !blocking.is_empty() && !force {
        return PullDecision::Refused(Reason::JobsFromCheckout(blocking));
    }
    PullDecision::FastForward {
        from: state.head.clone(),
        to: to.clone(),
    }
}

/// The script that prints the remote state as `key<TAB>value` lines.
pub fn state_script(target: &Target) -> String {
    let revision = quote(&format!("{}^{{commit}}", target.revision()));
    [
        r#"printf 'dir\t%s\n' "$(pwd -P)""#.to_string(),
        r#"printf 'head\t%s\n' "$(git rev-parse HEAD)""#.to_string(),
        "git fetch -q origin".to_string(),
        format!(r#"printf 'target\t%s\n' "$(git rev-parse -q --verify {revision} || true)""#),
        r#"git status --porcelain | sed 's/^/dirty\t/'"#.to_string(),
        r#"squeue --me -h -o '%i %Z' | sed 's/^/job\t/'"#.to_string(),
    ]
    .join("\n")
}

/// Parse the output of [`state_script`] (pure).
pub fn parse_state(text: &str) -> Result<RemoteState> {
    let (mut dir, mut head, mut target) = (None, None, None);
    let (mut dirty, mut jobs) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let Some((key, value)) = line.split_once('\t') else {
            continue;
        };
        match key {
            "dir" => dir = Some(value.to_string()),
            "head" => head = Some(Commit::parse(value)?),
            "target" if value.is_empty() => target = Some(None),
            "target" => target = Some(Some(Commit::parse(value)?)),
            "dirty" => dirty.push(value.to_string()),
            "job" => jobs.push(parse_job(value)?),
            _ => {}
        }
    }
    match (dir, head, target) {
        (Some(dir), Some(head), Some(target)) => Ok(RemoteState {
            dir,
            head,
            target,
            dirty,
            jobs,
        }),
        _ => bail!("incomplete remote state:\n{text}"),
    }
}

fn parse_job(value: &str) -> Result<Job> {
    match value.split_once(' ') {
        Some((id, workdir)) if !id.is_empty() => Ok(Job {
            id: JobId(id.to_string()),
            workdir: workdir.to_string(),
        }),
        _ => bail!("unexpected squeue line: {value:?}"),
    }
}

/// Exit code of [`apply_script`] when the environment does not match `uv.lock`.
pub const STALE_EXIT: i32 = 75;

/// Fast-forward (when `to` is given), then check a uv project's environment without changing it.
pub fn apply_script(to: Option<&Commit>) -> String {
    let mut lines = Vec::new();
    if let Some(to) = to {
        lines.push(format!("git merge -q --ff-only {}", quote(&to.0)));
    }
    lines.push(format!(
        "if [ -f uv.lock ] && ! uv sync --frozen --check -q; then exit {STALE_EXIT}; fi"
    ));
    lines.join("\n")
}

/// How a pull ended.
#[derive(Debug, PartialEq, Eq)]
pub enum PullOutcome {
    UpToDate,
    Advanced {
        from: Commit,
        to: Commit,
    },
    Refused(Reason),
    /// The checkout is at the target, but its environment does not match `uv.lock`.
    Stale,
}

/// Read the state, decide, and apply only what the decision allows.
pub fn pull(
    remote: &dyn Remote,
    place: &Place,
    target: &Target,
    force: bool,
) -> Result<PullOutcome> {
    let state = remote.capture(&script(place, &state_script(target)))?;
    if state.code != 0 {
        bail!(
            "reading the remote checkout failed ({}):\n{}",
            state.code,
            state.stderr.trim()
        );
    }
    let decision = decide(target, &parse_state(&state.stdout)?, force);
    let to = match &decision {
        PullDecision::Refused(reason) => return Ok(PullOutcome::Refused(reason.clone())),
        PullDecision::UpToDate => None,
        PullDecision::FastForward { to, .. } => Some(to),
    };
    let applied = remote.capture(&script(place, &apply_script(to)))?;
    match (applied.code, decision) {
        (STALE_EXIT, _) => Ok(PullOutcome::Stale),
        (0, PullDecision::FastForward { from, to }) => Ok(PullOutcome::Advanced { from, to }),
        (0, _) => Ok(PullOutcome::UpToDate),
        (code, _) => bail!(
            "updating the remote checkout failed ({code}):\n{}",
            applied.stderr.trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Captured;
    use std::cell::RefCell;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn commit(text: &str) -> Commit {
        Commit::parse(text).unwrap()
    }

    fn job(id: &str, workdir: &str) -> Job {
        Job {
            id: JobId(id.into()),
            workdir: workdir.into(),
        }
    }

    fn state(head: &str, target: Option<&str>, dirty: &[&str], jobs: Vec<Job>) -> RemoteState {
        RemoteState {
            dir: "/home/u/piaif".into(),
            head: commit(head),
            target: target.map(commit),
            dirty: dirty.iter().map(|s| s.to_string()).collect(),
            jobs,
        }
    }

    #[test]
    fn decide_table() {
        let target = Target::Local(commit(B));
        let here = || vec![job("7", "/home/u/piaif"), job("8_1", "/home/u/piaif/bin")];
        let elsewhere = || vec![job("9", "/home/u/piaif-runs"), job("10", "/home/u")];
        let ff = PullDecision::FastForward {
            from: commit(A),
            to: commit(B),
        };
        let cases = [
            (
                "up to date",
                state(B, Some(B), &[], here()),
                false,
                PullDecision::UpToDate,
            ),
            (
                "fast-forward",
                state(A, Some(B), &[], vec![]),
                false,
                ff.clone(),
            ),
            (
                "jobs elsewhere",
                state(A, Some(B), &[], elsewhere()),
                false,
                ff.clone(),
            ),
            (
                "not on origin",
                state(A, None, &[], vec![]),
                false,
                PullDecision::Refused(Reason::NotOnOrigin(target.clone())),
            ),
            (
                "dirty",
                state(B, Some(B), &[" M x.py"], vec![]),
                false,
                PullDecision::Refused(Reason::RemoteDirty(vec![" M x.py".into()])),
            ),
            (
                "jobs from checkout",
                state(A, Some(B), &[], here()),
                false,
                PullDecision::Refused(Reason::JobsFromCheckout(vec![
                    JobId("7".into()),
                    JobId("8_1".into()),
                ])),
            ),
            (
                "forced past jobs",
                state(A, Some(B), &[], here()),
                true,
                ff.clone(),
            ),
            (
                "force keeps dirty refused",
                state(A, Some(B), &["?? y"], vec![]),
                true,
                PullDecision::Refused(Reason::RemoteDirty(vec!["?? y".into()])),
            ),
        ];
        for (name, remote, force, expected) in cases {
            assert_eq!(decide(&target, &remote, force), expected, "{name}");
        }
    }

    #[test]
    fn parse_state_reads_every_key() {
        let text = format!(
            "dir\t/home/u/piaif\nhead\t{A}\ntarget\t{B}\ndirty\t M a b.py\njob\t12 /home/u/my dir\n"
        );
        let expected = state(
            A,
            Some(B),
            &[" M a b.py"],
            vec![job("12", "/home/u/my dir")],
        );
        assert_eq!(parse_state(&text).unwrap(), expected);
        let missing = format!("dir\t/home/u/piaif\nhead\t{A}\ntarget\t\n");
        assert_eq!(parse_state(&missing).unwrap().target, None);
        assert!(parse_state("dir\t/x\n").is_err());
    }

    #[test]
    fn commit_rejects_short_and_non_hex() {
        assert!(Commit::parse("abc123").is_err());
        assert!(Commit::parse(&"g".repeat(40)).is_err());
        assert_eq!(
            Commit::parse(&format!("{}\n", A.to_uppercase())).unwrap(),
            commit(A)
        );
    }

    struct Stub {
        replies: RefCell<Vec<Captured>>,
        sent: RefCell<Vec<String>>,
    }

    impl Stub {
        fn new(replies: Vec<(i32, String)>) -> Self {
            let replies = replies
                .into_iter()
                .rev()
                .map(|(code, stdout)| Captured {
                    code,
                    stdout,
                    stderr: String::new(),
                })
                .collect();
            Self {
                replies: RefCell::new(replies),
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl Remote for Stub {
        fn capture(&self, script: &str) -> Result<Captured> {
            self.sent.borrow_mut().push(script.to_string());
            Ok(self
                .replies
                .borrow_mut()
                .pop()
                .expect("an unexpected remote call"))
        }
        fn stream(&self, _script: &str, _tty: bool) -> Result<i32> {
            unreachable!("pull never streams")
        }
    }

    #[test]
    fn a_refused_pull_sends_no_merge() {
        let dirty = format!("dir\t/d\nhead\t{A}\ntarget\t{B}\ndirty\t M x\n");
        let stub = Stub::new(vec![(0, dirty)]);
        let outcome = pull(&stub, &Place::default(), &Target::Local(commit(B)), true).unwrap();
        assert!(matches!(
            outcome,
            PullOutcome::Refused(Reason::RemoteDirty(_))
        ));
        assert_eq!(stub.sent.borrow().len(), 1);
        assert!(!stub.sent.borrow()[0].contains("merge"));
    }

    #[test]
    fn a_fast_forward_merges_the_target_and_reports_a_stale_environment() {
        let clean = format!("dir\t/d\nhead\t{A}\ntarget\t{B}\n");
        let stub = Stub::new(vec![(0, clean), (STALE_EXIT, String::new())]);
        let outcome = pull(&stub, &Place::default(), &Target::Local(commit(B)), false).unwrap();
        assert_eq!(outcome, PullOutcome::Stale);
        assert!(stub.sent.borrow()[1].contains(&format!("git merge -q --ff-only '{B}'")));
    }

    #[test]
    fn up_to_date_still_checks_the_environment_without_merging() {
        let script = apply_script(None);
        assert!(!script.contains("merge"));
        assert!(script.contains("uv sync --frozen --check"));
    }
}
