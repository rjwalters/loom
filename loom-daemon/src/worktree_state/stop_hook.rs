//! `Stop` / `SubagentStop` hook decision: refuse a clean completion when the
//! session's own worktree still holds its deliverable (Issue #8267).
//!
//! # Contract
//!
//! Input (JSON on stdin, from Claude Code):
//! `{ "session_id", "transcript_path", "cwd", "stop_hook_active", "hook_event_name" }`
//!
//! Output: to block, print `{"decision":"block","reason":"…"}` and exit 0. To
//! allow, print nothing (or a `{"systemMessage":"…"}` advisory) and exit 0 —
//! the same contract `guard-background-subagents.sh` already implements for the
//! outstanding-background-work hazard. This guard is its sibling for the
//! *uncommitted-work* hazard: one blocks a turn that ends too early, this one
//! blocks a turn that ends with the work still only on this machine's disk.
//!
//! # Why ownership is established from Edit/Write, not from cwd alone
//!
//! A `Task`-tool subagent inherits the orchestrator's cwd (the main checkout),
//! so cwd alone identifies the *wrong* directory for exactly the sessions this
//! guard is for. But the orchestrator's transcript also *mentions* worktree
//! paths constantly (`check-main-clean.sh --label issue=N`, checkpoint writes),
//! so a bare substring scan would block the orchestrator for a Builder's mess —
//! a turn it cannot fix. Ownership is therefore taken from **Edit/Write-family
//! tool calls whose `file_path` lands inside a managed worktree**: a Builder
//! writes there, an orchestrator does not.
//!
//! # Failure mode: always allow
//!
//! Every unreadable payload, missing transcript, absent git, or unparseable
//! line resolves to "allow, say nothing". A guard that wedges a headless sweep
//! on its own parse bug is worse than the loss it prevents, and `stop_hook_active`
//! caps the cost at exactly one extra turn even when it is right.

use std::path::{Path, PathBuf};

use super::{collect, is_managed_worktree, WorktreeState};

/// Tool names whose `file_path` input proves this session wrote into a
/// directory. `Bash` is deliberately absent: a mentioned path is not an edit.
const WRITE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Only transcript lines containing this can carry worktree ownership, so the
/// scan JSON-parses a handful of lines out of a multi-megabyte transcript.
const WORKTREE_MARKER: &str = "/.loom/worktrees/";

/// The `guards.*` key and env override, following the established guard-toggle
/// convention (`guards.worktreeIsolation` / `LOOM_GUARD_WORKTREE_ISOLATION`).
pub const TOGGLE_CONFIG_KEY: &str = "guards.uncommittedWork";
pub const TOGGLE_ENV_VAR: &str = "LOOM_GUARD_UNCOMMITTED_WORK";

/// The hook payload fields this guard reads. Unknown fields are ignored, and
/// every field is optional — a payload shape change must degrade to "allow",
/// never to a parse error that a `Stop` hook has no channel to report.
#[derive(Debug, Default, serde::Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub stop_hook_active: bool,
    #[serde(default)]
    pub hook_event_name: Option<String>,
}

/// What the guard decided, before rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to say: no owned worktree, guard disabled, or nothing at risk
    /// and nothing worth reporting.
    Silent,
    /// Allow the stop, but state the repo state — the "surface it in the
    /// completion notification" half of #8267, for the turns that are fine.
    Advise(String),
    /// Block the stop once, naming what is unsaved and how to save it.
    Block(String),
}

impl Decision {
    /// The JSON a `Stop` hook prints on stdout, or `None` for silence.
    #[must_use]
    pub fn to_hook_json(&self) -> Option<serde_json::Value> {
        match self {
            Decision::Silent => None,
            Decision::Advise(msg) => Some(serde_json::json!({ "systemMessage": msg })),
            Decision::Block(reason) => {
                Some(serde_json::json!({ "decision": "block", "reason": reason }))
            }
        }
    }
}

/// Whether the guard is enabled for the workspace containing `worktree`.
///
/// Precedence mirrors every other guard toggle: env var wins, then
/// `guards.uncommittedWork` in the resolved config, then the default (on).
///
/// The config is resolved against the **main checkout**, not the worktree —
/// the host-local override tier (`.loom-local/local.json`) is gitignored and
/// therefore exists only there, exactly as `guard-background-subagents.sh`
/// reads its toggle from `$MAIN_ROOT/.loom/config.json`. See
/// [`main_checkout_root`](super::main_checkout_root).
#[must_use]
pub fn guard_enabled(worktree: &Path) -> bool {
    match std::env::var(TOGGLE_ENV_VAR).ok().as_deref() {
        Some("0" | "false" | "no" | "off") => return false,
        Some("1" | "true" | "yes" | "on") => return true,
        _ => {}
    }
    let repo_root = super::main_checkout_root(worktree).unwrap_or_else(|| worktree.to_path_buf());
    let config = crate::config_resolver::resolve_effective_config(&repo_root);
    !matches!(
        crate::config_resolver::get_path(&config, TOGGLE_CONFIG_KEY),
        Some(serde_json::Value::Bool(false))
    )
}

/// The managed worktree this session wrote into, if any.
///
/// `cwd` first (a headless role agent spawned inside its own worktree is the
/// unambiguous case), then the transcript's Edit/Write targets, newest first.
#[must_use]
pub fn owned_worktree(payload: &HookPayload) -> Option<PathBuf> {
    if let Some(cwd) = payload.cwd.as_deref() {
        let cwd = Path::new(cwd);
        if is_managed_worktree(cwd) {
            return Some(cwd.to_path_buf());
        }
    }
    let transcript = payload.transcript_path.as_deref()?;
    let paths = write_targets(Path::new(transcript));
    paths.into_iter().rev().find_map(|p| enclosing_worktree(&p))
}

