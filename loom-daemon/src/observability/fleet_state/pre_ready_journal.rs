//! On-disk journal of `outcomes::Memory.pre_ready_done` (#11954).
//!
//! The set is process-local, so a daemon restart forgot every label-derived
//! `eta.stage_outcome` already emitted and the first pass re-emitted them.
//! The journal (one JSON object per line, whole-file rewrite) is reloaded into
//! the memory before the first pass. `loom.fact_id` is untouched: this only
//! gates whether an already-emitted exit is emitted again. All I/O is
//! best-effort; a bad file is a warning and an empty or partial set.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::outcomes::PRE_READY_MEMORY_DAYS;
use crate::telemetry::kinds::fleet_state::FleetStage;

/// File name under `<workspace_root>/.loom/logs/`, beside the OTLP queues.
const FILE_NAME: &str = "fleet-outcomes-pre-ready-done.jsonl";

/// Hard cap on journal entries (newest `left_at` kept), beyond the 14-day prune.
pub(super) const MAX_ENTRIES: usize = 20_000;

pub(super) type Done = BTreeSet<(String, u32, FleetStage, DateTime<Utc>)>;

#[derive(Serialize, Deserialize)]
struct Entry {
    repo: String,
    issue: u32,
    stage: FleetStage,
    left_at: DateTime<Utc>,
}

pub(super) fn path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".loom").join("logs").join(FILE_NAME)
}

/// Load the journal, dropping entries older than the memory horizon and
/// skipping unparseable lines. Never fails.
pub(super) fn load(path: &Path, now: DateTime<Utc>) -> Done {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Done::new(),
        Err(e) => {
            log::warn!("fleet.state: pre-ready journal unreadable ({}): {e}", path.display());
            return Done::new();
        }
    };
    let horizon = now - Duration::days(PRE_READY_MEMORY_DAYS);
    let mut bad = 0usize;
    let mut done = Done::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Entry>(line) {
            Ok(e) if e.left_at >= horizon => {
                done.insert((e.repo, e.issue, e.stage, e.left_at));
            }
            Ok(_) => {}
            Err(_) => bad += 1,
        }
    }
    if bad > 0 {
        log::warn!("fleet.state: pre-ready journal had {bad} corrupt line(s): {}", path.display());
    }
    cap(done)
}

/// Keep at most [`MAX_ENTRIES`], dropping the oldest `left_at` first.
fn cap(done: Done) -> Done {
    if done.len() <= MAX_ENTRIES {
        return done;
    }
    let mut all: Vec<_> = done.into_iter().collect();
    all.sort_by_key(|(_, _, _, left)| *left);
    all.split_off(all.len() - MAX_ENTRIES).into_iter().collect()
}

/// Rewrite the journal atomically (temp file + rename). Never fails.
pub(super) fn save(path: &Path, done: &Done) {
    let done = cap(done.clone());
    let mut body = String::new();
    for (repo, issue, stage, left_at) in done {
        let entry = Entry {
            repo,
            issue,
            stage,
            left_at,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        log::warn!("fleet.state: pre-ready journal write failed ({}): {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(issue: u32, left: DateTime<Utc>) -> (String, u32, FleetStage, DateTime<Utc>) {
        ("o/r".to_owned(), issue, FleetStage::ReviewWait, left)
    }

    #[test]
    fn restart_reload_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        let now = Utc::now();
        let done: Done = [key(1, now), key(2, now - Duration::days(1))].into();
        save(&p, &done);
        assert_eq!(load(&p, now), done);
    }

    #[test]
    fn load_prunes_past_horizon() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        let now = Utc::now();
        let old = now - Duration::days(PRE_READY_MEMORY_DAYS + 1);
        save(&p, &[key(1, old), key(2, now)].into());
        assert_eq!(load(&p, now), [key(2, now)].into());
    }

    #[test]
    fn size_is_bounded_keeping_newest() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        let now = Utc::now();
        let total = MAX_ENTRIES as u32 + 50;
        let done: Done = (0..total)
            .map(|i| key(i, now - Duration::seconds(i64::from(total - i))))
            .collect();
        save(&p, &done);
        let loaded = load(&p, now);
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert!(!loaded.iter().any(|(_, i, _, _)| *i < 50));
    }

    #[test]
    fn corrupt_and_missing_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        let now = Utc::now();
        assert!(load(&p, now).is_empty());
        save(&p, &[key(7, now)].into());
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("{not json\n\n");
        std::fs::write(&p, text).unwrap();
        assert_eq!(load(&p, now), [key(7, now)].into());
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load(&p, now).is_empty());
    }
}
