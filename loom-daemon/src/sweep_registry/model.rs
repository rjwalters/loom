//! Dispatch model resolution and the tiered model-alias ladder.

use super::*;

/// Issue #3944: the shipped default model for **autonomous / daemon-dispatched**
/// sweep children when neither an explicit dispatch `model` param nor an
/// `autonomous.model` config value is present.
///
/// This is deliberately a **non-premium tier**. Without it, a daemon-dispatched
/// child (`claude -p "/loom:sweep N"`) emits **no** `--model` flag and inherits
/// whatever model the operator last configured interactively — on the v0.15.0
/// canary that was a premium tier (Fable 5) which meters premium usage credits
/// and hard-failed every spawn with "out of usage credits". An autonomous fleet
/// must never silently inherit a premium interactive default, so daemon dispatch
/// always pins an explicit, cost-appropriate model. `sonnet` is chosen as the
/// sane default (fast + cheap for the bulk of build work); override per-repo via
/// `autonomous.model` in `.loom/config.json`, or per-dispatch via the
/// `dispatch_sweep` `model` param.
pub const DEFAULT_DISPATCH_MODEL: &str = "sonnet";

/// Issue #3944: which tier of the dispatch-model precedence chain supplied the
/// resolved model. Surfaced in the daemon dispatch log line so an operator can
/// tell *why* a child is running a given model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// An explicit `dispatch_sweep` `model` param (highest precedence).
    Param,
    /// `autonomous.model` in `.loom/config.json`.
    Config,
    /// The shipped [`DEFAULT_DISPATCH_MODEL`] fallback (never a premium tier).
    Default,
}

impl ModelSource {
    /// A stable lower-case label for logging (`param` / `config` / `default`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelSource::Param => "param",
            ModelSource::Config => "config",
            ModelSource::Default => "default",
        }
    }
}

/// Read `autonomous.model` from `<repo_root>/.loom/config.json`, soft-failing to
/// `None` on any error (missing file, malformed JSON, absent `autonomous` /
/// `model` key, empty/whitespace string). Mirrors the soft-fail contract of the
/// other `autonomous.*` config readers (`work_finder::read_work_finder_config`,
/// `main_health_gate::read_autonomous_gate_config`).
#[must_use]
pub fn read_autonomous_model(repo_root: &Path) -> Option<String> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "autonomous.model")?
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(String::from)
}

/// Issue #3944: resolve the model a daemon-dispatched child should run with,
/// per the precedence **explicit dispatch `model` param > `autonomous.model` in
/// `.loom/config.json` > shipped [`DEFAULT_DISPATCH_MODEL`]**. Empty/whitespace
/// strings are treated as unset at every tier. Returns the resolved model plus
/// the [`ModelSource`] that supplied it (for the dispatch log line).
///
/// All autonomous dispatch paths (work-finder, epic supervisor) resolve with
/// `explicit = None` so they never fall through to the CLI-inherited default;
/// `dispatch_sweep` passes its optional `model` param as `explicit` so an
/// explicit request still wins, and an absent one still lands on the shipped
/// default rather than the operator's interactive CLI default.
#[must_use]
pub fn resolve_dispatch_model(repo_root: &Path, explicit: Option<&str>) -> (String, ModelSource) {
    let (model, source) = if let Some(m) = explicit.map(str::trim).filter(|m| !m.is_empty()) {
        (m.to_string(), ModelSource::Param)
    } else if let Some(m) = read_autonomous_model(repo_root) {
        (m, ModelSource::Config)
    } else {
        (DEFAULT_DISPATCH_MODEL.to_string(), ModelSource::Default)
    };
    // Issue #3982: resolve the logical tier/alias to the concrete model ID before
    // it goes on the wire. This is the daemon's half of the single indirection
    // point shared with `sweep.md` (via `resolve-model.sh`) and `loom_tools`
    // (`model_tiers`): every consumer keeps naming `opus`, and exactly one place
    // decides that `opus` means `claude-opus-5` (the CLI's own `opus` alias still
    // resolves to a gen-4 model). Applied at ALL tiers so an operator's
    // `autonomous.model = "opus"` (or an explicit `--model opus`) also reaches
    // Opus 5; the shipped default `sonnet` is not pinned and passes through.
    (resolve_model_alias(repo_root, &model), source)
}