/// The managed-worktree directory containing `path`, if any.
///
/// Walks up from the file to the first ancestor carrying the sentinel, so a
/// deep `…/issue-42/loom-daemon/src/foo.rs` resolves to `…/issue-42`.
#[must_use]
pub fn enclosing_worktree(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if is_managed_worktree(parent) {
            return Some(parent.to_path_buf());
        }
        cur = parent;
    }
    None
}

/// Every Edit/Write-family `file_path` in a transcript that names a worktree,
/// in transcript order.
///
/// Lines are pre-filtered on [`WORKTREE_MARKER`] before any JSON parse: a real
/// sweep transcript is tens of megabytes and this hook runs on every turn end.
#[must_use]
pub fn write_targets(transcript: &Path) -> Vec<PathBuf> {
    let Ok(raw) = std::fs::read_to_string(transcript) else {
        return Vec::new();
    };
    write_targets_from_str(&raw)
}

/// [`write_targets`]'s pure half, so the scan is testable without a file.
#[must_use]
pub fn write_targets_from_str(raw: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if !line.contains(WORKTREE_MARKER) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        collect_write_targets(&value, &mut out);
    }
    out
}

/// Walk an arbitrary transcript entry for `{"type":"tool_use","name":<write
/// tool>,"input":{"file_path":…}}` objects.
///
/// Recursive rather than indexed on a fixed `message.content[]` path on
/// purpose: transcript entry shapes have changed before (#5086's `Task` →
/// `Agent` rename), and a structural scan degrades to finding nothing instead
/// of silently matching the wrong key.
fn collect_write_targets(value: &serde_json::Value, out: &mut Vec<PathBuf>) {
    match value {
        serde_json::Value::Object(map) => {
            let is_write_tool = map.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && map
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| WRITE_TOOLS.contains(&n));
            if is_write_tool {
                if let Some(path) = map
                    .get("input")
                    .and_then(|i| i.get("file_path"))
                    .and_then(|p| p.as_str())
                {
                    if path.contains(WORKTREE_MARKER) {
                        out.push(PathBuf::from(path));
                    }
                }
            }
            for v in map.values() {
                collect_write_targets(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_write_targets(v, out);
            }
        }
        _ => {}
    }
}

/// Turn a measured state into a decision.
///
/// Pure: the block-once rule, the advisory threshold and the message text are
/// all testable without a hook payload or a git fixture.
#[must_use]
pub fn decide(state: &WorktreeState, stop_hook_active: bool) -> Decision {
    if !state.work_at_risk() {
        // Nothing is unsaved. Say the state anyway when there IS work, so a
        // clean completion carries evidence rather than an assertion; stay
        // silent for a session that produced nothing at all.
        if state.commits_ahead == 0 {
            return Decision::Silent;
        }
        return Decision::Advise(format!(
            "Work state (#8267): {}. Nothing uncommitted.",
            state.render_sentence()
        ));
    }

    if stop_hook_active {
        // Already blocked once this stop sequence. Blocking again risks
        // wedging a session on a judgement the agent has deliberately made
        // (leftover fixture files it does not intend to commit), so downgrade
        // to an advisory that still puts the state in the transcript.
        return Decision::Advise(format!(
            "Work state (#8267): {} — STILL uncommitted after one block; allowing the stop. \
             If any of these are deliverables, they will only exist on this host: {}",
            state.render_sentence(),
            render_paths(state),
        ));
    }

    Decision::Block(format!(
        "STOP BLOCKED (worktree-state, issue #8267): you are ending your turn with unsaved work. \
         {}. Uncommitted files: {}. \
         A completion report is indistinguishable from a lost one — two incidents lost an entire \
         deliverable this way (a zero-commit branch, and a 52 KB tool left untracked). \
         Do ONE of these, then end your turn: (1) commit the deliverables in {} and push \
         (`git -C {} add -A && git -C {} commit` — a PR needs a pushed branch); \
         (2) if they are genuinely intermediate, say so explicitly in your final message, naming \
         them — this guard blocks at most once per stop, so restating and stopping again \
         will proceed; (3) if you concluded no changes are needed, write the \
         `.no-changes-needed` marker (it is excluded from this check) and stop.",
        state.render_sentence(),
        render_paths(state),
        state.path.display(),
        state.path.display(),
        state.path.display(),
    ))
}

/// The at-risk path list, capped, with an honest `+N more` suffix.
fn render_paths(state: &WorktreeState) -> String {
    let total = state.uncommitted + state.untracked;
    let shown = state.at_risk_paths.len() as u32;
    let mut s = state.at_risk_paths.join(", ");
    if total > shown {
        s.push_str(&format!(" (+{} more)", total - shown));
    }
    if s.is_empty() {
        s.push_str("(paths unavailable)");
    }
    s
}

/// End-to-end: payload in, decision out.
///
/// `base_ref` is the branch point to count commits against (`origin/main` for
/// every Loom worktree today).
#[must_use]
pub fn evaluate(payload: &HookPayload, base_ref: &str) -> Decision {
    let Some(worktree) = owned_worktree(payload) else {
        return Decision::Silent;
    };
    if !guard_enabled(&worktree) {
        return Decision::Silent;
    }
    let state = collect(&worktree, base_ref);
    decide(&state, payload.stop_hook_active)
}
