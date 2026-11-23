//! The pure core: values, parsing, decisions and text. Nothing here starts a process, touches a
//! file, reads the environment, sleeps or prints; that is what keeps every rule testable as data.
//! It imports nothing from `app`, `ports`, `adapters`, `config` or `trust` (test modules may run a
//! real shell through `adapters::OsExec`), and `tests/layout.rs` checks that its code does not
//! name the standard library's I/O.

pub mod argv;
pub mod exit;
pub mod fwd;
pub mod host;
pub mod ids;
pub mod json;
pub mod local;
pub mod logpath;
pub mod outcome;
pub mod place;
pub mod pull;
pub mod recipe;
pub mod sanitize;
pub mod shell;
pub mod slurm;
pub mod ssh;
pub mod status;
pub mod transfer;
pub mod wait;
