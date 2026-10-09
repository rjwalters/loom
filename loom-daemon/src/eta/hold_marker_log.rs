//! The point-in-time log of Champion hold and release markers (#10958,
//! Slice 1): which kind of hold a held PR is under, read from the PR's
//! comments, so a later hold-release model can tell a merge-risk hold from a
//! critical-file hold from a bare operator label.
//!
//! # What is logged
//!
//! One [`HoldMarker`] per trusted `<!-- champion:<kind> … -->` hold or release
//! marker ([`MarkerKind`]):
//! `{repo, pr, thread, kind, head, comment_id, created_at, fetched_at, source}`.
//! Other Champion markers (digests, notices, dependency defers) are not hold
//! state and are not logged. A marker counts only when
//! [`crate::comment_trust`] trusts its author, on a line of its own (leading
//! whitespace aside) and outside a fenced code block, so a comment quoting a
//! marker in prose or in a code sample logs nothing.
//!
//! - `pr` is the PR the marker is about: the thread it was posted on, except
//!   for `ac-hold`, which Champion posts on the linked **issue** and which
//!   names its PR (`pr=<n>`). A hold or release marker on an issue thread is
//!   ignored.
//! - `head` is the commit the marker names: `hold-release-respected:<sha>`,
//!   `ac-hold … sha=<sha>`, or else the comment's
//!   `<!-- champion:hold-state head=<sha> -->` line.
//!
//! # Knowable-at
//!
//! A comment's `created_at` is an immutable forge timestamp, so a marker is
//! knowable from `created_at` (consumers apply their own lag, as with the raw
//! event cache). The residual leak: an edit or a delete after `as_of` is
//! invisible, because a row is never rewritten. `source` records whether the
//! row came from the repo's first walk (`backfill`) or a later one (`live`),
//! so a fit can run an ablation that drops backfilled rows.
//!
//! # Reads
//!
//! Forge reads only: one repo-wide, ETag'd conditional GET of
//! `issues/comments?since=…&sort=updated&direction=asc` per call, through the
//! reader Apps ([`super::pr_features_forge::fetch_comments_page`]), at most
//! [`MARKER_READ_BUDGET`] calls per ETA pass across every repo (zero while
//! the rate-limit breaker suppresses polling). A repo's first walk starts
//! [`BACKFILL_DAYS`] back. After a full page the walk advances `since` to the
//! newest `updated_at` it saw (or, when a whole page shares one instant, to
//! the next page), so it never pages deep; a short page means the repo is
//! caught up, and its cursor is stamped `caught_up_at`. A quiet repo then
//! costs one `304` per pass. Rows are keyed by `(repo, comment_id, kind)`, so
//! the overlap a walk re-reads appends nothing.
//!
//! # Coverage
//!
//! [`MarkerCursor`] carries each repo's coverage: complete from
//! `backfill_from` through `caught_up_at`. A reader that needs "no marker"
//! to mean "none was posted" checks [`RepoCursor::coverage`] first
//! ([`super::hold_kind`]); an uncovered instant is unknown, never "no marker".
//!
//! # Persistence
//!
//! `pr-hold-markers.jsonl` beside the fleet snapshots
//! ([`super::fleet::snapshot_dir`]) and its cursor `pr-hold-markers.cursor`;
//! neither extension is `*.json`, so [`super::fleet::load_all`]'s snapshot
//! listing never sees them. The read path is addressed by repo slug, never by
//! a per-repo checkout.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Schema tag carried by every [`HoldMarker`] row.
pub const MARKER_SCHEMA: &str = "eta-hold-marker/v1";

/// Schema tag of the cursor file.
pub const CURSOR_SCHEMA: &str = "eta-hold-marker-cursor/v1";

/// Most forge calls the marker reads of one ETA pass make, across repos.
pub const MARKER_READ_BUDGET: usize = 6;

/// How far back a repo's first walk starts: 14 days of fit rows plus a
/// 14-day trailing lookback for the earliest row's features.
pub const BACKFILL_DAYS: i64 = 28;

/// Rows per page: the REST maximum.
pub const PER_PAGE: usize = 100;

/// Markers older than this are dropped when the log is compacted.
pub const RETAIN_DAYS: i64 = 120;

/// Compact once the log holds more than this many rows.
const COMPACT_ABOVE: usize = 20_000;

