//! `rr`: run, submit, watch and copy on a remote Slurm cluster over the system `ssh`.
//!
//! The pure core (quoting, the pull decision, log paths, transfer limits) lives in these modules;
//! `main.rs` is the thin shell that talks to `ssh` and `rsync`.

pub mod config;
pub mod logpath;
pub mod pull;
pub mod quote;
pub mod remote;
pub mod transfer;
