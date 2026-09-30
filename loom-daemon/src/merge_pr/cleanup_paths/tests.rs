use super::*;

fn root() -> PathBuf {
    PathBuf::from("/repo/.loom/worktrees")
}

#[test]
fn a_loom_issue_branch_names_the_builder_worktree_and_the_review_worktree() {
    let got = plan("feature/issue-42", "1234", &root());
    assert_eq!(got.issue_num.as_deref(), Some("42"));
    assert_eq!(got.default_path, root().join("issue-42"));
    // #6264: the pr-<PR> path is checked ALONGSIDE issue-<N>, not instead of it.
    assert_eq!(got.judge_pr_path, Some(root().join("pr-1234")));
}

#[test]
fn a_non_issue_branch_names_only_the_pr_worktree() {
    let got = plan("fix/some-thing", "77", &root());
    assert_eq!(got.issue_num, None);
    assert_eq!(got.default_path, root().join("pr-77"));
    // Not `Some(pr-77)`: the shell left JUDGE_PR_WT_PATH empty here so the
    // #6264 call site is not a pure duplicate of the default one.
    assert_eq!(got.judge_pr_path, None);
}

#[test]
fn the_strict_pattern_keeps_trailing_number_branches_pr_style() {
    // The named regressions in merge-pr.sh's own comment: a trailing-number
    // heuristic would point cleanup at issue-1 / issue-42 for these.
    for branch in ["release-1", "fix-bug-42", "feature/issue-42-extra", "v2.1"] {
        assert_eq!(issue_number_of(branch), None, "branch {branch:?}");
        assert_eq!(plan(branch, "9", &root()).default_path, root().join("pr-9"));
    }
}

#[test]
fn only_the_exact_anchored_form_matches() {
    assert_eq!(issue_number_of("feature/issue-1"), Some("1"));
    assert_eq!(issue_number_of("feature/issue-12345"), Some("12345"));
    // Prefix must be exact and leading: bash's `^`.
    assert_eq!(issue_number_of("x/feature/issue-1"), None);
    assert_eq!(issue_number_of("Feature/issue-1"), None);
    assert_eq!(issue_number_of("feature/issue--1"), None);
    // Suffix must be exact: bash's `$`, which is end-of-STRING, not
    // end-of-line — the divergence a multiline regex would introduce.
    assert_eq!(issue_number_of("feature/issue-1\n"), None);
    assert_eq!(issue_number_of("feature/issue-1\nfeature/issue-2"), None);
    // `([0-9]+)` needs at least one digit, and admits ASCII digits only.
    assert_eq!(issue_number_of("feature/issue-"), None);
    assert_eq!(issue_number_of("feature/issue-1a"), None);
    assert_eq!(issue_number_of("feature/issue- 1"), None);
    assert_eq!(issue_number_of("feature/issue-١٢"), None); // Arabic-Indic digits
    assert_eq!(issue_number_of("feature/issue-１"), None); // fullwidth digit
}

#[test]
fn the_issue_number_is_the_written_token_not_a_parsed_integer() {
    // `${BASH_REMATCH[1]}` interpolated verbatim: a zero-padded branch names a
    // zero-padded worktree, and re-rendering it as 7 would look elsewhere.
    let got = plan("feature/issue-007", "5", &root());
    assert_eq!(got.issue_num.as_deref(), Some("007"));
    assert_eq!(got.default_path, root().join("issue-007"));
}

#[test]
fn the_pr_number_is_also_taken_as_written() {
    let got = plan("feature/issue-3", "0012", &root());
    assert_eq!(got.judge_pr_path, Some(root().join("pr-0012")));
}

#[test]
fn an_overridden_root_is_used_for_both_paths() {
    let over = PathBuf::from("/Volumes/scratch/wt/loom");
    let got = plan("feature/issue-8", "99", &over);
    assert_eq!(got.default_path, over.join("issue-8"));
    assert_eq!(got.judge_pr_path, Some(over.join("pr-99")));
}

#[test]
fn render_emits_four_tab_separated_fields() {
    let got = render(&plan("feature/issue-42", "1234", &root())).expect("renderable");
    assert_eq!(
        got,
        "LOOM-CLEANUP-PATHS\t/repo/.loom/worktrees/issue-42\t42\t/repo/.loom/worktrees/pr-1234\n"
    );
}

#[test]
fn render_leaves_the_absent_fields_empty_and_trailing() {
    let got = render(&plan("hotfix", "7", &root())).expect("renderable");
    assert_eq!(got, "LOOM-CLEANUP-PATHS\t/repo/.loom/worktrees/pr-7\t\t\n");
    // Exactly three separators, so the shell's `read -r a b c` fills all three
    // names (the last two empty) rather than folding two fields into one.
    assert_eq!(got.matches('\t').count(), 3);
    // The load-bearing half: the FIRST field after the sentinel must never be
    // empty. Tab is IFS whitespace, so bash's `read` strips a leading run of it
    // and an empty leading field is unrecoverable — that shape silently skipped
    // post-merge cleanup for every non-`feature/issue-<N>` branch.
    let after_sentinel = got
        .strip_prefix("LOOM-CLEANUP-PATHS\t")
        .expect("sentinel prefix");
    assert!(
        !after_sentinel.starts_with('\t'),
        "the leading field must be non-empty: {got:?}"
    );
}

