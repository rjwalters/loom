//! Unit tests for the `remove` verb's pure parts.
//!
//! The behavioural evidence — the verb end to end against a real repo,
//! including the space-in-path regression the port exists for — lives in
//! `loom-daemon/tests/worktree_remove_verb.rs`; the equivalence evidence is
//! the three retained shell suites run unchanged against the binary.

use super::*;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(std::string::ToString::to_string).collect()
}

// ---------------------------------------------------------------------------
// Argument grammar
// ---------------------------------------------------------------------------

#[test]
fn flags_are_order_independent_and_all_recognised() {
    let a = parse_args(&argv(&["--force", "42", "--json", "--keep-branch", "--dry-run"]))
        .expect("parses");
    assert_eq!(
        a,
        Args {
            issue_number: "42".to_string(),
            keep_branch: true,
            json: true,
            force: true,
            dry_run: true,
        }
    );
}

#[test]
fn short_flags_match_the_shell() {
    let a = parse_args(&argv(&["7", "-f", "-n"])).expect("parses");
    assert!(a.force && a.dry_run);
}

#[test]
fn a_missing_issue_number_is_its_own_error() {
    assert_eq!(parse_args(&argv(&["--force"])), Err(ArgError::Missing));
    assert_eq!(parse_args(&[]), Err(ArgError::Missing));
}

/// The shell rejects anything non-numeric rather than coercing, because the
/// issue number is interpolated straight into the worktree path that is about
/// to be deleted.
#[test]
fn a_non_numeric_issue_number_is_rejected_not_coerced() {
    for bad in ["abc", "42x", "0x2a", "４２", "4 2"] {
        assert!(
            matches!(parse_args(&argv(&[bad])), Err(ArgError::NotNumeric(_))),
            "{bad:?} must be refused as non-numeric"
        );
    }
    // A leading `-` is a flag as far as the shell's `case` is concerned; what
    // matters is only that it never becomes an issue number.
    assert!(parse_args(&argv(&["-1"])).is_err());
}

/// A path traversal in the "issue number" must never reach the worktree-path
/// join. It is caught by the same numeric gate, but it earns its own named
/// test: this is the one input that decides what gets deleted.
#[test]
fn a_traversal_shaped_issue_number_never_reaches_the_path_join() {
    for bad in ["../../etc", "42/../../..", "..", "42/.."] {
        assert!(
            matches!(parse_args(&argv(&[bad])), Err(ArgError::NotNumeric(_))),
            "{bad:?} must never reach the path join"
        );
    }
}

#[test]
fn an_unknown_flag_and_a_second_positional_are_distinct_errors() {
    assert_eq!(
        parse_args(&argv(&["42", "--nope"])),
        Err(ArgError::UnknownFlag("--nope".to_string()))
    );
    assert_eq!(parse_args(&argv(&["42", "43"])), Err(ArgError::Unexpected("43".to_string())));
}

// ---------------------------------------------------------------------------
// The dirty-marker filter (#4449 / #8279)
// ---------------------------------------------------------------------------

/// Loom's own runtime markers are bookkeeping, not work. If they counted, the
/// guard would refuse to remove EVERY managed worktree in a repo whose
/// `.gitignore` predates #3838 — worse than no guard at all.
#[test]
fn loom_runtime_markers_are_not_uncommitted_work() {
    for line in [
        "?? .loom-managed",
        "?? .loom-in-use",
        "?? .loom-checkpoint",
        "?? .no-changes-needed",
        "?? \".loom-managed\"",
    ] {
        assert!(is_marker_line(line), "must be filtered: {line:?}");
    }
}

/// …and a user's file never is, however similarly it is named. The filter can
/// only ever suppress a refusal, so a false positive here is a deletion.
#[test]
fn a_users_file_is_never_mistaken_for_a_marker() {
    for line in [
        "?? wip.txt",
        " M src/main.rs",
        "A  staged.txt",
        "?? .loom-managed.bak",
        "?? my.loom-managed",
        "?? notes/.loom-managed-notes",
    ] {
        assert!(!is_marker_line(line), "must NOT be filtered: {line:?}");
    }
}

/// A marker in a subdirectory is still Loom's, matching the shell twin's
/// `(^|/)\.loom-managed$` anchoring.
#[test]
fn a_marker_in_a_subdirectory_is_still_a_marker() {
    assert!(is_marker_line("?? sub/dir/.loom-in-use"));
}

// ---------------------------------------------------------------------------
// The JSON document
// ---------------------------------------------------------------------------

