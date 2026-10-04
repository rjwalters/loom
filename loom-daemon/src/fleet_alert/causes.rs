//! The single cause -> (explanation, fix) table (#10164).

use std::path::Path;

use crate::tokens_pool::bad_tokens;

/// Why the token pool has no healthy account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCause {
    /// No accounts provisioned at all.
    EmptyPool,
    /// Every account is `blocked` with no `.bad_tokens` history: a 401 or
    /// shape-mismatch probe result, which never self-heals.
    AuthDead,
    /// Quota / session-limit holds that age out on their own.
    Exhausted,
}

/// (what is wrong, what to do).
#[must_use]
pub fn token_text(cause: TokenCause) -> (&'static str, &'static str) {
    match cause {
        TokenCause::EmptyPool => (
            "no accounts are provisioned",
            "Add accounts: `loom-daemon tokens bootstrap --shared` (or `tokens import-from-monitor`).",
        ),
        TokenCause::AuthDead => (
            "the account(s) are auth-dead (auth_401) and will NOT clear on a timer",
            "Re-authenticate the account, or run `loom-daemon tokens import-from-monitor`, then \
             `loom-daemon tokens unblock` and `loom-daemon tokens check --ranking`.",
        ),
        TokenCause::Exhausted => (
            "the accounts are quota/session exhausted",
            "Wait for the limit reset or add accounts (`loom-daemon tokens bootstrap --shared`).",
        ),
    }
}

/// Fix text for one persistent role failure, from its tick detail.
#[must_use]
pub fn role_fix(detail: Option<&str>) -> &'static str {
    let d = detail.unwrap_or_default().to_ascii_lowercase();
    if d.contains("canary") || d.contains("guarded") || d.contains("opencode") {
        "Role launch fell through to a runtime that has no live guarded-canary receipt \
         (spawn-worker exit 78): runtime/version mismatch; check `runtimes.default` and the \
         opencode version."
    } else if d.contains("no-token-pool") || d.contains("token") {
        "Role cannot get a token: see the token-pool alert / `loom-daemon tokens check --ranking`."
    } else {
        "Read `loom-daemon health` (roles section) and the role log under `.loom/logs/`."
    }
}

/// Best-effort cause for a pool with zero healthy accounts, from the pool's
/// `.ranking` (`name|status|...` lines) and `.bad_tokens` history.
#[must_use]
pub fn token_cause(total_accounts: usize, pool_dir: Option<&Path>) -> TokenCause {
    if total_accounts == 0 {
        return TokenCause::EmptyPool;
    }
    let Some(dir) = pool_dir else {
        return TokenCause::Exhausted;
    };
    let Ok(text) = std::fs::read_to_string(dir.join(".ranking")) else {
        return TokenCause::Exhausted;
    };
    let mut any = false;
    let mut all_auth_dead = true;
    for line in text.lines() {
        let mut parts = line.split('|');
        let (Some(name), Some(status)) = (parts.next(), parts.next()) else {
            continue;
        };
        any = true;
        let auth_dead = status == "blocked" && bad_tokens::latest_entry_in_dir(dir, name).is_none();
        all_auth_dead &= auth_dead;
    }
    if any && all_auth_dead {
        TokenCause::AuthDead
    } else {
        TokenCause::Exhausted
    }
}
