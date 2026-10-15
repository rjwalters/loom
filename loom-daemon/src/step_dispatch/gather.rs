//! One workspace's items for the step tick (#11909): list, join, classify,
//! and the facts the classifier and the draw read.
//!
//! The pure half ([`build_inputs`], [`draw_inputs`], [`ladder_due`]) turns
//! listing rows and forge facts into the classifier's
//! [`Inputs`](crate::work_finder::step_item::Inputs) and the draw's
//! [`DrawInput`]s. The forge half ([`gather`]) reads them, each read
//! bounded:
//!
//! - **the items**: [`super::listing::list_all_open`] (mirror, else the
//!   shared ETag walk);
//! - **the PRs' head and author**: the open-PR listing
//!   (`forge_pull_listing`, ETag-cached), so the Builder branch links as it
//!   does in the open-PR guard. When it cannot be read, a PR row links by its
//!   body alone;
//! - **hold markers** on `loom:blocked` PRs: their trusted comments, cached
//!   per `(repo, PR, updated_at)`, at most [`MAX_MARKER_READS`] per tick;
//! - **dependency parks**: whether every named blocker is a closed issue or a
//!   merged PR, for curated parked issues only, cached for
//!   [`BLOCKER_TTL`], at most [`MAX_BLOCKER_READS`] per tick. A blocker in the
//!   open listing is open without a read;
//! - **landed** `loom:issue` items: the landed pre-filter's batched,
//!   cached verdicts (`work_finder::landed_prefilter`);
//! - **stage entry**: the provider (`super::stage_entry`), for stepped
//!   items only (a pass needs no age).
//!
//! A read that fails or runs out of budget leaves its fact at the value that
//! passes the item (a held PR without its marker reads as `blocked`, an
//! unread dependency park stays parked): never a step the forge did not
//! vouch for.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::item_draw::DrawInput;
use super::stage_entry::{GhEventsReader, StageEntry, StageEntryProvider};
use crate::forge_listing::RestIssue;
use crate::forge_pull_listing::RestPull;
use crate::step_role::StepRole;
use crate::sweep_registry::SweepRegistry;
use crate::work_finder::step_item::{
    self, HostInput, Inputs, IssueInput, IssueMarkers, Item, ItemState, LinkKind, MergedLink,
    ParkKind, PullInput, PullMarkers, RepoInput, TrustedComments,
};
use crate::worktree_ops::linked_pr_listing::linked_issues;

/// At most this many PRs' comments are read for hold markers per repo per
/// tick.
pub const MAX_MARKER_READS: usize = 8;
/// At most this many blocker states are read per repo per tick.
pub const MAX_BLOCKER_READS: usize = 8;
/// How long a blocker's state is believed.
pub const BLOCKER_TTL: Duration = Duration::from_secs(10 * 60);
/// Pages of the open-PR listing read per tick.
pub const PULL_PAGES: usize = 10;

const ISSUE: &str = concat!("loom", ":issue");
const CURATED: &str = concat!("loom", ":curated");

/// The per-issue facts read from the forge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueFacts {
    /// Issue → its merged closing PR (landed, not yet kept by Curator).
    pub landed: HashMap<u32, u32>,
    /// Dependency-parked issues whose every blocker is closed or merged.
    pub blockers_closed: HashSet<u32>,
    /// Issues whose red-main-fix marker counts: promoted, or filed by a
    /// trusted author.
    pub red_fix: HashSet<u32>,
}

/// One workspace's draw inputs, and what else the tick reads from it.
#[derive(Debug, Clone, Default)]
pub struct Gathered {
    /// The forge slug.
    pub slug: String,
    pub inputs: Vec<DrawInput>,
    /// The Champion re-evaluation ladder's due times, from the bodies.
    pub ladder: Vec<(u32, StepRole, DateTime<Utc>)>,
    /// Creation times by number.
    pub created_at: HashMap<u32, String>,
    /// The open-PR listing answered. When it did not, a PR linked only by
    /// its Builder branch is invisible, so the tick draws no Builder step in
    /// the repo (a duplicate build is worse than a tick's wait).
    pub pulls_complete: bool,
    /// The item listing failed and the rows are the last good one
    /// ([`LAST_GOOD_TTL`] old at most).
    pub stale_listing: bool,
}

/// How old a last good listing may be and still stand in for a failed one.
/// A fleet-wide forge outage (a secondary rate limit) would otherwise stop
/// every item dispatch everywhere; a short window rides it out, and every
/// claim still re-reads the forge.
pub const LAST_GOOD_TTL: Duration = Duration::from_secs(10 * 60);

