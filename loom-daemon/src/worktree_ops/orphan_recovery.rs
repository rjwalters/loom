//! `loom-daemon recover-orphans`: native port of `loom-recover-orphans`
//! (`orphan_recovery.py`).
//!
//! Detects two kinds of orphaned state and (with `--recover`) fixes them:
//!
//! - `untracked_building`: an issue carries `loom:building` but no live
//!   sweep is tracking it.
//! - `stale_heartbeat`: a `.loom/spawn-loop-state.json::running` entry's
//!   heartbeat is stale and its recorded PID is dead.
//!
//! Liveness evidence is gathered from every available source and unioned
//! (never intersected) — see [`gather_liveness_evidence`]. The fail-safe
//! invariant from issue #3651 is preserved exactly: **absent evidence means
//! treat every claim as ALIVE**, never as orphaned.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::claim_file::has_valid_claim;
use super::gh;
use super::liveness::{active_locked_issues, locks_dir};
use super::spawn_loop_state::{read_spawn_loop_state, SpawnLoopState};

pub const DEFAULT_HEARTBEAT_STALE_THRESHOLD_SECS: i64 = 300;
pub const DEFAULT_LABEL_GRACE_PERIOD_SECS: i64 = 600;
pub const DEFAULT_STALE_BUILDING_HOURS: f64 = 4.0;
pub const ORPHAN_COMMENT_DEDUP_SECONDS: i64 = 300;

