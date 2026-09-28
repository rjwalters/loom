//! The `eta-explanation/v1` record every estimate carries: enough detail to
//! recompute the number offline and to test later which feature matters.
//!
//! Rules: no raw sample arrays (the grid is the distribution, and its values
//! are observed durations); a feature is `null` when unmeasured, with a
//! [`FeatureOmitted`] reason, never a default; seconds are integers.
//!
//! Size: target [`TARGET_BYTES`], hard cap [`MAX_BYTES`] (the trace journal's
//! entry cap). [`Explanation::enforce_cap`] drops `features` first, then the
//! stage grids, and names what it dropped in `truncated`.

use super::{AgeSource, Kind, NoEstimateReason, Provenance, Stage, Subject};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Hard cap on one serialized explanation.
pub const MAX_BYTES: usize = 32 * 1024;

/// The size an ordinary explanation should stay under.
pub const TARGET_BYTES: usize = 8 * 1024;

/// `truncated[]` entry when `features` was dropped.
pub const TRUNCATED_FEATURES: &str = "features";

/// `truncated[]` entry when the stage grids were dropped.
pub const TRUNCATED_GRIDS: &str = "stages.distribution.grid";

/// `truncated[]` entry when every remaining list was dropped (stages,
/// branches, contributions, history detail) — the last resort that
/// guarantees the cap.
pub const TRUNCATED_DETAIL: &str = "detail";

/// One estimate, explained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// Always [`super::EXPLANATION_SCHEMA`].
    pub schema: String,
    /// Derived id ([`super::estimate_id`]).
    pub estimate_id: String,
    /// Heuristic id, e.g. `land-v1`.
    pub heuristic: String,
    /// What is predicted.
    pub kind: Kind,
    /// The computing build (required).
    pub loom: Provenance,
    /// The instant described.
    pub as_of: DateTime<Utc>,
    /// What is estimated.
    pub subject: Subject,
    /// The stage the item is in, when it has one.
    pub current_stage: Option<CurrentStageRecord>,
    /// The history the distributions came from.
    pub history_window: Option<HistoryWindow>,
    /// Whose history it was, and how many samples each source and host
    /// contributed (#9343: `local` until fleet history exists).
    pub history: Option<HistoryRecord>,
    /// The path the simulation walks.
    pub path: Option<PathRecord>,
    /// One entry per stage on the path, in path order.
    pub stages: Vec<StageEntry>,
    /// Branch probabilities.
    pub branches: Option<Branches>,
    /// How the stages were combined.
    pub combination: Option<Combination>,
    /// The estimate. `None` on a refusal.
    pub result: Option<EstimateResult>,
    /// Which stages and branches dominate the result.
    pub contributions: Option<Contributions>,
    /// Recorded context. `None` only when truncated.
    pub features: Option<Features>,
    /// Why features are null.
    pub features_omitted: Vec<FeatureOmitted>,
    /// Why there is no estimate. `None` when there is one.
    pub no_estimate_reason: Option<NoEstimateReason>,
    /// What [`Explanation::enforce_cap`] dropped, in drop order.
    pub truncated: Vec<String>,
}

/// The current stage as the estimate saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentStageRecord {
    /// The stage.
    pub stage: Stage,
    /// When it was entered, when known.
    pub entered_at: Option<DateTime<Utc>>,
    /// Seconds already spent in it.
    pub age_sec: i64,
    /// Where the age came from.
    pub age_source: AgeSource,
    /// Judge rejections already taken.
    pub rework_rounds: u32,
}

/// The history window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryWindow {
    /// Oldest instant a sample may have been observed at.
    pub from: DateTime<Utc>,
    /// Samples were observed strictly before this (= `as_of`).
    pub to: DateTime<Utc>,
    /// Which journals samples may come from.
    pub sources: Vec<String>,
}

/// Whose history an estimate read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScope {
    /// This host's own journals only (every v1 estimate).
    #[default]
    Local,
    /// A fleet-wide snapshot. Reserved for #9343; never produced yet.
    Fleet,
}

/// The history an estimate read, and where its samples came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// `local` (this host's journals) or, later, `fleet`.
    pub scope: HistoryScope,
    /// Journals the distributions may read.
    pub sources: Vec<String>,
    /// Stage samples used by the path's distributions, per journal.
    pub samples_by_source: std::collections::BTreeMap<String, usize>,
    /// Stage samples used by the path's distributions, per recording host.
    pub samples_by_host: std::collections::BTreeMap<String, usize>,
}

/// The simulated path's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathRecord {
    /// First stage (the current one).
    pub start: Stage,
    /// Whether the path ends with `merge_wait` after approval (otherwise it
    /// ends at the approving verdict).
    pub include_merge: bool,
    /// The last stage of an approved path.
    pub terminal: Stage,
    /// `finish` only: the share of the history's successful sweeps that
    /// merged in-sweep, which decides `include_merge`.
    pub merge_share: Option<f64>,
    /// How many successful sweeps `merge_share` is over.
    pub merge_share_n: Option<usize>,
}