/// Issue #3982: the shipped logical-tier → pinned model-ID default map. Pins ONLY
/// tiers whose bare CLI alias resolves to a stale generation — today just
/// `opus → claude-opus-5`. `sonnet`/`fable` are intentionally absent: the CLI
/// already resolves them to the current generation, so they pass through
/// unchanged and track future generations with no edit here. Overridable per-repo
/// via `.loom/config.json` → `sweep.modelAliases`. Kept byte-identical to the
/// Python default in `loom_tools/model_tiers.py::_DEFAULT_TIER_ALIASES`.
pub(crate) const DEFAULT_TIER_ALIASES: &[(&str, &str)] = &[("opus", "claude-opus-5")];

/// Read the `sweep.modelAliases` override map for `repo_root`, resolved through
/// the full config tier chain (`config_resolver::resolve_effective_config`:
/// private/shared defaults → legacy `.loom/config.json` →
/// `.loom-project/project.json` → `.loom-local/local.json`). Soft-fails to an
/// empty map on any error (missing/malformed tiers, absent key, non-object
/// value). Blank keys/values are dropped. Mirrors the soft-fail contract of
/// [`read_autonomous_model`] and `model_tiers._config_overrides`.
///
/// **Cross-tier merge (#4059):** `deep_merge` merges objects rather than
/// replacing them, so a `sweep.modelAliases` map split across tiers now *unions*
/// its keys, with the higher-precedence tier winning on any shared key. This is
/// deliberate and desirable — a `.loom-project`/`.loom-local` tier can repoint
/// an individual alias while inheriting the rest from the legacy tier — and is
/// asserted explicitly by `read_model_aliases_merges_across_tiers`.
///
/// **Single implementation (#4060 / #4275, epic #4081 family 5):** this used to
/// be one half of a hand-synced Python/Rust twin pair
/// (`model_tiers._config_overrides`), and the twins had already drifted —
/// Python kept blank alias keys, this side dropped them (#4059, Finding 2).
/// `loom_tools/model_tiers.py` was deleted in #4275 and
/// `crate::script_helpers::model_tiers` now calls straight into this function
/// (via [`model_aliases_from_config`]), so the Rust semantics below — blank
/// keys AND blank values dropped, cross-tier `deep_merge` union — are the only
/// surviving definition. The `resolve-model.sh` / `resolve-tier-model.sh` /
/// `sweep-experiment.sh` entry points and the daemon dispatch path therefore
/// cannot disagree by construction.
#[must_use]
pub fn read_model_aliases(repo_root: &Path) -> BTreeMap<String, String> {
    model_aliases_from_config(&crate::config_resolver::resolve_effective_config(repo_root))
}

/// The `sweep.modelAliases` extraction, applied to an **already-resolved**
/// config value.
///
/// Split out of [`read_model_aliases`] (#4275) so the `resolve-model.sh`
/// `--config <path>` explicit bypass — which reads exactly one file and skips
/// all config tiering — shares the same normalization rather than growing a
/// second copy. Soft-fails to an empty map on any error (absent key, non-object
/// value); blank keys and blank values are dropped.
#[must_use]
pub fn model_aliases_from_config(config: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(aliases) = crate::config_resolver::get_path(config, "sweep.modelAliases")
        .and_then(serde_json::Value::as_object)
    else {
        return out;
    };
    for (key, val) in aliases {
        let key = key.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        if let Some(v) = val.as_str().map(str::trim).filter(|v| !v.is_empty()) {
            out.insert(key, v.to_string());
        }
    }
    out
}

/// The effective logical-tier → ID map: shipped [`DEFAULT_TIER_ALIASES`]
/// overlaid with the config's `sweep.modelAliases` overrides.
#[must_use]
pub fn effective_tier_aliases(config: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = DEFAULT_TIER_ALIASES
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    out.extend(model_aliases_from_config(config));
    out
}

/// Issue #3982: resolve a logical model tier/alias to the concrete model ID to
/// dispatch, applying the shipped [`DEFAULT_TIER_ALIASES`] map overlaid with the
/// per-repo `sweep.modelAliases` override. Preserves any `@effort` suffix
/// (`opus@xhigh` → `claude-opus-5@xhigh`). Unknown aliases and pinned IDs
/// (`claude-sonnet-4-6`) pass through unchanged.
#[must_use]
pub fn resolve_model_alias(repo_root: &Path, model: &str) -> String {
    resolve_model_alias_from_config(
        &crate::config_resolver::resolve_effective_config(repo_root),
        model,
    )
}

