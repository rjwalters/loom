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
//! - one `issues/{n}/events` read gives an item's `labeled` times, and each
//!   found time is cached for the life of the process;
//! - one `pulls/{n}` read tells whether a PR that left review merged.
//!
//! Both come out of one shared budget of [`FETCH_BUDGET`] reads per sample
//! across all repos. Work over the budget waits for the next sample, and
//! nothing is ever read per tick or per issue per tick.
//!
//! # Stages (`state` label)
//!
//! | `state` | sampled when | seconds |
//! |---|---|---|
//! | `created_to_curated` | an issue newly appears under `loom:curated` | `loom:curated` labeled − created |
//! | `curated_to_issue` | an issue leaves `loom:curated` and is under `loom:issue` or `loom:building` | leave observed − `loom:curated` labeled |
//! | `building_to_review_requested` | a PR newly under a review label closes an issue with a known `loom:building` time | PR created − `loom:building` labeled |
//! | `review_requested_to_merged` | a PR leaves the review labels (`loom:review-requested`, `loom:changes-requested`, `loom:pr`) and merged | merged − PR created |
//!
//! `curated_to_issue` is resolved to one sample interval. The first sample
//! after start only records a baseline, so a restart never replays history
//! as new entries. Every host that manages a repo samples it independently:
//! sums scale with the host count, the mean (`dwell / samples`) does not.
//! `loom.forge.stage_items{state}` counts the open items under each stage
//! label, summed over this host's repos.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

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

/// Per-item forge reads allowed per sample, across all repos.
pub const FETCH_BUDGET: usize = 8;

/// Samples a pending PR lookup is retried before it is dropped.
pub const MAX_TRIES: u8 = 3;

/// Timeout for one per-item `gh api` read.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// One listed item, reduced to what the sampler needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageItem {
    pub number: u32,
    pub created_at: Option<DateTime<Utc>>,
    /// Issues a PR's body closes (`Closes #N`, `Fixes #N`, `Resolves #N`).
    pub closes: Vec<u32>,
}

