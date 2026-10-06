//! The forge bucket book (W1 of the forge API reduction plan): the newest
//! known reading of every GitHub rate-limit **bucket** this host spends.
//!
//! GitHub meters each credential separately, per resource: a GitHub App
//! installation for one owner has its own `core` and `graphql` pools, and a
//! reader App's installation for the same owner has others. A single
//! "remaining" figure per resource — what `forge_call_stats` and the
//! rate-limit breaker kept — cannot say which of those pools is draining.
//! The book keys every reading by `(account, owner, resource)`:
//!
//! - **account** — `app-<id>` for an App installation (`app-unknown` when
//!   the writer's id is not configured), never a token or path;
//! - **owner** — the GitHub owner the installation covers, lowercased;
//! - **resource** — `core`, `graphql` or `search`.
//!
//! Two sources feed it: the free `x-ratelimit-*` headers of any `gh api
//! --include` call the facade attributed to an App credential
//! ([`observe`]), and one free `gh api rate_limit` probe per published
//! credential directory after every reader-refresh pass ([`probe_all`]).
//! A reading counts only while its window is open and it is younger than
//! [`MAX_AGE_SECS`] ([`reading`]).
//!
//! This is the one per-bucket state in the daemon: later budget gates extend
//! it rather than keep a second copy. Every operation is in-memory or a
//! local file; none can fail or block a forge call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::forge_call_stats::RateLimitHeaders;

mod probe;
pub use probe::{
    parse_probe, probe_all, probe_all_with, probe_invocation, probe_targets, ProbeTarget,
    PROBE_OPERATION,
};

/// A reading older than this is not believed, whatever its reset says.
pub const MAX_AGE_SECS: i64 = 600;

/// Most buckets the book holds; a new bucket past this is dropped (a host
/// has a handful of credentials × owners × resources).
const MAX_BUCKETS: usize = 512;

/// The snapshot file [`persist`] writes into the forge-call sink directory,
/// so a separate CLI process can show the daemon's readings.
pub const SNAPSHOT_FILE: &str = "bucket-book.json";

/// A billed GitHub rate-limit resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Core,
    Graphql,
    Search,
}

impl Resource {
    /// Classify an `x-ratelimit-resource` value; any other resource
    /// (`code_search`, `integration_manifest`, …) is not booked.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "core" => Some(Self::Core),
            "graphql" => Some(Self::Graphql),
            "search" => Some(Self::Search),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Graphql => "graphql",
            Self::Search => "search",
        }
    }
}

/// One billed bucket.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BucketKey {
    /// `app-<id>` / `app-unknown`.
    pub account: String,
    /// The installation's owner, lowercased.
    pub owner: String,
    pub resource: Resource,
}

impl BucketKey {
    /// A key; `owner` is lowercased (GitHub owners are case-insensitive).
    #[must_use]
    pub fn new(account: &str, owner: &str, resource: Resource) -> Self {
        Self {
            account: account.to_string(),
            owner: owner.to_ascii_lowercase(),
            resource,
        }
    }
}

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A response's `x-ratelimit-*` headers.
    Header,
    /// A `gh api rate_limit` probe.
    Probe,
}

impl Source {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Probe => "probe",
        }
    }
}

/// One bucket's reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub used: Option<u64>,
    /// When the window resets (epoch seconds).
    pub reset_epoch: i64,
    /// When it was read (epoch seconds).
    pub observed_at: i64,
    pub source: Source,
}

impl Reading {
    /// Believed at `now`: the window is still open and the reading is
    /// younger than [`MAX_AGE_SECS`].
    #[must_use]
    pub fn is_fresh(&self, now: i64) -> bool {
        self.reset_epoch > now && now - self.observed_at < MAX_AGE_SECS
    }
}

fn book() -> &'static Mutex<HashMap<BucketKey, Reading>> {
    static BOOK: OnceLock<Mutex<HashMap<BucketKey, Reading>>> = OnceLock::new();
    BOOK.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keep `reading` for `key` unless a newer one is already held.
pub fn insert(key: BucketKey, reading: Reading) {
    let Ok(mut book) = book().lock() else {
        return;
    };
    if book.len() >= MAX_BUCKETS && !book.contains_key(&key) {
        return;
    }
    let newer = book
        .get(&key)
        .is_none_or(|held| held.observed_at <= reading.observed_at);
    if newer {
        book.insert(key, reading);
    }
}

/// Book the free headers of one response for `key`, read now.
pub fn observe(key: BucketKey, headers: &RateLimitHeaders, source: Source) {
    observe_at(key, headers, source, chrono::Utc::now().timestamp());
}

/// [`observe`] at an explicit instant. Headers without a reset, or with no
/// count at all, say nothing about the bucket and are ignored.
pub fn observe_at(key: BucketKey, headers: &RateLimitHeaders, source: Source, now: i64) {
    let Some(reset_epoch) = headers.reset_epoch else {
        return;
    };
    if headers.remaining.is_none() && headers.used.is_none() && headers.limit.is_none() {
        return;
    }
    insert(
        key,
        Reading {
            limit: headers.limit,
            remaining: headers.remaining,
            used: headers.used,
            reset_epoch,
            observed_at: now,
            source,
        },
    );
}

