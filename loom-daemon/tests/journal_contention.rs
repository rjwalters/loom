//! Cross-process writer serialisation for the shared journal core (#11345).
//!
//! The parent test re-executes this test binary as child processes (one
//! `--exact` child test each, selected by an env var naming the journal
//! root). Without the env var the child tests are no-ops.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use loom_daemon::journal::{
    verify, Journal, JournalError, JournalOptions, Position, VerifyOptions,
};
use serde_json::json;
use tempfile::TempDir;

const APPEND_ROOT_ENV: &str = "LOOM_JOURNAL_TEST_APPEND_ROOT";
const HOLD_ROOT_ENV: &str = "LOOM_JOURNAL_TEST_HOLD_ROOT";
const CHILDREN: usize = 4;
const PER_CHILD: u64 = 40;
const STREAM: &str = "contended";

fn spawn_child(test: &str, env: &str, root: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(env, root)
        .spawn()
        .unwrap()
}

/// Child: append `PER_CHILD` records with a generous lock budget.
#[test]
fn child_appender() {
    let Some(root) = std::env::var_os(APPEND_ROOT_ENV) else {
        return;
    };
    let options = JournalOptions {
        lock_retry: Duration::from_secs(60),
        max_segment_bytes: 4 * 1024,
        ..JournalOptions::default()
    };
    let stream = Journal::with_options(root, options).stream(STREAM).unwrap();
    let pid = std::process::id();
    for i in 0..PER_CHILD {
        stream
            .append("contention.tick", Some(&format!("{pid}-{i}")), json!({"i": i}))
            .unwrap();
    }
}

/// Child: hold the lock until the parent drops a `release` file.
#[test]
fn child_lock_holder() {
    let Some(root) = std::env::var_os(HOLD_ROOT_ENV) else {
        return;
    };
    let root = Path::new(&root);
    let stream = Journal::open(root).stream(STREAM).unwrap();
    let _guard = stream.lock().unwrap();
    std::fs::write(root.join("held"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("release").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn concurrent_processes_append_unique_contiguous_seqs() {
    let tmp = TempDir::new().unwrap();
    let children: Vec<Child> = (0..CHILDREN)
        .map(|_| spawn_child("child_appender", APPEND_ROOT_ENV, tmp.path()))
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success(), "child appender failed");
    }
    let stream = Journal::open(tmp.path()).stream(STREAM).unwrap();
    let mut reader = stream.reader(Position::START);
    let seqs: Vec<u64> = reader.by_ref().map(|r| r.unwrap().envelope.seq).collect();
    let total = CHILDREN as u64 * PER_CHILD;
    assert_eq!(seqs, (1..=total).collect::<Vec<_>>(), "unique, contiguous seq");
    let stats = reader.stats();
    assert_eq!(stats.corrupt_lines, 0);
    assert!(!stats.torn_tail, "no torn line");
    let report = verify(tmp.path(), &VerifyOptions::default());
    assert!(report.ok, "{report:?}");
    assert!(report.streams[0].segments > 1, "segments rolled under contention");
}

#[test]
fn a_lock_held_by_another_process_is_busy_within_budget() {
    let tmp = TempDir::new().unwrap();
    let options = JournalOptions {
        lock_retry: Duration::from_millis(200),
        ..JournalOptions::default()
    };
    let stream = Journal::with_options(tmp.path(), options)
        .stream(STREAM)
        .unwrap();
    stream.append("before", None, json!(1)).unwrap();
    let segment = tmp.path().join(STREAM).join("0000000001.jsonl");
    let before = std::fs::read(&segment).unwrap();

    let mut holder = spawn_child("child_lock_holder", HOLD_ROOT_ENV, tmp.path());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !tmp.path().join("held").exists() {
        assert!(Instant::now() < deadline, "holder never took the lock");
        std::thread::sleep(Duration::from_millis(10));
    }

    let started = Instant::now();
    let result = stream.append("blocked", None, json!(2));
    let waited = started.elapsed();
    std::fs::write(tmp.path().join("release"), b"").unwrap();
    assert!(holder.wait().unwrap().success());

    assert!(matches!(result, Err(JournalError::Busy)), "{result:?}");
    assert!(waited >= Duration::from_millis(200), "retried for the budget");
    assert!(waited < Duration::from_secs(5), "gave up within the budget");
    assert_eq!(std::fs::read(&segment).unwrap(), before, "nothing written");
    assert_eq!(stream.append("after", None, json!(3)).unwrap().seq, 2);
}