/// A Champion hold or release marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    /// `merge-risk-hold`: criterion #2's sticky hold opened.
    MergeRiskHold,
    /// `critical-file-hold`: criterion #3's durable hold opened.
    CriticalFileHold,
    /// `ac-hold pr=<n> sha=<sha>`: the linked issue's close is held.
    AcHold,
    /// `merge-risk-hold-cleared`: Champion closed a merge-risk episode.
    MergeRiskHoldCleared,
    /// `critical-file-hold-cleared`: Champion closed a critical-file episode.
    CriticalFileHoldCleared,
    /// `critical-file-release-respected`: Champion acknowledged a human release.
    CriticalFileReleaseRespected,
    /// `hold-release-respected:<sha>`: Champion acknowledged a human release.
    HoldReleaseRespected,
}

impl MarkerKind {
    /// Every kind.
    pub const ALL: [MarkerKind; 7] = [
        MarkerKind::MergeRiskHold,
        MarkerKind::CriticalFileHold,
        MarkerKind::AcHold,
        MarkerKind::MergeRiskHoldCleared,
        MarkerKind::CriticalFileHoldCleared,
        MarkerKind::CriticalFileReleaseRespected,
        MarkerKind::HoldReleaseRespected,
    ];

    /// The marker's name, as written after `<!-- champion:`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            MarkerKind::MergeRiskHold => "merge-risk-hold",
            MarkerKind::CriticalFileHold => "critical-file-hold",
            MarkerKind::AcHold => "ac-hold",
            MarkerKind::MergeRiskHoldCleared => "merge-risk-hold-cleared",
            MarkerKind::CriticalFileHoldCleared => "critical-file-hold-cleared",
            MarkerKind::CriticalFileReleaseRespected => "critical-file-release-respected",
            MarkerKind::HoldReleaseRespected => "hold-release-respected",
        }
    }

    /// The kind named `name` exactly (never a prefix match: `merge-risk-hold`
    /// is not `merge-risk-hold-digest`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Whether the marker opens (or re-arms) a hold.
    #[must_use]
    pub fn is_hold(self) -> bool {
        matches!(
            self,
            MarkerKind::MergeRiskHold | MarkerKind::CriticalFileHold | MarkerKind::AcHold
        )
    }

    /// Whether Champion itself closed the episode (`-cleared`), as opposed to
    /// acknowledging a human's release (`-respected`).
    #[must_use]
    pub fn is_champion_close(self) -> bool {
        matches!(self, MarkerKind::MergeRiskHoldCleared | MarkerKind::CriticalFileHoldCleared)
    }
}

/// Which walk read a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerSource {
    /// The repo's first walk, reaching back [`BACKFILL_DAYS`].
    Backfill,
    /// A later walk.
    Live,
}

/// One logged marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldMarker {
    /// Always [`MARKER_SCHEMA`].
    pub schema: String,
    /// `owner/repo`, lowercased.
    pub repo: String,
    /// The PR the marker is about.
    pub pr: u32,
    /// The issue or PR the comment was posted on.
    pub thread: u32,
    pub kind: MarkerKind,
    /// The head commit the marker names, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The forge comment id.
    pub comment_id: u64,
    /// When the comment was posted: the knowable-at instant.
    pub created_at: DateTime<Utc>,
    /// When this host read the page.
    pub fetched_at: DateTime<Utc>,
    pub source: MarkerSource,
}

impl HoldMarker {
    /// The dedupe key: one row per marker kind per comment.
    #[must_use]
    pub fn key(&self) -> (String, u64, MarkerKind) {
        (self.repo.to_ascii_lowercase(), self.comment_id, self.kind)
    }
}

/// One marker parsed from a comment body, before it is bound to a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMarker {
    pub kind: MarkerKind,
    /// The commit the marker (or the comment's `hold-state` line) names.
    pub head: Option<String>,
    /// The `pr=<n>` attribute (`ac-hold`).
    pub pr: Option<u32>,
}

/// The `<!-- champion:<name><rest> -->` on `line`, as `(name, rest)`, when the
/// line (trimmed) is exactly one such marker.
fn marker_line(line: &str) -> Option<(&str, &str)> {
    let inner = line
        .trim()
        .strip_prefix("<!-- champion:")?
        .strip_suffix("-->")?;
    let end = inner
        .find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
        .unwrap_or(inner.len());
    let (name, rest) = inner.split_at(end);
    Some((name, rest.trim()))
}

