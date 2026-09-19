//! Claude token-pool checks; other harnesses own their credential resolution.
use super::*;

pub(super) fn check(
    root: &Path,
    logs: &Path,
    role: &str,
    claude_pool: bool,
) -> Option<RoleTickOutcome> {
    if claude_pool && crate::tokens::token_pool_size(root) == 0 {
        NO_TOKEN_POOL_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
        note_pre_spawn_skip(
            logs,
            role,
            "no token pool available (neither a per-repo .loom/tokens/ pool nor a provisioned \
         shared pool); run `loom-daemon tokens bootstrap` — #4642",
        );
        return Some(RoleTickOutcome::NoTokenPool);
    }
    // Do not launch a known exhausted Claude pool (#7607).
    if claude_pool {
        let pool = crate::tokens_pool::select::spawnable_pool_state(root);
        if pool.total > 0 && pool.usable == 0 {
            POOL_EXHAUSTED_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
            let next_clear_at = crate::tokens_pool::select::pool_clear_estimate(&pool.dir);
            note_pre_spawn_skip(
                logs,
                role,
                &format!(
                    "token pool exhausted: 0/{} spawnable in {} (every account bad-marked or \
                 hard-excluded by .ranking); next check ~{} — run `loom-daemon tokens \
                 check --ranking` or `loom-daemon tokens unblock <name>` — #7607",
                    pool.total,
                    pool.dir.display(),
                    next_clear_at.to_rfc3339()
                ),
            );
            return Some(RoleTickOutcome::PoolExhausted {
                total: pool.total,
                next_clear_at,
            });
        }
    }
    None
}
