//! Config-driven N-arm assignment + budget-fraction sampling for the sweep
//! model-cost experiment (issue #9122, Phase 1 of the multi-model framework).
//!
//! The #3725 experiment shipped with exactly **two** hardcoded Claude arms
//! ([`super::sweep_experiment::ARM_MODEL`] — `A = opus`, `B = sonnet`) and no
//! way to spend only part of a budget on the experiment: every eligible issue
//! got a forced arm. This module generalizes both axes **without changing a
//! single byte of unconfigured behavior**:
//!
//! * `.loom/config.json` → `sweep.modelExperimentArms` — an array of
//!   `{id, model, weight}` declaring 2+ weighted arms. Absent (or rejected),
//!   [`ExperimentArms::load`] returns the built-in A/B pair and assignment
//!   falls back to [`super::sweep_experiment::assign_arm`]'s exact parity
//!   split, so an unconfigured repo is byte-for-byte unchanged.
//! * `.loom/config.json` → `sweep.modelExperimentBudgetFraction`, env
//!   `LOOM_MODEL_EXPERIMENT_BUDGET_FRACTION` (env > config > default `1.0`) —
//!   the fraction of experiment-eligible issues that receive **any** forced
//!   arm. The default `1.0` reproduces today's always-forced behavior exactly
//!   (it does not even evaluate the hash).
//!
//! # Two independent hashes, on purpose
//!
//! The in/out-of-experiment decision ([`in_experiment`]) and the arm choice
//! ([`ExperimentArms::assign`]) use **separately domain-separated** hashes of
//! the same `(issue, complexity-stratum)` key. Changing the budget fraction
//! therefore re-partitions *who is in the experiment* without reshuffling
//! *which arm* an in-experiment issue lands on — so a canary can be widened or
//! narrowed mid-flight and the arm attribution of every already-recorded issue
//! stays valid.
//!
//! Both hashes are pure functions of `(issue_number, stratum)`: assignment
//! stays deterministic and resume-safe (a killed-and-resumed sweep re-lands the
//! same arm) and stratified by the Curator complexity marker, exactly as the
//! 2-arm original. The domain strings below are part of that contract —
//! changing one reshuffles every future assignment.
//!
//! # Scope boundary (Phase 1)
//!
//! Arms are **Claude-runtime only**. An arm naming a non-Claude `runtime` is
//! rejected loudly rather than silently ignored or dispatched — non-Claude
//! runtime arms are Phase 2 (see issue #9122). An arm resolving to `fable` is
//! likewise rejected, mirroring `resolve_tier_model`'s No-Fable refusal
//! (#3702): warn, fall through, never fail the sweep.

use serde_json::Value;

use super::model_tiers;
use super::sweep_experiment::ARM_MODEL;

/// `.loom/config.json` key declaring the weighted arm roster.
pub const ARMS_CONFIG_KEY: &str = "sweep.modelExperimentArms";
/// `.loom/config.json` key holding the experiment budget fraction.
pub const BUDGET_FRACTION_CONFIG_KEY: &str = "sweep.modelExperimentBudgetFraction";
/// Env override for [`BUDGET_FRACTION_CONFIG_KEY`] (highest precedence).
pub const BUDGET_FRACTION_ENV: &str = "LOOM_MODEL_EXPERIMENT_BUDGET_FRACTION";

/// Domain separator for the arm-selection hash. Part of the determinism
/// contract — changing it reshuffles every future assignment.
const ARM_DOMAIN: &str = "loom.sweep.experiment.arm/v1";
/// Domain separator for the in/out-of-experiment hash. Deliberately distinct
/// from [`ARM_DOMAIN`] so the two decisions are independent.
const BUDGET_DOMAIN: &str = "loom.sweep.experiment.budget/v1";

/// One configured experiment arm.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentArm {
    /// The arm's reporting identity, upper-cased (`A`, `B`, `HAIKU`, …).
    /// Upper-casing matches the stats store's own convention
    /// ([`super::sweep_experiment::build_record`] upper-cases `arm`, and
    /// `harvest` keys on the upper-cased value), so a configured id round-trips
    /// through record → harvest unchanged.
    pub id: String,
    /// The logical model alias (or pinned ID) this arm forces for the Builder.
    pub model: String,
    /// Relative selection weight (> 0). Weights need not sum to anything.
    pub weight: f64,
}

impl ExperimentArm {
    /// The concrete model ID to dispatch, resolved through the SAME resolver
    /// `resolve-model.sh` uses (the #4060 contract) so an arm's alias and the
    /// escalation ladder can never disagree about what `opus` means.
    #[must_use]
    pub fn resolved_model(&self, config: &Value) -> String {
        if self.model.is_empty() {
            String::new()
        } else {
            model_tiers::resolve_model(&self.model, config)
        }
    }
}

