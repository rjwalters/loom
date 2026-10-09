//! The reads behind one `fleet.state` pass (Issue #10196): the sweep
//! registries, the review-label listings, the work finder's last tick and the
//! planner stamps. None of them touches the ETA subsystem.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{
    held_stage, FleetInput, HeldSweep, ListedPr, ReadyItem, ReadyQueue, RepoListing, REVIEW_LABELS,
};
use crate::sweep_registry::SweepRegistry;
use crate::telemetry::kinds::fleet_state::{FleetCapacity, FleetSlots, MainCi, PlannerStamps};
use crate::types::{ReadyQueueRow, SweepKind, WorkFinderTickSummary};
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
async fn managed_roots(
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

/// The live issue sweeps of every provisioned registry. Each registry lock is
/// held only for the in-memory clone; checkpoint reads happen after.
async fn held_sweeps(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) -> Vec<HeldSweep> {
    let mut held = Vec::new();
    for registry in workspace_pool.provisioned_registries() {
        let (snapshot, root, checkpoint_dir) = {
            let guard = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                guard.snapshot(),
                guard.config().workspace_root.clone(),
                guard.config().checkpoint_dir(),
            )
        };
        let Some(slug) = crate::observability::collector::resolve_repo_slug_cached(
            slug_cache,
            &root.to_string_lossy(),
        )
        .await
        else {
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

/// The effective operator priority level the planner ranked `row` on: its
/// `operator_priority_level` comparator key, else the bare star.
fn level(row: &ReadyQueueRow) -> u8 {
    row.plan
        .keys
        .iter()
        .find(|k| k.name == "operator_priority_level")
        .and_then(|k| k.value.as_u64())
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(u8::from(row.operator_priority))
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
    Some(ready_from_tick(&summary, slug, managed))
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
                star: row.operator_priority,
                star_at: parse_instant(row.operator_priority_at.as_deref()),
                level: level(row),
                fleet_priority: row.workspace_priority,
                created_at: parse_instant(row.created_at.as_deref()),
                main_red_fix: row.main_red_fix,
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
    }
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

/// `main` CI status of the repo rooted at `root` from the main-health gate:
/// red when halted on a verified-red run, unknown when the gate has not
/// produced a verdict or could not evaluate, else green.
fn main_ci(states: &crate::main_health_gate::WorkspaceHealthStates, root: &Path) -> MainCi {
    if states.is_halted(root) {
        MainCi::Red
    } else if states.is_unevaluated(root) || states.last_verdict_at(root).is_none() {
        MainCi::Unknown
    } else {
        MainCi::Green
    }
}

/// Everything one pass reads.
pub(super) async fn gather(
    workspace_root: &Path,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
    host_id: &str,
    now: DateTime<Utc>,
) -> FleetInput {
    let roots = managed_roots(workspace_pool, slug_cache).await;
    let managed: BTreeSet<String> = roots.iter().map(|(_, slug)| slug.clone()).collect();
    let held = held_sweeps(workspace_pool, slug_cache).await;
    let listings = review_listings(&roots).await;
    let ready = ready_queue(&managed, slug_cache).await;
    let health = crate::fleet_sync::checkout_ff::health_states();
    let main_ci = roots
        .iter()
        .map(|(root, slug)| (slug.clone(), main_ci(&health, root)))
        .collect();
    let capacity = Some(capacity(
        workspace_root,
        live_sweep_count(&workspace_pool.provisioned_registries()),
    ));
    FleetInput {
        capacity,
        main_ci,
        host_id: host_id.to_string(),
        managed,
        held,
        listed_at: (!listings.is_empty()).then_some(now),
        listings,
        ready,
    }
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
