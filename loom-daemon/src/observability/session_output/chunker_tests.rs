//! Chunker tests (2am#5933): size and age bounds, ordering, pointer shape,
//! redaction parity, the firewall exclusion, failure handling and replay.

#![allow(clippy::unwrap_used)]

use std::sync::Mutex;

use chrono::{Duration as ChronoDuration, TimeZone};

use super::*;
use crate::telemetry::kinds::session_output::{
    Coverage, Launch, OutputCategory, RunIdentity, RunState,
};

fn identity(repo: Option<&str>, issue: Option<u32>) -> RunIdentity {
    RunIdentity {
        repo: repo.map(str::to_string),
        visibility: crate::telemetry::RepoVisibility::Private,
        issue,
        session_kind: Some(crate::telemetry::SessionKind::Sweep),
        sweep_id: Some("sweep-1".to_string()),
        session_id: Some("sess-1".to_string()),
        attempt: Some(1),
        runtime: "claude".to_string(),
        role: Some("builder".to_string()),
        launch: Launch::Daemon,
    }
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 23, 12, 0, 0).unwrap()
}

fn after(seconds: i64) -> DateTime<Utc> {
    t0() + ChronoDuration::seconds(seconds)
}

fn output(repo: &str, stream: &str, sequence: u64, text: &str) -> SessionOutputRecord {
    SessionOutputRecord::new_output(
        identity(Some(repo), Some(5933)),
        stream,
        sequence,
        after(sequence as i64 % 4),
        after(sequence as i64 % 4),
        text,
    )
}

#[derive(Default)]
struct FakeStore {
    puts: Mutex<Vec<Chunk>>,
    failing: Mutex<bool>,
}

impl FakeStore {
    fn fail(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }
    fn stored(&self) -> Vec<Chunk> {
        self.puts.lock().unwrap().clone()
    }
}

impl ChunkStore for FakeStore {
    fn put(&self, chunk: &Chunk) -> Result<(), String> {
        if *self.failing.lock().unwrap() {
            return Err("broker unavailable".to_string());
        }
        self.puts.lock().unwrap().push(chunk.clone());
        Ok(())
    }
}

