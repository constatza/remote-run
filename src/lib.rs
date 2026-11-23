//! `rr`: run commands, fast-forward a checkout and copy files on a remote Slurm cluster over the system `ssh`.
//!
//! Dependencies point inward only:
//!
//! - [`domain`] is the functional core. It is pure: no `std::{fs, process, env, thread, time}`, no
//!   printing, and it imports nothing from the layers below.
//! - [`ports`] declares what the outside world offers (`Exec`, `Remote`, `Notes`), in the core's
//!   vocabulary.
//! - [`app`] is one function per command. It imports `domain` and `ports`, never `adapters`, and
//!   returns an `Outcome` instead of printing.
//! - [`adapters`] implement the ports with real processes. [`config`] and [`trust`] are the two
//!   edge modules that read and write files.
//! - `main.rs` wires adapters into the app, renders the outcome, prints it and exits.

pub mod adapters;
pub mod app;
pub mod config;
pub mod domain;
pub mod ports;
pub mod trust;