/// Field names, order and types are a consumer contract:
/// `test-worktree-remove.sh` greps `"success": false` / `"removed": false`,
/// and `test-cargo-target-dir-reclaim.sh` greps
/// `"targetDirStatus": "would-reclaim"` while asserting a single stdout line.
#[test]
fn the_json_document_keeps_the_shells_shape() {
    let args = Args {
        issue_number: "42".to_string(),
        dry_run: true,
        json: true,
        ..Args::default()
    };
    let mut report = Report::new(&args, Path::new("/repo/.loom/worktrees/issue-42"));
    report.branch = Some("feature/issue-42".to_string());
    report.branch_status = "dry-run";
    report.target_dir_status = "would-reclaim";
    report.target_dir_path = "/vol/target/issue-42".to_string();

    let doc = report.render(true, false);
    assert_eq!(doc.lines().count(), 1, "exactly one line");
    let parsed: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
    assert_eq!(parsed["success"], serde_json::json!(true));
    assert_eq!(parsed["issueNumber"], serde_json::json!(42));
    assert_eq!(parsed["removed"], serde_json::json!(false));
    assert_eq!(parsed["dryRun"], serde_json::json!(true));
    assert_eq!(parsed["branch"], serde_json::json!("feature/issue-42"));
    assert_eq!(parsed["branchStatus"], serde_json::json!("dry-run"));
    assert_eq!(parsed["targetDirStatus"], serde_json::json!("would-reclaim"));
    assert_eq!(parsed["targetDir"], serde_json::json!("/vol/target/issue-42"));
    // The shell's spacing is part of what the retained greps match on.
    assert!(doc.contains(r#""success": true"#));
    assert!(doc.contains(r#""removed": false"#));
}

/// A worktree root is operator-supplied (`LOOM_WORKTREE_ROOT`), so a path
/// containing a quote or a backslash must not produce a document no consumer
/// can parse. The shell interpolated these raw.
#[test]
fn a_hostile_path_still_produces_parseable_json() {
    let args = Args {
        issue_number: "9".to_string(),
        json: true,
        ..Args::default()
    };
    let mut report = Report::new(&args, Path::new(r#"/tmp/we"ird\path/issue-9"#));
    report.branch = Some(r#"feature/"quoted""#.to_string());
    let doc = report.render(false, false);
    let parsed: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
    assert_eq!(parsed["worktreePath"], serde_json::json!(r#"/tmp/we"ird\path/issue-9"#));
    assert_eq!(parsed["branch"], serde_json::json!(r#"feature/"quoted""#));
}

// ---------------------------------------------------------------------------
// The cargo-target-dir status mapping
// ---------------------------------------------------------------------------

/// Every [`cargo_target::TargetDirOutcome`] must map to the status token the
/// shell's `_loom_ctd_record` emitted, since `targetDirStatus` is asserted by
/// the retained suite.
#[test]
fn every_target_dir_outcome_maps_to_the_shells_status_token() {
    use cargo_target::TargetDirOutcome as O;
    let p = PathBuf::from("/vol/target");
    let cases: Vec<(O, &str)> = vec![
        (O::Inside(p.clone()), "inside"),
        (O::Absent(p.clone()), "absent"),
        (
            O::Refused {
                path: p.clone(),
                reason: "why".to_string(),
            },
            "refused",
        ),
        (
            O::Shared {
                path: p.clone(),
                by: p.clone(),
            },
            "shared",
        ),
        (
            O::Protected {
                path: p.clone(),
                holders: vec!["pid 1".to_string()],
            },
            "protected",
        ),
        (
            O::WouldReclaim {
                path: p.clone(),
                size_human: "1K".to_string(),
            },
            "would-reclaim",
        ),
        (
            O::Reclaimed {
                path: p.clone(),
                size_human: "1K".to_string(),
            },
            "reclaimed",
        ),
        (
            O::Failed {
                path: p.clone(),
                error: "boom".to_string(),
            },
            "failed",
        ),
    ];
    let args = Args::default();
    let out = Out::new(true);
    for (outcome, expected) in cases {
        let mut report = Report::new(&args, Path::new("/repo/.loom/worktrees/issue-1"));
        report.report_target_dir(&out, &outcome);
        assert_eq!(report.target_dir_status, expected);
        assert_eq!(report.target_dir_path, "/vol/target");
    }
}

/// The status tokens are the shell library's, verbatim. Pinned against the
/// real `lib/cargo-target-dir.sh` so a rename there cannot silently split the
/// vocabulary.
#[test]
fn status_tokens_match_the_shell_twin() {
    let lib =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/lib/cargo-target-dir.sh");
    let Ok(sh) = std::fs::read_to_string(&lib) else {
        return;
    };
    for token in [
        "inside",
        "absent",
        "refused",
        "shared",
        "protected",
        "would-reclaim",
        "reclaimed",
        "failed",
    ] {
        assert!(
            sh.contains(&format!("\"{token}\"")),
            "status token {token:?} missing from {lib:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Ledger attribution
// ---------------------------------------------------------------------------

/// The ledger's `mechanism` must stay the shell's string: `.loom/logs/
/// worktree-removals.log` is read by one `grep`/`jq` across pre- and
/// post-port history, and renaming the mechanism would orphan every existing
/// entry from its successor (#5950).
/// The pin is against `lib/worktree-removal-log.sh`, which enumerates the
/// mechanism vocabulary every writer reports under, and NOT against
/// `worktree.sh` — that script stopped writing the ledger in this slice, so a
/// grep over it would have been a pin on prose that happens to survive.
#[test]
fn the_ledger_mechanism_is_unchanged_from_the_shell() {
    assert_eq!(LEDGER_MECHANISM, "worktree.sh remove");
    assert_eq!(LEDGER_REASON, "explicit_remove");
    let lib = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../defaults/scripts/lib/worktree-removal-log.sh");
    let Ok(text) = std::fs::read_to_string(&lib) else {
        return; // not a full checkout
    };
    assert!(
        text.contains(LEDGER_MECHANISM),
        "{lib:?} no longer lists {LEDGER_MECHANISM:?} among the ledger's mechanisms; \
         renaming it would orphan every existing entry from its successor (#5950)"
    );
}