fn decompress(chunk: &Chunk) -> Vec<serde_json::Value> {
    let raw = zstd::stream::decode_all(chunk.bytes.as_slice()).unwrap();
    String::from_utf8(raw)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn chunks_close_at_the_size_bound_in_order_and_round_trip() {
    let mut chunker = Chunker::with_limits("host-a", 4_096, DEFAULT_MAX_AGE, 64);
    let store = FakeStore::default();
    let text = "x".repeat(200);
    let total = 100_u64;
    // Offered out of order within the stream: the chunk sorts on seal.
    for sequence in (0..total).rev() {
        chunker.offer(&output("o/r", "s1", sequence, &format!("{sequence}:{text}")), t0());
    }
    chunker.flush_all(t0());
    let pointers = chunker.pump(&store);
    let chunks = store.stored();
    assert!(chunks.len() > 1, "more than one chunk expected for {total} x ~300 B at 4 KB");
    assert_eq!(pointers.len(), chunks.len());

    let mut seen = Vec::new();
    for (chunk, pointer) in chunks.iter().zip(&pointers) {
        let records = decompress(chunk);
        let sequences: Vec<u64> = records
            .iter()
            .map(|r| r["sequence"].as_u64().unwrap())
            .collect();
        assert!(sequences.windows(2).all(|w| w[0] < w[1]), "ordered within a chunk");
        assert_eq!(pointer.first_sequence, sequences[0]);
        assert_eq!(pointer.last_sequence, *sequences.last().unwrap());
        assert_eq!(pointer.records, records.len() as u64);
        // A chunk closes on the record that reaches the bound, so it overshoots
        // by at most one record — never by a whole extra batch.
        let uncompressed = zstd::stream::decode_all(chunk.bytes.as_slice())
            .unwrap()
            .len();
        assert!(uncompressed < 4_096 + 512, "{uncompressed}");
        seen.extend(sequences);
    }
    seen.sort_unstable();
    assert_eq!(seen, (0..total).collect::<Vec<_>>(), "every record round-trips exactly once");
}

#[test]
fn a_lone_record_is_flushed_after_the_age_bound() {
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    chunker.offer(&output("o/r", "s1", 0, "hello"), t0());
    chunker.flush_due(after(4));
    assert!(chunker.pump(&store).is_empty(), "not yet due at 4 s");
    chunker.flush_due(after(5));
    let pointers = chunker.pump(&store);
    assert_eq!(pointers.len(), 1);
    assert_eq!(store.stored().len(), 1);
}

#[test]
fn each_chunk_has_exactly_one_pointer_with_every_field_and_no_text() {
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    chunker.offer(&output("o/r", "s1", 3, "needle-text-in-record"), t0());
    chunker.offer(&output("o/r", "s1", 4, "more"), t0());
    chunker.flush_all(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 1).unwrap());
    let pointers = chunker.pump(&store);
    assert_eq!(pointers.len(), 1);
    let payload = pointers[0].payload();
    let object = payload.as_object().unwrap();
    for key in [
        "loom.repo",
        "loom.issue",
        "loom.session_id",
        "stream_id",
        "first_sequence",
        "last_sequence",
        "first_source_at",
        "last_source_at",
        "host",
        "month",
        "sha256",
        "size",
    ] {
        assert!(object.contains_key(key), "pointer lacks {key}: {payload}");
    }
    assert_eq!(object["loom.repo"], "o/r");
    assert_eq!(object["loom.issue"], 5933);
    assert_eq!(object["host"], "host-a");
    // `<yyyy-mm>` is the writing time, not the source time.
    assert_eq!(object["month"], "2026-11");
    assert_eq!(object["first_sequence"], 3);
    assert_eq!(object["last_sequence"], 4);
    assert_eq!(object["sha256"], store.stored()[0].sha256);
    assert_eq!(object["size"], store.stored()[0].bytes.len());
    let wire = payload.to_string();
    assert!(!wire.contains("needle-text-in-record"), "pointer carries record text");
    assert!(wire.len() < 16 * 1024);
}

#[test]
fn every_edge_secret_class_is_gone_from_the_decompressed_chunk() {
    let secrets = [
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----\nTRUNCATEDKEYBYTES",
        "ghp_abcdefghijklmnopqrstuvwxyz0123",
        "github_pat_11ABCDEFG0abcdefghijklmnop",
        "AKIAIOSFODNN7EXAMPLE",
        concat!("ASIA", "IOSFODNN7EXAMPLE"),
        "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVP",
        "?sig=SUPERSECRETSIGNATURE&se=2026-10-23",
        "https://bucket.s3.amazonaws.com/k?X-Amz-Signature=0123abcdef&X-Amz-Date=1",
        "/harness-ops/github/token",
        "/2am-logic/prod/db",
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
    ];
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    for (sequence, secret) in secrets.iter().enumerate() {
        // Built as a status-free output record, then re-planted raw so the
        // chunker's own scrub — not the constructor's — is what is under test.
        let mut record = output("o/r", "s1", sequence as u64, "benign");
        record.text = Some(format!("before {secret} after"));
        record.gap_reason = Some(format!("note {secret}"));
        record.tool = Some(format!("tool {secret}"));
        chunker.offer(&record, t0());
    }
    chunker.flush_all(t0());
    chunker.pump(&store);
    let body =
        String::from_utf8(zstd::stream::decode_all(store.stored()[0].bytes.as_slice()).unwrap())
            .unwrap();
    for secret in secrets {
        for fragment in [
            "b3BlbnNzaC1r",
            "TRUNCATEDKEYBYTES",
            "ghp_abcdefghijkl",
            "github_pat_11ABC",
            "AKIAIOSFODNN",
            "ASIAIOSFODNN",
            "wJalrXUtnFEMI",
            "eyJhbGciOiJIUzI1NiJ9",
            "SUPERSECRETSIGNATURE",
            "0123abcdef",
            "/harness-ops/github",
            "/2am-logic/prod",
            "abcdefghijklmnopqrstuvwxyz012345",
        ] {
            if secret.contains(fragment) {
                assert!(!body.contains(fragment), "{fragment} survived in chunk body: {body}");
            }
        }
    }
    assert!(body.contains("[REDACTED:"));
}

