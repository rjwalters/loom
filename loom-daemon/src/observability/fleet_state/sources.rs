//! The reads behind one `fleet.state` pass (Issue #10196): the sweep
//! registries, the review-label listings, the work finder's last tick and the
//! planner stamps. None of them touches the ETA subsystem.
//!
//! Only the collector pass ([`gather`]) reads the forge: it resolves the repo
//! slugs and visibility and walks the review listings, and keeps them in
//! [`Reused`]. A tick pass ([`tick_input`], #11161) reads only local state
//! and that copy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use super::champion::champion_by_slug;
use super::outcomes::Curated;
use super::{
    held_stage, FleetInput, HeldSweep, ListedPr, ReadyItem, ReadyQueue, RepoLimits, RepoListing,
    REVIEW_LABELS,
};
use crate::sweep_registry::SweepRegistry;
use crate::telemetry::kinds::fleet_state::{
    FleetCapacity, FleetEligibleReason, FleetHaltReason, FleetRepoHalt, FleetSlots, MainCi,
    PlannerStamps,
};
use crate::telemetry::RepoVisibility;
use crate::types::{ReadyQueueRow, RepoLimitView, SweepKind, WorkFinderTickSummary};
use crate::workspace_pool::WorkspacePool;
use crate::worktree_ops::gh::{linkage_refs, LinkageKind};

/// The `ListIssues` caller tag these listings are attributed to.
const CALLER: &str = "fleet_state";

fn parse_instant(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

fn small(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Every provisioned root with its lowercased slug, one root per slug.
pub(super) async fn managed_roots(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) -> Vec<(PathBuf, String)> {
    let mut seen = BTreeSet::new();
    let mut roots = Vec::new();
    for root in crate::observability::collector::provisioned_roots(workspace_pool) {
        let Some(slug) = crate::observability::collector::resolve_repo_slug_cached(
            slug_cache,
            &root.to_string_lossy(),
        )
        .await
        else {
            continue;
        };
        let slug = slug.to_ascii_lowercase();
        if seen.insert(slug.clone()) {
            roots.push((root, slug));
        }
    }
    roots
}

/// The checkpoint `phase`, `timestamp` and `pr_number` of `issue`'s sweep,
/// read only when this run (started at `started_at`) wrote the file.
fn checkpoint(
    dir: &Path,
    issue: u32,
    started_at: DateTime<Utc>,
) -> (Option<String>, Option<DateTime<Utc>>, Option<u32>) {
    let path = dir.join(format!("issue-{issue}.json"));
    if !crate::sweep_registry::reaper::checkpoint_written_by_run(&path, started_at) {
        return (None, None, None);
    }
    let Some(v) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return (None, None, None);
    };
    (
        v.get("phase").and_then(|p| p.as_str()).map(str::to_string),
        parse_instant(v.get("timestamp").and_then(|t| t.as_str())),
        v.get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
    )
}

/// Non-terminal sweeps of every provisioned registry, of any kind (`Issue` or
/// `PrSet`). Deliberately independent of [`held_sweeps`]' issue-row projection
/// and of repo-slug resolution: `capacity.live_workers` counts workers this
/// host runs, not rows that can be attributed to a repo.
fn live_sweep_count(registries: &[std::sync::Arc<std::sync::Mutex<SweepRegistry>>]) -> usize {
    registries
        .iter()
        .map(|registry| {
            let snapshot = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot();
            snapshot
                .list(None)
                .iter()
                .filter(|info| !info.state.is_terminal())
                .count()
        })
        .sum()
}

/// The live issue sweeps of every provisioned registry, each root named by
/// `slug`. Each registry lock is held only for the in-memory clone;
/// checkpoint reads happen after.
fn held_sweeps(
    registries: &[std::sync::Arc<std::sync::Mutex<SweepRegistry>>],
    slug: impl Fn(&str) -> Option<String>,
) -> Vec<HeldSweep> {
    let mut held = Vec::new();
    for registry in registries {
        let (snapshot, root, checkpoint_dir, starts) = {
            let guard = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                guard.snapshot(),
                guard.config().workspace_root.clone(),
                guard.config().checkpoint_dir(),
                guard.start_facts_snapshot(),
            )
        };
        let Some(slug) = slug(&root.to_string_lossy()) else {
            continue;
        };
        for info in snapshot.list(None) {
            let SweepKind::Issue(issue) = info.kind else {
                continue;
            };
            if info.state.is_terminal() {
                continue;
            }
            let (phase, phase_at, pr) = checkpoint(&checkpoint_dir, issue, info.started_at);
            let (stage, entered_at, lower_bound) =
                held_stage(phase.as_deref(), phase_at, info.started_at);
            held.push(HeldSweep {
                repo: slug.to_ascii_lowercase(),
                issue,
                stage,
                entered_at,
                entered_at_lower_bound: lower_bound,
                pr,
                overflow: info.overflow,
                start: starts.get(&info.sweep_id).cloned().unwrap_or_default(),
                sweep_id: Some(info.sweep_id.clone()),
            });
        }
    }
    held
}