/// The value of attribute `key=` in a marker's `rest`.
fn attr<'a>(rest: &'a str, key: &str) -> Option<&'a str> {
    rest.split_whitespace()
        .find_map(|tok| tok.strip_prefix(key)?.strip_prefix('='))
        .filter(|v| !v.is_empty())
}

/// A commit-looking token: hex only, so prose never becomes a head.
fn sha(token: &str) -> Option<String> {
    let t = token.trim();
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit())).then(|| t.to_ascii_lowercase())
}

/// The hold and release markers in one comment body, each kind once (first
/// occurrence). Fenced code blocks are skipped. A marker with no head of its
/// own takes the comment's `hold-state head=<sha>` line, when it has one.
#[must_use]
pub fn parse_body(body: &str) -> Vec<ParsedMarker> {
    let mut fenced = false;
    let mut state_head: Option<String> = None;
    let mut out: Vec<ParsedMarker> = Vec::new();
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let Some((name, rest)) = marker_line(line) else {
            continue;
        };
        if name == "hold-state" {
            state_head = state_head.or_else(|| attr(rest, "head").and_then(sha));
            continue;
        }
        let Some(kind) = MarkerKind::from_name(name) else {
            continue;
        };
        if out.iter().any(|m| m.kind == kind) {
            continue;
        }
        let head = match kind {
            MarkerKind::HoldReleaseRespected => rest.strip_prefix(':').and_then(sha),
            MarkerKind::AcHold => attr(rest, "sha").and_then(sha),
            _ => None,
        };
        let pr = attr(rest, "pr").and_then(|v| v.parse().ok());
        if kind == MarkerKind::AcHold && pr.is_none() {
            continue;
        }
        out.push(ParsedMarker { kind, head, pr });
    }
    for m in &mut out {
        if m.head.is_none() {
            m.head.clone_from(&state_head);
        }
    }
    out
}

/// One parsed comment-listing page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    /// The trusted markers on the page.
    pub markers: Vec<HoldMarker>,
    /// How many comments the page held (the walk is caught up under
    /// [`PER_PAGE`]).
    pub rows: usize,
    /// The newest `updated_at` on the page.
    pub max_updated: Option<DateTime<Utc>>,
}

fn time(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key)?.as_str()?.parse().ok()
}

/// The issue or PR number a REST comment was posted on (its `issue_url`'s
/// last segment), and whether that thread is a PR (its `html_url` names
/// `/pull/`).
fn thread_of(v: &Value) -> Option<(u32, bool)> {
    let n = v
        .get("issue_url")?
        .as_str()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()?;
    let is_pr = v
        .get("html_url")
        .and_then(Value::as_str)
        .is_some_and(|u| u.contains("/pull/"));
    Some((n, is_pr))
}

/// Parse one page of the repo-wide comment listing into the markers of the
/// comments `trusts` believes, read at `fetched_at` by a `source` walk.
/// `None` when the body is not a comment array (a failed read, retried).
#[must_use]
pub fn parse_page(
    repo: &str,
    body: &Value,
    trusts: &dyn Fn(&Value) -> bool,
    fetched_at: DateTime<Utc>,
    source: MarkerSource,
) -> Option<Page> {
    let rows = body.as_array()?;
    let mut page = Page {
        rows: rows.len(),
        ..Page::default()
    };
    for row in rows {
        let updated = time(row, "updated_at");
        page.max_updated = page.max_updated.max(updated);
        let Some(text) = row.get("body").and_then(Value::as_str) else {
            continue;
        };
        if !text.contains("<!-- champion:") || !trusts(row) {
            continue;
        }
        let (Some((thread, is_pr)), Some(id), Some(created_at)) =
            (thread_of(row), row.get("id").and_then(Value::as_u64), time(row, "created_at"))
        else {
            continue;
        };
        for m in parse_body(text) {
            let pr = match (m.kind, is_pr) {
                (MarkerKind::AcHold, _) => m.pr,
                (_, true) => Some(thread),
                (_, false) => None,
            };
            let Some(pr) = pr else { continue };
            page.markers.push(HoldMarker {
                schema: MARKER_SCHEMA.to_string(),
                repo: repo.to_ascii_lowercase(),
                pr,
                thread,
                kind: m.kind,
                head: m.head,
                comment_id: id,
                created_at,
                fetched_at,
                source,
            });
        }
    }
    Some(page)
}