/// `key`'s reading, when one is believed at `now` ([`Reading::is_fresh`]).
#[must_use]
pub fn reading(key: &BucketKey, now: i64) -> Option<Reading> {
    let book = book().lock().ok()?;
    book.get(key).copied().filter(|r| r.is_fresh(now))
}

/// Every reading believed at `now`, in key order.
#[must_use]
pub fn snapshot(now: i64) -> Vec<(BucketKey, Reading)> {
    let mut out: Vec<(BucketKey, Reading)> = match book().lock() {
        Ok(book) => book
            .iter()
            .filter(|(_, r)| r.is_fresh(now))
            .map(|(k, r)| (k.clone(), *r))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: BucketKey,
    reading: Reading,
}

/// Write the readings believed at `now` to [`SNAPSHOT_FILE`] in `dir` (the
/// owner-only forge-call sink directory), atomically.
///
/// # Errors
///
/// When `dir` is not a private directory or the write fails.
pub fn persist(dir: &Path, now: i64) -> std::io::Result<()> {
    use std::io::Write;
    if !crate::forge_etag_store::private_dir(dir, true) {
        return Err(std::io::Error::other("untrusted sink dir"));
    }
    let entries: Vec<Entry> = snapshot(now)
        .into_iter()
        .map(|(key, reading)| Entry { key, reading })
        .collect();
    let body = serde_json::to_vec(&entries)?;
    let tmp = dir.join(format!(".{SNAPSHOT_FILE}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut file = crate::forge_etag_store::create_private_file(&tmp)?;
    file.write_all(&body)?;
    std::fs::rename(&tmp, dir.join(SNAPSHOT_FILE))
}

/// The readings in `dir`'s [`SNAPSHOT_FILE`] still believed at `now`. A
/// missing or unreadable snapshot reads as none.
#[must_use]
pub fn load(dir: &Path, now: i64) -> Vec<(BucketKey, Reading)> {
    if !crate::forge_etag_store::private_dir(dir, false) {
        return Vec::new();
    }
    let Ok(raw) = std::fs::read_to_string(dir.join(SNAPSHOT_FILE)) else {
        return Vec::new();
    };
    let entries: Vec<Entry> = serde_json::from_str(&raw).unwrap_or_default();
    let mut out: Vec<(BucketKey, Reading)> = entries
        .into_iter()
        .filter(|e| e.reading.is_fresh(now))
        .map(|e| (e.key, e.reading))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// Which bucket a credential directory spends
// ---------------------------------------------------------------------------

/// What a `GH_CONFIG_DIR` path is, by its shape alone (no file is read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirClass {
    /// `<root>/.loom/gh-config-by-owner/<owner>/<app id>`: a reader App's
    /// token for `owner`.
    Reader { owner: String, app_id: String },
    /// `<root>/.loom/gh-config-by-owner/<owner>`: the writer App's token for
    /// a non-primary owner.
    OwnerWriter { root: PathBuf, owner: String },
    /// `<root>/.loom/gh-config`: the writer App's token for the workspace's
    /// own owner.
    PrimaryWriter { root: PathBuf },
    /// Anything else: an operator's own `gh` login, a gateway profile, …
    Other,
}

/// Whether `owner` is a plausible GitHub owner name (GitHub's login rule).
#[must_use]
pub fn valid_owner(owner: &str) -> bool {
    (1..=39).contains(&owner.len())
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Classify a credential directory by its path shape.
#[must_use]
pub fn classify_dir(dir: &Path) -> DirClass {
    let parts: Vec<&str> = dir
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let tail = |n: usize| parts.len().checked_sub(n).map(|i| &parts[i..]);
    let root_minus = |n: usize| {
        let mut root = dir.to_path_buf();
        for _ in 0..n {
            root.pop();
        }
        root
    };
    if let Some([".loom", "gh-config-by-owner", owner, app]) = tail(4) {
        if valid_owner(owner) && !app.is_empty() && app.bytes().all(|b| b.is_ascii_digit()) {
            return DirClass::Reader {
                owner: owner.to_ascii_lowercase(),
                app_id: (*app).to_string(),
            };
        }
    }
    if let Some([".loom", "gh-config-by-owner", owner]) = tail(3) {
        if valid_owner(owner) {
            return DirClass::OwnerWriter {
                root: root_minus(3),
                owner: owner.to_ascii_lowercase(),
            };
        }
    }
    if let Some([".loom", "gh-config"]) = tail(2) {
        return DirClass::PrimaryWriter {
            root: root_minus(2),
        };
    }
    DirClass::Other
}

/// The writer App's account label for the workspace at `root`:
/// `app-<id>` from the identity roster, else `app-unknown`.
#[must_use]
pub fn writer_account(root: &Path) -> String {
    crate::forge_identity::cached(root)
        .writer
        .map(|w| crate::observability::ops::ratelimit::app_account_label(&w.app_id))
        .filter(|label| label != "unknown")
        .unwrap_or_else(|| "app-unknown".to_string())
}

/// The workspace at `root`'s own owner (its `origin` remote's owner,
/// lowercased), from a memoised local lookup — never a forge call.
#[must_use]
pub fn primary_owner(root: &Path) -> Option<String> {
    let nwo = crate::gh_invocation::accounting::remote_repo(root)?;
    let owner = crate::credential_preflight::owner_of_nwo(&nwo);
    valid_owner(owner).then(|| owner.to_ascii_lowercase())
}

#[cfg(test)]
mod tests;