/// The issue a PR body links: the lowest closing reference, else the lowest
/// `Part of` reference (the work finder's linkage rule).
fn linked_issue(body: &str) -> Option<u32> {
    let refs = linkage_refs(body);
    refs.iter()
        .find(|(_, kind)| *kind == LinkageKind::Closes)
        .or_else(|| refs.first())
        .map(|(n, _)| *n)
}

/// One repo's three review-label listings, each walked to its last page
/// (`list_issues_cached_all_as`: page 1 is the single-page listing's own
/// cache entry, so an unchanged queue costs one free `304`). Blocking.
///
/// # Errors
///
/// Any walk failed, read `MAX_PAGES` full pages or saw the listing shift:
/// the set may be incomplete, so the whole repo is unobserved this pass.
fn review_listing_with(
    gh: &Path,
    root: &Path,
    repo: Option<&str>,
    slug: &str,
) -> anyhow::Result<RepoListing> {
    let mut seen = BTreeSet::new();
    let mut prs = Vec::new();
    for label in REVIEW_LABELS {
        let items = crate::forge_listing::list_issues_cached_all_as(
            CALLER,
            gh,
            Some(root),
            repo,
            label,
            "open",
        )?;
        prs.extend(
            items
                .into_iter()
                .filter(|item| item.is_pull_request && seen.insert(item.number))
                .map(|item| ListedPr {
                    number: item.number,
                    issue: linked_issue(item.body.as_deref().unwrap_or_default()),
                    labels: item.labels,
                }),
        );
    }
    Ok(RepoListing {
        repo: slug.to_string(),
        prs,
    })
}

/// Each managed repo's review listings; a repo is included only when every
/// walk completed (a partial listing would read as PRs leaving).
async fn review_listings(roots: &[(PathBuf, String)]) -> Vec<RepoListing> {
    let mut out = Vec::new();
    for (root, slug) in roots {
        let (root, owned) = (root.clone(), slug.clone());
        let result = tokio::task::spawn_blocking(move || {
            let gh = PathBuf::from(crate::gh_invocation::gh_bin());
            review_listing_with(&gh, &root, None, &owned)
        })
        .await;
        match result {
            Ok(Ok(listing)) => out.push(listing),
            Ok(Err(error)) => {
                log::debug!("fleet.state: review listings of {slug} incomplete: {error:#}");
            }
            Err(join_error) => {
                log::debug!("fleet.state: review listings of {slug} panicked: {join_error}");
            }
        }
    }
    out
}

/// One repo's open `loom:curated` issues with their creation instants, walked
/// to the last page through the same ETag cache as the review listings
/// (#11368). Blocking.
///
/// # Errors
///
/// The walk failed, read `MAX_PAGES` full pages or saw the listing shift: the
/// set may be incomplete, so the repo has no curated listing this pass.
#[cfg(test)]
fn curated_listing_with(
    gh: &Path,
    root: &Path,
    repo: Option<&str>,
) -> anyhow::Result<BTreeMap<u32, Option<DateTime<Utc>>>> {
    label_listing_with(gh, root, repo, super::outcomes::CURATED_LABEL)
}

/// [`curated_listing_with`] for any issue label (`loom:triage`, #11398).
///
/// # Errors
///
/// As [`curated_listing_with`].
fn label_listing_with(
    gh: &Path,
    root: &Path,
    repo: Option<&str>,
    label: &str,
) -> anyhow::Result<BTreeMap<u32, Option<DateTime<Utc>>>> {
    let items = crate::forge_listing::list_issues_cached_all_as(
        CALLER,
        gh,
        Some(root),
        repo,
        label,
        "open",
    )?;
    Ok(items
        .into_iter()
        .filter(|item| !item.is_pull_request)
        .map(|item| (item.number, parse_instant(item.created_at.as_deref())))
        .collect())
}