/// The log's path under `workspace_root`.
#[must_use]
pub fn log_path(workspace_root: &Path) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root).join("pr-hold-markers.jsonl")
}

/// The cursor's path under `workspace_root`.
#[must_use]
pub fn cursor_path(workspace_root: &Path) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root).join("pr-hold-markers.cursor")
}

/// Every row in the log, in file order. Absent or unreadable is empty; a
/// line that does not parse (or carries another schema) is skipped.
#[must_use]
pub fn load(workspace_root: &Path) -> Vec<HoldMarker> {
    let Ok(text) = std::fs::read_to_string(log_path(workspace_root)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<HoldMarker>(l).ok())
        .filter(|m| m.schema == MARKER_SCHEMA)
        .collect()
}

/// `repo`'s rows in `log`, in `(created_at, comment_id, kind)` order.
#[must_use]
pub fn for_repo(log: &[HoldMarker], repo: &str) -> Vec<HoldMarker> {
    let mut rows: Vec<HoldMarker> = log
        .iter()
        .filter(|m| m.repo.eq_ignore_ascii_case(repo))
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        (a.created_at, a.comment_id, a.kind).cmp(&(b.created_at, b.comment_id, b.kind))
    });
    rows
}

/// Append `rows` to the log.
///
/// # Errors
///
/// The directory could not be created or the write failed.
pub fn append(workspace_root: &Path, rows: &[HoldMarker]) -> std::io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let path = log_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(out.as_bytes())
}

/// Rewrite the log without rows older than [`RETAIN_DAYS`] before `now`, once
/// it is large. Atomic (temp file and rename).
///
/// # Errors
///
/// The rewrite failed.
pub fn compact(workspace_root: &Path, now: DateTime<Utc>) -> std::io::Result<()> {
    let all = load(workspace_root);
    if all.len() <= COMPACT_ABOVE {
        return Ok(());
    }
    let from = now - Duration::days(RETAIN_DAYS);
    let mut out = String::new();
    for r in all.iter().filter(|r| r.created_at >= from) {
        out.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    let path = log_path(workspace_root);
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

/// One repo's walk state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCursor {
    /// The listing's `since` for the next call.
    pub since: DateTime<Utc>,
    /// The page of that listing to read next.
    pub page: u32,
    /// Where the first walk started: the log is complete from here.
    pub backfill_from: DateTime<Utc>,
    /// The last time a walk reached a short page: the log is complete through
    /// here. `None` until the first walk finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caught_up_at: Option<DateTime<Utc>>,
    /// The last time a call for this repo returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read: Option<DateTime<Utc>>,
}

/// The span of `created_at` instants a repo's log is complete over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Complete from here (`None`: never walked).
    pub from: Option<DateTime<Utc>>,
    /// Complete through here (`None`: the first walk has not finished).
    pub through: Option<DateTime<Utc>>,
}

impl Coverage {
    /// Whether every marker posted in `[a, b)` is in the log.
    #[must_use]
    pub fn covers(self, a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
        self.from.is_some_and(|f| f <= a) && self.through.is_some_and(|t| b <= t)
    }
}

impl RepoCursor {
    /// A first walk starting [`BACKFILL_DAYS`] before `now`.
    #[must_use]
    pub fn start(now: DateTime<Utc>) -> Self {
        let from = now - Duration::days(BACKFILL_DAYS);
        Self {
            since: from,
            page: 1,
            backfill_from: from,
            caught_up_at: None,
            last_read: None,
        }
    }

    /// The log's coverage for this repo.
    #[must_use]
    pub fn coverage(&self) -> Coverage {
        Coverage {
            from: Some(self.backfill_from),
            through: self.caught_up_at,
        }
    }

    /// The listing URL of the next call.
    #[must_use]
    pub fn url(&self, repo: &str) -> String {
        format!(
            "repos/{}/issues/comments?since={}&sort=updated&direction=asc&per_page={PER_PAGE}&page={}",
            repo.to_ascii_lowercase(),
            self.since.to_rfc3339_opts(SecondsFormat::Secs, true),
            self.page
        )
    }

