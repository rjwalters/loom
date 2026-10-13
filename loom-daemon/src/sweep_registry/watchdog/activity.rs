//! Shared work-liveness predicate for the review-stall (#3910) and
//! stale-sweep (#7529) watchdogs (Issue #9533).
//!
//! The sweep log's mtime is **not** a liveness signal: a headless
//! (`LOOM_HEADLESS_SESSION=1`, `claude -p`) sweep writes nothing to
//! `.loom/logs/sweep-issue-N.log` while it works, so a healthy sweep that is
//! compiling or waiting on the model looks identical to a dead one. The
//! session's own transcript (`~/.claude/projects/<slug>/<session>.jsonl`, plus
//! `subagents/*.jsonl`) *is* appended throughout, so the idle time a watchdog
//! judges is the **minimum** over every signal we can read:
//!
//! 1. the sweep log's mtime;
//! 2. the session (and subagent) transcripts' mtimes;
//! 3. worktree activity — [`crate::worktree_activity::probe_worktree_activity`]
//!    (root, git refs, `target/` depth 1, bounded source walk) plus `target/`'s
//!    depth-2 directories, which a running `cargo` rewrites constantly;
//! 4. the youngest tool process under the sweep's agent runtime (`claude` /
//!    `codex`) — a live agent spawns short-lived `bash`/`cargo`/`git`
//!    children, a hung one does not.
//!
//! Every signal is optional. Unreadable/missing signals are skipped, and when
//! none is readable the result is `None` — "cannot assess" — never a stall.
//! Signals 2-4 only ever report **fresh** evidence (younger than the window):
//! they can turn a log-silent sweep Healthy, never make a sweep look staler.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::orphan_process_reaper::{children_map, descendants_of, snapshot_processes, ProcEntry};
use crate::transcript_tokens::{
    head_names_sweep_issue, project_slug, read_head, session_transcripts,
};
use crate::worktree_activity::{probe_worktree_activity, ActivityProbe};

/// Cap on entries stat'ed by the `target/` depth-2 scan, so a pathological
/// `target/` (thousands of profile dirs) still costs a bounded number of
/// syscalls per watchdog tick.
const MAX_TARGET_ENTRIES: usize = 512;

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
    let worktree = issue_worktree(workspace_root, issue);
    if let Ok(entries) = std::fs::read_dir(projects_dir.join(project_slug(&worktree))) {
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|e| e == "jsonl") {
                // The parent plus its nested subagent transcripts: subagent
                // records are not duplicated into the parent.
                for t in session_transcripts(&path) {
                    consider(idle_of(&t));
                }
            }
        }
    }
    best
}

/// `.loom/worktrees/issue-<N>` under `workspace_root`.
fn issue_worktree(workspace_root: &Path, issue: u32) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{issue}"))
}

/// Freshest directory two levels under `worktree/target/` (e.g.
/// `target/debug/deps`, `target/debug/incremental`), bounded by
/// [`MAX_TARGET_ENTRIES`]. Depth 1 is already covered by
/// [`probe_worktree_activity`], but a compile mostly writes one level deeper,
/// which leaves `target/debug`'s own mtime untouched.
fn target_depth2_idle(worktree: &Path, now: SystemTime, within: Duration) -> Option<Duration> {
    let mut seen = 0usize;
    let mut best: Option<Duration> = None;
    let profiles = std::fs::read_dir(worktree.join("target")).ok()?;
    for profile in profiles.flatten() {
        if !profile.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(profile.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_TARGET_ENTRIES {
                return best;
            }
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let age = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                // A future mtime (clock skew) reads as maximally recent.
                .map(|m| now.duration_since(m).unwrap_or(Duration::ZERO));
            best = min_idle([best, age.filter(|a| *a < within)]);
        }
    }
    best
}

/// Signal 3: how recently anything under the issue worktree was written,
/// reported only when inside `within`. A missing/unreadable worktree, or one
/// verified idle, yields `None` (no fresh evidence).
#[must_use]
pub(crate) fn worktree_idle(worktree: &Path, within: Duration) -> Option<Duration> {
    let now = SystemTime::now();
    let probed = match probe_worktree_activity(worktree, now, within) {
        ActivityProbe::Recent { age_secs } => Some(Duration::from_secs(age_secs)),
        ActivityProbe::Idle | ActivityProbe::Unknown => None,
    };
    if probed.is_some() {
        return probed;
    }
    target_depth2_idle(worktree, now, within)
}

