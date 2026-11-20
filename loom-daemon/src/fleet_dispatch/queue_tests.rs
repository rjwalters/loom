#![allow(clippy::unwrap_used)]

use super::*;
use crate::discovery::chain::Tier;
use crate::discovery::snapshot::{RepoStatus, Source};
use crate::forge_listing::RestIssue;
use crate::forge_mirror::client::ChangesPage;
use crate::forge_mirror::wake::{self, Cursors, Nudge};
use chrono::Utc;
use serde_json::json;

const REPO: &str = "acme/widgets";
const OTHER: &str = "acme/gears";
const T1: &str = "2026-10-19T10:00:00Z";
const T2: &str = "2026-10-19T11:00:00Z";

fn mono(secs: u64) -> Mono {
    Mono(Duration::from_secs(secs))
}

fn row(number: u32, labels: &[&str], updated_at: &str) -> RestIssue {
    RestIssue {
        number,
        title: None,
        labels: labels.iter().map(ToString::to_string).collect(),
        created_at: Some(T1.to_string()),
        updated_at: Some(updated_at.to_string()),
        closed_at: None,
        state: "open".to_string(),
        body: None,
        author: None,
        author_association: None,
        is_pull_request: false,
        comments: 0,
        open_sub_issues: None,
        labels_at: None,
        draft: false,
    }
}

fn snapshot(slug: &str, rows: Vec<RestIssue>) -> RepoSnapshot {
    RepoSnapshot {
        slug: slug.to_string(),
        source: Source::Worklist,
        tier: Tier::Mirror,
        status: RepoStatus::Ok,
        rows,
        pulls: None,
        extras: BTreeMap::new(),
        watermark: None,
    }
}

fn steps(q: &Queue, repo: &str) -> Vec<(u32, StepRole)> {
    q.repo_entries(repo)
        .iter()
        .map(|e| (e.item, e.step))
        .collect()
}

#[test]
fn classifies_per_repo_from_the_worklist() {
    let mut q = Queue::new(Duration::from_secs(600));
    assert!(q.is_empty());
    q.refresh_all(
        mono(0),
        &[
            snapshot(REPO, vec![row(1, &["loom:issue"], T1), row(2, &["loom:triage"], T1)]),
            snapshot(OTHER, vec![row(9, &["loom:curated"], T1)]),
        ],
    );
    assert_eq!(steps(&q, REPO), vec![(1, StepRole::Builder), (2, StepRole::Curator)]);
    assert_eq!(steps(&q, OTHER), vec![(9, StepRole::ChampionPromote)]);
    assert_eq!(q.len(), 3);
}

#[test]
fn a_held_item_is_not_queued() {
    let mut q = Queue::new(Duration::from_secs(600));
    q.refresh_all(
        mono(0),
        &[snapshot(
            REPO,
            vec![row(1, &["loom:issue", "loom:operator"], T1)],
        )],
    );
    assert!(q.is_empty());
}

#[test]
fn a_change_rereads_only_that_repos_rows() {
    let mut q = Queue::new(Duration::from_secs(600));
    q.refresh_all(
        mono(0),
        &[
            snapshot(REPO, vec![row(1, &["loom:issue"], T1)]),
            snapshot(OTHER, vec![row(9, &["loom:curated"], T1)]),
        ],
    );
    // The change stream names REPO; its issue moved to building.
    let mut cursors = Cursors::new("o");
    let mut nudge = Nudge::default();
    let page: ChangesPage = serde_json::from_value(json!({
        "changes": [{"seq": 5, "repo": REPO, "kind": "label", "item_type": "issue"}],
        "repos": {REPO: {"next_after": 5, "more": false, "truncated": false,
                         "watermark": {"repo": REPO, "known": true, "latest_seq": 5}}},
        "more": false,
    }))
    .unwrap();
    wake::apply(&mut cursors, &[REPO.to_string()], &page, Utc::now(), &mut nudge);
    assert_eq!(nudge.repos, BTreeSet::from([REPO.to_string()]));

    // OTHER's row also changed in the new worklist, but was not named.
    let fresh = [
        snapshot(REPO, vec![row(1, &["loom:building"], T2)]),
        snapshot(OTHER, vec![row(9, &["loom:triage"], T2)]),
    ];
    q.refresh_repos(fresh.iter().filter(|s| nudge.repos.contains(&s.slug)));
    assert!(steps(&q, REPO).is_empty());
    assert_eq!(steps(&q, OTHER), vec![(9, StepRole::ChampionPromote)]);
    // A repo change does not touch the full refresh's age.
    assert_eq!(q.age(mono(30)), Some(Duration::from_secs(30)));
}

