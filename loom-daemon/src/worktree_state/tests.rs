//! Tests for the agent-work-state primitive and its Stop-hook decision
//! (Issue #8267).
//!
//! The git-backed cases build a throwaway repository per test: the whole point
//! of the feature is what `git status` and `git rev-list` actually say, and a
//! fake would be asserting the mock.

use super::stop_hook::{decide, write_targets_from_str, Decision, HookPayload};
use super::*;

use std::process::Command;

// ---------------------------------------------------------------------------
// Pure classification
// ---------------------------------------------------------------------------

#[test]
fn uncommitted_work_outranks_everything() {
    // The reported incident: commits exist AND are pushed, but a later fix was
    // left uncommitted. That is still a loss.
    assert_eq!(
        classify(3, 1, 0, PushState::Pushed),
        Verdict::Uncommitted,
        "a pushed branch does not make an uncommitted fix safe"
    );
    assert_eq!(classify(0, 0, 6, PushState::Absent), Verdict::Uncommitted);
}

#[test]
fn empty_is_not_a_loss() {
    assert_eq!(classify(0, 0, 0, PushState::Absent), Verdict::Empty);
    assert_eq!(Verdict::Empty.exit_code(), 0);
}

#[test]
fn unknown_push_state_never_reports_as_committed() {
    // "existence treated as evidence of a property" (#8265) is the bug class
    // this issue cites; an unverifiable push must not render as published.
    assert_eq!(classify(2, 0, 0, PushState::Unknown), Verdict::Unpushed);
    assert_eq!(classify(2, 0, 0, PushState::Absent), Verdict::Unpushed);
    assert_eq!(classify(2, 0, 0, PushState::Pushed), Verdict::Committed);
}

#[test]
fn only_uncommitted_sets_a_nonzero_exit() {
    assert_eq!(Verdict::Uncommitted.exit_code(), 3);
    assert_eq!(Verdict::Committed.exit_code(), 0);
    assert_eq!(Verdict::Unpushed.exit_code(), 0);
}

// ---------------------------------------------------------------------------
// Scratch filtering
// ---------------------------------------------------------------------------

#[test]
fn loom_runtime_markers_are_scratch() {
    for p in [
        ".loom-managed",
        ".loom-in-use",
        ".loom-checkpoint",
        ".no-changes-needed",
        "logs/sweep.log",
        "sweep.log",
        ".snapshots/issue-1-2026.patch",
    ] {
        assert!(is_scratch_path(p), "{p} should be scratch");
    }
}

#[test]
fn deliverables_are_not_scratch() {
    for p in [
        "probe_one.sh",
        "loom-daemon/src/worktree_state.rs",
        "docs/analysis.md",
        "tools/.loom-ish-name/real.rs",
        "catalog.json",
    ] {
        assert!(!is_scratch_path(p), "{p} should count as a deliverable");
    }
}

// ---------------------------------------------------------------------------
// Porcelain parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_status_counts_tracked_and_untracked_separately() {
    let raw = "?? probe.sh\0 M src/lib.rs\0A  new.rs\0";
    let (modified, untracked, scratch, paths) = parse_status_z(raw);
    assert_eq!(modified, 2);
    assert_eq!(untracked, 1);
    assert_eq!(scratch, 0);
    assert_eq!(paths, vec!["probe.sh", "src/lib.rs", "new.rs"]);
}

#[test]
fn parse_status_excludes_scratch_but_counts_it() {
    let raw = "?? .no-changes-needed\0?? .loom-managed\0?? run.log\0?? real.rs\0";
    let (modified, untracked, scratch, paths) = parse_status_z(raw);
    assert_eq!(modified, 0);
    assert_eq!(untracked, 1, "only real.rs is a deliverable");
    assert_eq!(scratch, 3);
    assert_eq!(paths, vec!["real.rs"]);
}

#[test]
fn parse_status_handles_paths_with_spaces() {
    // The NUL-delimited form is what makes this correct; a whitespace split
    // would report two files here.
    let raw = "?? my probe script.sh\0";
    let (_, untracked, _, paths) = parse_status_z(raw);
    assert_eq!(untracked, 1);
    assert_eq!(paths, vec!["my probe script.sh"]);
}

#[test]
fn parse_status_consumes_a_rename_origin_field() {
    let raw = "R  new/path.rs\0old/path.rs\0?? other.rs\0";
    let (modified, untracked, _, paths) = parse_status_z(raw);
    assert_eq!(modified, 1, "a rename is one change, not two");
    assert_eq!(untracked, 1);
    assert_eq!(paths, vec!["new/path.rs", "other.rs"]);
}

