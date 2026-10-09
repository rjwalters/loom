//! Unit tests for the guard-hook decisions (issue #10335). The end-to-end
//! wiring through the real hook files is `tests/hooks/test-guard-gh-body-heredoc.sh`
//! and `tests/hooks/test-guard-master-optout.sh`.

use super::*;

/// Mirrors the hook's own `gh\s+pr\s+merge` scan over the masked text.
fn still_scans_merge(command: &str) -> bool {
    let re = Regex::new(r"gh\s+pr\s+merge").unwrap();
    mask_gh_body_heredocs(command)
        .lines()
        .any(|l| re.is_match(l))
}

const M: &str = "gh pr merge";

#[test]
fn quoted_heredoc_fed_directly_to_gh_issue_create_is_masked() {
    let cmd =
        format!("gh issue create --title 'x' --body-file - <<'EOF'\nNever run {M} directly.\nEOF");
    assert!(!still_scans_merge(&cmd));
    assert_eq!(
        mask_gh_body_heredocs(&cmd),
        "gh issue create --title 'x' --body-file - <<'EOF'\n\nEOF",
        "the body line is blanked, not removed"
    );
}

#[test]
fn cat_quoted_heredoc_piped_into_gh_issue_create_is_masked() {
    let cmd = format!(
        "cat <<'EOF' | gh issue create --title x --body-file -\nNever run {M} directly.\nEOF"
    );
    assert!(!still_scans_merge(&cmd));
}

#[test]
fn quoted_heredoc_fed_to_gh_pr_comment_is_masked() {
    let cmd = format!("gh pr comment 12 --body-file - <<'EOF'\nsee {M} rule\nEOF");
    assert!(!still_scans_merge(&cmd));
}

#[test]
fn double_quoted_and_dash_delimiters_are_masked() {
    let cmd = format!("gh pr edit 3 --body-file - <<-\"BODY\"\n\t{M} 1\n\tBODY");
    assert!(!still_scans_merge(&cmd));
}

#[test]
fn a_real_merge_invocation_is_untouched() {
    assert!(still_scans_merge(&format!("{M} 123")));
}

#[test]
fn a_real_merge_after_the_heredoc_is_still_seen() {
    let cmd = format!("gh issue create --title x --body-file - <<'EOF'\nbody\nEOF\n{M} 123");
    assert!(still_scans_merge(&cmd));
}

#[test]
fn an_unquoted_delimiter_masks_nothing() {
    let cmd = format!("gh issue create --title x --body-file - <<EOF\n$({M} 5)\nEOF");
    assert_eq!(mask_gh_body_heredocs(&cmd), cmd);
}

#[test]
fn an_interpreter_heredoc_after_gh_masks_nothing() {
    let cmd = format!("gh issue create --title x; bash <<'EOF'\n{M} 123\nEOF");
    assert_eq!(mask_gh_body_heredocs(&cmd), cmd);
}

#[test]
fn a_pipe_onward_from_gh_masks_nothing() {
    let cmd = format!("cat <<'EOF' | gh issue create --title x --body-file - | bash\n{M} 123\nEOF");
    assert_eq!(mask_gh_body_heredocs(&cmd), cmd);
}

#[test]
fn an_earlier_quoted_line_taints_the_rest() {
    let cmd = format!("echo \"\ngh issue create --body-file - <<'EOF'\n\"; {M} 123\nEOF");
    assert_eq!(mask_gh_body_heredocs(&cmd), cmd);
}

#[test]
fn a_missing_closing_delimiter_masks_nothing() {
    let cmd = format!("gh issue create --body-file - <<'EOF'\n{M} 1");
    assert_eq!(mask_gh_body_heredocs(&cmd), cmd);
}

#[test]
fn a_metacharacter_in_the_double_quoted_arg_masks_nothing() {
    let cmd = format!("gh issue create --title \"$(x)\" --body-file - <<'EOF'\n{M} 1\nEOF");
    assert_eq!(mask_gh_body_heredocs(&cmd), cmd);
}

#[test]
fn a_trailing_newline_is_not_a_line() {
    assert_eq!(mask_gh_body_heredocs("echo hi\n"), "echo hi");
    assert_eq!(mask_gh_body_heredocs(""), "");
}

#[test]
fn delimiter_strips_quotes_dash_and_the_pipe_tail() {
    assert_eq!(delimiter("gh issue create <<'EOF'"), "EOF");
    assert_eq!(delimiter("gh issue create <<- \"EOF\"  "), "EOF");
    assert_eq!(delimiter("cat <<'EOF' | gh issue create --title x"), "EOF");
}

#[test]
#[serial_test::serial]
fn an_empty_root_reads_only_the_env_var() {
    std::env::remove_var("LOOM_GUARDS_ENABLED");
    assert!(!opted_out(Path::new("")));
    std::env::set_var("LOOM_GUARDS_ENABLED", "0");
    assert!(opted_out(Path::new("")));
    std::env::remove_var("LOOM_GUARDS_ENABLED");
}
