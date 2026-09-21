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

/// After a role tick exits, feed its own retained log back into the health
/// state of whichever account pool the tick actually spawned against —
/// scoped to this tick's own dispatch region via `tick_anchor` (a string
/// unique to this tick, appearing in the header line
/// `run_role_with_timeout` writes before spawning; mirrors the sweep path's
/// `sweep_id=` anchor).
///
/// **One seam per dispatch surface**, by runtime:
///
/// | Runtime | Reads | Writes |
/// |---|---|---|
/// | `codex` | `LOOM_TERMINAL_RESULT` | `.loom/account-health.json` (#8443) |
/// | native (`pi`/`opencode`) | `# LOOM_LAUNCH` + the harness's error events | the API-key pool's bad marks (#8424 item 1) |
///
/// Both are post-hoc readers of text a launch already wrote — the only
/// option once a native launch `exec`s (see
/// [`crate::api_keys_pool::ingest`]'s design note) — so they belong behind
/// one call rather than two competing hooks in the tick loop. A
/// Claude-runtime tick, or a test invocation that opted out of admission
/// entirely (a `spawn_bin` override, which leaves `admission` as `None`),
/// reaches neither.
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
    if crate::worker_spawn::is_native(&admission.runtime) {
        apply_role_tick_api_key_feedback(
            workspace_root,
            log_path,
            admission,
            tick_anchor,
            exit_code,
        );
        return;
    }
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

/// The native-runtime half (#8424 item 1): ingest this tick's own region of
/// the role log and let [`crate::api_keys_pool::ingest`] decide whether the
/// account it spawned against should be bad-marked.
///
/// Every safety decision lives in that module (pool-selected credentials
/// only, never an exit-0 run, auth failures surfaced without a mark), so this
/// is a call site and a log line — nothing here decides anything about an
/// account.
fn apply_role_tick_api_key_feedback(
    workspace_root: &Path,
    log_path: &Path,
    admission: &crate::runtime_admission::ResolvedRuntime,
    tick_anchor: &str,
    exit_code: Option<i32>,
) {
    let Some(feedback) = crate::api_keys_pool::ingest::ingest_launch_log_at(
        workspace_root,
        log_path,
        tick_anchor,
        exit_code,
    ) else {
        return;
    };
    log::warn!("role_runner: role={} {}", admission.role, feedback.detail);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