fn heartbeat_stale_threshold() -> i64 {
    std::env::var("LOOM_HEARTBEAT_STALE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HEARTBEAT_STALE_THRESHOLD_SECS)
}

fn label_grace_period() -> i64 {
    std::env::var("LOOM_LABEL_GRACE_PERIOD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_LABEL_GRACE_PERIOD_SECS)
}

/// Mirrors `orphan_recovery.py::_get_stale_building_hours`: env override must
/// be a positive float or the default is used.
fn stale_building_hours() -> f64 {
    std::env::var(crate::claim_reconciliation::STALE_HOURS_ENV)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_BUILDING_HOURS)
}

fn pid_alive(pid: u32) -> bool {
    crate::sweep_registry::is_pid_alive(pid)
}

/// A detected orphan.
#[derive(Debug, Clone)]
pub struct OrphanEntry {
    pub kind: &'static str, // "untracked_building" | "stale_heartbeat"
    pub issue: Option<u32>,
    pub pid: Option<u32>,
    pub title: Option<String>,
    pub reason: String,
    pub age_seconds: Option<i64>,
}

/// A recovery action taken.
#[derive(Debug, Clone)]
pub struct RecoveryEntry {
    pub action: &'static str, // "reset_issue_label" | "cleanup_stale_worktree"
    pub issue: Option<u32>,
    pub reason: String,
}

/// A `loom:building` issue the staleness gate skipped (visible even without
/// `--verbose`, issue #3975).
#[derive(Debug, Clone)]
pub struct WatchedEntry {
    pub issue: u32,
    pub title: Option<String>,
    pub reason: &'static str,
    pub age_seconds: Option<i64>,
    pub threshold_seconds: f64,
}

#[derive(Debug, Default)]
pub struct OrphanRecoveryResult {
    pub orphaned: Vec<OrphanEntry>,
    pub recovered: Vec<RecoveryEntry>,
    pub watched: Vec<WatchedEntry>,
    pub recover_mode: bool,
}

/// Authoritative evidence of which sweeps are alive right now.
#[derive(Debug, Default)]
pub struct LivenessEvidence {
    pub available: bool,
    pub live_issues: HashSet<u32>,
    pub sources: Vec<&'static str>,
    pub journal_present: bool,
    pub journal_issues: HashSet<u32>,
}

/// Best-effort daemon-registry query. Mirrors
/// `orphan_recovery.py::_query_daemon_live_issues`, which is (deliberately)
/// a permanent no-op stub: the CLI runs as a standalone process with no
/// socket to a possibly-running daemon, so this is never a contributing
/// source today. Returns `None` ("not a source"), never an empty set
/// ("daemon says nothing is live") — those are different claims.
fn query_daemon_live_issues(_repo_root: &Path) -> Option<HashSet<u32>> {
    None
}

/// Gather liveness evidence from every available source and union them.
/// Mirrors `orphan_recovery.py::gather_liveness_evidence`.
#[must_use]
pub fn gather_liveness_evidence(
    spawn_loop_state: &SpawnLoopState,
    repo_root: &Path,
) -> LivenessEvidence {
    let mut live: HashSet<u32> = HashSet::new();
    let mut sources: Vec<&'static str> = Vec::new();
    let mut journal_present = false;
    let mut journal_issues: HashSet<u32> = HashSet::new();

    if spawn_loop_state.present {
        sources.push("spawn-loop-state.json");
        live.extend(
            spawn_loop_state
                .running
                .iter()
                .filter(|t| t.issue != 0)
                .map(|t| t.issue),
        );
    }

    if let Some(daemon_issues) = query_daemon_live_issues(repo_root) {
        sources.push("loom-daemon");
        live.extend(daemon_issues);
    }

    let locked = active_locked_issues(repo_root);
    if !locked.is_empty() {
        sources.push(".loom/locks");
        live.extend(locked);
    }

    if let Ok(journal_path) = crate::sweep_journal::default_journal_path() {
        if journal_path.exists() {
            journal_present = true;
            sources.push("sweep-journal");
            let journal = crate::sweep_journal::load(&journal_path);
            let repo_str = repo_root.display().to_string();
            let repo_resolved = repo_root.canonicalize().ok();
            for entry in &journal.entries {
                let matches = entry.repo == repo_str
                    || repo_resolved.as_ref().is_some_and(|r| {
                        std::path::Path::new(&entry.repo)
                            .canonicalize()
                            .ok()
                            .as_ref()
                            == Some(r)
                    });
                if !matches {
                    continue;
                }
                journal_issues.insert(entry.issue);
                if pid_alive(entry.pid) {
                    live.insert(entry.issue);
                }
            }
        }
    }

    LivenessEvidence {
        available: !sources.is_empty(),
        live_issues: live,
        sources,
        journal_present,
        journal_issues,
    }
}

/// Cross-reference `loom:building` issues against `evidence`. Mirrors
/// `orphan_recovery.py::check_untracked_building`, including the #3975
/// "watched" bookkeeping and the #3953 dual staleness-threshold selection.
pub fn check_untracked_building(
    evidence: &LivenessEvidence,
    result: &mut OrphanRecoveryResult,
    repo_root: &Path,
    label_grace_period_secs: i64,
    verbose: bool,
) {
    if !evidence.available {
        if verbose {
            eprintln!(
                "No authoritative liveness source available (no spawn-loop-state.json, no \
                 reachable loom-daemon registry, no .loom/locks/issue-<N>/ locks) — refusing \
                 to flag any loom:building issue as orphaned (fail-safe: absent liveness data \
                 means treat claims as ALIVE, not orphaned). See issue #3651."
            );
        }
        return;
    }

    let building_issues = match gh::list_building_issues(repo_root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to list loom:building issues: {e}");
            return;
        }
    };
    if building_issues.is_empty() {
        return;
    }

    for issue_data in building_issues {
        let issue_num = issue_data.number;
        let issue_title = issue_data.title;

        if evidence.live_issues.contains(&issue_num) {
            continue;
        }

        if has_valid_claim(repo_root, issue_num) {
            continue;
        }

        let has_journal_entry = evidence.journal_issues.contains(&issue_num);
        let (threshold_seconds, reason): (f64, &'static str) =
            if evidence.journal_present && !has_journal_entry {
                (stale_building_hours() * 3600.0, "no_journal_record_stale")
            } else if has_journal_entry {
                (label_grace_period_secs as f64, "journal_pid_dead")
            } else {
                (label_grace_period_secs as f64, "no_spawn_loop_entry")
            };

        if threshold_seconds > 0.0 {
            let label_age = gh::building_label_age_seconds(repo_root, issue_num);
            if let Some(age) = label_age {
                if (age as f64) < threshold_seconds {
                    result.watched.push(WatchedEntry {
                        issue: issue_num,
                        title: Some(issue_title.clone()),
                        reason,
                        age_seconds: Some(age),
                        threshold_seconds,
                    });
                    continue;
                }
            }
        }

        result.orphaned.push(OrphanEntry {
            kind: "untracked_building",
            issue: Some(issue_num),
            pid: None,
            title: Some(issue_title),
            reason: reason.to_string(),
            age_seconds: None,
        });
    }
}

/// Flag `.loom/spawn-loop-state.json::running` entries whose heartbeat is
/// stale and PID is dead. Mirrors `orphan_recovery.py::check_stale_heartbeats`.
pub fn check_stale_heartbeats(
    spawn_loop_state: &SpawnLoopState,
    result: &mut OrphanRecoveryResult,
    heartbeat_threshold_secs: i64,
) {
    for task in &spawn_loop_state.running {
        let Some(hb) = &task.last_heartbeat else {
            continue;
        };
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(hb) else {
            continue;
        };
        let age = chrono::Utc::now()
            .signed_duration_since(dt.with_timezone(&chrono::Utc))
            .num_seconds();
        if age <= heartbeat_threshold_secs {
            continue;
        }
        if pid_alive(task.pid) {
            continue;
        }
        result.orphaned.push(OrphanEntry {
            kind: "stale_heartbeat",
            issue: if task.issue != 0 {
                Some(task.issue)
            } else {
                None
            },
            pid: Some(task.pid),
            title: None,
            reason: "heartbeat_stale".to_string(),
            age_seconds: Some(age),
        });
    }
}

/// Best-effort stale-worktree cleanup for a recovered issue (0 commits ahead
/// of main, only build-artifact uncommitted changes). Mirrors
/// `orphan_recovery.py::_cleanup_stale_worktree`.
fn cleanup_stale_worktree(repo_root: &Path, issue: u32) -> bool {
    let worktree_path =
        crate::worktree_root::worktree_root(repo_root).join(super::naming::worktree_name(issue));
    if !worktree_path.is_dir() {
        return false;
    }

    let log_out = std::process::Command::new("git")
        .args(["-C"])
        .arg(&worktree_path)
        .args(["log", "--oneline", "origin/main..HEAD"])
        .output();
    let Ok(log_out) = log_out else { return false };
    if !log_out.status.success() {
        return false;
    }
    if !String::from_utf8_lossy(&log_out.stdout).trim().is_empty() {
        return false;
    }

    let status_out = std::process::Command::new("git")
        .args(["-C"])
        .arg(&worktree_path)
        .args(["status", "--porcelain"])
        .output();
    let Ok(status_out) = status_out else {
        return false;
    };
    if !status_out.status.success() {
        return false;
    }
    const BUILD_ARTIFACT_PATTERNS: &[&str] = &[
        "node_modules",
        "pnpm-lock.yaml",
        ".venv",
        "target/",
        "Cargo.lock",
        "coverage/",
        ".loom-checkpoint",
        ".loom-in-use",
    ];
    for line in String::from_utf8_lossy(&status_out.stdout).trim().lines() {
        // porcelain lines look like "XY path" (or "XY orig -> new" for renames);
        // take the path portion after the two-char status + space.
        let path_part = line.get(3..).unwrap_or(line);
        if !BUILD_ARTIFACT_PATTERNS
            .iter()
            .any(|pat| path_part.contains(pat))
        {
            return false;
        }
    }

    let branch_out = std::process::Command::new("git")
        .args(["-C"])
        .arg(&worktree_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output();
    let branch = branch_out
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let removed = std::process::Command::new("git")
        .args(["worktree", "remove"])
        .arg(&worktree_path)
        .arg("--force")
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success());
    if !removed {
        return false;
    }

    if !branch.is_empty() && branch != "main" {
        let _ = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo_root)
            .args(["branch", "-D", &branch])
            .status();
        let _ = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo_root)
            .args(["push", "origin", "--delete", &branch])
            .status();
    }
    true
}

