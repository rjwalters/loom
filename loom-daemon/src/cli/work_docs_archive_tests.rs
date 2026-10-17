//! Tests for `work-docs archive|check` (issue #12633).

use std::fs;
use std::path::Path;

use chrono::NaiveDate;

use super::*;
use crate::cli::whole_cli_parse::try_parse_cli;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 17).unwrap()
}

const PREAMBLE: &str = "# Work Log\n\nHand-written intro.\n\n<!-- keep me -->\n\n";

fn section(date: &str, n: u32) -> String {
    format!(
        "### {date}\n- **PR #{n}**: title {n}\n- **Issue #{}** (closed): bug {n}\n\n",
        n + 1000
    )
}

/// A log spanning three months: two old (Jul, Aug) and one recent (Oct).
fn sample() -> String {
    let mut s = String::from(PREAMBLE);
    s.push_str(&section("2026-10-16", 9));
    s.push_str("### 2026-08-09 — Residual gap notice\nHand-written note.\n\n");
    s.push_str(&section("2026-08-10", 8));
    s.push_str(&section("2026-08-10", 7));
    s.push_str(&section("2026-07-02", 6));
    s
}

fn setup(log: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("WORK_LOG.md"), log).unwrap();
    dir
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn run(root: &Path) -> Result<String> {
    archive(root, 60, today(), false)
}

#[test]
fn rotates_multi_month_log_and_conserves() {
    let dir = setup(&sample());
    let root = dir.path();
    run(root).unwrap();

    let active = read(root, "WORK_LOG.md");
    assert!(active.contains("### 2026-10-16\n"));
    assert!(!active.contains("2026-08-10"));
    assert!(!active.contains("2026-07-02"));
    assert!(
        active.contains("## Archives\n\n- [2026-08](work_log/WORK_LOG_2026-08.md)\n- [2026-07]")
    );

    // Moved byte-for-byte, original relative order kept.
    let aug = read(root, "work_log/WORK_LOG_2026-08.md");
    assert!(aug.ends_with(&format!("{}{}", section("2026-08-10", 8), section("2026-08-10", 7))));
    let jul = read(root, "work_log/WORK_LOG_2026-07.md");
    assert!(jul.ends_with(&section("2026-07-02", 6)));

    let report = check(&RootArgs {
        root: root.to_path_buf(),
    })
    .unwrap();
    assert!(report.contains("8 entries unique"), "{report}");
}

#[test]
fn second_run_is_byte_identical_noop() {
    let dir = setup(&sample());
    let root = dir.path();
    run(root).unwrap();
    let snapshot: Vec<String> = [
        "WORK_LOG.md",
        "work_log/WORK_LOG_2026-08.md",
        "work_log/WORK_LOG_2026-07.md",
    ]
    .iter()
    .map(|p| read(root, p))
    .collect();
    run(root).unwrap();
    let again: Vec<String> = [
        "WORK_LOG.md",
        "work_log/WORK_LOG_2026-08.md",
        "work_log/WORK_LOG_2026-07.md",
    ]
    .iter()
    .map(|p| read(root, p))
    .collect();
    assert_eq!(snapshot, again);
}

#[test]
fn preamble_and_handwritten_content_untouched() {
    let dir = setup(&sample());
    run(dir.path()).unwrap();
    let active = read(dir.path(), "WORK_LOG.md");
    assert!(active.starts_with(PREAMBLE));
    assert!(active.contains("### 2026-08-09 — Residual gap notice\nHand-written note.\n\n"));
}

#[test]
fn overlapping_archive_date_fails_and_writes_nothing() {
    let dir = setup(&sample());
    let root = dir.path();
    fs::create_dir(root.join("work_log")).unwrap();
    let existing = format!("# Work Log Archive — 2026-08\n\n{}", section("2026-08-10", 1));
    fs::write(root.join("work_log/WORK_LOG_2026-08.md"), &existing).unwrap();
    let before = read(root, "WORK_LOG.md");

    let err = run(root).unwrap_err().to_string();
    assert!(err.contains("conflict"), "{err}");
    assert_eq!(read(root, "WORK_LOG.md"), before);
    assert_eq!(read(root, "work_log/WORK_LOG_2026-08.md"), existing);
    assert!(!root.join("work_log/WORK_LOG_2026-07.md").exists());
}