/// Whether `cmdline` is an agent runtime (`claude` / `codex`) by the basename
/// of argv0, or of argv1 for an interpreter shim (`node …/claude`).
///
/// Deliberately not [`crate::orphan_process_reaper::looks_like_agent`]: that
/// also matches any argv naming `/loom:`, which a `claude-wrapper.sh`
/// invocation (and every subshell it forks) does. The wrapper's background
/// monitors respawn `sleep 5` forever, so counting their children would make
/// every sweep — hung or not — look alive.
fn is_agent_runtime(cmdline: &str) -> bool {
    cmdline.split_whitespace().take(2).any(|tok| {
        Path::new(tok)
            .file_name()
            .is_some_and(|b| b == "claude" || b == "codex")
    })
}

/// Signal 4 (pure): age of the youngest process **under an agent runtime**
/// in `pid`'s process tree, reported only when inside `within`.
///
/// Only processes spawned by the agent count: they are its tool calls. A tool
/// call that is itself stuck ages out like everything else, because age is
/// time since *start*. `None` when no agent runtime is found under `pid`, or
/// none of its descendants is young — never a stall on its own.
#[must_use]
pub(crate) fn descendant_idle(pid: u32, procs: &[ProcEntry], within: Duration) -> Option<Duration> {
    let by_pid: HashMap<u32, &ProcEntry> = procs.iter().map(|p| (p.pid, p)).collect();
    let children = children_map(procs);
    let is_agent = |p: &u32| by_pid.get(p).is_some_and(|e| is_agent_runtime(&e.cmdline));
    let mut agents: Vec<u32> = descendants_of(&[pid], &children)
        .into_iter()
        .filter(|p| is_agent(p))
        .collect();
    if is_agent(&pid) {
        agents.push(pid);
    }
    if agents.is_empty() {
        return None;
    }
    descendants_of(&agents, &children)
        .iter()
        .filter_map(|p| by_pid.get(p)?.age_secs)
        .map(Duration::from_secs)
        .filter(|age| *age < within)
        .min()
}

/// The shared predicate: how long the sweep has shown **no** sign of life,
/// across its log, session transcripts, worktree, and agent tool processes
/// (see the module docs). `None` ⇒ cannot assess.
///
/// Cheapest signals first; as soon as one is fresh (inside `within`) the
/// sweep is Healthy and the costlier probes (`/proc` walk, worktree walk) are
/// skipped.
#[must_use]
pub(crate) fn sweep_idle(
    log_path: &Path,
    workspace_root: &Path,
    issue: u32,
    pid: u32,
    within: Duration,
) -> Option<Duration> {
    let transcripts = crate::transcript_tokens::claude_projects_dir()
        .and_then(|dir| transcript_idle(&dir, workspace_root, issue, within));
    let mut idle = min_idle([idle_of(log_path), transcripts]);
    let fresh = |d: Option<Duration>| d.is_some_and(|d| d < within);
    if fresh(idle) {
        return idle;
    }
    idle = min_idle([idle, descendant_idle(pid, &snapshot_processes(), within)]);
    if fresh(idle) {
        return idle;
    }
    min_idle([
        idle,
        worktree_idle(&issue_worktree(workspace_root, issue), within),
    ])
}

