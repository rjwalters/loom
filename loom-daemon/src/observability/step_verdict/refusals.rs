//! The refusal identity store (#12027 §4): per host, which approved PRs
//! Champion is refusing to merge, on which head, for what, and since when.
//!
//! A refusal comment is posted once per identity: the prompt's idempotency
//! guard suppresses a duplicate. So the moment a `champion:reject` marker is
//! journaled is the moment that identity was first refused, and a later
//! step that ends `rejected` with no marker is the same refusal still
//! standing. This store keeps that moment (`first_refused_at`) so every
//! record of the refusal, and the `pr_refused` gauge, can say how long the PR
//! has been stuck.
//!
//! Keyed by `(repo, pr)`; one entry per PR. An entry is:
//!
//! - **replaced** by a marker of a different identity (criterion, reason key
//!   or a known, different head), with `first_refused_at` = the marker's time;
//! - **kept** (only `last_seen_at` moves) by the same identity;
//! - **cleared** by a merge, a `head-moved` outcome, an open-items walk that
//!   no longer lists the PR open with `loom:pr`, or [`ENTRY_TTL`] without
//!   being seen (a refused PR is re-evaluated every Champion pass, so a
//!   silent entry no longer describes a live refusal).
//!
//! In memory, mirrored to `.loom/state/champion-refusals.json` per workspace
//! so a restart keeps `first_refused_at`. Never a forge write or read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Per reason: `(count, oldest first_refused_at)`.
pub type GaugeByReason = BTreeMap<&'static str, (usize, Option<DateTime<Utc>>)>;

/// An entry not seen for this long is dropped.
pub const ENTRY_TTL: Duration = Duration::from_secs(6 * 3600);

/// The store file under a workspace root.
const STORE_FILE: &str = "champion-refusals.json";

/// One standing refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub criterion_key: String,
    pub reason_key: String,
    /// The projected reason (`ci_failing`, …).
    pub reason: String,
    pub first_refused_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

impl Refusal {
    /// Whether a refusal on `head` (when known) is this one's.
    fn same_head(&self, head: Option<&str>) -> bool {
        match (self.head_sha.as_deref(), head) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

/// One newly observed refusal identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed<'a> {
    pub head_sha: Option<&'a str>,
    pub criterion_key: &'a str,
    pub reason_key: &'a str,
    pub reason: &'a str,
    pub at: DateTime<Utc>,
}

/// Every standing refusal this host knows, by `(repo, pr)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalStore {
    #[serde(default)]
    pub entries: BTreeMap<String, BTreeMap<u64, Refusal>>,
    /// Repos this store has held an entry for: the repos whose
    /// `pr_refused` gauge it reports (`0` included). A repo it never saw
    /// gets no series from this host, so a host that never ran Champion
    /// never reports a false `0`.
    #[serde(default)]
    pub repos: BTreeSet<String>,
}

impl RefusalStore {
    /// Record a refusal identity on `(repo, pr)`; the entry it leaves.
    pub fn observe(&mut self, repo: &str, pr: u64, seen: &Observed<'_>) -> Refusal {
        self.repos.insert(repo.to_string());
        let prs = self.entries.entry(repo.to_string()).or_default();
        if let Some(entry) = prs.get_mut(&pr).filter(|r| {
            r.criterion_key == seen.criterion_key
                && r.reason_key == seen.reason_key
                && r.same_head(seen.head_sha)
        }) {
            entry.last_seen_at = entry.last_seen_at.max(seen.at);
            if entry.head_sha.is_none() {
                entry.head_sha = seen.head_sha.map(str::to_string);
            }
            return entry.clone();
        }
        let entry = Refusal {
            head_sha: seen.head_sha.map(str::to_string),
            criterion_key: seen.criterion_key.to_string(),
            reason_key: seen.reason_key.to_string(),
            reason: seen.reason.to_string(),
            first_refused_at: seen.at,
            last_seen_at: seen.at,
        };
        prs.insert(pr, entry.clone());
        entry
    }

    /// The standing refusal of `(repo, pr)` on `head` (any head when
    /// unknown), marked seen at `at`. A different known head clears it.
    pub fn touch(
        &mut self,
        repo: &str,
        pr: u64,
        head: Option<&str>,
        at: DateTime<Utc>,
    ) -> Option<Refusal> {
        let prs = self.entries.get_mut(repo)?;
        if !prs.get(&pr)?.same_head(head) {
            prs.remove(&pr);
            return None;
        }
        let entry = prs.get_mut(&pr)?;
        entry.last_seen_at = entry.last_seen_at.max(at);
        Some(entry.clone())
    }