#[test]
fn at_risk_paths_are_capped() {
    let raw: String = (0..20).map(|i| format!("?? f{i}.rs\0")).collect();
    let (_, untracked, _, paths) = parse_status_z(&raw);
    assert_eq!(untracked, 20);
    assert_eq!(paths.len(), MAX_REPORTED_PATHS);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn render_line_states_both_halves() {
    let state = WorktreeState {
        path: PathBuf::from("/w/issue-1"),
        branch: Some("feature/issue-1".to_string()),
        commits_ahead: 0,
        uncommitted: 0,
        untracked: 6,
        scratch_ignored: 1,
        push_state: PushState::Absent,
        at_risk_paths: vec!["probe.sh".to_string()],
        verdict: Verdict::Uncommitted,
    };
    let line = state.render_line();
    assert!(line.contains("commits_ahead=0"), "{line}");
    assert!(line.contains("untracked=6"), "{line}");
    assert!(line.contains("verdict=uncommitted"), "{line}");
}

// ---------------------------------------------------------------------------
// Stop-hook decision
// ---------------------------------------------------------------------------

fn state_with(commits: u32, uncommitted: u32, untracked: u32) -> WorktreeState {
    let push = if commits > 0 {
        PushState::Pushed
    } else {
        PushState::Absent
    };
    WorktreeState {
        path: PathBuf::from("/w/issue-8267"),
        branch: Some("feature/issue-8267".to_string()),
        commits_ahead: commits,
        uncommitted,
        untracked,
        scratch_ignored: 0,
        push_state: push,
        at_risk_paths: vec!["probe.sh".to_string()],
        verdict: classify(commits, uncommitted, untracked, push),
    }
}

#[test]
fn incident_one_is_blocked_zero_commits_untracked_deliverables() {
    // "branch had zero commits; six probe scripts sat in the scratchpad."
    let decision = decide(&state_with(0, 0, 6), false);
    let Decision::Block(reason) = decision else {
        panic!("expected a block, got {decision:?}");
    };
    assert!(reason.contains("probe.sh"), "the reason names the files");
    assert!(reason.contains(".no-changes-needed"), "it names the escape");
}

#[test]
fn incident_two_is_blocked_committed_but_a_later_fix_left_uncommitted() {
    let decision = decide(&state_with(4, 2, 0), false);
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn a_clean_committed_completion_is_advised_not_blocked() {
    let decision = decide(&state_with(3, 0, 0), false);
    let Decision::Advise(msg) = decision else {
        panic!("expected an advisory, got {decision:?}");
    };
    assert!(msg.contains("3 commit(s) ahead"), "{msg}");
}

#[test]
fn a_deliberate_no_op_session_is_silent() {
    // Nothing committed, nothing at risk: the `.no-changes-needed` shape, which
    // must not produce noise on every such turn.
    assert_eq!(decide(&state_with(0, 0, 0), false), Decision::Silent);
}

#[test]
fn the_guard_blocks_at_most_once_per_stop_sequence() {
    let state = state_with(0, 0, 6);
    assert!(matches!(decide(&state, false), Decision::Block(_)));
    match decide(&state, true) {
        Decision::Advise(msg) => assert!(msg.contains("STILL uncommitted"), "{msg}"),
        other => panic!("a second stop must not block again, got {other:?}"),
    }
}

#[test]
fn block_reason_reports_the_uncounted_remainder_honestly() {
    let mut state = state_with(0, 0, 20);
    state.at_risk_paths = (0..MAX_REPORTED_PATHS)
        .map(|i| format!("f{i}.rs"))
        .collect();
    let Decision::Block(reason) = decide(&state, false) else {
        panic!("expected a block");
    };
    assert!(reason.contains("+12 more"), "{reason}");
}

// ---------------------------------------------------------------------------
// Transcript ownership
// ---------------------------------------------------------------------------

#[test]
fn a_bash_mention_of_a_worktree_is_not_ownership() {
    // The orchestrator's own transcript is full of these. Blocking it for a
    // Builder's uncommitted work would block a turn that cannot fix it.
    let line = r#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"./.loom/scripts/check-main-clean.sh --label issue=42 /repo/.loom/worktrees/issue-42"}}]}}"#;
    assert!(write_targets_from_str(line).is_empty());
}

#[test]
fn an_edit_inside_a_worktree_is_ownership() {
    let line = r#"{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/.loom/worktrees/issue-42/src/lib.rs"}}]}}"#;
    let targets = write_targets_from_str(line);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0], PathBuf::from("/repo/.loom/worktrees/issue-42/src/lib.rs"));
}

