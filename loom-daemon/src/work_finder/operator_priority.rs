//! `loom:operator-priority` ("starred") dispatch support (#9244, slice A).
//!
//! The operator stars an issue (directly, or through loom-ui's star) to say
//! "land this ASAP, ahead of everything else". Three mechanisms live here:
//!
//! 1. **The second listing.** A starred issue is a candidate even when it is
//!    not `loom:issue` yet — `loom:triage`, `loom:curated`, or no workflow
//!    label at all. [`merge_starred`] folds the ETag-cached
//!    `labels=loom:operator-priority` listing into the `loom:issue` rows,
//!    deduped by issue number. Dispatching such an issue is already supported:
//!    the registry's pre-flip classifier returns `NotYetApproved` for a
//!    non-`loom:issue` snapshot and the child sweep starts from Curator.
//! 2. **Starred-at.** Starred issues order among themselves by when they were
//!    starred. [`StarredAtCache`] reads the `labeled` timeline event for the
//!    label once per starred issue (never per tick, never for an unstarred
//!    issue) and forgets it when the star is removed. A missing value falls
//!    back to `createdAt` in [`super::ordering::candidate_cmp`].
//! 3. **The overflow slot.** [`OverflowSlot`] lets one starred issue that
//!    only the queue caps refused run as this host's single over-limit sweep.
//!    It never goes past disk or RAM headroom ([`CapTerms`]).
//!
//! This label is **not** `loom:operator` (the hold) and is not a hold of any
//! kind; nothing may prefix-match `loom:operator-` as a hold (see
//! `crate::pr_latency::hold_labels`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use super::{TickReport, WorkDispatcher, WorkItem, BUILDING_LABEL};

/// The operator-priority ("starred") label (#9244).
pub const OPERATOR_PRIORITY_LABEL: &str = "loom:operator-priority";

/// The in-progress Curator claim. A starred issue carrying it is being
/// curated right now, so the starred listing leaves it alone this tick.
pub const CURATING_LABEL: &str = "loom:curating";

/// Labels that route an issue through Champion rather than the Builder: an
/// epic, and the three proposal kinds. A star on one of these is not a
/// dispatch request, so the starred listing skips it.
pub const CHAMPION_PATH_LABELS: [&str; 4] =
    ["loom:epic", "loom:architect", "loom:hermit", "loom:auditor"];

/// How long an *unknown* starred-at (the timeline read failed or found no
/// event) is trusted before it is read again. A known starred-at is kept
/// until the star is removed.
pub const STARRED_AT_RETRY: Duration = Duration::from_secs(600);

impl WorkItem {
    /// True when the issue carries [`OPERATOR_PRIORITY_LABEL`] (#9244).
    #[must_use]
    pub fn is_operator_priority(&self) -> bool {
        self.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL)
    }

    /// Builder-style setter for the starred-at timestamp (#9244).
    #[must_use]
    pub fn with_operator_priority_at(mut self, at: Option<String>) -> Self {
        self.operator_priority_at = at;
        self
    }
}

/// Fold the starred listing into the `loom:issue` rows (#9244 §4).
///
/// A starred `loom:issue` appears in both listings; the `loom:issue` row wins
/// and the duplicate is dropped. A starred row that is already claimed
/// ([`BUILDING_LABEL`], or [`CURATING_LABEL`] while a Curator works it) is
/// dropped too: it is not ready work, and the `loom:issue` listing never
/// showed such rows either. Every other starred row is kept and goes through
/// the normal skip filters, so a starred issue carrying a park or skip label
/// is still never dispatched.
///
/// A starred epic or proposal ([`CHAMPION_PATH_LABELS`]) is dropped from the
/// starred rows too: the star does not make it build work. It keeps its
/// Champion path (a starred epic is curated, then taken first by Champion's
/// epic queue; #9244 slice B). The `loom:issue` rows are left as they are.
#[must_use]
pub fn merge_starred(mut ready: Vec<WorkItem>, starred: Vec<WorkItem>) -> Vec<WorkItem> {
    let listed: HashSet<u32> = ready.iter().map(|i| i.number).collect();
    ready.extend(starred.into_iter().filter(|i| {
        !listed.contains(&i.number)
            && !i
                .labels
                .iter()
                .any(|l| l == BUILDING_LABEL || l == CURATING_LABEL)
            && !i
                .labels
                .iter()
                .any(|l| CHAMPION_PATH_LABELS.contains(&l.as_str()))
    }));
    ready
}

/// Where a starred issue's starred-at comes from.
///
/// The production source reads the issue's forge timeline
/// ([`GhTimelineStarredAt`]); tests pass a fake. Slice C of #9244 adds the
/// loom-ui intent `requested_at` as a higher-precedence source by wrapping
/// this trait: a wrapper that answers from the intent marker first and falls
/// through to the timeline is the whole seam.
pub trait StarredAtSource {
    /// When `issue` was last labeled [`OPERATOR_PRIORITY_LABEL`], or `None`
    /// when the timeline carries no such event.
    ///
    /// # Errors
    ///
    /// Returns an error when the read itself failed. The cache treats that
    /// like `None` and retries after [`STARRED_AT_RETRY`].
    fn starred_at(&mut self, issue: u32) -> Result<Option<String>>;
}