type LastGood = HashMap<String, (Instant, Vec<RestIssue>)>;

fn last_good() -> &'static Mutex<LastGood> {
    static CACHE: OnceLock<Mutex<LastGood>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The listing, or on its failure the last good one while it is fresh.
fn listing_or_last_good(
    slug: &str,
    listing: anyhow::Result<super::listing::OpenItems>,
) -> anyhow::Result<(Vec<RestIssue>, bool)> {
    let mut cache = last_good()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match listing {
        Ok(items) => {
            cache.insert(slug.to_string(), (Instant::now(), items.rows.clone()));
            Ok((items.rows, false))
        }
        Err(e) => match cache.get(slug) {
            Some((at, rows)) if at.elapsed() < LAST_GOOD_TTL => {
                log::warn!(
                    "work_finder: {slug}: step listing failed ({e:#}); using the listing from \
                     {}s ago",
                    at.elapsed().as_secs()
                );
                Ok((rows.clone(), true))
            }
            _ => Err(e),
        },
    }
}

fn has(labels: &[String], label: &str) -> bool {
    labels.iter().any(|l| l == label)
}

fn parse_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A body carries the red-main-fix marker at the start of a line.
#[must_use]
pub fn has_red_fix_marker(body: Option<&str>) -> bool {
    body.is_some_and(|b| {
        b.lines().any(|l| {
            l.trim_start()
                .starts_with(crate::work_finder::main_red_fix::MAIN_RED_FIX_MARKER)
        })
    })
}

/// A PR as the classifier reads it, from an issue-listing row alone: its
/// body links it, its branch cannot (head unknown).
#[must_use]
pub fn pull_from_row(row: &RestIssue, markers: PullMarkers) -> PullInput {
    PullInput {
        number: row.number,
        labels: row.labels.clone(),
        body: row.body.clone(),
        head_ref: None,
        same_repo: false,
        author_untrusted: false,
        queued: false,
        markers,
    }
}

/// The classifier's inputs from the open issue rows, the PRs and the facts.
/// Pure.
#[must_use]
pub fn build_inputs(
    rows: &[RestIssue],
    pulls: Vec<PullInput>,
    facts: &IssueFacts,
    repo: RepoInput,
    host: HostInput,
) -> Inputs {
    let issues = rows
        .iter()
        .filter(|r| !r.is_pull_request)
        .map(|r| {
            let park = step_item::park_kind(&r.labels, r.body.as_deref());
            IssueInput {
                number: r.number,
                labels: r.labels.clone(),
                park,
                blockers_closed: facts.blockers_closed.contains(&r.number),
                capabilities: match (park, r.body.as_deref()) {
                    (ParkKind::Capability, Some(body)) => step_item::declared_capabilities(body),
                    _ => Vec::new(),
                },
                merged_links: facts
                    .landed
                    .get(&r.number)
                    .map(|&pr| {
                        vec![MergedLink {
                            pr,
                            kind: LinkKind::Closes,
                        }]
                    })
                    .unwrap_or_default(),
                markers: IssueMarkers::default(),
                red_fix: facts.red_fix.contains(&r.number),
            }
        })
        .collect();
    Inputs {
        issues,
        pulls,
        repo,
        host,
    }
}

/// The label naming the stage an item is in, whose entry time is its age;
/// `None` reads its creation time.
#[must_use]
pub fn stage_label(state: ItemState) -> Option<&'static str> {
    Some(match state {
        ItemState::Triage => concat!("loom", ":triage"),
        ItemState::DecisionMalformed => concat!("loom", ":decision-malformed"),
        ItemState::Curated => CURATED,
        ItemState::Ready => ISSUE,
        ItemState::Blocked | ItemState::Held => concat!("loom", ":blocked"),
        ItemState::ReviewRequested => concat!("loom", ":review-requested"),
        ItemState::ChangesRequested => concat!("loom", ":changes-requested"),
        ItemState::Approved => concat!("loom", ":pr"),
        _ => return None,
    })
}