/// [`resolve_model_alias`] against an already-resolved config value.
///
/// This is the one alias-resolution implementation in the tree (#4275); both
/// the daemon dispatch path and the `resolve-model.sh` CLI reach the wire
/// through it.
#[must_use]
pub fn resolve_model_alias_from_config(config: &serde_json::Value, model: &str) -> String {
    if model.is_empty() {
        return String::new();
    }
    let (base, effort) = match model.split_once('@') {
        Some((b, e)) => (b, Some(e)),
        None => (model, None),
    };
    let key = base.trim().to_lowercase();
    let overrides = model_aliases_from_config(config);
    let resolved = overrides
        .get(&key)
        .map(String::as_str)
        .or_else(|| {
            DEFAULT_TIER_ALIASES
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
        })
        .unwrap_or(base.trim());
    match effort {
        Some(e) => format!("{resolved}@{e}"),
        None => resolved.to_string(),
    }
}

// ----------------------------------------------------------------------------
// Model-cost A/B experiment (#3725) — daemon-native arm assignment (#4809)
// ----------------------------------------------------------------------------

/// Outcome of [`resolve_autonomous_dispatch_model`] — the daemon-native
/// replacement for the `sweep.md` "Model-cost experiment mode" prose
/// instrumentation (issue #4809). That prose never executes in a headless
/// `-p` child (the LLM-authored per-phase `sweep-experiment.sh record`/banner
/// calls simply do not run), and even a perfectly-instrumented child was
/// structurally defeated by the #4501 default-model pin's tier-1 precedence
/// over a "forced" arm that only ever existed in prose. This resolves the
/// experiment's arm assignment as PART of the daemon's own dispatch-model
/// precedence chain, so a resolved `experiment` mode genuinely changes what
/// model is dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentDispatchModel {
    /// The model to dispatch the child with.
    pub model: String,
    /// The effective experiment mode for this dispatch, AFTER the CANARY
    /// guardrail (`"off"` | `"observe"` | `"experiment"`).
    pub mode: String,
    /// The deterministic arm assigned to this issue — `Some("A" | "B")` only
    /// when `mode == "experiment"`. `observe`/`off` dispatches do not force a
    /// model, so they carry no arm at DISPATCH time; the outcome-recording
    /// path (`sweep_registry::outcome_journal`) still attributes an arm to
    /// such a record after the fact via
    /// [`crate::script_helpers::sweep_experiment::infer_arm_from_model`].
    pub arm: Option<&'static str>,
    /// A short label for the dispatch log line naming why `model` won:
    /// `"experiment"` when the arm-forced model won, otherwise the
    /// underlying [`ModelSource`] label (`param`/`config`/`default`) —
    /// unchanged from [`resolve_dispatch_model`] for `off`/`observe`.
    pub source_label: &'static str,
}

