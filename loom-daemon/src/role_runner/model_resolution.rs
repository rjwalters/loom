//! Role-runner model resolution (#4501 / #5001) and its runtime reconciliation
//! (#7894), split out of `role_runner.rs` so the over-threshold parent file
//! shrinks rather than grows (`.loom/docs/file-size-policy.md`).
//!
//! Two questions live here, and only here:
//!
//! 1. **Which model does a scheduled role tick run with?** —
//!    [`resolve_role_runner_model`], the per-role/global/shared precedence
//!    chain, including the `"default"` CLI pass-through sentinel.
//! 2. **Is that answer still right once the runtime is known?** —
//!    [`reconcile_unpinned_model_with_runtime`], which degrades an *unpinned*
//!    cross-family conflict to the runtime CLI's own default instead of letting
//!    `ScriptRoleInvocationRunner::invoke`'s #5028 preflight skip the tick
//!    forever. An explicit pin is never touched, so that refusal survives
//!    intact.

use super::*;

/// The detail carried by [`RoleTickOutcome::ModelRuntimeMismatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRuntimeMismatch {
    /// The role that was ticked (e.g. `"judge"`).
    pub role: String,
    /// The admitted runtime (e.g. `"codex"`).
    pub runtime: String,
    /// The model resolved by [`resolve_role_runner_model`] (or the test-only
    /// `with_model` override) for this role.
    pub model: String,
    /// The config/env tier label [`resolve_role_runner_model`] attributes the
    /// model to (e.g. `"default"`, `"autonomous.roleRunner.model"`), unchanged
    /// from what a successful spawn's log header would have recorded.
    pub model_source: String,
    /// The [`crate::sweep_registry::model_runtime_mismatch`] reason string
    /// naming the two conflicting families.
    pub reason: String,
}

impl ModelRuntimeMismatch {
    /// One-line, operator-facing detail. `record_role_tick` stores this
    /// verbatim on the ring record, and `assess_roles` in `health.rs` already
    /// renders a persistent failure's `detail` as-is — so `loom-daemon
    /// health` names the broken config key without an operator reading a
    /// spawn transcript (#5028 AC2).
    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "model/runtime mismatch: {} (model source={}); set \
             autonomous.roleRunner.roleModels.{} to a model the {} runtime accepts — or to \
             \"{}\" to pass through to the {} CLI's own default model, which is the only \
             working shape on a seat that rejects an explicit pin (e.g. a ChatGPT-plan Codex \
             account) — or point this role back at a Claude runtime",
            self.reason,
            self.model_source,
            self.role,
            self.runtime,
            CLI_DEFAULT_MODEL_SENTINEL,
            self.runtime
        )
    }
}

/// Issue #4501 / #5001: resolve the model a role-runner child must run with,
/// joining the SAME precedence chain sweep dispatch uses
/// ([`sweep_registry::resolve_dispatch_model`]) with a per-role override and the
/// role-runner-specific global `autonomous.roleRunner.model` occupying the
/// "explicit request" tier:
///
/// **`autonomous.roleRunner.roleModels.<role>` >
/// `autonomous.roleRunner.model` > `autonomous.model` > shipped
/// [`sweep_registry::DEFAULT_DISPATCH_MODEL`] (`sonnet`)**
///
/// Empty/whitespace values are treated as unset at every tier, so the resolved
/// model is never accidentally the empty string and never silently the
/// CLI-inherited interactive default. Returns the model plus a label naming the
/// tier that supplied it (for the per-role log header).
///
/// # The CLI-default pass-through sentinel (#7894)
///
/// The one way to get an empty model out of this function is to ask for it: a
/// role-runner tier set to [`CLI_DEFAULT_MODEL_SENTINEL`] (`"default"`, or the
/// `"cli-default"` spelling) resolves to the empty string, which
/// [`run_role_with_timeout`] renders as **no `--model` argument at all** so the
/// runtime's own CLI picks the model. That is not a stylistic option — it is the
/// only configuration a ChatGPT-plan Codex seat accepts, because such a seat
/// rejects *every* explicit pin on the wire (`400 invalid_request_error: The
/// 'gpt-5-codex' model is not supported when using Codex with a ChatGPT
/// account`), including a perfectly valid Codex model ID. Before #7894 the only
/// way to express it was to omit the pin entirely — which fell through to the
/// Claude-shaped shipped default and made #5028's mismatch preflight skip the
/// tick forever (#6565).
///
/// # Why the per-role tier (#5001)
///
/// `LOOM_RUNTIME_<ROLE>` gives each role its own **runtime** axis (Claude vs
/// Codex etc.), but before #5001 the model was a single global value shared by
/// every role. The moment one role (e.g. Judge) was pointed at a different
/// provider via `LOOM_RUNTIME_JUDGE=codex`, the globally-pinned Claude alias
/// (`sonnet`) was forwarded verbatim to the Codex adapter, which rejected it with
/// an HTTP 400 — so every Judge tick failed silently, fleet-wide. The per-role
/// override closes that gap: a repo can run Judge on Codex with a Codex-valid
/// model while Curator/Champion keep a Claude alias, all from config.
///
/// Before #4501, `run_role_with_timeout` emitted **no** `--model` argument at
/// all, so every scheduled curator/champion/judge/auditor/guide child inherited
/// whatever the selected account's interactive `claude` default happened to be —
/// the live defect this resolution exists to prevent.
#[must_use]
pub fn resolve_role_runner_model(repo_root: &Path, role: &str) -> (String, String) {
    let config = read_role_runner_config(repo_root);
    let role_key = role.trim().to_ascii_lowercase();
    // Per-role override (#5001) wins over the single global
    // `autonomous.roleRunner.model`; both occupy `resolve_dispatch_model`'s
    // "explicit request" (`Param`) tier, so a `per_role` flag disambiguates the
    // log label. A blank per-role value never reaches here — blanks are dropped
    // at parse time in `read_role_runner_config`, so it falls through to the
    // global tier just like an absent key.
    let (configured, per_role) = match config.role_models.get(&role_key) {
        Some(m) => (Some(m.clone()), true),
        None => (config.model.clone(), false),
    };
    let configured_tier = || {
        if per_role {
            format!("autonomous.roleRunner.roleModels.{role_key}")
        } else {
            "autonomous.roleRunner.model".to_string()
        }
    };
    // Issue #7894: an explicit "let the CLI pick" pin short-circuits the whole
    // alias/precedence chain — it must NOT be resolved through
    // `resolve_model_alias` (nothing to resolve) and must NOT fall through to a
    // lower tier (that would silently reinstate the shipped Claude default this
    // sentinel exists to avoid).
    if configured
        .as_deref()
        .is_some_and(is_cli_default_model_sentinel)
    {
        return (String::new(), format!("{} (CLI default)", configured_tier()));
    }
    let (model, source) = sweep_registry::resolve_dispatch_model(repo_root, configured.as_deref());
    let label = match source {
        sweep_registry::ModelSource::Param if per_role => {
            format!("autonomous.roleRunner.roleModels.{role_key}")
        }
        // `Param` without `per_role` can only arise from the global
        // `autonomous.roleRunner.model` — this function is its only caller.
        sweep_registry::ModelSource::Param => "autonomous.roleRunner.model".to_string(),
        sweep_registry::ModelSource::Config => "autonomous.model".to_string(),
        sweep_registry::ModelSource::Default => SHIPPED_DEFAULT_MODEL_SOURCE.to_string(),
    };
    (model, label)
}