/// The draw's inputs for `items`, reading each item's priority labels and
/// creation time from its rows; `entry` answers a stepped item's stage entry
/// from its target's row and stage label.
pub fn draw_inputs(
    items: Vec<Item>,
    rows: &[RestIssue],
    mut entry: impl FnMut(&RestIssue, &'static str) -> Option<StageEntry>,
) -> Vec<DrawInput> {
    let by_number: HashMap<u32, &RestIssue> = rows.iter().map(|r| (r.number, r)).collect();
    items
        .into_iter()
        .map(|item| {
            let own = item
                .issue
                .or(item.pr)
                .and_then(|n| by_number.get(&n).copied());
            let stage_entry = item
                .target()
                .and_then(|n| by_number.get(&n).copied())
                .zip(stage_label(item.state))
                .and_then(|(row, label)| entry(row, label));
            DrawInput {
                labels: own.map(|r| r.labels.clone()).unwrap_or_default(),
                created_at: own.and_then(|r| parse_time(r.created_at.as_deref())),
                stage_entry,
                item,
            }
        })
        .collect()
}

/// The Champion re-evaluation ladder's due times in the rows' bodies: a
/// curated issue's for a promote step, a PR's for a merge step (#11900).
#[must_use]
pub fn ladder_due(rows: &[RestIssue]) -> Vec<(u32, StepRole, DateTime<Utc>)> {
    rows.iter()
        .filter_map(|r| {
            let role = if r.is_pull_request {
                StepRole::ChampionMerge
            } else {
                StepRole::ChampionPromote
            };
            let due = super::reeval_ladder::due(r.body.as_deref()?, role)?;
            Some((r.number, role, due))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The forge half
// ---------------------------------------------------------------------------

type MarkerCache = HashMap<(String, u32), (Option<String>, PullMarkers)>;
type BlockerCache = HashMap<(String, String), (Instant, bool)>;

fn marker_cache() -> &'static Mutex<MarkerCache> {
    static CACHE: OnceLock<Mutex<MarkerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn blocker_cache() -> &'static Mutex<BlockerCache> {
    static CACHE: OnceLock<Mutex<BlockerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The hold markers of the `loom:blocked` PRs among `rows`, from their
/// trusted comments, cached per `updated_at`.
fn pull_markers(slug: &str, root: &Path, rows: &[RestIssue]) -> HashMap<u32, PullMarkers> {
    let mut out = HashMap::new();
    let mut reads = 0usize;
    let mut cache = marker_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    for row in rows
        .iter()
        .filter(|r| r.is_pull_request && crate::work_finder::is_blocked_hold(&r.labels))
    {
        let key = (slug.to_string(), row.number);
        if let Some((at, markers)) = cache.get(&key) {
            if *at == row.updated_at {
                out.insert(row.number, markers.clone());
                continue;
            }
        }
        if reads >= MAX_MARKER_READS {
            continue;
        }
        reads += 1;
        let Some(trusted) = crate::comment_trust::records::fetch_trusted_comments(
            slug,
            &row.number.to_string(),
            root,
            true,
        ) else {
            continue;
        };
        let markers = PullMarkers::from_trusted(&TrustedComments::filtered(&policy, trusted));
        cache.insert(key, (row.updated_at.clone(), markers.clone()));
        out.insert(row.number, markers);
    }
    out
}

/// Whether one blocker is a closed issue or a merged PR; `None` unknown.
fn blocker_closed(slug: &str, root: &Path, repo: &str, number: u64) -> Option<bool> {
    let key = (slug.to_string(), format!("{repo}#{number}"));
    if let Some((at, closed)) = blocker_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        if at.elapsed() < BLOCKER_TTL {
            return Some(*closed);
        }
    }
    let v =
        crate::comment_trust::records::fetch_issue_object(repo, &number.to_string(), root, true)?;
    let closed = v.get("state").and_then(|s| s.as_str()) == Some("closed")
        && v.get("pull_request")
            .is_none_or(|pr| pr.get("merged_at").is_some_and(|m| !m.is_null()));
    blocker_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (Instant::now(), closed));
    Some(closed)
}

/// The curated, dependency-parked issues among `rows` whose every blocker is
/// closed or merged.
fn blockers_closed(slug: &str, root: &Path, rows: &[RestIssue]) -> HashSet<u32> {
    let open: HashSet<u32> = rows.iter().map(|r| r.number).collect();
    let mut reads = 0usize;
    let mut out = HashSet::new();
    for row in rows.iter().filter(|r| {
        !r.is_pull_request
            && has(&r.labels, CURATED)
            && crate::work_finder::is_blocked_hold(&r.labels)
    }) {
        let Some(body) = row.body.as_deref() else {
            continue;
        };
        if step_item::park_kind(&row.labels, Some(body)) != ParkKind::Dependency {
            continue;
        }
        let blockers: Vec<_> = crate::park_record::parse(body)
            .into_iter()
            .filter_map(|r| r.blocker)
            .collect();
        let all_closed = blockers.iter().all(|b| {
            let repo = b.repo.clone().unwrap_or_else(|| slug.to_string());
            let local = repo.eq_ignore_ascii_case(slug);
            if local && u32::try_from(b.number).is_ok_and(|n| open.contains(&n)) {
                return false;
            }
            if reads >= MAX_BLOCKER_READS {
                return false;
            }
            reads += 1;
            blocker_closed(slug, root, &repo, b.number).unwrap_or(false)
        });
        if !blockers.is_empty() && all_closed {
            out.insert(row.number);
        }
    }
    out
}

/// One workspace's items, read from the forge (module docs). `Ok(None)` when
/// the workspace has no forge target (a hermetic fixture). `main_red` gets
/// the repo's red-main-fix issues (trusted or promoted) and answers whether
/// `main` counts as red (`work_finder::main_red_fix::evaluate`, which also
/// runs the fix escalation and the auto-filed fix).
///
/// # Errors
/// The listing's: the set is all-or-nothing.
pub fn gather(
    root: &Path,
    registry: &Arc<Mutex<SweepRegistry>>,
    main_red: impl FnOnce(Vec<crate::work_finder::WorkItem>) -> bool,
    held_capabilities: &BTreeSet<String>,
    provider: &mut StageEntryProvider,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Gathered>> {
    let target = registry
        .lock()
        .ok()
        .and_then(|reg| reg.open_pr_listing_target());
    let Some((gh_bin, root_dir, slug)) = target else {
        return Ok(None);
    };
    let (rows, stale_listing) = listing_or_last_good(
        &slug,
        super::listing::list_all_open(&gh_bin, Some(&root_dir), Some(&slug)),
    )?;
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    let pull_listing = crate::forge_pull_listing::list_open_pulls_cached_as(
        super::listing::CALLER,
        &gh_bin,
        Some(&root_dir),
        Some(&slug),
        PULL_PAGES,
    );
    let pulls_complete = pull_listing.is_ok();
    let full: HashMap<u32, RestPull> = pull_listing
        .map(|pulls| pulls.into_iter().map(|p| (p.number, p)).collect())
        .unwrap_or_default();
    let markers = pull_markers(&slug, root, &rows);
    let pulls: Vec<PullInput> = rows
        .iter()
        .filter(|r| r.is_pull_request)
        .map(|r| {
            let m = markers.get(&r.number).cloned().unwrap_or_default();
            match full.get(&r.number) {
                Some(p) => PullInput::from_rest(p, &slug, &policy, false, m),
                None => pull_from_row(r, m),
            }
        })
        .collect();
    let linked_by_pr: Vec<u32> = pulls
        .iter()
        .flat_map(|p| {
            linked_issues(p.body.as_deref(), p.head_ref.as_deref(), p.same_repo, p.author_untrusted)
                .into_keys()
        })
        .collect();
    let ready: Vec<u32> = rows
        .iter()
        .filter(|r| !r.is_pull_request && has(&r.labels, ISSUE))
        .map(|r| r.number)
        .collect();
    let facts = IssueFacts {
        landed: crate::work_finder::landed_prefilter::landed_in(registry, &ready, &linked_by_pr)
            .into_iter()
            .collect(),
        blockers_closed: blockers_closed(&slug, root, &rows),
        red_fix: rows
            .iter()
            .filter(|r| !r.is_pull_request && has_red_fix_marker(r.body.as_deref()))
            .filter(|r| {
                has(&r.labels, ISSUE)
                    || policy.trusts(&crate::comment_trust::Author::new(
                        r.author.as_deref(),
                        r.author_association.as_deref(),
                    ))
            })
            .map(|r| r.number)
            .collect(),
    };
    let fixes = rows
        .iter()
        .filter(|r| facts.red_fix.contains(&r.number))
        .map(|r| {
            let mut item = crate::work_finder::WorkItem::with_created_at(
                r.number,
                r.labels.clone(),
                r.created_at.clone(),
            );
            item.body.clone_from(&r.body);
            item
        })
        .collect();
    let main_red = main_red(fixes);
    let inputs = build_inputs(
        &rows,
        pulls,
        &facts,
        RepoInput { main_red },
        HostInput {
            held_capabilities: held_capabilities.clone(),
        },
    );
    let items = step_item::classify(&inputs);
    let mut events = GhEventsReader;
    let draw = draw_inputs(items, &rows, |row, label| {
        provider.entry(&mut events, root, &slug, row, label, now)
    });
    if !stale_listing {
        provider.retain_present(&slug, &rows);
    }
    Ok(Some(Gathered {
        ladder: ladder_due(&rows),
        created_at: rows
            .iter()
            .filter_map(|r| r.created_at.clone().map(|c| (r.number, c)))
            .collect(),
        slug,
        inputs: draw,
        pulls_complete,
        stale_listing,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "gather_tests.rs"]
mod tests;
