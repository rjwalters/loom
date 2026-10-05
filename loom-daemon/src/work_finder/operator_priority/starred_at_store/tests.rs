//! The starred-at restart store: reuse across a restart, every invalidation,
//! intent precedence, per-workspace files, and the kill switch.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use super::*;
use crate::types::work_finder_tick::StarredAtCacheTally;
use crate::work_finder::operator_priority::{
    resolve_starred_at_with, StarredAtCache, StarredAtSource, OPERATOR_PRIORITY_LABEL,
};
use crate::work_finder::WorkItem;

const T0: &str = "2026-09-01T00:00:00Z";
const T1: &str = "2026-09-20T00:00:00Z";
const REQUESTED: &str = "2026-08-15T12:00:00Z";
const NOW: u64 = 1_800_000_000;

#[derive(Default)]
struct Fake {
    answers: HashMap<u32, String>,
    intent: HashMap<u32, String>,
    calls: Vec<u32>,
}

impl StarredAtSource for Fake {
    fn starred_at(&mut self, issue: u32) -> Result<Option<String>> {
        self.calls.push(issue);
        Ok(self.answers.get(&issue).cloned())
    }

    fn authoritative(&mut self, issue: u32) -> Option<String> {
        self.intent.get(&issue).cloned()
    }
}

fn fake(issue: u32, at: &str) -> Fake {
    let mut f = Fake::default();
    f.answers.insert(issue, at.to_string());
    f
}

fn starred(n: u32) -> WorkItem {
    WorkItem::new(
        n,
        vec![
            OPERATOR_PRIORITY_LABEL.to_string(),
            "loom:issue".to_string(),
        ],
    )
}

fn high(n: u32) -> WorkItem {
    let label = crate::operator_levels::table()
        .iter()
        .find(|r| r.level == 2)
        .map(|r| r.operator_label)
        .unwrap();
    WorkItem::new(n, vec![OPERATOR_PRIORITY_LABEL.to_string(), label.to_string()])
}

fn store(dir: &Path, key: &str) -> StarredAtStore {
    StarredAtStore::in_dir(dir, key)
}

/// One tick of a fresh-or-continuing cache.
fn tick(
    cache: &mut StarredAtCache,
    items: &mut [WorkItem],
    src: &mut Fake,
    store: Option<&StarredAtStore>,
    now_unix: u64,
) -> StarredAtCacheTally {
    cache.resolve_persisted(items, src, Instant::now(), store, now_unix)
}

#[test]
fn a_restart_within_the_gap_reuses_the_persisted_value_with_zero_reads() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|");
    let mut src = fake(1, T0);

    let mut items = [starred(1)];
    let t = tick(&mut StarredAtCache::default(), &mut items, &mut src, Some(&st), NOW);
    assert_eq!((t.read_known, src.calls.len()), (1, 1));
    assert!(st.path().exists(), "a known value is persisted");

    // Restart: a new in-process cache, same levels, 25 minutes later.
    let mut items = [starred(1)];
    let t = tick(&mut StarredAtCache::default(), &mut items, &mut src, Some(&st), NOW + 1500);
    assert_eq!(src.calls.len(), 1, "zero source reads after the restart");
    assert_eq!((t.disk_hit, t.reads()), (1, 0));
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(T0));
}

#[test]
fn the_process_wide_path_survives_a_dropped_in_process_cache() {
    let dir = tempfile::tempdir().unwrap();
    let key = format!("{}|", dir.path().display());
    let st = store(dir.path(), &key);
    let mut src = fake(7, T0);

    resolve_starred_at_with(&key, &mut [starred(7)], &mut src, Some(&st), NOW);
    crate::work_finder::operator_priority::drop_in_process(&key);
    let mut items = [starred(7)];
    let t = resolve_starred_at_with(&key, &mut items, &mut src, Some(&st), NOW + 60);
    assert_eq!(src.calls, vec![7]);
    assert_eq!(t.disk_hit, 1);
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(T0));
}

