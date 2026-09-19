//! The Codex **provider health feedback** bridge: turning an adapter's
//! `LOOM_TERMINAL_RESULT` record into a `tokens_pool::health` write.
//!
//! Split out of `quarantine.rs` when #8277 threaded the in-flight model
//! through this path. It is the only production caller of
//! `tokens_pool::record_terminal_for_model`, and the sole producer for
//! #8058 Phase 2's class-scoped `class_cooldowns` marks — a topic of its own,
//! and `quarantine.rs` is over the file-size ratchet's threshold and
//! therefore frozen (see `.loom/docs/file-size-policy.md`).

use super::*;

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
        if let Err(error) = tokens_pool::record_terminal_for_model(
            &self.config.workspace_root,
            &id,
            result.category,
            result.model.as_deref(),
            "spawn-codex:v1",
        ) {
            log::warn!(
                "sweep_registry: failed to persist Codex terminal feedback for {sweep_id}: {error}"
            );
        }
    }
}

#[cfg(test)]
#[path = "provider_health_feedback_tests.rs"]
mod tests;
