//! `session.summary` join-key resolution tests (Issue #9445).

use super::*;
use crate::activity::transcript_parse::ParsedTranscript;
use std::path::PathBuf;
use std::process::Command;

/// A parse carrying only the fields the join keys are derived from.
fn parsed(
    cwd: Option<&str>,
    branch: Option<&str>,
    role: Option<&str>,
    issue: Option<i32>,
) -> ParsedTranscript {
    ParsedTranscript {
        cwd: cwd.map(ToString::to_string),
        branch: branch.map(ToString::to_string),
        role: role.map(ToString::to_string),
        issue,
        ..ParsedTranscript::default()
    }
}

/// `git init` a checkout at `dir` with `origin` pointing at `remote` (when
/// given). Local only — no config writes outside the repo, no network.
fn checkout(dir: &Path, remote: Option<&str>) {
    std::fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    if let Some(remote) = remote {
        git(&["remote", "add", "origin", remote]);
    }
}

// ------------------------------------------------------------------
// Issue attribution
// ------------------------------------------------------------------

#[test]
fn an_issue_worktree_cwd_names_its_issue() {
    assert_eq!(
        issue_from_cwd("/home/ubuntu/GitHub/loom/.loom/worktrees/issue-9445"),
        Some(9445)
    );
    // A cwd below the worktree root still resolves.
    assert_eq!(
        issue_from_cwd("/home/ubuntu/GitHub/loom/.loom/worktrees/issue-42/dashboard/src"),
        Some(42)
    );
    // The `.claude/worktrees/` layout `guard-hooks.md` also recognises.
    assert_eq!(issue_from_cwd("/srv/repo/.claude/worktrees/issue-7"), Some(7));
    // Not a worktree, and not a number.
    assert_eq!(issue_from_cwd("/home/ubuntu/GitHub/loom"), None);
    assert_eq!(issue_from_cwd("/home/ubuntu/GitHub/loom/.loom/worktrees/scratch"), None);
    assert_eq!(issue_from_cwd("/home/ubuntu/issue-42"), None, "issue-N alone is not a worktree");
    assert_eq!(issue_from_cwd(""), None);
}

#[test]
fn a_feature_branch_names_its_issue() {
    assert_eq!(issue_from_branch("feature/issue-9445"), Some(9445));
    // A real branch on this repo: trailing slug after the number.
    assert_eq!(issue_from_branch("feature/issue-9447-install-merge"), Some(9447));
    assert_eq!(issue_from_branch("robb/feature/issue-42"), Some(42));
    assert_eq!(issue_from_branch("main"), None);
    assert_eq!(issue_from_branch("feature/issue-abc"), None);
    assert_eq!(issue_from_branch("feature/issue-42abc"), None, "digits must end at a boundary");
    assert_eq!(
        issue_from_branch("fix/issue-42"),
        None,
        "only feature/ branches are the convention"
    );
}

#[test]
fn the_slash_command_argument_wins_over_cwd_and_branch() {
    // A `/loom:sweep 8757` session that happens to run in another issue's
    // worktree reports what it said itself.
    let ctx = SessionContext::derive(&parsed(
        Some("/home/ubuntu/GitHub/loom/.loom/worktrees/issue-42"),
        Some("feature/issue-99"),
        Some("sweep"),
        Some(8757),
    ));
    assert_eq!(ctx.issue, Some(8757));
    assert_eq!(ctx.kind, SessionKind::Sweep);
}

#[test]
fn a_subagent_with_no_command_argument_is_attributed_by_its_worktree() {
    // The dominant #9445 case: a Builder subagent's first user message is a
    // role prompt, not a slash command, so pre-#9445 it carried no issue.
    let ctx = SessionContext::derive(&parsed(
        Some("/home/ubuntu/GitHub/loom/.loom/worktrees/issue-9445"),
        Some("feature/issue-9445"),
        Some("builder"),
        None,
    ));
    assert_eq!(ctx.issue, Some(9445));
    assert_eq!(ctx.kind, SessionKind::Sweep);
}

#[test]
fn a_branch_attributes_a_session_whose_cwd_is_the_workspace_root() {
    let ctx = SessionContext::derive(&parsed(
        Some("/home/ubuntu/GitHub/loom"),
        Some("feature/issue-9445"),
        None,
        None,
    ));
    assert_eq!(ctx.issue, Some(9445));
    assert_eq!(ctx.kind, SessionKind::Sweep);
}

#[test]
fn a_role_tick_with_no_issue_is_role_and_a_bare_session_is_interactive() {
    let tick = SessionContext::derive(&parsed(
        Some("/home/ubuntu/GitHub/loom"),
        Some("main"),
        Some("champion"),
        None,
    ));
    assert_eq!(tick.issue, None);
    assert_eq!(tick.kind, SessionKind::Role, "a support-role tick is not interactive");

    let human =
        SessionContext::derive(&parsed(Some("/home/ubuntu/Downloads"), Some("main"), None, None));
    assert_eq!(human.issue, None);
    assert_eq!(human.kind, SessionKind::Interactive, "excluded deliberately, not by accident");
}

#[test]
fn a_negative_command_argument_is_dropped_not_coerced() {
    let ctx = SessionContext::derive(&parsed(
        Some("/home/ubuntu/GitHub/loom"),
        Some("main"),
        Some("sweep"),
        Some(-3),
    ));
    assert_eq!(ctx.issue, None);
    assert_eq!(ctx.kind, SessionKind::Role);
}