#[test]
fn firewall_and_malformed_repos_produce_no_chunk_and_no_pointer() {
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    for repo in [
        "owner/notebook",
        "owner/Notebook",
        "owner/CORPORATE",
        "owner/corporate",
    ] {
        chunker.offer(&output(repo, "s1", 0, "secret idea"), t0());
    }
    for repo in [
        "no-slash",
        "a/b/c",
        "owner/",
        "/repo",
        "ow ner/repo",
        "owner/re$po",
    ] {
        chunker.offer(&output(repo, "s2", 0, "x"), t0());
    }
    chunker.offer(
        &SessionOutputRecord::new_output(identity(None, Some(1)), "s3", 0, t0(), t0(), "unscoped"),
        t0(),
    );
    chunker.flush_all(after(60));
    assert!(chunker.pump(&store).is_empty());
    assert!(store.stored().is_empty());
    assert_eq!(chunker.queued(), 0);
    // A look-alike that is *not* a firewall repo is still written.
    chunker.offer(&output("owner/notebook-tools", "s4", 0, "ok"), t0());
    chunker.flush_all(t0());
    assert_eq!(chunker.pump(&store).len(), 1);
}

#[test]
fn a_failing_store_never_blocks_and_overflow_is_declared_on_the_next_chunk() {
    let mut chunker = Chunker::with_limits("host-a", DEFAULT_MAX_BYTES, DEFAULT_MAX_AGE, 2);
    let store = FakeStore::default();
    store.fail(true);
    for stream in 0..5 {
        chunker.offer(&output("o/r", &format!("s{stream}"), 0, "text"), t0());
        chunker.flush_all(t0());
        assert!(chunker.pump(&store).is_empty());
    }
    assert_eq!(chunker.queued(), 2, "queue is bounded");

    store.fail(false);
    let recovered = chunker.pump(&store);
    assert_eq!(recovered.len(), 2);
    // Oldest dropped first: the survivors are the newest streams.
    let streams: Vec<&str> = recovered.iter().map(|p| p.stream_id.as_str()).collect();
    assert_eq!(streams, ["s3", "s4"]);
    // Three one-record chunks (s0, s1, s2) were dropped. Each loss is
    // declared on the first chunk sealed after it, so the totals add up across
    // the chunks that follow, in the pointer and in the object itself.
    chunker.offer(&output("o/r", "s5", 0, "after the outage"), t0());
    chunker.flush_all(t0());
    let next = chunker.pump(&store);
    assert_eq!(next.len(), 1);
    let reported: u64 = recovered
        .iter()
        .chain(&next)
        .map(|p| p.dropped_events)
        .sum();
    assert_eq!(reported, 3, "every dropped record is declared exactly once");
    let body = decompress(store.stored().last().unwrap());
    assert_eq!(body[0]["chunk_meta"]["dropped_events"], next[0].dropped_events);
    assert!(next[0].dropped_events > 0);
    chunker.flush_all(t0());
    // And it is reported once.
    chunker.offer(&output("o/r", "s6", 0, "later"), t0());
    chunker.flush_all(t0());
    assert_eq!(chunker.pump(&store)[0].dropped_events, 0);
}

