//! Merge-queue telemetry with restart-safe deduplication (#10256, Phase B2).
//!
//! Three record kinds, one JSON line each in
//! `.loom/logs/merge-queue-events.jsonl` (a local append-only log, the same
//! shape of surface as `merge-admission-telemetry.sh` and the rework log):
//!
//! - `merge_queue.enqueued` — Loom handed the PR to the queue (NOT a merge).
//! - `merge_queue.merged` — GitHub confirmed the merge. Carries
//!   `enqueue_to_merge_secs`. The cross-mode comparable approval-to-merge
//!   latency is `pr-latency`'s PL3 (`loom:pr` labeling → merge), which is
//!   derived from forge history and so already covers direct and queue merges
//!   identically; this record adds the queue-only segment.
//! - `merge_queue.removed` — the PR left the queue without merging: a GitHub
//!   drop (with the classified reason and GitHub's raw text) or a Loom
//!   revocation.
//!
//! # Deduplication
//!
//! Every record has a key `<kind>-<pr>-<nonce>`, where the nonce is the
//! grant generation ([`super::grants`]). Recording creates
//! `.loom/state/merge-queue/seen/<key>` with `create_new` first — atomic
//! across processes and durable across restarts — and appends only when that
//! creation succeeded. A repeated event, a second reconcile pass, or a
//! daemon restart therefore never emits a second record.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Which record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "merge_queue.enqueued")]
    Enqueued,
    #[serde(rename = "merge_queue.merged")]
    Merged,
    #[serde(rename = "merge_queue.removed")]
    Removed,
}

impl EventKind {
    fn token(self) -> &'static str {
        match self {
            EventKind::Enqueued => "enqueued",
            EventKind::Merged => "merged",
            EventKind::Removed => "removed",
        }
    }
}

/// One telemetry record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEvent {
    pub kind: EventKind,
    pub pr: u32,
    /// Grant generation.
    pub nonce: u64,
    pub head: Option<String>,
    /// Removal: a [`super::removal::RemovalKind`] token, or `revoked-<why>`.
    pub reason: Option<String>,
    /// Removal: GitHub's text verbatim (absent for a Loom revocation).
    pub raw_reason: Option<String>,
    /// When the grant was written (≈ enqueue time), RFC 3339.
    pub enqueued_at: Option<String>,
    /// When the event happened, RFC 3339.
    pub at: String,
    /// `at - enqueued_at`, when both parse.
    pub enqueue_to_event_secs: Option<i64>,
    /// Merged although the newest grant record was a revocation — the
    /// pass-to-merge window (see `authz.rs`) observed in the wild.
    #[serde(default)]
    pub merged_after_revocation: bool,
}

impl QueueEvent {
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}-{}-{}", self.kind.token(), self.pr, self.nonce)
    }
}

/// Seconds between two RFC 3339 instants, when both parse.
#[must_use]
pub fn secs_between(from: Option<&str>, to: &str) -> Option<i64> {
    let a = chrono::DateTime::parse_from_rfc3339(from?).ok()?;
    let b = chrono::DateTime::parse_from_rfc3339(to).ok()?;
    Some((b - a).num_seconds())
}

/// Where records go.
pub trait EventSink {
    /// `Ok(true)` when recorded, `Ok(false)` when it was a duplicate.
    ///
    /// # Errors
    ///
    /// The record could not be written.
    fn record(&self, ev: &QueueEvent) -> Result<bool, String>;
    /// PRs with an `enqueued` record and no terminal record for the same
    /// generation — what a reconcile sweep must look at.
    ///
    /// # Errors
    ///
    /// The log could not be read.
    fn pending(&self) -> Result<Vec<u32>, String>;
}

/// Pending PRs in a sequence of records. Pure.
#[must_use]
pub fn pending_in(events: &[QueueEvent]) -> Vec<u32> {
    let mut open: Vec<(u32, u64)> = Vec::new();
    for ev in events {
        let k = (ev.pr, ev.nonce);
        match ev.kind {
            EventKind::Enqueued => {
                if !open.contains(&k) {
                    open.push(k);
                }
            }
            EventKind::Merged | EventKind::Removed => open.retain(|x| *x != k),
        }
    }
    let mut prs: Vec<u32> = open.into_iter().map(|(pr, _)| pr).collect();
    prs.sort_unstable();
    prs.dedup();
    prs
}

/// File-backed sink under a workspace root.
pub struct FileEventSink {
    log: PathBuf,
    seen: PathBuf,
}

impl FileEventSink {
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self {
            log: root.join(".loom/logs/merge-queue-events.jsonl"),
            seen: root.join(".loom/state/merge-queue/seen"),
        }
    }

    fn read_all(&self) -> Result<Vec<QueueEvent>, String> {
        match std::fs::read_to_string(&self.log) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|l| serde_json::from_str::<QueueEvent>(l).ok())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", self.log.display())),
        }
    }
}

impl EventSink for FileEventSink {
    fn record(&self, ev: &QueueEvent) -> Result<bool, String> {
        std::fs::create_dir_all(&self.seen).map_err(|e| format!("{}: {e}", self.seen.display()))?;
        let marker = self.seen.join(ev.key());
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(format!("{}: {e}", marker.display())),
        }
        let line = serde_json::to_string(ev).map_err(|e| e.to_string())?;
        let appended = self
            .log
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&self.log))
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = appended {
            // Release the key so a later pass can record it.
            let _ = std::fs::remove_file(&marker);
            return Err(format!("{}: {e}", self.log.display()));
        }
        Ok(true)
    }

    fn pending(&self) -> Result<Vec<u32>, String> {
        Ok(pending_in(&self.read_all()?))
    }
}

/// In-memory sink (tests).
#[derive(Default)]
pub struct MemoryEventSink {
    pub events: Mutex<Vec<QueueEvent>>,
}

impl MemoryEventSink {
    #[must_use]
    pub fn all(&self) -> Vec<QueueEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl EventSink for MemoryEventSink {
    fn record(&self, ev: &QueueEvent) -> Result<bool, String> {
        let mut v = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if v.iter().any(|e| e.key() == ev.key()) {
            return Ok(false);
        }
        v.push(ev.clone());
        Ok(true)
    }

    fn pending(&self) -> Result<Vec<u32>, String> {
        Ok(pending_in(&self.all()))
    }
}
