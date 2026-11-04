//! One workspace's items for the step tick (#11909): list, join, classify,
//! and the facts the classifier and the draw read.
//!
//! The pure half ([`build_inputs`], [`draw_inputs`], [`ladder_due`]) turns
//! listing rows and forge facts into the classifier's
//! [`Inputs`](crate::work_finder::step_item::Inputs) and the draw's
//! [`DrawInput`]s. The forge half ([`gather`]) reads them, each read
//! bounded:
//!
//! - **the items, and the PRs' head and author**: the tick's one discovery
//!   snapshot ([`crate::discovery`], #12306); the gather lists nothing itself.
//!   The Direct backend lists per repo ([`super::listing::list_all_open`]
//!   and the ETag-cached open-PR listing), the Mirror backend reads one
//!   worklist for every repo. The last good copy of either stands in on a
//!   failure (#12074), so the Builder branch links as it does in the open-PR
//!   guard. When the PRs cannot be read, a PR row links by its body alone;
//! - **hold markers** on `loom:blocked` PRs: their trusted comments, cached
//!   per `(repo, PR, updated_at)`, at most [`MAX_MARKER_READS`] per tick;
//! - **landed** `loom:issue` items: the landed pre-filter's batched,
//!   cached verdicts (`work_finder::landed_prefilter`);
//! - **stage entry**: the provider (`super::stage_entry`), for stepped
//!   items only (a pass needs no age);
//! - **CI at a Judge step's head** (#12042): a Judge step whose PR head has
//!   CI pending becomes a [`PassReason::CiPending`] pass ([`hold_ci_pending`]),
//!   the step path's copy of the #12003 role-runner gate (`pr_planning`'s
//!   `has_judge_work`), with its rule (started under 6 h ago) and its per-head
//!   60 s cache. At most [`MAX_CI_READS`] uncached heads are read per repo per
//!   tick; the reads are conditional.
//!
//! A read that fails or runs out of budget leaves its fact at the value that
//! passes the item (a held PR without its marker reads as `blocked`): never a
//! step the forge did not vouch for.
//!
//! A dependency park's blockers are **not** read here (#12078): a curated,
//! dependency-parked issue is a Champion re-evaluate on every draw, and
//! Champion re-checks the blockers itself (`champion-targeted.md`, `--issue`
//! step 1).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::item_draw::DrawInput;
use super::stage_entry::{GhEventsReader, StageEntry, StageEntryProvider};
use crate::discovery::{Target, TickSnapshot};
use crate::forge_listing::RestIssue;
use crate::forge_pull_listing::RestPull;
use crate::step_role::StepRole;
use crate::sweep_registry::SweepRegistry;
use crate::work_finder::step_item::{
    self, HostInput, Inputs, IssueInput, IssueMarkers, Item, ItemState, LinkKind, MergedLink,
    ParkKind, PassReason, PullInput, PullMarkers, RepoInput, Step, TrustedComments, Verdict,
};
use crate::worktree_ops::linked_pr_listing::linked_issues;

/// At most this many PRs' comments are read for hold markers per repo per
/// tick.
pub const MAX_MARKER_READS: usize = 8;
/// Pages of the open-PR listing read per tick.
pub const PULL_PAGES: usize = 10;
/// At most this many Judge-step heads without a cached CI answer are read
/// per repo per tick (#12042). A head past the budget is drawn unread: Judge's
/// own pre-check still stands it down, and a `ci_pending` no-op only cools
/// down for the short window (`outcome_ledger::EXTERNAL_WAIT_NOOPS`).
pub const MAX_CI_READS: usize = 4;

const ISSUE: &str = concat!("loom", ":issue");
const CURATED: &str = concat!("loom", ":curated");

/// The per-issue facts read from the forge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueFacts {
    /// Issue → its merged closing PR (landed, not yet kept by Curator).
    pub landed: HashMap<u32, u32>,
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
    /// The same ladder records' rung and raw `next`, by item number (#12918).
    pub reevals: HashMap<u32, super::reeval_ladder::Reeval>,
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
/// every item dispatch everywhere; a short window rides it out. The
/// non-Builder claims (the role-keyed lease) re-read the forge; a Builder
/// claim is a blind label flip, so no Builder step is drawn from it
/// ([`Gathered::stale_listing`]).
pub const LAST_GOOD_TTL: Duration = Duration::from_secs(10 * 60);

