//! Fleet-wide aggregation over the `sweep.outcome` telemetry journal — the
//! engine behind `loom-daemon sweep-outcomes summary` (Issue #8057).
//!
//! # Why this exists
//!
//! [`crate::sweep_outcomes`] gives a single workspace's journal and
//! [`crate::sweep_outcomes::summarize_by_model`] gives one fixed cut of it
//! (success rate + median duration by model, #4137 AC4). Answering "how did
//! arm A do against arm B across the fleet this week?" needed a hand-rolled
//! script every time: glob 48 workspaces' journals, unwrap every
//! [`crate::telemetry::TelemetryEnvelope`], drop spawn deaths, join
//! `pr_number` to the forge, and weight `tokens_by_model` through a pricing
//! table. This module is that script, once, with tests.
//!
//! # Relationship to the pre-existing summary
//!
//! The bare `loom-daemon sweep-outcomes` summary is **not** superseded and is
//! not touched: it keeps its exact columns, its exact `success_rate`
//! definition, and its byte-identical output. This module is a documented
//! **superset** reached only through the explicit `summary` sub-verb — a
//! different (richer) failure-rate definition lives here under a different
//! name (`real_failure_rate`, spawn deaths removed) precisely so the two can
//! never be mistaken for each other.
//!
//! # Three fields this wants that the record does not carry yet
//!
//! Each has a fallback and a single swap point, so the follow-up issues are
//! one-line changes here:
//!
//! * **`failure_class`** (#8056 item 1) — not on
//!   [`crate::telemetry::SweepOutcomeRecord`] yet. [`failure_class`] reads the
//!   forward-compatible `config["failure_class"]` key first, then falls back
//!   to a `sweep_id` join against the sibling `sweep-outcomes.jsonl`
//!   (`death_class` / `crash_classification`) and to the `< 60 s` heuristic.
//! * **`doctor_cycles`** (#8056) — [`doctor_engaged`] reads
//!   `config["doctor_cycles"]` first, then falls back to scanning
//!   `phase_durations` for a `doctor` phase. The column is an approximation
//!   until then and says so.
//! * **`experiment_arm`** (#8055) — [`resolve_arm`] prefers an explicit
//!   `config["experiment_arm"]`/`config["experiment_id"]` stamp, falls back to
//!   the *inferred* `config["arm"]` (marked [`ArmSource::Inferred`] in every
//!   output), and buckets everything else under `unknown` rather than dropping
//!   it.
//!
//! # Two honest caveats, surfaced in the output rather than hidden
//!
//! * **The rate card is stale.** Weighted tokens price through
//!   [`ModelPricing::for_model`], whose rates are commented "as of Jan 2025"
//!   and which deliberately prices `claude-fable-*` at the Opus rate. #8060
//!   tracks refreshing it. Both the JSON and the human header name the card
//!   and its as-of date so a reader can tell whether two numbers disagree
//!   because of the data or because of the card — and so "merges per
//!   weighted-token" is never read as more precise than its denominator.
//! * **`--group-by host` is degenerate on a local read.**
//!   [`crate::telemetry::TelemetryEnvelope::new`] stamps the *emitting* host,
//!   so every envelope read from one host's own workspaces carries the same
//!   `host_id`. The grouping is real only over a pooled/exported corpus; the
//!   report says so in its `notes`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::activity::resource_usage::ModelPricing;
use crate::runtime_preference::CredentialSource;
use crate::sweep_outcomes;
use crate::telemetry::{SweepOutcomeRecord, SweepResult, TelemetryRecord};

// ============================================================================
// Rate card identity (Issue #8060 is the refresh; this names what was used)
// ============================================================================

/// Which of the tree's pricing entry points weighted the tokens. Emitted
/// verbatim in `--json` and in the human header.
pub const RATE_CARD_ID: &str = "activity::resource_usage::ModelPricing::for_model";

/// The as-of date of [`RATE_CARD_ID`]'s rates, per the comment at its
/// definition ("prices as of Jan 2025").
pub const RATE_CARD_AS_OF: &str = "2025-01";

/// The caveat every consumer of `weighted_tokens_usd` /
/// `merges_per_weighted_token` needs to have read.
pub const RATE_CARD_CAVEAT: &str =
    "rates are the Jan-2025 card and are known stale (issue #8060); \
     `claude-fable-*` is deliberately priced at the Opus rate — treat \
     weighted-token figures as provisional";

/// A sweep that reached a terminal outcome faster than this never really ran
/// — the spawn-death heuristic the ask specifies, used when no class string
/// is available. Public so a caller can state the threshold it applied.
pub const SPAWN_DEATH_MIN_DURATION_SEC: i64 = 60;

/// Config keys carrying an **explicit** experiment arm stamp, in preference
/// order. Neither exists yet; #8055 adds them. Reading them now means this
/// command upgrades from "inferred" to "explicit" the moment it does.
pub const EXPLICIT_ARM_KEYS: &[&str] = &["experiment_arm", "experiment_id"];

/// The config key carrying the **inferred** arm
/// (`sweep_registry::outcome_journal` derives it from the dispatched model).
pub const INFERRED_ARM_KEY: &str = "arm";

/// Forward-compatible config key for #8056's `failure_class`. Read before the
/// sibling-journal join so this command needs no change when the real field
/// lands — only the one-line swap to `record.failure_class` in
/// [`failure_class`].
pub const FAILURE_CLASS_KEY: &str = "failure_class";

/// Forward-compatible config key for #8056's `doctor_cycles`, read before the
/// `phase_durations` scan in [`doctor_engaged`].
pub const DOCTOR_CYCLES_KEY: &str = "doctor_cycles";

/// The `unknown` bucket label. Records with no groupable value land here;
/// nothing is ever dropped for want of a group.
pub const UNKNOWN_GROUP: &str = "unknown";

// ============================================================================
// Grouping
// ============================================================================

