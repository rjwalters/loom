//! Persisted, machine-level sweep liveness journal (Issue #3953).
//!
//! ## Problem
//!
//! Before this module, the daemon's ONLY liveness record for a dispatched
//! sweep was the in-memory [`crate::sweep_registry::SweepRegistry`] entry. A
//! daemon restart — a routine event in the multi-repo autonomous world (rate
//! limit kills, the print-mode ceiling, an operator upgrade) — wipes that
//! registry clean. `loom-recover-orphans --recover` (the Python reclaim tool,
//! `loom_tools.orphan_recovery`) then finds **no authoritative liveness
//! source** and, per its #3651 fail-safe, refuses to reclaim ANY
//! `loom:building` claim — even a claim whose sweep process is provably dead.
//! Stale claims accumulate and an operator ends up hand-flipping labels.
//!
//! ## Fix
//!
//! This module persists a minimal `{repo, issue, pid, started_at}` record for
//! every dispatched sweep to a single machine-level file, `~/.loom/sweeps.json`
//! (override via [`JOURNAL_PATH_ENV`]). Unlike the in-memory registry, this
//! file **survives a daemon restart** — it is the authoritative liveness
//! source `loom-recover-orphans` was missing. The dead-PID-pruning pattern
//! mirrors `defaults/scripts/sweep-run-registry.sh`'s `peers`/`prune_dead`
//! (Issue #3768): a live PID keeps its entry, a dead PID's entry is dropped
//! the next time anything reads or writes the journal — so the file never
//! accumulates a `pid`-graveyard of every sweep that has ever run.
//!
//! ## Who writes it
//!
//! [`crate::sweep_registry::SweepRegistry::dispatch`] calls [`record_sweep`]
//! right after a child is spawned (mirrors the fields already tracked on
//! [`crate::types::SweepInfo`]: `repo`, the issue number from
//! [`crate::types::SweepKind::Issue`], `pid`, `started_at`). The reaper's
//! dead-PID paths call [`remove_sweep`] as a best-effort tidy-up (not
//! load-bearing for correctness — the next `record_sweep` or reconcile pass
//! prunes it anyway, but it keeps the file small).
//!
//! ## Who reads it
//!
//! - The Rust startup reconciliation pass ([`crate::claim_reconciliation`])
//!   consults it directly, in-process.
//! - `loom_tools.orphan_recovery` (Python) reads the same JSON file as a new
//!   liveness source, so a *manual* `loom-recover-orphans --recover` run also
//!   benefits from a fresh daemon's journal.
//!
//! All I/O here is best-effort by design: a missing, empty, or corrupt
//! journal degrades to an empty [`SweepJournal`] rather than propagating an
//! error that could block a dispatch or a reconciliation pass.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Environment override for the journal file location (mirrors
/// [`crate::workspace_registry::REGISTRY_PATH_ENV`]). Primarily a test seam.
pub const JOURNAL_PATH_ENV: &str = "LOOM_SWEEPS_JOURNAL_PATH";

/// Current on-disk schema version.
pub const JOURNAL_VERSION: u32 = 1;

fn default_version() -> u32 {
    JOURNAL_VERSION
}

/// One journal record: everything a liveness consumer needs to decide whether
/// a `(repo, issue)` claim still has a live sweep behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The owning workspace root, formatted identically to
    /// [`crate::types::SweepInfo::repo`] (`workspace_root.display().to_string()`)
    /// so journal lookups key on the exact same string dispatch stamps.
    pub repo: String,
    /// The GitHub/Gitea issue number this sweep is working.
    pub issue: u32,
    /// PID of the detached sweep child process.
    pub pid: u32,
    /// Timestamp of the original spawn.
    pub started_at: DateTime<Utc>,
}

/// The full on-disk journal: a flat list of [`JournalEntry`] records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepJournal {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<JournalEntry>,
}

impl Default for SweepJournal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Resolve the journal file path: [`JOURNAL_PATH_ENV`] override (non-empty),
/// else `~/.loom/sweeps.json`.
pub fn default_journal_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(JOURNAL_PATH_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".loom").join("sweeps.json"))
}

/// Load the journal from `path`. Tolerant by design: a missing file yields an
/// empty journal, and unreadable/corrupt contents log a warning and ALSO
/// yield an empty journal rather than propagating an error — a garbled
/// journal must never block a dispatch or a reconciliation pass (mirrors the
/// fail-safe philosophy of issue #3651: absent/bad liveness data is not proof
/// of anything, so callers fall back to "no journal evidence" rather than
/// erroring out).
#[must_use]
pub fn load(path: &Path) -> SweepJournal {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return SweepJournal::default();
            }
            match serde_json::from_str(&contents) {
                Ok(journal) => journal,
                Err(e) => {
                    log::warn!(
                        "sweep_journal: corrupt journal at {} ({e}) — treating as empty",
                        path.display()
                    );
                    SweepJournal::default()
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SweepJournal::default(),
        Err(e) => {
            log::warn!(
                "sweep_journal: failed to read {} ({e}) — treating as empty",
                path.display()
            );
            SweepJournal::default()
        }
    }
}

