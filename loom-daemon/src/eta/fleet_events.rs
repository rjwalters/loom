//! The raw, resumable fleet event cache (#10197, PR 1 of 3).
//!
//! # Why raw events, next to [`super::fleet`]'s samples
//!
//! [`super::fleet::FleetSnapshot`] stores *derived* per-stage durations. That
//! answers "how long does `review_wait` take", but not "what did the whole
//! fleet look like at instant `t`" — which open items there were, in which
//! stage, how many were held — and re-deriving anything new from it would need
//! a refetch. This module keeps the **raw** forge history instead: every label
//! added or removed, every open / close / reopen / merge, as one row each, so
//! [`super::fleet_state::fleet_state`] (and any later reconstruction) is a pure
//! function of what is already on disk.
//!
//! # Files
//!
//! Under the same `.loom/state/eta/fleet/` directory as the snapshots
//! ([`super::fleet::snapshot_dir`], `LOOM_ETA_FLEET_SNAPSHOT_DIR` overrides):
//!
//! - `events-<slug>.jsonl` — append-only, one [`RawEvent`] per line. A row is
//!   appended only when its content-derived [`RawEvent::id`] is not already in
//!   the file, so re-reading a page (a resumed run, an overlapping refresh)
//!   adds nothing. Readers sort into the canonical order ([`load_events`]);
//!   the file order is fetch order and carries no meaning.
//! - `events-<slug>.cursor.json` — the per-endpoint resume state
//!   ([`EventsCursor`]): the next backfill page, the refresh ETag, and an
//!   in-progress refresh's next page. Written atomically **after** the page's
//!   rows are appended, so a kill between the two re-reads that page on the
//!   next run and appends nothing.
//!
//! [`super::fleet::load_all`] lists `*.json` in the same directory, so it
//! sees the cursor file — and refuses it, because its schema is not the
//! snapshot schema. The events file is `.jsonl` and is never listed.
//!
//! # Fetch time is kept apart from event time
//!
//! Every row records `event_time` (when it happened on the forge — for a forge
//! row also the instant it became knowable) and `fetched_at` (when this host
//! read the page). `fetched_at` is excluded from the id, so the same forge
//! event read twice is one row, and the earliest read wins.
//!
//! # Deterministic order
//!
//! Same-second events are common (a sweep flips two labels in one call). The
//! canonical order is `(event_time, source, seq, id)`: `seq` is the source's own
//! monotonic sequence — the forge event id for forge rows, the delivery order
//! for webhook-mirror rows (PR 2) — so replays never depend on the order a
//! page happened to list rows in.
//!
//! # Sources
//!
//! The sync driver ([`sync`]) is source-agnostic: it pages a
//! [`RawEventSource`]. The forge sources are the repo-wide issue-events
//! listing ([`super::fleet_events_forge`]), the pulls listing (open, merge
//! and close times, closing references and head commits,
//! [`super::fleet_events_pulls`]) and two per-PR listings — formal reviews
//! and the head commit's check runs ([`super::fleet_events_reviews`]) —
//! walked one PR at a time by [`super::fleet_events_fanout`]. The
//! webhook-mirror importer is still to come.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Schema tag carried by every [`RawEvent`] row.
pub const EVENT_SCHEMA: &str = "eta-fleet-event/v1";

/// Schema tag of the cursor file.
pub const CURSOR_SCHEMA: &str = "eta-fleet-events-cursor/v1";

/// `source` of a row read from the forge's REST API.
pub const SOURCE_FORGE: &str = "forge";

/// Whether a row's item is an issue or a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    Pr,
}

impl ItemKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Issue => "issue",
            ItemKind::Pr => "pr",
        }
    }
}