// ------------------------------------------------------------------
// Repo slug resolution
// ------------------------------------------------------------------

/// Issue #9445's named regression: a worktree whose directory name has
/// nothing to do with the repo. The pre-#9445 derivation reported
/// `wood-reward` (the cwd basename); the slug comes from the remote.
#[test]
fn a_worktree_whose_directory_name_is_unrelated_still_resolves_the_repo_slug() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wood-reward");
    checkout(&root, Some("git@github.com:apache/superset.git"));
    let worktree = root.join(".loom/worktrees/issue-9445");
    std::fs::create_dir_all(&worktree).unwrap();

    let ctx = SessionContext::resolve(&parsed(
        Some(worktree.to_str().unwrap()),
        Some("feature/issue-9445"),
        Some("builder"),
        None,
    ));

    assert_eq!(ctx.repo.as_deref(), Some("apache/superset"), "the slug, not the directory name");
    assert!(
        !ctx.repo.as_deref().unwrap().contains("wood-reward"),
        "the cwd basename must never reach the record"
    );
    assert_eq!(ctx.issue, Some(9445));
    assert_eq!(ctx.kind, SessionKind::Sweep);
    // Fail-closed while this pass makes no forge probe.
    assert_eq!(ctx.visibility, RepoVisibility::Private);
}

#[test]
fn a_removed_worktree_falls_back_to_its_workspace_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("the-next-milestone-should-be-a");
    checkout(&root, Some("https://github.com/rjwalters/loom.git"));
    // The worktree the sweep ran in was removed on merge — the primary clone
    // it hung off is still there, and is the same repo.
    let gone = root.join(".loom/worktrees/issue-9445");

    let ctx =
        SessionContext::resolve(&parsed(Some(gone.to_str().unwrap()), None, Some("builder"), None));

    assert_eq!(ctx.repo.as_deref(), Some("rjwalters/loom"));
    assert_eq!(ctx.issue, Some(9445), "the cwd still names the issue even once it is gone");
}

#[test]
fn a_checkout_with_no_remote_omits_the_repo_rather_than_naming_a_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Downloads");
    checkout(&root, None);

    let ctx = SessionContext::resolve(&parsed(root.to_str(), Some("main"), None, None));

    assert_eq!(ctx.repo, None, "omitted, never the basename");
    assert_eq!(ctx.kind, SessionKind::Interactive);
}

#[test]
fn a_cwd_outside_any_checkout_omits_the_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = SessionContext::resolve(&parsed(tmp.path().to_str(), None, None, None));
    assert_eq!(ctx.repo, None);
}

#[test]
fn a_transcript_with_no_cwd_resolves_nothing_and_stays_interactive() {
    let ctx = SessionContext::resolve(&parsed(None, None, None, None));
    assert_eq!(ctx.repo, None);
    assert_eq!(ctx.issue, None);
    assert_eq!(ctx.pr_number, None);
    assert_eq!(ctx.kind, SessionKind::Interactive);
}

// ------------------------------------------------------------------
// PR number
// ------------------------------------------------------------------

fn write_checkpoint(root: &Path, issue: u32, body: &str) -> PathBuf {
    let dir = root.join(".loom/sweep-checkpoint");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("issue-{issue}.json"));
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn the_issues_own_sweep_checkpoint_supplies_the_pr_number() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("agent-afb133cdd702752a5");
    checkout(&root, Some("git@github.com:rjwalters/loom.git"));
    write_checkpoint(
        &root,
        9445,
        r#"{"phase":"builder-done","task_id":"sweep-1","timestamp":"2026-09-29T06:00:00Z","pr_number":9460}"#,
    );
    let worktree = root.join(".loom/worktrees/issue-9445");
    std::fs::create_dir_all(&worktree).unwrap();

    let mut parse = parsed(worktree.to_str(), None, Some("builder"), None);
    // The session began before the checkpoint was written (just now).
    parse.first_timestamp = Some(chrono::Utc::now() - chrono::Duration::hours(1));

    let ctx = SessionContext::resolve(&parse);
    assert_eq!(ctx.issue, Some(9445));
    assert_eq!(ctx.pr_number, Some(9460));
}

#[test]
fn a_checkpoint_predating_the_session_is_not_this_sessions_pr() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("loom-clone");
    checkout(&root, Some("git@github.com:rjwalters/loom.git"));
    write_checkpoint(&root, 9445, r#"{"phase":"merge-done","pr_number":1234}"#);

    let mut parse = parsed(root.to_str(), Some("feature/issue-9445"), Some("builder"), None);
    // A session that started an hour from now cannot be described by a
    // checkpoint written before it — that is an earlier dispatch's PR.
    parse.first_timestamp = Some(chrono::Utc::now() + chrono::Duration::hours(1));

    let ctx = SessionContext::resolve(&parse);
    assert_eq!(ctx.issue, Some(9445));
    assert_eq!(ctx.pr_number, None, "stale checkpoint PRs are not fabricated onto a session");
}

#[test]
fn a_pre_pr_checkpoint_leaves_the_pr_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("loom-clone");
    checkout(&root, Some("git@github.com:rjwalters/loom.git"));
    write_checkpoint(&root, 9445, r#"{"phase":"curator-done","pr_number":null}"#);

    let mut parse = parsed(root.to_str(), Some("feature/issue-9445"), Some("curator"), None);
    parse.first_timestamp = Some(chrono::Utc::now() - chrono::Duration::hours(1));

    assert_eq!(SessionContext::resolve(&parse).pr_number, None);
}
