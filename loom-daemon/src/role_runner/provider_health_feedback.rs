//! The role-tick analogue of `sweep_registry`'s provider-health bridge:
//! after a **codex**-runtime role tick exits, parse the tick's own
//! `LOOM_TERMINAL_RESULT` record out of its own per-role log and persist it
//! as account health (issue #8443).
//!
//! Before this module, a codex-pinned role (`runtimes.roles.<role> =
//! "codex"`) never fed anything back into `.loom/account-health.json`: the
//! adapter (`spawn-codex.sh`) prints the record on every exit, but
//! `sweep_registry::provider_health_feedback`'s doc comment names itself
//! "the only production caller of `tokens_pool::record_terminal_for_model`"
//! — and it is keyed on a `sweep_id=` anchor that only exists in a sweep's
//! own log. A role tick's own child never had anything reading its
//! terminal line back into health, so #8408's pre-spawn codex gate
//! (`role_runner::runtime_preflight::codex_gate`) had no holds to read on a
//! host where codex is used only by role ticks: every enabled account
//! looked spawnable no matter how many times it had already died on
//! `TOKEN_EXHAUSTED`.
//!
//! This module closes that gap by reusing the sweep path's own log parsers
//! (`sweep_registry::parse_terminal_result_after` /
//! `sweep_registry::parse_token_name_after`, both re-exported at the
//! `sweep_registry` module root via `pub use crash_signals::*;`) rather
//! than reimplementing terminal-line parsing, with the same
//! validate-then-write shape as
//! `sweep_registry::SweepRegistry::apply_provider_health_feedback`.

use super::*;
use crate::tokens_pool::{self, AccountId, AccountProvider};

/// After a role tick exits, parse its own `LOOM_TERMINAL_RESULT` record out
/// of `log_path` — scoped to this tick's own dispatch region via
/// `tick_anchor` (a string unique to this tick, appearing in the header
/// line `run_role_with_timeout` writes before spawning; mirrors the sweep
/// path's `sweep_id=` anchor) — and persist it as account health.
///
/// A no-op unless `admission` names the `codex` runtime: a Claude-runtime
/// tick, or a test invocation that opted out of admission entirely (a
/// `spawn_bin` override, which leaves `admission` as `None`), never reaches
/// this far — mirroring `apply_provider_health_feedback`'s own
/// `info.runtime != "codex"` guard.
pub(super) fn apply_role_tick_provider_health_feedback(
    workspace_root: &Path,
    log_path: &Path,
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    tick_anchor: &str,
    exit_code: Option<i32>,
) {
    let Some(admission) = admission else {
        return;
    };
    if admission.runtime != "codex" {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return;
    };
    // The account the real spawn selected, captured the same way the sweep
    // path captures `SweepInfo::token_name` (from the adapter's own
    // `# LOOM_ACCOUNT name=…` log line) — independent of the terminal
    // record's own self-reported `account=`, so a stale or malformed
    // terminal line can never claim a hold for an account this tick did not
    // actually use.
    let Some(expected_account) = sweep_registry::parse_token_name_after(&contents, tick_anchor)
    else {
        return;
    };
    let Some(result) = sweep_registry::parse_terminal_result_after(&contents, tick_anchor) else {
        return;
    };
    if result.provider != AccountProvider::Codex
        || result.account != expected_account
        || exit_code.is_some_and(|code| code != result.exit_code)
    {
        log::warn!(
            "role_runner: ignored mismatched Codex terminal feedback for role={}",
            admission.role
        );
        return;
    }
    let id = AccountId {
        provider: result.provider,
        name: result.account,
    };
    if let Err(error) = tokens_pool::record_terminal_for_model(
        workspace_root,
        &id,
        result.category,
        result.model.as_deref(),
        "spawn-codex:v1",
    ) {
        log::warn!(
            "role_runner: failed to persist Codex terminal feedback for role={}: {error}",
            admission.role
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
