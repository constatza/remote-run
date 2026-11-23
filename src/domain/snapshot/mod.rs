//! Snapshot mode: ship the working tree as an immutable, content-addressed directory and run a
//! command in it, with no commit and no push.
//!
//! The order is a type: [`BuiltTree`] (facts git gave) -> [`ClearedTree`] ([`BuiltTree::clear`]
//! against a [`Policy`]) -> [`ShippedSnapshot`] (the upload worked). The scripts and the
//! `git archive` command take the stage they need, so a tree that was not checked cannot be sent
//! and a command cannot run in a snapshot that was never shipped.

mod archive;
mod git;
mod guard;
mod names;
mod provenance;
mod remote;
mod tree;

pub use archive::count_files;
#[cfg(test)]
pub use archive::tar_of;
pub use git::{IndexFile, Repo, toplevel};
pub use guard::{
    BuiltTree, Cleanliness, ClearedTree, DirtyPolicy, Facts, Inclusion, Link, LockFile, Policy,
    Refusal, ShippedSnapshot, SubmodulePolicy, UnsafeLink,
};
pub use names::AllowedPath;
pub use remote::{
    Presence, ROOT, UNSAFE_EXIT, parse_presence, probe_script, run_prelude, stale_script,
    upload_script,
};
pub use tree::{
    Entry, EntryKind, ObjectId, TreeId, lfs_pointer_paths, may_be_lfs_pointer, parse_ls_tree,
    parse_names, parse_not_in_head,
};

/// Which way `rr run` gets code to the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Fast-forward the remote checkout to the local `HEAD`, with its guards.
    Checkout,
    /// Ship the working tree as a snapshot, checked against the policy.
    Snapshot(Policy),
}

/// The default of `--snapshot-limit`: 256 MiB of file content.
pub const DEFAULT_SNAPSHOT_LIMIT: u64 = 256 << 20;
