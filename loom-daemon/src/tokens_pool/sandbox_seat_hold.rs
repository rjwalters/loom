//! A **per-seat** hold for a Codex account whose sandbox just refused every
//! tool call (#14455).
//!
//! `runtime_preference::sandbox_hold` holds the *runtime* host-wide, in daemon
//! memory, for a role-runner tick. That is the wrong grain for a pool where one
//! seat is a sealed session container and another is a bare-metal profile on a
//! host whose kernel refuses user namespaces: the first runs commands, the
//! second never will. Holding the whole runtime idles the good seat; holding
//! nothing re-draws the bad one on every step.
//!
//! The hold is written to the account's own record in `account-health.json`
//! as a short account-wide cooldown, because the readers that must honour it
//! are not all in the daemon: `tokens select` (run by `spawn-codex.sh`) draws
//! the seat in a child process, and an in-memory table would not reach it.
//! The existing selection and pool-state reads already skip a seat in
//! cooldown, so the hold needs no reader of its own.
//!
//! It reuses [`HealthReason::TransientFailure`] with this module's
//! provenance, adding no `HealthReason` variant: a variant would make every
//! older daemon on a shared `account-health.json` refuse to parse the file.
//! It never touches `consecutive_transient_failures`, never overrides a sticky
//! `ReauthRequired`, and never shortens a longer live cooldown.
//!
//! It ends when it ages out (the same window as the runtime hold,
//! `LOOM_RUNTIME_SANDBOX_HOLD_SECS`; `0` disables), when a tick on the seat
//! succeeds (the `SUCCESS` terminal arm clears the cooldown), or when an
//! operator clears the account's health. The next draw after it ages out
//! re-tests the seat and re-arms the hold if it is still broken.

use std::path::Path;

use anyhow::{bail, Result};

use super::account_registry::AccountId;
use super::health::{with_state, AccountHealth, HealthReason};

/// The `signal_provenance` a seat hold carries, so `accounts status` and a
/// grep of `account-health.json` can tell it from an ordinary backoff.
pub const PROVENANCE: &str = "sandbox-unavailable:v1";

/// Hold `id` for `secs` from `now`. Returns whether a hold was written:
/// `false` for `secs == 0` (disabled), a sticky re-auth hold, or a live
/// cooldown that already lasts longer.
///
/// # Errors
/// An empty account name, or the health file cannot be locked, read or
/// written.
pub fn hold_seat(workspace: &Path, id: &AccountId, secs: u64, now: u64) -> Result<bool> {
    if id.name.is_empty() {
        bail!("account identity is required");
    }
    if secs == 0 {
        return Ok(false);
    }
    let until = now.saturating_add(secs);
    with_state(workspace, |state| {
        let existing = state.accounts.iter().position(|entry| entry.id() == *id);
        let mut entry = existing.map_or_else(
            || AccountHealth {
                provider: id.provider,
                name: id.name.clone(),
                reason: HealthReason::Healthy,
                updated_at: now,
                signal_provenance: PROVENANCE.to_string(),
                cooldown_until: None,
                consecutive_transient_failures: 0,
                last_success: None,
                last_probe: None,
                class_cooldowns: std::collections::HashMap::new(),
            },
            |index| state.accounts.remove(index),
        );
        let wins = entry.reason != HealthReason::ReauthRequired
            && !entry.cooldown_until.is_some_and(|live| live >= until);
        if wins {
            entry.reason = HealthReason::TransientFailure;
            entry.cooldown_until = Some(until);
            entry.updated_at = now;
            entry.signal_provenance = PROVENANCE.to_string();
        }
        state.accounts.push(entry);
        state
            .accounts
            .sort_by(|a, b| (a.provider as u8, &a.name).cmp(&(b.provider as u8, &b.name)));
        Ok(wins)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tokens_pool::account_registry::AccountProvider;
    use crate::tokens_pool::health::{account_health, record_terminal_for_model};
    use crate::tokens_pool::TerminalClassification;

    fn id(name: &str) -> AccountId {
        AccountId {
            provider: AccountProvider::Codex,
            name: name.to_string(),
        }
    }

    #[test]
    fn a_held_seat_is_ineligible_until_the_window_ends() {
        let dir = tempfile::tempdir().unwrap();
        assert!(hold_seat(dir.path(), &id("agent-3"), 600, 1_000).unwrap());
        let health = account_health(dir.path(), &id("agent-3")).unwrap().unwrap();
        assert_eq!(health.signal_provenance, PROVENANCE);
        assert_eq!(health.consecutive_transient_failures, 0);
        assert!(!health.is_eligible_at(1_599));
        assert!(health.is_eligible_at(1_600), "ages out at `until`");
        assert_eq!(account_health(dir.path(), &id("agent-4")).unwrap(), None, "per seat");
    }

    #[test]
    fn a_disabled_hold_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!hold_seat(dir.path(), &id("agent-3"), 0, 1_000).unwrap());
        assert_eq!(account_health(dir.path(), &id("agent-3")).unwrap(), None);
    }

    #[test]
    fn a_longer_live_cooldown_and_a_sticky_reauth_hold_are_never_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap();
        record_terminal_for_model(
            dir.path(),
            &id("agent-3"),
            TerminalClassification::TokenExhausted,
            None,
            "test",
        )
        .unwrap();
        let before = account_health(dir.path(), &id("agent-3")).unwrap().unwrap();
        assert!(!hold_seat(dir.path(), &id("agent-3"), 600, now).unwrap());
        assert_eq!(account_health(dir.path(), &id("agent-3")).unwrap().unwrap(), before);

        record_terminal_for_model(
            dir.path(),
            &id("agent-4"),
            TerminalClassification::TokenExpired,
            None,
            "test",
        )
        .unwrap();
        assert!(!hold_seat(dir.path(), &id("agent-4"), 600, now).unwrap());
        let reauth = account_health(dir.path(), &id("agent-4")).unwrap().unwrap();
        assert_eq!(reauth.reason, HealthReason::ReauthRequired);
    }

    #[test]
    fn a_success_on_the_seat_clears_the_hold() {
        let dir = tempfile::tempdir().unwrap();
        hold_seat(dir.path(), &id("agent-3"), 3_600, 1_000).unwrap();
        record_terminal_for_model(
            dir.path(),
            &id("agent-3"),
            TerminalClassification::Success,
            None,
            "test",
        )
        .unwrap();
        let health = account_health(dir.path(), &id("agent-3")).unwrap().unwrap();
        assert!(health.is_eligible_at(1_001));
    }
}
