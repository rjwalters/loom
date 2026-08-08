//! Logical model-tier → concrete model-ID resolution (issues #3982 / #4238 /
//! #4282), the native port of `loom_tools.model_tiers` (#4275).
//!
//! This is the **single indirection point** that maps a logical tier to the
//! concrete model ID dispatched on the wire, so every consumer keeps saying
//! `opus` and exactly one place decides what `opus` means. Consumers:
//!
//! * the `sweep.md` skill — via the `resolve-model.sh` shell stub, which now
//!   execs `loom-daemon resolve-model`;
//! * `resolve-tier-model.sh` — via `resolve-model.sh --tier`;
//! * `sweep-experiment.sh` — Arm A resolves through the same code path, so the
//!   experiment's Arm A and `resolve-model.sh` can never disagree;
//! * the daemon itself — [`crate::sweep_registry::resolve_dispatch_model`].
//!
//! # Alias normalization is ONE implementation (#4060 contract)
//!
//! Before this port, `model_tiers._config_overrides` (Python) and
//! [`crate::sweep_registry::read_model_aliases`] (Rust) were a byte-identical
//! *twin pair* that had to be kept in sync by hand — and had already drifted
//! (#4059 Finding 2: Rust dropped blank alias keys, Python kept them). The pair
//! is now collapsed: this module calls
//! [`crate::sweep_registry::model_aliases_from_config`] /
//! [`crate::sweep_registry::resolve_model_alias_from_config`] and the Rust
//! semantics (blank keys dropped, cross-tier `deep_merge` union per #4059) are
//! the only surviving definition.
//!
//! # `--config <path>` is an explicit bypass (#4060 contract)
//!
//! [`load_config`] with an explicit path reads **exactly that file** and skips
//! all config tiering; only the default path routes through
//! [`crate::config_resolver::resolve_effective_config`]. Both forms
//! **never raise** — a missing/malformed file, or no Loom repo at all, degrades
//! to an empty config so `resolve-model.sh` still works outside a Loom repo.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::config_resolver;
use crate::sweep_registry;

/// The three cost-of-being-wrong strata. An absent/unknown marker ⇒ `routine`.
pub const COMPLEXITY_TIERS: [&str; 3] = ["mechanical", "routine", "complex"];

/// The operator-facing `sweep.optimization` policy switch values.
pub const OPTIMIZATION_PROFILES: [&str; 3] = ["cost", "speed", "balanced"];

/// The alias values the in-session Task/Agent tool's `model` parameter accepts,
/// in **ascending capability/cost order**. The ordering is load-bearing for
/// [`downgrade_task_alias`] (issue #5687) — it is the ladder a mid-wave model
/// credit exhaustion steps *down*, and the same ladder `sweep.escalation`
/// climbs *up* on a Judge rejection.
const TASK_TOOL_ALIASES: [&str; 4] = ["haiku", "sonnet", "opus", "fable"];

/// The generation each bare CLI alias resolves to on the wire TODAY (probed
/// 2026-07-27). `opus` lags at generation 4 — the bug #3982 fixes at the
/// resolution layer. Update this table (and drop the matching pin in
/// [`crate::sweep_registry`]'s `DEFAULT_TIER_ALIASES`) when Anthropic repoints
/// an alias.
const ALIAS_GENERATION: &[(&str, u32)] = &[("haiku", 5), ("sonnet", 5), ("opus", 4), ("fable", 5)];

/// tier → logical model, per optimization profile.
///
/// `balanced` is intentionally EMPTY: an absent/default profile must not
/// materialize any preset, so a repo with no `sweep.optimization` configured
/// (or explicitly set to `balanced`) dispatches byte-identically to
/// pre-Phase-B behavior.
const OPTIMIZATION_PRESETS: &[(&str, &[(&str, &str)])] = &[
    // Cheapest model the Judge gate can safely correct, full 3-stratum spread.
    (
        "cost",
        &[
            ("mechanical", "haiku"),
            ("routine", "sonnet"),
            ("complex", "opus"),
        ],
    ),
    // Wall-clock in a sweep is dominated by Judge-rejection / Doctor round-trip
    // COUNT, not per-turn latency — so "speed" starts a tier higher than
    // "balanced" to buy fewer retry cycles. `complex` is already at the ceiling
    // under "balanced" via the tier-2.5 bump, so "speed" leaves it unchanged.
    (
        "speed",
        &[
            ("mechanical", "sonnet"),
            ("routine", "opus"),
            ("complex", "opus"),
        ],
    ),
    ("balanced", &[]),
];