/// Persist the journal to `path` atomically (write to a sibling temp file,
/// then rename) so a concurrent reader never observes a half-written file.
/// Creates the parent directory if needed.
pub fn save(path: &Path, journal: &SweepJournal) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating journal dir {}", parent.display()))?;
    }
    let mut json = serde_json::to_string_pretty(journal)?;
    json.push('\n');

    // Temp file in the same directory guarantees the rename is atomic (same
    // filesystem). Include the PID to avoid collisions between concurrent
    // writers (mirrors `WorkspaceRegistry::save`).
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("writing temp journal {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Prune entries whose `pid` — per `is_alive` — is no longer live. Returns the
/// number of entries removed.
///
/// This is the dead-PID-pruning step mirrored from
/// `sweep-run-registry.sh`'s `prune_dead` / `peers` (#3768): callers run this
/// at both write time ([`upsert`]) and read time (the reconciliation pass in
/// [`crate::claim_reconciliation`]), so a crashed sweep's entry never survives
/// past the next journal touch.
pub fn prune_dead(journal: &mut SweepJournal, is_alive: impl Fn(u32) -> bool) -> usize {
    let before = journal.entries.len();
    journal.entries.retain(|e| is_alive(e.pid));
    before - journal.entries.len()
}

/// Find the entry for `(repo, issue)`, if any.
#[must_use]
pub fn find<'a>(journal: &'a SweepJournal, repo: &str, issue: u32) -> Option<&'a JournalEntry> {
    journal
        .entries
        .iter()
        .find(|e| e.repo == repo && e.issue == issue)
}

/// Insert or replace the entry keyed by `(repo, issue)`. Prunes dead entries
/// first so the journal never grows unbounded across a long-lived daemon's
/// lifetime.
pub fn upsert(journal: &mut SweepJournal, entry: JournalEntry, is_alive: impl Fn(u32) -> bool) {
    prune_dead(journal, is_alive);
    journal
        .entries
        .retain(|e| !(e.repo == entry.repo && e.issue == entry.issue));
    journal.entries.push(entry);
}

/// Remove the entry for `(repo, issue)` if present. Returns `true` if an
/// entry was actually removed.
pub fn remove(journal: &mut SweepJournal, repo: &str, issue: u32) -> bool {
    let before = journal.entries.len();
    journal
        .entries
        .retain(|e| !(e.repo == repo && e.issue == issue));
    before != journal.entries.len()
}

/// High-level convenience: record a freshly-dispatched sweep in the journal
/// at an explicit `path` (test seam / per-registry override — see
/// [`crate::sweep_registry::SweepRegistryConfig::journal_path`]). Best-effort
/// by contract — callers (`SweepRegistry::dispatch`) log a warning on `Err`
/// but never fail dispatch because of a journal-write hiccup.
pub fn record_sweep_at(
    path: &Path,
    repo: &str,
    issue: u32,
    pid: u32,
    started_at: DateTime<Utc>,
) -> Result<()> {
    let mut journal = load(path);
    upsert(
        &mut journal,
        JournalEntry {
            repo: repo.to_string(),
            issue,
            pid,
            started_at,
        },
        crate::sweep_registry::is_pid_alive,
    );
    save(path, &journal)
}

/// Convenience: [`record_sweep_at`] the default (env-overridable) path.
pub fn record_sweep(repo: &str, issue: u32, pid: u32, started_at: DateTime<Utc>) -> Result<()> {
    record_sweep_at(&default_journal_path()?, repo, issue, pid, started_at)
}

/// High-level convenience: remove a sweep's journal entry (e.g. on reap or
/// cancel) at an explicit `path`. Best-effort; a no-op write is skipped.
pub fn remove_sweep_at(path: &Path, repo: &str, issue: u32) -> Result<()> {
    let mut journal = load(path);
    if remove(&mut journal, repo, issue) {
        save(path, &journal)?;
    }
    Ok(())
}

