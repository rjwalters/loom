//! Shared work-liveness predicate for the review-stall (#3910) and
//! stale-sweep (#7529) watchdogs (Issue #9533).
//!
//! The sweep log's mtime is **not** a liveness signal: a headless
//! (`LOOM_HEADLESS_SESSION=1`, `claude -p`) sweep writes nothing to
//! `.loom/logs/sweep-issue-N.log` while it works, so a healthy sweep that is
//! compiling or waiting on the model looks identical to a dead one. The
//! session's own transcript (`~/.claude/projects/<slug>/<session>.jsonl`, plus
//! `subagents/*.jsonl`) *is* appended throughout, so the idle time a watchdog
//! judges is the **minimum** over every signal we can read.
//!
//! Every signal is optional. Unreadable/missing signals are skipped, and when
//! none is readable the result is `None` — "cannot assess" — never a stall.

use std::path::Path;
use std::time::Duration;

use crate::transcript_tokens::{
    head_names_sweep_issue, project_slug, read_head, session_transcripts,
};

fn idle_of(path: &Path) -> Option<Duration> {
    // `elapsed()` errors on a future mtime (clock skew): map to None.
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
}

/// Smallest of the readable idle times; `None` when none is readable.
#[must_use]
pub(crate) fn min_idle(signals: impl IntoIterator<Item = Option<Duration>>) -> Option<Duration> {
    signals.into_iter().flatten().min()
}

/// Idle time of the freshest transcript belonging to `issue`'s own
/// `/loom:sweep` session(s) (parent + subagent files) under
/// `projects_dir/<slug(workspace_root)>`, and of any transcript in the issue
/// worktree's own project dir (subagents whose cwd is the worktree).
///
/// Only files modified within `within` are inspected, which bounds the head
/// reads; a file older than that cannot change a Healthy verdict.
#[must_use]
pub(crate) fn transcript_idle(
    projects_dir: &Path,
    workspace_root: &Path,
    issue: u32,
    within: Duration,
) -> Option<Duration> {
    let mut best: Option<Duration> = None;
    let mut consider = |d: Option<Duration>| best = min_idle([best, d]);

    let project = projects_dir.join(project_slug(workspace_root));
    if let Ok(entries) = std::fs::read_dir(&project) {
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            // The parent session's mtime gates the (more expensive) head read.
            let parent_idle = idle_of(&path);
            let subagent_idle = min_idle(
                session_transcripts(&path)
                    .iter()
                    .skip(1)
                    .map(|p| idle_of(p)),
            );
            let freshest = min_idle([parent_idle, subagent_idle]);
            if freshest.is_none_or(|d| d >= within) {
                continue;
            }
            if read_head(&path).is_some_and(|h| head_names_sweep_issue(&h, issue)) {
                consider(freshest);
            }
        }
    }

    // Builder/Judge subagents run with the issue worktree as cwd.
    let worktree = workspace_root
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{issue}"));
    if let Ok(entries) = std::fs::read_dir(projects_dir.join(project_slug(&worktree))) {
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|e| e == "jsonl") {
                consider(idle_of(&path));
            }
        }
    }
    best
}

/// The shared predicate: how long the sweep has shown **no** sign of life,
/// across its log and its session transcripts. `None` ⇒ cannot assess.
#[must_use]
pub(crate) fn sweep_idle(
    log_path: &Path,
    workspace_root: &Path,
    issue: u32,
    within: Duration,
) -> Option<Duration> {
    let transcripts = crate::transcript_tokens::claude_projects_dir()
        .and_then(|dir| transcript_idle(&dir, workspace_root, issue, within));
    min_idle([idle_of(log_path), transcripts])
}

/// Phase-accurate wording for watchdog log lines: the sweep's checkpoint
/// phase, or a generic "sweep" when there is none. Never claims a "review
/// phase" for a sweep that has not reached one.
#[must_use]
pub(crate) fn phase_label(phase: Option<&str>) -> String {
    phase.map_or_else(
        || "sweep (no checkpoint phase)".to_string(),
        |p| format!("sweep (checkpoint phase `{p}`)"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::time::SystemTime;

    fn age(path: &Path, secs: u64) {
        let f = File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    fn seed(projects: &Path, root: &Path, issue: u32, secs: u64) -> std::path::PathBuf {
        let dir = projects.join(project_slug(root));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        fs::write(
            &p,
            format!(
                "{{\"content\":\"<command-name>/loom:sweep</command-name>\
                 <command-args>{issue} --claim-owned {issue}</command-args>\"}}\n"
            ),
        )
        .unwrap();
        age(&p, secs);
        p
    }

    const WITHIN: Duration = Duration::from_secs(2700);

    #[test]
    fn fresh_transcript_beats_old_log_inputs() {
        let t = tempfile::tempdir().unwrap();
        let (projects, root) = (t.path().join("p"), t.path().join("ws"));
        seed(&projects, &root, 7, 5);
        let idle = transcript_idle(&projects, &root, 7, WITHIN).unwrap();
        assert!(idle < Duration::from_secs(60));
        assert_eq!(min_idle([Some(Duration::from_secs(9000)), Some(idle)]), Some(idle));
    }

    #[test]
    fn fresh_subagent_transcript_counts() {
        let t = tempfile::tempdir().unwrap();
        let (projects, root) = (t.path().join("p"), t.path().join("ws"));
        let parent = seed(&projects, &root, 7, 9000);
        let sub = parent.with_extension("").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("a.jsonl"), "{}\n").unwrap();
        let idle = transcript_idle(&projects, &root, 7, WITHIN).unwrap();
        assert!(idle < Duration::from_secs(60));
    }

    #[test]
    fn all_old_signals_stay_old() {
        let t = tempfile::tempdir().unwrap();
        let (projects, root) = (t.path().join("p"), t.path().join("ws"));
        seed(&projects, &root, 7, 9000);
        assert_eq!(transcript_idle(&projects, &root, 7, WITHIN), None);
    }

    #[test]
    fn other_issues_transcript_is_ignored() {
        let t = tempfile::tempdir().unwrap();
        let (projects, root) = (t.path().join("p"), t.path().join("ws"));
        seed(&projects, &root, 8, 5);
        assert_eq!(transcript_idle(&projects, &root, 7, WITHIN), None);
    }

    #[test]
    fn absent_dirs_fail_open() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(transcript_idle(&t.path().join("nope"), t.path(), 7, WITHIN), None);
        assert_eq!(min_idle([None, None]), None);
    }

    #[test]
    fn phase_label_is_generic_without_a_phase() {
        assert!(phase_label(None).contains("no checkpoint phase"));
        assert!(phase_label(Some("builder")).contains("builder"));
        assert!(!phase_label(Some("builder")).contains("review"));
    }
}
