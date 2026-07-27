//! Per-account consecutive-failure counter, ported from
//! `loom_tools.tokens.failure_counts`.
//!
//! Tracks consecutive `TOKEN_EXHAUSTED` failures per account so the spawn
//! wrapper can auto-unpin a stuck pin. Stored at `.loom/tokens/.failure_counts`
//! as JSON: `{"<account_name>": {"count": <int>, "last_failure": "<ISO8601 UTC>"}}`.
//!
//! The counter is reset on a successful spawn for that account
//! ([`record_success`]) or any operator allowlist mutation ([`reset_all`]).
//! Reaching the threshold (default 5) is checked `>= 5` — idempotent
//! at-or-above, matching the Python semantics exactly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::allowlist::atomic_write;
use super::locking::MkdirLock;
use super::paths::resolve_tokens_dir;

pub const DEFAULT_THRESHOLD: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    pub count: u32,
    pub last_failure: String,
}

type State = BTreeMap<String, FailureEntry>;

fn tokens_dir(workspace: &Path) -> PathBuf {
    resolve_tokens_dir(workspace)
}

fn state_path(workspace: &Path) -> PathBuf {
    tokens_dir(workspace).join(".failure_counts")
}

fn lock_path(workspace: &Path) -> PathBuf {
    tokens_dir(workspace).join(".failure_counts.lock")
}

/// Read the JSON state file, returning an empty map on any failure — a
/// malformed/unreadable file is treated as empty rather than a hard error
/// (matches Python: better to lose history than refuse to record failures).
fn read_state(path: &Path) -> State {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return State::new();
    };
    if raw.trim().is_empty() {
        return State::new();
    }
    // Parse defensively: malformed entries (non-object values, negative or
    // non-integer counts) are dropped rather than propagated.
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return State::new();
    };
    let mut cleaned = State::new();
    for (k, v) in map {
        let serde_json::Value::Object(obj) = v else {
            continue;
        };
        let count = match obj.get("count") {
            Some(serde_json::Value::Number(n)) if n.is_i64() || n.is_u64() => {
                let c = n.as_i64().unwrap_or(-1);
                if c < 0 {
                    continue;
                }
                c as u32
            }
            _ => continue,
        };
        let last_failure = obj
            .get("last_failure")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        cleaned.insert(
            k,
            FailureEntry {
                count,
                last_failure,
            },
        );
    }
    cleaned
}

fn write_state(path: &Path, state: &State) -> Result<(), String> {
    let mut body = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    body.push('\n');
    atomic_write(path, &body)
}

fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Increment the consecutive-failure counter for `name`, capped at
/// `threshold`. Returns the new (post-increment) count.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn record_failure(workspace: &Path, name: &str, threshold: u32) -> Result<u32, String> {
    let dir = tokens_dir(workspace);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = state_path(workspace);
    let _lock = MkdirLock::acquire(&lock_path(workspace))?;
    let mut state = read_state(&path);
    let prev = state.get(name).map_or(0, |e| e.count);
    let new_count = (prev + 1).min(threshold);
    state.insert(
        name.to_string(),
        FailureEntry {
            count: new_count,
            last_failure: now_iso(),
        },
    );
    write_state(&path, &state)?;
    Ok(new_count)
}

/// Reset the counter for `name` (drops the key entirely). No-op if the state
/// file is missing or the key is absent.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn record_success(workspace: &Path, name: &str) -> Result<(), String> {
    let path = state_path(workspace);
    if !path.is_file() {
        return Ok(());
    }
    let _lock = MkdirLock::acquire(&lock_path(workspace))?;
    let mut state = read_state(&path);
    if state.remove(name).is_none() {
        return Ok(());
    }
    if state.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        write_state(&path, &state)?;
    }
    Ok(())
}

