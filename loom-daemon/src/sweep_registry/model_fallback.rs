//! Selection-time model fallback — `autonomous.modelFallback` (#13873).
//!
//! The daemon and role-runner paths pin a model per role
//! (`autonomous.model`, `autonomous.roleRunner.model`,
//! `roleModels.<role>`) and, before #13873, had no answer when that model's
//! *class* limit was hit across the whole pool: rotation had nothing to
//! rotate to (the marks were per-account, the ceiling org-wide), the pool
//! preflights held dispatch, and the fleet idled until an operator moved
//! every role off the pinned model by hand (the 2026-11-13 fable incident,
//! 2AMLogic/.loom#596/#597). The operator's direction this implements: keep
//! the stronger model configured and fall back one rung automatically when
//! its limit is hit, since limits reset.
//!
//! # Where the fallback resolves — daemon-side, one source of truth
//!
//! The decision lives HERE, in the daemon's model resolution, not in
//! `spawn-claude.sh`'s selection block: the `loom.dispatch.step` span that
//! must record `loom.step.model_fallback_from` is written by the daemon at
//! spawn time, and the chain is config parsed next to the model keys — a
//! wrapper-side walk would need the chain a second time (a second source of
//! truth) and could not stamp the span at all. The wrapper keeps doing
//! exactly what it already does: `tokens select --model <m>` with the model
//! the daemon resolved — which, after a fallback, is the rung, so selection
//! succeeds on an account the rung's class can actually use.
//!
//! # Non-stickiness by construction
//!
//! Every spawn re-resolves: [`read_model_fallback_config`] is read and the
//! pool re-probed per launch, so the moment an account usable for the
//! configured model reappears (mark expired, probe headroom), the next spawn
//! uses the configured model again. No state outlives a launch.
//!
//! # Never climbs
//!
//! The walk only visits chain entries **strictly below** the configured
//! model (matched by class, so a pinned `claude-opus-5` pins the `opus`
//! rung). A configured model that is absent from the chain never falls back
//! at all — there is no provable "below" for it, and guessing would let a
//! weak pin silently climb to fable, the most constrained tier (#4501's
//! original defect).
//!
//! # Default OFF
//!
//! `enabled` defaults to `false`: an absent or disabled `modelFallback`
//! block must reproduce today's behaviour byte-for-byte (issue AC), so
//! rollout is an explicit per-repo choice. Every pool preflight this module
//! touches keeps its exact account-wide read when the block is disabled.

use std::path::Path;

use crate::tokens_pool::bad_tokens;
use crate::tokens_pool::select::{spawnable_pool_state, SpawnablePoolState};

/// The shipped chain: the sweep ladder's upper rungs (`fable → opus →
/// sonnet`, matching `resolve-model.sh --downgrade`'s order). Configurable
/// per repo through the effective config like the model keys; entries are
/// raw model names compared by class.
pub(crate) const DEFAULT_CHAIN: &[&str] = &["fable", "opus", "sonnet"];

/// Why a launch ran below its configured model (#13873). One closed value
/// today: the selector result "no account usable for the configured model's
/// class" — the only trigger the selection-time fallback has.
pub(crate) const REASON_NO_ACCOUNT_FOR_CLASS: &str = "no-account-for-class";

/// `autonomous.modelFallback` as resolved through the effective config
/// (`config_resolver::resolve_effective_config`, the same tier chain the
/// model keys read). Soft-fails to [`Default`] — disabled, shipped chain —
/// on any error, mirroring [`super::read_autonomous_model`]'s contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelFallbackConfig {
    /// Default `false`: absent must reproduce today's behaviour exactly.
    pub enabled: bool,
    /// Raw model names, strongest first. Blank entries are dropped at parse
    /// time; entries whose class the classifier does not recognize are
    /// skipped by the walk (never an error — a chain must not fail closed
    /// on a model name it has not been taught).
    pub chain: Vec<String>,
}

impl Default for ModelFallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chain: DEFAULT_CHAIN.iter().map(|s| (*s).to_string()).collect(),
        }
    }
}

