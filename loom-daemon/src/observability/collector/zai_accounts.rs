//! Z.ai api-keys seat sampling for `tokens.snapshot` (Issue #11396).
//!
//! Split out of `collector.rs`, which is frozen by the file-size ratchet
//! (`scripts/check-file-size-budget.sh`); the collector keeps only the
//! [`sample_zai_accounts`] call.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::observability::ops::quota::zai;
use crate::telemetry::TokenAccountState;

/// The enabled `zai` api-keys seats as quota-poll inputs. A seat whose key
/// cannot be read is kept with `key: None` so it still gets an UNKNOWN row
/// (no request is attempted); the key never leaves this struct.
fn zai_seats(workspace_root: &Path) -> Vec<zai::Seat> {
    zai_seats_with(workspace_root, |root, provider, name| {
        crate::api_keys_pool::registry::read_credential(root, provider, name)
            .map(|credential| credential.value)
    })
}

/// [`zai_seats`] with an injectable credential reader (test seam for the
/// read-failure path, which a well-formed on-disk seat cannot reach).
fn zai_seats_with(
    workspace_root: &Path,
    read: impl Fn(&Path, &str, &str) -> Result<String, String>,
) -> Vec<zai::Seat> {
    use crate::api_keys_pool::{paths, select, Ineligible};
    let provider = zai::ZAI_PROVIDER;
    let roots = paths::pool_roots(workspace_root);
    let Ok(accounts) = select::list_accounts_in(&roots, Some(provider)) else {
        return Vec::new();
    };
    let Ok(provider_root) = paths::resolve_provider_root_in(&roots, provider) else {
        return Vec::new();
    };
    accounts
        .into_iter()
        .filter(|account| account.enabled)
        .map(|account| {
            // A malformed/unreadable entry is kept (UNKNOWN row), never polled.
            let key = (account.ineligible != Some(Ineligible::Malformed))
                .then(|| read(&provider_root, provider, &account.name).ok())
                .flatten();
            zai::Seat {
                marked_exhausted: account.ineligible == Some(Ineligible::Exhausted),
                mark_resets_at: account
                    .resets_at
                    .and_then(|at| DateTime::<Utc>::from_timestamp(i64::try_from(at).ok()?, 0)),
                name: account.name,
                key,
            }
        })
        .collect()
}

/// `zai` seat rows for `tokens.snapshot` (Issue #11396).
pub(super) async fn sample_zai_accounts(workspace_root: &Path) -> Vec<TokenAccountState> {
    let root = workspace_root.to_path_buf();
    let Ok(seats) = tokio::task::spawn_blocking(move || zai_seats(&root)).await else {
        return Vec::new();
    };
    zai::sample(seats).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