/// Issue #4809: resolve the model for an autonomous, per-`issue` sweep
/// dispatch, inserting the model-cost A/B experiment's forced arm model
/// ahead of the #3944/#4501 default-pin precedence chain when — and ONLY
/// when — the workspace resolves to `experiment` mode. Mode resolution reads
/// the SAME env vars (`LOOM_MODEL_EXPERIMENT` / `LOOM_MODEL_EXPERIMENT_CANARY`)
/// and CANARY guardrail (an uncommitted env confirmation or a gitignored
/// `.loom/CANARY` sentinel under `repo_root` — never the committed
/// `sweep.modelExperimentCanary` flag, rejected in #3731) that
/// `sweep-experiment.sh resolve-mode` uses — see
/// [`crate::script_helpers::sweep_experiment::resolve_effective_mode_default`].
///
/// `off` and `observe` modes are BYTE-IDENTICAL to calling
/// [`resolve_dispatch_model`] directly (the required no-behavior-change
/// contract for both) — only `experiment` mode substitutes the arm-forced
/// model, and an explicit dispatch `model` param is never seen at this
/// call's `resolve_dispatch_model(repo_root, None)` fallback (callers pass
/// `explicit = None`, matching every existing autonomous dispatch site).
///
/// The arm itself is [`crate::script_helpers::sweep_experiment::assign_arm`]
/// — a pure function of `issue` (+ an optional complexity stratum, currently
/// always `None` here; real per-issue `<!-- loom:complexity=... -->`
/// extraction at dispatch time is deferred — see issue #4809's PR
/// description), so the SAME issue resolves to the SAME arm across a
/// dispatch resume — no re-roll.
#[must_use]
pub fn resolve_autonomous_dispatch_model(repo_root: &Path, issue: u32) -> ExperimentDispatchModel {
    use crate::script_helpers::sweep_experiment as se;

    let config = crate::config_resolver::resolve_effective_config(repo_root);
    let env_mode = std::env::var("LOOM_MODEL_EXPERIMENT").ok();
    let env_canary = std::env::var("LOOM_MODEL_EXPERIMENT_CANARY").ok();
    let sentinel_path = repo_root.join(se::CANARY_SENTINEL);
    let (mode, warnings) = se::resolve_effective_mode_default(
        env_mode.as_deref(),
        env_canary.as_deref(),
        &config,
        Some(&sentinel_path),
    );
    for w in &warnings {
        log::warn!("sweep-experiment: {w}");
    }

    if mode == "experiment" {
        let arm = se::assign_arm(i64::from(issue), None);
        let model = se::resolved_arm_model(arm, &config);
        return ExperimentDispatchModel {
            model,
            mode,
            arm: Some(arm),
            source_label: "experiment",
        };
    }

    let (model, source) = resolve_dispatch_model(repo_root, None);
    ExperimentDispatchModel {
        model,
        mode,
        arm: None,
        source_label: source.as_str(),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// With no `.loom/config.json` and no explicit param, resolution falls back
    /// to the shipped non-premium default — NOT the CLI-inherited default.
    #[test]
    fn resolve_dispatch_model_falls_back_to_shipped_default() {
        let dir = tempdir().unwrap();
        let (model, source) = resolve_dispatch_model(dir.path(), None);
        assert_eq!(model, DEFAULT_DISPATCH_MODEL);
        assert_eq!(model, "sonnet", "shipped default must be a non-premium tier");
        assert_eq!(source, ModelSource::Default);
    }

    /// `autonomous.model` in config wins over the shipped default when no
    /// explicit param is supplied. The resolved value is passed through the
    /// #3982 tier map, so a logical `opus` reaches `claude-opus-5` on the wire
    /// while the source stays `Config`.
    #[test]
    fn resolve_dispatch_model_reads_autonomous_config() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"autonomous": {"model": "opus"}}"#);
        let (model, source) = resolve_dispatch_model(dir.path(), None);
        assert_eq!(model, "claude-opus-5");
        assert_eq!(source, ModelSource::Config);
    }

    /// An explicit dispatch `model` param wins over both config and the default
    /// (the `dispatch_sweep` highest-precedence tier).
    #[test]
    fn resolve_dispatch_model_explicit_param_wins() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"autonomous": {"model": "opus"}}"#);
        let (model, source) = resolve_dispatch_model(dir.path(), Some("claude-sonnet-4-6"));
        assert_eq!(model, "claude-sonnet-4-6");
        assert_eq!(source, ModelSource::Param);
    }

    /// Empty/whitespace values are treated as unset at every tier: an empty
    /// explicit param falls through to config, and an empty config value falls
    /// through to the shipped default.
    #[test]
    fn resolve_dispatch_model_treats_blank_as_unset() {
        let dir = tempdir().unwrap();
        // Empty explicit param + populated config → config wins (and `opus`
        // resolves through the #3982 tier map to `claude-opus-5`).
        write_config(dir.path(), r#"{"autonomous": {"model": "opus"}}"#);
        let (model, source) = resolve_dispatch_model(dir.path(), Some("   "));
        assert_eq!(model, "claude-opus-5");
        assert_eq!(source, ModelSource::Config);

        // Blank config value → shipped default.
        let dir2 = tempdir().unwrap();
        write_config(dir2.path(), r#"{"autonomous": {"model": "  "}}"#);
        let (model2, source2) = resolve_dispatch_model(dir2.path(), None);
        assert_eq!(model2, DEFAULT_DISPATCH_MODEL);
        assert_eq!(source2, ModelSource::Default);
    }

    /// A config with an `autonomous` block but no `model` key soft-falls to the
    /// shipped default (no panic, no error).
    #[test]
    fn resolve_dispatch_model_absent_model_key_uses_default() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        assert_eq!(read_autonomous_model(dir.path()), None);
        let (model, source) = resolve_dispatch_model(dir.path(), None);
        assert_eq!(model, DEFAULT_DISPATCH_MODEL);
        assert_eq!(source, ModelSource::Default);
    }

    /// Malformed JSON soft-fails to `None` (and thus the shipped default) rather
    /// than propagating an error.
    #[test]
    fn resolve_dispatch_model_malformed_config_soft_fails() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), "{ this is not valid json");
        assert_eq!(read_autonomous_model(dir.path()), None);
        let (model, _) = resolve_dispatch_model(dir.path(), None);
        assert_eq!(model, DEFAULT_DISPATCH_MODEL);
    }

    #[test]
    #[serial(loom_config_env)]
    fn read_autonomous_model_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write_project_model_config(dir.path(), r#"{"autonomous": {"model": "opus"}}"#);
        let model = read_autonomous_model(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(model, Some("opus".to_string()));
    }

    #[test]
    #[serial(loom_config_env)]
    fn read_autonomous_model_project_tier_overrides_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"autonomous": {"model": "sonnet"}}"#);
        write_project_model_config(dir.path(), r#"{"autonomous": {"model": "opus"}}"#);
        let model = read_autonomous_model(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(model, Some("opus".to_string()));
    }

    // --- Issue #3982: logical-tier → concrete-ID resolution --------------- //

    /// The shipped default map pins the stale `opus` alias to gen-5 while leaving
    /// current-generation aliases and pinned IDs untouched (passthrough).
    #[test]
    fn resolve_model_alias_pins_opus_and_passes_through_rest() {
        let dir = tempdir().unwrap(); // no config → shipped defaults only
        assert_eq!(resolve_model_alias(dir.path(), "opus"), "claude-opus-5");
        // sonnet/fable are NOT pinned — the CLI resolves them to gen-5 already.
        assert_eq!(resolve_model_alias(dir.path(), "sonnet"), "sonnet");
        assert_eq!(resolve_model_alias(dir.path(), "fable"), "fable");
        // A pinned ID is never rewritten.
        assert_eq!(resolve_model_alias(dir.path(), "claude-sonnet-4-6"), "claude-sonnet-4-6");
        // An unknown alias passes through unchanged.
        assert_eq!(resolve_model_alias(dir.path(), "mystery"), "mystery");
        // Empty stays empty.
        assert_eq!(resolve_model_alias(dir.path(), ""), "");
    }

    /// The `model@effort` suffix is preserved across resolution.
    #[test]
    fn resolve_model_alias_preserves_effort_suffix() {
        let dir = tempdir().unwrap();
        assert_eq!(resolve_model_alias(dir.path(), "opus@xhigh"), "claude-opus-5@xhigh");
        // Passthrough model keeps its effort too.
        assert_eq!(resolve_model_alias(dir.path(), "sonnet@xhigh"), "sonnet@xhigh");
    }

    /// A `sweep.modelAliases` block repoints a tier without a code change, and
    /// can drop the shipped pin by mapping the alias back to itself.
    #[test]
    fn resolve_model_alias_honors_config_override() {
        // Repoint opus to a hypothetical next generation.
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"sweep": {"modelAliases": {"opus": "claude-opus-6"}}}"#);
        assert_eq!(resolve_model_alias(dir.path(), "opus"), "claude-opus-6");

        // Drop the pin: map opus back to the bare alias (CLI resolves it).
        let dir2 = tempdir().unwrap();
        write_config(dir2.path(), r#"{"sweep": {"modelAliases": {"opus": "opus"}}}"#);
        assert_eq!(resolve_model_alias(dir2.path(), "opus"), "opus");

        // Malformed config soft-falls to the shipped default map.
        let dir3 = tempdir().unwrap();
        write_config(dir3.path(), "{ not json");
        assert_eq!(resolve_model_alias(dir3.path(), "opus"), "claude-opus-5");

        // #4059 Finding 1: the overlay direction is config-WINS / default-LOSES.
        // A config that overrides an UNRELATED alias must NOT disturb `opus`,
        // which still falls back to the shipped `DEFAULT_TIER_ALIASES` default.
        let dir4 = tempdir().unwrap();
        write_config(dir4.path(), r#"{"sweep": {"modelAliases": {"sonnet": "claude-sonnet-9"}}}"#);
        assert_eq!(resolve_model_alias(dir4.path(), "opus"), "claude-opus-5"); // default wins (no override)
        assert_eq!(resolve_model_alias(dir4.path(), "sonnet"), "claude-sonnet-9");
        // config wins
    }

    /// #4059: `sweep.modelAliases` split across the legacy and project tiers is
    /// MERGED by `config_resolver::deep_merge` (object-merge, not replace). Keys
    /// union across tiers, and the higher-precedence tier wins on a shared key.
    /// The private/shared defaults tier is disabled for hermeticity.
    #[test]
    #[serial_test::serial]
    fn read_model_aliases_merges_across_tiers() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        // Legacy tier supplies `opus` and `sonnet`.
        write_config(
            dir.path(),
            r#"{"sweep": {"modelAliases": {"opus": "legacy-opus", "sonnet": "legacy-sonnet"}}}"#,
        );
        // Project tier adds `fable` and overrides `sonnet`.
        let project_dir = dir.path().join(".loom-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"sweep": {"modelAliases": {"sonnet": "project-sonnet", "fable": "project-fable"}}}"#,
        )
        .unwrap();

        let aliases = read_model_aliases(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);

        // Union of keys across both tiers.
        assert_eq!(aliases.get("opus").map(String::as_str), Some("legacy-opus")); // only in legacy
        assert_eq!(aliases.get("fable").map(String::as_str), Some("project-fable")); // only in project
                                                                                     // Higher-precedence (project) tier wins on the shared `sonnet` key.
        assert_eq!(aliases.get("sonnet").map(String::as_str), Some("project-sonnet"));
        assert_eq!(aliases.len(), 3);
    }

    // --- Issue #4809: daemon-native model-cost experiment dispatch --------- //

    /// Clean slate for the experiment env vars this suite mutates — leaked
    /// state from another test in the same process would make these false
    /// pass/fail.
    fn clear_experiment_env() {
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
    }

    /// With no env/config/sentinel, mode resolves to `off` and the result is
    /// byte-identical to calling `resolve_dispatch_model` directly — no arm,
    /// `source_label` is the underlying `ModelSource` label.
    #[test]
    #[serial]
    fn autonomous_dispatch_off_by_default_is_byte_identical() {
        clear_experiment_env();
        let dir = tempdir().unwrap();
        let resolved = resolve_autonomous_dispatch_model(dir.path(), 100);
        clear_experiment_env();

        assert_eq!(resolved.mode, "off");
        assert_eq!(resolved.arm, None);
        assert_eq!(resolved.source_label, "default");
        let (expected_model, expected_source) = resolve_dispatch_model(dir.path(), None);
        assert_eq!(resolved.model, expected_model);
        assert_eq!(resolved.source_label, expected_source.as_str());
    }

    /// `experiment` mode requested with NO canary confirmation downgrades to
    /// `observe` (the #3731 guardrail) — no arm, model resolution unaffected.
    #[test]
    #[serial]
    fn autonomous_dispatch_experiment_without_canary_downgrades_to_observe() {
        clear_experiment_env();
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        let dir = tempdir().unwrap();
        let resolved = resolve_autonomous_dispatch_model(dir.path(), 100);
        clear_experiment_env();

        assert_eq!(resolved.mode, "observe");
        assert_eq!(resolved.arm, None);
        assert_eq!(resolved.model, DEFAULT_DISPATCH_MODEL, "observe must not force a model");
    }

    /// `experiment` mode + a confirmed canary forces the deterministic arm's
    /// model, suppressing the #3944/#4501 default pin — the core #4809 fix.
    #[test]
    #[serial]
    fn autonomous_dispatch_experiment_with_canary_forces_the_arm_model() {
        clear_experiment_env();
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        let dir = tempdir().unwrap();
        // Issue 100, no complexity ⇒ arm A (see `assign_arm`'s parity table).
        let resolved = resolve_autonomous_dispatch_model(dir.path(), 100);
        clear_experiment_env();

        assert_eq!(resolved.mode, "experiment");
        assert_eq!(resolved.arm, Some("A"));
        assert_eq!(resolved.model, "claude-opus-5", "Arm A resolves through the #3982 tier map");
        assert_eq!(resolved.source_label, "experiment");

        // Issue 101 ⇒ arm B (sonnet), still deterministic and un-pinned by
        // the default.
        clear_experiment_env();
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        let resolved_b = resolve_autonomous_dispatch_model(dir.path(), 101);
        clear_experiment_env();
        assert_eq!(resolved_b.arm, Some("B"));
        assert_eq!(resolved_b.model, "sonnet");
    }

    /// Deterministic assignment: repeated calls for the same issue (a
    /// dispatch resume) land on the same arm — no re-roll.
    #[test]
    #[serial]
    fn autonomous_dispatch_arm_assignment_is_deterministic_across_resumes() {
        clear_experiment_env();
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        let dir = tempdir().unwrap();
        let first = resolve_autonomous_dispatch_model(dir.path(), 4242);
        let second = resolve_autonomous_dispatch_model(dir.path(), 4242);
        clear_experiment_env();
        assert_eq!(first.arm, second.arm);
        assert_eq!(first.model, second.model);
    }

    /// A committed `.loom/CANARY` (git-tracked) is refused exactly like the
    /// underlying `sweep-experiment.sh` guardrail — this test only exercises
    /// the untracked-sentinel confirmation path (the tracked-refusal path is
    /// already covered directly against `evaluate_canary` in
    /// `script_helpers::sweep_experiment`); here we confirm the SENTINEL path
    /// (not just the env var) reaches the daemon dispatch resolver.
    #[test]
    #[serial]
    fn autonomous_dispatch_confirms_canary_via_untracked_sentinel_file() {
        clear_experiment_env();
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(dir.path().join(".loom").join("CANARY"), "").unwrap();

        let resolved = resolve_autonomous_dispatch_model(dir.path(), 100);
        clear_experiment_env();

        assert_eq!(resolved.mode, "experiment");
        assert_eq!(resolved.arm, Some("A"));
    }

    /// `observe` mode (explicit) never forces a model, matching #4809's "zero
    /// behavior change" contract even with `autonomous.model` configured.
    #[test]
    #[serial]
    fn autonomous_dispatch_observe_mode_honors_existing_config_precedence() {
        clear_experiment_env();
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "observe");
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"autonomous": {"model": "opus"}}"#);
        let resolved = resolve_autonomous_dispatch_model(dir.path(), 100);
        clear_experiment_env();

        assert_eq!(resolved.mode, "observe");
        assert_eq!(resolved.arm, None);
        assert_eq!(resolved.model, "claude-opus-5", "config tier still resolves through #3982");
        assert_eq!(resolved.source_label, "config");
    }

    /// Ladder monotonicity: no rung of the shipped escalation ladder resolves to
    /// an older generation than the rung below it. This is the regression guard
    /// for #3982 — before the fix, `opus` resolved to a gen-4 model, one below
    /// the `sonnet@xhigh` rung beneath it. The generation each bare alias
    /// resolves to on the wire is the probed table (sonnet/fable → 5, and the
    /// unfixed `opus` alias → 4); the tier map lifts `opus` to gen-5.
    #[test]
    fn resolve_model_alias_ladder_is_monotonic() {
        let dir = tempdir().unwrap();
        // Probed alias generations (mirrors model_tiers._ALIAS_GENERATION).
        fn alias_generation(alias: &str) -> u32 {
            match alias {
                "sonnet" | "fable" | "haiku" => 5,
                "opus" => 4, // the unfixed CLI alias — the bug
                other => panic!("unexpected bare alias in ladder: {other}"),
            }
        }
        fn generation_of(dir: &Path, rung: &str) -> u32 {
            let resolved = resolve_model_alias(dir, rung);
            let base = resolved.split('@').next().unwrap();
            // Pinned ID `claude-<family>-<gen>[-...]` → parse the generation.
            if let Some(rest) = base.strip_prefix("claude-") {
                let mut parts = rest.split('-');
                let first = parts.next().unwrap_or("");
                // `claude-opus-5` → family then gen; `claude-3-5-sonnet` → gen first.
                if let Ok(g) = first.parse::<u32>() {
                    return g;
                }
                if let Some(g) = parts.next().and_then(|p| p.parse::<u32>().ok()) {
                    return g;
                }
            }
            // A bare alias that passed through unmapped.
            alias_generation(base)
        }

        let ladder = ["sonnet", "sonnet@xhigh", "opus", "fable"];
        let gens: Vec<u32> = ladder
            .iter()
            .map(|r| generation_of(dir.path(), r))
            .collect();
        for pair in gens.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "escalation ladder is non-monotonic: {ladder:?} resolved to generations {gens:?}"
            );
        }
        // And specifically the previously-broken rung is now gen-5.
        assert_eq!(generation_of(dir.path(), "opus"), 5);
    }
}
