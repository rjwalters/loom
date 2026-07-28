//! Rust port of `.loom/spawn-loop-state.json` reading
//! (`loom_tools.models.spawn_loop_state.SpawnLoopState` +
//! `loom_tools.common.state.read_spawn_loop_state`).
//!
//! `spawn-loop.sh` — the historical writer — was deleted in v0.11.0, so in
//! practice this file is never present on a live workspace today. It is
//! ported anyway for byte-for-byte behavioral parity with the Python
//! original: `gather_liveness_evidence` (in `orphan_recovery.rs`) treats a
//! *present* state file as one of several unioned liveness sources, and a
//! missing file as simply "not a contributing source" (never an error).

use std::path::Path;

use serde::Deserialize;

/// A single sweep child tracked by the (retired) spawn loop.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SpawnLoopTask {
    #[serde(default)]
    pub issue: u32,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub last_heartbeat: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SpawnLoopStateFile {
    #[serde(default)]
    running: Vec<SpawnLoopTask>,
}

/// Parsed contents of `.loom/spawn-loop-state.json`. `present` is `true` only
/// when the file existed (whether or not it parsed cleanly) — mirrors the
/// Python `SpawnLoopState.present` semantics exactly, including "present but
/// malformed" collapsing to an empty `running` list rather than an error.
#[derive(Debug, Clone, Default)]
pub struct SpawnLoopState {
    pub running: Vec<SpawnLoopTask>,
    pub present: bool,
}

/// Load `<repo_root>/.loom/spawn-loop-state.json`. Returns `present: false`
/// when the file is missing (the overwhelmingly common case post-v0.11.0).
#[must_use]
pub fn read_spawn_loop_state(repo_root: &Path) -> SpawnLoopState {
    let path = repo_root.join(".loom").join("spawn-loop-state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return SpawnLoopState::default();
    };
    match serde_json::from_str::<SpawnLoopStateFile>(&text) {
        Ok(parsed) => SpawnLoopState {
            running: parsed.running,
            present: true,
        },
        Err(_) => SpawnLoopState {
            running: Vec::new(),
            present: true,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_is_absent() {
        let dir = tempdir().unwrap();
        let state = read_spawn_loop_state(dir.path());
        assert!(!state.present);
        assert!(state.running.is_empty());
    }

    #[test]
    fn present_file_parses_running_tasks() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom").join("spawn-loop-state.json"),
            r#"{"started_at": "2026-01-01T00:00:00Z", "running": [{"issue": 42, "pid": 111}]}"#,
        )
        .unwrap();
        let state = read_spawn_loop_state(dir.path());
        assert!(state.present);
        assert_eq!(state.running.len(), 1);
        assert_eq!(state.running[0].issue, 42);
        assert_eq!(state.running[0].pid, 111);
    }

    #[test]
    fn present_but_malformed_file_is_present_with_empty_running() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(dir.path().join(".loom").join("spawn-loop-state.json"), "not json").unwrap();
        let state = read_spawn_loop_state(dir.path());
        assert!(state.present);
        assert!(state.running.is_empty());
    }
}
