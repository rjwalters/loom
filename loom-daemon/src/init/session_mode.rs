//! Install-time workspace mode — `loom-daemon init --mode <MODE>` (issue #8884).
//!
//! A repo run in **session mode** is one where every Loom role is invoked by an
//! attended operator: no tmux agent pool, no daemon-tier work generation. Before
//! #8884 that intent had no install-time expression — a fresh install always
//! wrote `defaults/config.json`'s four-terminal array, and somebody had to
//! remember never to start the pool.
//!
//! The mode is expressed as ONE persisted marker, the top-level `"mode"` key in
//! `.loom/config.json`, and everything else is derived from it:
//!
//! | key | session-mode value | what it disables |
//! |---|---|---|
//! | `terminals` | `[]` | the tmux agent pool — `loom-start.sh`'s `check_config()` already refuses to start on an empty array (it has since the config-tiering work), so session mode needs no new guard |
//! | `autonomous.roleRunner.enabled` | `false` | the daemon-native role runner |
//! | `autonomous.workFinder.enabled` | `false` | the daemon's autonomous work finder |
//!
//! Why config.json rather than `install-metadata.json`: the marker has to
//! survive `loom update` / `resync-installed.sh`, and `.loom/config.json` is the
//! one installed file that already has exactly that property — resync never
//! touches it at all, and a reinstall runs it through `merge_config_file`'s
//! existing-values-win deep merge. A second copy in `install-metadata.json`
//! would add a way for the two to disagree and buy nothing.
//!
//! **Session mode is re-asserted, not merely preserved.** Every `init` that sees
//! the marker (with or without `--mode session` on the command line) re-applies
//! the table above, so a template change can never quietly hand an
//! unattended-agent capability back to a session-mode repo. The corollary is
//! that leaving session mode is an explicit edit — remove the `"mode"` key from
//! `.loom/config.json`; `--mode default` is the *absence* of the flag, not an
//! undo for a marker already on disk.
//!
//! **Scope (the open question #8884 left to the implementer).** These are
//! install-time config writes, not runtime vetoes: nothing in `loom-daemon`
//! refuses to honour `autonomous.roleRunner.enabled: true` if an operator later
//! sets it back by hand. That residual gap is documented in
//! `defaults/docs/session-mode.md` rather than closed here, because a runtime
//! veto is a behaviour change in the daemon's config resolution, not an
//! install-time one.

use std::path::Path;

use serde_json::{json, Value};

/// Install-time workspace mode, selected by `loom-daemon init --mode <MODE>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum InstallMode {
    /// Stock install: the shipped `terminals` array is written as-is and the
    /// `autonomous` block is left entirely to the consumer.
    #[default]
    Default,
    /// Attended-operator install: no tmux agent pool, no daemon-tier work
    /// generation. See the module docs for the exact key set.
    Session,
}

impl InstallMode {
    /// True for [`InstallMode::Session`].
    pub fn is_session(self) -> bool {
        matches!(self, InstallMode::Session)
    }
}

/// Top-level `.loom/config.json` key carrying the persisted mode.
pub const MODE_KEY: &str = "mode";

/// The one non-default value [`MODE_KEY`] takes today.
pub const SESSION_MODE: &str = "session";

/// True when `config` already carries the persisted session-mode marker.
///
/// Deliberately keyed on the marker alone and never inferred from an empty
/// `terminals` array: an empty array is already a meaningful, unrelated state
/// (an operator driving the session tools by hand with no terminals
/// configured), and inferring session mode from it would opt repos in that
/// never asked.
pub fn is_session_config(config: &Value) -> bool {
    config.get(MODE_KEY).and_then(Value::as_str) == Some(SESSION_MODE)
}

/// True when this `init` must write a session-mode config: the operator passed
/// `--mode session`, or the config already on disk carries the marker.
pub fn session_mode_applies(mode: InstallMode, existing: Option<&Value>) -> bool {
    mode.is_session() || existing.is_some_and(is_session_config)
}