#[test]
fn gap_and_coverage_records_pass_through_verbatim() {
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    let gap = SessionOutputRecord::status(
        identity(Some("o/r"), Some(5933)),
        OutputCategory::Gap,
        "status-1",
        0,
        t0(),
        Coverage::Degraded,
        RunState::Running,
    )
    .with_gap("export_queue_overflow", 7);
    let coverage = SessionOutputRecord::status(
        identity(Some("o/r"), Some(5933)),
        OutputCategory::Coverage,
        "status-1",
        1,
        t0(),
        Coverage::Ended,
        RunState::Ended,
    );
    chunker.offer(&gap, t0());
    chunker.offer(&coverage, t0());
    chunker.flush_all(t0());
    chunker.pump(&store);
    let records = decompress(&store.stored()[0]);
    assert_eq!(records[0], serde_json::to_value(&gap).unwrap());
    assert_eq!(records[1], serde_json::to_value(&coverage).unwrap());
}

#[test]
fn replaying_the_same_records_yields_the_same_key() {
    let run = || {
        let mut chunker = Chunker::new("host-a");
        let store = FakeStore::default();
        for sequence in 0..10 {
            chunker.offer(&output("o/r", "s1", sequence, "same bytes"), t0());
        }
        chunker.flush_all(t0());
        chunker.pump(&store);
        store.stored()[0].sha256.clone()
    };
    assert_eq!(run(), run());
}

#[test]
fn a_record_without_an_issue_is_written_but_unscoped_in_its_pointer() {
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    let record = SessionOutputRecord::new_output(
        identity(Some("o/r"), None),
        "s1",
        0,
        t0(),
        t0(),
        "role run",
    );
    chunker.offer(&record, t0());
    chunker.flush_all(t0());
    let pointers = chunker.pump(&store);
    assert_eq!(store.stored().len(), 1);
    assert_eq!(pointers[0].payload()["loom.issue"], serde_json::Value::Null);
}

#[test]
fn a_stream_whose_owner_changes_mid_run_starts_a_new_chunk() {
    let mut chunker = Chunker::new("host-a");
    let store = FakeStore::default();
    chunker.offer(&output("o/r", "s1", 0, "a"), t0());
    chunker.offer(&output("o/other", "s1", 1, "b"), t0());
    chunker.flush_all(t0());
    let pointers = chunker.pump(&store);
    assert_eq!(pointers.len(), 2);
    assert_ne!(pointers[0].repo, pointers[1].repo);
}

#[test]
fn commit_stored_removes_only_the_stored_ids_even_if_the_queue_shifted() {
    let mut chunker = Chunker::with_limits("host-a", 1, DEFAULT_MAX_AGE, 2);
    chunker.offer(&output("o/r", "s1", 0, "a"), t0());
    let snapshot = chunker.queued_snapshot();
    assert_eq!(snapshot.len(), 1);
    let stored_id = snapshot[0].0;
    // While the lock would be released for the put, two more chunks seal and
    // overflow evicts the snapshotted one (cap 2).
    chunker.offer(&output("o/r", "s1", 1, "b"), t0());
    chunker.offer(&output("o/r", "s1", 2, "c"), t0());
    assert_eq!(chunker.queued(), 2);
    assert!(chunker.commit_stored(&[stored_id]).is_empty());
    assert_eq!(chunker.queued(), 2, "newer chunks must survive the commit");
}

#[test]
fn a_failed_put_keeps_it_and_everything_behind_it_queued_in_order() {
    let mut chunker = Chunker::with_limits("host-a", 1, DEFAULT_MAX_AGE, 10);
    let store = FakeStore::default();
    chunker.offer(&output("o/r", "s1", 0, "a"), t0());
    chunker.offer(&output("o/r", "s1", 1, "b"), t0());
    assert_eq!(chunker.pump(&store).len(), 2);
    store.fail(true);
    chunker.offer(&output("o/r", "s1", 2, "c"), t0());
    chunker.offer(&output("o/r", "s1", 3, "d"), t0());
    assert!(chunker.pump(&store).is_empty());
    assert_eq!(chunker.queued(), 2);
    store.fail(false);
    let pointers = chunker.pump(&store);
    assert_eq!(pointers[0].first_sequence, 2);
    assert_eq!(pointers[1].first_sequence, 3);
}
