//! J1 shadow mechanics: settings, default-off, verdicts, divergence records,
//! failure isolation, restart, corruption, schema skew and cross-process
//! lock contention. Producer wiring is in `tests_producers.rs`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tempfile::tempdir;

use super::*;
use crate::journal::{Journal, JournalOptions, Position};

pub(super) fn settings(root: &Path) -> Settings {
    Settings {
        root: root.to_path_buf(),
        verify: true,
        options: JournalOptions {
            lock_retry: Duration::from_millis(50),
            ..JournalOptions::default()
        },
    }
}

/// Enable the shadow journal for this thread only.
pub(super) fn enable(root: &Path) {
    set_test_settings(Some(Some(settings(root))));
}

pub(super) fn disable() {
    set_test_settings(Some(None));
}

fn record(item: &str, change: Change) -> ShadowRecord {
    ShadowRecord {
        kind: Kind::ClaimAcquired,
        store: Store::Sweeps,
        path: PathBuf::from("/machine/sweeps.json"),
        item: item.to_owned(),
        change,
        subject: Subject {
            workspace: "/ws".into(),
            issue: Some(7),
            ..Subject::default()
        },
        fields: Map::new(),
        key: None,
    }
}

pub(super) fn records(root: &Path) -> Vec<crate::journal::Envelope> {
    Journal::open(root)
        .stream(STREAM)
        .unwrap()
        .reader(Position::START)
        .map(|r| r.unwrap().envelope)
        .collect()
}

fn appended(outcome: ShadowOutcome) -> (u64, Verdict) {
    match outcome {
        ShadowOutcome::Appended { seq, verdict } => (seq, verdict),
        other => panic!("expected an append, got {other:?}"),
    }
}

#[test]
fn settings_default_off_and_resolution() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    let home = tempdir().unwrap();
    assert_eq!(Settings::resolve(env(&[]), Some(home.path())), None);
    assert_eq!(Settings::resolve(env(&[(ENABLE_ENV, "off")]), Some(home.path())), None);
    assert_eq!(Settings::resolve(env(&[(ENABLE_ENV, "")]), Some(home.path())), None);
    let on = Settings::resolve(env(&[(ENABLE_ENV, "shadow")]), Some(home.path())).unwrap();
    assert_eq!(on.root, home.path().join(".loom").join("journal"));
    assert!(on.verify);
    let over = Settings::resolve(
        env(&[
            (ENABLE_ENV, "1"),
            (DIR_ENV, "/tmp/j1-elsewhere"),
            (VERIFY_ENV, "off"),
        ]),
        Some(home.path()),
    )
    .unwrap();
    assert_eq!(over.root, PathBuf::from("/tmp/j1-elsewhere"));
    assert!(!over.verify);
}

#[test]
fn a_root_inside_a_repository_is_refused() {
    let repo = tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let inside = repo.path().join(".loom").join("journal");
    let inside_str = inside.display().to_string();
    let get = move |k: &str| match k {
        ENABLE_ENV => Some("shadow".to_owned()),
        DIR_ENV => Some(inside_str.clone()),
        _ => None,
    };
    assert_eq!(Settings::resolve(get, None), None);
}

#[test]
fn disabled_writes_nothing() {
    let root = tempdir().unwrap();
    disable();
    let outcome = shadow(record("a", Change::Set(json!({"pid": 1}))));
    assert_eq!(outcome, ShadowOutcome::Disabled);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn set_patch_remove_project_and_match() {
    let root = tempdir().unwrap();
    enable(root.path());
    let base = json!({"issue": 7, "owner_pid": 1, "pgid": null, "acquired_at": "2026-10-10T16:18:13.170Z"});
    let (seq, verdict) = appended(shadow(record("o", Change::Set(base.clone()))));
    assert_eq!((seq, verdict), (1, Verdict::Match));
    let after = json!({"issue": 7, "owner_pid": 42, "pgid": 42, "model": "opus",
                       "acquired_at": "2026-10-10T16:18:13Z"});
    let patch = diff_patch(&base, &after);
    let (_, verdict) = appended(shadow(record(
        "o",
        Change::Patch {
            patch,
            expected: after,
        },
    )));
    assert_eq!(verdict, Verdict::Match);
    let (_, verdict) = appended(shadow(record("o", Change::Remove)));
    assert_eq!(verdict, Verdict::Match);
    let kinds: Vec<String> = records(root.path()).into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, ["claim.acquired"; 3]);
}