/// Each managed repo's listing of open `label` issues; an incomplete one
/// leaves the repo out (a partial listing would read as issues arriving or
/// leaving).
async fn label_listings(roots: &[(PathBuf, String)], label: &'static str) -> Curated {
    let mut out = Curated::new();
    for (root, slug) in roots {
        let root = root.clone();
        let result = tokio::task::spawn_blocking(move || {
            let gh = PathBuf::from(crate::gh_invocation::gh_bin());
            label_listing_with(&gh, &root, None, label)
        })
        .await;
        match result {
            Ok(Ok(listing)) => {
                out.insert(slug.clone(), listing);
            }
            Ok(Err(error)) => {
                log::debug!("fleet.state: {label} listing of {slug} incomplete: {error:#}");
            }
            Err(join_error) => {
                log::debug!("fleet.state: {label} listing of {slug} panicked: {join_error}");
            }
        }
    }
    out
}

/// The priority level the planner ranked `row` on (0 default, 1
/// `loom:important`, 2 `loom:very-important`): its `level` comparator key,
/// else the row's own `level` (#11510).
fn level(row: &ReadyQueueRow) -> u8 {
    row.plan
        .keys
        .iter()
        .find(|k| k.name == "level")
        .and_then(|k| k.value.as_u64())
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or_else(|| row.level.rank())
}

/// The work finder's last tick: every row not running, with its rank and
/// ranking inputs as the planner published them.
async fn ready_queue(
    managed: &BTreeSet<String>,
    slug_cache: &mut HashMap<String, String>,
) -> Option<ReadyQueue> {
    let summary = crate::work_finder::last_tick_summary()?;
    let refs = crate::observability::queue_snapshot::resolve_repos(&summary, slug_cache).await;
    let slug = |root: &str| refs.get(root).map(|r| r.repo.to_ascii_lowercase());
    let mut ready = ready_from_tick(&summary, slug, managed);
    ready.halts = halts_by_slug(&crate::work_finder::halt_cause::since::current(), slug);
    Some(ready)
}