#[test]
fn the_leading_field_is_never_empty_for_any_branch_shape() {
    // The invariant the field order rests on, across both sides of the
    // classification and both root styles.
    for branch in [
        "feature/issue-42",
        "feature/issue-007",
        "fix/foo-bar",
        "docs/guide-update-2026-09-30",
        "security/9548-marker-auth",
        "feature/issue-8195-slice-13",
        "release-1",
        "",
    ] {
        for r in [root(), PathBuf::from("/Volumes/scratch/wt/loom")] {
            let line = render(&plan(branch, "777", &r)).expect("renderable");
            let body = line
                .strip_prefix("LOOM-CLEANUP-PATHS\t")
                .expect("sentinel prefix");
            let fields: Vec<&str> = body.trim_end_matches('\n').split('\t').collect();
            assert_eq!(fields.len(), 3, "branch {branch:?}: {line:?}");
            assert!(
                !fields[0].is_empty(),
                "branch {branch:?}: the leading (default-path) field must be non-empty: {line:?}"
            );
        }
    }
}

#[test]
fn the_two_optional_fields_are_always_empty_together() {
    // Why leading with default-path is sufficient rather than merely necessary:
    // the two fields that CAN be empty are empty together, so neither can ever
    // end up leading. #6264's asymmetry is what guarantees this.
    for branch in [
        "feature/issue-1",
        "feature/issue-007",
        "fix/foo-bar",
        "docs/guide-update",
        "release-1",
        "feature/issue-42-extra",
        "",
    ] {
        let p = plan(branch, "9", &root());
        assert_eq!(
            p.issue_num.is_some(),
            p.judge_pr_path.is_some(),
            "branch {branch:?}: issue_num and judge_pr_path must be Some/None together"
        );
    }
}

/// The round-trip the differential test cannot cover: `render`'s bytes fed to
/// the REAL `IFS=$'\t' read -r DEFAULT_WT_PATH ISSUE_NUM JUDGE_PR_WT_PATH` that
/// `merge-pr.sh` uses, asserting bash recovers all three values. This is the gap
/// class the field-order bug lived in — both sides of the differential rendered
/// byte-identical lines, and the framing/parse seam is new surface the port
/// introduced, so only an actual `bash` can falsify it.
#[test]
fn the_rendered_line_round_trips_through_the_shells_read() {
    let cases: &[(&str, &str, &str, &str)] = &[
        // (branch, pr, expected DEFAULT_WT_PATH, expected ISSUE_NUM)
        ("feature/issue-42", "1234", "/repo/.loom/worktrees/issue-42", "42"),
        ("fix/foo-bar", "777", "/repo/.loom/worktrees/pr-777", ""),
        ("docs/guide-update-2026-09-30", "9", "/repo/.loom/worktrees/pr-9", ""),
        ("feature/issue-8195-slice-13", "88", "/repo/.loom/worktrees/pr-88", ""),
    ];
    for (branch, pr, want_default, want_issue) in cases {
        let line = render(&plan(branch, pr, &root())).expect("renderable");
        let want_judge = if want_issue.is_empty() {
            String::new()
        } else {
            format!("/repo/.loom/worktrees/pr-{pr}")
        };
        // Verbatim from merge-pr.sh's cleanup block, including the sentinel strip.
        let script = r#"set -uo pipefail
_CP_OUT="$1"
IFS=$'\t' read -r DEFAULT_WT_PATH ISSUE_NUM JUDGE_PR_WT_PATH <<<"${_CP_OUT#*$'\t'}"
printf 'D=[%s] I=[%s] J=[%s]\n' "$DEFAULT_WT_PATH" "$ISSUE_NUM" "$JUDGE_PR_WT_PATH"
"#;
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .arg("bash")
            .arg(line.trim_end_matches('\n'))
            .output()
            .expect("run bash");
        assert!(out.status.success(), "bash failed for branch {branch:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            format!("D=[{want_default}] I=[{want_issue}] J=[{want_judge}]"),
            "branch {branch:?}: the shell's read did not recover the rendered fields \
             (line was {line:?})"
        );
    }
}

#[test]
fn render_refuses_a_root_that_would_break_the_framing() {
    for bad in ["/tmp/a\tb", "/tmp/a\nb"] {
        let p = plan("feature/issue-1", "2", Path::new(bad));
        assert_eq!(render(&p), None, "root {bad:?} must not render");
    }
}
