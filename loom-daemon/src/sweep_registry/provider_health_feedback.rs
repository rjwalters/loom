//! The **provider health feedback** bridge: after a sweep's child exits,
//! read the log it already wrote and persist what it says about the account
//! it spawned against — before any reaper retry/failover decision.
//!
//! One seam, two runtimes:
//!
//! | Runtime | Reads | Writes |
//! |---|---|---|
//! | `codex` | the adapter's `LOOM_TERMINAL_RESULT` | `tokens_pool::health` (#8277) |
//! | native (`pi`/`opencode`) | `# LOOM_LAUNCH` + the harness's error events | the API-key pool's bad marks (#8424 item 1) |
//!
//! Both halves are post-hoc readers of already-captured text, which for a
//! native launch is the only possibility at all — it `exec`s, so no Loom
//! process survives to watch the child (see [`crate::api_keys_pool::ingest`]
//! for that design decision). Keeping them behind one call keeps the reaper's
//! own call site unchanged, which matters twice over: `reaper.rs` sits one
//! code line under the file-size ratchet's threshold, and `quarantine.rs`
//! (this file's parent) is already over it and therefore frozen — see
//! `.loom/docs/file-size-policy.md`.
//!
//! The Codex half is the only production caller of
//! `tokens_pool::record_terminal_for_model`, and the sole producer for
//! #8058 Phase 2's class-scoped `class_cooldowns` marks.

use super::*;
use crate::tokens_pool::codex_reset::exhaustion_reset_horizon;

impl SweepRegistry {
    /// Persist provider health before any reaper retry/failover decision.
    pub(crate) fn apply_provider_health_feedback(
        &self,
        sweep_id: &SweepId,
        exit_code: Option<i32>,
    ) {
        let Some(info) = self.entries.get(sweep_id) else {
            return;
        };
        // A native-harness sweep has no `LOOM_TERMINAL_RESULT` and no Codex
        // account; its credential came from the API-key pool instead.
        if crate::worker_spawn::is_native(&info.runtime) {
            self.apply_api_key_pool_feedback(sweep_id, &info.log_path, exit_code);
            return;
        }
        if info.runtime != "codex" || info.token_name == UNKNOWN_TOKEN_NAME {
            return;
        }
        let Ok(contents) = std::fs::read_to_string(&info.log_path) else {
            return;
        };
        let anchor = format!("sweep_id={sweep_id}");
        let Some(result) = parse_terminal_result_after(&contents, &anchor) else {
            return;
        };
        if result.provider != AccountProvider::Codex
            || result.account != info.token_name
            || exit_code.is_some_and(|code| code != result.exit_code)
        {
            log::warn!("sweep_registry: ignored mismatched Codex terminal feedback for {sweep_id}");
            return;
        }
        let id = AccountId {
            provider: result.provider,
            name: result.account,
        };
        // #8539: the provider's own reset horizon, when its refusal named one.
        // Read from the same region, gated on the adapter's classification —
        // see `tokens_pool::codex_reset` for why the text may only ever say
        // *when* a hold ends, never *whether* there is one.
        let reset_at = exhaustion_reset_horizon(&contents, &anchor, result.category);
        match tokens_pool::record_terminal_for_model_with_reset(
            &self.config.workspace_root,
            &id,
            result.category,
            result.model.as_deref(),
            reset_at,
            "spawn-codex:v1",
        ) {
            // #8931: the reason-classified mark, emitted only once persisted.
            Ok(()) => crate::observability::ops::pool_marks::record_codex(result.category),
            Err(error) => log::warn!(
                "sweep_registry: failed to persist Codex terminal feedback for {sweep_id}: {error}"
            ),
        }
    }

    /// The native-runtime half (#8424 item 1): ingest this sweep's own region
    /// of its log — anchored on `sweep_id=<id>`, the same anchor the Codex
    /// half and `containment_signal` use — and let
    /// [`crate::api_keys_pool::ingest`] decide whether the account it spawned
    /// against should be bad-marked.
    ///
    /// Every safety decision (pool-selected credentials only, never an exit-0
    /// run, auth failures surfaced without a mark) lives in that module; this
    /// is a call site and a log line.
    fn apply_api_key_pool_feedback(
        &self,
        sweep_id: &SweepId,
        log_path: &Path,
        exit_code: Option<i32>,
    ) {
        let Some(feedback) = crate::api_keys_pool::ingest::ingest_launch_log_at(
            &self.config.workspace_root,
            log_path,
            &format!("sweep_id={sweep_id}"),
            exit_code,
        ) else {
            return;
        };
        crate::observability::ops::pool_marks::record_api_key(&feedback);
        log::warn!("sweep_registry: {sweep_id} {}", feedback.detail);
    }

    /// The Claude insta-crash seam's account mark (#4122), plus its
    /// reason-classified `loom.pool.account_marks` point (#8931). A method
    /// here, not in `quarantine.rs`, because that file is frozen by the
    /// file-size ratchet.
    pub(crate) fn mark_exhausted_account(
        &self,
        token_name: &str,
        reason: &str,
        signature: &str,
    ) -> Result<(), String> {
        crate::observability::ops::pool_marks::mark_claude_bad(
            &self.config.workspace_root,
            token_name,
            reason,
            signature,
        )
    }
}

#[cfg(test)]
#[path = "provider_health_feedback_tests.rs"]
mod tests;
