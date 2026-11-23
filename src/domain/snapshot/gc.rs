//! `rr gc`: how old a snapshot must be to go, how many of the newest stay, and the report the remote
//! script prints, parsed so that anything it does not understand is an error.

use super::tree::TreeId;
use crate::domain::sanitize::sanitize;
use crate::domain::span::{parse_seconds, show_seconds};
use anyhow::{Context, Result, bail};
use std::fmt;

/// The least `--older-than` accepts. `rr run` touches `.rr-used` when a run starts, and a job
/// takes time to be queued, so a snapshot younger than this may be about to be used.
pub const AGE_FLOOR_SECONDS: u32 = 3600;

/// The most `--older-than` accepts (ten years): a bound that keeps the remote arithmetic small, far
/// beyond what a retention policy needs.
const AGE_CEILING_SECONDS: u32 = 3650 * 86_400;

/// How long a snapshot must have gone unused to be deleted: at least [`AGE_FLOOR_SECONDS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Age(u32);

impl Age {
    /// `90m`, `12h` or `30d`, between one hour and ten years.
    pub fn parse(text: &str) -> Result<Self> {
        let bad = || {
            format!(
                "not an age: {text:?} (use 90m, 12h or 30d: at least {}, at most 3650d)",
                show_seconds(AGE_FLOOR_SECONDS.into())
            )
        };
        let seconds = parse_seconds(text).with_context(bad)?;
        match u32::try_from(seconds) {
            Ok(seconds) if (AGE_FLOOR_SECONDS..=AGE_CEILING_SECONDS).contains(&seconds) => {
                Ok(Self(seconds))
            }
            _ => bail!(bad()),
        }
    }

    pub fn seconds(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&show_seconds(self.0.into()))
    }
}

/// How many of the newest snapshots `--keep` protects, whatever their age.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keep(u32);

impl Keep {
    pub const NONE: Keep = Keep(0);

    /// A count of ASCII digits (`u32::from_str` would also take a leading `+`).
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            bail!("not a number of snapshots to keep: {text:?}");
        }
        text.parse()
            .map(Self)
            .with_context(|| format!("not a number of snapshots to keep: {text:?}"))
    }

    pub fn count(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Keep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The name of something left in the snapshot root by an upload or a clean-up that did not finish:
/// `.tmp-…` (a staging directory) or `.trash-…` (set aside for removal), in the plain characters
/// `mktemp` and `rr` use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leftover(String);

impl Leftover {
    const PREFIXES: [&'static str; 2] = [".tmp-", ".trash-"];

    pub fn parse(text: &str) -> Result<Self> {
        let suffix = Self::PREFIXES
            .iter()
            .find_map(|prefix| text.strip_prefix(prefix))
            .filter(|suffix| !suffix.is_empty());
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        match suffix {
            Some(suffix) if suffix.chars().all(plain) => Ok(Self(text.to_string())),
            _ => bail!("not a snapshot or a leftover of one: {text:?}"),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A directory of the snapshot root that `rr gc` looked at: a snapshot (only a full lowercase tree
/// id names one) or a leftover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootEntry {
    Snapshot(TreeId),
    Leftover(Leftover),
}

impl RootEntry {
    pub fn parse(text: &str) -> Result<Self> {
        match TreeId::parse(text) {
            Ok(id) if id.as_str() == text => Ok(Self::Snapshot(id)),
            _ => Leftover::parse(text).map(Self::Leftover),
        }
    }

    /// The directory name, in full.
    pub fn name(&self) -> &str {
        match self {
            Self::Snapshot(id) => id.as_str(),
            Self::Leftover(leftover) => leftover.as_str(),
        }
    }
}

/// Why an entry was left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// One of the `--keep` newest snapshots.
    Newest,
    /// Used (or, for a leftover, touched) more recently than the age.
    Young,
    /// An active job was submitted from it or from inside it.
    InUse,
    /// A symlink, not a directory, or owned by someone else: never followed or touched.
    NotYours,
    /// A step of its removal failed before anything was deleted.
    Error,
}

impl Why {
    /// The word the remote script prints.
    fn parse(text: &str) -> Result<Self> {
        match text {
            "keep" => Ok(Self::Newest),
            "young" => Ok(Self::Young),
            "in-use" => Ok(Self::InUse),
            "not-yours" => Ok(Self::NotYours),
            "error" => Ok(Self::Error),
            other => bail!("unknown reason {other:?}"),
        }
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Newest => "one of the newest kept by --keep",
            Self::Young => "used or touched more recently than the age",
            Self::InUse => "an active job runs from it",
            Self::NotYours => "a symlink, not a directory, or not yours: not touched",
            Self::Error => "could not be set aside or put back; it may be under .trash-*",
        })
    }
}

/// An entry that stayed, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub entry: RootEntry,
    pub why: Why,
}

/// What a finished clean-up did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub removed: Vec<RootEntry>,
    pub kept: Vec<Kept>,
    /// Set aside under `.trash-…` but the removal itself failed: some of it may be gone. A later
    /// `rr gc` sweeps it.
    pub failed: Vec<RootEntry>,
}

