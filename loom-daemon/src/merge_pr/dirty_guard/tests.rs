//! Unit tests for the #5031 data-loss guard's port.
//!
//! The retained suite (`test-merge-pr-dirty-worktree-guard.sh`) exercises this
//! through real git repos and real worktree removals, which is the right way to
//! prove the guard still stops a `--force`. These tests cover what that cannot
//! reach cheaply: the exact porcelain shapes, including the two the retired
//! `grep -vE` got wrong, so a Rust-only regression fails without a shell suite
//! in the loop.

use super::*;

/// Porcelain line for an untracked path, as `git status --porcelain` writes it.
fn untracked(path: &str) -> String {
    format!("?? {path}")
}

// --- the marker filter -----------------------------------------------------

#[test]
fn all_four_loom_runtime_markers_are_bookkeeping_not_work() {
    for marker in [
        ".loom-managed",
        ".loom-in-use",
        ".loom-checkpoint",
        ".no-changes-needed",
    ] {
        assert!(
            is_loom_runtime_marker_line(&untracked(marker)),
            "{marker} must not count as user work"
        );
        // A tracked, modified marker is Loom's file either way — the retired
        // `[ /]…$` regex filtered this shape too.
        assert!(is_loom_runtime_marker_line(&format!(" M {marker}")));
    }
}

#[test]
fn a_marker_in_a_subdirectory_is_recognised() {
    // The retired regex's `[ /]` prefix class covered this; the port's
    // final-component test must too.
    assert!(is_loom_runtime_marker_line(&untracked("sub/.loom-managed")));
    assert!(is_loom_runtime_marker_line(&untracked("a/b/.loom-in-use")));
}

#[test]
fn a_user_file_merely_ending_in_a_marker_name_is_still_work() {
    // `x.loom-managed` was never a marker: the retired regex required a space
    // or `/` immediately before the dot, and the port requires the whole final
    // component to match.
    assert!(!is_loom_runtime_marker_line(&untracked("x.loom-managed")));
    assert!(!is_loom_runtime_marker_line(&untracked("docs/about-loom-managed")));
}

#[test]
fn the_snapshots_wip_directory_is_bookkeeping_but_a_file_named_snapshots_is_not() {
    assert!(is_loom_runtime_marker_line(&untracked(".snapshots/issue-1-123.patch")));
    // The collapsed untracked-directory line git emits without `-uall`.
    assert!(is_loom_runtime_marker_line(&untracked(".snapshots/")));
    assert!(is_loom_runtime_marker_line(&untracked("sub/.snapshots/x")));
    // Required a trailing `/` then; requires a non-final component now.
    assert!(!is_loom_runtime_marker_line(&untracked(".snapshots")));
    assert!(!is_loom_runtime_marker_line(&untracked("a.snapshots/x")));
}

#[test]
fn divergence_a_a_rename_into_a_marker_name_is_real_work() {
    // The retired whole-line regex `[ /]\.loom-managed$` matched this line and
    // dropped it, so a rename that was the ONLY dirt left the guard seeing a
    // clean worktree and force-removing a tracked-file change.
    let line = "R  src/app.rs -> .loom-managed";
    assert!(
        !is_loom_runtime_marker_line(line),
        "a rename is a tracked-file change and must survive as dirt"
    );
    assert_eq!(user_dirt(line), vec![line]);
    assert!(has_real_work(&user_dirt(line)));
}

#[test]
fn divergence_b_a_marker_under_a_quoted_path_is_recognised() {
    // `git status` quotes a path containing non-ASCII bytes; the trailing quote
    // defeated the retired `$` anchor, so Loom's own breadcrumb counted as
    // work and blocked cleanup forever.
    let line = r#"?? "sub\303\251/.loom-managed""#;
    assert!(is_loom_runtime_marker_line(line));
    assert!(user_dirt(line).is_empty());
}

#[test]
fn blank_and_whitespace_only_lines_are_dropped() {
    assert!(user_dirt("").is_empty());
    assert!(user_dirt("\n\n").is_empty());
    assert!(user_dirt("   \n\t\n").is_empty());
    // Unicode whitespace is NOT POSIX `[[:space:]]` in the C locale, so a line
    // of it is kept — the retired filter kept it too.
    assert_eq!(user_dirt("\u{a0}"), vec!["\u{a0}"]);
}

#[test]
fn a_line_too_short_to_have_a_path_field_is_kept_as_dirt() {
    // Unclassifiable input must never authorize a `--force`.
    assert_eq!(user_dirt("ab"), vec!["ab"]);
    assert!(has_real_work(&user_dirt("ab")));
}

// --- the #5658 classification ---------------------------------------------

#[test]
fn lockfile_shaped_paths_are_artifact_churn() {
    for path in [
        "Cargo.lock",
        "yarn.lock",
        "package-lock.json",
        "some-lib-lock.json",
        "deep/nested/pnpm.lock",
    ] {
        assert_eq!(
            classify(&untracked(path)),
            Churn::Artifact,
            "{path} should read as artifact churn"
        );
    }
}