/// The work finder's held roots (#11564) by lowercased slug, each as its
/// `fleet.state` fact. Every held root is kept (no cap); a root `slug` cannot
/// name is left out, as its ready rows are. Pure.
pub(super) fn halts_by_slug(
    halts: &Option<Vec<crate::work_finder::halt_cause::since::RootHalt>>,
    slug: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, FleetRepoHalt> {
    let mut out = BTreeMap::new();
    for halt in halts.iter().flatten() {
        let Some(repo) = slug(&halt.root.to_string_lossy()) else {
            continue;
        };
        out.entry(repo).or_insert_with(|| FleetRepoHalt {
            reason: FleetHaltReason::from_cause(halt.cause),
            since: halt.since,
            since_lower_bound: halt.since_lower_bound,
        });
    }
    out
}

/// [`ready_queue`] over one tick, with `slug` resolving a root. Pure.
///
/// `listed` is every managed repo the tick read without a listing failure. A
/// single-workspace tick records no rows, so it lists no repo. `complete` is
/// the listed repos whose listing the work finder walked to its last page
/// (#11139): every listed repo not in `listing_incomplete`. A listed repo
/// that is not complete has its ready rows replaced wholesale on the wire
/// (`ready_replace`).
fn ready_from_tick(
    summary: &WorkFinderTickSummary,
    slug: impl Fn(&str) -> Option<String>,
    managed: &BTreeSet<String>,
) -> ReadyQueue {
    let items = summary
        .queue
        .iter()
        .filter(|row| row.disposition.state() != "running")
        .filter_map(|row| {
            Some(ReadyItem {
                repo: slug(&row.repo)?,
                issue: row.issue,
                rank: small(row.rank),
                level: level(row),
                fleet_priority: row.workspace_priority,
                created_at: parse_instant(row.created_at.as_deref()),
                main_red_fix: row.main_red_fix,
                eligible_reason: FleetEligibleReason::from_disposition(row.disposition),
                // Only a clocked hold's own expiry: never a stray one.
                eligible_at: FleetEligibleReason::from_disposition(row.disposition)
                    .filter(|reason| reason.has_clock())
                    .and(row.plan.held_until),
            })
        })
        .collect();
    let slugs = |roots: &[String]| -> BTreeSet<String> {
        roots.iter().filter_map(|root| slug(root)).collect()
    };
    let listed: BTreeSet<String> = if summary.plan.is_some() {
        managed
            .difference(&slugs(&summary.listing_failed))
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    };
    let complete = listed
        .difference(&slugs(&summary.listing_incomplete))
        .cloned()
        .collect();
    let slots = summary.plan.as_ref().map(|plan| FleetSlots {
        max_concurrent: small(plan.slots.max_concurrent),
        occupancy: plan.slots.occupancy.map(small),
    });
    ReadyQueue {
        items,
        listed,
        complete,
        slots,
        halts: BTreeMap::new(),
        limits: limits_by_slug(&summary.repo_limits, slug),
    }
}

/// The tick's per-repo dispatch limits (#11606) by lowercased slug. Every
/// entry is kept (no cap); a root `slug` cannot name is left out, as its
/// ready rows are. Pure.
pub(super) fn limits_by_slug(
    limits: &[RepoLimitView],
    slug: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, RepoLimits> {
    let mut out = BTreeMap::new();
    for limit in limits {
        let Some(repo) = slug(&limit.repo) else {
            continue;
        };
        out.entry(repo).or_insert_with(|| RepoLimits {
            in_flight: small(limit.in_flight),
            cap: limit.cap.map(small),
            deferred_repo_cap: small(limit.deferred_repo_cap),
        });
    }
    out
}

/// Available and exhausted token accounts in the rotation ranking at
/// `workspace_root`; `None` when the ranking is missing or empty.
fn account_counts(workspace_root: &Path) -> Option<(u32, u32)> {
    let ranking = crate::tokens_pool::paths::resolve_tokens_dir(workspace_root).join(".ranking");
    parse_account_counts(&std::fs::read_to_string(ranking).ok()?)
}

/// `(available, exhausted)` over the ranking `contents`. Only
/// `AccountHealth::Available` is usable and only `AccountHealth::Exhausted`
/// is exhausted; `rate_limited`, `blocked` and unknown words are neither.
/// `None` when no account is listed.
pub(super) fn parse_account_counts(contents: &str) -> Option<(u32, u32)> {
    use crate::capacity::AccountHealth;
    let (mut listed, mut usable, mut exhausted) = (0_u32, 0_u32, 0_u32);
    for line in contents.lines().filter(|l| l.contains('|')) {
        let Some(row) = crate::tokens_pool::select::parse_ranking_line(line) else {
            continue;
        };
        listed += 1;
        match AccountHealth::parse(&row.status) {
            AccountHealth::Available => usable += 1,
            AccountHealth::Exhausted => exhausted += 1,
            AccountHealth::RateLimited | AccountHealth::Blocked | AccountHealth::Unknown => {}
        }
    }
    (listed > 0).then_some((usable, exhausted))
}

/// This host's capacity facts. Only discrete values: no utilisation fraction.
fn capacity(workspace_root: &Path, live_workers: usize) -> FleetCapacity {
    let accounts = account_counts(workspace_root);
    FleetCapacity {
        live_workers: small(live_workers),
        accounts_usable: accounts.map(|(usable, _)| usable),
        accounts_exhausted: accounts.map(|(_, exhausted)| exhausted),
        host_breaker: crate::host_breaker::global_snapshot()
            .filter(|b| b.enabled)
            .map(|b| b.phase.as_str().to_string()),
        rate_limit_breaker: crate::rate_limit_breaker::global_snapshot()
            .filter(|b| b.enabled)
            .map(|b| b.phase.as_str().to_string()),
        admission_brake_held: crate::admission_brake::global_snapshot()
            .filter(|b| b.enabled)
            .map(|b| b.held),
    }
}

/// This host's slots, measured from local state (#11643): `max_concurrent`
/// from the work-finder config (precedence env > config > default, via
/// [`resolve_max_concurrent_with_source`]) and `occupancy` from the
/// already-computed live-sweep count. Independent of the work-finder plan, so
/// it is present from the first pass of a process and after a roll/restart —
/// the plan, and thus `ready.slots`, is absent until the first tick.
///
/// `occupancy` stays an `Option` (absent = unknown, never a synthetic `0`);
/// here it is always `Some`, because every pass re-measures the live count, so
/// a genuinely idle host emits `occupancy: 0`, distinct from "not yet
/// measured".
fn local_slots(workspace_root: &Path, live_workers: usize) -> FleetSlots {
    let config = crate::work_finder::read_work_finder_config(workspace_root);
    let max_concurrent = crate::work_finder::resolve_max_concurrent_with_source(&config).0;
    FleetSlots {
        max_concurrent: small(max_concurrent),
        occupancy: Some(small(live_workers)),
    }
}

/// `main` CI status of the repo rooted at `root` from the main-health gate:
/// red when halted on a verified-red run, green when the gate has produced a
/// green verdict, unknown otherwise (#11565). Unknown means the gate has no
/// verdict for the root: it is not enabled for it (`autonomous.mainHealthGate`
/// is off by default, or the repo has no usable `buildGate`), has not
/// completed a run yet this process, or its last tick could not evaluate.
fn main_ci(states: &crate::main_health_gate::WorkspaceHealthStates, root: &Path) -> MainCi {
    let Some(state) = states.find(root) else {
        return MainCi::Unknown;
    };
    if state.is_halted() {
        MainCi::Red
    } else if state.is_unevaluated() || state.last_verdict_at().is_none() {
        MainCi::Unknown
    } else {
        MainCi::Green
    }
}

/// Each repo's `main` CI status by lowercased slug, from `(root, slug)` pairs.
fn main_ci_by_slug<'a>(
    roots: impl Iterator<Item = (&'a Path, &'a str)>,
) -> BTreeMap<String, MainCi> {
    let health = crate::fleet_sync::checkout_ff::health_states();
    roots
        .map(|(root, slug)| (slug.to_string(), main_ci(&health, root)))
        .collect()
}

/// What the last collector pass read from the forge, which every tick pass
/// reuses as is (#11161): a tick pass makes no forge call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Reused {
    pub managed: BTreeSet<String>,
    pub listings: Vec<RepoListing>,
    pub listed_at: Option<DateTime<Utc>>,
    /// Root path to lowercased slug, for every root the collector cached.
    pub slugs: HashMap<String, String>,
    /// The managed `(root, slug)` pairs the collector's `main_ci` was read
    /// from: unlike `slugs`, never an unprovisioned or respelled root.
    pub roots: Vec<(PathBuf, String)>,
    /// Lowercased slug to visibility. A repo missing here is private.
    pub visibility: BTreeMap<String, RepoVisibility>,
}