impl GcReport {
    /// The entries a step of removal failed for before anything was deleted: they could not be set
    /// aside (or put back), and stay where they are.
    pub fn errored(&self) -> impl Iterator<Item = &RootEntry> {
        self.kept
            .iter()
            .filter(|kept| kept.why == Why::Error)
            .map(|kept| &kept.entry)
    }
}

/// Why `rr gc` deleted nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcRefusal {
    /// The snapshot root is a symlink, someone else's, not mode 700, or resolves to `/` or `$HOME`.
    RootNotSafe,
    /// `squeue`, `realpath` or `pwd` failed, or a job's directory could not be placed, so whether a
    /// snapshot is in use is unknown.
    QueueUnreadable,
}

impl GcRefusal {
    fn parse(text: &str) -> Result<Self> {
        match text {
            "root" => Ok(Self::RootNotSafe),
            "queue" => Ok(Self::QueueUnreadable),
            other => bail!("unknown refusal {other:?}"),
        }
    }
}

impl fmt::Display for GcRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RootNotSafe => {
                "the snapshot root is not a private directory of yours, or resolves to / or your \
                 home; nothing was deleted"
            }
            Self::QueueUnreadable => {
                "could not tell which snapshots jobs use (squeue or realpath failed, or a job's \
                 directory could not be placed); nothing was deleted"
            }
        })
    }
}

/// How `rr gc` ended on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GcOutcome {
    Cleaned(GcReport),
    Refused(GcRefusal),
    /// Refused after entries were set aside, and some of them could not be put back: nothing was
    /// deleted, but those entries (the `Why::Error` ones in the report) may now exist only as
    /// `.trash-…`. A clean refusal would hide that.
    Stranded {
        why: GcRefusal,
        report: GcReport,
    },
}

/// Exit code of the clean-up script when it refuses; the report says why.
pub const GC_REFUSED_EXIT: i32 = 78;

/// Starts every line of the report, so a login banner is ignored but an unknown prefixed line is
/// an error.
const PREFIX: &str = "rr-gc";

/// One line of the report.
enum Line {
    Removed(RootEntry),
    Kept(Kept),
    Failed(RootEntry),
    Refused(GcRefusal),
    Done,
}

fn parse_line(fields: &[&str]) -> Result<Line> {
    match fields {
        ["removed", name] => Ok(Line::Removed(RootEntry::parse(name)?)),
        ["kept", why, name] => Ok(Line::Kept(Kept {
            entry: RootEntry::parse(name)?,
            why: Why::parse(why)?,
        })),
        ["failed", name] => Ok(Line::Failed(RootEntry::parse(name)?)),
        ["refused", why] => Ok(Line::Refused(GcRefusal::parse(why)?)),
        ["done"] => Ok(Line::Done),
        _ => bail!("unknown line"),
    }
}

