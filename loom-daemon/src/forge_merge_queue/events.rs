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

/// The event log, relative to a workspace root.
pub const LOG_RELATIVE_PATH: &str = ".loom/logs/merge-queue-events.jsonl";

/// File-backed sink under a workspace root.
pub struct FileEventSink {
    root: PathBuf,
    log: PathBuf,
    seen: PathBuf,
}

impl FileEventSink {
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            log: root.join(LOG_RELATIVE_PATH),
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
        // #11348: recorded and durable in the legacy log — shadow it into the
        // J2 `merge_queue` stream. The `seen` dedupe above stays authoritative.
        crate::telemetry::journal_shadow::shadow_append(
            &self.root,
            crate::telemetry::journal_shadow::TelemetryStream::MergeQueue,
            || {
                let kind = serde_json::to_value(ev.kind).ok()?;
                crate::telemetry::journal_shadow::ShadowRecord::of(
                    kind.as_str()?,
                    Some(ev.key()),
                    ev,
                )
            },
        );
        // #12653: export the appended record to OTLP. Emitting only here, after
        // the `seen` marker and the append, keeps the JSONL dedupe the single
        // authority: a duplicate returned early and never reaches this line.
        emit_otlp(ev);
        Ok(true)
    }

    fn pending(&self) -> Result<Vec<u32>, String> {
        Ok(pending_in(&self.read_all()?))
    }
}

/// The OTLP record for `ev`, or `None` when its timestamp does not parse or
/// the build provenance is not valid.
fn otlp_record(
    ev: &QueueEvent,
    loom: crate::telemetry::provenance::Provenance,
) -> Option<crate::telemetry::kinds::merge_queue_event::MergeQueueEventRecord> {
    if !loom.is_valid() {
        return None;
    }
    let parse = |t: &str| {
        chrono::DateTime::parse_from_rfc3339(t)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    Some(crate::telemetry::kinds::merge_queue_event::MergeQueueEventRecord {
        event: ev.kind.token().to_owned(),
        pr: ev.pr,
        nonce: ev.nonce,
        head: ev.head.clone(),
        reason: ev.reason.clone(),
        enqueued_at: ev.enqueued_at.as_deref().and_then(parse),
        at: parse(&ev.at)?,
        enqueue_to_event_secs: ev.enqueue_to_event_secs,
        merged_after_revocation: ev.merged_after_revocation,
        loom,
    })
}

/// Emit `ev` as a `merge_queue.event` record. A no-op without an ops sink
/// (the short-lived CLI processes have none).
fn emit_otlp(ev: &QueueEvent) {
    if let Some(record) = otlp_record(ev, crate::telemetry::provenance::Provenance::current()) {
        crate::observability::ops::emit_record(crate::telemetry::TelemetryRecord::MergeQueueEvent(
            record,
        ));
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

#[cfg(test)]
mod otlp_tests {
    use super::*;
    use crate::telemetry::provenance::Provenance;

    fn event() -> QueueEvent {
        QueueEvent {
            kind: EventKind::Removed,
            pr: 9,
            nonce: 2,
            head: Some("abc".into()),
            reason: Some("checks-timed-out".into()),
            raw_reason: Some("raw".into()),
            enqueued_at: Some("2026-10-17T11:00:00Z".into()),
            at: "2026-10-17T12:00:00Z".into(),
            enqueue_to_event_secs: Some(3600),
            merged_after_revocation: false,
        }
    }

    fn provenance() -> Provenance {
        Provenance {
            version: "0.19.1".into(),
            revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
            tree_state: "clean".into(),
            complete: true,
        }
    }

    #[test]
    fn the_record_carries_the_classified_reason_and_latency() {
        let r = otlp_record(&event(), provenance()).unwrap();
        assert_eq!(r.event, "removed");
        assert_eq!(r.reason.as_deref(), Some("checks-timed-out"));
        assert_eq!(r.enqueue_to_event_secs, Some(3600));
        assert_eq!(r.at.to_rfc3339(), "2026-10-17T12:00:00+00:00");
    }

    #[test]
    fn an_unparseable_timestamp_or_invalid_provenance_is_not_exported() {
        let mut ev = event();
        ev.at = "not a time".into();
        assert!(otlp_record(&ev, provenance()).is_none());
        let mut bad = provenance();
        bad.revision = String::new();
        assert!(otlp_record(&event(), bad).is_none());
    }

    #[test]
    fn only_a_recorded_event_is_emitted_a_duplicate_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileEventSink::for_root(dir.path());
        let (first, emitted) = crate::observability::ops::capture::capture(|| {
            let a = sink.record(&event()).unwrap();
            let b = sink.record(&event()).unwrap();
            (a, b)
        });
        assert_eq!(first, (true, false));
        let kinds = emitted
            .records
            .iter()
            .filter(|r| matches!(r, crate::telemetry::TelemetryRecord::MergeQueueEvent(_)))
            .count();
        // One recorded event emits at most one record (none when the test
        // build has no valid provenance); the duplicate never adds a second.
        assert!(kinds <= 1, "a duplicate never emits, got {kinds}");
    }
}
