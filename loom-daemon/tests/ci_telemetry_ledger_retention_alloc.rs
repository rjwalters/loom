//! Regression test for Issue #11159: opening the CI-telemetry dedupe ledger
//! costs memory in proportion to the keys it **retains**, not to the history
//! in the file.
//!
//! `Ledger::open` runs every poll cycle. Before #11159 it read the whole
//! `seen.jsonl` into a `Vec<String>` and put every key ever committed into
//! the seen set, so per-cycle memory grew with all history. This test builds
//! a ledger of N expired keys plus M recent ones, asserts what `open`
//! retains, and measures the peak heap during `open` with a counting
//! allocator (not an RSS constant).
//!
//! It is an integration test, its own binary, so its `#[global_allocator]`
//! wraps only this test, not the whole `loom-daemon` lib test suite. The
//! expiry rule and its consumers are covered in
//! `src/ci_telemetry/tests/ledger_retention.rs`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use loom_daemon::ci_telemetry::ledger::{Ledger, UnitKey};

// ---------------------------------------------------------------------------
// A per-thread counting allocator: only the measuring thread's allocations
// are counted, and only while it measures.
// ---------------------------------------------------------------------------

struct Counting;

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    let _ = MEASURING.try_with(|measuring| {
        if measuring.get() {
            LIVE.with(|live| {
                let now = live.get() + delta;
                live.set(now);
                PEAK.with(|peak| peak.set(peak.get().max(now)));
            });
        }
    });
}

fn signed(size: usize) -> isize {
    isize::try_from(size).unwrap_or(isize::MAX)
}

// SAFETY: every call is forwarded unchanged to `System`; the bookkeeping only
// touches const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            note(signed(layout.size()));
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            note(signed(layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        note(-signed(layout.size()));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            note(signed(new_size) - signed(layout.size()));
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Peak bytes this thread had live (above its starting point) while running
/// `f`, and `f`'s result.
fn peak_during<T>(f: impl FnOnce() -> T) -> (usize, T) {
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    let out = f();
    MEASURING.with(|measuring| measuring.set(false));
    (usize::try_from(PEAK.with(Cell::get)).unwrap_or(0), out)
}

// ---------------------------------------------------------------------------
// Ledger builders (the on-disk line shapes, written by hand)
// ---------------------------------------------------------------------------

const REPO: &str = "o/r";

fn watermark() -> DateTime<Utc> {
    "2026-06-01T00:00:00Z".parse().unwrap()
}

fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn write_ledger(dir: &Path, old: u64, recent: u64, with_watermark: bool) -> PathBuf {
    let w = watermark();
    let line = |run_id: u64, job_id: u64, at: DateTime<Utc>| {
        format!(
            r#"{{"type":"seen","seq":1,"repo":"{REPO}","run_id":{run_id},"job_id":{job_id},"attempt":1,"committed_at":"{}"}}"#,
            ts(at)
        )
    };
    let mut text = String::new();
    if with_watermark {
        text.push_str(&format!(
            r#"{{"type":"watermark","repo":"{REPO}","created_at":"{}"}}"#,
            ts(w)
        ));
        text.push('\n');
    }
    for i in 0..old {
        text.push_str(&line(1_000 + i / 8, 500_000 + i, w - Duration::days(90)));
        text.push('\n');
    }
    for i in 0..recent {
        text.push_str(&line(9_000_000 + i / 8, 9_500_000 + i, w - Duration::days(1)));
        text.push('\n');
    }
    let path = dir.join("seen.jsonl");
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn open_retains_only_the_working_set_and_its_peak_allocation_is_o_retained() {
    const OLD: u64 = 40_000;
    const RECENT: u64 = 500;
    let old = usize::try_from(OLD).unwrap();
    let recent = usize::try_from(RECENT).unwrap();

    let long_dir = tempfile::TempDir::new().unwrap();
    let long = write_ledger(long_dir.path(), OLD, RECENT, true);
    let (long_peak, ledger) = peak_during(|| Ledger::open(long).unwrap());
    assert_eq!(ledger.unit_count(), recent);
    assert_eq!(ledger.expired_on_open(), old);
    assert!(!ledger.is_seen(&UnitKey::job(REPO, 1_000, 500_000, 1)));
    assert!(ledger.is_seen(&UnitKey::job(REPO, 9_000_000, 9_500_000, 1)));
    drop(ledger);

    let short_dir = tempfile::TempDir::new().unwrap();
    let short = write_ledger(short_dir.path(), 0, RECENT, true);
    let (short_peak, ledger) = peak_during(|| Ledger::open(short).unwrap());
    assert_eq!(ledger.unit_count(), recent);
    drop(ledger);

    // 40k expired lines (~6 MB of file) must cost no more than a small
    // constant over the same ledger without them: nothing per line outlives
    // the line. Before #11159 the whole file was held as `Vec<String>` and
    // every key entered the set.
    assert!(
        long_peak <= short_peak + 64 * 1024,
        "open peaked at {long_peak} B with {OLD} expired lines vs {short_peak} B without; \
         memory must follow the retained set, not the file"
    );

    // The counter is not blind: the same history with every key retained (no
    // watermark, so nothing can expire) costs memory per key.
    let all_dir = tempfile::TempDir::new().unwrap();
    let all = write_ledger(all_dir.path(), OLD, RECENT, false);
    let (all_peak, ledger) = peak_during(|| Ledger::open_read_only(all).unwrap());
    assert_eq!(ledger.unit_count(), old + recent);
    assert!(
        all_peak > long_peak + old * 16,
        "retaining {OLD} more keys should cost far more than {long_peak} B (got {all_peak} B)"
    );
}