#[test]
fn unstar_then_a_nothing_starred_tick_then_restar_reads_the_new_time() {
    let dir = tempfile::tempdir().unwrap();
    let key = format!("{}|restar", dir.path().display());
    let st = store(dir.path(), &key);
    let mut src = fake(1, T0);

    resolve_starred_at_with(&key, &mut [starred(1)], &mut src, Some(&st), NOW);
    assert!(st.path().exists());
    // Nothing starred: the whole repo's entries go, on disk too.
    resolve_starred_at_with(&key, &mut [WorkItem::new(1, vec![])], &mut src, Some(&st), NOW + 60);
    assert!(!st.path().exists(), "the whole-map drop truncates the file");
    // Re-starred within the gap, after a restart: the source is read.
    crate::work_finder::operator_priority::drop_in_process(&key);
    src.answers.insert(1, T1.into());
    let mut items = [starred(1)];
    resolve_starred_at_with(&key, &mut items, &mut src, Some(&st), NOW + 120);
    assert_eq!(src.calls, vec![1, 1]);
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(T1), "a NEW time, not T0");
}

#[test]
fn an_intent_outranks_the_persisted_value_and_round_trips_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|intent");
    let mut src = fake(1, T0);
    tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&st), NOW);

    // An intent is applied after the disk entry exists: it wins.
    src.intent.insert(1, REQUESTED.into());
    let mut items = [starred(1)];
    let t = tick(&mut StarredAtCache::default(), &mut items, &mut src, Some(&st), NOW + 60);
    assert_eq!((t.intent_hit, t.disk_hit), (1, 0));
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(REQUESTED));

    // Restart again with the in-memory intent record gone: the persisted
    // value is the intent's requested_at, byte for byte.
    src.intent.clear();
    let mut items = [starred(1)];
    let t = tick(&mut StarredAtCache::default(), &mut items, &mut src, Some(&st), NOW + 120);
    assert_eq!(t.disk_hit, 1);
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(REQUESTED));
    assert_eq!(src.calls, vec![1], "only the first tick read the timeline");
}

#[test]
fn a_different_level_set_forces_a_reread() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|levels");
    let mut src = fake(1, T0);
    tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&st), NOW);
    src.answers.insert(1, T1.into());
    let mut items = [high(1)];
    let t = tick(&mut StarredAtCache::default(), &mut items, &mut src, Some(&st), NOW + 60);
    assert_eq!((t.disk_hit, t.read_known), (0, 1));
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(T1));
}

#[test]
fn a_value_last_seen_beyond_the_gap_forces_a_reread() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|gap");
    let mut src = fake(1, T0);
    tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&st), NOW);
    let t = tick(
        &mut StarredAtCache::default(),
        &mut [starred(1)],
        &mut src,
        Some(&st),
        NOW + RESTART_GAP_SECS + 1,
    );
    assert_eq!((t.disk_hit, t.read_known), (0, 1));
    assert_eq!(src.calls, vec![1, 1]);
}

#[test]
fn an_unknown_value_is_never_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|unknown");
    let mut src = Fake::default();
    let t = tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&st), NOW);
    assert_eq!(t.read_none, 1);
    assert!(st.load().is_empty(), "no unknown on disk");
    let t = tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&st), NOW + 60);
    assert_eq!((t.disk_hit, t.read_none), (0, 1), "the restart reads again");
}

#[test]
fn eviction_from_the_starred_set_deletes_the_disk_entry() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|evict");
    let mut src = fake(1, T0);
    src.answers.insert(2, T0.into());
    let mut cache = StarredAtCache::default();
    tick(&mut cache, &mut [starred(1), starred(2)], &mut src, Some(&st), NOW);
    assert_eq!(st.load().len(), 2);
    // Issue 1 leaves the starred set (claimed or unstarred); 2 stays.
    tick(&mut cache, &mut [starred(2)], &mut src, Some(&st), NOW + 10);
    assert_eq!(st.load().keys().copied().collect::<Vec<_>>(), vec![2]);
    // Restart and issue 1 is starred again: read, not reused.
    let t = tick(
        &mut StarredAtCache::default(),
        &mut [starred(1), starred(2)],
        &mut src,
        Some(&st),
        NOW + 20,
    );
    assert_eq!((t.disk_hit, t.read_known), (1, 1));
    assert_eq!(src.calls, vec![1, 2, 1]);
}

