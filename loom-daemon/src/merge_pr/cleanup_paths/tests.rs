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
        "LOOM-CLEANUP-PATHS\t42\t/repo/.loom/worktrees/issue-42\t/repo/.loom/worktrees/pr-1234\n"
    );
}

#[test]
fn render_leaves_the_absent_fields_empty() {
    let got = render(&plan("hotfix", "7", &root())).expect("renderable");
    assert_eq!(got, "LOOM-CLEANUP-PATHS\t\t/repo/.loom/worktrees/pr-7\t\n");
    // Exactly three separators, so the shell's `read -r a b c` fills all three
    // names (the last one empty) rather than folding two fields into one.
    assert_eq!(got.matches('\t').count(), 3);
}

#[test]
fn render_refuses_a_root_that_would_break_the_framing() {
    for bad in ["/tmp/a\tb", "/tmp/a\nb"] {
        let p = plan("feature/issue-1", "2", Path::new(bad));
        assert_eq!(render(&p), None, "root {bad:?} must not render");
    }
}