fn warn(msg: &str) {
    eprintln!("[model-tiers] WARNING: {msg}");
}

// --------------------------------------------------------------------------
// Config (best-effort — never raises)
// --------------------------------------------------------------------------

/// Resolve the effective config for model-tier lookups.
///
/// * `Some(path)` — the **explicit bypass**: read exactly that file, skip all
///   tiering. A missing/malformed/non-object file degrades to `{}`.
/// * `None` — the default path: route `cwd` through the full Rust config
///   resolver tier chain (private/shared defaults → `.loom/config.json` →
///   `.loom-project/project.json` → `.loom-local/local.json`).
///
/// The default form deliberately anchors on the **current directory**, not a
/// walked-up repo root, so it matches the Python original's cwd-relative
/// `.loom/config.json` default exactly. Outside a Loom repo every tier is
/// absent and the result is `{}` — `resolve-model.sh` keeps working.
#[must_use]
pub fn load_config(explicit: Option<&Path>) -> Value {
    match explicit {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v @ Value::Object(_)) => v,
                _ => Value::Object(serde_json::Map::new()),
            },
            Err(_) => Value::Object(serde_json::Map::new()),
        },
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            config_resolver::resolve_effective_config(&cwd)
        }
    }
}

/// The effective tier→ID map: shipped defaults overlaid with config overrides.
#[must_use]
pub fn tier_map(config: &Value) -> BTreeMap<String, String> {
    sweep_registry::effective_tier_aliases(config)
}

// --------------------------------------------------------------------------
// Resolution
// --------------------------------------------------------------------------

/// Resolve a logical tier / alias / pinned ID to the concrete model ID.
///
/// Preserves any `@effort` suffix (`opus@xhigh` → `claude-opus-5@xhigh`).
/// Unknown aliases and pinned IDs pass through unchanged. Empty → `""`.
#[must_use]
pub fn resolve_model(model: &str, config: &Value) -> String {
    sweep_registry::resolve_model_alias_from_config(config, model)
}

/// The model generation a logical tier / ID resolves to on the wire.
///
/// `claude-opus-5` → 5, `claude-sonnet-4-6` → 4, legacy `claude-3-5-sonnet`
/// → 3. A bare alias that stays unmapped is looked up in the probed
/// [`ALIAS_GENERATION`] table. `None` for anything unrecognized.
#[must_use]
pub fn generation_of(model: &str, config: &Value) -> Option<u32> {
    let resolved = resolve_model(model, config);
    let base = base_of(&resolved);
    if base.is_empty() {
        return None;
    }
    // Modern IDs: claude-<family>-<gen>[-<minor>]
    if let Some(gen) = parse_modern_generation(&base) {
        return Some(gen);
    }
    // Legacy IDs: claude-<gen>-...
    if let Some(gen) = parse_legacy_generation(&base) {
        return Some(gen);
    }
    // A bare alias that passed through unmapped.
    ALIAS_GENERATION
        .iter()
        .find(|(k, _)| *k == base)
        .map(|(_, g)| *g)
}

/// Map a model to the nearest value the in-session Task/Agent tool accepts.
///
/// The Task tool's `model` parameter is an alias-only enum
/// (`haiku`/`sonnet`/`opus`/`fable`); a pinned ID like `claude-opus-5` is an
/// invalid value there, so #3982's resolved IDs must degrade to their family
/// alias on the in-session dispatch path (issue #4282).
///
/// * A value already in the Task enum passes through unchanged.
/// * A concrete ID maps to its family alias by parsing the family segment
///   (`claude-opus-5` → `opus`, legacy `claude-3-5-sonnet` → `sonnet`).
/// * An `@effort` suffix is stripped (the Task tool has no effort parameter —
///   composes with the #3705 degradation rule).
/// * Anything unrecognized returns `""`, and the CLI then exits 3 with no
///   output so the caller omits `model` entirely.
///
/// The reverse mapping is mechanical and config-independent (its input is
/// already a resolved ID/alias), so unlike the Python signature this takes no
/// `config`.
#[must_use]
pub fn task_alias_of(model: &str) -> String {
    let base = base_of(model);
    if base.is_empty() {
        return String::new();
    }
    // Already a Task-passable alias (or a bare alias that resolved to itself).
    if TASK_TOOL_ALIASES.contains(&base.as_str()) {
        return base;
    }
    // Modern IDs: claude-<family>-<gen>[-<minor>] → the family segment.
    if let Some(rest) = base.strip_prefix("claude-") {
        if let Some((family, tail)) = rest.split_once('-') {
            if tail.starts_with(|c: char| c.is_ascii_digit())
                && family.chars().all(|c| c.is_ascii_lowercase())
                && TASK_TOOL_ALIASES.contains(&family)
            {
                return family.to_string();
            }
        }
    }
    // Legacy IDs: claude-<gen>-...-<family> (claude-3-5-sonnet, claude-3-opus).
    for family in TASK_TOOL_ALIASES {
        if base.ends_with(&format!("-{family}")) {
            return family.to_string();
        }
    }
    String::new()
}

