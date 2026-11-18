//! Per-role model resolution for the ATTENDED subagent path (issue #13956).
//!
//! The daemon dispatch path resolves a sweep child's model in
//! [`crate::sweep_registry::model`]; the in-session `/loom:sweep` skill
//! resolves its Task-tool dispatches by hand, following the prose chain in
//! `sweep-execution-model.md`. A *bare* attended spawn — any session doing
//! `Task(subagent_type = "loom-judge")` outside a sweep — had no
//! deterministic way to consult that chain, and the shipped agent
//! definitions deliberately carry no `model:` frontmatter (the operator
//! declined hardcoded tiers, #13956), so such a spawn silently inherited the
//! parent session's model.
//!
//! This module is the missing lookup: given a role name, walk the SAME chain
//! the sweep prose documents and report the resolved model plus WHICH tier
//! supplied it, so an experiment can confirm what actually ran:
//!
//! 1. explicit override (the caller's own tier-1 param) -> `param`
//! 2. `terminals[].roleConfig.model`, matched by `roleConfig.roleFile`
//!    (`judge` <-> `judge.md`, case- and suffix-insensitive)
//!    -> `roleConfig.model`
//! 3. `.loom/roles/<role>.json` `suggestedModel` -> `suggestedModel`
//! 4. nothing configured -> `inherit`
//!
//! Tier 4 keeps the zero-config behavior byte-for-byte: the caller omits the
//! Task `model` parameter and the subagent inherits the parent session's
//! model, exactly as before this module existed. Both config tiers read the
//! *effective* config (private/shared defaults -> `.loom/config.json` ->
//! `.loom-project/project.json` -> `.loom-local/fleet-repo.json` ->
//! `.loom-local/local.json`), so a per-host override needs no shipped-file
//! edit and survives every resync — the setting lives entirely outside the
//! resynced surfaces.
//!
//! Everything soft-fails (missing role JSON, malformed config, blank
//! values) — model selection must never block a spawn, matching the sweep
//! prose's resolution-failure rule. Resolution anchors on the CURRENT
//! DIRECTORY the same way [`super::model_tiers::load_config`] does, so the
//! `resolve-role-model.sh` stub keeps working when invoked as
//! `./.loom/scripts/resolve-role-model.sh` from a repo root.
//!
//! NOT modeled here (deliberately): the sweep skill's dispatch *policy* —
//! the escalation ladder, No-Fable-Judge, `sweep.modelAliases` resolution —
//! stays in the prose where the policy lives. This module answers only
//! "what does the chain say for this role?"; a caller composing it with
//! `--task-alias` gets the mechanical [`super::model_tiers::task_alias_of`]
//! degradation, nothing more.
//!
//! Reasoning effort has NO attended-path knob: the in-session Task tool's
//! interface carries model only (the #4282 "no effort knob" rule), so a
//! `roleEfforts` setting remains a daemon-path setting; `model-selection.md`
//! documents this.

use std::path::Path;

use serde_json::Value;

/// The chain supplied the model: an explicit caller override (`--model`).
pub const SOURCE_PARAM: &str = "param";
/// The chain supplied the model: a `terminals[].roleConfig.model` pin.
pub const SOURCE_ROLE_CONFIG: &str = "roleConfig.model";
/// The chain supplied the model: the role JSON's `suggestedModel` default.
pub const SOURCE_SUGGESTED: &str = "suggestedModel";
/// Nothing configured: the caller omits the `model` parameter and the
/// subagent inherits the parent session's model (zero-config behavior).
pub const SOURCE_INHERIT: &str = "inherit";

/// One resolution result: the model (if any) and a stable label naming the
/// tier that supplied it — the observability half of #13956's contract, so
/// an operator can confirm which tier a spawn actually ran on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoleModel {
    /// The resolved model (alias or pinned ID), or `None` when the chain
    /// yields nothing and the caller must omit the `model` parameter.
    pub model: Option<String>,
    /// Which tier supplied it: [`SOURCE_PARAM`], [`SOURCE_ROLE_CONFIG`],
    /// [`SOURCE_SUGGESTED`], or [`SOURCE_INHERIT`].
    pub source: &'static str,
}