/// The report the clean-up script printed. It must end with `done` (a clean-up) or `refused` (a
/// refusal) and name no entry twice; anything else, including a script that stopped half way, is an
/// error.
pub fn parse_report(stdout: &str) -> Result<GcOutcome> {
    let mut report = GcReport::default();
    let mut ended: Option<GcOutcome> = None;
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix(PREFIX).and_then(|r| r.strip_prefix('\t')) else {
            continue;
        };
        let fields: Vec<&str> = rest.split('\t').collect();
        let parsed = parse_line(&fields)
            .with_context(|| format!("unreadable line from the clean-up: {:?}", sanitize(line)))?;
        if ended.is_some() {
            bail!(
                "the clean-up printed a line after its last one: {:?}",
                sanitize(line)
            );
        }
        match parsed {
            Line::Removed(entry) => report.removed.push(entry),
            Line::Kept(kept) => report.kept.push(kept),
            Line::Failed(entry) => report.failed.push(entry),
            Line::Refused(why) => ended = Some(GcOutcome::Refused(why)),
            Line::Done => ended = Some(GcOutcome::Cleaned(std::mem::take(&mut report))),
        }
    }
    let Some(ended) = ended else {
        bail!("the clean-up ended without saying how");
    };
    match ended {
        GcOutcome::Cleaned(report) => {
            check_names_once(&report)?;
            Ok(GcOutcome::Cleaned(report))
        }
        GcOutcome::Refused(why) if report.errored().next().is_some() => {
            check_names_once(&report)?;
            Ok(GcOutcome::Stranded { why, report })
        }
        other => Ok(other),
    }
}

fn check_names_once(report: &GcReport) -> Result<()> {
    let names = report
        .removed
        .iter()
        .chain(&report.failed)
        .chain(report.kept.iter().map(|kept| &kept.entry))
        .map(RootEntry::name);
    let mut seen = std::collections::BTreeSet::new();
    match names.into_iter().find(|name| !seen.insert(*name)) {
        Some(twice) => bail!("the clean-up named {twice} twice"),
        None => Ok(()),
    }
}