/// The effective arm roster: either the built-in A/B pair or a validated
/// config-declared roster.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentArms {
    arms: Vec<ExperimentArm>,
    configured: bool,
}

impl ExperimentArms {
    /// The built-in, unconfigured A/B pair (`A = opus`, `B = sonnet`).
    #[must_use]
    pub fn default_pair() -> Self {
        Self {
            arms: ARM_MODEL
                .iter()
                .map(|(id, model)| ExperimentArm {
                    id: (*id).to_string(),
                    model: (*model).to_string(),
                    weight: 1.0,
                })
                .collect(),
            configured: false,
        }
    }

    /// Load `sweep.modelExperimentArms`, returning `(roster, warnings)`.
    ///
    /// **Any** validation failure rejects the WHOLE roster and falls through to
    /// [`Self::default_pair`] with a loud warning — a partially-honored roster
    /// would silently change the weights the operator asked for, which is worse
    /// than ignoring the block. Rejection reasons:
    ///
    /// * not an array, or fewer than 2 arms (an "experiment" with one arm is
    ///   not an experiment — use a tier pin instead);
    /// * an entry that is not an object, or has a blank `id` / `model`;
    /// * a duplicate `id` (arm ids are the stats store's grouping key);
    /// * a `weight` that is not a finite number greater than zero;
    /// * a `runtime` naming anything but `claude` (Phase 2 scope boundary);
    /// * a `model` that names or resolves to `fable` (No-Fable bound, #3702).
    #[must_use]
    pub fn load(config: &Value) -> (Self, Vec<String>) {
        let mut warnings: Vec<String> = Vec::new();
        let Some(raw) = crate::config_resolver::get_path(config, ARMS_CONFIG_KEY) else {
            return (Self::default_pair(), warnings);
        };
        if raw.is_null() {
            return (Self::default_pair(), warnings);
        }
        let Some(items) = raw.as_array() else {
            warnings.push(format!(
                "{ARMS_CONFIG_KEY} is not an array — ignoring it and falling through to the \
                 built-in A/B pair"
            ));
            return (Self::default_pair(), warnings);
        };
        if items.len() < 2 {
            warnings.push(format!(
                "{ARMS_CONFIG_KEY} declares {} arm(s); at least 2 are required — falling through \
                 to the built-in A/B pair",
                items.len()
            ));
            return (Self::default_pair(), warnings);
        }

        let mut arms: Vec<ExperimentArm> = Vec::with_capacity(items.len());
        for (idx, item) in items.iter().enumerate() {
            match parse_arm(item, idx, config) {
                Ok(arm) => {
                    if arms.iter().any(|a| a.id == arm.id) {
                        warnings.push(format!(
                            "{ARMS_CONFIG_KEY}[{idx}] repeats arm id '{}' — ids are the stats \
                             store's grouping key and must be unique",
                            arm.id
                        ));
                    } else {
                        arms.push(arm);
                    }
                }
                Err(why) => warnings.push(why),
            }
        }

        if arms.len() == items.len() {
            return (
                Self {
                    arms,
                    configured: true,
                },
                warnings,
            );
        }
        warnings.push(format!(
            "{ARMS_CONFIG_KEY} was REJECTED (see the warning(s) above) — falling through to the \
             built-in A/B pair, exactly as if the key were absent"
        ));
        (Self::default_pair(), warnings)
    }

    /// The validated arms, in configured order.
    #[must_use]
    pub fn arms(&self) -> &[ExperimentArm] {
        &self.arms
    }

    /// `true` when the roster came from config (so [`Self::assign`] uses the
    /// weighted hash); `false` for the built-in pair (legacy parity split).
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.configured
    }

    /// Deterministically assign an arm for `issue_number` within its
    /// complexity stratum.
    ///
    /// * **Unconfigured roster** — delegates to
    ///   [`super::sweep_experiment::assign_arm`], so the A/B parity split (and
    ///   therefore every existing assertion about it) is preserved
    ///   byte-for-byte.
    /// * **Configured roster** — a weighted pick from the domain-separated
    ///   [`ARM_DOMAIN`] hash of `(stratum, issue)`. Pure, so a resumed sweep
    ///   re-lands the same arm; stratified, so `complex` and `routine` each
    ///   converge to the configured weights independently.
    #[must_use]
    pub fn assign(&self, issue_number: i64, complexity: Option<&str>) -> &ExperimentArm {
        if !self.configured {
            let legacy = super::sweep_experiment::assign_arm(issue_number, complexity);
            if let Some(arm) = self.arms.iter().find(|a| a.id == legacy) {
                return arm;
            }
        }
        let stratum = super::sweep_experiment::normalize_complexity(complexity);
        let total: f64 = self.arms.iter().map(|a| a.weight).sum();
        let mut target = unit_hash(ARM_DOMAIN, issue_number, stratum) * total;
        for arm in &self.arms {
            target -= arm.weight;
            if target < 0.0 {
                return arm;
            }
        }
        // Unreachable for a validated roster (every weight > 0 and the hash is
        // < 1.0); float rounding at the very top of the range lands here.
        self.arms.last().expect("a roster is never empty")
    }
}