#[test]
fn two_workspaces_of_one_repo_keep_separate_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = store(dir.path(), "/ws-a|");
    let b = store(dir.path(), "/ws-b|");
    assert_ne!(a.path(), b.path());

    let mut src = fake(1, T0);
    let (mut ca, mut cb) = (StarredAtCache::default(), StarredAtCache::default());
    tick(&mut ca, &mut [starred(1)], &mut src, Some(&a), NOW);
    tick(&mut cb, &mut [starred(1)], &mut src, Some(&b), NOW);
    // A evicts issue 1; B keeps it and writes its last_seen later.
    tick(&mut ca, &mut [starred(9)], &mut Fake::default(), Some(&a), NOW + 10);
    tick(&mut cb, &mut [starred(1)], &mut src, Some(&b), NOW + LAST_SEEN_WRITE_SECS + 10);
    assert!(!a.load().contains_key(&1), "B's write never resurrects A's eviction");
    assert_eq!(b.load()[&1].last_seen, NOW + LAST_SEEN_WRITE_SECS + 10);
    // A restarts: it reads the timeline again.
    let calls = src.calls.len();
    let t = tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&a), NOW + 400);
    assert_eq!((t.disk_hit, src.calls.len()), (0, calls + 1));
}

#[test]
fn last_seen_is_written_at_most_every_five_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|touch");
    let mut src = fake(1, T0);
    let mut cache = StarredAtCache::default();
    tick(&mut cache, &mut [starred(1)], &mut src, Some(&st), NOW);
    tick(&mut cache, &mut [starred(1)], &mut src, Some(&st), NOW + 60);
    assert_eq!(st.load()[&1].last_seen, NOW, "no rewrite inside the window");
    tick(&mut cache, &mut [starred(1)], &mut src, Some(&st), NOW + LAST_SEEN_WRITE_SECS);
    assert_eq!(st.load()[&1].last_seen, NOW + LAST_SEEN_WRITE_SECS);
}

#[test]
fn the_kill_switch_writes_no_file_and_reads_none() {
    let dir = tempfile::tempdir().unwrap();
    for off in ["0", "false", "off", "no"] {
        assert!(StarredAtStore::for_repo_with("/ws|", Some(dir.path().into()), Some(off)).is_none());
    }
    assert!(StarredAtStore::for_repo_with("/ws|", Some(dir.path().into()), None).is_some());
    assert!(StarredAtStore::for_repo_with("/ws|", Some(dir.path().into()), Some("1")).is_some());

    // A file a persisting run left behind is not read with the switch off.
    let st = store(dir.path(), "/ws|kill");
    let mut src = fake(1, T0);
    tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, Some(&st), NOW);
    let off = StarredAtStore::for_repo_with("/ws|kill", Some(dir.path().into()), Some("0"));
    let t = tick(
        &mut StarredAtCache::default(),
        &mut [starred(1)],
        &mut src,
        off.as_ref(),
        NOW + 60,
    );
    assert_eq!((t.disk_hit, t.read_known), (0, 1));

    // And nothing is written with it off.
    let empty = tempfile::tempdir().unwrap();
    let off = StarredAtStore::for_repo_with("/ws|", Some(empty.path().into()), Some("0"));
    tick(&mut StarredAtCache::default(), &mut [starred(1)], &mut src, off.as_ref(), NOW);
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}