/// [`phase_label`] for `issue`, read from its checkpoint under `checkpoint_dir`.
#[must_use]
pub(crate) fn checkpoint_phase_label(checkpoint_dir: &Path, issue: u32) -> String {
    let path = checkpoint_dir.join(format!("issue-{issue}.json"));
    phase_label(crate::sweep_registry::reaper::read_checkpoint_phase(&path).as_deref())
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
    fn fresh_worktree_subagent_transcript_counts() {
        let t = tempfile::tempdir().unwrap();
        let (projects, root) = (t.path().join("p"), t.path().join("ws"));
        let worktree = root.join(".loom").join("worktrees").join("issue-7");
        let dir = projects.join(project_slug(&worktree));
        fs::create_dir_all(&dir).unwrap();
        let parent = dir.join("s.jsonl");
        fs::write(&parent, "{}\n").unwrap();
        age(&parent, 9000);
        let sub = dir.join("s").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        let nested = sub.join("a.jsonl");
        fs::write(&nested, "{}\n").unwrap();
        let idle = transcript_idle(&projects, &root, 7, WITHIN).unwrap();
        assert!(idle < Duration::from_secs(60));
        age(&nested, 9000);
        let idle = transcript_idle(&projects, &root, 7, WITHIN).unwrap();
        assert!(idle >= Duration::from_secs(9000));
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

    /// Age a directory's own mtime (opened read-only; `futimens` as owner).
    fn age_dir(path: &Path, secs: u64) {
        File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    /// A worktree whose every entry is far older than the window.
    fn old_worktree(t: &Path) -> std::path::PathBuf {
        let wt = t.join("wt");
        fs::create_dir_all(wt.join("src")).unwrap();
        fs::create_dir_all(wt.join("target").join("debug").join("deps")).unwrap();
        fs::write(wt.join("src").join("lib.rs"), "\n").unwrap();
        age(&wt.join("src").join("lib.rs"), 9000);
        for d in ["target/debug/deps", "target/debug", "target", "src", ""] {
            age_dir(&wt.join(d), 9000);
        }
        wt
    }

    #[test]
    fn worktree_signal_reports_only_fresh_writes() {
        let t = tempfile::tempdir().unwrap();
        let wt = old_worktree(t.path());
        assert_eq!(worktree_idle(&wt, WITHIN), None, "all-old worktree is no evidence");
        fs::write(wt.join("src").join("lib.rs"), "// edit\n").unwrap();
        assert!(worktree_idle(&wt, WITHIN).unwrap() < Duration::from_secs(60));
    }

    #[test]
    fn worktree_signal_sees_a_build_writing_two_levels_into_target() {
        let t = tempfile::tempdir().unwrap();
        let wt = old_worktree(t.path());
        // A compile adds a file under target/debug/deps: only `deps`' own
        // mtime moves, which the depth-1 probe cannot see.
        fs::write(wt.join("target/debug/deps/libx.rlib"), "").unwrap();
        assert!(worktree_idle(&wt, WITHIN).unwrap() < Duration::from_secs(60));
    }

    #[test]
    fn missing_worktree_is_no_evidence() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(worktree_idle(&t.path().join("nope"), WITHIN), None);
    }

    fn proc(pid: u32, ppid: u32, cmdline: &str, age: Option<u64>) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            cwd: None,
            cmdline: cmdline.to_string(),
            age_secs: age,
        }
    }

    /// The daemon-spawned shape: wrapper bash -> {monitor subshell -> sleep,
    /// claude -> {MCP server, tool bash}}.
    fn sweep_tree(tool_age: Option<u64>) -> Vec<ProcEntry> {
        let wrapper = "bash claude-wrapper.sh -p /loom:sweep 7";
        let mut procs = vec![
            proc(10, 1, wrapper, Some(9000)),
            proc(11, 10, wrapper, Some(9000)),
            proc(12, 11, "sleep 5", Some(1)),
            proc(13, 10, "/home/u/.local/bin/claude -p /loom:sweep 7", Some(9000)),
            proc(14, 13, "node mcp-loom/dist/index.js", Some(9000)),
            proc(99, 1, "bash unrelated", Some(0)),
        ];
        if let Some(age) = tool_age {
            procs.push(proc(15, 13, "bash -c cargo build", Some(age)));
        }
        procs
    }

    #[test]
    fn young_tool_process_under_the_agent_counts() {
        let idle = descendant_idle(10, &sweep_tree(Some(66)), WITHIN);
        assert_eq!(idle, Some(Duration::from_secs(66)));
    }

    #[test]
    fn wrapper_monitor_sleep_does_not_count_as_activity() {
        // Only the wrapper's forever-respawned `sleep 5` is young: not
        // evidence, or every hung sweep would look alive.
        assert_eq!(descendant_idle(10, &sweep_tree(None), WITHIN), None);
    }

    #[test]
    fn stuck_old_tool_process_ages_out() {
        assert_eq!(descendant_idle(10, &sweep_tree(Some(9000)), WITHIN), None);
    }

    #[test]
    fn no_agent_or_unreadable_tree_is_no_evidence() {
        let procs = vec![
            proc(10, 1, "sh -c sleep 600", Some(9000)),
            proc(11, 10, "sleep 600", Some(1)),
        ];
        assert_eq!(descendant_idle(10, &procs, WITHIN), None);
        assert_eq!(descendant_idle(10, &[], WITHIN), None);
        // The agent is the sweep pid itself; an underivable age is skipped.
        let procs = vec![proc(10, 1, "claude -p x", None), proc(11, 10, "bash", None)];
        assert_eq!(descendant_idle(10, &procs, WITHIN), None);
    }

    #[test]
    fn agent_runtime_matches_argv0_or_interpreter_shim_only() {
        assert!(is_agent_runtime("/usr/bin/claude -p /loom:sweep 1"));
        assert!(is_agent_runtime("node /opt/npm/bin/claude -p x"));
        assert!(is_agent_runtime("codex exec y"));
        assert!(!is_agent_runtime("bash claude-wrapper.sh /loom:sweep 1"));
        assert!(!is_agent_runtime("bash -c echo claude"));
    }

    #[test]
    fn phase_label_is_generic_without_a_phase() {
        assert!(phase_label(None).contains("no checkpoint phase"));
        assert!(phase_label(Some("builder")).contains("builder"));
        assert!(!phase_label(Some("builder")).contains("review"));
    }
}