/// Validate one `sweep.modelExperimentArms[]` entry.
fn parse_arm(item: &Value, idx: usize, config: &Value) -> Result<ExperimentArm, String> {
    let Some(obj) = item.as_object() else {
        return Err(format!("{ARMS_CONFIG_KEY}[{idx}] is not an object"));
    };
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_uppercase();
    if id.is_empty() {
        return Err(format!("{ARMS_CONFIG_KEY}[{idx}] has no non-empty 'id'"));
    }
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if model.is_empty() {
        return Err(format!("{ARMS_CONFIG_KEY}[{idx}] (arm '{id}') has no non-empty 'model'"));
    }

    // Phase 2 scope boundary: a non-Claude runtime arm is refused LOUDLY, never
    // silently ignored and never dispatched (issue #9122).
    if let Some(rt) = obj.get("runtime") {
        let name = rt.as_str().unwrap_or("").trim().to_lowercase();
        if name != "claude" {
            return Err(format!(
                "{ARMS_CONFIG_KEY}[{idx}] (arm '{id}') names runtime {rt} — the model-cost \
                 experiment is Claude-only in this phase; non-Claude runtime arms are refused \
                 (see issue #9122 Phase 2), not silently ignored"
            ));
        }
    }

    let weight = match obj.get("weight") {
        None | Some(Value::Null) => 1.0,
        Some(v) => {
            let n = v.as_f64().unwrap_or(f64::NAN);
            if !n.is_finite() || n <= 0.0 {
                return Err(format!(
                    "{ARMS_CONFIG_KEY}[{idx}] (arm '{id}') has weight {v}; expected a finite \
                     number greater than zero"
                ));
            }
            n
        }
    };

    // No-Fable bound (#3702), checked before AND after alias resolution — the
    // same two-sided refusal `model_tiers::resolve_tier_model` applies, so an
    // arm cannot reach fable through a `sweep.modelAliases` indirection either.
    if base_of(&model).contains("fable") {
        return Err(format!(
            "{ARMS_CONFIG_KEY}[{idx}] (arm '{id}') names 'fable' — refusing (No-Fable bound, \
             #3702)"
        ));
    }
    let resolved = model_tiers::resolve_model(&model, config);
    if base_of(&resolved).contains("fable") {
        return Err(format!(
            "{ARMS_CONFIG_KEY}[{idx}] (arm '{id}') model '{model}' resolves to '{resolved}', a \
             fable model — refusing (No-Fable bound, #3702)"
        ));
    }

    Ok(ExperimentArm { id, model, weight })
}

/// The `model` half of the `model@effort` grammar, trimmed and lower-cased.
fn base_of(model: &str) -> String {
    model
        .split_once('@')
        .map_or(model, |(b, _)| b)
        .trim()
        .to_lowercase()
}

/// Resolve the experiment budget fraction **env > config > default `1.0`** —
/// the same string-valued precedence pattern as `sweep.modelExperiment` itself.
///
/// Returns `(fraction, warnings)`. A malformed or out-of-range value warns and
/// falls back to `1.0` (today's always-forced behavior); it never fails a
/// sweep. `1.0` means "every eligible issue is in the experiment", `0.0` means
/// "none are" (the experiment is effectively parked without editing the mode).
#[must_use]
pub fn resolve_budget_fraction(env_value: Option<&str>, config: &Value) -> (f64, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();

    if let Some(raw) = env_value.map(str::trim).filter(|v| !v.is_empty()) {
        return match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && (0.0..=1.0).contains(&v) => (v, warnings),
            _ => {
                warnings.push(format!(
                    "{BUDGET_FRACTION_ENV}={raw:?} is not a number in [0.0, 1.0]; using 1.0 (all \
                     eligible issues in the experiment)"
                ));
                (1.0, warnings)
            }
        };
    }

    if let Some(v) = crate::config_resolver::get_path(config, BUDGET_FRACTION_CONFIG_KEY) {
        if v.is_null() {
            return (1.0, warnings);
        }
        if let Some(n) = v.as_f64() {
            if n.is_finite() && (0.0..=1.0).contains(&n) {
                return (n, warnings);
            }
        }
        warnings.push(format!(
            "{BUDGET_FRACTION_CONFIG_KEY}={v} is not a number in [0.0, 1.0]; using 1.0 (all \
             eligible issues in the experiment)"
        ));
    }

    (1.0, warnings)
}