/// The `--group-by` dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    /// Experiment arm — explicit stamp preferred, inferred marked, unstamped
    /// bucketed as `unknown` (see [`resolve_arm`]).
    Arm,
    /// Dispatched model, `default` for a record with none.
    Model,
    /// `owner/repo`.
    Repo,
    /// Emitting host. Degenerate on a local read — see the module docs.
    Host,
    /// UTC calendar day of the envelope's `emitted_at`.
    Day,
    /// The **tap** — `(runtime, credential source)` — that paid for the sweep
    /// (Issue #8556). See [`resolve_tap`] for how a record with no explicit
    /// `config["tap"]` stamp is placed.
    Tap,
}

impl GroupBy {
    /// Parse a `--group-by` value. Case-insensitive.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "arm" => Ok(Self::Arm),
            "model" => Ok(Self::Model),
            "repo" => Ok(Self::Repo),
            "host" => Ok(Self::Host),
            "day" => Ok(Self::Day),
            "tap" => Ok(Self::Tap),
            other => bail!(
                "unknown --group-by value {other:?} (expected one of: arm, model, repo, host, \
                 day, tap)"
            ),
        }
    }

    /// The wire/display spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arm => "arm",
            Self::Model => "model",
            Self::Repo => "repo",
            Self::Host => "host",
            Self::Day => "day",
            Self::Tap => "tap",
        }
    }
}

/// Where a row's arm label came from. Carried on every arm-grouped row so an
/// inferred arm is never read as an experiment's own stamp (#8055).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArmSource {
    /// An explicit `experiment_arm`/`experiment_id` stamp on the record.
    Explicit,
    /// Derived from the dispatched model by
    /// `sweep_experiment::infer_arm_from_model` — conflates "arm A" with "arm
    /// B escalated to Opus" and with an explicit `--model` pin.
    Inferred,
    /// No arm information at all; the record is in the `unknown` bucket.
    Unknown,
}

impl ArmSource {
    /// The wire/display spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Inferred => "inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// Resolve a record's arm and where that arm came from. Never returns an
/// error and never declines: an unstamped record resolves to
/// ([`UNKNOWN_GROUP`], [`ArmSource::Unknown`]).
#[must_use]
pub fn resolve_arm(record: &SweepOutcomeRecord) -> (String, ArmSource) {
    for key in EXPLICIT_ARM_KEYS {
        if let Some(value) = record.config.get(*key).filter(|v| !v.trim().is_empty()) {
            return (value.trim().to_string(), ArmSource::Explicit);
        }
    }
    if let Some(value) = record
        .config
        .get(INFERRED_ARM_KEY)
        .filter(|v| !v.trim().is_empty())
    {
        return (value.trim().to_string(), ArmSource::Inferred);
    }
    (UNKNOWN_GROUP.to_string(), ArmSource::Unknown)
}

/// The tap a record's spend belongs to — `<runtime>@<credential source>`
/// (Issue #8556).
///
/// Prefers the explicit `config["tap"]` stamp a post-#8556 native-harness spawn
/// writes (see `sweep_registry::outcome_journal`), which is the only source that
/// can name the *model profile* half — and therefore the only source that can
/// tell a flat-rate coding plan apart from a metered endpoint reached through
/// the same runtime.
///
/// Without that stamp the tap is **reconstructed** from the credential keys
/// #8447 already writes, so pre-#8556 history and the Claude/legacy-adapter
/// path (which writes no `# LOOM_LAUNCH` record at all) still land in a tap
/// bucket rather than being dropped from the report. Reconstruction never
/// invents a profile: a reconstructed key is always the bare runtime, and a
/// credential half nothing in the record names falls to [`UNKNOWN_GROUP`]
/// instead of being guessed at.
#[must_use]
pub fn resolve_tap(record: &SweepOutcomeRecord) -> String {
    let field = |key: &str| {
        record
            .config
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    };
    if let Some(tap) = field("tap") {
        return tap.to_string();
    }
    let runtime = field("runtime").unwrap_or(UNKNOWN_GROUP);
    let credential = match field("credential_source") {
        Some("pool") => field("credential_provider")
            .map_or_else(|| "api_keys".to_string(), |p| format!("api_keys:{p}")),
        Some(crate::launch_record::CREDENTIAL_WIRE_ENV) => {
            crate::launch_record::CREDENTIAL_WIRE_ENV.to_string()
        }
        Some("none") => crate::launch_record::CREDENTIAL_WIRE_HARNESS_OWN.to_string(),
        Some(other) => other.to_string(),
        // No API-key-pool attribution: the two subscription pools are a
        // property of the runtime itself, so naming them here is a reading of
        // the record rather than a guess.
        None => match runtime {
            "claude" => CredentialSource::ClaudeTokens.wire(),
            "codex" => CredentialSource::CodexAccounts.wire(),
            _ => UNKNOWN_GROUP.to_string(),
        },
    };
    format!("{runtime}@{credential}")
}

// ============================================================================
// Input
// ============================================================================

/// One journal line, with the envelope context grouping needs. Built by
/// [`collect_records`] from any number of workspaces.
#[derive(Debug, Clone)]
pub struct SummaryRecord {
    /// The emitting daemon's timestamp (`TelemetryEnvelope::emitted_at`) —
    /// the `--since` window and `--group-by day` both key off this.
    pub emitted_at: DateTime<Utc>,
    /// `TelemetryEnvelope::host_id`. See the module docs on degeneracy.
    pub host_id: String,
    /// The workspace root whose journal this line came from. Kept for
    /// provenance/reporting; `--group-by repo` uses the record's own `repo`.
    pub workspace: String,
    /// The unwrapped outcome record.
    pub record: SweepOutcomeRecord,
}

/// Read every `sweep.outcome` envelope from one workspace root's telemetry
/// journal. A missing journal yields an empty vec (a registered workspace
/// that has never run a sweep is not an error), and non-`SweepOutcome`
/// records are skipped — the same filter the existing CLI path applies.
#[must_use]
pub fn collect_records(workspace_root: &Path) -> Vec<SummaryRecord> {
    let path = sweep_outcomes::default_outcome_telemetry_path(workspace_root);
    let workspace = workspace_root.display().to_string();
    sweep_outcomes::read_all_outcome_telemetry(&path)
        .into_iter()
        .filter_map(|envelope| match envelope.record {
            TelemetryRecord::SweepOutcome(record) => Some(SummaryRecord {
                emitted_at: envelope.emitted_at,
                host_id: envelope.host_id,
                workspace: workspace.clone(),
                record,
            }),
            _ => None,
        })
        .collect()
}

/// A `sweep_id` -> death/crash classification index built from the **sibling**
/// `sweep-outcomes.jsonl` journal, which is where the class strings live until
/// #8056 puts `failure_class` on the telemetry record itself.
#[derive(Debug, Clone, Default)]
pub struct SpawnDeathIndex {
    classes: BTreeMap<String, Vec<String>>,
}

impl SpawnDeathIndex {
    /// Build from the sibling journal of one workspace root. A missing
    /// journal yields an empty index — the `< 60 s` heuristic still applies.
    #[must_use]
    pub fn from_workspace(workspace_root: &Path) -> Self {
        let path = sweep_outcomes::default_outcomes_path(workspace_root);
        let mut index = Self::default();
        index.absorb(&sweep_outcomes::read_all(&path));
        index
    }