/// What happened to the item.
///
/// `ClosingRef` and `HeadCommit` come from the pulls listing
/// ([`super::fleet_events_pulls`]); `Review` and `CheckRun` from the per-PR
/// listings ([`super::fleet_events_reviews`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Opened,
    LabelAdded,
    LabelRemoved,
    Closed,
    Reopened,
    Merged,
    Review,
    CheckRun,
    /// A PR's body links an issue ([`RawEvent::target`]) with the open-PR
    /// guard's phrase set — `label` is `"closes"` or `"part_of"` — or, with no
    /// target and no label, links none. Either way it records that the PR's
    /// linkage references were read.
    ClosingRef,
    /// The PR's head commit (`label` = the SHA) as the pulls listing showed
    /// it, stamped at the PR's `updated_at`: a push bumps `updated_at`, so the
    /// head was this commit by then. The work list of the check-run fetcher,
    /// and the head [`super::fleet_state_prs`] scopes a PR's CI to.
    HeadCommit,
}

impl EventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Opened => "opened",
            EventKind::LabelAdded => "label_added",
            EventKind::LabelRemoved => "label_removed",
            EventKind::Closed => "closed",
            EventKind::Reopened => "reopened",
            EventKind::Merged => "merged",
            EventKind::Review => "review",
            EventKind::CheckRun => "check_run",
            EventKind::ClosingRef => "closing_ref",
            EventKind::HeadCommit => "head_commit",
        }
    }
}

/// One raw forge fact about one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEvent {
    /// Always [`EVENT_SCHEMA`].
    pub schema: String,
    /// Content-derived, never random: a digest of every field except
    /// `fetched_at` and `schema`.
    pub id: String,
    /// `owner/repo`.
    pub repo: String,
    /// Issue or PR number.
    pub item: u32,
    pub item_kind: ItemKind,
    pub kind: EventKind,
    /// The label, for `label_added` / `label_removed`; the phrase family
    /// (`closes` / `part_of`) for a targeted `closing_ref`; the SHA for
    /// `head_commit`; the review state for `review` and `<state>:<check name>`
    /// for `check_run` (absent on a per-PR settle marker,
    /// [`super::fleet_events_fanout`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The issue a `closing_ref` row says the PR links. Absent on every
    /// other row, and then not part of the id, so rows written before the
    /// field existed keep their ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u32>,
    /// The commit a `check_run` row's run ran on (its `head_sha`), so replay
    /// can scope CI to the PR's head ([`super::fleet_state_prs`]). Absent on
    /// every other row, and then not part of the id, like `target`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// When it happened on the forge — the knowable-at instant.
    pub event_time: DateTime<Utc>,
    /// Where the row came from ([`SOURCE_FORGE`], later `webhook-mirror`).
    pub source: String,
    /// The source's own monotonic order (forge event id; webhook delivery
    /// order). `0` for a row synthesised from an item's own fields (`opened`).
    pub seq: u64,
    /// When this host read the page the row came from. Never part of the id.
    pub fetched_at: DateTime<Utc>,
}

impl RawEvent {
    /// A row with its id derived from its content.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        repo: &str,
        item: u32,
        item_kind: ItemKind,
        kind: EventKind,
        label: Option<String>,
        event_time: DateTime<Utc>,
        source: &str,
        seq: u64,
        fetched_at: DateTime<Utc>,
    ) -> Self {
        let mut event = RawEvent {
            schema: EVENT_SCHEMA.to_string(),
            id: String::new(),
            repo: repo.to_string(),
            item,
            item_kind,
            kind,
            label,
            target: None,
            commit: None,
            event_time,
            source: source.to_string(),
            seq,
            fetched_at,
        };
        event.id = event.derive_id();
        event
    }

    /// The same row naming `target` (a `closing_ref`'s issue), id re-derived.
    #[must_use]
    pub fn with_target(mut self, target: Option<u32>) -> Self {
        self.target = target;
        self.id = self.derive_id();
        self
    }

    /// The same row naming the `commit` its run ran on, id re-derived.
    #[must_use]
    pub fn with_commit(mut self, commit: Option<String>) -> Self {
        self.commit = commit;
        self.id = self.derive_id();
        self
    }

    fn derive_id(&self) -> String {
        let repo = self.repo.to_ascii_lowercase();
        let item = self.item.to_string();
        let seq = self.seq.to_string();
        let at = crate::telemetry::trace::instant(self.event_time);
        let target = self.target.map(|t| format!("target={t}"));
        let commit = self.commit.as_ref().map(|c| format!("commit={c}"));
        let mut parts = vec![
            "loom.eta.fleet.event",
            repo.as_str(),
            self.source.as_str(),
            item.as_str(),
            self.item_kind.as_str(),
            self.kind.as_str(),
            self.label.as_deref().unwrap_or(""),
            at.as_str(),
            seq.as_str(),
        ];
        if let Some(target) = &target {
            parts.push(target.as_str());
        }
        if let Some(commit) = &commit {
            parts.push(commit.as_str());
        }
        crate::telemetry::trace::derived_hex(&parts, 16)
    }

    /// The canonical sort key: `(event_time, source, seq, id)`.
    #[must_use]
    pub fn canonical_key(&self) -> (DateTime<Utc>, &str, u64, &str) {
        (self.event_time, self.source.as_str(), self.seq, self.id.as_str())
    }
}

