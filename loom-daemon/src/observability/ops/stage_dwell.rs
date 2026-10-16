//! Forge label-stage dwell (Issue #8929, part 2): how long issues and PRs sit
//! in the Loom stages the work finder never lists.
//!
//! # Rate-limit posture
//!
//! On the collector's `host.health` cadence (5 minutes), and only when the
//! OTLP ops sink is registered, each managed repo's open items are listed for
//! the [`STAGE_LABELS`] with the ETag-cached REST listing
//! ([`crate::forge_listing::list_issues_cached`]). An unchanged listing is a
//! free `304`. Per-item reads are rare and bounded:
//!
//! - one `issues/{n}/events` read gives an item's latest `labeled` times
//!   ([`super::stage_events`]: page 1, then back from the last page, each
//!   page one read), and each found time is cached for the life of the
//!   process;
//! - one `pulls/{n}` read tells whether a PR that left review merged.
//!
//! Both come out of one shared budget of [`FETCH_BUDGET`] reads per sample
//! across all repos; sampling reads come first, cache warm-up only spends
//! what is left. Work over the budget waits for the next sample, and nothing
//! is ever read per tick or per issue per tick. At most
//! [`super::stage_events::MAX_PAGES`] events pages are read per item, so an
//! item whose latest label event lies further back than that has no time
//! and is not sampled; an earlier application is never taken for it
//! (#11924). The reads run on a detached task, so a slow forge
//! never stalls the collector; a sample still running when the next is due
//! makes that next one a no-op.
//!
//! # Stages (`state` label)
//!
//! | `state` | sampled when | seconds |
//! |---|---|---|
//! | `created_to_curated` | an issue newly appears under `loom:curated` | `loom:curated` labeled − created |
//! | `curated_to_issue` | a `loom:curated` issue newly appears under `loom:issue` or `loom:building` (`loom:curated` stays on) | promotion observed − `loom:curated` labeled |
//! | `building_to_review_requested` | a PR newly under a review label closes an issue with a known `loom:building` time | PR created − `loom:building` labeled |
//! | `review_requested_to_merged` | a PR leaves the review labels (`loom:review-requested`, `loom:changes-requested`, `loom:pr`) and merged | merged − PR created |
//!
//! `curated_to_issue` is resolved to one sample interval. The first sample
//! after start only records a baseline, so a restart never replays history
//! as new entries. Every host that manages a repo samples it independently:
//! sums scale with the host count, the mean (`dwell / samples`) does not.
//! `loom.forge.stage_items{state, repo}` counts the open items under each
//! stage label per repo, plus the queue states of [`super::stage_queue`]
//! (`triage`, `unlabeled`, …) from one open-items walk per repo (#11847).
//!
//! # One sampler per fleet (W12)
//!
//! With `fleet.captainGauges` configured, the declared fleet captain samples
//! (`stage-dwell` is one of its singleton jobs) and a dispatcher that opted
//! in skips every repo the captain's fresh heartbeat covers: no listing and
//! no per-item read for it, and its sampler state for the repo is dropped,
//! so taking the repo back after a stale heartbeat starts from a baseline.
//! See [`crate::observability::captain_gauges`]. When enabled and with no
//! captain configured, every host samples as before.
//!
//! # Off by default (#12203)
//!
//! Every host sampling every repo cost about 8.9k billable GitHub calls an
//! hour fleet-wide and tripped `rate_limit_breaker`, which halts dispatch. So
//! the whole sampler sits behind [`ENABLED_KEY`]
//! (`observability.stageDwell.enabled`, default `false`): off, a pass makes no
//! listing, no per-item read and no `merged_at` read, emits no
//! `loom.forge.stage_dwell` / `loom.forge.stage_items` point, and the captain
//! does not arm the `stage-dwell` job. [`INTERVAL_KEY`]
//! (`observability.stageDwell.intervalSecs`) spaces passes further apart than
//! the collector's forge-gather period. Both are re-read every pass (live).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use chrono::{DateTime, Utc};