#[test]
fn the_production_store_follows_the_daemon_store_dir() {
    crate::forge_etag_store::set_test_daemon_store_dir(None);
    assert!(StarredAtStore::for_repo("/ws|").is_none(), "tests are off by default");
    let dir = tempfile::tempdir().unwrap();
    crate::forge_etag_store::set_test_daemon_store_dir(Some(dir.path().into()));
    let st = StarredAtStore::for_repo("/ws|");
    crate::forge_etag_store::set_test_daemon_store_dir(None);
    if std::env::var(PERSIST_ENV).is_err() {
        assert_eq!(st.unwrap().path().parent(), Some(dir.path()));
    }
}

#[test]
fn a_file_for_another_key_is_never_served() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|one");
    tick(
        &mut StarredAtCache::default(),
        &mut [starred(1)],
        &mut fake(1, T0),
        Some(&st),
        NOW,
    );
    // Same path, different key (a hash collision stand-in).
    let other = StarredAtStore {
        path: st.path().to_path_buf(),
        repo_key: "/ws|two".into(),
    };
    assert!(other.load().is_empty());
}

#[test]
fn old_store_files_are_garbage_collected() {
    let dir = tempfile::tempdir().unwrap();
    let st = store(dir.path(), "/ws|gc");
    tick(
        &mut StarredAtCache::default(),
        &mut [starred(1)],
        &mut fake(1, T0),
        Some(&st),
        NOW,
    );
    let unrelated = dir.path().join("listing-0000.json");
    std::fs::write(&unrelated, "{}").unwrap();
    let now = unix_now();
    assert_eq!(gc_dir(dir.path(), now), 0, "a fresh file stays");
    assert_eq!(gc_dir(dir.path(), now + GC_AFTER_SECS + 60), 1);
    assert!(!st.path().exists());
    assert!(unrelated.exists(), "only starred-at files are collected");
}

#[test]
fn the_in_process_hit_is_tallied() {
    let mut src = fake(1, T0);
    let mut cache = StarredAtCache::default();
    let t = cache.resolve(&mut [starred(1), starred(2)], &mut src, Instant::now());
    assert_eq!((t.read_known, t.read_none, t.mem_hit), (1, 1, 0));
    let t = cache.resolve(&mut [starred(1)], &mut src, Instant::now());
    assert_eq!((t.mem_hit, t.reads()), (1, 0));
}

#[test]
fn the_timeline_read_is_typed_and_names_the_repo_when_one_is_known() {
    use crate::gh_invocation::GhTarget;
    use crate::work_finder::operator_priority::timeline_target;
    let (target, url) = timeline_target(Some("acme/widgets"), Some("acme/other"), 12);
    assert_eq!(target, GhTarget::repo("acme/widgets").unwrap());
    assert_eq!(url, "repos/acme/widgets/issues/12/timeline");
    // LOOM_REPO is what the placeholder resolves to (the facade's GH_REPO).
    let (target, url) = timeline_target(None, Some("acme/other"), 12);
    assert_eq!(target, GhTarget::repo("acme/other").unwrap());
    assert_eq!(url, "repos/acme/other/issues/12/timeline");
    // Neither: the placeholder, resolved by gh from the working directory.
    let (target, url) = timeline_target(None, None, 12);
    assert_eq!(target, GhTarget::None);
    assert_eq!(url, "repos/{owner}/{repo}/issues/12/timeline");
}

#[test]
fn an_intent_answering_a_level_change_is_not_tallied_as_a_read() {
    let mut src = fake(1, T0);
    let mut cache = StarredAtCache::default();
    cache.resolve(&mut [starred(1)], &mut src, Instant::now());
    src.intent.insert(1, REQUESTED.into());
    let mut items = [high(1)];
    let t = cache.resolve(&mut items, &mut src, Instant::now());
    assert_eq!((t.intent_hit, t.reads()), (1, 0));
    assert_eq!(src.calls, vec![1]);
    assert_eq!(items[0].operator_priority_at.as_deref(), Some(REQUESTED));
}
