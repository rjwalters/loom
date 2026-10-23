//! The captain's fleet queue (#12816; design `docs/design/fleet-captain-dispatch.md`
//! §10, §13.2): the items that call for a step, per repo, classified from the
//! mirror's worklist.
//!
//! Pure: no I/O, no wall-clock read. Every call takes `now` from the caller's
//! **monotonic** clock ([`Mono`]), so the refresh age never depends on a
//! timestamp from the forge, and tests drive time by hand. The worklist
//! arrives as [`RepoSnapshot`]s from the slice-1 library API
//! ([`crate::discovery::mirror::MirrorBackend::fetch_from`]); the change
//! stream ([`crate::forge_mirror::wake`]) only says *which* repos to re-read.
//!
//! # Feeding
//!
//! - **A change for a repo** ([`Queue::refresh_repos`]) re-reads that repo's
//!   rows alone and re-classifies them; every other repo's entries stay. An
//!   event is a prompt, never an edit: nothing here reads the event.
//! - **The full refresh** ([`Queue::refresh_all`]) is the backstop: every
//!   `queueRefreshSecs` the whole worklist replaces the queue, so a lossy
//!   stream costs at most one refresh of staleness. A `304`
//!   ([`Queue::not_modified`]) changes no entry and only advances the age.
//!
//! # Tombstones
//!
//! A refused entry ([`Queue::tombstone`]) leaves the queue and records
//! `(repo, item, updated_at)`. A refresh that shows the **same** `updated_at`
//! does not re-admit it, so a stale mirror cannot feed a refused item back
//! every refresh; a **newer** one does, and clears the tombstone. A tombstone
//! also ends once its item is no longer open in its repo's rows (or the repo
//! left the worklist).
//!
//! # Not here
//!
//! Soft state: a captain restart starts with an empty queue until the first
//! refresh. The queue serves no live pre-check (invariant 3 keeps that a live
//! read), assigns nothing (`next`, the ledger and per-host fit are later
//! slices), and holds no forge write. Classification uses a default
//! [`HostInput`]: capability-lane items read as not held until per-host fit
//! lands.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::table::Mono;
use crate::discovery::snapshot::RepoSnapshot;
use crate::forge_pull_listing::RestPull;
use crate::step_dispatch::gather;
use crate::step_role::StepRole;
use crate::work_finder::step_item::{self, HostInput, RepoInput};

/// `fleetDispatch.queueRefreshSecs`' default (§13.4).
pub const DEFAULT_REFRESH_SECS: u64 = 600;

/// One item that calls for a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub repo: String,
    /// The number the step's role is invoked on (the PR for a PR step).
    pub item: u32,
    pub step: StepRole,
    /// The newest `updated_at` of the item's issue and PR, as the worklist
    /// sent it (RFC-3339: lexicographic order is chronological).
    pub updated_at: String,
}

/// The entries `snapshot` classifies into a step, in item order. Pure.
#[must_use]
pub fn classify_repo(snapshot: &RepoSnapshot) -> Vec<Entry> {
    let pulls: Vec<_> = match &snapshot.pulls {
        Some(pulls) => pulls
            .iter()
            .map(|p| pull_input(&snapshot.slug, p))
            .collect(),
        None => snapshot
            .rows
            .iter()
            .filter(|r| r.is_pull_request)
            .map(|r| gather::pull_from_row(r, step_item::PullMarkers::default()))
            .collect(),
    };
    let inputs = gather::build_inputs(
        &snapshot.rows,
        pulls,
        &gather::IssueFacts::default(),
        RepoInput::default(),
        HostInput::default(),
    );
    let updated: BTreeMap<u32, &str> = snapshot
        .rows
        .iter()
        .filter_map(|r| Some((r.number, r.updated_at.as_deref()?)))
        .collect();
    let at = |n: Option<u32>| n.and_then(|n| updated.get(&n).copied());
    step_item::classify(&inputs)
        .into_iter()
        .filter_map(|item| {
            let step = item.step()?;
            let target = item.target()?;
            let updated_at = at(item.issue).max(at(item.pr))?.to_string();
            Some(Entry {
                repo: snapshot.slug.to_ascii_lowercase(),
                item: target,
                step: step.into(),
                updated_at,
            })
        })
        .collect()
}

fn pull_input(slug: &str, p: &RestPull) -> step_item::PullInput {
    step_item::PullInput {
        number: p.number,
        labels: p.labels.clone(),
        body: p.body.clone(),
        head_ref: p.head_ref.clone(),
        same_repo: p
            .head_repo
            .as_deref()
            .is_some_and(|h| h.eq_ignore_ascii_case(slug)),
        author_untrusted: false,
        queued: false,
        draft: p.draft,
        markers: step_item::PullMarkers::default(),
    }
}