/// One stage on the path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEntry {
    /// The stage.
    pub stage: Stage,
    /// Its duration distribution.
    pub distribution: Distribution,
    /// Present on the current stage only.
    pub conditioning: Option<Conditioning>,
    /// Fraction of simulated paths that visit this stage.
    pub reached_with_probability: Option<f64>,
    /// Mean visits per simulated path (`review_wait` can repeat).
    pub mean_visits: Option<f64>,
}

/// A stage's duration distribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    /// Samples it was built from.
    pub n: usize,
    /// Which samples were used.
    pub filters: Filters,
    /// `[0, 5, …, 100]`. Empty only when truncated.
    pub grid_pct: Vec<u8>,
    /// Observed durations at those percentiles. Empty only when truncated.
    pub grid_sec: Vec<i64>,
    /// Nearest-rank quartiles and p90.
    pub p25: i64,
    /// Median.
    pub p50: i64,
    /// Upper quartile.
    pub p75: i64,
    /// 90th percentile.
    pub p90: i64,
}

/// The sample filter a distribution was built with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    /// The repo, or `None` at host level.
    pub repo: Option<String>,
    /// `repo` or `host`.
    pub level: String,
    /// Size bucket (unused in v1).
    pub size_bucket: Option<String>,
    /// Journals the samples came from.
    pub sources: Vec<String>,
}

/// How the current stage was conditioned on its age.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conditioning {
    /// Seconds already spent.
    pub age_sec: i64,
    /// Grid CDF at the age; draws come from `[f_age, 1]`.
    pub f_age: f64,
    /// Samples longer than the age.
    pub n_above: usize,
    /// Always `truncate_inverse_cdf`.
    pub method: String,
}

/// Branch probabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Branches {
    /// The Judge's changes-requested branch.
    pub changes_requested: ChangesRequested,
}

/// `P(changes requested | attempt k)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangesRequested {
    /// Probability per attempt, `1..=cap`.
    pub p_by_attempt: Vec<f64>,
    /// Verdicts observed per attempt.
    pub n_by_attempt: Vec<usize>,
    /// `observed`, or `carried_forward` when the attempt had too few verdicts
    /// and the previous attempt's probability was reused.
    pub source_by_attempt: Vec<String>,
    /// `repo` or `host`.
    pub level: String,
    /// Most rework rounds per path.
    pub cap: u32,
    /// What happens at the cap: `approve` (the path is landing-conditional).
    pub after_cap: String,
    /// Mean rework rounds per simulated path.
    pub expected_rework_rounds: Option<f64>,
    /// Always `judge_verdicts`.
    pub source: String,
}

/// How the stages were combined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Combination {
    /// `monte_carlo_grid_resample`.
    pub method: String,
    /// Paths simulated.
    pub draws: usize,
    /// `0x`-prefixed 16-hex seed.
    pub seed: String,
    /// `splitmix64`.
    pub rng: String,
    /// Order the generator is consumed in, per path.
    pub draw_order: String,
    /// Stage durations are drawn independently.
    pub independence_assumed: bool,
}

/// The estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateResult {
    /// Remaining seconds, 25th percentile.
    pub p25_sec: i64,
    /// Remaining seconds, median.
    pub p50_sec: i64,
    /// Remaining seconds, 75th percentile.
    pub p75_sec: i64,
    /// `as_of + p50`.
    pub eta_p50_at: DateTime<Utc>,
    /// Smallest `n` over the path's distributions.
    pub samples_min: usize,
}

/// What dominates the result, per stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contributions {
    /// Each stage's share of the mean total over paths ranked p40–p60.
    pub p50_share: std::collections::BTreeMap<String, f64>,
    /// Each stage's share of the gap between paths ranked p50–p75 and
    /// p25–p50 (negative gaps count as zero).
    pub iqr_share: std::collections::BTreeMap<String, f64>,
    /// Fraction of paths taking at least one new rework round, per quartile
    /// of the total (fastest first).
    pub rework_fraction_by_quartile: Vec<f64>,
}