type LastGood = HashMap<String, (Instant, Vec<RestIssue>)>;

fn last_good() -> &'static Mutex<LastGood> {
    static CACHE: OnceLock<Mutex<LastGood>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The listing, or on its failure the last good one while it is fresh.
fn listing_or_last_good(
    slug: &str,
    listing: anyhow::Result<Vec<RestIssue>>,
) -> anyhow::Result<(Vec<RestIssue>, bool)> {
    let mut cache = last_good()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match listing {
        Ok(rows) => {
            cache.insert(slug.to_string(), (Instant::now(), rows.clone()));
            Ok((rows, false))
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

type LastGoodPulls = HashMap<String, (Instant, Vec<RestPull>)>;

fn last_good_pulls() -> &'static Mutex<LastGoodPulls> {
    static CACHE: OnceLock<Mutex<LastGoodPulls>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The open-PR listing, or on its failure the last good one while it is
/// fresh (#12074), and whether this tick's listing answered. A shed listing
/// (a reader back-off) then keeps the PR set (heads for the CI hold, branch
/// links, authors) instead of emptying it. It never vouches for a Builder
/// step: the second value stays `false`, so [`super::driver::shape_pool`]
/// still holds Builders (#12025).
pub fn pulls_or_last_good(
    slug: &str,
    listing: anyhow::Result<Vec<RestPull>>,
) -> (Vec<RestPull>, bool) {
    let mut cache = last_good_pulls()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match listing {
        Ok(pulls) => {
            cache.insert(slug.to_string(), (Instant::now(), pulls.clone()));
            (pulls, true)
        }
        Err(_) => match cache.get(slug) {
            Some((at, pulls)) if at.elapsed() < LAST_GOOD_TTL => (pulls.clone(), false),
            _ => (Vec::new(), false),
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

/// The tracker marker (#10886): a living tracker, never drawn as work.
const TRACKER_MARKER: &str = "<!-- loom:tracker -->";

/// A body carries the tracker marker alone on a line, outside a code fence
/// (`label-state-machine.md` § "Tracker marker"); a quoted or inline mention
/// does not count.
#[must_use]
pub fn has_tracker_marker(body: Option<&str>) -> bool {
    body.is_some_and(|b| {
        crate::merge_pr::refs::strip_fenced_code_blocks(b)
            .lines()
            .any(|l| l.trim() == TRACKER_MARKER)
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
        draft: row.draft,
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
                tracker: has_tracker_marker(r.body.as_deref()),
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
/// Read at `now`: a marker written under the pre-#12513 ladder is due `now`
/// ([`super::reeval_ladder::Reeval::due_at`]).
#[must_use]
pub fn ladder_due(rows: &[RestIssue], now: DateTime<Utc>) -> Vec<(u32, StepRole, DateTime<Utc>)> {
    rows.iter()
        .filter_map(|r| {
            let role = if r.is_pull_request {
                StepRole::ChampionMerge
            } else {
                StepRole::ChampionPromote
            };
            let due = super::reeval_ladder::due(r.body.as_deref()?, role, now)?;
            Some((r.number, role, due))
        })
        .collect()
}

/// The ladder record each row's body carries, by number (#12918): the one
/// [`ladder_due`] reads (a PR's `champion_merge`, an issue's
/// `champion_promote`), with its rung and its raw `next`. Read at `now` for
/// the choice between several records, as [`ladder_due`] does.
#[must_use]
pub fn ladder_state(
    rows: &[RestIssue],
    now: DateTime<Utc>,
) -> HashMap<u32, super::reeval_ladder::Reeval> {
    rows.iter()
        .filter_map(|r| {
            let role = if r.is_pull_request {
                StepRole::ChampionMerge
            } else {
                StepRole::ChampionPromote
            };
            let record = super::reeval_ladder::parse(r.body.as_deref()?)
                .into_iter()
                .filter(|e| e.step == role)
                .max_by_key(|e| e.due_at(now))?;
            Some((r.number, record))
        })
        .collect()
}

/// Turn every Judge step whose PR head has CI pending into a
/// [`PassReason::CiPending`] pass (#12042), and return how many it held.
/// `heads` maps a PR to its head SHA (a PR with none is drawn unread);
/// `known` is a cached answer, free; `read` asks the forge and is called at
/// most `max_reads` times. A head past the budget is drawn. Pure.
pub fn hold_ci_pending(
    items: &mut [Item],
    heads: &HashMap<u32, String>,
    max_reads: usize,
    mut known: impl FnMut(&str) -> Option<bool>,
    mut read: impl FnMut(&str) -> bool,
) -> usize {
    let mut reads = 0usize;
    let mut held = 0usize;
    for item in items.iter_mut() {
        if item.step() != Some(Step::Judge) {
            continue;
        }
        let Some(head) = item.target().and_then(|pr| heads.get(&pr)) else {
            continue;
        };
        let pending = known(head).unwrap_or_else(|| {
            if reads < max_reads {
                reads += 1;
                read(head)
            } else {
                false
            }
        });
        if pending {
            item.verdict = Verdict::Pass(PassReason::CiPending);
            held += 1;
        }
    }
    held
}

/// Whether a head's snapshot check summary holds a Judge step (#12317): the
/// same rule as `ci_pending_at_sha` (pending, and started inside the 6-hour
/// window of #12003), evaluated on the summary's newest run time. A
/// `success` or `failure` summary never holds; a pending one with no usable
/// start stamp counts as stuck, as the live read does. Pure.
#[must_use]
pub fn summary_pending(checks: &crate::discovery::Checks, now: DateTime<Utc>) -> bool {
    checks.state.eq_ignore_ascii_case("pending")
        && crate::judge_backstop::pending_exempts(
            std::slice::from_ref(&checks.started_at.clone().unwrap_or_default()),
            now,
        )
}

/// Each PR's head → whether its snapshot check summary holds a Judge step.
/// A head the snapshot carries no summary for is absent (the caller falls
/// back to a conditional read). Pure.
#[must_use]
pub fn summary_verdicts(
    heads: &HashMap<u32, String>,
    extras: &std::collections::BTreeMap<u32, crate::discovery::ItemExtras>,
    now: DateTime<Utc>,
) -> HashMap<String, bool> {
    heads
        .iter()
        .filter_map(|(n, sha)| {
            let checks = extras.get(n)?.checks.as_ref()?;
            Some((sha.clone(), summary_pending(checks, now)))
        })
        .collect()
}

/// Turn every [`Step::ChampionHoldReevaluate`] whose PR is still at the head
/// its hold recorded into a [`PassReason::HeldAtHead`] pass (#12900), and
/// return how many it held. Such a run re-reads the same head and ends
/// `held`, a no-op that occupies the repo's `champion_merge` slot.
/// `hold_heads` maps a PR to [`PullMarkers::hold_head`] (already `None` once a
/// release comment followed); `released` names the PRs carrying
/// `loom:auto-merge-ok`. Fails open: a PR with no recorded or current head is
/// drawn. A recorded SHA may be abbreviated, so heads match on prefix.
///
/// A change in the repo's standing authorizations is a release signal too
/// (#12933): `hold_auths` maps a PR to the digest its hold recorded
/// ([`PullMarkers::hold_auth`]) and `current_auth` is the repo's digest now.
/// A hold whose recorded digest differs is drawn once; its re-hold records
/// the current digest. A hold that recorded none (every merge-risk hold, and
/// a legacy critical-file one) keeps the head-only rule (#13008): nothing
/// re-records a digest on the merge-risk path, so drawing it would redraw it
/// on every tick. `current_auth` of `None` (no digest available) keeps the
/// head-only rule too. Pure.
pub fn hold_at_recorded_head(
    items: &mut [Item],
    heads: &HashMap<u32, String>,
    hold_heads: &HashMap<u32, String>,
    hold_auths: &HashMap<u32, String>,
    current_auth: Option<&str>,
    released: &HashSet<u32>,
) -> usize {
    let mut held = 0usize;
    for item in items.iter_mut() {
        if item.step() != Some(Step::ChampionHoldReevaluate) {
            continue;
        }
        let Some(pr) = item.target() else { continue };
        if released.contains(&pr) {
            continue;
        }
        let (Some(head), Some(recorded)) = (heads.get(&pr), hold_heads.get(&pr)) else {
            continue;
        };
        let moved = current_auth.is_some_and(|cur| {
            hold_auths
                .get(&pr)
                .is_some_and(|recorded| recorded.as_str() != cur)
        });
        if moved {
            continue;
        }
        if !recorded.is_empty() && (head.starts_with(recorded) || recorded.starts_with(head)) {
            item.verdict = Verdict::Pass(PassReason::HeldAtHead);
            held += 1;
        }
    }
    held
}

/// Turn every [`Step::ChampionPromote`] whose issue the promotion author gate
/// already holds into a [`PassReason::AuthorGateHeld`] pass (#13142), and
/// return how many it held. Such a draw is a full Champion session that finds
/// the issue still gated and changes nothing. `gates` maps an issue to the
/// [`Stamp`]s of its trusted gate comments; `revs` to the revision of its
/// current title and body. The issue is held while one stamp names that
/// revision and the current trust digest: an edit, a trust change, or a
/// legacy (unstamped) notice makes it drawn once, and its re-gate posts a
/// notice with the new stamp. A trusted `loom:issue` leaves the
/// `ChampionPromote` queue by itself. Fails open: an issue with no readable
/// stamp, revision or trust digest is drawn. Pure.
pub fn hold_author_gated(
    items: &mut [Item],
    gates: &HashMap<u32, Vec<crate::promotion_author::Stamp>>,
    revs: &HashMap<u32, String>,
    trust: Option<&str>,
) -> usize {
    let Some(trust) = trust else { return 0 };
    let mut held = 0usize;
    for item in items.iter_mut() {
        if item.step() != Some(Step::ChampionPromote) {
            continue;
        }
        let Some(n) = item.target() else { continue };
        let (Some(stamps), Some(rev)) = (gates.get(&n), revs.get(&n)) else {
            continue;
        };
        if stamps.iter().any(|s| s.rev == *rev && s.trust == trust) {
            item.verdict = Verdict::Pass(PassReason::AuthorGateHeld);
            held += 1;
        }
    }
    held
}

// ---------------------------------------------------------------------------
// The forge half
// ---------------------------------------------------------------------------

type MarkerCache = HashMap<(String, u32), (Option<String>, PullMarkers)>;

fn marker_cache() -> &'static Mutex<MarkerCache> {
    static CACHE: OnceLock<Mutex<MarkerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The hold markers of the `loom:blocked` PRs among `rows`, from their
/// trusted comments, cached per `updated_at`, and how many comment reads that
/// took (`loom.discovery.live_reads.markers`, #12317).
fn pull_markers(slug: &str, root: &Path, rows: &[RestIssue]) -> (HashMap<u32, PullMarkers>, u64) {
    let mut out = HashMap::new();
    let mut reads = 0usize;
    let cache = || {
        marker_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    // #12310: the hold-marker comment reads are discovery.
    let _discovery = crate::gh_invocation::purpose::discovery();
    for row in rows
        .iter()
        .filter(|r| r.is_pull_request && crate::work_finder::is_blocked_hold(&r.labels))
    {
        let key = (slug.to_string(), row.number);
        // The lock is held only for the lookup and the insert, never across
        // the comment read: several repos are gathered at once (#12256).
        let cached = cache()
            .get(&key)
            .filter(|(at, _)| *at == row.updated_at)
            .map(|(_, m)| m.clone());
        if let Some(markers) = cached {
            out.insert(row.number, markers);
            continue;
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
        cache().insert(key, (row.updated_at.clone(), markers.clone()));
        out.insert(row.number, markers);
    }
    (out, reads as u64)
}

type GateCache = HashMap<(String, u32), (Option<String>, Vec<crate::promotion_author::Stamp>)>;

fn gate_cache() -> &'static Mutex<GateCache> {
    static CACHE: OnceLock<Mutex<GateCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The gate stamps of the trusted comments on the curated issues among
/// `numbers` (the `ChampionPromote` candidates), cached per `updated_at`, at
/// most [`MAX_MARKER_READS`] reads per tick. An issue whose comments cannot
/// be read, or are past the budget, is absent: it is drawn.
fn gate_stamps(
    slug: &str,
    root: &Path,
    rows: &[RestIssue],
    numbers: &[u32],
) -> HashMap<u32, Vec<crate::promotion_author::Stamp>> {
    let mut out = HashMap::new();
    let mut reads = 0usize;
    let cache = || {
        gate_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    // #12310: the gate-marker comment reads are discovery.
    let _discovery = crate::gh_invocation::purpose::discovery();
    for row in rows
        .iter()
        .filter(|r| numbers.contains(&r.number) && r.comments > 0)
    {
        let key = (slug.to_string(), row.number);
        let cached = cache()
            .get(&key)
            .filter(|(at, _)| *at == row.updated_at)
            .map(|(_, s)| s.clone());
        if let Some(stamps) = cached {
            out.insert(row.number, stamps);
            continue;
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
        let stamps: Vec<_> = TrustedComments::filtered(&policy, trusted)
            .bodies()
            .iter()
            .filter_map(|b| crate::promotion_author::Stamp::parse(b))
            .collect();
        cache().insert(key, (row.updated_at.clone(), stamps.clone()));
        out.insert(row.number, stamps);
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
    snapshot: &TickSnapshot,
    main_red: impl FnOnce(Vec<crate::work_finder::WorkItem>) -> bool,
    held_capabilities: &BTreeSet<String>,
    provider: &mut StageEntryProvider,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Gathered>> {
    Ok(list(root, registry, snapshot, main_red, held_capabilities)?
        .map(|l| finish(l, root, provider, now)))
}

/// One workspace's forge half of [`gather`] (#12256): the listings, the
/// facts and the classification, everything but the stage-entry lookups.
/// It touches no state shared across repos but the process caches (each
/// behind its own lock), so the driver runs it for several repos at once.
#[derive(Debug, Clone)]
pub struct Listed {
    pub slug: String,
    rows: Vec<RestIssue>,
    items: Vec<Item>,
    pulls_complete: bool,
    stale_listing: bool,
}

/// [`gather`]'s forge half (module docs, [`Listed`]).
///
/// # Errors
/// The listing's: the set is all-or-nothing.
pub fn list(
    root: &Path,
    registry: &Arc<Mutex<SweepRegistry>>,
    snapshot: &TickSnapshot,
    main_red: impl FnOnce(Vec<crate::work_finder::WorkItem>) -> bool,
    held_capabilities: &BTreeSet<String>,
) -> anyhow::Result<Option<Listed>> {
    let target = registry
        .lock()
        .ok()
        .and_then(|reg| reg.open_pr_listing_target());
    let Some((gh_bin, root_dir, slug)) = target else {
        return Ok(None);
    };
    // The tick's one snapshot is the only place the items come from (#12306).
    // #12310: the open-PR listing is discovery.
    let repo_snapshot = {
        let _discovery = crate::gh_invocation::purpose::discovery();
        snapshot.repo(&Target {
            slug: slug.clone(),
            gh_bin: gh_bin.clone(),
            root_dir: root_dir.clone(),
        })
    };
    let (rows, stale_listing) = listing_or_last_good(
        &slug,
        repo_snapshot
            .as_ref()
            .map(|r| r.rows.clone())
            .map_err(|e| anyhow::anyhow!("{e:#}")),
    )?;
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    // #12317: a worklist repo's per-PR check summaries.
    let summaries = repo_snapshot
        .as_ref()
        .ok()
        .filter(|r| r.source == crate::discovery::Source::Worklist)
        .map(|r| r.extras.clone());
    let pull_listing = repo_snapshot
        .ok()
        .and_then(|r| r.pulls.clone())
        .ok_or_else(|| anyhow::anyhow!("open-PR listing unavailable"));
    let (pulls, pulls_complete) = pulls_or_last_good(&slug, pull_listing);
    let full: HashMap<u32, RestPull> = pulls.into_iter().map(|p| (p.number, p)).collect();
    let (markers, marker_reads) = pull_markers(&slug, root, &rows);
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
    let hold_heads: HashMap<u32, String> = pulls
        .iter()
        .filter_map(|p| p.markers.hold_head.clone().map(|h| (p.number, h)))
        .collect();
    let hold_auths: HashMap<u32, String> = pulls
        .iter()
        .filter_map(|p| p.markers.hold_auth.clone().map(|a| (p.number, a)))
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
    let (landed, landed_reads) =
        crate::work_finder::landed_prefilter::landed_in_counted(registry, &ready, &linked_by_pr);
    let facts = IssueFacts {
        landed: landed.into_iter().collect(),
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
    let mut items = step_item::classify(&inputs);
    let heads: HashMap<u32, String> = full
        .iter()
        .filter_map(|(n, p)| p.head_sha.clone().map(|sha| (*n, sha)))
        .collect();
    // #12317: on a worklist repo the snapshot's check summary answers; a
    // head with none is read once (conditionally) and counted.
    let verdicts = summaries
        .as_ref()
        .map(|extras| summary_verdicts(&heads, extras, Utc::now()));
    let ci_fallbacks = std::cell::Cell::new(0u64);
    let held = hold_ci_pending(
        &mut items,
        &heads,
        MAX_CI_READS,
        |sha| {
            verdicts
                .as_ref()
                .and_then(|v| v.get(sha).copied())
                .or_else(|| crate::pr_planning::ci_pending_cached(&root_dir, sha))
        },
        |sha| {
            if verdicts.is_some() {
                ci_fallbacks.set(ci_fallbacks.get() + 1);
            }
            crate::pr_planning::ci_pending_at_sha(&root_dir, &gh_bin, sha)
        },
    );
    snapshot.note_live_reads(landed_reads, marker_reads, ci_fallbacks.get());
    if held > 0 {
        log::debug!("work_finder: {slug}: {held} Judge step(s) held while CI runs at the head");
    }
    let released: HashSet<u32> = rows
        .iter()
        .filter(|r| r.is_pull_request && has(&r.labels, "loom:auto-merge-ok"))
        .map(|r| r.number)
        .collect();
    let at_head = hold_at_recorded_head(
        &mut items,
        &heads,
        &hold_heads,
        &hold_auths,
        Some(crate::champion_hold::standing_auth_digest(root).as_str()),
        &released,
    );
    if at_head > 0 {
        log::debug!("work_finder: {slug}: {at_head} held PR(s) unchanged since their hold");
    }
    let promote: Vec<u32> = items
        .iter()
        .filter(|i| i.step() == Some(Step::ChampionPromote))
        .filter_map(Item::target)
        .collect();
    if !promote.is_empty() {
        let revs: HashMap<u32, String> = rows
            .iter()
            .filter(|r| promote.contains(&r.number))
            .map(|r| {
                let rev = crate::promotion_author::revision(
                    r.title.as_deref().unwrap_or_default(),
                    r.body.as_deref().unwrap_or_default(),
                );
                (r.number, rev)
            })
            .collect();
        let gated = hold_author_gated(
            &mut items,
            &gate_stamps(&slug, root, &rows, &promote),
            &revs,
            Some(policy.digest().as_str()),
        );
        if gated > 0 {
            log::debug!("work_finder: {slug}: {gated} curated issue(s) held by the author gate");
        }
    }
    Ok(Some(Listed {
        slug,
        rows,
        items,
        pulls_complete,
        stale_listing,
    }))
}

/// [`gather`]'s serial half: the stage-entry lookups (one provider, one
/// per-tick events budget, spent in the driver's walk order so it is
/// deterministic) and the draw inputs. Its events reads are bounded by the
/// provider's budget.
pub fn finish(
    listed: Listed,
    root: &Path,
    provider: &mut StageEntryProvider,
    now: DateTime<Utc>,
) -> Gathered {
    let Listed {
        slug,
        rows,
        items,
        pulls_complete,
        stale_listing,
    } = listed;
    let mut events = GhEventsReader;
    let draw = draw_inputs(items, &rows, |row, label| {
        provider.entry(&mut events, root, &slug, row, label, now)
    });
    if !stale_listing {
        provider.retain_present(&slug, &rows);
    }
    Gathered {
        ladder: ladder_due(&rows, now),
        reevals: ladder_state(&rows, now),
        created_at: rows
            .iter()
            .filter_map(|r| r.created_at.clone().map(|c| (r.number, c)))
            .collect(),
        slug,
        inputs: draw,
        pulls_complete,
        stale_listing,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "gather_tests.rs"]
mod tests;