/// Map a model to the next **cheaper** Task-passable alias (issue #5687).
///
/// This is the deterministic "one rung down" step the `/loom:sweep`
/// orchestrator applies when an in-session role subagent is killed by a
/// per-model-tier credit exhaustion (`MODEL_CREDITS_EXHAUSTED` —
/// "You're out of usage credits"). Credits are scoped to a model tier, so
/// re-dispatching the same work on a cheaper tier under the same account is
/// the remedy; the in-session Task dispatch path has no account pool to rotate
/// through, so this is the *only* remedy available there.
///
/// The step is over [`TASK_TOOL_ALIASES`] in ascending capability order, so
/// `fable → opus → sonnet → haiku`. Note `fable → opus` reproduces exactly the
/// rung step the pre-existing `MODEL_REFUSAL` fallback already documents —
/// this generalizes that one hard-coded hop to every rung.
///
/// * Any input `task_alias_of` understands (bare alias, pinned ID, `@effort`
///   suffix) is accepted; the effort suffix is dropped with the rest of the
///   Task-tool degradation.
/// * Returns `""` when the model is already at the cheapest rung (`haiku`) or
///   is unrecognized. The CLI then exits 3 with no output, and the caller must
///   fall through to its normal mid-phase-death handling rather than guessing
///   a model.
#[must_use]
pub fn downgrade_task_alias(model: &str) -> String {
    let alias = task_alias_of(model);
    if alias.is_empty() {
        return String::new();
    }
    match TASK_TOOL_ALIASES.iter().position(|a| *a == alias) {
        // Index 0 is the cheapest rung — there is nothing below it.
        Some(0) | None => String::new(),
        Some(index) => TASK_TOOL_ALIASES[index - 1].to_string(),
    }
}

// --------------------------------------------------------------------------
// Complexity-tier → model resolution (issue #4238)
// --------------------------------------------------------------------------

/// The `sweep.tierModels` map: `{runtime: {tier: logical_model}}`.
///
/// Best-effort and tolerant — a non-object `sweep`/`tierModels`, non-string
/// models, or blank models are dropped rather than raising. Runtime and tier
/// keys are trimmed and lower-cased.
#[must_use]
pub fn tier_models(config: &Value) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let Some(raw) =
        config_resolver::get_path(config, "sweep.tierModels").and_then(Value::as_object)
    else {
        return out;
    };
    for (runtime, tiers) in raw {
        let Some(tiers) = tiers.as_object() else {
            continue;
        };
        let mut inner: BTreeMap<String, String> = BTreeMap::new();
        for (tier, model) in tiers {
            if let Some(m) = model.as_str().map(str::trim).filter(|m| !m.is_empty()) {
                inner.insert(tier.trim().to_lowercase(), m.to_string());
            }
        }
        if !inner.is_empty() {
            out.insert(runtime.trim().to_lowercase(), inner);
        }
    }
    out
}