/// Reset an orphaned issue's label back to `loom:issue` (+ best-effort
/// stale-worktree cleanup + a dedup'd comment). Mirrors
/// `orphan_recovery.py::recover_issue`.
pub fn recover_issue(
    repo_root: &Path,
    issue: u32,
    reason: &str,
    result: &mut OrphanRecoveryResult,
    label_grace_period_secs: i64,
) {
    if label_grace_period_secs > 0 {
        if let Some(age) = gh::building_label_age_seconds(repo_root, issue) {
            if age < label_grace_period_secs {
                eprintln!(
                    "Skipping recovery for issue #{issue}: loom:building label applied {age}s ago \
                     (grace period: {label_grace_period_secs}s)"
                );
                return;
            }
        }
    }

    if has_valid_claim(repo_root, issue) {
        eprintln!("Skipping recovery for issue #{issue}: valid file-based claim exists");
        return;
    }

    let worktree_cleaned = cleanup_stale_worktree(repo_root, issue);
    if worktree_cleaned {
        result.recovered.push(RecoveryEntry {
            action: "cleanup_stale_worktree",
            issue: Some(issue),
            reason: reason.to_string(),
        });
    }

    if let Err(e) = gh::edit_labels(repo_root, issue, "loom:building", "loom:issue") {
        eprintln!("Failed to update labels for issue #{issue}: {e}");
        return;
    }

    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut actions = vec![
        "- Removed `loom:building` label".to_string(),
        "- Added `loom:issue` label to return to ready queue".to_string(),
    ];
    if worktree_cleaned {
        actions.push("- Cleaned up stale worktree and branches".to_string());
    }
    let comment = format!(
        "## Orphan Recovery\n\n\
         This issue was automatically recovered from an orphaned state.\n\n\
         **Reason**: {reason}\n\
         **What happened**:\n\
         - The spawn-loop task that was working on this issue crashed or was terminated\n\
         - The issue was left in `loom:building` state with no active worker\n\n\
         **Action taken**:\n{}\n\n\
         This issue is now available for a new sweep to pick up.\n\n\
         ---\n\
         *Recovered by loom-recover-orphans at {ts}*",
        actions.join("\n")
    );

    if !gh::has_recent_orphan_comment(repo_root, issue, ORPHAN_COMMENT_DEDUP_SECONDS) {
        if let Err(e) = gh::comment(repo_root, issue, &comment) {
            eprintln!("Failed to add comment to issue #{issue}: {e}");
        }
    }

    result.recovered.push(RecoveryEntry {
        action: "reset_issue_label",
        issue: Some(issue),
        reason: reason.to_string(),
    });
    println!("Recovered issue #{issue}");
}