/// The lines `rr gc` prints on stdout: one per entry, what became of it, then a total.
pub fn gc_lines(report: &GcReport) -> Vec<String> {
    let shown = |entry: &RootEntry| sanitize(entry.name());
    let mut lines = Vec::new();
    lines.extend(
        report
            .removed
            .iter()
            .map(|e| format!("removed {}", shown(e))),
    );
    lines.extend(
        report
            .kept
            .iter()
            .map(|k| format!("kept {} ({})", shown(&k.entry), k.why)),
    );
    lines.extend(report.failed.iter().map(|e| {
        format!(
            "failed {} (set aside as .trash-*, removal did not finish)",
            shown(e)
        )
    }));
    lines.push(format!(
        "{} removed, {} kept, {} failed",
        report.removed.len(),
        report.kept.len(),
        report.failed.len()
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pull::{Job, Workdir};
    use crate::domain::slurm::SqueueId;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn an_age_is_at_least_an_hour_and_at_most_ten_years() {
        for (text, seconds) in [
            ("1h", 3600),
            ("60m", 3600),
            ("90m", 5400),
            ("30d", 2_592_000),
            ("3650d", 315_360_000),
        ] {
            let age = Age::parse(text).unwrap();
            assert_eq!(age.seconds(), seconds, "{text}");
        }
        for text in [
            "",
            "0s",
            "59m",
            "3599s",
            "1s",
            "1",
            "h",
            "-1h",
            "+2h",
            "2H",
            "1.5h",
            "2h30m",
            "3651d",
            "99999999999999999999d",
            "4294967296s",
            "2é",
            "\u{661}h",
        ] {
            assert!(Age::parse(text).is_err(), "{text:?}");
        }
        assert_eq!(Age::parse("90m").unwrap().to_string(), "90m");
        assert_eq!(Age::parse("48h").unwrap().to_string(), "2d");
    }

    #[test]
    fn keep_is_a_plain_count_and_none_by_default() {
        assert_eq!(Keep::parse("0").unwrap(), Keep::NONE);
        assert_eq!(Keep::parse("12").unwrap().count(), 12);
        assert_eq!(Keep::default(), Keep::NONE);
        for text in ["", "-1", "+1", "1.5", "x", " 1", "4294967296"] {
            assert!(Keep::parse(text).is_err(), "{text:?}");
        }
    }

    /// Only a full lowercase tree id is a snapshot; the leftovers are the two prefixes with a
    /// plain, non-empty rest. Everything else in the root is not `rr gc`'s to read.
    #[test]
    fn an_entry_is_a_full_lowercase_tree_id_or_a_plain_leftover() {
        for (text, is_snapshot) in [
            (A, true),
            (".tmp-ab12CD", false),
            (".trash-x.y_z", false),
            (".trash-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", false),
        ] {
            let entry = RootEntry::parse(text).unwrap();
            assert_eq!(
                matches!(entry, RootEntry::Snapshot(_)),
                is_snapshot,
                "{text}"
            );
            assert_eq!(entry.name(), text);
        }
        let upper = A.to_uppercase();
        let short = &A[..12];
        let long = format!("{A}0");
        let spaced = format!(" {A}");
        let newline = format!("{A}\n");
        for text in [
            "",
            upper.as_str(),
            short,
            long.as_str(),
            spaced.as_str(),
            newline.as_str(),
            ".tmp-",
            ".trash-",
            ".tmp",
            "tmp-x",
            ".tmp-a b",
            ".tmp-a/b",
            ".tmp-é",
            "..",
            ".",
            "/etc",
            "../x",
            ".rr",
            ".rr-used",
            "provenance.json",
        ] {
            assert!(RootEntry::parse(text).is_err(), "{text:?}");
        }
    }

    fn report(lines: &[&str]) -> String {
        lines
            .iter()
            .map(|line| format!("rr-gc\t{line}\n"))
            .collect()
    }

    #[test]
    fn a_report_lists_what_was_removed_kept_and_why() {
        let text = format!(
            "login banner\n{}",
            report(&[
                &format!("kept\tkeep\t{A}"),
                &format!("kept\tin-use\t{B}"),
                "kept\tyoung\t.tmp-abc",
                "kept\tnot-yours\t.trash-x",
                &format!("kept\terror\t{}", "c".repeat(40)),
                &format!("removed\t{}", "d".repeat(40)),
                "removed\t.tmp-old",
                &format!("failed\t{}", "e".repeat(40)),
                "done",
            ])
        );
        let GcOutcome::Cleaned(parsed) = parse_report(&text).unwrap() else {
            panic!("a clean-up");
        };
        let whys: Vec<Why> = parsed.kept.iter().map(|k| k.why).collect();
        assert_eq!(
            whys,
            [
                Why::Newest,
                Why::InUse,
                Why::Young,
                Why::NotYours,
                Why::Error
            ]
        );
        assert_eq!(parsed.removed.len(), 2);
        assert_eq!(parsed.failed.len(), 1);
        assert_eq!(
            gc_lines(&parsed).last().unwrap(),
            "2 removed, 5 kept, 1 failed"
        );
        assert_eq!(
            parse_report(&report(&["done"])).unwrap(),
            GcOutcome::Cleaned(GcReport::default())
        );
    }

    #[test]
    fn a_refusal_names_its_reason() {
        assert_eq!(
            parse_report(&report(&["refused\troot"])).unwrap(),
            GcOutcome::Refused(GcRefusal::RootNotSafe)
        );
        assert_eq!(
            parse_report(&report(&[&format!("kept\tyoung\t{A}"), "refused\tqueue"])).unwrap(),
            GcOutcome::Refused(GcRefusal::QueueUnreadable)
        );
    }

    /// A refusal that follows `kept error` lines is not a clean one: those entries could not be put
    /// back, and the report keeps them (and what else was kept) for the user.
    #[test]
    fn a_refusal_after_an_error_is_stranded_and_keeps_the_report() {
        let text = report(&[
            &format!("kept\tyoung\t{B}"),
            &format!("kept\terror\t{A}"),
            "refused\tqueue",
        ]);
        let GcOutcome::Stranded {
            why,
            report: parsed,
        } = parse_report(&text).unwrap()
        else {
            panic!("a stranded refusal");
        };
        assert_eq!(why, GcRefusal::QueueUnreadable);
        assert_eq!(
            parsed.errored().map(RootEntry::name).collect::<Vec<_>>(),
            [A]
        );
        assert_eq!(parsed.kept.len(), 2);
        let twice = report(&[
            &format!("kept\terror\t{A}"),
            &format!("kept\tyoung\t{A}"),
            "refused\tqueue",
        ]);
        assert!(parse_report(&twice).is_err(), "an entry named twice");
    }

    /// A report that is cut short, contradicts itself or has a line it cannot read is never a
    /// clean-up: the user would be told about deletions that did not happen as described.
    #[test]
    fn anything_unreadable_is_an_error() {
        let long = "z".repeat(40);
        let cases: Vec<String> = vec![
            String::new(),
            "no prefix at all\n".into(),
            report(&[&format!("removed\t{A}")]),
            report(&["done", "done"]),
            report(&["done", &format!("removed\t{A}")]),
            report(&["refused\tqueue", "done"]),
            report(&["refused\tmaybe"]),
            report(&["refused"]),
            report(&[
                &format!("removed\t{A}"),
                &format!("kept\tyoung\t{A}"),
                "done",
            ]),
            report(&[&format!("removed\t{A}"), &format!("removed\t{A}"), "done"]),
            report(&[&format!("kept\tsoon\t{A}"), "done"]),
            report(&[&format!("kept\t{A}"), "done"]),
            report(&[&format!("removed\t{long}"), "done"]),
            report(&["removed\t../etc", "done"]),
            report(&[&format!("removed\t{A}\textra"), "done"]),
            report(&["deleted\tx", "done"]),
            "rr-gcdone\n".into(),
            "rr-gc\n".into(),
        ];
        for text in cases {
            assert!(parse_report(&text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn what_is_shown_is_sanitized() {
        let report = GcReport {
            removed: vec![RootEntry::parse(A).unwrap()],
            ..GcReport::default()
        };
        let shown = gc_lines(&report).join("\n");
        assert!(shown.starts_with(&format!("removed {A}")), "{shown}");
        let err = parse_report("rr-gc\tremoved\t\u{1b}[2J\n").unwrap_err();
        assert!(!format!("{err:#}").contains('\u{1b}'), "{err:#}");
    }

    fn job(dir: &str) -> Job {
        Job {
            id: SqueueId::parse("1").unwrap(),
            workdir: Workdir::Resolved(dir.into()),
        }
    }

    /// The test `rr gc` applies to a job is the pull guard's `inside`: the snapshot itself or
    /// anything below it, and a sibling that only shares the prefix is not inside.
    #[test]
    fn a_job_is_in_a_snapshot_only_when_its_directory_is_the_snapshot_or_below_it() {
        let snap = format!("/p/.rr-snapshots/{A}");
        for (workdir, inside) in [
            (snap.clone(), true),
            (format!("{snap}/src/pkg"), true),
            (format!("{snap}x"), false),
            (format!("{snap}-old"), false),
            (format!("/p/.rr-snapshots/{B}"), false),
            ("/p/.rr-snapshots".to_string(), false),
            ("/p".to_string(), false),
            (format!("/q{snap}"), false),
        ] {
            assert_eq!(job(&workdir).is_inside(&snap), inside, "{workdir}");
        }
        let unplaced = Job {
            id: SqueueId::parse("2").unwrap(),
            workdir: Workdir::Unresolved,
        };
        assert!(
            unplaced.is_inside(&snap),
            "a job that cannot be placed counts as inside"
        );
    }
}