    /// Merge another workspace's sibling-journal records into this index.
    pub fn absorb(&mut self, records: &[sweep_outcomes::OutcomeRecord]) {
        for record in records {
            let entry = self.classes.entry(record.sweep_id.clone()).or_default();
            for class in [
                record.death_class.as_deref(),
                record.crash_classification.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !entry.iter().any(|c| c == class) {
                    entry.push(class.to_string());
                }
            }
        }
    }

    /// Every class string known for `sweep_id`.
    #[must_use]
    pub fn classes_for(&self, sweep_id: &str) -> &[String] {
        self.classes.get(sweep_id).map_or(&[], Vec::as_slice)
    }

    /// Number of sweeps the index knows anything about (test/reporting aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }
}

/// Whether a class string names a spawn death — a run that died before doing
/// any work, which must not be counted against a model's failure rate.
///
/// Matches the ask's set: `preflight-token-selection-failed` exactly, and
/// `account-exhausted:*` by prefix (the suffix distinguishes `rate-limited` /
/// `rate-limit-abort` / `model-limit` / `model-credits-exhausted`; all four
/// are environmental, none is the sweep's fault).
#[must_use]
pub fn is_spawn_death_class(class: &str) -> bool {
    class == "preflight-token-selection-failed" || class.starts_with("account-exhausted:")
}

/// The record's failure class, once one is knowable.
///
/// **Single swap point for #8056**: when `failure_class` becomes a real field
/// on [`SweepOutcomeRecord`], the first branch becomes
/// `record.failure_class.as_deref()` and everything else here is unchanged.
#[must_use]
pub fn failure_class<'a>(
    record: &'a SweepOutcomeRecord,
    index: &'a SpawnDeathIndex,
) -> Vec<&'a str> {
    if let Some(class) = record
        .config
        .get(FAILURE_CLASS_KEY)
        .map(String::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        return vec![class];
    }
    index
        .classes_for(&record.sweep_id)
        .iter()
        .map(String::as_str)
        .collect()
}

/// Why a record counts as a spawn death, or `None` if it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnDeathReason {
    /// A class string matched [`is_spawn_death_class`]. Carries the string so
    /// the exclusion is auditable rather than a bare count.
    Class(String),
    /// No class was available, but the run was shorter than
    /// [`SPAWN_DEATH_MIN_DURATION_SEC`].
    TooShort(i64),
}

/// Classify a record as a spawn death (or not).
///
/// Only non-success, non-blocked records are eligible: a fast *success* is a
/// good sweep, not a death, and `blocked` is a human-decision terminal state
/// the daemon reaches deliberately. Failures and cancellations are both in
/// scope — a cancel that never got past token selection is exactly the shape
/// this filter exists to remove.
#[must_use]
pub fn spawn_death_reason(
    record: &SweepOutcomeRecord,
    index: &SpawnDeathIndex,
) -> Option<SpawnDeathReason> {
    if matches!(record.result, SweepResult::Success | SweepResult::Blocked) {
        return None;
    }
    for class in failure_class(record, index) {
        if is_spawn_death_class(class) {
            return Some(SpawnDeathReason::Class(class.to_string()));
        }
    }
    if record.total_duration_sec < SPAWN_DEATH_MIN_DURATION_SEC {
        return Some(SpawnDeathReason::TooShort(record.total_duration_sec));
    }
    None
}

/// Whether the sweep engaged the Doctor phase.
///
/// **Single swap point for #8056's `doctor_cycles`**: the config key is read
/// first, so this becomes `record.doctor_cycles > 0` in one line when the
/// real field lands. Until then it is the `phase_durations` approximation the
/// ask specifies — a sweep whose Doctor phase ran entirely inside one reaper
/// sampling gap can be missed.
#[must_use]
pub fn doctor_engaged(record: &SweepOutcomeRecord) -> bool {
    if let Some(raw) = record.config.get(DOCTOR_CYCLES_KEY) {
        if let Ok(cycles) = raw.trim().parse::<u64>() {
            return cycles > 0;
        }
    }
    record
        .phase_durations
        .iter()
        .any(|p| p.phase.eq_ignore_ascii_case("doctor"))
}

/// Cost-weight one record's `tokens_by_model` through [`RATE_CARD_ID`].
///
/// `None` (never `0.0`) when the record carries no `tokens_by_model` — the
/// same "unknown != zero" contract the field itself uses.
#[must_use]
pub fn weighted_tokens_usd(record: &SweepOutcomeRecord) -> Option<f64> {
    let totals = record.tokens_by_model.as_ref()?;
    let mut usd = 0.0;
    for t in totals {
        let pricing = ModelPricing::for_model(&t.model);
        usd += pricing.calculate_cost(
            t.input,
            t.output,
            Some(t.cache_read),
            Some(t.cache_write_5m + t.cache_write_1h),
        );
    }
    Some(usd)
}