/// Drop all per-account counters. No-op if the state file is missing.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn reset_all(workspace: &Path) -> Result<(), String> {
    let path = state_path(workspace);
    if !path.is_file() {
        return Ok(());
    }
    let _lock = MkdirLock::acquire(&lock_path(workspace))?;
    match std::fs::remove_file(&path) {
        Ok(()) | Err(_) => Ok(()),
    }
}

/// Return the current consecutive-failure count for `name` (0 if absent).
#[must_use]
pub fn get_count(workspace: &Path, name: &str) -> u32 {
    let path = state_path(workspace);
    if !path.is_file() {
        return 0;
    }
    read_state(&path).get(name).map_or(0, |e| e.count)
}

/// Return `true` iff the counter for `name` is `>= threshold`.
#[must_use]
pub fn threshold_reached(workspace: &Path, name: &str, threshold: u32) -> bool {
    get_count(workspace, name) >= threshold
}

/// Return the entire state map (for status/debugging).
#[must_use]
pub fn all_counts(workspace: &Path) -> State {
    let path = state_path(workspace);
    if !path.is_file() {
        return State::new();
    }
    read_state(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_pool() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        // resolve_tokens_dir() only picks the per-repo pool when it holds at
        // least one `*.token` file — seed one so these tests deterministically
        // exercise the per-repo pool rather than falling back to this host's
        // real shared pool (~/.loom/tokens) when it is empty.
        fs::write(dir.join("seed.token"), "sk-ant-oat01-fake").unwrap();
        tmp
    }

    #[test]
    fn record_failure_increments() {
        let tmp = make_pool();
        assert_eq!(record_failure(tmp.path(), "a", 5).unwrap(), 1);
        assert_eq!(record_failure(tmp.path(), "a", 5).unwrap(), 2);
        assert_eq!(get_count(tmp.path(), "a"), 2);
    }

    #[test]
    fn record_failure_caps_at_threshold() {
        let tmp = make_pool();
        for _ in 0..10 {
            record_failure(tmp.path(), "a", 5).unwrap();
        }
        assert_eq!(get_count(tmp.path(), "a"), 5);
    }

    #[test]
    fn threshold_reached_is_at_or_above() {
        let tmp = make_pool();
        for _ in 0..5 {
            record_failure(tmp.path(), "a", 5).unwrap();
        }
        assert!(threshold_reached(tmp.path(), "a", 5));
        // A 6th failure (capped) still triggers — idempotent at-or-above.
        record_failure(tmp.path(), "a", 5).unwrap();
        assert!(threshold_reached(tmp.path(), "a", 5));
    }

    #[test]
    fn record_success_drops_key() {
        let tmp = make_pool();
        record_failure(tmp.path(), "a", 5).unwrap();
        record_success(tmp.path(), "a").unwrap();
        assert_eq!(get_count(tmp.path(), "a"), 0);
    }

    #[test]
    fn record_success_removes_file_when_last_key_dropped() {
        let tmp = make_pool();
        record_failure(tmp.path(), "a", 5).unwrap();
        record_success(tmp.path(), "a").unwrap();
        assert!(!state_path(tmp.path()).exists());
    }

    #[test]
    fn reset_all_clears_state_file() {
        let tmp = make_pool();
        record_failure(tmp.path(), "a", 5).unwrap();
        record_failure(tmp.path(), "b", 5).unwrap();
        reset_all(tmp.path()).unwrap();
        assert!(!state_path(tmp.path()).exists());
        assert_eq!(get_count(tmp.path(), "a"), 0);
    }

    #[test]
    fn malformed_state_file_reads_as_empty() {
        let tmp = make_pool();
        fs::write(state_path(tmp.path()), "not json").unwrap();
        assert_eq!(get_count(tmp.path(), "a"), 0);
        assert!(all_counts(tmp.path()).is_empty());
    }

    #[test]
    fn negative_counts_are_dropped_defensively() {
        let tmp = make_pool();
        fs::write(state_path(tmp.path()), r#"{"a": {"count": -1, "last_failure": ""}}"#).unwrap();
        assert_eq!(get_count(tmp.path(), "a"), 0);
    }
}