/// Recorded context. `null` = not measured; no v1 heuristic reads these.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Features {
    /// Current labels.
    pub labels: Option<Vec<String>>,
    /// `<!-- loom:complexity=… -->`.
    pub complexity_marker: Option<String>,
    /// `<!-- loom:points=… -->`.
    pub points_marker: Option<u32>,
    /// Model tier.
    pub tier: Option<String>,
    /// Urgent flag.
    pub urgent: Option<bool>,
    /// Workspace priority.
    pub workspace_priority: Option<i64>,
    /// Issue creation instant.
    pub issue_created_at: Option<DateTime<Utc>>,
    /// Issue age at `as_of`.
    pub issue_age_sec: Option<i64>,
    /// Issue author.
    pub author: Option<String>,
    /// Rank in the ready queue.
    pub queue_rank: Option<u32>,
    /// Ready queue depth.
    pub queue_ready: Option<u32>,
    /// Running sweeps in the queue view.
    pub queue_running: Option<u32>,
    /// Configured concurrency.
    pub max_concurrent: Option<u32>,
    /// Active sweeps on this host.
    pub active_sweeps_host: Option<u32>,
    /// Sweep runtime adapter.
    pub sweep_runtime: Option<String>,
    /// Sweep model.
    pub sweep_model: Option<String>,
    /// Sweep effort.
    pub sweep_effort: Option<String>,
    /// Sweep attempt number.
    pub attempt: Option<u32>,
    /// Doctor cycles so far.
    pub doctor_cycles_so_far: Option<u32>,
    /// Judge verdicts so far (`pass`/`fail`).
    pub judge_verdicts_so_far: Option<Vec<String>>,
    /// PR additions.
    pub pr_additions: Option<i64>,
    /// PR deletions.
    pub pr_deletions: Option<i64>,
    /// PR changed files.
    pub pr_changed_files: Option<i64>,
    /// PR commits.
    pub pr_commits: Option<i64>,
    /// PR creation instant.
    pub pr_created_at: Option<DateTime<Utc>>,
    /// Whether the Judge runs inside the sweep.
    pub sweep_internal: Option<bool>,
    /// Hour of `as_of`, UTC.
    pub hour_utc: Option<u32>,
    /// Weekday of `as_of`, UTC, Monday = 0.
    pub weekday_utc: Option<u32>,
    /// Emitting host.
    pub host_id: Option<String>,
    /// Usable pool accounts.
    pub pool_usable_accounts: Option<u32>,
    /// Pool exhausted.
    pub pool_exhausted: Option<bool>,
    /// The repo's first-pass approval rate.
    pub repo_first_pass_approval_rate: Option<f64>,
}

/// Why a feature is null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureOmitted {
    /// Feature name.
    pub name: String,
    /// Why (`not_collected`, `budget_exhausted`, …).
    pub reason: String,
}

impl Features {
    /// Every feature name, in field order.
    pub const NAMES: [&'static str; 32] = [
        "labels",
        "complexity_marker",
        "points_marker",
        "tier",
        "urgent",
        "workspace_priority",
        "issue_created_at",
        "issue_age_sec",
        "author",
        "queue_rank",
        "queue_ready",
        "queue_running",
        "max_concurrent",
        "active_sweeps_host",
        "sweep_runtime",
        "sweep_model",
        "sweep_effort",
        "attempt",
        "doctor_cycles_so_far",
        "judge_verdicts_so_far",
        "pr_additions",
        "pr_deletions",
        "pr_changed_files",
        "pr_commits",
        "pr_created_at",
        "sweep_internal",
        "hour_utc",
        "weekday_utc",
        "host_id",
        "pool_usable_accounts",
        "pool_exhausted",
        "repo_first_pass_approval_rate",
    ];

    /// `omitted` plus a `reason` entry for every null feature it does not
    /// already name — so every null carries a reason.
    #[must_use]
    pub fn complete_omissions(
        &self,
        mut omitted: Vec<FeatureOmitted>,
        reason: &str,
    ) -> Vec<FeatureOmitted> {
        let value = serde_json::to_value(self).unwrap_or_default();
        for name in Self::NAMES {
            let is_null = value.get(name).is_none_or(serde_json::Value::is_null);
            if is_null && !omitted.iter().any(|o| o.name == name) {
                omitted.push(FeatureOmitted {
                    name: name.to_string(),
                    reason: reason.to_string(),
                });
            }
        }
        omitted
    }
}

impl Explanation {
    /// The `(p25, p50, p75)` remaining seconds, when there is an estimate.
    #[must_use]
    pub fn quantiles(&self) -> Option<(i64, i64, i64)> {
        self.result
            .as_ref()
            .map(|r| (r.p25_sec, r.p50_sec, r.p75_sec))
    }

    /// Serialized size in bytes.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        serde_json::to_vec(self)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
    }

    /// Enforce [`MAX_BYTES`]: drop `features`, then the stage grids, then
    /// every remaining list, stopping as soon as the record fits, and
    /// recording each drop in `truncated`.
    pub fn enforce_cap(&mut self) {
        if self.size_bytes() <= MAX_BYTES {
            return;
        }
        self.features = None;
        self.features_omitted.clear();
        self.truncated.push(TRUNCATED_FEATURES.to_string());
        if self.size_bytes() <= MAX_BYTES {
            return;
        }
        for entry in &mut self.stages {
            entry.distribution.grid_pct.clear();
            entry.distribution.grid_sec.clear();
        }
        self.truncated.push(TRUNCATED_GRIDS.to_string());
        if self.size_bytes() <= MAX_BYTES {
            return;
        }
        // Last resort: keep the identity, provenance, subject and result;
        // drop every list. Nothing unbounded is left after this.
        self.stages.clear();
        self.branches = None;
        self.contributions = None;
        if let Some(window) = &mut self.history_window {
            window.sources.clear();
        }
        if let Some(history) = &mut self.history {
            history.sources.clear();
            history.samples_by_source.clear();
            history.samples_by_host.clear();
        }
        self.truncated.push(TRUNCATED_DETAIL.to_string());
    }
}