    /// Advance past a page read at `at`. Returns whether the repo is caught
    /// up (the page was short).
    pub fn advance(&mut self, page: &Page, at: DateTime<Utc>) -> bool {
        self.last_read = Some(at);
        if page.rows >= PER_PAGE {
            match page.max_updated {
                Some(m) if m > self.since => {
                    self.since = m;
                    self.page = 1;
                }
                _ => self.page += 1,
            }
            return false;
        }
        if let Some(m) = page.max_updated {
            self.since = self.since.max(m);
        }
        self.page = 1;
        self.caught_up_at = Some(at);
        true
    }
}

/// Every repo's walk state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerCursor {
    /// Always [`CURSOR_SCHEMA`].
    pub schema: String,
    /// By lowercased `owner/repo`.
    pub repos: BTreeMap<String, RepoCursor>,
}

impl Default for MarkerCursor {
    fn default() -> Self {
        Self {
            schema: CURSOR_SCHEMA.to_string(),
            repos: BTreeMap::new(),
        }
    }
}

impl MarkerCursor {
    /// The cursor under `workspace_root`; absent, unreadable or another
    /// schema is empty (every repo walks again, and the dedupe keeps the log
    /// unchanged).
    #[must_use]
    pub fn read(workspace_root: &Path) -> Self {
        std::fs::read_to_string(cursor_path(workspace_root))
            .ok()
            .and_then(|t| serde_json::from_str::<Self>(&t).ok())
            .filter(|c| c.schema == CURSOR_SCHEMA)
            .unwrap_or_default()
    }

    /// Write the cursor atomically.
    ///
    /// # Errors
    ///
    /// The write failed.
    pub fn write(&self, workspace_root: &Path) -> std::io::Result<()> {
        let path = cursor_path(workspace_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("cursor.tmp");
        std::fs::write(&tmp, serde_json::to_string(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// `repo`'s coverage (empty when it was never walked).
    #[must_use]
    pub fn coverage(&self, repo: &str) -> Coverage {
        self.repos
            .get(&repo.to_ascii_lowercase())
            .map(RepoCursor::coverage)
            .unwrap_or_default()
    }
}

/// Run one pass over `repos`: read through `fetch` (the forge read of a
/// listing URL for a repo, `None` when it failed) at most `budget` calls,
/// round-robin from the repo never read (then the stalest), each repo
/// until its walk is caught up or a read fails. `trusts` decides a
/// comment's author for its repo; `now` stamps each read's return. Returns
/// the rows to append (none already in `log`); the caller writes them
/// **before** the cursor, so a crash re-reads and appends nothing.
pub fn refresh(
    repos: &[String],
    log: &[HoldMarker],
    cursor: &mut MarkerCursor,
    budget: usize,
    now: impl Fn() -> DateTime<Utc>,
    mut fetch: impl FnMut(&str, &str) -> Option<Value>,
    trusts: impl Fn(&str, &Value) -> bool,
) -> Vec<HoldMarker> {
    let mut seen: BTreeSet<(String, u64, MarkerKind)> = log.iter().map(HoldMarker::key).collect();
    let mut order: Vec<String> = repos
        .iter()
        .map(|r| r.to_ascii_lowercase())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    order.sort_by_key(|r| cursor.repos.get(r).and_then(|c| c.last_read));
    let mut out = Vec::new();
    let mut calls = 0;
    while calls < budget && !order.is_empty() {
        let mut still = Vec::new();
        for repo in order {
            if calls >= budget {
                break;
            }
            let state = cursor
                .repos
                .entry(repo.clone())
                .or_insert_with(|| RepoCursor::start(now()));
            let source = if state.caught_up_at.is_some() {
                MarkerSource::Live
            } else {
                MarkerSource::Backfill
            };
            calls += 1;
            let body = fetch(&repo, &state.url(&repo));
            let at = now();
            let trust = |v: &Value| trusts(&repo, v);
            let Some(page) = body.and_then(|b| parse_page(&repo, &b, &trust, at, source)) else {
                continue;
            };
            for m in &page.markers {
                if seen.insert(m.key()) {
                    out.push(m.clone());
                }
            }
            if !state.advance(&page, at) {
                still.push(repo);
            }
        }
        order = still;
    }
    out
}