/// Is this issue sampled INTO the experiment under `fraction`?
///
/// Deterministic and resume-safe, on a hash domain-separated from the
/// arm-selection hash (see the module docs). `fraction >= 1.0` short-circuits
/// to `true` WITHOUT hashing, so the default path is provably identical to the
/// pre-#9122 "every eligible issue gets an arm" behavior.
#[must_use]
pub fn in_experiment(issue_number: i64, complexity: Option<&str>, fraction: f64) -> bool {
    if !fraction.is_finite() || fraction >= 1.0 {
        return true;
    }
    if fraction <= 0.0 {
        return false;
    }
    let stratum = super::sweep_experiment::normalize_complexity(complexity);
    unit_hash(BUDGET_DOMAIN, issue_number, stratum) < fraction
}

/// The loud banner for an issue sampled OUT of the experiment by the budget
/// fraction. Mirrors [`super::sweep_experiment::format_banner`]'s shape — same
/// bar width, same `mode=` / canary-source lines — so the sweep log reads the
/// same either way and an operator can still see which signal confirmed the
/// canary.
#[must_use]
pub fn format_budget_skip_banner(issue: i64, fraction: f64, canary_source: Option<&str>) -> String {
    let bar = "=".repeat(72);
    let mut lines = vec![
        bar.clone(),
        format!("  LOOM MODEL EXPERIMENT — mode=EXPERIMENT  issue #{issue}"),
        format!("  NOT IN EXPERIMENT — sampled out by budget fraction {fraction}"),
        "  No arm, no model forcing: normal tier-2.5/tier-3 resolution applies.".to_string(),
        "  Stats -> .loom/stats/sweep-model-stats.jsonl (arm null)".to_string(),
    ];
    if let Some(src) = canary_source {
        lines.push(format!("  Canary confirmed by: {src}"));
    }
    lines.push(bar);
    lines.join("\n")
}

/// The per-arm comparison tail of [`format_harvest_text`].
///
/// Both halves are **additive and independent**, exactly as before #9122:
///
/// * Whenever named arms `A` **and** `B` are both present, the #3718
///   inequality-inputs text is emitted first, byte-for-byte unchanged.
/// * Whenever the named arms are anything **other than** exactly `{A, B}` (and
///   there are at least 2 of them), the generalized per-arm table follows.
///
/// So the legacy pair gets only the first block (byte-identical output), an
/// N-arm roster that happens to include `A` and `B` gets both, and an N-arm
/// roster that does not gets only the table. Fewer than 2 named arms and
/// neither applies. The `"?"` bucket of unattributed records is never a named
/// arm.
///
/// [`format_harvest_text`]: super::sweep_experiment::format_harvest_text
#[must_use]
pub fn arm_comparison_lines(arms: &[Value]) -> Vec<String> {
    // Inequality inputs the retune (#3718) consumes.
    let find_arm = |name: &str| {
        arms.iter()
            .find(|a| a.get("arm").and_then(Value::as_str) == Some(name))
    };
    let mut lines: Vec<String> = Vec::new();
    if let (Some(a), Some(b)) = (find_arm("A"), find_arm("B")) {
        lines.extend([
            String::new(),
            "  Inequality inputs for #3718 (cost + merge-rate floor):".to_string(),
            format!(
                "    opus-first  (A): mean ${} / issue, merge-rate {}",
                fmt_number(&a["mean_cost_per_issue_usd"]),
                fmt_number(&a["merge_rate"])
            ),
            format!(
                "    sonnet-first(B): mean ${} / issue, merge-rate {}",
                fmt_number(&b["mean_cost_per_issue_usd"]),
                fmt_number(&b["merge_rate"])
            ),
        ]);
    }
    lines.extend(generalized_arm_lines(arms));
    lines
}

/// The N-arm table: every named arm's mean cost per issue and merge-rate floor,
/// cheapest first — the same quantities the A/B block reports, generalized.
fn generalized_arm_lines(arms: &[Value]) -> Vec<String> {
    let named: Vec<&Value> = arms
        .iter()
        .filter(|a| !matches!(a.get("arm").and_then(Value::as_str), None | Some("?") | Some("")))
        .collect();
    let ids: Vec<&str> = named
        .iter()
        .filter_map(|a| a.get("arm").and_then(Value::as_str))
        .collect();
    let is_legacy_pair = ids.len() == 2 && ids.contains(&"A") && ids.contains(&"B");
    if named.len() < 2 || is_legacy_pair {
        return Vec::new();
    }

    let mut rows: Vec<(&str, Option<f64>, &Value, &Value)> = named
        .iter()
        .map(|a| {
            (
                a.get("arm").and_then(Value::as_str).unwrap_or("?"),
                a.get("mean_cost_per_issue_usd").and_then(Value::as_f64),
                a.get("mean_cost_per_issue_usd").unwrap_or(&Value::Null),
                a.get("merge_rate").unwrap_or(&Value::Null),
            )
        })
        .collect();
    // Cheapest first; arms with no cost datum sort last, then by id for stability.
    rows.sort_by(|a, b| {
        match (a.1, b.1) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.0.cmp(b.0))
    });

    let mut lines = vec![
        String::new(),
        format!("  Per-arm cost + merge-rate floor ({} arms, cheapest first):", rows.len()),
    ];
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(1);
    for (id, _, cost, merge) in rows {
        lines.push(format!(
            "    {id:<width$}: mean ${} / issue, merge-rate {}",
            fmt_number(cost),
            fmt_number(merge),
        ));
    }
    lines
}

