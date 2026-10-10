//! Tree invariant for Epic #9908 Phase 2a (#11345): the shared journal core
//! has **no production caller**. No runtime producer is migrated or wired,
//! so no existing state or read path can change by this slice landing.
//!
//! Every `src/` file other than the journal module itself and its CLI seam
//! (`cli/journal_cli.rs`) must not name `crate::journal` /
//! `loom_daemon::journal`. A later slice (2b-2e) that wires a producer
//! extends [`ALLOWED`] deliberately, in review.
//!
//! A Rust test, not a `scripts/check-*.sh`, per the shell-language policy.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Paths (relative to the crate root) allowed to reference the core.
const ALLOWED: &[&str] = &["src/journal/", "src/cli/journal_cli.rs"];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn nothing_outside_the_core_and_its_cli_references_the_journal_core() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let reference = Regex::new(r"\b(?:crate|loom_daemon)::journal(?:[^A-Za-z0-9_]|$)").unwrap();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    assert!(files.len() > 100, "scan found the source tree");
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|allowed| relative.starts_with(allowed)) {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            if reference.is_match(line) {
                offenders.push(format!("{relative}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the journal core is wired into production code (Phase 2a forbids it):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_crate_root_only_declares_the_module() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(lib.lines().any(|l| l.trim() == "pub mod journal;"));
    // The CLI seam is the only consumer `main.rs` names.
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    let mentions: Vec<&str> = main.lines().filter(|l| l.contains("journal_cli")).collect();
    assert_eq!(mentions.len(), 2, "one variant + one dispatch arm: {mentions:?}");
}