// ============================================================================
// Merge join
// ============================================================================

/// One PR's merge state as far as this run could determine it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    /// The forge (or the cache) says merged.
    Merged,
    /// The forge (or the cache) says not merged.
    NotMerged,
    /// Could not be determined — forge unreachable, `gh` missing, join
    /// skipped. **Never collapsed into `NotMerged`**: the whole group's
    /// merged count degrades to "unavailable" instead (#8057 AC6).
    Unavailable,
}

/// The `pr_number` -> merged? join. A trait so the aggregation is testable
/// without a forge and so `--no-merge-join` is just another implementation.
pub trait MergeLookup {
    /// Merge state of `repo#pr`.
    fn merge_state(&mut self, repo: &str, pr: u32) -> MergeState;
}

/// The `--no-merge-join` implementation: answers `Unavailable` for everything
/// without touching the forge.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkipMergeJoin;

impl MergeLookup for SkipMergeJoin {
    fn merge_state(&mut self, _repo: &str, _pr: u32) -> MergeState {
        MergeState::Unavailable
    }
}

/// One cached `(repo, pr)` answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeCacheEntry {
    /// Whether the PR was merged when last observed.
    pub merged: bool,
    /// When that observation was made.
    pub checked_at: DateTime<Utc>,
}

/// How long a **negative** (not-yet-merged) answer stays fresh. A positive
/// answer never expires — a merged PR does not un-merge, so re-asking the
/// forge about it is pure waste, which is the point of the cache for a week
/// of 48 workspaces.
pub const MERGE_CACHE_NEGATIVE_TTL_HOURS: i64 = 6;

/// Environment override for the merge-join cache path (test seam).
pub const MERGE_CACHE_PATH_ENV: &str = "LOOM_PR_MERGE_CACHE_PATH";

/// The probe signature: `Some(merged)` on a definite answer, `None` when the
/// forge could not be reached. A plain `fn` pointer (not a closure) so the
/// lookup stays `Send`-free, cheap to clone, and trivially stubbable in tests.
pub type ForgeProbe = fn(&str, u32) -> Option<bool>;

/// Disk-cached merge join over `gh pr view --json mergedAt`.
///
/// Degradation contract: the **first** probe failure latches
/// [`Self::forge_failed`] and every subsequent uncached lookup answers
/// [`MergeState::Unavailable`] without another subprocess — one dead forge
/// must not cost 48 workspaces' worth of timeouts.
#[derive(Debug)]
pub struct CachedMergeLookup {
    path: PathBuf,
    entries: BTreeMap<String, MergeCacheEntry>,
    probe: ForgeProbe,
    dirty: bool,
    forge_failed: bool,
    probe_calls: usize,
    cache_hits: usize,
    now: DateTime<Utc>,
}

impl CachedMergeLookup {
    /// Default cache path: [`MERGE_CACHE_PATH_ENV`] when set and non-empty,
    /// else `~/.loom/cache/pr-merge-state.json` (host-level, shared by every
    /// workspace this command reads — that sharing is the whole saving).
    pub fn default_cache_path() -> Result<PathBuf> {
        if let Ok(path) = std::env::var(MERGE_CACHE_PATH_ENV) {
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home directory for the merge-join cache"))?;
        Ok(home.join(".loom").join("cache").join("pr-merge-state.json"))
    }

    /// Load (or start) the cache at `path`, probing through `probe`. An
    /// unreadable/corrupt cache file is treated as empty rather than fatal —
    /// this command is a read-only report, never worth failing over a cache.
    #[must_use]
    pub fn new(path: PathBuf, probe: ForgeProbe) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            entries,
            probe,
            dirty: false,
            forge_failed: false,
            probe_calls: 0,
            cache_hits: 0,
            now: Utc::now(),
        }
    }

    /// Load the default-path cache with the real `gh` probe.
    pub fn with_defaults() -> Result<Self> {
        Ok(Self::new(Self::default_cache_path()?, gh_pr_merged))
    }

    /// Whether the forge has failed at least once this run.
    #[must_use]
    pub fn forge_failed(&self) -> bool {
        self.forge_failed
    }

    /// How many forge subprocesses this run has spawned.
    #[must_use]
    pub fn probe_calls(&self) -> usize {
        self.probe_calls
    }

    /// How many lookups were served from the cache.
    #[must_use]
    pub fn cache_hits(&self) -> usize {
        self.cache_hits
    }

    /// The cache file path (reported in `--json` so a stale answer is
    /// traceable to a file an operator can delete).
    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.path
    }

    /// Persist the cache if anything changed. Best-effort: a write failure is
    /// logged, never propagated — the report is still correct without it.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        match serde_json::to_string(&self.entries) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&self.path, text) {
                    log::debug!("merge-join cache write failed at {}: {e}", self.path.display());
                } else {
                    self.dirty = false;
                }
            }
            Err(e) => log::debug!("merge-join cache serialization failed: {e}"),
        }
    }

    fn key(repo: &str, pr: u32) -> String {
        format!("{repo}#{pr}")
    }

    fn fresh(&self, entry: &MergeCacheEntry) -> bool {
        entry.merged
            || self.now - entry.checked_at < Duration::hours(MERGE_CACHE_NEGATIVE_TTL_HOURS)
    }
}