#[test]
fn older_nonoverlapping_archive_gets_appended_in_date_order() {
    let dir = setup(&sample());
    let root = dir.path();
    fs::create_dir(root.join("work_log")).unwrap();
    let older = section("2026-08-03", 1);
    fs::write(
        root.join("work_log/WORK_LOG_2026-08.md"),
        format!("# Work Log Archive — 2026-08\n\n{older}"),
    )
    .unwrap();
    run(root).unwrap();
    let aug = read(root, "work_log/WORK_LOG_2026-08.md");
    let new = aug.find("### 2026-08-10").unwrap();
    let old = aug.find("### 2026-08-03").unwrap();
    assert!(new < old, "newest first:\n{aug}");
    assert!(aug.starts_with("# Work Log Archive — 2026-08\n\n"));
    check(&RootArgs {
        root: root.to_path_buf(),
    })
    .unwrap();
}

#[test]
fn floor_is_enforced() {
    let dir = setup(&sample());
    let err = archive(dir.path(), 44, today(), false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("minimum 45"), "{err}");
    assert!(archive(dir.path(), 45, today(), true).is_ok());
}

#[test]
fn log_without_date_headings_is_a_noop() {
    let text = "# Work Log\n\nnothing dated here\n### 2026-08-09 — not a date\n";
    let dir = setup(text);
    let msg = run(dir.path()).unwrap();
    assert!(msg.contains("nothing to do"), "{msg}");
    assert_eq!(read(dir.path(), "WORK_LOG.md"), text);
    assert!(!dir.path().join("work_log").exists());
}

#[test]
fn archive_path_that_is_not_a_regular_file_fails() {
    let dir = setup(&sample());
    let root = dir.path();
    fs::create_dir_all(root.join("work_log/WORK_LOG_2026-08.md")).unwrap();
    let before = read(root, "WORK_LOG.md");
    // A directory named like an archive is skipped by the scan only if
    // unreadable as a file; it must surface as an error instead.
    let err = run(root).unwrap_err().to_string();
    assert!(err.contains("not a regular file"), "{err}");
    assert_eq!(read(root, "WORK_LOG.md"), before);
}

#[test]
fn bullet_text_mentioning_pr_does_not_confuse_counter() {
    let mut s = String::from(PREAMBLE);
    s.push_str("### 2026-07-02\n- **PR #5**: fix handling of `- **PR #123**` text\n  - **PR #123** continuation\n- plain bullet mentioning PR #123 and Issue #9\n\n");
    let dir = setup(&s);
    run(dir.path()).unwrap();
    let jul = read(dir.path(), "work_log/WORK_LOG_2026-07.md");
    assert!(jul.contains("fix handling of"));
    let report = check(&RootArgs {
        root: dir.path().to_path_buf(),
    })
    .unwrap();
    assert!(report.contains("1 entries unique"), "{report}");
    assert_eq!(bullet_key("- **PR #5**: x"), Some("PR #5".into()));
    assert_eq!(bullet_key("  - **PR #123** x"), None);
    assert_eq!(bullet_key("- **PR #12 x"), None);
}

#[test]
fn duplicate_entry_fails_check() {
    let mut s = String::from(PREAMBLE);
    s.push_str(&section("2026-10-16", 9));
    s.push_str(&section("2026-10-15", 9));
    let dir = setup(&s);
    assert!(check(&RootArgs {
        root: dir.path().to_path_buf()
    })
    .is_err());
}

#[test]
fn dry_run_writes_nothing() {
    let dir = setup(&sample());
    let before = read(dir.path(), "WORK_LOG.md");
    let msg = archive(dir.path(), 60, today(), true).unwrap();
    assert!(msg.starts_with("dry run:"), "{msg}");
    assert_eq!(read(dir.path(), "WORK_LOG.md"), before);
    assert!(!dir.path().join("work_log").exists());
}

#[test]
fn cli_parses_and_defaults_to_sixty_days() {
    let Ok(cli) = try_parse_cli(&["loom-daemon", "work-docs", "archive", "--dry-run"]) else {
        panic!("did not parse");
    };
    match cli.command {
        Some(crate::Commands::ScriptPorts(
            crate::cli::script_ports::ScriptPortCommand::WorkDocs(WorkDocsCommand::Archive(a)),
        )) => {
            assert_eq!(a.older_than_days, 60);
            assert!(a.dry_run);
        }
        _ => panic!("parsed to another command"),
    }
}