/// Convenience: [`remove_sweep_at`] the default (env-overridable) path.
pub fn remove_sweep(repo: &str, issue: u32) -> Result<()> {
    remove_sweep_at(&default_journal_path()?, repo, issue)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn entry(repo: &str, issue: u32, pid: u32) -> JournalEntry {
        JournalEntry {
            repo: repo.to_string(),
            issue,
            pid,
            started_at: Utc::now(),
        }
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweeps.json");
        let journal = load(&path);
        assert!(journal.entries.is_empty());
        assert_eq!(journal.version, JOURNAL_VERSION);
    }

    #[test]
    fn load_empty_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweeps.json");
        std::fs::write(&path, "   \n").unwrap();
        let journal = load(&path);
        assert!(journal.entries.is_empty());
    }

    #[test]
    fn load_corrupt_file_is_empty_not_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweeps.json");
        std::fs::write(&path, "{ not json").unwrap();
        let journal = load(&path);
        assert!(journal.entries.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweeps.json");
        let mut journal = SweepJournal::default();
        journal.entries.push(entry("/repo/a", 42, 1234));

        save(&path, &journal).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].repo, "/repo/a");
        assert_eq!(loaded.entries[0].issue, 42);
        assert_eq!(loaded.entries[0].pid, 1234);
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("sweeps.json");
        save(&path, &SweepJournal::default()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn find_returns_matching_entry_only() {
        let mut journal = SweepJournal::default();
        journal.entries.push(entry("/repo/a", 1, 100));
        journal.entries.push(entry("/repo/b", 1, 200));
        journal.entries.push(entry("/repo/a", 2, 300));

        let found = find(&journal, "/repo/a", 1).unwrap();
        assert_eq!(found.pid, 100);
        assert!(find(&journal, "/repo/a", 99).is_none());
        assert!(find(&journal, "/repo/z", 1).is_none());
    }

    #[test]
    fn prune_dead_removes_only_dead_pids() {
        let mut journal = SweepJournal::default();
        journal.entries.push(entry("/repo/a", 1, 111)); // alive
        journal.entries.push(entry("/repo/a", 2, 222)); // dead
        journal.entries.push(entry("/repo/a", 3, 333)); // alive

        let removed = prune_dead(&mut journal, |pid| pid != 222);
        assert_eq!(removed, 1);
        assert_eq!(journal.entries.len(), 2);
        assert!(find(&journal, "/repo/a", 2).is_none());
        assert!(find(&journal, "/repo/a", 1).is_some());
        assert!(find(&journal, "/repo/a", 3).is_some());
    }

    #[test]
    fn upsert_replaces_existing_entry_for_same_key() {
        let mut journal = SweepJournal::default();
        journal.entries.push(entry("/repo/a", 1, 111));

        upsert(&mut journal, entry("/repo/a", 1, 999), |_| true);

        assert_eq!(journal.entries.len(), 1);
        assert_eq!(find(&journal, "/repo/a", 1).unwrap().pid, 999);
    }

    #[test]
    fn upsert_prunes_dead_entries_before_inserting() {
        let mut journal = SweepJournal::default();
        journal.entries.push(entry("/repo/a", 1, 111)); // dead
        journal.entries.push(entry("/repo/a", 2, 222)); // alive

        upsert(&mut journal, entry("/repo/b", 5, 500), |pid| pid == 222);

        assert_eq!(journal.entries.len(), 2);
        assert!(find(&journal, "/repo/a", 1).is_none());
        assert!(find(&journal, "/repo/a", 2).is_some());
        assert!(find(&journal, "/repo/b", 5).is_some());
    }

    #[test]
    fn remove_deletes_matching_entry_and_reports_change() {
        let mut journal = SweepJournal::default();
        journal.entries.push(entry("/repo/a", 1, 111));
        journal.entries.push(entry("/repo/a", 2, 222));

        assert!(remove(&mut journal, "/repo/a", 1));
        assert_eq!(journal.entries.len(), 1);
        assert!(find(&journal, "/repo/a", 1).is_none());

        // Idempotent: removing again reports no change.
        assert!(!remove(&mut journal, "/repo/a", 1));
    }

    #[test]
    #[serial]
    fn default_journal_path_honors_env_override() {
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom-sweeps.json");
        std::env::set_var(JOURNAL_PATH_ENV, &custom);
        assert_eq!(default_journal_path().unwrap(), custom);
        std::env::remove_var(JOURNAL_PATH_ENV);
    }

    #[test]
    #[serial]
    fn record_sweep_then_remove_sweep_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweeps.json");
        std::env::set_var(JOURNAL_PATH_ENV, &path);

        record_sweep("/repo/a", 42, std::process::id(), Utc::now()).unwrap();
        let journal = load(&path);
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].issue, 42);

        remove_sweep("/repo/a", 42).unwrap();
        let journal = load(&path);
        assert!(journal.entries.is_empty());

        std::env::remove_var(JOURNAL_PATH_ENV);
    }

    #[test]
    #[serial]
    fn record_sweep_prunes_dead_peers_via_default_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweeps.json");
        std::env::set_var(JOURNAL_PATH_ENV, &path);

        // Seed a dead entry directly (PID 0 is always dead per is_pid_alive).
        let mut seeded = SweepJournal::default();
        seeded.entries.push(entry("/repo/a", 1, 0));
        save(&path, &seeded).unwrap();

        record_sweep("/repo/a", 2, std::process::id(), Utc::now()).unwrap();

        let journal = load(&path);
        assert_eq!(journal.entries.len(), 1, "dead PID-0 entry should be pruned");
        assert!(find(&journal, "/repo/a", 2).is_some());

        std::env::remove_var(JOURNAL_PATH_ENV);
    }
}
