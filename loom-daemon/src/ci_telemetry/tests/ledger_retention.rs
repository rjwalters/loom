//! The dedupe ledger's bounded working set (Issue #11159).
//!
//! `Ledger::open` runs every poll cycle, so it must stream the file and keep
//! only the keys a consumer can still ask about — not every unit since the
//! ledger was created. These tests pin the expiry rule (relative to the
//! repo's watermark, never to unstamped or watermark-less keys), the
//! consumers it must not break (pending units, wanted job logs, a re-polled
//! fixture after compaction), the older-daemon compatibility of the new
//! `committed_at` field. The allocation regression (peak memory during `open`
//! is O(retained), measured with a counting allocator) lives in its own test
//! binary, `tests/ci_telemetry_ledger_retention_alloc.rs`, so its
//! `#[global_allocator]` does not wrap the whole lib test suite.

use std::path::PathBuf;

use super::*;
use crate::ci_telemetry::ledger::SEEN_RETENTION_DAYS;

// ---------------------------------------------------------------------------
// Ledger builders
// ---------------------------------------------------------------------------

const REPO: &str = "o/r";

fn watermark() -> DateTime<Utc> {
    "2026-06-01T00:00:00Z".parse().unwrap()
}

fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn watermark_line(repo: &str, at: DateTime<Utc>) -> String {
    format!(r#"{{"type":"watermark","repo":"{repo}","created_at":"{}"}}"#, ts(at))
}

/// A compacted `seen` job key; `committed_at: None` is a pre-#11159 line.
fn seen_line(repo: &str, run_id: u64, job_id: u64, at: Option<DateTime<Utc>>) -> String {
    let stamp = at.map_or(String::new(), |at| format!(r#","committed_at":"{}""#, ts(at)));
    format!(
        r#"{{"type":"seen","seq":1,"repo":"{repo}","run_id":{run_id},"job_id":{job_id},"attempt":1{stamp}}}"#
    )
}

fn write_ledger(dir: &Path, lines: &[String]) -> PathBuf {
    let path = dir.join("seen.jsonl");
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    path
}

// ---------------------------------------------------------------------------
// The expiry rule
// ---------------------------------------------------------------------------

#[test]
fn expiry_is_relative_to_the_watermark_and_spares_unstamped_and_watermarkless_keys() {
    let w = watermark();
    let inside = w - Duration::days(SEEN_RETENTION_DAYS - 1);
    let outside = w - Duration::days(SEEN_RETENTION_DAYS + 1);
    let ancient = w - Duration::days(400);
    let dir = TempDir::new().unwrap();
    let path = write_ledger(
        dir.path(),
        &[
            // A key read BEFORE its repo's first watermark line is kept: the
            // watermark read so far is the only one used, and none was.
            seen_line(REPO, 1, 10, Some(ancient)),
            watermark_line(REPO, w),
            seen_line(REPO, 2, 20, Some(inside)),
            seen_line(REPO, 3, 30, Some(outside)),
            seen_line(REPO, 4, 40, None),
            // No watermark for this repo at all (feed-only so far): kept.
            seen_line("o/feed-only", 5, 50, Some(ancient)),
        ],
    );
    for ledger in [
        Ledger::open(path.clone()).unwrap(),
        Ledger::open_read_only(path.clone()).unwrap(),
    ] {
        assert!(ledger.is_seen(&UnitKey::job(REPO, 1, 10, 1)), "read before the watermark");
        assert!(ledger.is_seen(&UnitKey::job(REPO, 2, 20, 1)), "inside retention");
        assert!(!ledger.is_seen(&UnitKey::job(REPO, 3, 30, 1)), "past retention");
        assert!(ledger.is_seen(&UnitKey::job(REPO, 4, 40, 1)), "unstamped (pre-#11159)");
        assert!(ledger.is_seen(&UnitKey::job("o/feed-only", 5, 50, 1)), "no watermark");
        assert_eq!((ledger.unit_count(), ledger.expired_on_open()), (4, 1));
    }
}

#[test]
fn a_pending_unit_is_kept_and_replayed_however_old() {
    let w = watermark();
    let old = ts(w - Duration::days(SEEN_RETENTION_DAYS * 3));
    let dir = TempDir::new().unwrap();
    let path = write_ledger(
        dir.path(),
        &[
            watermark_line(REPO, w),
            format!(
                r#"{{"type":"unit","seq":7,"repo":"{REPO}","run_id":1,"job_id":11,"committed_at":"{old}","envelopes":[]}}"#
            ),
            r#"{"type":"emitted","through_seq":6}"#.to_string(),
        ],
    );
    let ledger = Ledger::open(path).unwrap();
    assert_eq!(ledger.pending().len(), 1, "an unconfirmed unit must replay");
    assert!(ledger.is_seen(&UnitKey::job(REPO, 1, 11, 1)), "and must stay seen");
}

#[test]
fn an_expired_log_marker_retires_its_wanted_line_and_a_pending_one_survives() {
    let w = watermark();
    let target = |job_id: u64| {
        format!(
            r#"{{"type":"log_wanted","target":{{"repo":"{REPO}","run_id":1,"job_id":{job_id},"attempt":1,"workflow":"ci","job":"build","completed_at":"{}"}}}}"#,
            ts(w - Duration::days(100))
        )
    };
    let old = ts(w - Duration::days(SEEN_RETENTION_DAYS + 10));
    let dir = TempDir::new().unwrap();
    let path = write_ledger(
        dir.path(),
        &[
            watermark_line(REPO, w),
            // Job 11: wanted, then captured long ago — the capture's marker
            // expires, and the wanted line must not turn back into "pending".
            target(11),
            format!(
                r#"{{"type":"unit","seq":1,"repo":"{REPO}","run_id":1,"job_id":11,"logs":true,"committed_at":"{old}","envelopes":[]}}"#
            ),
            // Job 12: wanted and still uncaptured, however old — it stays.
            target(12),
            r#"{"type":"emitted","through_seq":1}"#.to_string(),
        ],
    );
    let ledger = Ledger::open_read_only(path).unwrap();
    let pending: Vec<u64> = ledger.pending_logs().iter().map(|t| t.job_id).collect();
    assert_eq!(pending, vec![12], "an expired done-marker must not re-capture its log");
    assert_eq!(
        ledger.log_counts(),
        ledger::LogCounts {
            done: 0,
            pending: 1,
            failed: 0
        }
    );
}

// ---------------------------------------------------------------------------
// Compaction and the on-disk format
// ---------------------------------------------------------------------------

/// The `seen`/`unit`/`watermark`/`emitted` lines exactly as a pre-#11159
/// daemon declared them (no `committed_at`). Serde ignores unknown fields
/// unless told otherwise, so an older daemon reads the new lines unchanged.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum PreRetentionLine {
    Unit {
        seq: u64,
        repo: String,
        run_id: u64,
        #[serde(default)]
        job_id: Option<u64>,
        attempt: u32,
        #[serde(default)]
        logs: bool,
        envelopes: Vec<serde_json::Value>,
    },
    Seen {
        seq: u64,
        repo: String,
        run_id: u64,
        #[serde(default)]
        job_id: Option<u64>,
        attempt: u32,
        #[serde(default)]
        logs: bool,
    },
    Watermark {
        repo: String,
        created_at: DateTime<Utc>,
    },
    Emitted {
        through_seq: u64,
    },
    LogWanted {
        target: serde_json::Value,
    },
    LogFailure {
        repo: String,
        job_id: u64,
        attempts: u32,
        error: String,
    },
}

#[test]
fn compaction_stamps_legacy_keys_drops_expired_ones_and_stays_readable_by_an_older_daemon() {
    let w = watermark();
    let dir = TempDir::new().unwrap();
    let mut lines = vec![watermark_line(REPO, w)];
    lines.push(seen_line(REPO, 1, 10, None)); // legacy
    lines.push(seen_line(REPO, 2, 20, Some(w - Duration::days(SEEN_RETENTION_DAYS + 1))));
    lines.push(seen_line(REPO, 3, 30, Some(w - Duration::days(2))));
    let path = write_ledger(dir.path(), &lines);
    // A freshly committed unit is stamped too.
    let mut ledger = Ledger::open(path.clone()).unwrap();
    ledger
        .commit(vec![UnitDraft {
            key: UnitKey::job(REPO, 4, 40, 1),
            envelopes: Vec::new(),
        }])
        .unwrap();
    ledger.mark_emitted(ledger.pending()[0].seq).unwrap();
    let before = Utc::now() - Duration::seconds(5);
    assert!(ledger.compact_if_large(0).unwrap(), "compaction must have run");

    let text = std::fs::read_to_string(&path).unwrap();
    let compacted: Vec<&str> = text.lines().collect();
    assert!(
        compacted[0].contains(r#""type":"watermark""#),
        "watermarks lead, so the next open expires from the first key: {text}"
    );
    let seen: Vec<serde_json::Value> = compacted
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|line| line["type"] == "seen")
        .collect();
    let jobs: Vec<u64> = seen
        .iter()
        .map(|line| line["job_id"].as_u64().unwrap())
        .collect();
    assert_eq!(jobs, vec![10, 30, 40], "the expired key is gone for good");
    for line in &seen {
        let stamp: DateTime<Utc> = line["committed_at"].as_str().unwrap().parse().unwrap();
        if line["job_id"] == 10 || line["job_id"] == 40 {
            assert!(stamp >= before, "legacy/new keys stamp at compaction/commit time");
        }
    }
    for line in &compacted {
        serde_json::from_str::<PreRetentionLine>(line)
            .unwrap_or_else(|e| panic!("an older daemon could not read {line}: {e}"));
    }

    // Reopened, all three survive; once the watermark has moved far past the
    // compaction stamp, the formerly-legacy key ages out like any other.
    assert_eq!(Ledger::open(path.clone()).unwrap().unit_count(), 3);
    // (The advanced watermark replaces the first line, where compaction
    // writes it.)
    let later = Utc::now() + Duration::days(SEEN_RETENTION_DAYS + 1);
    let mut advanced = watermark_line(REPO, later);
    for line in &compacted[1..] {
        advanced.push('\n');
        advanced.push_str(line);
    }
    advanced.push('\n');
    std::fs::write(&path, advanced).unwrap();
    let aged = Ledger::open_read_only(path).unwrap();
    assert_eq!(aged.unit_count(), 0);
    assert!(!aged.is_seen(&UnitKey::job(REPO, 1, 10, 1)));
}

#[test]
fn a_compacted_ledger_still_dedupes_the_whole_fixture_and_every_log() {
    // End to end: poll (records + logs), compact, poll again — the stamped,
    // compacted ledger must still recognise every run, job and captured log.
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let first = run_cycle(&ctx_with_logs(dir.path()), &api).unwrap();
    assert!(first.summary.logs_captured > 0);
    let path = state_dir(dir.path()).join("seen.jsonl");
    let mut ledger = Ledger::open(path.clone()).unwrap();
    let units = ledger.unit_count();
    assert!(ledger.compact_if_large(0).unwrap());
    drop(ledger);
    let before = journal(dir.path()).len();
    let again = run_cycle(&ctx_with_logs(dir.path()), &api).unwrap();
    assert_eq!(
        (
            again.summary.runs_emitted,
            again.summary.jobs_emitted,
            again.summary.logs_captured
        ),
        (0, 0, 0)
    );
    assert_eq!(journal(dir.path()).len(), before);
    assert_eq!(Ledger::open_read_only(path).unwrap().unit_count(), units);
}