#[test]
fn write_targets_keep_transcript_order_and_skip_unrelated_paths() {
    let raw = concat!(
        r#"{"message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/repo/.loom/worktrees/issue-1/a.rs"}}]}}"#,
        "\n",
        r#"{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/README.md"}}]}}"#,
        "\n",
        r#"{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/.loom/worktrees/issue-2/b.rs"}}]}}"#,
        "\n",
        "not json at all\n",
    );
    let targets = write_targets_from_str(raw);
    assert_eq!(targets.len(), 2);
    assert!(targets[1].ends_with("issue-2/b.rs"));
}

// ---------------------------------------------------------------------------
// Against a real git repository
// ---------------------------------------------------------------------------

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = Fixture { dir };
        f.git(&["init", "-q", "-b", "main"]);
        f.git(&["config", "user.email", "t@example.com"]);
        f.git(&["config", "user.name", "Test"]);
        f.write("README.md", "seed\n");
        f.git(&["add", "-A"]);
        f.git(&["commit", "-qm", "seed"]);
        // A stand-in for the branch point: `origin/main` does not exist in a
        // fixture with no remote, and `collect`'s fallback resolves the bare
        // local name, which is exactly the offline-clone path.
        f.git(&["branch", "-q", "work"]);
        f.git(&["checkout", "-q", "work"]);
        std::fs::write(f.path().join(MANAGED_SENTINEL), "").unwrap();
        f
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn state(&self) -> WorktreeState {
        collect(self.path(), "main")
    }
}

#[test]
fn a_worktree_with_an_untracked_deliverable_reports_uncommitted() {
    let f = Fixture::new();
    f.write("probe.sh", "#!/bin/sh\necho hi\n");
    let state = f.state();
    assert_eq!(state.commits_ahead, 0);
    assert_eq!(state.untracked, 1);
    assert_eq!(state.verdict, Verdict::Uncommitted);
    assert_eq!(state.verdict.exit_code(), 3);
    assert!(state.at_risk_paths.iter().any(|p| p == "probe.sh"));
}

#[test]
fn the_same_worktree_reports_clean_once_the_work_is_committed() {
    let f = Fixture::new();
    f.write("probe.sh", "#!/bin/sh\necho hi\n");
    assert_eq!(f.state().verdict, Verdict::Uncommitted, "before");

    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "probe"]);

    let after = f.state();
    assert_eq!(after.commits_ahead, 1);
    assert_eq!(after.uncommitted + after.untracked, 0);
    // No remote in the fixture, so the honest verdict is "unpushed", which is
    // reported but never blocked on.
    assert_eq!(after.verdict, Verdict::Unpushed);
    assert_eq!(after.verdict.exit_code(), 0);
    assert!(matches!(decide(&after, false), Decision::Advise(_)));
}

#[test]
fn a_no_changes_needed_marker_alone_does_not_trip_the_guard() {
    let f = Fixture::new();
    f.write(".no-changes-needed", "already fixed on main\n");
    let state = f.state();
    assert_eq!(state.untracked, 0);
    // Two scratch paths, not one: the marker plus the `.loom-managed` sentinel
    // `worktree.sh` drops into every managed worktree, which must never count
    // as a deliverable either.
    assert_eq!(state.scratch_ignored, 2);
    assert_eq!(state.verdict, Verdict::Empty);
    assert_eq!(decide(&state, false), Decision::Silent);
}

#[test]
fn a_tracked_edit_left_uncommitted_is_caught() {
    let f = Fixture::new();
    f.write("README.md", "seed\nlater fix\n");
    let state = f.state();
    assert_eq!(state.uncommitted, 1);
    assert_eq!(state.verdict, Verdict::Uncommitted);
}

#[test]
fn ownership_resolves_through_the_managed_sentinel() {
    let f = Fixture::new();
    let nested = f.path().join("src/deep/file.rs");
    assert_eq!(
        stop_hook::enclosing_worktree(&nested).as_deref(),
        Some(f.path()),
        "an edit deep inside a managed worktree resolves to the worktree"
    );
}

#[test]
fn an_unmanaged_directory_is_never_owned() {
    let dir = tempfile::tempdir().unwrap();
    let payload = HookPayload {
        cwd: Some(dir.path().display().to_string()),
        ..Default::default()
    };
    assert_eq!(
        stop_hook::owned_worktree(&payload),
        None,
        "the primary checkout legitimately holds operator WIP"
    );
}

#[test]
fn a_managed_cwd_is_owned_without_a_transcript() {
    let f = Fixture::new();
    let payload = HookPayload {
        cwd: Some(f.path().display().to_string()),
        ..Default::default()
    };
    assert_eq!(stop_hook::owned_worktree(&payload).as_deref(), Some(f.path()));
}