/// Assert the session-mode key set on a parsed config object, in place.
///
/// Returns `false` (and changes nothing) when `config` is not a JSON object,
/// which is the caller's signal that the requested mode could not be honoured.
///
/// Nested writes create only the path they need: an existing `autonomous` block
/// keeps every sibling key, and only the two `enabled` leaves are overwritten.
pub fn apply_session_mode(config: &mut Value) -> bool {
    let Some(map) = config.as_object_mut() else {
        return false;
    };
    map.insert(MODE_KEY.to_string(), Value::String(SESSION_MODE.to_string()));
    map.insert("terminals".to_string(), Value::Array(Vec::new()));

    let autonomous = map.entry("autonomous").or_insert_with(|| json!({}));
    if !autonomous.is_object() {
        *autonomous = json!({});
    }
    for generator in ["roleRunner", "workFinder"] {
        let Some(auto_map) = autonomous.as_object_mut() else {
            return false;
        };
        let slot = auto_map.entry(generator).or_insert_with(|| json!({}));
        if !slot.is_object() {
            *slot = json!({});
        }
        if let Some(slot_map) = slot.as_object_mut() {
            slot_map.insert("enabled".to_string(), Value::Bool(false));
        }
    }
    true
}

/// Write `value` to `dst` through the canonical pretty-print path every other
/// `.loom/config.json` write uses, so a later reinstall's merge re-emits
/// byte-identical output and the file is never left dirty (issue #3619).
pub fn write_pretty_json(dst: &Path, value: &Value) -> Result<(), String> {
    let mut serialized = serde_json::to_string_pretty(value)
        .map_err(|e| format!("Failed to serialize {}: {e}", dst.display()))?;
    serialized.push('\n');
    std::fs::write(dst, serialized).map_err(|e| format!("Failed to write {}: {e}", dst.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_not_session() {
        assert!(!InstallMode::default().is_session());
        assert!(InstallMode::Session.is_session());
    }

    #[test]
    fn marker_detection_requires_the_mode_key() {
        assert!(is_session_config(&json!({"mode": "session"})));
        assert!(!is_session_config(&json!({"mode": "default"})));
        assert!(!is_session_config(&json!({})));
        // #8884 AC: never inferred from an empty terminals array alone.
        assert!(!is_session_config(&json!({"terminals": []})));
    }

    #[test]
    fn session_mode_applies_from_flag_or_persisted_marker() {
        let session = json!({"mode": "session"});
        let plain = json!({"terminals": []});
        assert!(session_mode_applies(InstallMode::Session, None));
        assert!(session_mode_applies(InstallMode::Default, Some(&session)));
        assert!(!session_mode_applies(InstallMode::Default, Some(&plain)));
        assert!(!session_mode_applies(InstallMode::Default, None));
    }

    #[test]
    fn apply_writes_the_whole_key_set() {
        let mut config = json!({
            "version": "2",
            "terminals": [{"id": "terminal-1"}, {"id": "terminal-2"}],
        });
        assert!(apply_session_mode(&mut config));
        assert_eq!(config["mode"], json!("session"));
        assert_eq!(config["terminals"], json!([]));
        assert_eq!(config["autonomous"]["roleRunner"]["enabled"], json!(false));
        assert_eq!(config["autonomous"]["workFinder"]["enabled"], json!(false));
        // Untouched consumer keys survive.
        assert_eq!(config["version"], json!("2"));
    }

    #[test]
    fn apply_preserves_sibling_autonomous_keys() {
        let mut config = json!({
            "autonomous": {
                "roleRunner": {"enabled": true, "roles": ["judge"]},
                "workFinder": {"enabled": true, "maxConcurrent": 3},
                "epicSupervisor": {"enabled": true},
            }
        });
        assert!(apply_session_mode(&mut config));
        assert_eq!(config["autonomous"]["roleRunner"]["enabled"], json!(false));
        assert_eq!(config["autonomous"]["roleRunner"]["roles"], json!(["judge"]));
        assert_eq!(config["autonomous"]["workFinder"]["enabled"], json!(false));
        assert_eq!(config["autonomous"]["workFinder"]["maxConcurrent"], json!(3));
        // A generator session mode says nothing about is left exactly as found.
        assert_eq!(config["autonomous"]["epicSupervisor"], json!({"enabled": true}));
    }

    #[test]
    fn apply_repairs_a_non_object_autonomous_block() {
        let mut config = json!({"autonomous": "nonsense"});
        assert!(apply_session_mode(&mut config));
        assert_eq!(config["autonomous"]["workFinder"]["enabled"], json!(false));
    }

    #[test]
    fn apply_is_idempotent() {
        let mut once = json!({"terminals": [{"id": "terminal-1"}]});
        assert!(apply_session_mode(&mut once));
        let mut twice = once.clone();
        assert!(apply_session_mode(&mut twice));
        assert_eq!(once, twice);
    }

    #[test]
    fn apply_refuses_a_non_object_config() {
        let mut config = json!([1, 2, 3]);
        assert!(!apply_session_mode(&mut config));
        assert_eq!(config, json!([1, 2, 3]));
    }
}