/// Run all detection phases and, if `recover`, perform recovery. Mirrors
/// `orphan_recovery.py::run_orphan_recovery`.
pub fn run_orphan_recovery(repo_root: &Path, recover: bool, verbose: bool) -> OrphanRecoveryResult {
    let mut result = OrphanRecoveryResult {
        recover_mode: recover,
        ..Default::default()
    };
    let heartbeat_threshold = heartbeat_stale_threshold();
    let grace_period = label_grace_period();

    let spawn_loop_state = read_spawn_loop_state(repo_root);
    let evidence = gather_liveness_evidence(&spawn_loop_state, repo_root);

    if verbose {
        if evidence.available {
            let mut live: Vec<_> = evidence.live_issues.iter().collect();
            live.sort();
            eprintln!(
                "Liveness sources: {} (live issues: {:?})",
                evidence.sources.join(", "),
                live
            );
        } else {
            eprintln!(
                "No authoritative liveness source found — untracked-building cross-check will \
                 fail safe (emit zero orphans). See #3651."
            );
        }
    }

    check_untracked_building(&evidence, &mut result, repo_root, grace_period, verbose);
    check_stale_heartbeats(&spawn_loop_state, &mut result, heartbeat_threshold);

    if !recover {
        return result;
    }

    let orphans: Vec<(u32, String)> = result
        .orphaned
        .iter()
        .filter_map(|o| o.issue.map(|n| (n, o.reason.clone())))
        .collect();
    for (issue, reason) in orphans {
        recover_issue(repo_root, issue, &reason, &mut result, grace_period);
    }

    result
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[must_use]
pub fn format_result_human(result: &OrphanRecoveryResult) -> String {
    let mut lines: Vec<String> = Vec::new();
    if result.orphaned.is_empty() {
        lines.push("No orphaned tasks found".to_string());
    } else {
        lines.push(format!("Found {} orphaned task(s)", result.orphaned.len()));
        lines.push(String::new());
        for orphan in &result.orphaned {
            match orphan.kind {
                "untracked_building" => lines.push(format!(
                    "  [{}] #{}: {} -- no active spawn-loop task",
                    orphan.kind,
                    orphan.issue.unwrap_or(0),
                    orphan.title.as_deref().unwrap_or("no title")
                )),
                "stale_heartbeat" => lines.push(format!(
                    "  [{}] issue #{} (pid {}): heartbeat stale ({})",
                    orphan.kind,
                    orphan.issue.unwrap_or(0),
                    orphan.pid.unwrap_or(0),
                    format_duration(orphan.age_seconds.unwrap_or(0))
                )),
                _ => {}
            }
        }
        if result.recover_mode {
            lines.push(String::new());
            lines.push(format!("Recovered {} item(s)", result.recovered.len()));
        } else {
            lines.push(String::new());
            lines.push("Run with --recover to fix these issues".to_string());
        }
    }

    if !result.watched.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{} claim(s) seen but not yet stale enough to reclaim:",
            result.watched.len()
        ));
        for w in &result.watched {
            let age = w.age_seconds.unwrap_or(0);
            let threshold = w.threshold_seconds as i64;
            let remaining = (threshold - age).max(0);
            lines.push(format!(
                "  [watched] #{}: {} -- skipped ({}): label age {}, threshold {}, eligible in {}",
                w.issue,
                w.title.as_deref().unwrap_or("no title"),
                w.reason,
                format_duration(age),
                format_duration(threshold),
                format_duration(remaining)
            ));
        }
    }

    lines.join("\n")
}