#[test]
fn a_patch_without_a_journaled_base_is_explained_not_divergent() {
    let root = tempdir().unwrap();
    enable(root.path());
    let before = divergence_count();
    let expected = json!({"issue": 7, "owner_pid": 42});
    let patch = json!({"owner_pid": 42});
    let (_, verdict) = appended(shadow(record("nb", Change::Patch { patch, expected })));
    assert_eq!(verdict, Verdict::NoBase);
    assert!(records(root.path())
        .iter()
        .all(|e| e.kind != "journal.divergence"));
    // The counter is process-wide; this test adds nothing to it.
    let _ = before;
}

#[test]
fn a_mismatch_counts_warns_and_records_a_durable_divergence() {
    let root = tempdir().unwrap();
    enable(root.path());
    appended(shadow(record("d", Change::Set(json!({"pid": 1, "model": "a"})))));
    let before = divergence_count();
    // The patch does not produce what the legacy store now holds.
    let rec = record(
        "d",
        Change::Patch {
            patch: json!({"pid": 2}),
            expected: json!({"pid": 3, "model": "a"}),
        },
    );
    let (seq, verdict) = appended(shadow(rec));
    assert_eq!(verdict, Verdict::Diverged(vec!["pid".to_owned()]));
    assert!(divergence_count() > before);
    let all = records(root.path());
    let div = all.iter().find(|e| e.kind == "journal.divergence").unwrap();
    assert_eq!(div.data["store"], "sweeps_json");
    assert_eq!(div.data["item"], "d");
    assert_eq!(div.data["seq"], seq);
    assert_eq!(div.data["fields"], json!(["pid"]));
    // The divergence record is not itself projected.
    let (projection, _) =
        Projection::rebuild(&Journal::open(root.path()).stream(STREAM).unwrap()).unwrap();
    assert_eq!(projection.divergence_records, 1);
}

#[test]
fn verification_off_appends_without_comparing() {
    let root = tempdir().unwrap();
    let mut s = settings(root.path());
    s.verify = false;
    let (_, verdict) = appended(shadow_with(&s, record("v", Change::Set(json!(1)))));
    assert_eq!(verdict, Verdict::Unchecked);
}

#[test]
fn a_journal_failure_is_contained_and_counted() {
    let dir = tempdir().unwrap();
    // The root is a regular file: every append fails with an I/O error.
    let root = dir.path().join("not-a-dir");
    std::fs::write(&root, "x").unwrap();
    let before = shadow_failure_count();
    let outcome = shadow_with(&settings(&root), record("f", Change::Set(json!(1))));
    assert!(matches!(outcome, ShadowOutcome::Failed(_)), "{outcome:?}");
    assert!(shadow_failure_count() > before);
}

#[test]
fn a_held_lock_fails_the_shadow_only() {
    let root = tempdir().unwrap();
    let stream = Journal::open(root.path()).stream(STREAM).unwrap();
    let guard = stream.lock().unwrap();
    let outcome = shadow_with(&settings(root.path()), record("l", Change::Set(json!(1))));
    assert!(
        matches!(&outcome, ShadowOutcome::Failed(e) if e.contains("busy")),
        "{outcome:?}"
    );
    drop(guard);
    let (seq, _) =
        appended(shadow_with(&settings(root.path()), record("l", Change::Set(json!(1)))));
    assert_eq!(seq, 1, "the busy attempt wrote nothing");
}

#[test]
fn restart_rebuilds_the_same_projection_and_resumes_seq() {
    let root = tempdir().unwrap();
    enable(root.path());
    for i in 0..5 {
        appended(shadow(record(&format!("k{i}"), Change::Set(json!({"pid": i})))));
    }
    appended(shadow(record("k0", Change::Remove)));
    // A "restart": a fresh projection rebuilt from disk alone.
    let stream = Journal::open(root.path()).stream(STREAM).unwrap();
    let (projection, stats) = Projection::rebuild(&stream).unwrap();
    assert_eq!(stats.records, 6);
    assert_eq!(
        projection
            .get(Store::Sweeps, "/machine/sweeps.json", "k0")
            .unwrap()
            .value,
        None
    );
    assert_eq!(
        projection
            .get(Store::Sweeps, "/machine/sweeps.json", "k4")
            .unwrap()
            .value,
        Some(json!({"pid": 4}))
    );
    let (seq, verdict) = appended(shadow(record("k9", Change::Set(json!(9)))));
    assert_eq!((seq, verdict), (7, Verdict::Match));
}