/// Sort into the canonical order and drop repeated ids (first occurrence in
/// file order wins, so the earliest `fetched_at` is kept).
pub fn canonicalize(events: &mut Vec<RawEvent>) {
    let mut seen = HashSet::new();
    events.retain(|e| seen.insert(e.id.clone()));
    events.sort_by(|a, b| a.canonical_key().cmp(&b.canonical_key()));
}

/// The events file for `repo` under `workspace_root`.
#[must_use]
pub fn events_path(workspace_root: &Path, repo: &str) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root)
        .join(format!("events-{}.jsonl", super::fleet::snapshot_slug(repo)))
}

/// The cursor file for `repo` under `workspace_root`.
#[must_use]
pub fn cursor_path(workspace_root: &Path, repo: &str) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root)
        .join(format!("events-{}.cursor.json", super::fleet::snapshot_slug(repo)))
}

/// Every row in the events file at `path`, in canonical order. A missing file
/// is an empty log; a torn trailing line (a kill mid-append) and rows of an
/// unknown schema are skipped, never half-parsed.
#[must_use]
pub fn load_events(path: &Path) -> Vec<RawEvent> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut events: Vec<RawEvent> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<RawEvent>(line).ok())
        .filter(|e| e.schema == EVENT_SCHEMA)
        .collect();
    canonicalize(&mut events);
    events
}

/// The append-only events file, with the ids already in it.
pub struct EventLog {
    path: PathBuf,
    known: HashSet<String>,
}