#[must_use]
pub fn format_result_json(result: &OrphanRecoveryResult) -> String {
    let orphaned: Vec<_> = result
        .orphaned
        .iter()
        .map(|o| {
            let mut m = serde_json::Map::new();
            m.insert("type".to_string(), o.kind.into());
            m.insert("reason".to_string(), o.reason.clone().into());
            if let Some(i) = o.issue {
                m.insert("issue".to_string(), i.into());
            }
            if let Some(p) = o.pid {
                m.insert("pid".to_string(), p.into());
            }
            if let Some(t) = &o.title {
                m.insert("title".to_string(), t.clone().into());
            }
            if let Some(a) = o.age_seconds {
                m.insert("age_seconds".to_string(), a.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let recovered: Vec<_> = result
        .recovered
        .iter()
        .map(|r| {
            let mut m = serde_json::Map::new();
            m.insert("action".to_string(), r.action.into());
            m.insert("reason".to_string(), r.reason.clone().into());
            if let Some(i) = r.issue {
                m.insert("issue".to_string(), i.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let watched: Vec<_> = result
        .watched
        .iter()
        .map(|w| {
            let mut m = serde_json::Map::new();
            m.insert("issue".to_string(), w.issue.into());
            m.insert("reason".to_string(), w.reason.into());
            m.insert("threshold_seconds".to_string(), w.threshold_seconds.into());
            if let Some(t) = &w.title {
                m.insert("title".to_string(), t.clone().into());
            }
            if let Some(a) = w.age_seconds {
                m.insert("age_seconds".to_string(), a.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();

    let obj = serde_json::json!({
        "orphaned": orphaned,
        "recovered": recovered,
        "watched": watched,
        "total_orphaned": result.orphaned.len(),
        "total_recovered": result.recovered.len(),
        "total_watched": result.watched.len(),
        "recover_mode": result.recover_mode,
    });
    serde_json::to_string_pretty(&obj).unwrap_or_default()
}

/// Path to the spawn-loop claim-lock directory (re-exported for the CLI's
/// help text / diagnostics).
#[must_use]
pub fn locks_dir_for(repo_root: &Path) -> PathBuf {
    locks_dir(repo_root)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn absent_state() -> SpawnLoopState {
        SpawnLoopState::default()
    }

    /// Point the machine-level sweep journal at a path scoped to this test's
    /// tempdir (guaranteed not to exist), so `gather_liveness_evidence`
    /// doesn't pick up this dev machine's real `~/.loom/sweeps.json` as an
    /// unexpected evidence source. Mirrors the journal-path isolation pattern
    /// used by `claim_reconciliation`'s own tests.
    fn with_isolated_journal_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let journal_path = dir.join("sweeps.json");
        std::env::set_var(crate::sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        let result = f();
        std::env::remove_var(crate::sweep_journal::JOURNAL_PATH_ENV);
        result
    }

    #[test]
    #[serial]
    fn no_evidence_sources_means_unavailable() {
        let dir = tempdir().unwrap();
        let evidence = with_isolated_journal_path(dir.path(), || {
            gather_liveness_evidence(&absent_state(), dir.path())
        });
        assert!(!evidence.available, "fail-safe: no sources present must mean unavailable");
        assert!(evidence.live_issues.is_empty());
    }

    #[test]
    #[serial]
    fn locks_dir_contributes_evidence() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(locks_dir(dir.path()).join("issue-7")).unwrap();
        let evidence = with_isolated_journal_path(dir.path(), || {
            gather_liveness_evidence(&absent_state(), dir.path())
        });
        assert!(evidence.available);
        assert!(evidence.live_issues.contains(&7));
        assert!(evidence.sources.contains(&".loom/locks"));
    }

    #[test]
    #[serial]
    fn spawn_loop_state_present_contributes_evidence_even_if_empty() {
        let dir = tempdir().unwrap();
        let state = SpawnLoopState {
            running: Vec::new(),
            present: true,
        };
        let evidence =
            with_isolated_journal_path(dir.path(), || gather_liveness_evidence(&state, dir.path()));
        assert!(evidence.available);
        assert!(evidence.sources.contains(&"spawn-loop-state.json"));
    }

    #[test]
    fn check_untracked_building_emits_nothing_with_no_evidence() {
        let dir = tempdir().unwrap();
        let mut result = OrphanRecoveryResult::default();
        let evidence = LivenessEvidence::default();
        // No gh call is even attempted (fail-safe short-circuit) so this is
        // safe to run without a real `gh`/network — it must return with zero
        // orphans and zero watched entries.
        check_untracked_building(&evidence, &mut result, dir.path(), 600, false);
        assert!(result.orphaned.is_empty());
        assert!(result.watched.is_empty());
    }

    #[test]
    fn stale_heartbeat_with_dead_pid_is_orphaned() {
        let mut result = OrphanRecoveryResult::default();
        let state = SpawnLoopState {
            present: true,
            running: vec![super::super::spawn_loop_state::SpawnLoopTask {
                issue: 42,
                pid: 0, // pid 0 is never alive per sweep_registry::is_pid_alive
                last_heartbeat: Some(
                    (chrono::Utc::now() - chrono::Duration::seconds(600)).to_rfc3339(),
                ),
            }],
        };
        check_stale_heartbeats(&state, &mut result, 300);
        assert_eq!(result.orphaned.len(), 1);
        assert_eq!(result.orphaned[0].kind, "stale_heartbeat");
        assert_eq!(result.orphaned[0].issue, Some(42));
    }

    #[test]
    fn fresh_heartbeat_is_not_orphaned() {
        let mut result = OrphanRecoveryResult::default();
        let state = SpawnLoopState {
            present: true,
            running: vec![super::super::spawn_loop_state::SpawnLoopTask {
                issue: 42,
                pid: 0,
                last_heartbeat: Some(chrono::Utc::now().to_rfc3339()),
            }],
        };
        check_stale_heartbeats(&state, &mut result, 300);
        assert!(result.orphaned.is_empty());
    }

    #[test]
    fn format_result_human_reports_zero_orphans() {
        let result = OrphanRecoveryResult::default();
        assert_eq!(format_result_human(&result), "No orphaned tasks found");
    }

    #[test]
    fn format_result_json_round_trips_totals() {
        let mut result = OrphanRecoveryResult::default();
        result.orphaned.push(OrphanEntry {
            kind: "untracked_building",
            issue: Some(1),
            pid: None,
            title: Some("t".to_string()),
            reason: "no_spawn_loop_entry".to_string(),
            age_seconds: None,
        });
        let json = format_result_json(&result);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total_orphaned"], 1);
    }
}