/// Normalize a role name the way the role config spells it: `judge`,
/// `Judge`, and `judge.md` all resolve the same role. Mirrors the
/// `.md`-suffix strip `role_validation::extract_roles_from_value` applies to
/// `roleConfig.roleFile`, plus case folding so a `Judge` argument matches a
/// `judge.md` roleFile (the same lower-casing `parse_effort_config` applies
/// to `roleEfforts` keys).
#[must_use]
pub fn normalize_role(role: &str) -> String {
    role.trim().trim_end_matches(".md").to_lowercase()
}

/// Tier 2: the `terminals[]` entry whose `roleConfig.roleFile` matches the
/// role -> its optional `roleConfig.model`. The FIRST `roleFile` match
/// decides (a duplicated roleFile is a config error and a deterministic
/// first-match rule beats guessing); a blank or absent `model` on the
/// matching terminal yields nothing, exactly like the prose chain's
/// "resolves to an empty string" clause. Soft-fails to `None` on any shape
/// deviation (`terminals` missing or not an array, entries without the
/// keys, non-string values).
fn role_config_model(config: &Value, role_norm: &str) -> Option<String> {
    let terminals = config.get("terminals")?.as_array()?;
    for terminal in terminals {
        let Some(role_config) = terminal.get("roleConfig") else {
            continue;
        };
        let Some(role_file) = role_config.get("roleFile").and_then(Value::as_str) else {
            continue;
        };
        if normalize_role(role_file) != role_norm {
            continue;
        }
        return role_config
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m: &&str| !m.is_empty())
            .map(String::from);
    }
    None
}

/// Tier 3: `.loom/roles/<role>.json` -> `suggestedModel`. The path is
/// symlink-tolerant (Loom installs `.loom/roles` as a symlink in this repo,
/// the same tolerance `agent_session::spawn::validate_role` extends to role
/// *definitions*). Soft-fails to `None` on a missing or malformed file or a
/// blank/absent `suggestedModel`.
fn suggested_model(repo_root: &Path, role_norm: &str) -> Option<String> {
    let path = repo_root
        .join(".loom")
        .join("roles")
        .join(format!("{role_norm}.json"));
    if path.symlink_metadata().is_err() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("suggestedModel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m: &&str| !m.is_empty())
        .map(String::from)
}

/// Walk the attended-path chain for `role` against an already-resolved
/// effective config and a repo root (the current directory, matching
/// [`super::model_tiers::load_config`]'s anchoring). Never fails: every
/// absent/malformed tier falls through to the next, and an unconfigured
/// role resolves to [`SOURCE_INHERIT`].
#[must_use]
pub fn resolve_role_model(
    config: &Value,
    repo_root: &Path,
    role: &str,
    explicit: Option<&str>,
) -> ResolvedRoleModel {
    // Tier 1: explicit override. Blank means unset, the same contract
    // `resolve_dispatch_model` applies to its explicit param.
    if let Some(model) = explicit.map(str::trim).filter(|m| !m.is_empty()) {
        return ResolvedRoleModel {
            model: Some(model.to_string()),
            source: SOURCE_PARAM,
        };
    }

    let role_norm = normalize_role(role);
    if role_norm.is_empty() {
        return ResolvedRoleModel {
            model: None,
            source: SOURCE_INHERIT,
        };
    }

    // Tier 2: workspace override.
    if let Some(model) = role_config_model(config, &role_norm) {
        return ResolvedRoleModel {
            model: Some(model),
            source: SOURCE_ROLE_CONFIG,
        };
    }

    // Tier 3: role default.
    if let Some(model) = suggested_model(repo_root, &role_norm) {
        return ResolvedRoleModel {
            model: Some(model),
            source: SOURCE_SUGGESTED,
        };
    }

    // Tier 4: session default.
    ResolvedRoleModel {
        model: None,
        source: SOURCE_INHERIT,
    }
}

#[cfg(test)]
#[path = "role_model_tests.rs"]
mod tests;
