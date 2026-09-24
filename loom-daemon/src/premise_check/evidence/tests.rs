//! Anchor extraction and the scan. The scan half needs a real git tree, so it
//! builds a tiny one rather than reaching for the repo it happens to run in —
//! a test that greps its own checkout passes or fails on unrelated commits.

use super::*;
use std::fs;
use std::process::Command;

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn backticked_paths_that_exist_become_path_anchors() {
    let dir = root();
    fs::create_dir_all(dir.path().join("loom-daemon/src/watchdog")).unwrap();
    fs::write(dir.path().join("loom-daemon/src/watchdog/mod.rs"), "x").unwrap();
    let a = anchors("t", "see `loom-daemon/src/watchdog/mod.rs` line 902", dir.path());
    assert!(a.contains(&"loom-daemon/src/watchdog/mod.rs".to_string()), "{a:?}");
}

/// #8310's defect: the cited path had moved. The basename is still a useful
/// anchor, so a stale citation degrades to a search rather than to nothing.
#[test]
fn a_stale_path_citation_degrades_to_its_basename() {
    let dir = root();
    let a = anchors(
        "t",
        "see `defaults/scripts/cli/loom-daemon-watchdog.sh` lines 91-96",
        dir.path(),
    );
    assert!(a.contains(&"loom-daemon-watchdog.sh".to_string()), "{a:?}");
}

#[test]
fn shouty_tokens_are_anchors() {
    let dir = root();
    let a = anchors(
        "watchdog never restarted it",
        "logged `[DIVERGENCE] daemon IPC UNRESPONSIVE (CONFIRMED)` at 10:44:59Z",
        dir.path(),
    );
    for want in ["DIVERGENCE", "UNRESPONSIVE", "CONFIRMED"] {
        assert!(a.contains(&want.to_string()), "{want} missing from {a:?}");
    }
}

#[test]
fn stopwords_and_short_tokens_are_dropped() {
    let dir = root();
    let a = anchors("t", "the `daemon` had an `error` in `cfg`", dir.path());
    assert!(a.is_empty(), "{a:?}");
}

#[test]
fn anchors_are_deduplicated_case_insensitively() {
    let dir = root();
    let a = anchors("t", "`watchdog` and WATCHDOG and `watchdog`", dir.path());
    assert_eq!(a.len(), 1, "{a:?}");
}

// ---- the scan --------------------------------------------------------------

fn git_tree() -> tempfile::TempDir {
    let dir = root();
    let p = dir.path();
    let run = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(p)
            .args(args)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    fs::create_dir_all(p.join("src")).unwrap();
    fs::write(
        p.join("src/watchdog.rs"),
        // The intent assertion and the anchor are 2 lines apart.
        "fn confirm() {\n    // CONFIRMED hang\n    // No automatic kill/restart is attempted.\n}\n",
    )
    .unwrap();
    fs::write(
        p.join("src/faraway.rs"),
        format!(
            "// deliberately keeps the old name\n{}// CONFIRMED appears far below\n",
            "//\n".repeat(200)
        ),
    )
    .unwrap();
    fs::write(p.join("src/quiet.rs"), "// CONFIRMED, with no intent claim\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "seed"]);
    dir
}

#[test]
fn scan_reports_an_intent_line_near_an_anchor() {
    let dir = git_tree();
    let found = scan(dir.path(), &["CONFIRMED".to_string()], 8);
    assert!(found.iter().any(|c| c.path == "src/watchdog.rs"), "{found:?}");
}

#[test]
fn scan_drops_a_co_occurrence_that_is_only_whole_file() {
    let dir = git_tree();
    let found = scan(dir.path(), &["CONFIRMED".to_string()], 8);
    assert!(
        !found.iter().any(|c| c.path == "src/faraway.rs"),
        "far-apart co-occurrence should not be reported: {found:?}"
    );
}

#[test]
fn scan_ignores_a_file_with_no_intent_marker() {
    let dir = git_tree();
    let found = scan(dir.path(), &["CONFIRMED".to_string()], 8);
    assert!(!found.iter().any(|c| c.path == "src/quiet.rs"), "{found:?}");
}

#[test]
fn a_cited_path_waives_the_proximity_window() {
    let dir = git_tree();
    let found = scan(dir.path(), &["src/faraway.rs".to_string()], 8);
    assert!(found.iter().any(|c| c.path == "src/faraway.rs" && c.cited), "{found:?}");
}

#[test]
fn scan_is_bounded_by_its_limit() {
    let dir = git_tree();
    assert!(scan(dir.path(), &["CONFIRMED".to_string()], 0).is_empty());
    assert!(scan(dir.path(), &[], 8).is_empty());
}

/// The scan is advisory, so a root that is not a git checkout at all must be a
/// quiet empty result and never an error the caller could mistake for a
/// verdict.
#[test]
fn scan_on_a_non_repo_is_empty_not_an_error() {
    let dir = root();
    assert!(scan(dir.path(), &["CONFIRMED".to_string()], 8).is_empty());
}

#[test]
fn snippets_are_one_squeezed_line() {
    let dir = git_tree();
    let found = scan(dir.path(), &["CONFIRMED".to_string()], 8);
    for c in &found {
        assert!(!c.snippet.contains('\n'), "{c:?}");
        assert!(!c.snippet.contains("  "), "{c:?}");
        assert!(c.snippet.chars().count() <= 160, "{c:?}");
    }
}