use super::stage_events::LabelTimes;
use crate::forge_listing::RestIssue;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::workspace_pool::WorkspacePool;

pub const CURATED: &str = "loom:curated";
pub const ISSUE: &str = "loom:issue";
pub const BUILDING: &str = "loom:building";
pub const REVIEW_REQUESTED: &str = "loom:review-requested";
pub const CHANGES_REQUESTED: &str = "loom:changes-requested";
pub const PR_APPROVED: &str = "loom:pr";

/// Every label listed per repo per sample.
pub const STAGE_LABELS: [&str; 6] = [
    CURATED,
    ISSUE,
    BUILDING,
    REVIEW_REQUESTED,
    CHANGES_REQUESTED,
    PR_APPROVED,
];

/// The labels a PR is "in review" under.
pub const REVIEW_LABELS: [&str; 3] = [REVIEW_REQUESTED, CHANGES_REQUESTED, PR_APPROVED];

/// Per-item forge reads allowed per sample, across all repos (each events
/// page is one).
pub const FETCH_BUDGET: usize = 8;

/// Samples a pending PR lookup is retried before it is dropped.
pub const MAX_TRIES: u8 = 3;
/// Config key: the sampler's master switch (#12203). Default `false`.
pub const ENABLED_KEY: &str = "observability.stageDwell.enabled";
/// Config key: the minimum seconds between two samples (#12203). Unset or
/// non-positive: every forge-gather pass.
pub const INTERVAL_KEY: &str = "observability.stageDwell.intervalSecs";

/// The resolved `observability.stageDwell` block, re-read every pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// `enabled` (default `false`).
    pub enabled: bool,
    /// `intervalSecs`, when positive.
    pub interval_secs: Option<u64>,
}

impl Settings {
    /// Resolve from an effective config. A missing or malformed value is its
    /// default: off, no extra spacing.
    #[must_use]
    pub fn from_effective(effective: &serde_json::Value) -> Self {
        let get = |key: &str| crate::config_resolver::get_path(effective, key);
        Self {
            enabled: get(ENABLED_KEY)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            interval_secs: get(INTERVAL_KEY)
                .and_then(serde_json::Value::as_u64)
                .filter(|s| *s > 0),
        }
    }

    /// Whether a pass at `now` samples: switched on, and `intervalSecs` (if
    /// set) has elapsed since `last` began. Pure.
    #[must_use]
    pub fn due(&self, last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        match (self.interval_secs, last) {
            (Some(secs), Some(last)) => {
                (now - last).num_seconds() >= i64::try_from(secs).unwrap_or(i64::MAX)
            }
            _ => true,
        }
    }
}

/// When the last sampling pass began (for [`INTERVAL_KEY`]).
static LAST_PASS: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);
/// The switch state last logged: 0 none yet, 1 on, 2 off.
static LOGGED_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Log the switch at INFO on the first pass and whenever it flips.
fn log_switch(enabled: bool) {
    let state = if enabled { 1 } else { 2 };
    if LOGGED_STATE.swap(state, std::sync::atomic::Ordering::Relaxed) == state {
        return;
    }
    if enabled {
        log::info!("stage-dwell sampler on ({ENABLED_KEY}=true)");
    } else {
        log::info!(
            "stage-dwell sampler off: no stage listings or loom.forge.stage_dwell gauges \
             (set {ENABLED_KEY}=true to sample; #12203)"
        );
    }
}

/// Gate one pass: log the switch, and claim the pass when it is due. `false`
/// means the pass makes no forge call at all.
fn claim_pass(settings: Settings, now: DateTime<Utc>) -> bool {
    log_switch(settings.enabled);
    let mut last = match LAST_PASS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !settings.due(*last, now) {
        return false;
    }
    *last = Some(now);
    true
}