/// The effective `sweep.optimization` profile: **env > config > default**.
///
/// `env_override` is the raw `LOOM_SWEEP_OPTIMIZATION` value (the CLI passes
/// `std::env::var(..).ok()`; tests pass a literal). An unrecognized value from
/// either source warns and falls back to `balanced` — profile resolution is
/// best-effort and must never fail dispatch.
#[must_use]
pub fn resolve_optimization_profile(config: &Value, env_override: Option<&str>) -> String {
    let balanced = || "balanced".to_string();

    let (raw, source): (Option<&Value>, &str) = match env_override.filter(|v| !v.is_empty()) {
        Some(_) => (None, "env LOOM_SWEEP_OPTIMIZATION"),
        None => (
            config_resolver::get_path(config, "sweep.optimization"),
            "config sweep.optimization",
        ),
    };

    // Env branch: always a string, so normalize directly.
    if let Some(v) = env_override.filter(|v| !v.is_empty()) {
        let value = v.trim().to_lowercase();
        if value.is_empty() || !OPTIMIZATION_PROFILES.contains(&value.as_str()) {
            warn(&format!(
                "{source}={v:?} is not one of {OPTIMIZATION_PROFILES:?}; falling back to 'balanced'"
            ));
            return balanced();
        }
        return value;
    }

    // Config branch.
    let Some(raw) = raw else { return balanced() };
    if raw.is_null() {
        return balanced();
    }
    let Some(s) = raw.as_str() else {
        warn(&format!("{source}={raw} is not a string; falling back to 'balanced'"));
        return balanced();
    };
    if s.is_empty() {
        // Genuinely unset — the silent, expected default. No warning.
        return balanced();
    }
    let value = s.trim().to_lowercase();
    if value.is_empty() || !OPTIMIZATION_PROFILES.contains(&value.as_str()) {
        warn(&format!(
            "{source}={s:?} is not one of {OPTIMIZATION_PROFILES:?}; falling back to 'balanced'"
        ));
        return balanced();
    }
    value
}