impl MergeLookup for CachedMergeLookup {
    fn merge_state(&mut self, repo: &str, pr: u32) -> MergeState {
        let key = Self::key(repo, pr);
        if let Some(entry) = self.entries.get(&key) {
            if self.fresh(entry) {
                self.cache_hits += 1;
                return if entry.merged {
                    MergeState::Merged
                } else {
                    MergeState::NotMerged
                };
            }
        }
        if self.forge_failed {
            return MergeState::Unavailable;
        }
        self.probe_calls += 1;
        match (self.probe)(repo, pr) {
            Some(merged) => {
                self.entries.insert(
                    key,
                    MergeCacheEntry {
                        merged,
                        checked_at: self.now,
                    },
                );
                self.dirty = true;
                if merged {
                    MergeState::Merged
                } else {
                    MergeState::NotMerged
                }
            }
            None => {
                self.forge_failed = true;
                MergeState::Unavailable
            }
        }
    }
}

/// The real probe: `gh pr view <pr> --repo <repo> --json mergedAt`.
///
/// `Some(true)`/`Some(false)` on a parsed answer, `None` on any failure
/// (missing `gh`, network error, deleted PR, unparseable output) — which the
/// caller turns into "unavailable", never into "not merged".
#[must_use]
pub fn gh_pr_merged(repo: &str, pr: u32) -> Option<bool> {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &pr.to_string(),
            "--repo",
            repo,
            "--json",
            "mergedAt",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let merged_at = value.get("mergedAt")?;
    Some(!merged_at.is_null())
}

// ============================================================================
// Report
// ============================================================================

/// The pricing card a report's weighted-token figures came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RateCard {
    /// Where the rates live — [`RATE_CARD_ID`].
    pub id: &'static str,
    /// The rates' as-of date — [`RATE_CARD_AS_OF`].
    pub as_of: &'static str,
    /// Why the number above it should not be over-trusted.
    pub caveat: &'static str,
}

impl Default for RateCard {
    fn default() -> Self {
        Self {
            id: RATE_CARD_ID,
            as_of: RATE_CARD_AS_OF,
            caveat: RATE_CARD_CAVEAT,
        }
    }
}

/// Provenance for the merged-PR column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergeJoinStatus {
    /// Whether the join was attempted at all (`false` under
    /// `--no-merge-join`).
    pub attempted: bool,
    /// Whether the forge failed during the run — when `true`, at least one
    /// group's `merged_prs` is `null`.
    pub degraded: bool,
    /// Cache file consulted, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,
}

/// One group's aggregate row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupRow {
    /// The group key (arm, model, `owner/repo`, host id, or `YYYY-MM-DD`).
    pub group: String,
    /// Only for `--group-by arm`: where this row's label came from. A row
    /// mixing explicit and inferred records reports `inferred` — the weaker
    /// provenance wins, so a marked row is never over-trusted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arm_source: Option<ArmSource>,
    /// Records in this group (after `--since` and any spawn-death exclusion).
    pub sweeps: usize,
    /// `result == success`.
    pub success: usize,
    /// `result == failure`.
    pub failure: usize,
    /// `result == cancelled`.
    pub cancelled: usize,
    /// `result == blocked`.
    pub blocked: usize,
    /// Records in this group classified as spawn deaths. Zero when
    /// `--exclude-spawn-deaths` already removed them (see the report-level
    /// `spawn_deaths_excluded`); otherwise a count of what `real_failure_rate`
    /// discounted.
    pub spawn_deaths: usize,
    /// Failures that are **not** spawn deaths.
    pub real_failures: usize,
    /// `real_failures / sweeps`; `0.0` for an empty group, never `NaN`.
    pub real_failure_rate: f64,
    /// Median `total_duration_sec` over **successes only**. `None` for a
    /// group with no successes (never `0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_success_duration_sec: Option<i64>,
    /// 75th-percentile `total_duration_sec` over successes only (nearest-rank).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p75_success_duration_sec: Option<i64>,
    /// Fraction of the group's sweeps that engaged Doctor. Approximate until
    /// #8056 — see [`doctor_engaged`].
    pub doctor_phase_rate: f64,
    /// PRs opened by this group's sweeps (`pr_number` present).
    pub prs_opened: usize,
    /// Merged PRs. **`None` means "could not be determined"**, never zero —
    /// see [`MergeState::Unavailable`].
    pub merged_prs: Option<usize>,
    /// How many of `prs_opened` the join could not resolve.
    pub merge_state_unknown: usize,
    /// Cost-weighted token total in US$ at [`RATE_CARD_ID`]. `None` when no
    /// record in the group carried `tokens_by_model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_tokens_usd: Option<f64>,
    /// How many of the group's sweeps contributed to `weighted_tokens_usd` —
    /// the denominator a reader needs before comparing two groups' costs.
    pub weighted_tokens_records: usize,
    /// The headline experiment metric: `merged_prs / weighted_tokens_usd`
    /// (merges per cost-weighted US$ of tokens, at the named rate card).
    /// `None` when either input is unavailable or the cost is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merges_per_weighted_token: Option<f64>,
    /// Mean `lines_added` per merged PR, over the merged PRs whose record
    /// actually carried the field (`lines_per_merged_pr_denominator`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_added_per_merged_pr: Option<f64>,
    /// Mean `lines_deleted` per merged PR, same denominator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_deleted_per_merged_pr: Option<f64>,
    /// How many merged PRs the lines-per-PR means actually averaged over —
    /// stated rather than implied, because `lines_added`/`lines_deleted` are
    /// `Option` and a partial denominator is not the same as a full one.
    pub lines_per_merged_pr_denominator: usize,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SummaryReport {
    /// When the report was produced.
    pub generated_at: DateTime<Utc>,
    /// The `--group-by` dimension.
    pub group_by: GroupBy,
    /// The `--since` cutoff, when one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Workspace roots read, in registry order.
    pub workspaces: Vec<String>,
    /// Envelopes unwrapped across every workspace, before `--since`.
    pub records_read: usize,
    /// Records inside the `--since` window.
    pub records_in_window: usize,
    /// Records removed by `--exclude-spawn-deaths` (always `0` without it —
    /// the exclusion is reported, never silent).
    pub spawn_deaths_excluded: usize,
    /// Records actually aggregated. **Invariant:** equals the sum of every
    /// row's `sweeps`, and equals `records_in_window - spawn_deaths_excluded`.
    pub records_grouped: usize,
    /// Whether `--exclude-spawn-deaths` was in effect.
    pub exclude_spawn_deaths: bool,
    /// The pricing card behind every weighted-token figure.
    pub rate_card: RateCard,
    /// Provenance for the merged-PR column.
    pub merge_join: MergeJoinStatus,
    /// One row per group, ordered by `sweeps` descending then group name.
    pub rows: Vec<GroupRow>,
    /// Caveats that apply to this particular report (degenerate host
    /// grouping, inferred arms, an unavailable merge join, ...).
    pub notes: Vec<String>,
}