/// Read `autonomous.modelFallback` for `repo_root`. Absent block, malformed
/// JSON, or a non-object block soft-fails to the default (disabled); a
/// non-array / non-string `chain` soft-fails to the shipped chain, while an
/// explicit empty array is honoured as "nowhere to fall back to" — the walk
/// finds no rung and the launch holds as today; a non-bool `enabled`
/// soft-fails to `false`.
pub(crate) fn read_model_fallback_config(repo_root: &Path) -> ModelFallbackConfig {
    let mut config = ModelFallbackConfig::default();
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.modelFallback")
    else {
        return config;
    };
    let Some(obj) = block.as_object() else {
        return config;
    };
    if let Some(enabled) = obj.get("enabled") {
        config.enabled = enabled.as_bool().unwrap_or(false);
    }
    if let Some(entries) = obj.get("chain").and_then(serde_json::Value::as_array) {
        config.chain = entries
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
    }
    config
}

/// A selection-time fallback that fired (#13873): what launched, what it
/// fell back from, and the selector result that triggered it. Carried to the
/// step span as `loom.step.model_fallback_from` / `_reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelFallbackDecision {
    /// The chain rung to launch (a raw model name from the configured chain).
    pub model: String,
    /// The configured model the launch fell back from.
    pub from: String,
    /// Always [`REASON_NO_ACCOUNT_FOR_CLASS`] today.
    pub reason: &'static str,
}

/// The model a launch should run after selection-time fallback (#13873):
/// the configured model when its class has a usable account (or fallback is
/// disabled / the class is unknown / no rung is usable), else the first rung
/// strictly below it with one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveModel {
    /// The model to launch — the rung when a fallback fired.
    pub model: String,
    /// The configured model before any fallback; `""` when none was resolved
    /// (the CLI-default pass-through). Telemetry's
    /// `loom.step.model_configured`.
    pub configured: String,
    /// Set when a fallback fired.
    pub fallback: Option<ModelFallbackDecision>,
}

/// Resolve [`EffectiveModel`] for `workspace_root` (#13873). Pure re-read of
/// config + pool state per call — never cached, so a fallback never becomes
/// sticky. An empty `configured` (CLI default), an unrecognized class, a
/// missing pool, an explicit-credential spawn (which never consults the
/// pool), or a disabled block all return the configured model unchanged.
pub(crate) fn effective_model(workspace_root: &Path, configured: &str) -> EffectiveModel {
    let config = read_model_fallback_config(workspace_root);
    effective_model_with(workspace_root, configured, &config)
}

/// The rung walk: `None` unless the configured model's class has no usable
/// account AND a strictly-lower chain rung has one.
fn walk_chain(
    workspace_root: &Path,
    configured: &str,
    config: &ModelFallbackConfig,
) -> Option<ModelFallbackDecision> {
    let configured_class = bad_tokens::model_class_of(configured)?;
    let pool = crate::tokens_pool::select::spawnable_pool_state_for_class(
        workspace_root,
        &configured_class,
    );
    if pool.total == 0 || pool.usable > 0 {
        // No pool to fall back within, or the preferred rung is fine.
        return None;
    }
    let classes: Vec<Option<String>> = config
        .chain
        .iter()
        .map(|entry| bad_tokens::model_class_of(entry))
        .collect();
    // Never climb: only entries strictly below the configured model's own
    // position in the chain are candidates; a configured model not in the
    // chain has no provable "below", so it never falls back.
    let start = classes.iter().position(|c| {
        c.as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(&configured_class))
    })?;
    for (entry, class) in config.chain.iter().zip(classes.iter()).skip(start + 1) {
        // An unrecognized rung cannot be probed; the configured class again
        // is the same wall (same marks) — skip both, keep walking.
        let Some(class) = class.as_deref() else {
            continue;
        };
        if class.eq_ignore_ascii_case(&configured_class) {
            continue;
        }
        let pool =
            crate::tokens_pool::select::spawnable_pool_state_for_class(workspace_root, class);
        if pool.total > 0 && pool.usable > 0 {
            return Some(ModelFallbackDecision {
                model: entry.trim().to_string(),
                from: configured.to_string(),
                reason: REASON_NO_ACCOUNT_FOR_CLASS,
            });
        }
    }
    // Nothing usable at any rung: hold exactly as today.
    None
}

/// A pool preflight's fallback-aware answer to "is this launch's pool a
/// wall?" (#13873): the pool state that should hold/skip the launch, `None`
/// when it may proceed. With the block disabled this is byte-for-byte the
/// pre-#13873 account-wide read (`total > 0 && usable == 0`); with it
/// enabled, the usable count is scoped to the class of the model the launch
/// would actually run — the configured model, or the fallback rung the walk
/// would pick — so a fable-exhausted pool with healthy opus capacity lets
/// the launch through (the launch lands on opus) and only an
/// every-rung-exhausted pool holds.
pub(crate) struct LaunchPoolHold {
    /// The exhausted pool, as the caller's skip text / hold record reads it.
    pub pool: SpawnablePoolState,
    /// The class the probe was scoped to; `None` for the account-wide read
    /// (disabled block, or no recognizable class to probe).
    pub class: Option<String>,
}