impl StageItem {
    #[must_use]
    pub fn from_rest(item: &RestIssue) -> Self {
        StageItem {
            number: item.number,
            created_at: item.created_at.as_deref().and_then(parse_time),
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
}

impl RepoInput {
    fn numbers(&self, label: &str) -> BTreeSet<u32> {
        self.listings
            .get(label)
            .map(|items| items.iter().map(|i| i.number).collect())
            .unwrap_or_default()
    }

    fn review(&self) -> BTreeMap<u32, &StageItem> {
        REVIEW_LABELS
            .iter()
            .filter_map(|label| self.listings.get(label))
            .flatten()
            .map(|item| (item.number, item))
            .collect()
    }
}

/// The per-item forge reads, injected so the sampler is testable offline.
pub trait StageFetcher {
    /// The latest `labeled` time of every label on issue/PR `number`, or
    /// `None` when the read failed.
    fn label_times(
        &mut self,
        root: &Path,
        slug: &str,
        number: u32,
    ) -> Option<BTreeMap<String, DateTime<Utc>>>;
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
        *budget -= 1;
        let times = fetcher.label_times(&input.root, &input.slug, number)?;
        for (name, at) in &times {
            self.label_times
                .insert((input.slug.clone(), number, name.clone()), Some(*at));
        }
        let found = times.get(label).copied();
        self.label_times.insert(key, found);
        found
    }

    /// Take one sample of every repo in `inputs` at `now`. A repo absent from
    /// `inputs` (its listing failed) keeps its state untouched.
    pub fn sample(
        &mut self,
        inputs: &[RepoInput],
        now: DateTime<Utc>,
        fetcher: &mut impl StageFetcher,
    ) -> Vec<DwellSample> {
        let mut budget = FETCH_BUDGET;
        let mut samples = Vec::new();
        for input in inputs {
            let baseline = !self.repos.contains_key(&input.slug);
            let mut state = self.repos.remove(&input.slug).unwrap_or_default();
            self.sample_repo(&mut state, input, baseline, now, fetcher, &mut budget, &mut samples);
            self.repos.insert(input.slug.clone(), state);
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
                None => {
                    pending.tries += 1;
                    if pending.tries < MAX_TRIES {
                        still_new.insert(number, pending);
                    }
                }
            }
        }
        state.new_prs = still_new;

        // Issues that left `loom:curated` for the ready queue or a build.
        for number in state.curated.difference(&curated) {
            if !promoted.contains(number) {
                continue;
            }
            let key = (input.slug.clone(), *number, CURATED.to_string());
            if let Some(Some(curated_at)) = self.label_times.get(&key) {
                samples.push(DwellSample {
                    stage: "curated_to_issue",
                    seconds: (now - *curated_at).num_seconds().max(0),
                });
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

        // Warm the curated times of items already present, so their exit can
        // be sampled later. Only spare budget is used.
        for number in &curated {
            if *budget == 0 {
                break;
            }
            self.label_time(fetcher, budget, input, *number, CURATED);
        }

        state.curated = curated;
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
}

/// The metric points for one sample: the dwell delta pair per stage with a
/// sample, plus the item gauges per stage label.
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
    for label in STAGE_LABELS {
        let count: usize = inputs
            .iter()
            .map(|input| input.listings.get(label).map_or(0, Vec::len))
            .sum();
        let state = label.trim_start_matches("loom:").replace('-', "_");
        points.push(
            MetricPoint::int(MetricName::ForgeStageItems, i64::try_from(count).unwrap_or(i64::MAX))
                .label("state", state),
        );
    }
    points
}

/// The live forge reads, through `gh api` in the repo's own checkout (so a
/// cross-owner repo uses its own owner's credential).
pub struct GhStageFetcher;

fn gh_json(root: &Path, path: &str) -> Option<serde_json::Value> {
    let mut cmd = Command::new("gh");
    cmd.arg("api").arg(path).current_dir(root);
    crate::credential_preflight::apply_gh_config_for_cwd(&mut cmd, Some(root));
    let output = crate::sweep_registry::output_with_timeout(cmd, GH_TIMEOUT).ok()??;
    if !output.status.success() {
        log::debug!("observability: gh api {path} failed in {}", root.display());
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The latest `labeled` time per label in an issue's events page.
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
    ) -> Option<BTreeMap<String, DateTime<Utc>>> {
        let events = gh_json(root, &format!("repos/{slug}/issues/{number}/events?per_page=100"))?;
        Some(parse_label_times(&events))
    }

    fn merged_at(&mut self, root: &Path, slug: &str, number: u32) -> Option<Option<DateTime<Utc>>> {
        let pull = gh_json(root, &format!("repos/{slug}/pulls/{number}"))?;
        Some(pull["merged_at"].as_str().and_then(parse_time))
    }
}

static SAMPLER: Mutex<Option<Sampler>> = Mutex::new(None);

/// Sample every managed repo's stage listings and export the dwell points.
/// A no-op (no forge reads at all) without the OTLP ops sink.
pub(in crate::observability) async fn record(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let mut inputs = Vec::new();
    let mut seen = BTreeSet::new();
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
        let mut input = RepoInput {
            slug,
            root: root.clone(),
            listings: BTreeMap::new(),
        };
        let mut complete = true;
        for label in STAGE_LABELS {
            match crate::observability::queue_blocked::list_open(root.clone(), label).await {
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
            inputs.push(input);
        }
    }
    let joined = tokio::task::spawn_blocking(move || {
        let mut guard = SAMPLER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sampler = guard.get_or_insert_with(Sampler::default);
        let previous = sampler.last_at();
        let samples = sampler.sample(&inputs, Utc::now(), &mut GhStageFetcher);
        (stage_points(&samples, &inputs), previous)
    })
    .await;
    if let Ok((points, previous)) = joined {
        sink.emit_metrics_since(points, previous);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_dwell_tests.rs"]
mod tests;