fn segment(root: &Path) -> PathBuf {
    let dir = root.join(STREAM);
    let mut segments: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();
    segments.pop().unwrap()
}

fn raw_append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

#[test]
fn corrupt_and_partial_records_are_skipped_and_repaired() {
    let root = tempdir().unwrap();
    enable(root.path());
    appended(shadow(record("c", Change::Set(json!({"pid": 1})))));
    let seg = segment(root.path());
    raw_append(&seg, b"{not json}\n");
    raw_append(&seg, b"{\"v\":1,\"stream\":\"execution\",\"seq\":"); // torn tail
    let stream = Journal::open(root.path()).stream(STREAM).unwrap();
    let (_, stats) = Projection::rebuild(&stream).unwrap();
    assert_eq!(stats.corrupt_lines, 1);
    assert!(stats.torn_tail);
    // The next writer repairs the tail and the shadow still matches.
    let (seq, verdict) = appended(shadow(record("c", Change::Set(json!({"pid": 2})))));
    assert_eq!(verdict, Verdict::Match);
    assert_eq!(seq, 2);
    let (projection, stats) = Projection::rebuild(&stream).unwrap();
    assert!(!stats.torn_tail);
    assert_eq!(
        projection
            .get(Store::Sweeps, "/machine/sweeps.json", "c")
            .unwrap()
            .value,
        Some(json!({"pid": 2}))
    );
}

#[test]
fn schema_skew_unknown_major_kind_and_fields_are_tolerated() {
    let root = tempdir().unwrap();
    enable(root.path());
    appended(shadow(record("s", Change::Set(json!({"pid": 1})))));
    let seg = segment(root.path());
    // A newer major, an unknown kind, and a known kind with extra fields.
    raw_append(
        &seg,
        b"{\"v\":2,\"stream\":\"execution\",\"seq\":2,\"kind\":\"claim.acquired\"}\n",
    );
    let w = serde_json::to_string(&crate::journal::WriterIdentity::current()).unwrap();
    let line = |seq: u64, kind: &str, data: Value| {
        format!(
            "{{\"v\":1,\"stream\":\"execution\",\"seq\":{seq},\"kind\":\"{kind}\",\"ts\":\"2026-10-10T00:00:00Z\",\"writer\":{w},\"future\":true,\"data\":{data}}}\n"
        )
    };
    raw_append(&seg, line(3, "claim.teleported", json!({"store": "sweeps_json"})).as_bytes());
    raw_append(
        &seg,
        line(
            4,
            "claim.acquired",
            json!({"store": "sweeps_json", "path": "/machine/sweeps.json", "item": "s",
                   "op": "set", "value": {"pid": 5}, "brand_new": [1, 2]}),
        )
        .as_bytes(),
    );
    let stream = Journal::open(root.path()).stream(STREAM).unwrap();
    let (projection, stats) = Projection::rebuild(&stream).unwrap();
    assert_eq!(stats.unknown_major, 1);
    assert_eq!(projection.unapplied, 1);
    assert_eq!(
        projection
            .get(Store::Sweeps, "/machine/sweeps.json", "s")
            .unwrap()
            .value,
        Some(json!({"pid": 5}))
    );
    // A writer after the skew continues the sequence and still verifies.
    let (seq, verdict) = appended(shadow(record("s", Change::Set(json!({"pid": 6})))));
    assert_eq!((seq, verdict), (5, Verdict::Match));
}

#[test]
fn normalisation_follows_the_documented_rules() {
    assert_eq!(
        normalize(&json!({"a": null, "t": "2026-10-10T16:18:13.170+00:00", "n": [null, 1]})),
        json!({"t": "2026-10-10T16:18:13Z", "n": [null, 1]})
    );
    assert_eq!(normalize(&json!("2026-10-10T18:18:13.9+02:00")), json!("2026-10-10T16:18:13Z"));
}

#[test]
fn diff_patch_round_trips_through_merge_patch() {
    let before = json!({"a": 1, "b": {"x": 1, "y": 2}, "gone": true});
    let after = json!({"a": 2, "b": {"x": 1}, "new": "v"});
    let mut target = before.clone();
    merge_patch(&mut target, &diff_patch(&before, &after));
    assert_eq!(target, after);
}

