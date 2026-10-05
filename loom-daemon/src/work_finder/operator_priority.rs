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
/// dispatch request, so the starred listing skips it. Derived from the label
/// registry's `champion_path` property (#10013).
pub static CHAMPION_PATH_LABELS: crate::label_registry::LabelSet =
    crate::label_registry::LabelSet::new(|| crate::label_registry::embedded_set("champion_path"));

/// How long an *unknown* starred-at (the timeline read failed or found no
/// event) is trusted before it is read again. A known starred-at is kept
/// until the star is removed.
pub const STARRED_AT_RETRY: Duration = Duration::from_secs(600);

impl WorkItem {
    /// True when the issue is starred at any level (#9244, #10307): it
    /// carries a level label ([`crate::operator_levels`], own or inherited),
    /// or blocks a starred issue and inherits its star (#9244 C). Dispatch
    /// treats them alike: an inherited star must land before the star can.
    #[must_use]
    pub fn is_operator_priority(&self) -> bool {
        self.operator_level() >= 1
    }

    /// The issue's effective operator priority level (#10307): the highest
    /// of its own level labels, its inherited level labels, and the
    /// in-memory inherited star (level 1). 0 = not starred.
    #[must_use]
    pub fn operator_level(&self) -> u8 {
        self.operator_level_in(crate::operator_levels::table())
    }