#[test]
fn the_full_refresh_recovers_a_lossy_stream() {
    let mut q = Queue::new(Duration::from_secs(600));
    q.refresh_all(mono(0), &[snapshot(REPO, vec![row(1, &["loom:issue"], T1)])]);
    assert!(!q.refresh_due(mono(599)));
    // No change ever named REPO; its issue closed and a new one appeared.
    assert!(q.refresh_due(mono(600)));
    q.refresh_all(mono(600), &[snapshot(REPO, vec![row(3, &["loom:issue"], T2)])]);
    assert_eq!(steps(&q, REPO), vec![(3, StepRole::Builder)]);
    assert!(!q.refresh_due(mono(601)));
}

#[test]
fn a_full_refresh_drops_a_repo_the_worklist_no_longer_carries() {
    let mut q = Queue::new(Duration::from_secs(600));
    q.refresh_all(
        mono(0),
        &[
            snapshot(REPO, vec![row(1, &["loom:issue"], T1)]),
            snapshot(OTHER, vec![row(9, &["loom:curated"], T1)]),
        ],
    );
    q.refresh_all(mono(600), &[snapshot(REPO, vec![row(1, &["loom:issue"], T1)])]);
    assert!(q.repo_entries(OTHER).is_empty());
    assert_eq!(q.len(), 1);
}

#[test]
fn a_304_only_advances_the_age() {
    let mut q = Queue::new(Duration::from_secs(600));
    q.refresh_all(mono(0), &[snapshot(REPO, vec![row(1, &["loom:issue"], T1)])]);
    let before: Vec<_> = q.entries().cloned().collect();
    assert!(q.refresh_due(mono(600)));
    q.not_modified(mono(600));
    assert_eq!(q.age(mono(610)), Some(Duration::from_secs(10)));
    assert!(!q.refresh_due(mono(610)));
    assert_eq!(q.entries().cloned().collect::<Vec<_>>(), before);
}

#[test]
fn a_refused_entry_is_tombstoned_until_a_newer_updated_at() {
    let mut q = Queue::new(Duration::from_secs(600));
    let same = snapshot(REPO, vec![row(1, &["loom:issue"], T1)]);
    q.refresh_all(mono(0), std::slice::from_ref(&same));
    assert!(q.tombstone(REPO, 1));
    assert!(q.is_empty());
    assert_eq!(q.tombstones(), 1);

    // A stale mirror shows the same updated_at: not re-admitted.
    q.refresh_all(mono(600), std::slice::from_ref(&same));
    assert!(q.is_empty());
    q.refresh_repos([&same]);
    assert!(q.is_empty());
    assert_eq!(q.tombstones(), 1);

    // A newer updated_at re-admits it and clears the tombstone.
    q.refresh_all(mono(1200), &[snapshot(REPO, vec![row(1, &["loom:issue"], T2)])]);
    assert_eq!(steps(&q, REPO), vec![(1, StepRole::Builder)]);
    assert_eq!(q.tombstones(), 0);
}

#[test]
fn a_tombstone_ends_when_its_item_closes() {
    let mut q = Queue::new(Duration::from_secs(600));
    q.refresh_all(mono(0), &[snapshot(REPO, vec![row(1, &["loom:issue"], T1)])]);
    assert!(q.tombstone(REPO, 1));
    q.refresh_all(mono(600), &[snapshot(REPO, vec![])]);
    assert_eq!(q.tombstones(), 0);
    // A new item that reuses nothing of the old one is admitted.
    q.refresh_all(mono(1200), &[snapshot(REPO, vec![row(1, &["loom:issue"], T1)])]);
    assert_eq!(q.len(), 1);
}

#[test]
fn tombstoning_an_absent_entry_does_nothing() {
    let mut q = Queue::new(Duration::from_secs(600));
    assert!(!q.tombstone(REPO, 1));
    assert_eq!(q.tombstones(), 0);
}

#[test]
fn a_restarted_captain_has_an_empty_queue_until_the_first_refresh() {
    let q = Queue::new(Duration::from_secs(600));
    assert!(q.is_empty());
    assert_eq!(q.age(mono(5)), None);
    assert!(q.refresh_due(mono(5)));
}