/// Render a JSON number the way Python's `str()` does for the harvest summary
/// lines — `None` for null, `1.0` for an integral float. Moved here with the
/// comparison tail it is the only caller of (#9122).
fn fmt_number(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Number(n) => n.as_f64().map_or_else(
            || n.to_string(),
            |f| {
                if f.fract().abs() < f64::EPSILON {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            },
        ),
        other => other.to_string(),
    }
}

// --------------------------------------------------------------------------
// Deterministic hashing
// --------------------------------------------------------------------------

/// A uniform `[0.0, 1.0)` draw keyed on `(domain, issue, stratum)`.
///
/// FNV-1a for the byte fold, then a SplitMix64 finalizer for avalanche — FNV
/// alone mixes short keys unevenly, which would bias a weighted pick over the
/// small, dense integer domain issue numbers occupy. The top 53 bits become the
/// mantissa of an exactly-representable double.
#[allow(clippy::cast_precision_loss)]
fn unit_hash(domain: &str, issue_number: i64, stratum: &str) -> f64 {
    let key = format!("{domain}\u{1f}{stratum}\u{1f}{issue_number}");
    let h = mix64(fnv1a(key.as_bytes()));
    ((h >> 11) as f64) / ((1_u64 << 53) as f64)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const fn mix64(mut z: u64) -> u64 {
    z ^= z >> 33;
    z = z.wrapping_mul(0xff51_afd7_ed55_8ccd);
    z ^= z >> 33;
    z = z.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    z ^= z >> 33;
    z
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;
    use serde_json::json;

    fn three_arms() -> Value {
        json!({"sweep": {"modelExperimentArms": [
            {"id": "A", "model": "opus",   "weight": 1.0},
            {"id": "B", "model": "sonnet", "weight": 2.0},
            {"id": "C", "model": "haiku",  "weight": 7.0},
        ]}})
    }

    // ===== unconfigured => byte-for-byte the pre-#9122 A/B pair =====

    #[test]
    fn absent_config_is_the_builtin_pair_with_the_legacy_parity_split() {
        let (arms, warnings) = ExperimentArms::load(&json!({}));
        assert!(warnings.is_empty());
        assert!(!arms.is_configured());
        assert_eq!(arms.arms().len(), 2);

        // Exactly the table `assign_arm` (and test-sweep-experiment.sh) assert.
        for issue in 0..500_i64 {
            for complexity in [None, Some("routine"), Some("complex")] {
                let legacy = super::super::sweep_experiment::assign_arm(issue, complexity);
                let arm = arms.assign(issue, complexity);
                assert_eq!(arm.id, legacy, "issue {issue} {complexity:?}");
                assert_eq!(
                    arm.model,
                    super::super::sweep_experiment::arm_model(legacy),
                    "issue {issue}"
                );
            }
        }
        assert_eq!(arms.assign(100, Some("routine")).id, "A");
        assert_eq!(arms.assign(100, Some("routine")).model, "opus");
        assert_eq!(arms.assign(100, Some("complex")).id, "B");
        assert_eq!(arms.assign(101, Some("routine")).id, "B");
    }

    #[test]
    fn a_null_arms_key_is_treated_as_absent() {
        let (arms, warnings) =
            ExperimentArms::load(&json!({"sweep": {"modelExperimentArms": null}}));
        assert!(warnings.is_empty());
        assert!(!arms.is_configured());
    }

    // ===== N-arm weighted assignment =====

    #[test]
    fn three_arms_load_with_their_weights() {
        let (arms, warnings) = ExperimentArms::load(&three_arms());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(arms.is_configured());
        let ids: Vec<&str> = arms.arms().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["A", "B", "C"]);
        assert_eq!(arms.arms()[2].weight, 7.0);
    }

    #[test]
    fn an_omitted_weight_defaults_to_one() {
        let cfg = json!({"sweep": {"modelExperimentArms": [
            {"id": "x", "model": "opus"},
            {"id": "y", "model": "sonnet"},
        ]}});
        let (arms, warnings) = ExperimentArms::load(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(arms.arms().iter().all(|a| a.weight == 1.0));
        // ids are upper-cased to match the stats store's own convention.
        assert_eq!(arms.arms()[0].id, "X");
    }

    #[test]
    fn assignment_is_deterministic_and_resume_safe() {
        let (arms, _) = ExperimentArms::load(&three_arms());
        for issue in [1_i64, 42, 9122, 1_000_003] {
            let first = arms.assign(issue, Some("complex")).id.clone();
            for _ in 0..5 {
                assert_eq!(arms.assign(issue, Some("complex")).id, first);
            }
        }
    }

    /// Over a large synthetic sample each arm's observed frequency converges to
    /// its configured weight ratio — independently within each stratum.
    #[test]
    fn weighted_frequencies_converge_within_each_complexity_stratum() {
        let (arms, _) = ExperimentArms::load(&three_arms());
        let total_weight: f64 = arms.arms().iter().map(|a| a.weight).sum();
        const N: i64 = 2000;
        const TOLERANCE: f64 = 0.035;

        for stratum in [Some("routine"), Some("complex")] {
            let mut counts = std::collections::BTreeMap::<String, i64>::new();
            for issue in 1..=N {
                *counts
                    .entry(arms.assign(issue, stratum).id.clone())
                    .or_default() += 1;
            }
            assert_eq!(counts.len(), 3, "every arm must be drawn ({stratum:?})");
            for arm in arms.arms() {
                let observed = counts[&arm.id] as f64 / N as f64;
                let expected = arm.weight / total_weight;
                assert!(
                    (observed - expected).abs() < TOLERANCE,
                    "{stratum:?} arm {} observed {observed:.4} vs expected {expected:.4}",
                    arm.id
                );
            }
        }
    }

    /// The strata must be independent, not a relabeling of one another.
    #[test]
    fn strata_are_assigned_independently() {
        let (arms, _) = ExperimentArms::load(&three_arms());
        let differing = (1..=500_i64)
            .filter(|i| arms.assign(*i, Some("routine")).id != arms.assign(*i, Some("complex")).id)
            .count();
        assert!(differing > 50, "strata look correlated ({differing}/500 differ)");
    }

    // ===== rejection: fable, non-Claude runtimes, malformed rosters =====

    fn assert_rejected(cfg: &Value, needle: &str) {
        let (arms, warnings) = ExperimentArms::load(cfg);
        assert!(!arms.is_configured(), "roster should have been rejected");
        assert_eq!(arms, ExperimentArms::default_pair());
        assert!(
            warnings.iter().any(|w| w.contains(needle)),
            "no warning mentioned {needle:?}: {warnings:?}"
        );
    }

    #[test]
    fn an_arm_naming_fable_is_rejected_and_falls_through() {
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "F", "model": "fable"},
            ]}}),
            "No-Fable bound",
        );
        // …including a pinned fable ID and an `@effort` suffix.
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "F", "model": "claude-fable-5@xhigh"},
            ]}}),
            "No-Fable bound",
        );
    }

    #[test]
    fn an_arm_resolving_to_fable_through_an_alias_is_rejected() {
        assert_rejected(
            &json!({
                "sweep": {
                    "modelAliases": {"sneaky": "claude-fable-5"},
                    "modelExperimentArms": [
                        {"id": "A", "model": "opus"},
                        {"id": "S", "model": "sneaky"},
                    ],
                }
            }),
            "fable model",
        );
    }

    #[test]
    fn a_non_claude_runtime_arm_is_rejected_never_silently_ignored() {
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "GLM", "model": "glm-5.3", "runtime": "opencode"},
            ]}}),
            "Claude-only",
        );
        // An explicit `runtime: "claude"` is fine.
        let (arms, warnings) = ExperimentArms::load(&json!({"sweep": {"modelExperimentArms": [
            {"id": "A", "model": "opus", "runtime": "claude"},
            {"id": "B", "model": "sonnet", "runtime": "Claude"},
        ]}}));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(arms.is_configured());
    }

    #[test]
    fn malformed_rosters_are_rejected_with_a_named_reason() {
        assert_rejected(&json!({"sweep": {"modelExperimentArms": "opus,sonnet"}}), "not an array");
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [{"id": "A", "model": "opus"}]}}),
            "at least 2",
        );
        assert_rejected(&json!({"sweep": {"modelExperimentArms": []}}), "at least 2");
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [{"id": "A", "model": "opus"}, "sonnet"]}}),
            "not an object",
        );
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "  ", "model": "sonnet"},
            ]}}),
            "no non-empty 'id'",
        );
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "B"},
            ]}}),
            "no non-empty 'model'",
        );
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "B", "model": "sonnet", "weight": 0},
            ]}}),
            "greater than zero",
        );
        assert_rejected(
            &json!({"sweep": {"modelExperimentArms": [
                {"id": "A", "model": "opus"},
                {"id": "a", "model": "sonnet"},
            ]}}),
            "repeats arm id",
        );
    }

    #[test]
    fn a_configured_arm_resolves_its_model_through_the_shared_resolver() {
        let (arms, _) = ExperimentArms::load(&three_arms());
        let cfg = json!({});
        // The #4060 contract: `opus` reaches the pinned gen-5 ID, `sonnet`
        // passes through unchanged.
        assert_eq!(arms.arms()[0].resolved_model(&cfg), "claude-opus-5");
        assert_eq!(arms.arms()[1].resolved_model(&cfg), "sonnet");
    }

    // ===== budget fraction =====

    #[test]
    fn budget_fraction_defaults_to_one_and_prefers_env_over_config() {
        assert_eq!(resolve_budget_fraction(None, &json!({})).0, 1.0);
        let cfg = json!({"sweep": {"modelExperimentBudgetFraction": 0.3}});
        assert_eq!(resolve_budget_fraction(None, &cfg).0, 0.3);
        assert_eq!(resolve_budget_fraction(Some("0.1"), &cfg).0, 0.1);
        // A blank env value is unset, so config still applies.
        assert_eq!(resolve_budget_fraction(Some("  "), &cfg).0, 0.3);
    }

    #[test]
    fn a_malformed_budget_fraction_warns_and_falls_back_to_one() {
        for bad in ["bogus", "-0.5", "1.5", "NaN"] {
            let (v, warnings) = resolve_budget_fraction(Some(bad), &json!({}));
            assert_eq!(v, 1.0, "{bad}");
            assert_eq!(warnings.len(), 1, "{bad}");
        }
        let (v, warnings) =
            resolve_budget_fraction(None, &json!({"sweep": {"modelExperimentBudgetFraction": 42}}));
        assert_eq!(v, 1.0);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn fraction_one_keeps_every_issue_in_the_experiment() {
        for issue in 0..1000_i64 {
            assert!(in_experiment(issue, Some("routine"), 1.0));
            assert!(in_experiment(issue, Some("complex"), 1.0));
        }
    }

    #[test]
    fn fraction_zero_keeps_every_issue_out() {
        for issue in 0..1000_i64 {
            assert!(!in_experiment(issue, None, 0.0));
        }
    }

    #[test]
    fn the_sampled_in_share_converges_to_the_fraction() {
        const N: i64 = 2000;
        for fraction in [0.1, 0.3, 0.75] {
            for stratum in [Some("routine"), Some("complex")] {
                let hits = (1..=N)
                    .filter(|i| in_experiment(*i, stratum, fraction))
                    .count();
                let observed = hits as f64 / N as f64;
                assert!(
                    (observed - fraction).abs() < 0.035,
                    "fraction {fraction} {stratum:?}: observed {observed:.4}"
                );
            }
        }
    }

    /// The load-bearing independence property: changing the budget fraction
    /// re-partitions who is IN the experiment without reshuffling which arm an
    /// in-experiment issue lands on.
    #[test]
    fn changing_the_fraction_never_reshuffles_arm_assignment() {
        let (arms, _) = ExperimentArms::load(&three_arms());
        for issue in 1..=500_i64 {
            let baseline = arms.assign(issue, Some("routine")).id.clone();
            for fraction in [0.05, 0.3, 0.9, 1.0] {
                if in_experiment(issue, Some("routine"), fraction) {
                    assert_eq!(arms.assign(issue, Some("routine")).id, baseline);
                }
            }
        }
    }

    #[test]
    fn sampling_in_is_deterministic_and_resume_safe() {
        for issue in [7_i64, 9122, 55_555] {
            let first = in_experiment(issue, Some("complex"), 0.3);
            for _ in 0..5 {
                assert_eq!(in_experiment(issue, Some("complex"), 0.3), first);
            }
        }
    }

    #[test]
    fn the_skip_banner_names_the_fraction_and_the_absent_arm() {
        let banner = format_budget_skip_banner(9122, 0.3, Some("env"));
        assert!(banner.contains("mode=EXPERIMENT"));
        assert!(banner.contains("NOT IN EXPERIMENT"));
        assert!(banner.contains("0.3"));
        assert!(banner.contains("Canary confirmed by: env"));
        // The canary-source line is omitted, not blank, when there is none.
        assert!(!format_budget_skip_banner(9122, 0.3, None).contains("Canary confirmed"));
    }

    // ===== harvest text generalization =====

    fn arm_row(id: &str, cost: f64, merge: f64) -> Value {
        json!({"arm": id, "mean_cost_per_issue_usd": cost, "merge_rate": merge})
    }

    /// The load-bearing regression guarantee: for the legacy pair the tail is
    /// EXACTLY the pre-#9122 #3718 block — no generalized table appended.
    #[test]
    fn the_legacy_ab_pair_emits_only_the_3718_block_verbatim() {
        let expect = [
            String::new(),
            "  Inequality inputs for #3718 (cost + merge-rate floor):".to_string(),
            "    opus-first  (A): mean $1.0 / issue, merge-rate 1.0".to_string(),
            "    sonnet-first(B): mean $0.5 / issue, merge-rate 1.0".to_string(),
        ];
        assert_eq!(arm_comparison_lines(&[arm_row("A", 1.0, 1.0), arm_row("B", 0.5, 1.0)]), expect);
        // …and the unattributed "?" bucket is not a named arm.
        assert_eq!(
            arm_comparison_lines(&[
                arm_row("A", 1.0, 1.0),
                arm_row("B", 0.5, 1.0),
                arm_row("?", 0.2, 0.0),
            ]),
            expect
        );
    }

    #[test]
    fn fewer_than_two_named_arms_emits_nothing() {
        // A single named arm has nothing to compare against (pre-#9122 too).
        assert!(arm_comparison_lines(&[arm_row("A", 1.0, 1.0)]).is_empty());
        assert!(arm_comparison_lines(&[arm_row("HAIKU", 1.0, 1.0)]).is_empty());
        assert!(arm_comparison_lines(&[]).is_empty());
    }

    #[test]
    fn three_named_arms_get_a_cheapest_first_table() {
        // A/B are present, so the #3718 block is emitted first (unchanged) and
        // the generalized table is APPENDED — neither replaces the other.
        let all = arm_comparison_lines(&[
            arm_row("A", 1.25, 0.9),
            arm_row("B", 0.5, 0.8),
            arm_row("C", 0.1, 0.5),
            arm_row("?", 9.9, 0.0),
        ]);
        assert!(all[1].contains("#3718"), "{all:?}");
        let lines: Vec<&String> = all.iter().skip(4).collect();
        assert!(lines[1].contains("3 arms"), "{lines:?}");
        let body: Vec<&&String> = lines.iter().skip(2).collect();
        assert_eq!(body.len(), 3);
        assert!(body[0].contains("C: mean $0.1"), "{body:?}");
        assert!(body[1].contains("B: mean $0.5"), "{body:?}");
        assert!(body[2].contains("A: mean $1.25"), "{body:?}");
        assert!(body[2].contains("merge-rate 0.9"), "{body:?}");
    }

    #[test]
    fn a_non_ab_two_arm_roster_gets_only_the_generalized_block() {
        let lines = arm_comparison_lines(&[arm_row("HAIKU", 0.2, 0.7), arm_row("OPUS", 2.0, 0.9)]);
        assert!(!lines.iter().any(|l| l.contains("#3718")), "{lines:?}");
        assert!(lines[1].contains("2 arms"), "{lines:?}");
        assert!(lines[2].contains("HAIKU"), "{lines:?}");
    }

    /// A configured-roster arm id (#9122) is rolled up by `harvest` unchanged —
    /// it is already arm-name-agnostic — and its `model` column falls back to
    /// the Builder model actually observed, since `ARM_MODEL` cannot name it.
    #[test]
    fn harvest_rolls_up_a_configured_arm_id_with_its_observed_model() {
        use super::super::sweep_experiment as se;
        let dir = tempfile::tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl").to_string_lossy().to_string();
        for (phase, role, model, verdict) in [
            ("builder", "builder", Some("haiku"), None),
            ("judge", "judge", None, Some("pass")),
        ] {
            let fields = se::RecordFields {
                issue: 9122,
                phase,
                role,
                model,
                mode: "experiment",
                arm: Some("HAIKU"),
                attempt: 1,
                judge_verdict: verdict,
                token_fidelity: "none",
                ..se::RecordFields::default()
            };
            se::append_record(&se::build_record(&fields, "2026-01-01T00:00:00Z"), Some(&stats))
                .unwrap();
        }
        let report = se::harvest(Some(&stats), None);
        assert_eq!(report["arms"][0]["arm"], json!("HAIKU"));
        assert_eq!(report["arms"][0]["model"], json!("haiku"));
        assert_eq!(report["arms"][0]["first_attempt_pass_rate"], json!(1.0));
        // A lone non-A/B arm has nothing to compare against: neither the #3718
        // inequality block nor the generalized table (the title line mentions
        // #3718 unconditionally, so assert on the block's own heading).
        let text = se::format_harvest_text(&report);
        assert!(!text.contains("Inequality inputs"), "{text}");
        assert!(!text.contains("cheapest first"), "{text}");
        // …but the roll-up row itself is there, keyed on the configured id.
        assert!(text.contains("HAIKU haiku"), "{text}");
    }

    #[test]
    fn a_missing_cost_datum_renders_as_none_and_sorts_last() {
        let lines = arm_comparison_lines(&[
            json!({"arm": "X", "mean_cost_per_issue_usd": Value::Null, "merge_rate": Value::Null}),
            arm_row("Y", 1.0, 1.0),
            arm_row("Z", 2.0, 1.0),
        ]);
        let body: Vec<&String> = lines.iter().skip(2).collect();
        assert!(body[2].contains("X: mean $None / issue, merge-rate None"), "{body:?}");
        assert!(body[0].contains("Y: mean $1.0 / issue, merge-rate 1.0"), "{body:?}");
    }
}