#[test]
fn credential_values_are_scrubbed_and_names_kept() {
    let mut v = json!({
        "token_name": "agent-1",
        "gh_token": "abc",
        "apiKey": "zzz",
        "nested": {"refresh-token": "r", "account": "acct", "input_tokens": 12},
        "note": "ghp_0123456789",
    });
    scrub(&mut v);
    assert_eq!(v["token_name"], "agent-1");
    assert_eq!(v["gh_token"], REDACTED);
    assert_eq!(v["apiKey"], REDACTED);
    assert_eq!(v["nested"]["refresh-token"], REDACTED);
    assert_eq!(v["nested"]["account"], "acct");
    assert_eq!(v["nested"]["input_tokens"], 12);
    assert_eq!(v["note"], REDACTED);
}

// ---- cross-process lock contention -------------------------------------

const CHILD_MODE: &str = "LOOM_TEST_J1_CHILD_MODE";
const CHILD_ROOT: &str = "LOOM_TEST_J1_CHILD_ROOT";
const CHILD_RECORDS: u64 = 40;

/// Not a test: the body of a child process spawned by the tests below.
#[test]
#[ignore = "child-process helper for the cross-process tests"]
fn child_process_writer() {
    let (Ok(mode), Ok(root)) = (std::env::var(CHILD_MODE), std::env::var(CHILD_ROOT)) else {
        return;
    };
    let root = PathBuf::from(root);
    let mut s = settings(&root);
    s.options.lock_retry = Duration::from_secs(10);
    match mode.as_str() {
        "append" => {
            for i in 0..CHILD_RECORDS {
                let outcome = shadow_with(&s, record(&format!("child-{i}"), Change::Set(json!(i))));
                assert!(matches!(outcome, ShadowOutcome::Appended { .. }), "{outcome:?}");
            }
        }
        "hold" => {
            let stream = Journal::open(&root).stream(STREAM).unwrap();
            let _guard = stream.lock().unwrap();
            std::fs::write(root.join("held"), b"1").unwrap();
            std::thread::sleep(Duration::from_millis(1500));
        }
        other => panic!("unknown child mode {other}"),
    }
}

fn spawn_child(mode: &str, root: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "journal::execution::tests::child_process_writer",
            "--ignored",
            "--test-threads=1",
        ])
        .env(CHILD_MODE, mode)
        .env(CHILD_ROOT, root)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn two_processes_interleave_without_gaps_or_divergence() {
    let root = tempdir().unwrap();
    let mut child = spawn_child("append", root.path());
    let mut s = settings(root.path());
    s.options.lock_retry = Duration::from_secs(10);
    for i in 0..CHILD_RECORDS {
        let outcome = shadow_with(&s, record(&format!("parent-{i}"), Change::Set(json!(i))));
        let (_, verdict) = appended(outcome);
        assert!(matches!(verdict, Verdict::Match | Verdict::Skipped), "{verdict:?}");
    }
    assert!(child.wait().unwrap().success());
    let seqs: Vec<u64> = records(root.path()).iter().map(|e| e.seq).collect();
    let expected: Vec<u64> = (1..=2 * CHILD_RECORDS).collect();
    assert_eq!(seqs, expected);
    let report = verify_concern(root.path());
    assert_eq!(report.recorded_divergences, 0);
    let stream = Journal::open(root.path()).stream(STREAM).unwrap();
    let (projection, _) = Projection::rebuild(&stream).unwrap();
    for i in 0..CHILD_RECORDS {
        for who in ["parent", "child"] {
            let item = format!("{who}-{i}");
            let entry = projection
                .get(Store::Sweeps, "/machine/sweeps.json", &item)
                .unwrap();
            assert_eq!(entry.value, Some(json!(i)));
        }
    }
}

#[test]
fn a_lock_held_by_another_process_fails_only_the_shadow() {
    let root = tempdir().unwrap();
    let mut child = spawn_child("hold", root.path());
    let held = root.path().join("held");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !held.exists() {
        assert!(std::time::Instant::now() < deadline, "child never took the lock");
        std::thread::sleep(Duration::from_millis(10));
    }
    let outcome = shadow_with(&settings(root.path()), record("x", Change::Set(json!(1))));
    assert!(
        matches!(&outcome, ShadowOutcome::Failed(e) if e.contains("busy")),
        "{outcome:?}"
    );
    assert!(child.wait().unwrap().success());
    let (seq, verdict) =
        appended(shadow_with(&settings(root.path()), record("x", Change::Set(json!(1)))));
    assert_eq!((seq, verdict), (1, Verdict::Match));
}