/// The tier → logical-model preset for an optimization profile.
///
/// An unrecognized profile name returns the empty (`balanced`) preset rather
/// than panicking — callers should resolve through
/// [`resolve_optimization_profile`] first, but this stays defensive.
#[must_use]
pub fn optimization_preset(profile: &str) -> BTreeMap<String, String> {
    OPTIMIZATION_PRESETS
        .iter()
        .find(|(name, _)| *name == profile)
        .map(|(_, entries)| {
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a complexity tier to the concrete model ID to dispatch on the wire.
///
/// Looks up `sweep.tierModels[<runtime>][<tier>]` first — an operator-authored
/// map is the more specific configuration and always wins. Absent that, falls
/// back to the tier's entry (if any) in the `sweep.optimization` profile preset
/// (#4238 Phase B). Either way the resulting logical tier is passed through
/// [`resolve_model`]. An unrecognized/absent tier is treated as `routine`.
///
/// Returns `""` when neither source has a mapping (the caller then falls
/// through to its normal precedence chain, keeping unconfigured dispatch
/// byte-identical), and also `""` — with a warning — when the mapping would
/// resolve to `fable`: neither a Curator marker nor an optimization profile can
/// ever dispatch the frontier/refusal-prone model (#3702).
#[must_use]
pub fn resolve_tier_model(
    tier: Option<&str>,
    runtime: &str,
    config: &Value,
    env_override: Option<&str>,
) -> String {
    let mut key = tier.unwrap_or("").trim().to_lowercase();
    if !COMPLEXITY_TIERS.contains(&key.as_str()) {
        key = "routine".to_string();
    }
    let mut rt = runtime.trim().to_lowercase();
    if rt.is_empty() {
        rt = "claude".to_string();
    }

    let mut source = "tierModels";
    let mut logical = tier_models(config)
        .get(&rt)
        .and_then(|m| m.get(&key))
        .cloned()
        .unwrap_or_default();
    if logical.is_empty() {
        source = "optimization";
        let profile = resolve_optimization_profile(config, env_override);
        logical = optimization_preset(&profile)
            .get(&key)
            .cloned()
            .unwrap_or_default();
    }
    if logical.is_empty() {
        return String::new();
    }

    // No-Fable hard bound: refuse a mapping that names fable, before resolution…
    if base_of(&logical) == "fable" {
        warn(&format!(
            "{source}[{rt}][{key}] maps to 'fable' — refusing (No-Fable bound); falling through"
        ));
        return String::new();
    }
    let resolved = resolve_model(&logical, config);
    // …and after resolution, in case an alias/override lands on a fable model ID.
    if base_of(&resolved).contains("fable") {
        warn(&format!(
            "{source}[{rt}][{key}] resolves to a fable model — refusing; falling through"
        ));
        return String::new();
    }
    resolved
}

// --------------------------------------------------------------------------
// Small parsing helpers
// --------------------------------------------------------------------------

/// The `model` half of the `model@effort` grammar, trimmed and lower-cased.
fn base_of(model: &str) -> String {
    model
        .split_once('@')
        .map_or(model, |(b, _)| b)
        .trim()
        .to_lowercase()
}

/// `^claude-[a-z]+-(\d+)` → the generation.
fn parse_modern_generation(base: &str) -> Option<u32> {
    let rest = base.strip_prefix("claude-")?;
    let (family, tail) = rest.split_once('-')?;
    if family.is_empty() || !family.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// `^claude-(\d+)-` → the generation.
fn parse_legacy_generation(base: &str) -> Option<u32> {
    let rest = base.strip_prefix("claude-")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // The regex requires a trailing `-` after the digits.
    if !rest[digits.len()..].starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn empty() -> Value {
        json!({})
    }

    // ===== resolve_model / alias normalization =====

    #[test]
    fn opus_pins_to_current_generation() {
        assert_eq!(resolve_model("opus", &empty()), "claude-opus-5");
    }

    #[test]
    fn unpinned_aliases_and_ids_pass_through() {
        assert_eq!(resolve_model("sonnet", &empty()), "sonnet");
        assert_eq!(resolve_model("fable", &empty()), "fable");
        assert_eq!(resolve_model("claude-sonnet-4-6", &empty()), "claude-sonnet-4-6");
        assert_eq!(resolve_model("totally-unknown", &empty()), "totally-unknown");
        assert_eq!(resolve_model("", &empty()), "");
    }

    #[test]
    fn effort_suffix_is_preserved() {
        assert_eq!(resolve_model("sonnet@xhigh", &empty()), "sonnet@xhigh");
        assert_eq!(resolve_model("opus@xhigh", &empty()), "claude-opus-5@xhigh");
    }

    #[test]
    fn config_override_repoints_and_can_drop_a_pin() {
        let cfg = json!({"sweep": {"modelAliases": {"opus": "opus"}}});
        assert_eq!(resolve_model("opus", &cfg), "opus");
        let cfg = json!({"sweep": {"modelAliases": {"sonnet": "claude-sonnet-9"}}});
        assert_eq!(resolve_model("sonnet", &cfg), "claude-sonnet-9");
    }

    /// #4060: exactly ONE alias-normalization implementation survives. The
    /// tier map this module exposes and the `sweep_registry` resolver the
    /// daemon dispatch path uses must be the same code, so they can never
    /// drift the way the Python/Rust twins did.
    #[test]
    fn alias_normalization_is_shared_with_sweep_registry() {
        let cfg = json!({"sweep": {"modelAliases": {"  OPUS  ": "  claude-opus-9  "}}});
        assert_eq!(tier_map(&cfg).get("opus").map(String::as_str), Some("claude-opus-9"));
        assert_eq!(
            resolve_model("opus", &cfg),
            sweep_registry::resolve_model_alias_from_config(&cfg, "opus")
        );
    }

    /// #4059 Finding 2: the surviving (Rust) semantics drop blank alias keys.
    /// The Python twin kept them; collapsing the pair resolves that divergence
    /// in favor of the Rust behavior, which is what #4059 asserts.
    #[test]
    fn blank_alias_keys_are_dropped() {
        let cfg = json!({"sweep": {"modelAliases": {"   ": "claude-opus-9", "opus": "x"}}});
        let map = tier_map(&cfg);
        assert!(!map.contains_key(""));
        assert_eq!(map.get("opus").map(String::as_str), Some("x"));
    }

    #[test]
    fn blank_alias_values_are_dropped_leaving_the_shipped_default() {
        let cfg = json!({"sweep": {"modelAliases": {"opus": "   "}}});
        assert_eq!(resolve_model("opus", &cfg), "claude-opus-5");
    }

    #[test]
    fn malformed_alias_block_never_raises() {
        for cfg in [
            json!({"sweep": "not-an-object"}),
            json!({"sweep": {"modelAliases": []}}),
            json!({"sweep": {"modelAliases": {"opus": 5}}}),
            json!([]),
        ] {
            assert_eq!(resolve_model("opus", &cfg), "claude-opus-5");
        }
    }

    // ===== load_config: --config is an explicit bypass, and never raises =====

    #[test]
    fn explicit_config_reads_exactly_that_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("custom.json");
        std::fs::write(&path, r#"{"sweep": {"modelAliases": {"opus": "from-explicit"}}}"#).unwrap();
        let cfg = load_config(Some(&path));
        assert_eq!(resolve_model("opus", &cfg), "from-explicit");
    }

    /// The explicit path must SKIP the tier chain: a `.loom-local/local.json`
    /// sitting next to the named file must not be merged in.
    #[test]
    fn explicit_config_skips_the_tier_chain() {
        let dir = tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        let named = loom.join("config.json");
        std::fs::write(&named, r#"{"sweep": {"modelAliases": {"opus": "from-named"}}}"#).unwrap();
        let local = dir.path().join(".loom-local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(
            local.join("local.json"),
            r#"{"sweep": {"modelAliases": {"opus": "from-local-tier"}}}"#,
        )
        .unwrap();

        let cfg = load_config(Some(&named));
        assert_eq!(
            resolve_model("opus", &cfg),
            "from-named",
            "--config must read exactly the named file, not the tier chain"
        );
    }

    #[test]
    fn explicit_config_never_raises_on_missing_or_malformed() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert_eq!(load_config(Some(&missing)), json!({}));
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(load_config(Some(&bad)), json!({}));
        let arr = dir.path().join("arr.json");
        std::fs::write(&arr, "[1,2,3]").unwrap();
        assert_eq!(load_config(Some(&arr)), json!({}));
        // …and resolution still works on the degraded config.
        assert_eq!(resolve_model("opus", &load_config(Some(&missing))), "claude-opus-5");
    }

    // ===== generation_of =====

    #[test]
    fn generation_of_modern_legacy_and_bare_aliases() {
        assert_eq!(generation_of("opus", &empty()), Some(5)); // pinned → claude-opus-5
        assert_eq!(generation_of("sonnet", &empty()), Some(5)); // bare alias table
        assert_eq!(generation_of("fable", &empty()), Some(5));
        assert_eq!(generation_of("haiku", &empty()), Some(5));
        assert_eq!(generation_of("claude-sonnet-4-6", &empty()), Some(4));
        assert_eq!(generation_of("claude-3-5-sonnet", &empty()), Some(3));
        assert_eq!(generation_of("claude-3-opus", &empty()), Some(3));
        assert_eq!(generation_of("gpt-5", &empty()), None);
        assert_eq!(generation_of("", &empty()), None);
    }

    /// The `opus` pin exists precisely so the shipped ladder is monotonic:
    /// the bare CLI alias still resolves to generation 4.
    #[test]
    fn opus_pin_restores_ladder_monotonicity() {
        let unpinned = json!({"sweep": {"modelAliases": {"opus": "opus"}}});
        assert_eq!(generation_of("opus", &unpinned), Some(4));
        assert_eq!(generation_of("opus", &empty()), Some(5));
    }

    // ===== task_alias_of (#4282) =====

    #[test]
    fn task_alias_passes_through_enum_values() {
        for alias in TASK_TOOL_ALIASES {
            assert_eq!(task_alias_of(alias), alias);
        }
        assert_eq!(task_alias_of("OPUS"), "opus");
    }

    #[test]
    fn task_alias_degrades_pinned_ids_to_family() {
        assert_eq!(task_alias_of("claude-opus-5"), "opus");
        assert_eq!(task_alias_of("claude-sonnet-4-6"), "sonnet");
        assert_eq!(task_alias_of("claude-haiku-5"), "haiku");
        assert_eq!(task_alias_of("claude-fable-5"), "fable");
        assert_eq!(task_alias_of("claude-3-5-sonnet"), "sonnet");
        assert_eq!(task_alias_of("claude-3-opus"), "opus");
    }

    #[test]
    fn task_alias_strips_effort_suffix() {
        assert_eq!(task_alias_of("sonnet@xhigh"), "sonnet");
        assert_eq!(task_alias_of("claude-opus-5@xhigh"), "opus");
    }

    #[test]
    fn task_alias_unrecognized_is_empty() {
        assert_eq!(task_alias_of("gpt-5"), "");
        assert_eq!(task_alias_of("claude-mystery-9"), "");
        assert_eq!(task_alias_of(""), "");
        assert_eq!(task_alias_of("   "), "");
    }

    // ===== downgrade_task_alias (#5687) =====

    #[test]
    fn downgrade_walks_the_cost_ladder_one_rung_at_a_time() {
        assert_eq!(downgrade_task_alias("fable"), "opus");
        assert_eq!(downgrade_task_alias("opus"), "sonnet");
        assert_eq!(downgrade_task_alias("sonnet"), "haiku");
    }

    /// The `fable → opus` hop must match the pre-existing `MODEL_REFUSAL`
    /// fallback documented in sweep.md, so the two fallbacks cannot drift.
    #[test]
    fn downgrade_reproduces_the_refusal_fallback_hop() {
        assert_eq!(downgrade_task_alias("fable"), "opus");
        assert_eq!(downgrade_task_alias("claude-fable-5"), "opus");
    }

    #[test]
    fn downgrade_accepts_pinned_ids_and_effort_suffixes() {
        assert_eq!(downgrade_task_alias("claude-opus-5"), "sonnet");
        assert_eq!(downgrade_task_alias("claude-sonnet-4-6"), "haiku");
        assert_eq!(downgrade_task_alias("sonnet@xhigh"), "haiku");
        assert_eq!(downgrade_task_alias("claude-opus-5@xhigh"), "sonnet");
        assert_eq!(downgrade_task_alias("claude-3-opus"), "sonnet");
    }

    /// Terminal cases: the caller must fall through to normal mid-phase-death
    /// handling rather than receive a guessed model.
    #[test]
    fn downgrade_is_empty_at_the_cheapest_rung_or_when_unrecognized() {
        assert_eq!(downgrade_task_alias("haiku"), "");
        assert_eq!(downgrade_task_alias("claude-haiku-5"), "");
        assert_eq!(downgrade_task_alias("haiku@xhigh"), "");
        assert_eq!(downgrade_task_alias("gpt-5"), "");
        assert_eq!(downgrade_task_alias(""), "");
    }

    /// Repeated application terminates — no cycle, no infinite downgrade loop.
    #[test]
    fn downgrade_terminates_after_walking_the_whole_ladder() {
        let mut model = "fable".to_string();
        let mut steps = 0;
        loop {
            let next = downgrade_task_alias(&model);
            if next.is_empty() {
                break;
            }
            model = next;
            steps += 1;
            assert!(steps <= TASK_TOOL_ALIASES.len(), "downgrade ladder did not terminate");
        }
        assert_eq!(model, "haiku");
        assert_eq!(steps, TASK_TOOL_ALIASES.len() - 1);
    }

    // ===== tierModels + optimization presets (#4238) =====

    #[test]
    fn unconfigured_repo_has_no_tier_mapping() {
        assert_eq!(resolve_tier_model(Some("complex"), "claude", &empty(), None), "");
        assert_eq!(resolve_tier_model(Some("routine"), "claude", &empty(), None), "");
        assert_eq!(resolve_tier_model(None, "claude", &empty(), None), "");
    }

    #[test]
    fn tier_models_entry_resolves_through_the_alias_map() {
        let cfg = json!({"sweep": {"tierModels": {"claude": {"complex": "opus"}}}});
        assert_eq!(resolve_tier_model(Some("complex"), "claude", &cfg, None), "claude-opus-5");
    }

    #[test]
    fn unknown_tier_falls_back_to_routine() {
        let cfg = json!({"sweep": {"tierModels": {"claude": {"routine": "sonnet"}}}});
        assert_eq!(resolve_tier_model(Some("bogus"), "claude", &cfg, None), "sonnet");
        assert_eq!(resolve_tier_model(None, "claude", &cfg, None), "sonnet");
    }

    #[test]
    fn unknown_runtime_has_no_mapping() {
        let cfg = json!({"sweep": {"tierModels": {"claude": {"routine": "sonnet"}}}});
        assert_eq!(resolve_tier_model(Some("routine"), "codex", &cfg, None), "");
    }

    #[test]
    fn cost_and_speed_presets_materialize() {
        let cost = json!({"sweep": {"optimization": "cost"}});
        assert_eq!(resolve_tier_model(Some("mechanical"), "claude", &cost, None), "haiku");
        assert_eq!(resolve_tier_model(Some("routine"), "claude", &cost, None), "sonnet");
        assert_eq!(resolve_tier_model(Some("complex"), "claude", &cost, None), "claude-opus-5");

        let speed = json!({"sweep": {"optimization": "speed"}});
        assert_eq!(resolve_tier_model(Some("mechanical"), "claude", &speed, None), "sonnet");
        assert_eq!(resolve_tier_model(Some("routine"), "claude", &speed, None), "claude-opus-5");
    }

    /// The acceptance criterion for #4238 Phase B: `balanced`/unset must
    /// materialize NOTHING, so an unconfigured repo dispatches byte-identically
    /// to pre-Phase-B behavior.
    #[test]
    fn balanced_preset_is_empty() {
        assert!(optimization_preset("balanced").is_empty());
        assert!(optimization_preset("nonsense").is_empty());
        let balanced = json!({"sweep": {"optimization": "balanced"}});
        for tier in COMPLEXITY_TIERS {
            assert_eq!(resolve_tier_model(Some(tier), "claude", &balanced, None), "");
            assert_eq!(resolve_tier_model(Some(tier), "claude", &empty(), None), "");
        }
    }

    #[test]
    fn explicit_tier_models_entry_beats_the_preset() {
        let cfg = json!({
            "sweep": {
                "optimization": "cost",
                "tierModels": {"claude": {"mechanical": "sonnet"}}
            }
        });
        // preset would say haiku; the explicit entry wins.
        assert_eq!(resolve_tier_model(Some("mechanical"), "claude", &cfg, None), "sonnet");
        // …and the preset still fills the tiers tierModels leaves unmapped.
        assert_eq!(resolve_tier_model(Some("routine"), "claude", &cfg, None), "sonnet");
    }

    #[test]
    fn env_override_beats_config_profile() {
        let cfg = json!({"sweep": {"optimization": "balanced"}});
        assert_eq!(resolve_optimization_profile(&cfg, Some("cost")), "cost");
        assert_eq!(resolve_tier_model(Some("mechanical"), "claude", &cfg, Some("cost")), "haiku");
        // An empty env value is "unset", not an override.
        assert_eq!(resolve_optimization_profile(&cfg, Some("")), "balanced");
    }

    #[test]
    fn invalid_profile_falls_back_to_balanced() {
        assert_eq!(resolve_optimization_profile(&empty(), Some("bogus")), "balanced");
        assert_eq!(
            resolve_optimization_profile(&json!({"sweep": {"optimization": "bogus"}}), None),
            "balanced"
        );
        assert_eq!(
            resolve_optimization_profile(&json!({"sweep": {"optimization": 7}}), None),
            "balanced"
        );
        assert_eq!(resolve_optimization_profile(&empty(), None), "balanced");
    }

    #[test]
    fn profile_values_are_trimmed_and_case_insensitive() {
        assert_eq!(resolve_optimization_profile(&empty(), Some("  COST  ")), "cost");
        assert_eq!(
            resolve_optimization_profile(&json!({"sweep": {"optimization": " Speed "}}), None),
            "speed"
        );
    }

    /// Neither a Curator marker nor an optimization profile may ever dispatch
    /// `fable` — before OR after alias resolution (#3702).
    #[test]
    fn never_resolves_to_fable() {
        let direct = json!({"sweep": {"tierModels": {"claude": {"complex": "fable"}}}});
        assert_eq!(resolve_tier_model(Some("complex"), "claude", &direct, None), "");

        let via_alias = json!({
            "sweep": {
                "modelAliases": {"opus": "claude-fable-5"},
                "tierModels": {"claude": {"complex": "opus"}}
            }
        });
        assert_eq!(resolve_tier_model(Some("complex"), "claude", &via_alias, None), "");

        // And the presets themselves never name fable.
        for profile in OPTIMIZATION_PROFILES {
            for (_, model) in optimization_preset(profile) {
                assert_ne!(model, "fable");
            }
        }
    }

    #[test]
    fn effort_suffix_survives_tier_resolution() {
        let cfg = json!({"sweep": {"tierModels": {"claude": {"complex": "opus@xhigh"}}}});
        assert_eq!(
            resolve_tier_model(Some("complex"), "claude", &cfg, None),
            "claude-opus-5@xhigh"
        );
    }

    #[test]
    fn tier_models_tolerates_malformed_shapes() {
        for cfg in [
            json!({"sweep": {"tierModels": "nope"}}),
            json!({"sweep": {"tierModels": {"claude": "nope"}}}),
            json!({"sweep": {"tierModels": {"claude": {"routine": 5}}}}),
            json!({"sweep": {"tierModels": {"claude": {"routine": "  "}}}}),
        ] {
            assert_eq!(resolve_tier_model(Some("routine"), "claude", &cfg, None), "");
        }
    }

    #[test]
    fn tier_and_runtime_keys_are_normalized() {
        let cfg = json!({"sweep": {"tierModels": {"  CLAUDE ": {"  COMPLEX ": "sonnet"}}}});
        assert_eq!(resolve_tier_model(Some(" Complex "), " Claude ", &cfg, None), "sonnet");
    }
}