    /// [`Self::operator_level`] against an explicit level `table`.
    #[must_use]
    pub fn operator_level_in(&self, table: &[crate::operator_levels::PriorityLevel]) -> u8 {
        let in_memory = u8::from(self.operator_priority_inherited_from.is_some());
        crate::operator_levels::level_in(table, &self.labels).max(in_memory)
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
    /// The level the value was read at (#10307): a level change re-reads,
    /// so an issue raised to level 2 orders by when it was raised.
    level: u8,
}

/// Per-repo starred-at cache (#9244 §3).
///
/// Holds one entry per currently-starred issue. [`Self::resolve`] fetches
/// only for a starred issue with no entry (or an unknown one older than
/// [`STARRED_AT_RETRY`]) and drops the entry of every issue that is no longer
/// starred, so un-starring and re-starring reads the new event.
///
/// # Accepted staleness (Issue #9314)
///
/// The one policy choice here — **evict the moment a starred issue is not in
/// this tick's starred set** — is what keeps a re-star honest, and everything
/// below is a consequence of it, reviewed and kept as-is. Nothing here can
/// affect safety or admission: starred-at is the *second* ordering key among
/// starred issues only ([`super::ordering::candidate_cmp`]), so every
/// consequence is at worst two starred issues dispatched in the wrong order,
/// one tick apart.
///
/// - **A star flipped off and back on entirely between two ticks keeps the
///   old time.** The cache never saw the gap, so the entry survives the
///   `retain` and a known value is never re-read. Detecting it would mean
///   re-reading every starred issue's timeline every tick (the cost this cache
///   exists to avoid) or watching the label events themselves. Pinned by
///   `the_starred_at_cache_trades_restar_staleness_for_prompt_eviction`.
/// - **A starred issue that leaves the listing loses its entry**, so its
///   timeline is read again when it returns — `merge_starred` drops a starred
///   row that is `loom:building`/`loom:curating`, which is exactly the common
///   case. This is the *same* eviction, and holding entries through a claim to
///   save that one read would extend the staleness window above across the
///   whole claim. One extra read per claim cycle is the cheaper side.
/// - **A cache miss costs several REST pages** on a long timeline
///   (`--paginate`). Bounded to once per starred issue per star, on the REST
///   pool rather than GraphQL, and never for an unstarred issue.
/// - **While the rate-limit breaker suppresses forge reads** the source errors,
///   the value is cached as unknown, and the read is retried only after
///   [`STARRED_AT_RETRY`]. Deliberate: the breaker exists to stop hammering an
///   exhausted forge, and an unknown starred-at falls back to `createdAt`,
///   which still orders starred work ahead of everything unstarred.
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
            let level = item.operator_level();
            let fresh = self.entries.get(&item.number).is_some_and(|e| {
                e.level == level
                    && (e.at.is_some()
                        || now.saturating_duration_since(e.fetched) < STARRED_AT_RETRY)
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
                self.entries.insert(
                    item.number,
                    CachedAt {
                        at,
                        fetched: now,
                        level,
                    },
                );
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

/// The `--jq` program for [`GhTimelineStarredAt`]: `L <created_at>` per
/// `labeled` event for any level label ([`crate::operator_levels`], #10307),
/// and `C <created_at> <requested_at> <author_association> <login>` per
/// loom-ui star-intent audit comment (#9244 C, any level), whose
/// `requested_at` is the authoritative starred-at when its author is
/// trusted. Parsed by
/// [`crate::star_liveness::intents::starred_at_from_timeline`].
///
/// The starred-at is therefore when the issue last *reached* a level, and
/// the cache re-reads it when the level changes: a level-2 issue orders
/// among level-2 issues by when it was raised. A demotion back to the star
/// keeps the later (level-2) time, which only orders it later among stars.
#[must_use]
pub fn starred_at_jq() -> String {
    let labels = crate::operator_levels::starred_labels(crate::operator_levels::table())
        .iter()
        .map(|l| format!(".label.name == \"{l}\""))
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        r#".[] | if (.event == "labeled" and ({labels})) then "L \(.created_at)" elif (.event == "commented" and ((.body // "") | contains("loom:operator-priority-intent=") and contains("action=star"))) then "C \(.created_at) \((.body | capture("requested_at=(?<t>[^ >]+)") | .t) // "-") \(.author_association // "-") \(.actor.login // .user.login // "-")" else empty end"#
    )
}

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
        // #10089: counted via the facade (`work_finder.starred_at`); with no
        // cwd the facade runs in the daemon's own directory.
        let mut inv = crate::gh_invocation::GhInvocation::new(
            crate::gh_invocation::Operation::new("work_finder.starred_at"),
            crate::gh_invocation::AccessIntent::Read,
            crate::gh_invocation::GhTarget::None,
            crate::claim_reconciliation::gh_call::GH_TIMEOUT,
        )
        .forge_op(crate::forge_call_stats::ops::TIMELINE_READ)
        .program(&self.gh_bin)
        .args(["api", &format!("repos/{repo}/issues/{issue}/timeline")])
        .args(["--paginate", "--jq", &starred_at_jq()]);
        if let Some(dir) = self.cwd.as_deref() {
            inv = inv.current_dir(dir);
        }
        let out = crate::claim_reconciliation::gh_call::output(inv)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "work_finder_starred_at");
            return Err(anyhow!("gh api timeline for #{issue} failed: {stderr}"));
        }
        Ok(crate::star_liveness::intents::starred_at_from_timeline(
            &String::from_utf8_lossy(&out.stdout),
        ))
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

    /// [`Self::new`] for a production tick: also records the raw terms for
    /// the published tick summary (Issue #10214), so the liveness pass and
    /// the fleet alert can say which term holds the cap down.
    #[must_use]
    pub fn observed(configured: usize, disk_headroom: usize, ram_headroom: usize) -> Self {
        super::tick_summary::record_cap(crate::types::CapView::from_terms(
            configured,
            disk_headroom,
            ram_headroom,
        ));
        Self::new(configured, disk_headroom, ram_headroom)
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

    /// Whether the slot is still unused: no live overflow sweep and none
    /// taken this tick (Issue #9288's `plan.slots.overflow_free`).
    #[must_use]
    pub fn is_free(&self) -> bool {
        !self.taken
    }

    /// Mark the slot used by a dispatch that actually started.
    pub fn take(&mut self, report: &mut TickReport) {
        self.taken = true;
        report.dispatched_overflow += 1;
    }
}
