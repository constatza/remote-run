//! `.rr/provenance.json` in a snapshot: what it was made from. A `Serialize` struct, never a
//! format string, so no field can break the document.

use super::guard::{Cleanliness, ClearedTree, LockFile};
use serde::Serialize;

/// The provenance of the first upload of a tree. A tree that is shipped again later (from another
/// base commit, say) is not uploaded again, so the file keeps what the first upload recorded.
#[derive(Serialize)]
struct Provenance<'a> {
    tree: &'a str,
    base_commit: &'a str,
    dirty: Cleanliness,
    /// The git blob id of `uv.lock`, when the tree has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    uv_lock_blob: Option<&'a str>,
    rr_version: &'static str,
}

/// The provenance document of `tree`, as one line of JSON.
pub fn provenance_json(tree: &ClearedTree) -> serde_json::Result<String> {
    let document = Provenance {
        tree: tree.id().as_str(),
        base_commit: tree.base().as_str(),
        dirty: tree.cleanliness(),
        uv_lock_blob: match tree.lock() {
            LockFile::Absent => None,
            LockFile::Blob(oid) => Some(oid.as_str()),
        },
        rr_version: env!("CARGO_PKG_VERSION"),
    };
    serde_json::to_string(&document)
}
