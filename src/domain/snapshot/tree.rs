//! What git says about a tree: its id and its entries, parsed from `git ls-tree -r -l -z`.

use crate::domain::ids::HexId;
use anyhow::{Context, Result, bail};
use std::fmt;

/// A git object id (a blob here), 40 hex digits. A repository that uses SHA-256 object ids fails
/// here, closed, with the id's text in the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectId(HexId<40>);

impl ObjectId {
    pub fn parse(text: &str) -> Result<Self> {
        HexId::parse("object id", text).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// The id of a git tree: what names a snapshot. A type of its own, so a commit id cannot be passed
/// where a tree is meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeId(HexId<40>);

impl TreeId {
    pub fn parse(text: &str) -> Result<Self> {
        HexId::parse("tree id", text).map(Self)
    }

    /// All 40 digits: the directory name on the remote host.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// The leading digits only, as people write ids.
impl fmt::Display for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What kind of thing a tree entry is. A closed set: a mode git adds later is an error, not a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Executable,
    Symlink,
    /// A gitlink (mode 160000): another repository, whose content the archive does not hold.
    Submodule,
}

/// One entry of a tree (never a directory: the listing is recursive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub oid: ObjectId,
    /// Bytes of the blob; 0 for a submodule.
    pub size: u64,
    pub path: String,
}

impl Entry {
    /// Whether the archive holds a file (or a link) for this entry.
    pub fn is_archived_file(&self) -> bool {
        match self.kind {
            EntryKind::Regular | EntryKind::Executable | EntryKind::Symlink => true,
            EntryKind::Submodule => false,
        }
    }
}

fn parse_kind(mode: &str) -> Result<EntryKind> {
    match mode {
        "100644" => Ok(EntryKind::Regular),
        "100755" => Ok(EntryKind::Executable),
        "120000" => Ok(EntryKind::Symlink),
        "160000" => Ok(EntryKind::Submodule),
        other => bail!("unknown git file mode {other:?}"),
    }
}

fn parse_entry(record: &str) -> Result<Entry> {
    let (meta, path) = record
        .split_once('\t')
        .with_context(|| format!("git ls-tree printed a line without a path: {record:?}"))?;
    let fields: Vec<&str> = meta.split_whitespace().collect();
    let [mode, _type, oid, size] = fields[..] else {
        bail!("git ls-tree printed an unreadable line: {meta:?}");
    };
    let kind = parse_kind(mode)?;
    let size = match (kind, size) {
        (EntryKind::Submodule, "-") => 0,
        (_, digits) => digits
            .parse()
            .with_context(|| format!("not a size in git ls-tree: {digits:?}"))?,
    };
    Ok(Entry {
        kind,
        oid: ObjectId::parse(oid)?,
        size,
        path: path.to_string(),
    })
}

/// The entries of `git ls-tree -r -l -z --full-tree` output (pure). Any line that cannot be read is
/// an error: a file left out of the list would escape every guard.
pub fn parse_ls_tree(stdout: &str) -> Result<Vec<Entry>> {
    stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(parse_entry)
        .collect()
}

/// The names in `-z` output (NUL-terminated), without empty ones.
pub fn parse_names(stdout: &str) -> Vec<String> {
    stdout
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// The paths a working tree has that `HEAD` does not, from `git status --porcelain=v1 -z`: the
/// untracked ones (`??`) and the new names of staged additions, renames and copies. `git add -u`
/// leaves exactly these out of the snapshot.
pub fn parse_not_in_head(status_z: &str) -> Vec<String> {
    let mut fields = status_z.split('\0').filter(|field| !field.is_empty());
    let mut found = Vec::new();
    while let Some(field) = fields.next() {
        let (code, path) = field.split_at(field.len().min(2));
        let path = path.strip_prefix(' ').unwrap_or(path);
        let renamed = code.contains(['R', 'C']);
        if renamed {
            // `-z` prints the new name, then the old one as a field of its own
            fields.next();
        }
        if code == "??" || code.contains(['A', 'R', 'C']) {
            found.push(path.to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_tree_id_is_forty_hex_digits_and_shows_twelve() {
        let id = TreeId::parse(&format!("{A}\n")).unwrap();
        assert_eq!(id.as_str(), A);
        assert_eq!(id.to_string(), "aaaaaaaaaaaa");
        assert!(TreeId::parse("abc").is_err());
        assert!(ObjectId::parse(&"z".repeat(40)).is_err());
    }

    #[test]
    fn ls_tree_output_is_read_entry_by_entry_with_odd_names() {
        let text = format!(
            "100644 blob {A}     120\tsrc/a b.py\0100755 blob {A}       0\trun.sh\0\
             120000 blob {A}       5\tlink\0160000 commit {A}       -\tvendor/lib\0"
        );
        let entries = parse_ls_tree(&text).unwrap();
        let shape: Vec<_> = entries
            .iter()
            .map(|e| (e.kind, e.size, e.path.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (EntryKind::Regular, 120, "src/a b.py"),
                (EntryKind::Executable, 0, "run.sh"),
                (EntryKind::Symlink, 5, "link"),
                (EntryKind::Submodule, 0, "vendor/lib"),
            ]
        );
        assert!(parse_ls_tree("").unwrap().is_empty());
    }

    #[test]
    fn a_name_with_a_tab_or_newline_stays_one_entry() {
        let text = format!("100644 blob {A}       1\ta\tb\nc\0");
        assert_eq!(parse_ls_tree(&text).unwrap()[0].path, "a\tb\nc");
    }

    #[test]
    fn an_unreadable_line_is_an_error_never_a_skipped_file() {
        for bad in [
            format!("100644 blob {A} 1 no-tab\0"),
            format!("100666 blob {A} 1\tx\0"),
            format!("100644 blob {A} big\tx\0"),
            "100644 blob xyz 1\tx\0".to_string(),
            format!("100644 blob {A}\tx\0"),
        ] {
            assert!(parse_ls_tree(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_are_split_on_nul() {
        assert_eq!(parse_names("a\0b c\0"), ["a", "b c"]);
        assert!(parse_names("").is_empty());
    }

    #[test]
    fn paths_not_in_head_are_untracked_files_and_the_new_names_of_staged_ones() {
        let status = "?? new.py\0 M changed.py\0A  added.py\0AM both.py\0\
                      R  moved.py\0old.py\0C  copy.py\0orig.py\0 D gone.py\0 A intent.py\0!! ignored\0";
        assert_eq!(
            parse_not_in_head(status),
            [
                "new.py",
                "added.py",
                "both.py",
                "moved.py",
                "copy.py",
                "intent.py"
            ]
        );
        assert!(parse_not_in_head("").is_empty());
        // the field after a rename is its old name, whatever it looks like
        assert_eq!(parse_not_in_head("R  b\0?? old\0"), ["b"]);
    }
}
