//! `Ledger::compact_if_large` idempotence (Issue #11160): a compacted ledger
//! that is still above the threshold must not be rewritten every cycle.

use super::*;
use std::os::unix::fs::MetadataExt;

fn drafts(range: std::ops::Range<u64>) -> Vec<UnitDraft> {
    range
        .map(|job_id| UnitDraft {
            key: UnitKey::job("fixture-org/alpha", 1, job_id, 1),
            envelopes: Vec::new(),
        })
        .collect()
}

fn fingerprint(path: &std::path::Path) -> (u64, std::time::SystemTime, Vec<u8>) {
    let meta = std::fs::metadata(path).unwrap();
    (meta.ino(), meta.modified().unwrap(), std::fs::read(path).unwrap())
}

#[test]
fn a_compacted_ledger_above_the_threshold_is_not_rewritten_again() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seen.jsonl");
    let mut ledger = Ledger::open(path.clone()).unwrap();
    let committed = ledger.commit(drafts(0..50)).unwrap();
    ledger.mark_emitted(committed.last().unwrap().seq).unwrap();

    // Threshold 1 byte: the compacted file is necessarily still above it.
    assert!(ledger.compact_if_large(1).unwrap(), "first compaction runs");
    let after_first = fingerprint(&path);
    assert!(after_first.2.len() > 1);

    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!ledger.compact_if_large(1).unwrap(), "nothing new: must be a no-op");
    assert_eq!(fingerprint(&path), after_first, "file was replaced or rewritten");

    // A fresh process loading the already-compact file does not rewrite it.
    let mut reopened = Ledger::open(path.clone()).unwrap();
    assert!(!reopened.compact_if_large(1).unwrap());
    assert_eq!(fingerprint(&path), after_first);
}

#[test]
fn committing_new_units_makes_the_next_compaction_run() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seen.jsonl");
    let mut ledger = Ledger::open(path.clone()).unwrap();
    let committed = ledger.commit(drafts(0..50)).unwrap();
    ledger.mark_emitted(committed.last().unwrap().seq).unwrap();
    assert!(ledger.compact_if_large(1).unwrap());
    assert!(!ledger.compact_if_large(1).unwrap());

    let committed = ledger.commit(drafts(50..60)).unwrap();
    // Still pending: compaction must wait for the emit.
    assert!(!ledger.compact_if_large(1).unwrap());
    ledger.mark_emitted(committed.last().unwrap().seq).unwrap();
    assert!(ledger.compact_if_large(1).unwrap(), "new units must trigger a rewrite");
    assert!(!ledger.compact_if_large(1).unwrap(), "and then settle again");
    assert_eq!(Ledger::open_read_only(path).unwrap().unit_count(), 60);
}
