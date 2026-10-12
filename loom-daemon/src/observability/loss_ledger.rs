//! Durable, by-reason loss ledger for one exporter's queue (Issue #11527,
//! slice A: "fail loud and never drop silently").
//!
//! [`super::queue::DurableQueue::dropped_total`] used to be an in-memory
//! counter that reset on every restart, and transport-level losses (a 400 the
//! OTLP sink will never accept) never reached it at all. The ledger is the
//! fix: one JSON object per exporter at `<queue path>.loss.json`
//! (`observability-queue.otlp.loss.json` next to
//! `observability-queue.otlp.jsonl`), mapping each loss reason to its
//! cumulative count. It is loaded on [`LossLedger::open`] (soft-fail — a
//! missing file is a fresh start, a corrupt one degrades to empty with a
//! WARN) and rewritten atomically (temp + rename, the same pattern
//! [`super::queue::persist_to`] uses) on every [`LossLedger::record_losses`].
//! Losses are rare, so a full-file write per loss event is fine.
//!
//! `dropped_total` in `host.export` becomes this ledger's sum, so the counter
//! is durable across restarts, and `dropped_by_reason` carries the map. See
//! `defaults/docs/telemetry-schema.md` § `host.export` for the reader
//! contract.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Loss reason: the queue was at capacity and evicted the oldest envelope
/// ([`super::queue::DurableQueue::push`]'s drop-oldest path). Envelope units.
pub const REASON_QUEUE_OVERFLOW: &str = "queue_overflow";

/// Loss reason: a reopen found more queued envelopes than the (shrunk)
/// capacity allows, so the front was trimmed. Envelope units.
pub const REASON_CAPACITY_SHRINK: &str = "capacity_shrink";

/// Loss reason: an OTLP receiver answered 200 with `partialSuccess` and
/// rejected items. **Signal units** (log records / data points / spans), not
/// envelopes — the receiver counts items, and does not identify which
/// envelopes carried them.
pub const REASON_RECEIVER_REJECTED: &str = "receiver_rejected";

/// Loss reason: trace envelopes that could not be mapped to spans (the
/// `count - items` path in `OtlpExporter::emit_batch_outcome`'s traces arm).
/// Envelope units.
pub const REASON_TRACE_UNMAPPABLE: &str = "trace_unmappable";

/// Loss reason: an OTLP HTTP 200 whose content type was not
/// `application/json`, so acceptance is unknown and the request is dropped
/// rather than retried forever. Envelope units.
pub const REASON_NON_JSON_RESPONSE: &str = "non_json_response";

/// Loss reason: an OTLP response exceeded the bounded read of 4096 bytes.
/// Envelope units.
pub const REASON_RESPONSE_TOO_LARGE: &str = "response_too_large";

/// Loss reason: an OTLP HTTP 200 body that could not be parsed as a valid
/// Loss reason: an OTLP HTTP 200 body that could not be parsed as a valid
/// Export response. Envelope units.
pub const REASON_INVALID_RESPONSE: &str = "invalid_response";

/// Loss reason for any non-200 OTLP status that is neither retriable
/// (429/502/503/504) nor a credential refusal held for retry (401/403):
/// `status_400`, `status_404`, … Envelope units.
#[must_use]
pub fn reason_for_status(status: u16) -> String {
    format!("status_{status}")
}

/// The by-reason loss counts for one queue, durable across restarts.
pub struct LossLedger {
    path: PathBuf,
    counts: Mutex<BTreeMap<String, u64>>,
}

impl LossLedger {
    /// Open (or create) the ledger at `path`. A missing file opens empty; a
    /// corrupt one opens empty **with a warning** — the queue's own
    /// soft-fail posture (a damaged diagnostics file must never block the
    /// export path it diagnoses).
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let counts = match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(counts) => counts,
                Err(error) => {
                    log::warn!(
                        "observability: loss ledger {} is unreadable ({error}); \
                         reopening it empty",
                        path.display()
                    );
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        LossLedger {
            path,
            counts: Mutex::new(counts),
        }
    }

    /// Add `count` losses under `reason`, persisting atomically. A write
    /// failure is logged and swallowed (the in-memory count still advances,
    /// so `host.export` stays honest for this process — only the
    /// across-restart durability is lost), mirroring the queue's own
    /// never-block-the-caller persist contract.
    pub fn record(&self, reason: &str, count: u64) {
        let mut losses = BTreeMap::new();
        losses.insert(reason.to_string(), count);
        self.record_losses(&losses);
    }

    /// Add every entry of `losses`, then persist once. A no-op for an empty
    /// map — opening a queue must not create a ledger file.
    pub fn record_losses(&self, losses: &BTreeMap<String, u64>) {
        if losses.is_empty() {
            return;
        }
        let mut guard = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (reason, count) in losses {
            let entry = guard.entry(reason.clone()).or_insert(0);
            *entry = entry.saturating_add(*count);
        }
        if let Err(error) = persist(&self.path, &guard) {
            log::warn!(
                "observability: failed to persist loss ledger to {}: {error}",
                self.path.display()
            );
        }
    }

    /// The durable total over every reason.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.by_reason()
            .values()
            .copied()
            .fold(0u64, u64::saturating_add)
    }

    /// A clone of the by-reason counts, for `host.export`'s
    /// `dropped_by_reason`.
    #[must_use]
    pub fn by_reason(&self) -> BTreeMap<String, u64> {
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Atomically rewrite `path` to contain exactly `counts` as one JSON object —
/// temp-file-then-rename, mirroring [`super::queue::persist_to`].
fn persist(path: &std::path::Path, counts: &BTreeMap<String, u64>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("loss.json.tmp-{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_string(counts).unwrap_or_default())?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn record_then_reopen_preserves_totals_by_reason() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.loss.json");
        {
            let ledger = LossLedger::open(path.clone());
            ledger.record(REASON_QUEUE_OVERFLOW, 2);
            ledger.record_losses(&BTreeMap::from([
                (reason_for_status(400), 3),
                (REASON_QUEUE_OVERFLOW.to_string(), 1),
            ]));
            assert_eq!(ledger.total(), 6);
        }
        // A fresh instance over the same path — simulates a daemon restart.
        let reopened = LossLedger::open(path);
        assert_eq!(
            reopened.by_reason(),
            BTreeMap::from([
                (REASON_QUEUE_OVERFLOW.to_string(), 3),
                (reason_for_status(400), 3),
            ])
        );
        assert_eq!(reopened.total(), 6, "total = the sum over reasons");
    }

    #[test]
    fn missing_file_opens_empty_and_records_nothing_until_a_loss() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.loss.json");
        let ledger = LossLedger::open(path.clone());
        assert_eq!(ledger.total(), 0);
        assert!(ledger.by_reason().is_empty());
        assert!(!path.exists(), "opening a ledger never creates the file");
    }

    #[test]
    fn empty_losses_are_a_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.loss.json");
        let ledger = LossLedger::open(path.clone());
        ledger.record_losses(&BTreeMap::new());
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_file_opens_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.loss.json");
        std::fs::write(&path, "not json at all").unwrap();
        let ledger = LossLedger::open(path.clone());
        assert_eq!(ledger.total(), 0);
        // Recording after the reset rewrites the file cleanly.
        ledger.record(REASON_CAPACITY_SHRINK, 1);
        assert_eq!(LossLedger::open(path).total(), 1);
    }
}
