//! Keeping a hostname-keyed host on fleet config across #11530 (Issue #11740).
//!
//! #11530 stopped resolving `host.id` from the hostname. A host that never
//! set `fleet.hostId` or `LOOM_HOST_ID`, but is registered in the store under
//! its hostname, then resolved to a generated `loom-host-…` id, which names
//! no store host: every pass failed to render its config tiers and the host
//! ran on whatever it had last rendered, with only a log WARN to show for it.
//!
//! Two things here:
//!
//! - **A one-time migration** ([`pass_host`]). When the id is a *generated*
//!   one the store does not know, and exactly one store host equals one of
//!   this machine's short hostnames
//!   ([`crate::host_identity::hostname_candidates`]), the persisted id is
//!   rewritten to that roster name
//!   ([`crate::host_identity::adopt_roster_name`]) and the pass reads it. The
//!   hostname is only matched against the roster, never used on its own, so
//!   the reason #11530 dropped it (launchd hostnames like
//!   `MacBook-Air.localdomain`) does not come back: such a name matches no
//!   store host and changes nothing. An explicit `fleet.hostId` or
//!   `LOOM_HOST_ID` is never second-guessed.
//! - **Loudness** ([`host_not_in_store`]). When the id still names no store
//!   host, the config pass records `hostNotInStore`, which `loom-daemon
//!   status` (text and JSON), the `fleet_identity` health section and an
//!   ERROR log all surface as [`NOT_IN_STORE`].

use std::sync::OnceLock;

use chrono::{DateTime, Utc};

use crate::fleet_store::fetch::{self, Policy, Snapshot, Transport};
use crate::fleet_store::{self as store, StoreLocation};
use crate::host_identity::GENERATED_PREFIX;

/// The alert text every surface uses for a host whose identity names no
/// store host.
pub const NOT_IN_STORE: &str = "fleet config not applied: host identity not in store";

/// Every host the store has a machine-tier overlay for: `fleet.json`'s
/// `config.hosts.<h>.defaults`, else the legacy `fleet/hosts/<h>/defaults.json`.
pub fn store_hosts(snapshot: &Snapshot) -> anyhow::Result<Vec<String>> {
    if let Some(doc) = store::compiled::from_snapshot(snapshot)? {
        return Ok(doc.host_names());
    }
    Ok(snapshot
        .files
        .keys()
        .filter_map(|p| {
            p.strip_prefix("fleet/hosts/")?
                .strip_suffix("/defaults.json")
        })
        .filter(|h| !h.is_empty() && !h.contains('/'))
        .map(str::to_string)
        .collect())
}

/// Whether the snapshot is readable and has no config for `host`. A snapshot
/// that cannot be read is not evidence either way, so it is `false`.
#[must_use]
pub fn host_not_in_store(snapshot: &Snapshot, host: &str) -> bool {
    store_hosts(snapshot).is_ok_and(|hosts| !hosts.iter().any(|h| h == host))
}

/// The one store host named by `candidates` (ASCII case-insensitive), in the
/// store's spelling; `None` when none or more than one is.
#[must_use]
pub fn match_roster(hosts: &[String], candidates: &[String]) -> Option<String> {
    let mut hits = hosts
        .iter()
        .filter(|h| candidates.iter().any(|c| c.eq_ignore_ascii_case(h)));
    let first = hits.next()?;
    hits.next().is_none().then(|| first.clone())
}

fn adopted_cell() -> &'static OnceLock<String> {
    static ADOPTED: OnceLock<String> = OnceLock::new();
    &ADOPTED
}

/// The roster name this process adopted, if it adopted one.
#[must_use]
pub fn adopted() -> Option<String> {
    adopted_cell().get().cloned()
}

/// The host id this pass reads the store as: `host`, unless the migration
/// above applies (or already applied in this process).
pub fn pass_host(
    transport: &dyn Transport,
    cache_dir: &std::path::Path,
    location: &StoreLocation,
    host: &str,
    now: DateTime<Utc>,
) -> String {
    if let Some(name) = adopted() {
        return name;
    }
    if !host.starts_with(GENERATED_PREFIX) {
        return host.to_string();
    }
    let Ok(loaded) = fetch::load(transport, cache_dir, location, Policy::AllowStale, now) else {
        return host.to_string();
    };
    let candidates = crate::host_identity::hostname_candidates();
    match decide(&loaded.snapshot, host, &candidates, crate::host_identity::adopt_roster_name) {
        Some(name) => adopted_cell().get_or_init(|| name).clone(),
        None => host.to_string(),
    }
}

/// The pure half of [`pass_host`]: the roster name to adopt, having adopted
/// it through `adopt`, or `None` to keep `host`.
pub(crate) fn decide(
    snapshot: &Snapshot,
    host: &str,
    candidates: &[String],
    adopt: impl FnOnce(&str) -> std::io::Result<bool>,
) -> Option<String> {
    let hosts = store_hosts(snapshot).ok()?;
    if hosts.iter().any(|h| h == host) {
        return None;
    }
    let Some(name) = match_roster(&hosts, candidates) else {
        log::error!(
            "fleet_sync: {NOT_IN_STORE} — `{host}` is a generated id and none of this machine's \
             hostnames {candidates:?} names exactly one store host. Set LOOM_HOST_ID (or \
             fleet.hostId) to this host's roster name and restart (Issue #11740)."
        );
        return None;
    };
    store::validate_host(&name).ok()?;
    match adopt(&name) {
        Ok(true) => {
            log::warn!(
                "fleet_sync: host identity `{host}` (generated, Issue #11530) is not in the store, \
                 but this machine's hostname matches store host `{name}` — adopted `{name}` as \
                 this host's persisted id. Telemetry already labelled `{host}` keeps that label \
                 until the daemon restarts. Set LOOM_HOST_ID or fleet.hostId to make it \
                 explicit (Issue #11740)."
            );
            Some(name)
        }
        Ok(false) => None,
        Err(e) => {
            log::warn!(
                "fleet_sync: could not adopt store host `{name}` as this host's id: {e} — \
                 {NOT_IN_STORE} (Issue #11740)"
            );
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/identity.rs"]
mod tests;