#[derive(Debug, Clone)]
struct CachedAt {
    at: Option<String>,
    fetched: Instant,
}

/// Per-repo starred-at cache (#9244 §3).
///
/// Holds one entry per currently-starred issue. [`Self::resolve`] fetches
/// only for a starred issue with no entry (or an unknown one older than
/// [`STARRED_AT_RETRY`]) and drops the entry of every issue that is no longer
/// starred, so un-starring and re-starring reads the new event.
#[derive(Debug, Default)]
pub struct StarredAtCache {
    entries: HashMap<u32, CachedAt>,
}

impl StarredAtCache {
    /// Stamp every starred item in `items` with its cached starred-at,
    /// reading `source` only where the cache has nothing usable.
    pub fn resolve(
        &mut self,
        items: &mut [WorkItem],
        source: &mut dyn StarredAtSource,
        now: Instant,
    ) {
        let starred: HashSet<u32> = items
            .iter()
            .filter(|i| i.is_operator_priority())
            .map(|i| i.number)
            .collect();
        self.entries.retain(|n, _| starred.contains(n));
        for item in items.iter_mut().filter(|i| i.is_operator_priority()) {
            let fresh = self.entries.get(&item.number).is_some_and(|e| {
                e.at.is_some() || now.saturating_duration_since(e.fetched) < STARRED_AT_RETRY
            });
            if !fresh {
                let at = source.starred_at(item.number).unwrap_or_else(|e| {
                    log::debug!(
                        "work_finder: starred-at read for issue #{} failed ({e}); \
                         ordering it by createdAt until the retry (#9244)",
                        item.number
                    );
                    None
                });
                self.entries
                    .insert(item.number, CachedAt { at, fetched: now });
            }
            item.operator_priority_at = self.entries.get(&item.number).and_then(|e| e.at.clone());
        }
    }