impl EventLog {
    /// Open (without creating) the log at `path`. A torn trailing line left by
    /// a kill mid-append is truncated away, so the next append starts on a
    /// line boundary and the file stays byte-identical to an uninterrupted
    /// run's.
    ///
    /// # Errors
    ///
    /// The existing file could not be read or truncated.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let mut known = HashSet::new();
        match std::fs::read(path) {
            Ok(bytes) => {
                let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                if keep < bytes.len() {
                    let file = std::fs::OpenOptions::new().write(true).open(path)?;
                    file.set_len(keep as u64)?;
                }
                for line in String::from_utf8_lossy(&bytes[..keep]).lines() {
                    if let Ok(event) = serde_json::from_str::<RawEvent>(line) {
                        known.insert(event.id);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(EventLog {
            path: path.to_path_buf(),
            known,
        })
    }

    /// Whether a row with this id is already in the log.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.known.contains(id)
    }

    /// Rows held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.known.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// Append every row of `events` not already held, in canonical order, in
    /// one write. Returns how many were appended.
    ///
    /// # Errors
    ///
    /// The directory could not be created or the write failed.
    pub fn append(&mut self, events: &[RawEvent]) -> std::io::Result<usize> {
        let mut fresh: Vec<RawEvent> = events
            .iter()
            .filter(|e| !self.known.contains(&e.id))
            .cloned()
            .collect();
        canonicalize(&mut fresh);
        if fresh.is_empty() {
            return Ok(0);
        }
        let mut buf = String::new();
        for event in &fresh {
            buf.push_str(&serde_json::to_string(event).map_err(std::io::Error::other)?);
            buf.push('\n');
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(buf.as_bytes())?;
        file.sync_data()?;
        for event in fresh.iter() {
            self.known.insert(event.id.clone());
        }
        Ok(fresh.len())
    }
}

/// Resume state of one paged endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCursor {
    /// The next page a backfill reads (1-based). Starts at 1.
    pub backfill_next_page: u32,
    /// The backfill walked off the end of the listing.
    pub backfill_complete: bool,
    /// Validator of page 1 as last fully ingested: a refresh sends it, and a
    /// `304` means nothing new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_etag: Option<String>,
    /// A refresh interrupted mid-walk resumes at this page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_next_page: Option<u32>,
    /// Page 1's validator from the interrupted refresh, promoted to
    /// `head_etag` only once that refresh reaches already-cached rows — so a
    /// `304` can never hide a page the refresh did not finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_head_etag: Option<String>,
    /// Pages read from this endpoint, ever (`200`s and `304`s) — the
    /// forge-call ledger for this cache.
    pub pages_fetched: u64,
    /// A per-PR walk ([`super::fleet_events_fanout`]) that settles the PR
    /// closed at this instant: once complete, only the marker is left to
    /// write, so a kill before it re-reads nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_for: Option<DateTime<Utc>>,
    /// On a per-PR endpoint's ledger entry: the PR whose walk last completed.
    /// The next run starts at the PR after it (wrapping), so bounded runs
    /// visit every pending PR ([`super::fleet_events_fanout`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_after: Option<u32>,
}

/// The cursor file: per-endpoint resume state for one repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsCursor {
    /// Always [`CURSOR_SCHEMA`].
    pub schema: String,
    /// `owner/repo`.
    pub repo: String,
    /// Keyed by `<source>:<endpoint>`.
    pub endpoints: BTreeMap<String, EndpointCursor>,
}

impl EventsCursor {
    #[must_use]
    pub fn empty(repo: &str) -> Self {
        EventsCursor {
            schema: CURSOR_SCHEMA.to_string(),
            repo: repo.to_string(),
            endpoints: BTreeMap::new(),
        }
    }

    /// The cursor at `path`, or an empty one when absent or of another schema.
    #[must_use]
    pub fn read(path: &Path, repo: &str) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<EventsCursor>(&t).ok())
            .filter(|c| c.schema == CURSOR_SCHEMA)
            .unwrap_or_else(|| Self::empty(repo))
    }

    /// Write atomically (temp file + rename).
    ///
    /// # Errors
    ///
    /// The directory could not be created or the write/rename failed.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{text}\n"))?;
        std::fs::rename(&tmp, path)
    }
}

/// One page request's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageFetch {
    /// `304`: the page is unchanged since the ETag sent.
    NotModified,
    /// The page's rows.
    Page {
        events: Vec<RawEvent>,
        /// The page's validator, when the source has one.
        etag: Option<String>,
        /// No page follows this one.
        last: bool,
    },
    /// The source cannot answer now (rate limit, reserve floor, transport
    /// failure). The run stops cleanly; the cursor already holds every page
    /// completed, so re-running resumes here.
    Stopped(String),
}

/// A paged, newest-first source of raw events.
///
/// Newest-first is what makes a page index a safe resume point: new events
/// only push older rows to *later* pages, so resuming a backfill at page `n`
/// can re-read a row (deduplicated) but never skip one.
pub trait RawEventSource {
    /// The `<source>:<endpoint>` cursor key, e.g. `forge:issues-events`.
    fn cursor_key(&self) -> String;
    /// Fetch one 1-based page, conditional on `etag` when given.
    fn fetch_page(&mut self, page: u32, etag: Option<&str>) -> PageFetch;
    /// Whether the listing is newest-first. A refresh of a newest-first
    /// listing stops at the first page holding a cached row; any other
    /// listing (a PR's reviews, a commit's check runs) is walked to its end.
    fn newest_first(&self) -> bool {
        true
    }
}

