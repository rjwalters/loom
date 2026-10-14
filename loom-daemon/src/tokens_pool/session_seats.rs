//! The host's session-managed Codex seats across **every** registered root
//! (issue #10600, Epic #10452).
//!
//! Container names are host-global (`loom-codex-session-<account>`), so a
//! seat is one account name, whichever root lists it. Before #10600 the
//! session watch read only the daemon's own root: on a host whose daemon root
//! had no session-managed account but another registered root did, nothing
//! was published and that root's liveness-aware selection was silently blind.
//!
//! The rule is the reconciler's (`session_reconcile::AccountIndex` and
//! `run_tick`), so the watch, `loom-daemon status` and the reconciler agree on
//! which accounts are seats:
//!
//! * a Codex account that is `enabled` and session-managed
//!   ([`is_session_managed`]) in at least one root is a seat;
//! * `enabled=false` in **any** root takes it out everywhere;
//! * its profile directories in every root are kept, because an operator
//!   hold or a drift-removal record in any of them applies to the account.
//!
//! Filesystem only; never calls docker. A root whose inventory cannot be
//! read contributes nothing.

use std::path::{Path, PathBuf};

use super::account_registry::{account_inventory_quiet, AccountDescriptor, AccountProvider};
use super::session_lifecycle::{container_name, is_session_managed};

/// One session-managed Codex account on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seat {
    pub account: String,
    pub container: String,
    /// Its profile directory in every root that lists it (holds and removal
    /// records are read from all of them).
    pub profiles: Vec<PathBuf>,
}

/// The seats in `inventories` (one per registered root), in first-seen order.
/// `session_managed` is [`is_session_managed`] in production.
#[must_use]
pub fn seats_from(
    inventories: &[&[AccountDescriptor]],
    session_managed: &dyn Fn(&Path) -> bool,
) -> Vec<Seat> {
    let codex = || {
        inventories
            .iter()
            .flat_map(|inventory| inventory.iter())
            .filter(|a| a.id.provider == AccountProvider::Codex)
    };
    let disabled = |name: &str| codex().any(|a| a.id.name == name && !a.enabled);
    let mut seats: Vec<Seat> = Vec::new();
    for account in codex() {
        let name = &account.id.name;
        if !account.enabled || disabled(name) || !session_managed(&account.credential_reference) {
            continue;
        }
        if seats.iter().any(|seat| &seat.account == name) {
            continue;
        }
        let mut profiles: Vec<PathBuf> = Vec::new();
        for profile in codex()
            .filter(|a| &a.id.name == name)
            .map(|a| &a.credential_reference)
        {
            if !profiles.contains(profile) {
                profiles.push(profile.clone());
            }
        }
        seats.push(Seat {
            account: name.clone(),
            container: container_name(name),
            profiles,
        });
    }
    seats
}

/// Every seat across `fallback_root`'s effective registered roots (the
/// registry's roots, or `fallback_root` alone when none are registered or the
/// registry cannot be read).
#[must_use]
pub fn seats(fallback_root: &Path) -> Vec<Seat> {
    let registry = crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let inventories: Vec<Vec<AccountDescriptor>> = registry
        .effective_roots(fallback_root)
        .iter()
        .filter_map(|root| account_inventory_quiet(root, AccountProvider::Codex).ok())
        .collect();
    let slices: Vec<&[AccountDescriptor]> = inventories.iter().map(Vec::as_slice).collect();
    seats_from(&slices, &is_session_managed)
}

#[cfg(test)]
#[path = "session_seats_tests.rs"]
mod tests;
