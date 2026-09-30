use super::*;

fn ctx<'a>(kind: Kind, preserve_check: bool, landed: bool) -> Context<'a> {
    Context {
        kind,
        path: "/repo/.loom/worktrees/issue-42",
        repo_root: "/repo",
        pr_number: "99",
        branch: "feature/issue-42",
        issue_num: "42",
        preserve_check,
        landed,
        landed_verdict: "unknown",
        landed_evidence: "no ancestry proof",
    }
}

#[test]
fn no_preserve_check_default_removes_silently() {
    let (action, lines) = decide(&ctx(Kind::Default, false, false));
    assert_eq!(action, Action::Remove);
    assert!(lines.is_empty());
}

#[test]
fn no_preserve_check_discovered_removes_with_a_note() {
    let (action, lines) = decide(&ctx(Kind::Discovered, false, false));
    assert_eq!(action, Action::Remove);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, Level::Info);
    assert_eq!(
        lines[0].1,
        "Discovered Loom-managed worktree at non-standard path: /repo/.loom/worktrees/issue-42"
    );
}

#[test]
fn no_preserve_check_judge_pr_removes_with_a_note() {
    let (action, lines) = decide(&ctx(Kind::JudgePr, false, false));
    assert_eq!(action, Action::Remove);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, Level::Info);
    assert_eq!(
        lines[0].1,
        "Found co-existing Judge/Doctor review worktree at /repo/.loom/worktrees/issue-42 (PR #99, alongside issue-42 handling above) — removing (#6264)"
    );
}

#[test]
fn preserve_check_and_not_landed_preserves_with_two_lines() {
    let (action, lines) = decide(&ctx(Kind::Default, true, false));
    assert_eq!(action, Action::Preserve);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].0, Level::Warning);
    assert_eq!(
        lines[0].1,
        "Preserving worktree at /repo/.loom/worktrees/issue-42 — issue #42 is not a close target of PR #99, its live state is not CLOSED, and branch 'feature/issue-42' has not landed (unknown/no ancestry proof) — it carries content the default branch does not have"
    );
    assert_eq!(lines[1].0, Level::Info);
    assert_eq!(
        lines[1].1,
        "This may be the partial-increment case (#3667) awaiting a future closing merge, or an issue-state lookup failure — cleanup retries automatically on a merge that closes #42. If #42 is a programme issue designed never to close (#6694), that retry never fires: remove manually with 'git -C \"/repo\" worktree remove \"/repo/.loom/worktrees/issue-42\" --force && git -C \"/repo\" branch -D feature/issue-42'"
    );
}

#[test]
fn preserve_check_and_landed_removes_with_one_line() {
    let (action, lines) = decide(&ctx(Kind::Default, true, true));
    assert_eq!(action, Action::Remove);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, Level::Info);
    assert_eq!(
        lines[0].1,
        "Issue #42 is not a close target of PR #99 (partial-increment case, #3667), but branch 'feature/issue-42' has already landed (no ancestry proof) — its content is already on the default branch, so the worktree holds nothing unmerged; removing it (#6694)"
    );
}

#[test]
fn discovered_kind_uses_its_own_noun_in_both_6694_messages() {
    let (action, lines) = decide(&ctx(Kind::Discovered, true, true));
    assert_eq!(action, Action::Remove);
    assert!(lines[0]
        .1
        .contains("the discovered worktree holds nothing unmerged"));

    let (action, lines) = decide(&ctx(Kind::Discovered, true, false));
    assert_eq!(action, Action::Preserve);
    assert!(lines[0].1.starts_with("Preserving discovered worktree at"));
}

#[test]
fn judge_pr_kind_uses_its_own_noun_in_both_6694_messages() {
    let (action, lines) = decide(&ctx(Kind::JudgePr, true, true));
    assert_eq!(action, Action::Remove);
    assert!(lines[0]
        .1
        .contains("the Judge/Doctor review worktree holds nothing unmerged"));

    let (action, lines) = decide(&ctx(Kind::JudgePr, true, false));
    assert_eq!(action, Action::Preserve);
    assert!(lines[0]
        .1
        .starts_with("Preserving Judge/Doctor review worktree at"));
}

#[test]
fn render_emits_the_action_token_then_one_tab_led_line_per_record() {
    let rendered = render(
        Action::Preserve,
        &[
            (Level::Warning, "w".to_string()),
            (Level::Info, "i".to_string()),
        ],
    );
    assert_eq!(rendered, "PRESERVE\nWARNING\tw\nINFO\ti\n");
}

#[test]
fn render_with_no_lines_is_just_the_action_token() {
    assert_eq!(render(Action::Remove, &[]), "REMOVE\n");
}