/// This sampler's fleet-captain singleton job (W12,
/// [`crate::observability::captain_gauges`]).
const STAGE_JOB: &str = crate::observability::captain_gauges::STAGE_DWELL_JOB;

/// Timeout for one per-item `gh api` read.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// One listed item, reduced to what the sampler needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageItem {
    pub number: u32,
    pub created_at: Option<DateTime<Utc>>,
    /// Issues a PR's body closes (`Closes #N`, `Fixes #N`, `Resolves #N`).
    pub closes: Vec<u32>,
    /// REST issue listings include PRs; stages are issue-only or PR-only.
    pub is_pr: bool,
}

impl StageItem {
    #[must_use]
    pub fn from_rest(item: &RestIssue) -> Self {
        StageItem {
            number: item.number,
            created_at: item.created_at.as_deref().and_then(parse_time),
            is_pr: item.is_pull_request,
            closes: if item.is_pull_request {
                closing_refs(item.body.as_deref().unwrap_or(""))
            } else {
                Vec::new()
            },
        }
    }
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The issue numbers `body` closes with a GitHub closing keyword.
#[must_use]
pub fn closing_refs(body: &str) -> Vec<u32> {
    const KEYWORDS: [&str; 9] = [
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let words: Vec<String> = body
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let mut refs = Vec::new();
    for pair in words.windows(2) {
        let keyword = pair[0].trim_end_matches(':');
        if !KEYWORDS.contains(&keyword) {
            continue;
        }
        let digits: String = pair[1]
            .strip_prefix('#')
            .unwrap_or("")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(number) = digits.parse::<u32>() {
            if !refs.contains(&number) {
                refs.push(number);
            }
        }
    }
    refs
}

/// One repo's listings for this sample: open items per stage label.
#[derive(Debug, Clone, Default)]
pub struct RepoInput {
    pub slug: String,
    pub root: PathBuf,
    pub listings: BTreeMap<&'static str, Vec<StageItem>>,
    /// The [`super::stage_queue`] state counts from this sample's open-items
    /// walk; `None` when the walk did not complete (those states are absent).
    pub queue: Option<BTreeMap<&'static str, usize>>,
}

impl RepoInput {
    /// The issues (not PRs) under `label`.
    fn numbers(&self, label: &str) -> BTreeSet<u32> {
        self.listings
            .get(label)
            .map(|items| {
                items
                    .iter()
                    .filter(|i| !i.is_pr)
                    .map(|i| i.number)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn review(&self) -> BTreeMap<u32, &StageItem> {
        REVIEW_LABELS
            .iter()
            .filter_map(|label| self.listings.get(label))
            .flatten()
            .filter(|item| item.is_pr)
            .map(|item| (item.number, item))
            .collect()
    }
}

/// The per-item forge reads, injected so the sampler is testable offline.
pub trait StageFetcher {
    /// The latest `labeled` time of `label` on issue/PR `number`, and of
    /// every other label the pages read vouch for, in at most `max_pages`
    /// page reads ([`super::stage_events::walk`]).
    fn label_times(
        &mut self,
        root: &Path,
        slug: &str,
        number: u32,
        label: &str,
        max_pages: usize,
    ) -> LabelTimes;
    /// When PR `number` merged (`Some(None)`: not merged), or `None` when the
    /// read failed.
    fn merged_at(&mut self, root: &Path, slug: &str, number: u32) -> Option<Option<DateTime<Utc>>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPr {
    created_at: DateTime<Utc>,
    closes: Vec<u32>,
    tries: u8,
}

#[derive(Debug, Default)]
struct RepoState {
    curated: BTreeSet<u32>,
    /// Issues under `loom:issue` or `loom:building` last sample.
    promoted: BTreeSet<u32>,
    /// Curated issues promoted after the baseline, not yet sampled, with the
    /// instant the promotion was observed.
    new_promoted: BTreeMap<u32, DateTime<Utc>>,
    building: BTreeSet<u32>,
    review: BTreeMap<u32, DateTime<Utc>>,
    /// Issues that entered `loom:curated` after the baseline, not yet sampled.
    new_curated: BTreeMap<u32, DateTime<Utc>>,
    /// PRs that entered review after the baseline, not yet sampled.
    new_prs: BTreeMap<u32, PendingPr>,
    /// PRs that left review, awaiting their merge check.
    left_review: BTreeMap<u32, PendingPr>,
}

/// The sampler's state across samples.
#[derive(Debug, Default)]
pub struct Sampler {
    repos: HashMap<String, RepoState>,
    /// `(slug, number, label)` → labeled time; `None` = read, not found.
    label_times: HashMap<(String, u32, String), Option<DateTime<Utc>>>,
    last_at: Option<DateTime<Utc>>,
    /// When each repo was last sampled (its listing was complete).
    sampled_at: HashMap<String, DateTime<Utc>>,
    /// A repo unsampled for longer than this is baselined instead of diffed
    /// ([`Self::set_replay_bound`]). `None`: always diff.
    replay_bound: Option<chrono::Duration>,
}

/// One dwell sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwellSample {
    pub stage: &'static str,
    pub seconds: i64,
}

impl Sampler {
    /// `label` time for `number`, from the cache or (budget permitting) one
    /// events read that also caches every other label time it returns.
    fn label_time(
        &mut self,
        fetcher: &mut impl StageFetcher,
        budget: &mut usize,
        input: &RepoInput,
        number: u32,
        label: &str,
    ) -> Option<DateTime<Utc>> {
        let key = (input.slug.clone(), number, label.to_string());
        if let Some(cached) = self.label_times.get(&key) {
            return *cached;
        }
        if *budget == 0 {
            return None;
        }
        let read = fetcher.label_times(&input.root, &input.slug, number, label, *budget);
        *budget = budget.saturating_sub(read.pages.max(1));
        let times = read.times?;
        for (name, at) in &times {
            self.label_times
                .insert((input.slug.clone(), number, name.clone()), Some(*at));
        }
        let found = times.get(label).copied();
        self.label_times.insert(key, found);
        found
    }

    /// Bound how long a repo may go unsampled before its next sample is a
    /// baseline (W12). The fleet captain sets its heartbeat's staleness bound:
    /// once the captain has lost a repo for that long, every dispatcher has
    /// fallen back, baselined and emitted the repo's transitions itself, so
    /// diffing against the captain's pre-outage state would emit them a
    /// second time. `None` (every other host) keeps catching up after an
    /// outage, as before.
    pub fn set_replay_bound(&mut self, bound: Option<chrono::Duration>) {
        self.replay_bound = bound;
    }

    /// Take one sample of every repo in `inputs` at `now`. A repo absent from
    /// `inputs` (its listing failed) keeps its state untouched, unless a
    /// replay bound says it is too old to keep ([`Self::set_replay_bound`]).
    pub fn sample(
        &mut self,
        inputs: &[RepoInput],
        now: DateTime<Utc>,
        fetcher: &mut impl StageFetcher,
    ) -> Vec<DwellSample> {
        let mut budget = FETCH_BUDGET;
        let mut samples = Vec::new();
        for input in inputs {
            // Too long since this repo's last good sample: its state is not a
            // base to diff against any more (see `set_replay_bound`).
            let outage = self
                .replay_bound
                .zip(self.sampled_at.get(&input.slug))
                .is_some_and(|(bound, last)| now - *last > bound);
            if outage {
                // Only the diff base: label times are facts and stay cached.
                self.repos.remove(&input.slug);
            }
            self.sampled_at.insert(input.slug.clone(), now);
            let baseline = !self.repos.contains_key(&input.slug);
            let mut state = self.repos.remove(&input.slug).unwrap_or_default();
            self.sample_repo(&mut state, input, baseline, now, fetcher, &mut budget, &mut samples);
            self.repos.insert(input.slug.clone(), state);
        }
        // Spare budget only, after every repo's sampling reads: warm the
        // curated times of issues already present, so their promotion can
        // be sampled without a read later.
        for input in inputs {
            for number in input.numbers(CURATED) {
                if budget == 0 {
                    break;
                }
                self.label_time(fetcher, &mut budget, input, number, CURATED);
            }
        }
        self.last_at = Some(now);
        samples
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_repo(
        &mut self,
        state: &mut RepoState,
        input: &RepoInput,
        baseline: bool,
        now: DateTime<Utc>,
        fetcher: &mut impl StageFetcher,
        budget: &mut usize,
        samples: &mut Vec<DwellSample>,
    ) {
        let curated = input.numbers(CURATED);
        let building = input.numbers(BUILDING);
        let promoted: BTreeSet<u32> = input.numbers(ISSUE).union(&building).copied().collect();
        let review = input.review();

        // PRs that left review: queue a merge check (first, as it is the
        // most time-sensitive read).
        for (number, created_at) in &state.review {
            if !review.contains_key(number) {
                let closes = Vec::new();
                state.left_review.insert(
                    *number,
                    PendingPr {
                        created_at: *created_at,
                        closes,
                        tries: 0,
                    },
                );
            }
        }
        let mut still_left = BTreeMap::new();
        for (number, mut pending) in std::mem::take(&mut state.left_review) {
            if review.contains_key(&number) {
                continue; // back under review (e.g. relabelled after Doctor)
            }
            if *budget == 0 {
                still_left.insert(number, pending);
                continue;
            }
            *budget -= 1;
            match fetcher.merged_at(&input.root, &input.slug, number) {
                Some(Some(merged_at)) => samples.push(DwellSample {
                    stage: "review_requested_to_merged",
                    seconds: (merged_at - pending.created_at).num_seconds().max(0),
                }),
                Some(None) => {}
                None => {
                    pending.tries += 1;
                    if pending.tries < MAX_TRIES {
                        still_left.insert(number, pending);
                    }
                }
            }
        }
        state.left_review = still_left;

        // PRs newly under review: building → review-requested.
        if !baseline {
            for (number, item) in &review {
                if state.review.contains_key(number) {
                    continue;
                }
                if let Some(created_at) = item.created_at {
                    state.new_prs.insert(
                        *number,
                        PendingPr {
                            created_at,
                            closes: item.closes.clone(),
                            tries: 0,
                        },
                    );
                }
            }
        }
        let mut still_new = BTreeMap::new();
        for (number, mut pending) in std::mem::take(&mut state.new_prs) {
            let issue = pending
                .closes
                .iter()
                .copied()
                .find(|n| building.contains(n));
            let Some(issue) = issue else {
                continue; // closes no issue under `loom:building`: not this stage
            };
            match self.label_time(fetcher, budget, input, issue, BUILDING) {
                Some(building_at) => samples.push(DwellSample {
                    stage: "building_to_review_requested",
                    seconds: (pending.created_at - building_at).num_seconds().max(0),
                }),
                // Out of budget is not a failed read: retry, uncounted.
                None if *budget == 0 => {
                    still_new.insert(number, pending);
                }
                None => {
                    pending.tries += 1;
                    if pending.tries < MAX_TRIES {
                        still_new.insert(number, pending);
                    }
                }
            }
        }
        state.new_prs = still_new;

        // Curated issues newly promoted to the ready queue or a build.
        // `loom:curated` is additive (it stays on after promotion), so the
        // transition is the entry into `loom:issue` ∪ `loom:building`.
        if !baseline {
            for number in promoted.difference(&state.promoted) {
                if curated.contains(number) {
                    state.new_promoted.insert(*number, now);
                }
            }
        }
        for (number, observed_at) in std::mem::take(&mut state.new_promoted) {
            match self.label_time(fetcher, budget, input, number, CURATED) {
                Some(curated_at) => samples.push(DwellSample {
                    stage: "curated_to_issue",
                    seconds: (observed_at - curated_at).num_seconds().max(0),
                }),
                None if *budget == 0 => {
                    state.new_promoted.insert(number, observed_at);
                }
                None => {}
            }
        }

        // Issues newly under `loom:curated`: created → curated.
        if !baseline {
            for item in input.listings.get(CURATED).into_iter().flatten() {
                if !state.curated.contains(&item.number) {
                    if let Some(created_at) = item.created_at {
                        state.new_curated.insert(item.number, created_at);
                    }
                }
            }
        }
        for (number, created_at) in std::mem::take(&mut state.new_curated) {
            if !curated.contains(&number) {
                continue;
            }
            match self.label_time(fetcher, budget, input, number, CURATED) {
                Some(curated_at) => samples.push(DwellSample {
                    stage: "created_to_curated",
                    seconds: (curated_at - created_at).num_seconds().max(0),
                }),
                None if *budget == 0 => {
                    state.new_curated.insert(number, created_at);
                }
                None => {}
            }
        }

        state.curated = curated;
        state.promoted = promoted;
        state.building = building;
        state.review = review
            .iter()
            .filter_map(|(number, item)| item.created_at.map(|at| (*number, at)))
            .collect();
        // Forget cached label times of items no longer in any tracked stage.
        let live: BTreeSet<u32> = state
            .curated
            .iter()
            .chain(state.building.iter())
            .copied()
            .collect();
        let slug = input.slug.as_str();
        self.label_times
            .retain(|(s, number, _), _| s != slug || live.contains(number));
    }

    /// When the previous sample was taken.
    #[must_use]
    pub fn last_at(&self) -> Option<DateTime<Utc>> {
        self.last_at
    }

    /// Drop everything known about `slug`: its stage sets, pending samples
    /// and cached label times. Used while the fleet captain samples the repo
    /// (W12), so that when this host takes it back its first sample is a
    /// fresh baseline rather than a diff against a stale one, which would
    /// replay every transition it missed as new.
    pub fn forget(&mut self, slug: &str) {
        self.repos.remove(slug);
        self.sampled_at.remove(slug);
        self.label_times.retain(|(s, _, _), _| s != slug);
    }
}

/// The metric points for one sample: the dwell delta pair per stage with a
/// sample, plus the item gauges per repo (`repo`, lowercase `owner/name`,
/// #11847): one per stage label, then the [`super::stage_queue`] states when
/// that repo's walk completed.
#[must_use]
pub fn stage_points(samples: &[DwellSample], inputs: &[RepoInput]) -> Vec<MetricPoint> {
    let mut sums: BTreeMap<&'static str, (i64, i64)> = BTreeMap::new();
    for sample in samples {
        let entry = sums.entry(sample.stage).or_default();
        entry.0 = entry.0.saturating_add(sample.seconds);
        entry.1 += 1;
    }
    let mut points = Vec::new();
    for (stage, (seconds, count)) in sums {
        points.push(MetricPoint::int(MetricName::ForgeStageDwell, seconds).label("state", stage));
        points.push(
            MetricPoint::int(MetricName::ForgeStageDwellSamples, count).label("state", stage),
        );
    }
    if inputs.is_empty() {
        return points;
    }
    let item = |count: usize, state: String, repo: &str| {
        MetricPoint::int(MetricName::ForgeStageItems, i64::try_from(count).unwrap_or(i64::MAX))
            .label("state", state)
            .label("repo", repo)
    };
    for input in inputs {
        let repo = input.slug.to_ascii_lowercase();
        for label in STAGE_LABELS {
            let count = input.listings.get(label).map_or(0, Vec::len);
            let state = label.trim_start_matches("loom:").replace('-', "_");
            points.push(item(count, state, &repo));
        }
        for (state, count) in input.queue.iter().flatten() {
            points.push(item(*count, (*state).to_string(), &repo));
        }
    }
    points
}

/// [`stage_points`] split into records of at most
/// [`crate::telemetry::ops::MAX_POINTS_PER_RECORD`] points. One record would
/// be truncated by `bounded_points` past that bound, and with per-repo states
/// a host sampling a few dozen repos (the fleet captain) crosses it. The
/// dwell delta pairs come first, so they always ride in the first batch.
#[must_use]
pub fn stage_batches(samples: &[DwellSample], inputs: &[RepoInput]) -> Vec<Vec<MetricPoint>> {
    stage_points(samples, inputs)
        .chunks(crate::telemetry::ops::MAX_POINTS_PER_RECORD)
        .map(<[MetricPoint]>::to_vec)
        .collect()
}

/// The live forge reads, through `gh api` in the repo's own checkout (so a
/// cross-owner repo uses its own owner's credential).
pub struct GhStageFetcher;

/// The `caller` stage-dwell's events reads are counted under in
/// `loom.forge.calls` (the step engine's provider has its own).
pub const EVENTS_CALLER: &str = "stage_dwell.events";

/// The `github.operation` of stage-dwell's `pulls/{n}` read.
pub const MERGED_AT_OP: &str = "stage_dwell.merged_at";

/// One Observability `gh api` GET of `path` in `root`, as JSON, named
/// `operation` (never the generic `api.rest`: 2026-10-15, the fleet.state
/// history reads were invisible in the writer's spend under that name).
pub(in crate::observability) fn gh_json(
    root: &Path,
    operation: &'static str,
    path: &str,
) -> Option<serde_json::Value> {
    let CmdOutcome::Ran(output) = gh_json_invocation(root, operation, path).run() else {
        return None;
    };
    if !output.status.success() {
        log::debug!("observability: gh api {path} failed in {}", root.display());
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The invocation [`gh_json`] runs (split out so a test can inspect it).
pub(in crate::observability) fn gh_json_invocation(
    root: &Path,
    operation: &'static str,
    path: &str,
) -> GhInvocation {
    GhInvocation::new(Operation::new(operation), AccessIntent::Read, GhTarget::None, GH_TIMEOUT)
        .read_class(crate::gh_invocation::ReadClass::Observability)
        .args(["api", path])
        .current_dir(root)
}

/// The latest `labeled` time per label in one events page.
#[must_use]
pub fn parse_label_times(events: &serde_json::Value) -> BTreeMap<String, DateTime<Utc>> {
    let mut times = BTreeMap::new();
    for event in events.as_array().into_iter().flatten() {
        if event["event"] != "labeled" {
            continue;
        }
        let (Some(name), Some(at)) = (
            event["label"]["name"].as_str(),
            event["created_at"].as_str().and_then(parse_time),
        ) else {
            continue;
        };
        let entry = times.entry(name.to_string()).or_insert(at);
        if at > *entry {
            *entry = at;
        }
    }
    times
}

impl StageFetcher for GhStageFetcher {
    fn label_times(
        &mut self,
        root: &Path,
        slug: &str,
        number: u32,
        label: &str,
        max_pages: usize,
    ) -> LabelTimes {
        super::stage_events::read(EVENTS_CALLER, root, slug, number, label, max_pages)
    }

    fn merged_at(&mut self, root: &Path, slug: &str, number: u32) -> Option<Option<DateTime<Utc>>> {
        let pull = gh_json(root, MERGED_AT_OP, &format!("repos/{slug}/pulls/{number}"))?;
        Some(pull["merged_at"].as_str().and_then(parse_time))
    }
}

static SAMPLER: Mutex<Option<Sampler>> = Mutex::new(None);

/// One stage listing from the GitHub state mirror, or `None` (read the forge
/// as today). No thread hop while no mirror is live.
async fn mirror_listing(root: &Path, slug: &str, label: &'static str) -> Option<Vec<RestIssue>> {
    use crate::forge_mirror::discovery::{list_open, Caller, Pages};
    if !crate::forge_mirror::is_active() {
        return None;
    }
    let (root, slug) = (root.to_path_buf(), slug.to_string());
    tokio::task::spawn_blocking(move || {
        list_open(
            crate::forge_events::poll_gate::ReadKind::Discovery,
            Caller::StageDwell,
            Some(&root),
            Some(&slug),
            Some(label),
            Pages::First,
        )
    })
    .await
    .ok()
    .flatten()
}

/// Sample every managed repo's stage listings and export the dwell points.
/// A no-op (no forge reads at all) without the OTLP ops sink, or while
/// `settings` says off or not yet due (#12203).
pub(in crate::observability) async fn record(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
    settings: Settings,
) {
    if !claim_pass(settings, Utc::now()) {
        return;
    }
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let mut inputs = Vec::new();
    let mut seen = BTreeSet::new();
    let mut left_to_captain = Vec::new();
    // #12027: the repos whose `pr_refused` this host reports (stage_queue).
    let mut refused = Vec::new();
    for root in crate::observability::collector::provisioned_roots(workspace_pool) {
        let root_str = root.to_string_lossy().to_string();
        let Some(slug) =
            crate::observability::collector::resolve_repo_slug_cached(slug_cache, &root_str).await
        else {
            continue;
        };
        if !seen.insert(slug.clone()) {
            continue;
        }
        // W12: the fleet captain samples this repo and its data is fresh.
        if crate::observability::captain_gauges::captain_covers(STAGE_JOB, &slug) {
            refused.push((root.clone(), slug.clone()));
            left_to_captain.push(slug);
            continue;
        }
        let mut input = RepoInput {
            slug,
            root: root.clone(),
            listings: BTreeMap::new(),
            queue: None,
        };
        let mut complete = true;
        for label in STAGE_LABELS {
            // 2am#4367 P5: the state mirror first (a discovery read, first
            // page like the REST listing), today's listing otherwise.
            let mirrored = mirror_listing(&root, &input.slug, label).await;
            let listing = match mirrored {
                Some(rows) => Some(rows),
                None => {
                    crate::observability::queue_blocked::list_open_observability(
                        root.clone(),
                        label,
                        "stage_dwell",
                    )
                    .await
                }
            };
            match listing {
                Some(listing) => {
                    let items = listing.iter().map(StageItem::from_rest).collect();
                    input.listings.insert(label, items);
                }
                None => {
                    complete = false;
                    break;
                }
            }
        }
        // A partial listing would read as mass exits; skip the repo instead.
        if complete {
            // #11847: the states no label listing answers, from one walk.
            input.queue = super::stage_queue::walk_counts(root.clone(), input.slug.clone()).await;
            if input.queue.is_some() {
                refused.push((root.clone(), input.slug.clone()));
            }
            inputs.push(input);
        }
    }
    // Detached: up to FETCH_BUDGET per-item reads (each bounded by
    // GH_TIMEOUT) must never stall the collector's event loop.
    drop(tokio::task::spawn_blocking(move || {
        let mut guard = match SAMPLER.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            // The previous sample is still reading: skip this one.
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        let sampler = guard.get_or_insert_with(Sampler::default);
        sampler.set_replay_bound(crate::observability::captain_gauges::replay_bound());
        for slug in &left_to_captain {
            sampler.forget(slug);
        }
        let previous = sampler.last_at();
        let samples = sampler.sample(&inputs, Utc::now(), &mut GhStageFetcher);
        drop(guard);
        for batch in stage_batches(&samples, &inputs) {
            sink.emit_metrics_since(batch, previous);
        }
        let refused = super::stage_queue::refused_points_for(&refused, Utc::now());
        for batch in refused.chunks(crate::telemetry::ops::MAX_POINTS_PER_RECORD) {
            sink.emit_metrics(batch.to_vec());
        }
        let sampled = inputs.iter().map(|input| input.slug.clone());
        crate::observability::captain_gauges::note_produced(STAGE_JOB, sampled, Utc::now());
    }));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_dwell_tests.rs"]
mod tests;