#[test]
fn everything_else_is_real_work_including_lockfiles_the_retired_globs_never_covered() {
    for path in [
        "README.md",
        "new_module.py",
        // `pnpm-lock.yaml` matches neither `*.lock` nor `*-lock.json`. The
        // retired globs did not cover it and this port does not widen them:
        // the only thing the classification changes is which advisory sentence
        // prints, and widening it belongs in #5658's own follow-up, not in a
        // port whose job is to not change behaviour.
        "pnpm-lock.yaml",
        ".lock/keep",
    ] {
        assert_eq!(classify(&untracked(path)), Churn::RealWork, "{path} should read as real work");
    }
}

#[test]
fn a_rename_is_classified_by_its_destination() {
    assert_eq!(classify("R  a.lock -> b.rs"), Churn::RealWork);
    assert_eq!(classify("R  b.rs -> a.lock"), Churn::Artifact);
    // `${p##* -> }` is greedy: the LAST arrow wins.
    assert_eq!(classify("R  a -> b -> c.lock"), Churn::Artifact);
}

#[test]
fn mixed_dirt_reports_real_work() {
    let dirt = user_dirt(" M package-lock.json\n M README.md\n");
    assert_eq!(dirt.len(), 2);
    assert!(
        has_real_work(&dirt),
        "a real edit beside a regenerated lockfile is still a live sibling's work"
    );
}

#[test]
fn lockfile_only_dirt_reports_no_real_work() {
    let dirt = user_dirt("A  some-lib-lock.json\n");
    assert_eq!(dirt.len(), 1);
    assert!(!has_real_work(&dirt));
}

// --- the assembled verdict -------------------------------------------------

fn ctx<'a>(branch: &'a str) -> Context<'a> {
    Context {
        worktree_path: "/tmp/wt",
        repo_root: "/tmp/repo",
        branch,
    }
}

#[test]
fn a_clean_worktree_produces_no_refusal() {
    assert!(assess(&ctx("feature/issue-1"), "").is_none());
    assert!(assess(&ctx("feature/issue-1"), "?? .loom-managed\n").is_none());
    assert!(assess(
        &ctx("feature/issue-1"),
        "?? .loom-managed\n?? .loom-in-use\n?? .loom-checkpoint\n?? .no-changes-needed\n?? .snapshots/\n"
    )
    .is_none());
}

#[test]
fn the_refusal_reproduces_the_retired_shell_output_in_order() {
    let records = assess(&ctx("feature/issue-5001a"), " M README.md\n").expect("refusal");
    let rendered: Vec<(&str, &str)> = records
        .iter()
        .map(|(l, m)| (l.token(), m.as_str()))
        .collect();
    assert_eq!(
        rendered,
        vec![
            (
                "WARNING",
                "Refusing to remove worktree at /tmp/wt — it has uncommitted changes on branch \
'feature/issue-5001a' (data-loss guard, #5031):"
            ),
            ("WARNING", " M README.md"),
            (
                "WARNING",
                "A different, still-live builder session likely shares this branch name \
(cross-host duplicate dispatch). Leaving it in place so that work is not lost."
            ),
            ("WARNING", "Remove it manually once those changes are saved/committed:"),
            ("PLAIN", "  git -C \"/tmp/repo\" worktree remove \"/tmp/wt\" --force"),
        ]
    );
}

#[test]
fn an_unresolved_branch_omits_the_on_branch_clause() {
    let records = assess(&ctx(""), " M README.md\n").expect("refusal");
    assert_eq!(
        records[0].1,
        "Refusing to remove worktree at /tmp/wt — it has uncommitted changes (data-loss guard, \
#5031):"
    );
}

#[test]
fn artifact_only_dirt_suppresses_the_cross_host_hypothesis_but_still_refuses() {
    let records = assess(&ctx("feature/issue-5658a"), "A  some-lib-lock.json\n").expect("refusal");
    let joined = records
        .iter()
        .map(|(_, m)| m.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Refusing to remove worktree"));
    assert!(joined.contains("some-lib-lock.json"));
    assert!(!joined.contains("cross-host duplicate dispatch"));
    assert!(joined.contains("environment/artifact churn"));
}

#[test]
fn every_dirty_line_is_quoted_verbatim_and_in_order() {
    let porcelain = " M README.md\n?? new_module.py\n?? .loom-managed\nA  x-lock.json\n";
    let records = assess(&ctx("b"), porcelain).expect("refusal");
    let quoted: Vec<&str> = records[1..4].iter().map(|(_, m)| m.as_str()).collect();
    assert_eq!(
        quoted,
        vec![" M README.md", "?? new_module.py", "A  x-lock.json"],
        "the markers are filtered; everything else is shown exactly as git reported it"
    );
}

#[test]
fn a_dirty_line_is_never_reshaped_on_its_way_into_a_record() {
    // Porcelain quotes control characters (`core.quotePath`), so a literal tab
    // inside a path does not actually reach the protocol — git writes `\t`
    // inside quotes instead. Pinned anyway: whatever git hands over is carried
    // through unaltered, so the record's text is always exactly the line the
    // operator would have seen from `git status` itself.
    let records = assess(&ctx("b"), "?? has\tliteral\ttabs.rs\n").expect("refusal");
    assert_eq!(records[1].1, "?? has\tliteral\ttabs.rs");
    assert_eq!(records[1].0, Level::Warning);
    let quoted = assess(&ctx("b"), r#"?? "has\ttabs.rs""#).expect("refusal");
    assert_eq!(quoted[1].1, r#"?? "has\ttabs.rs""#);
}