/// Which walk [`sync`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Continue the historical walk to the end of the listing.
    Backfill,
    /// Read from the head until rows already cached are reached.
    Refresh,
}

/// How a [`sync`] run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The walk finished (end of listing, or caught up with the cache).
    Complete,
    /// `max_pages` reached; re-run to continue.
    PageBudget,
    /// The source stopped; re-run to resume.
    Stopped(String),
}

/// What a [`sync`] run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub outcome: SyncOutcome,
    /// Page requests made this run (including `304`s).
    pub pages: u64,
    /// Rows appended this run.
    pub appended: usize,
}

/// Page `source` into `log`, checkpointing `cursor` to `cursor_file` after
/// every page. Makes at most `max_pages` requests.
///
/// # Errors
///
/// Appending to the log or writing the cursor failed. Source failures are not
/// errors: they end the run with [`SyncOutcome::Stopped`].
pub fn sync(
    source: &mut dyn RawEventSource,
    log: &mut EventLog,
    cursor: &mut EventsCursor,
    cursor_file: &Path,
    mode: SyncMode,
    max_pages: u64,
) -> std::io::Result<SyncReport> {
    let key = source.cursor_key();
    let newest_first = source.newest_first();
    let mut report = SyncReport {
        outcome: SyncOutcome::Complete,
        pages: 0,
        appended: 0,
    };
    loop {
        let state = cursor.endpoints.entry(key.clone()).or_default();
        if state.backfill_next_page == 0 {
            state.backfill_next_page = 1;
        }
        if mode == SyncMode::Backfill && state.backfill_complete {
            return Ok(report);
        }
        if report.pages >= max_pages {
            report.outcome = SyncOutcome::PageBudget;
            return Ok(report);
        }
        let (page, etag) = match mode {
            SyncMode::Backfill => (state.backfill_next_page, None),
            SyncMode::Refresh => match state.refresh_next_page {
                Some(p) => (p, None),
                None => (1, state.head_etag.clone()),
            },
        };
        let answer = source.fetch_page(page, etag.as_deref());
        if !matches!(answer, PageFetch::Stopped(_)) {
            report.pages += 1;
            state.pages_fetched += 1;
        }
        match answer {
            PageFetch::Stopped(why) => {
                report.outcome = SyncOutcome::Stopped(why);
                cursor.write(cursor_file)?;
                return Ok(report);
            }
            PageFetch::NotModified => {
                // Only a refresh's page 1 is conditional; anything else
                // answering 304 has nothing new either.
                state.refresh_next_page = None;
                state.refresh_head_etag = None;
                cursor.write(cursor_file)?;
                return Ok(report);
            }
            PageFetch::Page { events, etag, last } => {
                let overlaps = events
                    .iter()
                    .any(|e| e.kind != EventKind::Opened && log.contains(&e.id));
                report.appended += log.append(&events)?;
                let state = cursor.endpoints.entry(key.clone()).or_default();
                let done = match mode {
                    SyncMode::Backfill => {
                        if page == 1 {
                            state.head_etag = etag;
                        }
                        state.backfill_next_page = page + 1;
                        if last {
                            state.backfill_complete = true;
                        }
                        state.backfill_complete
                    }
                    SyncMode::Refresh => {
                        if page == 1 {
                            state.refresh_head_etag = etag;
                        }
                        if (overlaps && newest_first) || last {
                            state.head_etag = state.refresh_head_etag.take();
                            state.refresh_next_page = None;
                            true
                        } else {
                            state.refresh_next_page = Some(page + 1);
                            false
                        }
                    }
                };
                cursor.write(cursor_file)?;
                if done {
                    return Ok(report);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "fleet_events_tests.rs"]
mod tests;