/// The fleet queue and its tombstones.
#[derive(Debug, Clone)]
pub struct Queue {
    refresh: Duration,
    repos: BTreeMap<String, Vec<Entry>>,
    tombstones: BTreeMap<(String, u32), String>,
    /// When a full worklist read last answered (`200` or `304`).
    last_full: Option<Mono>,
}

impl Queue {
    /// An empty queue that wants a full refresh every `refresh`.
    #[must_use]
    pub fn new(refresh: Duration) -> Self {
        Self {
            refresh,
            repos: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            last_full: None,
        }
    }

    /// Change the full refresh's period; entries and age are kept.
    pub fn set_refresh(&mut self, refresh: Duration) {
        self.refresh = refresh;
    }

    /// Whether the full refresh is due: never done, or `refresh` old.
    #[must_use]
    pub fn refresh_due(&self, now: Mono) -> bool {
        self.age(now).is_none_or(|age| age >= self.refresh)
    }

    /// Time since the last full worklist read answered; `None` before the
    /// first.
    #[must_use]
    pub fn age(&self, now: Mono) -> Option<Duration> {
        self.last_full.map(|at| now.0.saturating_sub(at.0))
    }

    /// A `304` on the full read: the queue is as current as the mirror says,
    /// so only the age advances.
    pub fn not_modified(&mut self, now: Mono) {
        self.last_full = Some(now);
    }

    /// The full refresh: `snapshots` are every repo the worklist carries.
    /// Replaces every repo's entries; a repo the worklist no longer carries
    /// leaves the queue.
    pub fn refresh_all<'a>(
        &mut self,
        now: Mono,
        snapshots: impl IntoIterator<Item = &'a RepoSnapshot>,
    ) {
        let mut seen = BTreeSet::new();
        for snapshot in snapshots {
            seen.insert(snapshot.slug.to_ascii_lowercase());
            self.apply(snapshot);
        }
        self.repos.retain(|repo, _| seen.contains(repo));
        self.tombstones.retain(|(repo, _), _| seen.contains(repo));
        self.last_full = Some(now);
    }

    /// A change for these repos: re-read and re-classify only their rows.
    /// Does not advance the full refresh's age.
    pub fn refresh_repos<'a>(&mut self, snapshots: impl IntoIterator<Item = &'a RepoSnapshot>) {
        for snapshot in snapshots {
            self.apply(snapshot);
        }
    }

    /// Forget a repo the worklist can no longer vouch for or no longer reads.
    pub fn drop_repo(&mut self, repo: &str) {
        let repo = repo.to_ascii_lowercase();
        self.repos.remove(&repo);
        self.tombstones.retain(|(r, _), _| *r != repo);
    }

    fn apply(&mut self, snapshot: &RepoSnapshot) {
        let repo = snapshot.slug.to_ascii_lowercase();
        let open: BTreeSet<u32> = snapshot.rows.iter().map(|r| r.number).collect();
        let mut entries = Vec::new();
        for entry in classify_repo(snapshot) {
            let key = (repo.clone(), entry.item);
            match self.tombstones.get(&key) {
                Some(refused) if entry.updated_at.as_str() <= refused.as_str() => continue,
                Some(_) => {
                    self.tombstones.remove(&key);
                }
                None => {}
            }
            entries.push(entry);
        }
        // An item that left the open rows no longer needs its tombstone.
        self.tombstones
            .retain(|(r, item), _| *r != repo || open.contains(item));
        self.repos.insert(repo, entries);
    }

    /// A refusal of `(repo, item)`: drop its entry and tombstone its
    /// `updated_at`. `false` when the queue holds no such entry (nothing to
    /// tombstone: the next refresh decides afresh).
    pub fn tombstone(&mut self, repo: &str, item: u32) -> bool {
        let repo = repo.to_ascii_lowercase();
        let Some(entries) = self.repos.get_mut(&repo) else {
            return false;
        };
        let Some(at) = entries.iter().position(|e| e.item == item) else {
            return false;
        };
        let gone = entries.remove(at);
        self.tombstones.insert((repo, item), gone.updated_at);
        true
    }

    /// Every entry, by repo then classification order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.repos.values().flatten()
    }

    /// One repo's entries.
    #[must_use]
    pub fn repo_entries(&self, repo: &str) -> &[Entry] {
        self.repos
            .get(&repo.to_ascii_lowercase())
            .map_or(&[], Vec::as_slice)
    }

    /// Entries held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.repos.values().map(Vec::len).sum()
    }

    /// No entries held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repos.values().all(Vec::is_empty)
    }

    /// Tombstones held.
    #[must_use]
    pub fn tombstones(&self) -> usize {
        self.tombstones.len()
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