/// Issue #7894: the model-source label [`resolve_role_runner_model`] attributes
/// to the shipped [`sweep_registry::DEFAULT_DISPATCH_MODEL`] tier — i.e. "no
/// pin was found at any tier". [`reconcile_unpinned_model_with_runtime`] keys
/// the pass-through decision off this exact label, so the two must stay in sync;
/// it is a single constant precisely so they cannot drift.
pub(super) const SHIPPED_DEFAULT_MODEL_SOURCE: &str = "default";

/// Issue #7894: the canonical spelling of the "use the runtime CLI's own
/// default model" pin, as it appears in `ModelRuntimeMismatch::detail()`'s
/// remedy text and the docs.
pub const CLI_DEFAULT_MODEL_SENTINEL: &str = "default";

/// Issue #7894: is this configured model value the CLI-default pass-through
/// sentinel rather than a real model name? Trimmed and case-insensitive, with
/// `"cli-default"` accepted as a more explicit synonym of
/// [`CLI_DEFAULT_MODEL_SENTINEL`]. No real Claude or Codex model is named
/// `default`, so this can never shadow a model an operator meant literally.
#[must_use]
pub fn is_cli_default_model_sentinel(value: &str) -> bool {
    let key = value.trim().to_ascii_lowercase();
    matches!(key.as_str(), CLI_DEFAULT_MODEL_SENTINEL | "cli-default")
}

/// Issue #7894: reconcile an **unpinned** resolved model with the runtime the
/// role was actually admitted onto, returning the `(model, model_source)` pair
/// the tick should launch with.
///
/// The bug this closes: `resolve_role_runner_model` knows nothing about
/// runtimes, so a role bound to `codex` via `runtimes.roles.<role>` with no
/// `roleModels` pin falls through every tier to the shipped Claude-shaped
/// default (`sonnet`). #5028's preflight then — correctly, on the evidence it
/// has — refuses that pair as a provable cross-family conflict, so the tick
/// skips. Forever: the condition is a pure function of static config, so every
/// subsequent tick skips identically (453+ consecutive skips on the host that
/// filed #6565). And the "obvious" remedy of pinning a Codex model is not
/// available on a ChatGPT-plan seat, which rejects every explicit pin — so that
/// configuration was simultaneously the only correct one and permanently
/// unadmittable.
///
/// The resolution: when the model came from the **shipped default** tier (no
/// operator ever asked for it) and it conflicts with the admitted runtime,
/// degrade to the CLI-default pass-through instead of refusing — the same
/// outcome an explicit [`CLI_DEFAULT_MODEL_SENTINEL`] pin produces. A model that
/// reached us from any *configured* tier is left exactly as-is, so #5028's
/// refusal of a genuinely mismatched explicit pin is preserved verbatim: an
/// operator who pinned `sonnet` onto a Codex runtime still gets the refusal and
/// the log line, because that pin is a statement of intent that is wrong.
pub(super) fn reconcile_unpinned_model_with_runtime(
    runtime: &str,
    model: String,
    model_source: String,
) -> (String, String) {
    if model_source != SHIPPED_DEFAULT_MODEL_SOURCE {
        return (model, model_source);
    }
    let Some(reason) = crate::sweep_registry::model_runtime_mismatch(runtime, &model) else {
        return (model, model_source);
    };
    log::info!(
        "role_runner: no model pin for the {runtime} runtime ({reason}); launching with the \
         {runtime} CLI's own default model instead of skipping the tick (#7894)"
    );
    (
        String::new(),
        format!("{SHIPPED_DEFAULT_MODEL_SOURCE} (CLI default for {runtime})"),
    )
}