/// Knobs for [`summarize`].
#[derive(Debug, Clone, Copy)]
pub struct SummaryOptions {
    /// Grouping dimension.
    pub group_by: GroupBy,
    /// Only consider records emitted at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Drop spawn deaths entirely instead of only discounting them from
    /// `real_failure_rate`.
    pub exclude_spawn_deaths: bool,
    /// Whether the merge join was attempted (reported, not enforced — pass
    /// [`SkipMergeJoin`] as the lookup to actually skip it).
    pub merge_join_attempted: bool,
}

/// Parse a `--since` value: `<N>[m|h|d|w]` relative to `now`, or an absolute
/// `YYYY-MM-DD` (interpreted as that day's 00:00:00 UTC).
pub fn parse_since(spec: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("--since requires a value (e.g. 7d, 36h, 2026-09-22)");
    }
    if let Ok(date) = NaiveDate::parse_from_str(spec, "%Y-%m-%d") {
        return Ok(DateTime::from_naive_utc_and_offset(
            date.and_hms_opt(0, 0, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid date {spec:?}"))?,
            Utc,
        ));
    }
    let (digits, unit) = spec.split_at(spec.len() - 1);
    let n: i64 = digits.parse().map_err(|_| {
        anyhow::anyhow!("unparseable --since {spec:?} (expected e.g. 7d, 36h, 90m, 2w, 2026-09-22)")
    })?;
    if n < 0 {
        bail!("--since must not be negative: {spec:?}");
    }
    let delta = match unit {
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        other => bail!("unknown --since unit {other:?} (expected m, h, d, w, or YYYY-MM-DD)"),
    };
    Ok(now - delta)
}

/// Integer median of an ascending slice: the average of the two central
/// values for an even-sized input (matching
/// [`crate::sweep_outcomes::summarize_by_model`]'s convention). `None` for an
/// empty slice — never `0`.
#[must_use]
fn median_sorted(sorted: &[i64]) -> Option<i64> {
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[n / 2]),
        n => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2),
    }
}

/// Nearest-rank 75th percentile of an ascending slice: the value at
/// `ceil(0.75 * n)`, 1-indexed. `None` for an empty slice. For a
/// single-element slice this is that element, which is the only defensible
/// answer.
#[must_use]
fn p75_sorted(sorted: &[i64]) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    // ceil(3n/4), 1-indexed, clamped into range.
    let rank = (3 * n).div_ceil(4).max(1);
    Some(sorted[rank.min(n) - 1])
}

/// Per-group accumulator.
#[derive(Default)]
struct Accum {
    sweeps: usize,
    success: usize,
    failure: usize,
    cancelled: usize,
    blocked: usize,
    spawn_deaths: usize,
    real_failures: usize,
    success_durations: Vec<i64>,
    doctor: usize,
    prs_opened: usize,
    merged: usize,
    merge_unknown: usize,
    weighted_usd: f64,
    weighted_records: usize,
    lines_added_merged: i64,
    lines_deleted_merged: i64,
    lines_denominator: usize,
    arm_sources: BTreeSet<&'static str>,
}

