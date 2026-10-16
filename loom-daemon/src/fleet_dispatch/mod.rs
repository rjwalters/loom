//! Fleet dispatch, Phase 1 (#12447): the captain is the claim arbiter.
//!
//! A worker in fleet mode asks the captain before its forge pre-claim
//! ("may I claim this item I drew?"). A refusal is a free pass with zero
//! forge calls; a grant runs today's lease path unchanged, and the forge
//! lease stays the only ownership record. The design is
//! `docs/design/fleet-captain-dispatch.md`.
//!
//! - [`proto`]: the wire contract, shared by the server and the client.
//! - [`client`]: the worker's HTTP client and its `loom.fleet_dispatch.call`
//!   span (#12450).
//! - [`fallback`]: the worker's fail-open hysteresis, pure (#12450).
//! - [`worker`]: the worker's claim, refuse, release and heartbeat, behind
//!   the per-host `fleetDispatch.enabled` flag (#12450).
//! - [`table`]: the soft assignment table and its policy, pure, on an
//!   injected monotonic clock.
//! - [`identity`]: remote address → tailnet node (LocalAPI `whois`).
//! - [`roster`]: tailnet node → fleet `host_id`, and each host's cap, from the
//!   fleet store's `fleet.json`.
//! - [`server`]: `loom-daemon fleet-dispatch serve`.
//! - [`telemetry`]: the `loom.fleet_dispatch.call` span and the captain's
//!   counters.

pub mod client;
pub mod fallback;
pub mod identity;
pub mod proto;
pub mod roster;
pub mod server;
pub mod table;
pub mod telemetry;
#[cfg(test)]
pub(crate) mod test_server;
pub mod worker;

/// A fresh server epoch: changes on every start, so a worker can tell its
/// table entries were rebuilt.
#[must_use]
pub fn new_epoch() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
