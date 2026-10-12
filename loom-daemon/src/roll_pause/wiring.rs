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
//! Nothing is added only when the settings Claude Code will already load
//! (the launch directory's `.claude/settings.json` and
//! `.claude/settings.local.json`, plus the user's `settings.json`) run
//! `roll-pause.sh` for every one of those events with a match-all matcher, as
//! the Loom repo's committed settings do. Any gap (an event left out, or a
//! matcher narrowed to some tools) gets the full injection: a second firing
//! for an event that was already covered is harmless, because the ledger,
//! park and safe-point writes are idempotent per tool call, while a missed
//! call leaves the session unable to reach a safe point.

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

/// Whether a hook entry's matcher selects every tool. Claude Code treats an
/// absent, empty or `*` matcher as match-all; `.*` is the same as a regex.
fn matches_all_tools(matcher: Option<&serde_json::Value>) -> bool {
    match matcher {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(m)) => matches!(m.trim(), "" | "*" | ".*"),
        Some(_) => false,
    }
}

/// Whether `settings` runs `roll-pause.sh` on `event` for every tool.
fn covers_event(settings: &serde_json::Value, event: &str) -> bool {
    settings
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                matches_all_tools(entry.get("matcher"))
                    && entry
                        .get("hooks")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|hooks| {
                            hooks.iter().any(|h| {
                                h.get("command")
                                    .and_then(serde_json::Value::as_str)
                                    .is_some_and(|c| c.contains("roll-pause.sh"))
                            })
                        })
            })
        })
}

/// The settings files Claude Code merges hooks from for a launch in
/// `launch_dir`: the two project files, then the user's `settings.json`
/// under `user_config_dir` when one is known.
#[must_use]
pub fn settings_files(launch_dir: &Path, user_config_dir: Option<&Path>) -> Vec<PathBuf> {
    PROJECT_SETTINGS
        .iter()
        .map(|rel| launch_dir.join(rel))
        .chain(user_config_dir.map(|d| d.join("settings.json")))
        .collect()
}

/// Whether `files`, merged as Claude Code merges hooks, already run
/// `roll-pause.sh` for every tool on every one of [`EVENTS`]. A file that is
/// missing or not JSON contributes nothing.
#[must_use]
pub fn fully_wired(files: &[PathBuf]) -> bool {
    let parsed: Vec<serde_json::Value> = files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect();
    EVENTS
        .iter()
        .all(|event| parsed.iter().any(|v| covers_event(v, event)))
}

/// The user-scope Claude config directory: `CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
fn user_config_dir() -> Option<PathBuf> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
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
/// never paused), a hook script resolves, and the settings Claude Code will
/// load for `launch_dir` (its project files and the user settings under
/// `user_config_dir`) do not already wire it fully (see [`fully_wired`]).
#[must_use]
pub fn settings_args(
    item: Option<&str>,
    workspace_root: &Path,
    launch_dir: &Path,
    user_config_dir: Option<&Path>,
    override_path: Option<&Path>,
) -> Vec<String> {
    if !item.is_some_and(valid_item_id) || fully_wired(&settings_files(launch_dir, user_config_dir))
    {
        return Vec::new();
    }
    match resolve_hook(workspace_root, override_path) {
        Some(hook) => vec!["--settings".to_string(), settings_json(&hook)],
        None => Vec::new(),
    }
}

/// [`settings_args`] from the process environment: the item from
/// [`ITEM_ENV`], the workspace from `LOOM_PROJECT_ROOT` or the git main
/// checkout of the cwd, the launch dir from the cwd, the user settings from
/// `CLAUDE_CONFIG_DIR` or `~/.claude`.
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
        user_config_dir().as_deref(),
        var(HOOK_PATH_ENV).map(PathBuf::from).as_deref(),
    )
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