/// Aggregate `records` into a [`SummaryReport`].
///
/// Read-only and side-effect-free apart from whatever `lookup` does. Nothing
/// is ever dropped for want of a group key: an unstamped arm, a model-less
/// record and an empty repo string all land in a visible bucket, which is
/// what makes the `records_grouped == sum(rows.sweeps)` invariant meaningful.
pub fn summarize(
    records: &[SummaryRecord],
    index: &SpawnDeathIndex,
    opts: SummaryOptions,
    lookup: &mut dyn MergeLookup,
    workspaces: Vec<String>,
) -> SummaryReport {
    let records_read = records.len();
    let in_window: Vec<&SummaryRecord> = records
        .iter()
        .filter(|r| opts.since.is_none_or(|since| r.emitted_at >= since))
        .collect();
    let records_in_window = in_window.len();

    let mut spawn_deaths_excluded = 0usize;
    let mut groups: BTreeMap<String, Accum> = BTreeMap::new();
    let mut any_inferred_arm = false;
    let mut merge_degraded = false;

    for entry in in_window {
        let record = &entry.record;
        let death = spawn_death_reason(record, index);
        if opts.exclude_spawn_deaths && death.is_some() {
            spawn_deaths_excluded += 1;
            continue;
        }

        let (key, arm_source) = match opts.group_by {
            GroupBy::Arm => {
                let (arm, source) = resolve_arm(record);
                if source == ArmSource::Inferred {
                    any_inferred_arm = true;
                }
                (arm, Some(source))
            }
            GroupBy::Model => (
                record
                    .model
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
                None,
            ),
            GroupBy::Repo => {
                let repo = record.repo.trim();
                let repo = if repo.is_empty() { UNKNOWN_GROUP } else { repo };
                (repo.to_string(), None)
            }
            GroupBy::Host => {
                let host = entry.host_id.trim();
                let host = if host.is_empty() { UNKNOWN_GROUP } else { host };
                (host.to_string(), None)
            }
            GroupBy::Day => (entry.emitted_at.format("%Y-%m-%d").to_string(), None),
            GroupBy::Tap => (resolve_tap(record), None),
        };

        let acc = groups.entry(key).or_default();
        if let Some(source) = arm_source {
            acc.arm_sources.insert(source.as_str());
        }
        acc.sweeps += 1;
        match record.result {
            SweepResult::Success => {
                acc.success += 1;
                acc.success_durations.push(record.total_duration_sec);
            }
            SweepResult::Failure => acc.failure += 1,
            SweepResult::Cancelled => acc.cancelled += 1,
            SweepResult::Blocked => acc.blocked += 1,
        }
        if death.is_some() {
            acc.spawn_deaths += 1;
        } else if record.result == SweepResult::Failure {
            acc.real_failures += 1;
        }
        if doctor_engaged(record) {
            acc.doctor += 1;
        }
        if let Some(usd) = weighted_tokens_usd(record) {
            acc.weighted_usd += usd;
            acc.weighted_records += 1;
        }
        if let Some(pr) = record.pr_number {
            acc.prs_opened += 1;
            match lookup.merge_state(&record.repo, pr) {
                MergeState::Merged => {
                    acc.merged += 1;
                    if let (Some(added), Some(deleted)) = (record.lines_added, record.lines_deleted)
                    {
                        acc.lines_added_merged += added;
                        acc.lines_deleted_merged += deleted;
                        acc.lines_denominator += 1;
                    }
                }
                MergeState::NotMerged => {}
                MergeState::Unavailable => {
                    acc.merge_unknown += 1;
                    merge_degraded = true;
                }
            }
        }
    }

    let mut rows: Vec<GroupRow> = groups
        .into_iter()
        .map(|(group, mut acc)| {
            acc.success_durations.sort_unstable();
            let merged_prs = if acc.merge_unknown > 0 {
                None
            } else {
                Some(acc.merged)
            };
            let weighted = (acc.weighted_records > 0).then_some(acc.weighted_usd);
            let merges_per_weighted_token = match (merged_prs, weighted) {
                (Some(merged), Some(usd)) if usd > 0.0 =>
                {
                    #[allow(clippy::cast_precision_loss)]
                    Some(merged as f64 / usd)
                }
                _ => None,
            };
            #[allow(clippy::cast_precision_loss)]
            let denom = acc.lines_denominator as f64;
            let (added_per, deleted_per) = if acc.lines_denominator > 0 {
                #[allow(clippy::cast_precision_loss)]
                (
                    Some(acc.lines_added_merged as f64 / denom),
                    Some(acc.lines_deleted_merged as f64 / denom),
                )
            } else {
                (None, None)
            };
            #[allow(clippy::cast_precision_loss)]
            let sweeps_f = acc.sweeps as f64;
            let arm_source = if opts.group_by == GroupBy::Arm {
                Some(if group == UNKNOWN_GROUP {
                    ArmSource::Unknown
                } else if acc.arm_sources.contains("inferred") {
                    ArmSource::Inferred
                } else if acc.arm_sources.contains("explicit") {
                    ArmSource::Explicit
                } else {
                    ArmSource::Unknown
                })
            } else {
                None
            };
            GroupRow {
                group,
                arm_source,
                sweeps: acc.sweeps,
                success: acc.success,
                failure: acc.failure,
                cancelled: acc.cancelled,
                blocked: acc.blocked,
                spawn_deaths: acc.spawn_deaths,
                real_failures: acc.real_failures,
                #[allow(clippy::cast_precision_loss)]
                real_failure_rate: if acc.sweeps == 0 {
                    0.0
                } else {
                    acc.real_failures as f64 / sweeps_f
                },
                median_success_duration_sec: median_sorted(&acc.success_durations),
                p75_success_duration_sec: p75_sorted(&acc.success_durations),
                #[allow(clippy::cast_precision_loss)]
                doctor_phase_rate: if acc.sweeps == 0 {
                    0.0
                } else {
                    acc.doctor as f64 / sweeps_f
                },
                prs_opened: acc.prs_opened,
                merged_prs,
                merge_state_unknown: acc.merge_unknown,
                weighted_tokens_usd: weighted,
                weighted_tokens_records: acc.weighted_records,
                merges_per_weighted_token,
                lines_added_per_merged_pr: added_per,
                lines_deleted_per_merged_pr: deleted_per,
                lines_per_merged_pr_denominator: acc.lines_denominator,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.sweeps.cmp(&a.sweeps).then_with(|| a.group.cmp(&b.group)));

    let records_grouped = rows.iter().map(|r| r.sweeps).sum();

    let mut notes = Vec::new();
    if opts.group_by == GroupBy::Host {
        notes.push(
            "--group-by host is degenerate on a local read: TelemetryEnvelope stamps the EMITTING \
             host, so every journal read from this host's own workspaces carries one host_id. The \
             grouping is meaningful only over a pooled/exported corpus."
                .to_string(),
        );
    }
    if any_inferred_arm {
        notes.push(
            "at least one arm was INFERRED from the dispatched model (issue #8055): it conflates \
             'arm A' with 'arm B escalated to Opus via the ladder' and with an explicit --model \
             pin. See each row's arm_source."
                .to_string(),
        );
    }
    if opts.group_by == GroupBy::Arm && rows.iter().any(|r| r.group == UNKNOWN_GROUP) {
        notes.push(
            "records with no arm stamp are bucketed as 'unknown' rather than dropped — group \
             counts still sum to records_grouped."
                .to_string(),
        );
    }
    if !opts.merge_join_attempted {
        notes.push(
            "merge join skipped (--no-merge-join): merged_prs is unavailable, NOT zero."
                .to_string(),
        );
    } else if merge_degraded {
        notes.push(
            "the forge could not be reached for at least one PR: the affected groups report \
             merged_prs as unavailable (null), NOT zero."
                .to_string(),
        );
    }
    if index.is_empty() {
        notes.push(
            "no sibling sweep-outcomes.jsonl classifications were found: spawn-death detection \
             fell back to the total_duration_sec < 60 heuristic alone."
                .to_string(),
        );
    }

    SummaryReport {
        generated_at: Utc::now(),
        group_by: opts.group_by,
        since: opts.since,
        workspaces,
        records_read,
        records_in_window,
        spawn_deaths_excluded,
        records_grouped,
        exclude_spawn_deaths: opts.exclude_spawn_deaths,
        rate_card: RateCard::default(),
        merge_join: MergeJoinStatus {
            attempted: opts.merge_join_attempted,
            degraded: merge_degraded,
            cache_path: None,
        },
        rows,
        notes,
    }
}

/// Read every workspace root in `roots` and aggregate them into one report.
///
/// This is the whole read path in one call: per-root telemetry journals,
/// per-root sibling `sweep-outcomes.jsonl` classifications for the
/// spawn-death join, and the aggregation. A root with no journal contributes
/// nothing and is not an error — a registered workspace that has never run a
/// sweep is a normal state, not a failure.
pub fn summarize_workspaces(
    roots: &[PathBuf],
    opts: SummaryOptions,
    lookup: &mut dyn MergeLookup,
) -> SummaryReport {
    let mut records = Vec::new();
    let mut index = SpawnDeathIndex::default();
    let mut names = Vec::with_capacity(roots.len());
    for root in roots {
        records.extend(collect_records(root));
        index.absorb(&sweep_outcomes::read_all(&sweep_outcomes::default_outcomes_path(root)));
        names.push(root.display().to_string());
    }
    summarize(&records, &index, opts, lookup, names)
}

// ============================================================================
// Human rendering
// ============================================================================

fn fmt_opt_i64(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_string(), |n| n.to_string())
}

fn fmt_opt_f64(v: Option<f64>, places: usize) -> String {
    v.map_or_else(|| "-".to_string(), |n| format!("{n:.places$}"))
}

/// Render `report` as the human-readable table.
#[must_use]
pub fn render_text(report: &SummaryReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("sweep.outcome summary — grouped by {}\n", report.group_by.as_str()));
    out.push_str(&format!(
        "  window:     {}\n",
        report
            .since
            .map_or_else(|| "all records".to_string(), |s| format!("since {}", s.to_rfc3339()))
    ));
    out.push_str(&format!(
        "  workspaces: {} ({})\n",
        report.workspaces.len(),
        if report.workspaces.len() == 1 {
            report.workspaces.first().cloned().unwrap_or_default()
        } else {
            "all registered".to_string()
        }
    ));
    out.push_str(&format!(
        "  records:    {} read, {} in window, {} spawn death(s) excluded, {} summarized\n",
        report.records_read,
        report.records_in_window,
        report.spawn_deaths_excluded,
        report.records_grouped
    ));
    out.push_str(&format!(
        "  rate card:  {} (as of {})\n",
        report.rate_card.id, report.rate_card.as_of
    ));
    out.push('\n');

    if report.rows.is_empty() {
        out.push_str("No sweep.outcome records matched.\n");
        return out;
    }

    let arm = report.group_by == GroupBy::Arm;
    if arm {
        out.push_str(&format!(
            "{:<16} {:<9} {:>6} {:>5} {:>5} {:>5} {:>5} {:>9} {:>7} {:>7} {:>8} {:>7} {:>9} {:>9} {:>8} {:>8}\n",
            "GROUP", "ARM_SRC", "SWEEPS", "SUCC", "FAIL", "CANC", "BLKD", "REALFAIL", "MED_S",
            "P75_S", "DOCTOR%", "MERGED", "WTOK_USD", "MERGE/USD", "+L/PR", "-L/PR"
        ));
    } else {
        out.push_str(&format!(
            "{:<26} {:>6} {:>5} {:>5} {:>5} {:>5} {:>9} {:>7} {:>7} {:>8} {:>7} {:>9} {:>9} {:>8} {:>8}\n",
            "GROUP", "SWEEPS", "SUCC", "FAIL", "CANC", "BLKD", "REALFAIL", "MED_S", "P75_S",
            "DOCTOR%", "MERGED", "WTOK_USD", "MERGE/USD", "+L/PR", "-L/PR"
        ));
    }

    for row in &report.rows {
        let merged = row
            .merged_prs
            .map_or_else(|| "n/a".to_string(), |m| m.to_string());
        let tail = format!(
            "{:>6} {:>5} {:>5} {:>5} {:>5} {:>8.1}% {:>7} {:>7} {:>7.1}% {:>7} {:>9} {:>9} {:>8} {:>8}",
            row.sweeps,
            row.success,
            row.failure,
            row.cancelled,
            row.blocked,
            row.real_failure_rate * 100.0,
            fmt_opt_i64(row.median_success_duration_sec),
            fmt_opt_i64(row.p75_success_duration_sec),
            row.doctor_phase_rate * 100.0,
            merged,
            fmt_opt_f64(row.weighted_tokens_usd, 2),
            fmt_opt_f64(row.merges_per_weighted_token, 3),
            fmt_opt_f64(row.lines_added_per_merged_pr, 0),
            fmt_opt_f64(row.lines_deleted_per_merged_pr, 0),
        );
        if arm {
            out.push_str(&format!(
                "{:<16} {:<9} {tail}\n",
                row.group,
                row.arm_source.map_or("-", ArmSource::as_str)
            ));
        } else {
            out.push_str(&format!("{:<26} {tail}\n", row.group));
        }
    }

    out.push('\n');
    out.push_str(&format!("Rate card: {}\n", report.rate_card.caveat));
    out.push_str(
        "Doctor% is approximate: derived from sampled phase_durations until doctor_cycles lands \
         (#8056).\n",
    );
    for note in &report.notes {
        out.push_str(&format!("Note: {note}\n"));
    }
    out.push_str(
        "This is the richer superset of the bare `sweep-outcomes` summary, which is unchanged; \
         `real_failure_rate` here discounts spawn deaths and is NOT that summary's success_rate.\n",
    );
    out
}

#[cfg(test)]
mod tests;