impl Reused {
    fn slug(&self, root: &str) -> Option<String> {
        self.slugs.get(root).cloned()
    }
}

static REUSED: Mutex<Option<Reused>> = Mutex::new(None);

/// The last collector pass's reads; `None` before the first one.
pub(super) fn reused() -> Option<Reused> {
    REUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Keep `reused` for the tick passes until the next collector pass.
pub(super) fn keep(reused: Reused) {
    *REUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reused);
}

/// Everything one collector pass reads, and the part of it a tick pass
/// reuses (without `visibility`, which the caller resolves).
pub(super) async fn gather(
    workspace_root: &Path,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
    host_id: &str,
    now: DateTime<Utc>,
) -> (FleetInput, Reused, Curated, Curated) {
    // Resolves every provisioned root, so the registries' roots are cached.
    let roots = managed_roots(workspace_pool, slug_cache).await;
    let managed: BTreeSet<String> = roots.iter().map(|(_, slug)| slug.clone()).collect();
    let held = held_sweeps(&workspace_pool.provisioned_registries(), |root| {
        slug_cache.get(root).map(|s| s.to_ascii_lowercase())
    });
    let listings = review_listings(&roots).await;
    let curated = label_listings(&roots, super::outcomes::CURATED_LABEL).await;
    let triage = label_listings(&roots, super::outcomes::close::TRIAGE_LABEL).await;
    let ready = ready_queue(&managed, slug_cache).await;
    let main_ci = main_ci_by_slug(
        roots
            .iter()
            .map(|(root, slug)| (root.as_path(), slug.as_str())),
    );
    let live_workers = live_sweep_count(&workspace_pool.provisioned_registries());
    let capacity = Some(capacity(workspace_root, live_workers));
    let slots = Some(local_slots(workspace_root, live_workers));
    let listed_at = (!listings.is_empty()).then_some(now);
    let reused = Reused {
        managed: managed.clone(),
        listings: listings.clone(),
        listed_at,
        roots,
        slugs: slug_cache
            .iter()
            .map(|(root, slug)| (root.clone(), slug.to_ascii_lowercase()))
            .collect(),
        visibility: BTreeMap::new(),
    };
    let champion = champion_by_slug(&reused.roots);
    let input = FleetInput {
        champion,
        capacity,
        slots,
        main_ci,
        host_id: host_id.to_string(),
        managed,
        held,
        listed_at,
        listings,
        ready,
    };
    (input, reused, curated, triage)
}