    /// Forget `(repo, pr)`: merged, or its head moved.
    pub fn clear(&mut self, repo: &str, pr: u64) {
        if let Some(prs) = self.entries.get_mut(repo) {
            prs.remove(&pr);
        }
    }

    /// Keep only the entries of `repo` whose PR a complete open-items walk
    /// still lists open with `loom:pr` (`approved_open`).
    pub fn reconcile(&mut self, repo: &str, approved_open: &BTreeSet<u64>) {
        if let Some(prs) = self.entries.get_mut(repo) {
            prs.retain(|pr, _| approved_open.contains(pr));
        }
    }

    /// Drop every entry not seen within [`ENTRY_TTL`] of `now`.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let ttl =
            chrono::Duration::from_std(ENTRY_TTL).unwrap_or_else(|_| chrono::Duration::zero());
        for prs in self.entries.values_mut() {
            prs.retain(|_, r| now - r.last_seen_at <= ttl);
        }
        self.entries.retain(|_, prs| !prs.is_empty());
    }

    /// `repo`'s standing refusals per reason: `(count, oldest
    /// first_refused_at)`, every reason of `reasons` present. `None` for a
    /// repo this store never held an entry for.
    #[must_use]
    pub fn gauge(&self, repo: &str, reasons: &[&'static str]) -> Option<GaugeByReason> {
        if !self.repos.contains(repo) {
            return None;
        }
        let mut out: GaugeByReason = reasons.iter().map(|r| (*r, (0, None))).collect();
        for refusal in self
            .entries
            .get(repo)
            .into_iter()
            .flat_map(BTreeMap::values)
        {
            let Some(slot) = reasons.iter().find(|r| **r == refusal.reason) else {
                continue;
            };
            let (count, oldest) = out.entry(slot).or_default();
            *count += 1;
            *oldest =
                Some(oldest.map_or(refusal.first_refused_at, |o| o.min(refusal.first_refused_at)));
        }
        Some(out)
    }

    /// The part of the store that belongs to `repo` (one workspace's file).
    fn for_repo(&self, repo: &str) -> Self {
        Self {
            entries: self
                .entries
                .get(repo)
                .map(|prs| BTreeMap::from([(repo.to_string(), prs.clone())]))
                .unwrap_or_default(),
            repos: self.repos.iter().filter(|r| *r == repo).cloned().collect(),
        }
    }

    fn merge(&mut self, other: Self) {
        for (repo, prs) in other.entries {
            let mine = self.entries.entry(repo).or_default();
            for (pr, refusal) in prs {
                mine.entry(pr).or_insert(refusal);
            }
        }
        self.repos.extend(other.repos);
    }
}

/// The store file of the workspace at `root`.
#[must_use]
pub fn store_path(root: &Path) -> PathBuf {
    root.join(".loom").join("state").join(STORE_FILE)
}

struct Shared {
    store: RefusalStore,
    loaded: BTreeSet<PathBuf>,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

/// Run `f` on this host's store, loading `root`'s file the first time it is
/// named, and write `repo`'s part back to it when `f` reports a change.
pub fn with_store<R>(
    root: Option<&Path>,
    repo: &str,
    f: impl FnOnce(&mut RefusalStore) -> (R, bool),
) -> R {
    let mut guard = SHARED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let shared = guard.get_or_insert_with(|| Shared {
        store: RefusalStore::default(),
        loaded: BTreeSet::new(),
    });
    if let Some(root) = root {
        if shared.loaded.insert(root.to_path_buf()) {
            if let Some(saved) = std::fs::read_to_string(store_path(root))
                .ok()
                .and_then(|t| serde_json::from_str::<RefusalStore>(&t).ok())
            {
                shared.store.merge(saved);
            }
        }
    }
    let (out, changed) = f(&mut shared.store);
    if let (Some(root), true) = (root, changed) {
        save(&store_path(root), &shared.store.for_repo(repo));
    }
    out
}

/// Best-effort write: a failed save loses only restart continuity.
fn save(path: &Path, store: &RefusalStore) {
    let Ok(text) = serde_json::to_string(store) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
