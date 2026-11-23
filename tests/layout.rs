//! Rules about where code may live, checked on the source text so a violation fails `cargo test`
//! even where clippy was told to look away.
#![allow(clippy::disallowed_methods)] // reading the source tree is this test's job

use std::fs;
use std::path::{Path, PathBuf};

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("entry is readable").path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

fn all_sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    sources(&root, &mut found);
    assert!(!found.is_empty());
    found
}

/// Printing is `main`'s job alone: everything else returns data.
#[test]
fn only_main_may_allow_printing() {
    for path in all_sources() {
        if path.file_name().is_some_and(|name| name == "main.rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("source is readable");
        for lint in ["print_stdout", "print_stderr"] {
            let allowed = text
                .lines()
                .any(|line| line.contains("allow(") && line.contains(lint));
            assert!(!allowed, "{} allows {lint}", path.display());
        }
    }
}

/// A mode is an enum, not a flag: no parameter or field of non-test code is a `bool` (a predicate
/// may return one).
#[test]
fn no_parameter_or_field_is_a_bool() {
    for path in all_sources() {
        let text = fs::read_to_string(&path).expect("source is readable");
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        assert!(
            !production.contains(": bool"),
            "{} has a bool parameter or field",
            path.display()
        );
    }
}

/// What the pure core must not name: the machine, and the layers that touch it.
const FORBIDDEN_IN_DOMAIN: [&str; 14] = [
    "std::process",
    "std::fs",
    "std::env",
    "std::thread",
    "std::time",
    "println!",
    "eprintln!",
    "print!(",
    "crate::app",
    "crate::ports",
    "crate::adapters",
    "crate::config",
    "crate::trust",
    "tempfile",
];

/// The forbidden names in the code of a domain file; a test module may build real files and run a
/// real shell, so only what precedes `#[cfg(test)]` counts.
fn domain_violations(text: &str) -> Vec<&'static str> {
    let code = text.split("#[cfg(test)]").next().unwrap_or_default();
    FORBIDDEN_IN_DOMAIN
        .into_iter()
        .filter(|name| code.contains(name))
        .collect()
}

#[test]
fn the_domain_neither_touches_the_machine_nor_imports_the_layers_that_do() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(files.len() > 10, "the domain directory moved");
    for path in files {
        let text = fs::read_to_string(&path).expect("source is readable");
        let found = domain_violations(&text);
        assert!(found.is_empty(), "{} names {found:?}", path.display());
    }
}

#[test]
fn the_domain_check_sees_what_it_forbids() {
    let bad = "use std::process::Command;\nfn f() { println!(\"x\"); }\n#[cfg(test)]\nmod t {}";
    assert_eq!(domain_violations(bad), ["std::process", "println!"]);
    let in_tests_only = "fn pure() {}\n#[cfg(test)]\nmod t { use std::fs; }";
    assert!(domain_violations(in_tests_only).is_empty());
    assert_eq!(
        domain_violations("use crate::ports::Exec;"),
        ["crate::ports"]
    );
}
