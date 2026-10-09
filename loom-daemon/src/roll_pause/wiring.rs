//! The pause hook's Claude Code wiring, injected at launch (issue #11049).
//!
//! # Why the launch carries it
//!
//! The roll-pause hook only parks a session when the harness runs it. The
//! Loom repo wires it in its own committed `.claude/settings.json`, but a
//! consumer repo's `.claude/settings.json` is the consumer's file. Only
//! `install.sh` writes hook entries there (`ensure_project_hook_wiring`). The
//! daemon's workspace resync (#10718, #11027) refreshes `.loom/hooks/` and
//! `.loom/scripts/` but never edits that file, so every consumer repo
//! installed before #10830 got `roll-pause.sh` without a single entry that
//! runs it. In the 0.19.948 → 0.19.950 roll, every sweep in a consumer repo
//! missed its safe point, and the one sweep in the Loom repo parked.
//!
//! So a daemon-dispatched Claude launch carries the wiring itself:
//! `loom-daemon agent-resume claude-args` (which `spawn-claude.sh` already
//! calls for every pinned or resumed session) appends `--settings <json>`
//! with match-all `PreToolUse`, `PostToolUse` and `PostToolUseFailure`
//! entries for the installed `roll-pause.sh`. Claude Code merges hooks from
//! `--settings` with the project's and the user's, so nothing the consumer
//! configured is replaced, and no consumer-owned file is written.
//!
//! When the launch directory's own settings already run `roll-pause.sh` (the
//! Loom repo), nothing is added: one firing per event is enough. A second one
//! would also be harmless, because the ledger, park and safe-point writes are
//! idempotent per tool call.

use std::path::{Path, PathBuf};

use super::{valid_item_id, ITEM_ENV};

/// Names the `roll-pause.sh` to wire, overriding the lookup (tests, or a
/// launch whose hooks live elsewhere).
pub const HOOK_PATH_ENV: &str = "LOOM_ROLL_PAUSE_HOOK";

/// The hook script, relative to a workspace root, in lookup order: an
/// installed consumer copy, then the Loom source tree's own.
const HOOK_CANDIDATES: [&str; 2] = [".loom/hooks/roll-pause.sh", "defaults/hooks/roll-pause.sh"];

/// The project settings files Claude Code reads from its launch directory.
const PROJECT_SETTINGS: [&str; 2] = [".claude/settings.json", ".claude/settings.local.json"];

/// The hook events the wiring covers: the park and ledger open on
/// `PreToolUse`, the ledger closes on the other two.
const EVENTS: [&str; 3] = ["PreToolUse", "PostToolUse", "PostToolUseFailure"];

/// The `roll-pause.sh` a launch should wire: `override_path` when it names a
/// file, else the first of [`HOOK_CANDIDATES`] under `workspace_root`.
#[must_use]
pub fn resolve_hook(workspace_root: &Path, override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return p.is_file().then(|| p.to_path_buf());
    }
    HOOK_CANDIDATES
        .iter()
        .map(|rel| workspace_root.join(rel))
        .find(|p| p.is_file())
}

/// Whether the project settings in `launch_dir` already run `roll-pause.sh`
/// from a hook entry.
#[must_use]
pub fn project_wires_hook(launch_dir: &Path) -> bool {
    PROJECT_SETTINGS.iter().any(|rel| {
        std::fs::read_to_string(launch_dir.join(rel))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("hooks").map(ToString::to_string))
            .is_some_and(|hooks| hooks.contains("roll-pause.sh"))
    })
}

/// `s` as one POSIX shell word.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `--settings` JSON that wires `hook` to every tool call.
///
/// `exec` keeps the hook's parent the harness, so the `--harness-pid` the
/// hook records is Claude's own pid, as it is for the installer's entries.
#[must_use]
pub fn settings_json(hook: &Path) -> String {
    let command = format!("exec bash {}", shell_quote(&hook.to_string_lossy()));
    let entry = serde_json::json!([{
        "matcher": "*",
        "hooks": [{"type": "command", "command": command}],
    }]);
    let hooks: serde_json::Map<String, serde_json::Value> = EVENTS
        .iter()
        .map(|e| ((*e).to_string(), entry.clone()))
        .collect();
    serde_json::json!({ "hooks": hooks }).to_string()
}

/// The `--settings` arguments for a launch, or nothing.
///
/// Nothing unless `item` is a valid daemon item id (an attended launch is
/// never paused), a hook script resolves, and `launch_dir`'s project
/// settings do not already wire it.
#[must_use]
pub fn settings_args(
    item: Option<&str>,
    workspace_root: &Path,
    launch_dir: &Path,
    override_path: Option<&Path>,
) -> Vec<String> {
    if !item.is_some_and(valid_item_id) || project_wires_hook(launch_dir) {
        return Vec::new();
    }
    match resolve_hook(workspace_root, override_path) {
        Some(hook) => vec!["--settings".to_string(), settings_json(&hook)],
        None => Vec::new(),
    }
}

/// [`settings_args`] from the process environment: the item from
/// [`ITEM_ENV`], the workspace from `LOOM_PROJECT_ROOT` or the git main
/// checkout of the cwd, the launch dir from the cwd.
#[must_use]
pub fn settings_args_from_env() -> Vec<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let item = var(ITEM_ENV);
    if !item.as_deref().is_some_and(valid_item_id) {
        return Vec::new();
    }
    settings_args(
        item.as_deref(),
        &super::project_root(),
        &cwd,
        var(HOOK_PATH_ENV).map(PathBuf::from).as_deref(),
    )
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