    /// How many starred issues the cache currently tracks (tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache tracks no starred issue (tests).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The process-wide caches, one per repo key. The work finder rebuilds its
/// `GhWorkSource` every tick, so the cache cannot live on the source.
fn caches() -> &'static Mutex<HashMap<String, StarredAtCache>> {
    static CACHES: OnceLock<Mutex<HashMap<String, StarredAtCache>>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve starred-at for `items` through the process-wide cache for
/// `repo_key`.
pub fn resolve_starred_at(
    repo_key: &str,
    items: &mut [WorkItem],
    source: &mut dyn StarredAtSource,
) {
    if !items.iter().any(WorkItem::is_operator_priority) {
        // Nothing starred: drop any stale entries without reading anything.
        if let Ok(mut guard) = caches().lock() {
            guard.remove(repo_key);
        }
        return;
    }
    let mut guard = caches()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .entry(repo_key.to_string())
        .or_default()
        .resolve(items, source, Instant::now());
}

/// The `--jq` program for [`GhTimelineStarredAt`]: one `created_at` per
/// `labeled` event for [`OPERATOR_PRIORITY_LABEL`].
const STARRED_AT_JQ: &str = r#".[] | select(.event == "labeled" and .label.name == "loom:operator-priority") | .created_at"#;

/// The latest RFC-3339 timestamp in `stdout` (one per line), i.e. the most
/// recent time the label was applied. Unparseable lines are skipped.
#[must_use]
pub fn latest_labeled_at(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(|l| l.trim().trim_matches('"'))
        .filter_map(|l| chrono::DateTime::parse_from_rfc3339(l).ok())
        .max()
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
}

/// The production [`StarredAtSource`]: a REST `gh api …/issues/<n>/timeline`
/// read (the larger pool, not GraphQL), made only for starred issues the
/// cache does not already know.
pub struct GhTimelineStarredAt {
    /// The `gh` binary.
    pub gh_bin: PathBuf,
    /// The workspace root `gh` resolves `{owner}/{repo}` from.
    pub cwd: Option<PathBuf>,
    /// An explicit `owner/repo`, when the caller has one.
    pub repo: Option<String>,
}

impl StarredAtSource for GhTimelineStarredAt {
    fn starred_at(&mut self, issue: u32) -> Result<Option<String>> {
        if crate::rate_limit_breaker::global_is_suppressed() {
            return Err(anyhow!("rate-limit breaker is suppressing forge reads"));
        }
        let repo = self.repo.as_deref().unwrap_or("{owner}/{repo}");
        let mut cmd = Command::new(&self.gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{repo}/issues/{issue}/timeline"))
            .arg("--paginate")
            .arg("--jq")
            .arg(STARRED_AT_JQ);
        if let Some(dir) = self.cwd.as_deref() {
            cmd.current_dir(dir);
        }
        crate::credential_preflight::apply_gh_config_for_cwd(&mut cmd, self.cwd.as_deref());
        let out = cmd.output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "work_finder_starred_at");
            return Err(anyhow!("gh api timeline for #{issue} failed: {stderr}"));
        }
        Ok(latest_labeled_at(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// The cache key for one listing context: the same `cwd|repo` pair the
/// ETag listing cache keys on, so two workspaces never share entries.
#[must_use]
pub fn repo_key(cwd: Option<&Path>, repo: Option<&str>) -> String {
    format!(
        "{}|{}",
        cwd.map(|p| p.display().to_string()).unwrap_or_default(),
        repo.unwrap_or_default()
    )
}

/// The two kinds of term in this tick's concurrency cap (#9244 §5, #5270).
///
/// The dynamic cap is `min(disk headroom, ram headroom, configured)`. Only the
/// **configured** term is a queue limit an operator's star may overflow; the
/// headroom terms say the host cannot hold another worktree, so they are hard
/// limits that nothing overflows. The tick needs both to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapTerms {
    /// The configured `maxConcurrent` (after env / config resolution).
    pub configured: usize,
    /// `min(disk headroom, ram headroom)`: how many sweeps the host can hold.
    pub headroom: usize,
}

impl CapTerms {
    /// The terms from this tick's configured cap and its disk / RAM probes.
    #[must_use]
    pub fn new(configured: usize, disk_headroom: usize, ram_headroom: usize) -> Self {
        Self {
            configured,
            headroom: disk_headroom.min(ram_headroom),
        }
    }

    /// The effective cap every non-overflow admission is held to.
    #[must_use]
    pub fn effective(&self) -> usize {
        self.configured.min(self.headroom)
    }
}

impl From<usize> for CapTerms {
    /// A cap with no known resource headroom: every term is configured. The
    /// plain `tick*` entry points use this; the production loops build the
    /// real terms from their disk and RAM probes.
    fn from(configured: usize) -> Self {
        Self {
            configured,
            headroom: usize::MAX,
        }
    }
}

/// The per-host overflow slot (#9244 §5): **one** over-limit sweep, starred
/// work only, and only past the *configured* cap.
///
/// A starred candidate that only the global `max_concurrent` cap and/or the
/// per-repo cap refused may be dispatched anyway, as long as:
///
/// - no live sweep on this host is already marked `overflow` (seeded from
///   [`super::WorkDispatcher::overflow_in_flight`]), and no earlier candidate
///   took the slot this tick;
/// - the host has resource headroom for one more sweep
///   (`occupancy < headroom`, and the effective cap is not 0). Disk and RAM
///   headroom are safety gates (#5270), never overflowed: when either binds
///   the cap, the star waits like everything else;
/// - `occupancy <= configured`. The cap can drop below occupancy mid-flight;
///   a host already over its limit for that reason must not add another
///   sweep, which is what keeps it to *one* over-limit sweep.
///
/// Every other gate still applies before this is consulted: the saturation
/// brake, host-class and pool gates, skip/park labels, quarantine, backoff,
/// peer claims and the per-tick ramp cap. Unstarred work, including red-main
/// fixes, never uses the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowSlot {
    taken: bool,
    terms: CapTerms,
}

impl OverflowSlot {
    /// A slot for this tick; `already_live` is whether an overflow sweep is
    /// still running from an earlier tick.
    #[must_use]
    pub fn new(already_live: bool, terms: CapTerms) -> Self {
        Self {
            taken: already_live,
            terms,
        }
    }

    /// The tick's effective cap plus its slot, for the single-workspace tick.
    #[must_use]
    pub fn open(already_live: bool, terms: CapTerms) -> (usize, Self) {
        (terms.effective(), Self::new(already_live, terms))
    }

    /// [`Self::open`] for the multi-workspace tick: the slot is taken when
    /// any workspace already has a live overflow sweep.
    #[must_use]
    pub fn for_workspaces<S, D: WorkDispatcher>(
        workspaces: &[(S, D)],
        terms: CapTerms,
    ) -> (usize, Self) {
        let live = workspaces.iter().any(|(_, d)| d.overflow_in_flight());
        Self::open(live, terms)
    }

    /// Whether a candidate refused only by a queue cap may take the slot.
    #[must_use]
    pub fn admits(&self, starred: bool, occupancy: usize) -> bool {
        starred
            && !self.taken
            && self.terms.effective() > 0
            && occupancy < self.terms.headroom
            && occupancy <= self.terms.configured
    }

    /// Mark the slot used by a dispatch that actually started.
    pub fn take(&mut self, report: &mut TickReport) {
        self.taken = true;
        report.dispatched_overflow += 1;
    }
}