/// The launch-scoped preflight read ([`LaunchPoolHold`]'s doc). `configured`
/// resolves lazily — the gate only pays for the model resolution when the
/// block is enabled — and answers with the model this spawn is configured to
/// run (empty / `None` = session default, which selects account-wide — the
/// account-wide probe applies).
pub(crate) fn exhausted_for_launch(
    workspace_root: &Path,
    configured: impl FnOnce() -> Option<String>,
) -> Option<LaunchPoolHold> {
    let config = read_model_fallback_config(workspace_root);
    if !config.enabled {
        return account_wide_hold(workspace_root);
    }
    // Explicit-credential spawns never consult the pool, so the pool cannot
    // be their wall. (The unprovisioned-pool check stays ahead of this in
    // the callers, exactly as before #13873.)
    if explicit_credential_env_set() {
        return None;
    }
    let configured = configured()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let effective = match configured {
        Some(m) => effective_model_with(workspace_root, &m, &config),
        None => EffectiveModel {
            model: String::new(),
            configured: String::new(),
            fallback: None,
        },
    };
    match bad_tokens::model_class_of(&effective.model) {
        Some(class) => {
            let pool =
                crate::tokens_pool::select::spawnable_pool_state_for_class(workspace_root, &class);
            (pool.total > 0 && pool.usable == 0).then_some(LaunchPoolHold {
                pool,
                class: Some(class),
            })
        }
        // No model resolved, or a class the classifier does not know: the
        // spawn selects account-wide, so the account-wide read is the honest
        // one — including its "must never fail closed on an unknown name"
        // contract.
        None => account_wide_hold(workspace_root),
    }
}

/// The model-less pool preflight read (the work finder's hold, the
/// preference walk's availability): with the block disabled, the
/// byte-identical account-wide [`SpawnablePoolState`]; with it enabled, the
/// any-class count — an account whose live marks are all class-scoped still
/// counts, so the pool holds only when *no* model class could spawn from
/// it. Per-launch precision (this role's configured model, that chain's
/// rungs) belongs to [`exhausted_for_launch`]; this read answers for every
/// spawn the pool could receive.
pub(crate) fn preflight_pool_state(workspace_root: &Path) -> SpawnablePoolState {
    let config = read_model_fallback_config(workspace_root);
    if config.enabled {
        crate::tokens_pool::select::spawnable_pool_state_any_class(workspace_root)
    } else {
        spawnable_pool_state(workspace_root)
    }
}

/// The pre-#13873 account-wide read both preflights shared.
fn account_wide_hold(workspace_root: &Path) -> Option<LaunchPoolHold> {
    let pool = spawnable_pool_state(workspace_root);
    (pool.total > 0 && pool.usable == 0).then_some(LaunchPoolHold { pool, class: None })
}

/// [`effective_model`] against an already-read config, so a caller that
/// already holds the config (the preflight, resolving both the hold question
/// and the launch model) does not read it twice.
fn effective_model_with(
    workspace_root: &Path,
    configured: &str,
    config: &ModelFallbackConfig,
) -> EffectiveModel {
    let configured = configured.trim();
    if !config.enabled
        || configured.is_empty()
        || explicit_credential_env_set()
        || bad_tokens::model_class_of(configured).is_none()
    {
        return EffectiveModel {
            model: configured.to_string(),
            configured: configured.to_string(),
            fallback: None,
        };
    }
    match walk_chain(workspace_root, configured, config) {
        Some(fallback) => EffectiveModel {
            model: fallback.model.clone(),
            configured: configured.to_string(),
            fallback: Some(fallback),
        },
        None => EffectiveModel {
            model: configured.to_string(),
            configured: configured.to_string(),
            fallback: None,
        },
    }
}

/// Whether this process would hand the child an explicit credential, making
/// the pool irrelevant to its launch: spawn-claude.sh skips token selection
/// entirely under either name, so a pool read must neither hold such a
/// spawn nor change its model (the same two names its own gate checks).
fn explicit_credential_env_set() -> bool {
    ["LOOM_SPAWN_NO_EXPORT", "CLAUDE_CODE_OAUTH_TOKEN"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

#[cfg(test)]
#[path = "model_fallback_tests.rs"]
mod tests;
