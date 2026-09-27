//! Unit tests for the version-policy guard's pure classification. The real
//! git/checker wiring is exercised by
//! `tests/merge_pr_version_policy_differential.rs` (against the retired
//! shell) and by the retained `test-merge-pr-defaults-version-bump-collision.sh`
//! suite (through `merge-pr.sh`'s stub).

use super::*;

fn inputs(dry_run: bool) -> Inputs<'static> {
    Inputs {
        repo_root: Path::new("/nonexistent"),
        default_branch: "main",
        branch: "feature/issue-1",
        head_sha: "headsha",
        pr_number: "42",
        dry_run,
    }
}

#[test]
fn checker_exit_zero_passes_and_keeps_earlier_warnings() {
    let r = classify(&inputs(false), vec!["w".into()], "base", "ref", 0, "");
    assert_eq!(r.verdict, Verdict::Pass);
    assert_eq!(r.warnings, vec!["w".to_string()]);
}

#[test]
fn checker_exit_one_blocks_with_the_checker_output_and_the_oracle_named() {
    let r = classify(&inputs(false), vec![], "base", "the PR head (h)", 1, "VERSION: a -> b");
    let Verdict::Block(msg) = r.verdict else {
        panic!("exit 1 must block");
    };
    assert!(msg.starts_with("Merge blocked: PR #42 hand-edits a version-bearing value (#7827)."));
    assert!(msg.contains("\n\nVERSION: a -> b\n\n"));
    assert!(msg.ends_with("(Checker from the PR head (h).)"));
    // The remedy must never recommend the bump the policy forbids.
    assert!(!msg.contains("version.sh bump"));
}

#[test]
fn dry_run_reports_the_would_be_block_without_refusing() {
    let r = classify(&inputs(true), vec![], "base", "'main' (base)", 1, "out");
    assert_eq!(r.verdict, Verdict::Pass);
    assert_eq!(
        r.warnings,
        vec![
            "[dry-run] Would BLOCK merge of PR #42: forbidden version edit relative to 'main' (base), per the checker from 'main' (base)."
                .to_string()
        ]
    );
}

#[test]
fn any_other_exit_is_a_guard_fault_that_skips_never_blocks() {
    for rc in [2, 126, 127, 130, 255] {
        let r = classify(&inputs(false), vec![], "base", "ref", rc, "boom");
        assert_eq!(r.verdict, Verdict::Pass, "rc {rc} must not block");
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains(&format!("exited {rc} against current 'main' (base)")));
        assert!(r.warnings[0].ends_with(":\nboom"));
    }
}

#[test]
fn a_dry_run_guard_fault_is_still_a_fault_not_a_would_be_block() {
    let r = classify(&inputs(true), vec![], "base", "ref", 2, "");
    assert_eq!(r.verdict, Verdict::Pass);
    assert!(!r.warnings[0].contains("Would BLOCK"));
}

#[test]
fn the_machinery_warning_keeps_the_retired_text_byte_for_byte() {
    assert_eq!(
        machinery_warning("the PR head (abc)", "main"),
        "Version policy guard: this PR's own commits change the version-policy machinery, so the guard evaluates the checker from the PR head (abc) — the ref CI's defaults-version-bump-check job evaluates (#8284). A head lookup that fails falls back to 'main''s copy, never to skipping the check."
    );
}

#[test]
fn machinery_names_exactly_the_three_files_that_define_version_bearing() {
    assert_eq!(
        MACHINERY,
        &[
            "defaults/scripts/check-defaults-version-bump.sh",
            "defaults/scripts/version-check-gate.sh",
            "scripts/version.sh",
        ]
    );
}

#[test]
fn a_repo_with_no_checker_on_disk_is_a_silent_skip() {
    let r = evaluate(&inputs(false));
    assert_eq!(r, Report::pass(Vec::new()));
}

#[test]
fn command_substitution_strips_only_trailing_newlines() {
    assert_eq!(strip_trailing_newlines("a\n\nb\n\n\n"), "a\n\nb");
    assert_eq!(strip_trailing_newlines(""), "");
}