/// A tick pass's input (#11161): the registries and the work finder's last
/// tick, host capacity and `main` CI, which are local, with the slugs and review listings of the last
/// collector pass. Nothing here reads the forge.
pub(super) fn tick_input(
    workspace_root: &Path,
    workspace_pool: &WorkspacePool,
    host_id: &str,
    reused: &Reused,
) -> FleetInput {
    let live_workers = live_sweep_count(&workspace_pool.provisioned_registries());
    FleetInput {
        champion: champion_by_slug(&reused.roots),
        capacity: Some(capacity(workspace_root, live_workers)),
        slots: Some(local_slots(workspace_root, live_workers)),
        main_ci: tick_main_ci(reused),
        held: held_sweeps(&workspace_pool.provisioned_registries(), |root| reused.slug(root)),
        ready: crate::work_finder::last_tick_summary().map(|summary| ready_input(&summary, reused)),
        ..reused_input(host_id, reused)
    }
}

/// A tick pass's `main` CI: the managed roots only, as the collector pass
/// reads it, so the two passes never disagree about a repo.
pub(super) fn tick_main_ci(reused: &Reused) -> BTreeMap<String, MainCi> {
    main_ci_by_slug(
        reused
            .roots
            .iter()
            .map(|(root, slug)| (root.as_path(), slug.as_str())),
    )
}

/// [`tick_input`] without the registries and the tick. Pure.
pub(super) fn reused_input(host_id: &str, reused: &Reused) -> FleetInput {
    FleetInput {
        host_id: host_id.to_string(),
        managed: reused.managed.clone(),
        held: Vec::new(),
        listings: reused.listings.clone(),
        listed_at: reused.listed_at,
        ready: None,
        slots: None,
        capacity: None,
        champion: BTreeMap::new(),
        main_ci: BTreeMap::new(),
    }
}

/// The ready queue of `summary`, its roots named by the collector's slugs,
/// with the work finder's current repo halts (#11564).
pub(super) fn ready_input(summary: &WorkFinderTickSummary, reused: &Reused) -> ReadyQueue {
    let mut ready = ready_from_tick(summary, |root| reused.slug(root), &reused.managed);
    let halts = crate::work_finder::halt_cause::since::current();
    ready.halts = halts_by_slug(&halts, |root| reused.slug(root));
    ready
}

/// The seconds between passes: the work finder's tick interval once it has
/// ticked in this process, else the collector's snapshot interval.
pub(super) fn tick_interval_secs(workspace_root: &Path) -> u64 {
    if crate::work_finder::last_tick_summary().is_none() {
        return crate::observability::SNAPSHOT_INTERVAL.as_secs();
    }
    let config = crate::work_finder::read_work_finder_config(workspace_root);
    crate::work_finder::resolve_interval_with_config(&config).as_secs()
}

/// This host's planner stamps: the daemon version, the hash of the
/// planner-relevant effective config at `workspace_root`, and the fleet store
/// commit the last fleet-sync config pass resolved.
pub(super) fn stamps(workspace_root: &Path) -> PlannerStamps {
    let effective = crate::config_resolver::resolve_effective_config(workspace_root);
    PlannerStamps {
        planner_version: env!("CARGO_PKG_VERSION").to_string(),
        planner_config_hash: super::planner_config_hash(&effective),
        fleet_config_hash: crate::fleet_sync::cached_status().and_then(|s| s.config.commit),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "sources_tests.rs"]
mod tests;
