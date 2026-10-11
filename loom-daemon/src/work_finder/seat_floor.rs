//! The Z.ai free-seat floor for the work finder's concurrency cap (#11284).
//!
//! # Why a floor
//!
//! Z.ai GLM coding-plan seats are flat-rate with a weekly cap: an idle seat is
//! paid capacity earning nothing. But the operator ceiling
//! (`autonomous.workFinder.maxConcurrent`) is sized for the *metered* pools a
//! host also serves, so on a host whose coding phases route GLM-first
//! (#11412), the ceiling — not the seats, and not disk/RAM — can be what
//! leaves seats idle while backlog waits. The fix is not to make every host
//! raise `maxConcurrent` (that un-bounds the metered pools too); it is to let
//! *free seats themselves* raise the ceiling, per tick:
//!
//! ```text
//! dynamic cap = min(disk headroom, ram headroom,
//!                   max(configured_max, live occupancy + free zai seats))
//! ```
//!
//! The floor is sized **over live occupancy** (second review): the ceiling it
//! justifies must leave room for every free seat *on top of* the sweeps
//! already running — seat-holding and metered alike — so N free seats admit
//! N more launches (`cap − occupancy == free`) instead of free seats
//! competing with running launches for the same N slots.
//!
//! Disk and RAM stay safety gates (#5270) — a seat never buys a sweep the
//! host cannot hold. Drain, the host breaker, the saturation brake, the
//! per-repo cap and every pre-dispatch hold are untouched: the floor only
//! lifts the one term that was leaving seats idle.
//!
//! # What counts as a free seat
//!
//! The actual available slots, not the configured accounts: for every
//! **selectable** Z.ai-pool account that *declares* a concurrency cap
//! (`loom-daemon api-keys limit … --max-concurrent N`, #8424), a seat is
//! `N − live in-flight leases` ([`super::super::api_keys_pool::inflight`]) —
//! the same reservation store the spawn-time selection ladder enforces, so
//! admission and selection cannot disagree about what is free. Accounts that
//! are disabled, malformed, bad-marked or unverifiable are excluded by
//! `selectable()`; an account at its cap contributes zero; an account with
//! **no declared cap** contributes zero too (unbounded is not a number this
//! floor may invent — raising the ceiling past what an operator declared
//! elsewhere is the breach the conservative direction exists to avoid).
//!
//! Over-admission is impossible by construction and self-correcting when it
//! races: a seat taken between this tick's count and the phase's launch makes
//! the GLM tap unavailable at selection, and the preference walk falls
//! through to claude exactly as it does for any exhausted pool — the designed
//! degradation, not a new failure.
//!
//! # Failure direction
//!
//! An unreadable pool root contributes nothing and records why in
//! [`SeatFloor::note`] (paths only, never credentials): a floor that cannot
//! be computed is a floor of zero, i.e. today's cap, never a guess upward.
//!
//! # Which providers are "Z.ai"
//!
//! The pool namespace is a directory name, and the fleet has spelled the
//! subscription both `zai` and `zai-coding-plan` (OpenCode's own provider id
//! for it) — the same alias set
//! `observability::ops::quota::opencode::pool_provider` folds together, kept
//! in sync by test.

use std::path::Path;

use crate::api_keys_pool::inflight;
use crate::api_keys_pool::paths::{list_providers_in, pool_roots, resolve_provider_root_in};
use crate::api_keys_pool::registry::list_provider;

/// Pool namespaces that are Z.ai subscriptions — the exact key set
/// `quota::opencode::pool_provider` normalizes to `"zai"`.
const ZAI_POOL_NAMES: [&str; 4] = ["zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan"];

/// Whether a pool namespace names a Z.ai subscription.
#[must_use]
pub fn is_zai_pool(provider: &str) -> bool {
    ZAI_POOL_NAMES.contains(&provider)
}

/// One tick's free-seat observation over the workspace's effective Z.ai pool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatFloor {
    /// Sum over selectable capped accounts of `cap − live leases`. The value
    /// the cap floor uses.
    pub free: usize,
    /// How many selectable accounts with a declared cap were counted.
    pub accounts: usize,
    /// Sum of those accounts' declared caps.
    pub capacity: usize,
    /// Live in-flight leases held against those caps.
    pub reserved: usize,
    /// Why part of the pool could not be counted, when it could not
    /// (a path, never a credential).
    pub note: Option<String>,
}

impl SeatFloor {
    /// Read the workspace's effective Z.ai pool and count free seats.
    ///
    /// Never fails: an unusable pool yields an all-zero floor plus a
    /// [`note`](Self::note), which leaves the configured cap in force.
    #[must_use]
    pub fn observe(workspace: &Path) -> Self {
        let mut floor = Self::default();
        let roots = pool_roots(workspace);
        let providers = match list_providers_in(&roots) {
            Ok(providers) => providers,
            Err(e) => {
                floor.note = Some(format!("api-keys pool unreadable: {e}"));
                return floor;
            }
        };
        for provider in providers.into_iter().filter(|p| is_zai_pool(p)) {
            let root = match resolve_provider_root_in(&roots, &provider) {
                Ok(root) => root,
                Err(e) => {
                    floor.note = Some(format!("pool {provider} unreadable: {e}"));
                    continue;
                }
            };
            let accounts = match list_provider(&root, &provider) {
                Ok(accounts) => accounts,
                Err(e) => {
                    floor.note = Some(format!("pool {provider} unreadable: {e}"));
                    continue;
                }
            };
            for account in accounts.iter().filter(|a| a.selectable()) {
                let Some(cap) = account.max_concurrent else {
                    continue;
                };
                let cap = cap as usize;
                let live = inflight::live_count(&root, &provider, &account.name) as usize;
                floor.accounts += 1;
                floor.capacity += cap;
                floor.reserved += live;
                floor.free += cap.saturating_sub(live);
            }
        }
        floor
    }

    /// The total-occupancy ceiling the seats justify over the sweeps already
    /// running (#11284, second review): `occupancy + free`, or `0` when there
    /// are no free seats (a floor of zero is no floor — it must not pin the
    /// cap to current occupancy either). Sizing the ceiling this way is what
    /// makes `cap − occupancy == free`: every free seat is an admission.
    #[must_use]
    pub fn ceiling_over(&self, occupancy: usize) -> usize {
        if self.free == 0 {
            0
        } else {
            occupancy + self.free
        }
    }

    /// The configured ceiling with this floor applied:
    /// `max(configured, occupancy + free)`. A floor of zero returns
    /// `configured` unchanged.
    #[must_use]
    pub fn raise(&self, configured: usize, occupancy: usize) -> usize {
        configured.max(self.ceiling_over(occupancy))
    }
}

impl From<SeatFloor> for crate::types::ZaiSeats {
    fn from(floor: SeatFloor) -> Self {
        Self {
            free: floor.free,
            accounts: floor.accounts,
            capacity: floor.capacity,
            reserved: floor.reserved,
        }
    }
}

/// [`SeatFloor::observe`] + [`SeatFloor::raise`] in one step for the
/// advisory surfaces (#11284): the configured ceiling with the observed
/// floor applied over `occupancy`.
#[must_use]
pub fn seated_raise(workspace: &Path, configured: usize, occupancy: usize) -> usize {
    SeatFloor::observe(workspace).raise(configured, occupancy)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::api_keys_pool::limits;
    use crate::api_keys_pool::registry;

    /// The pool root [`SeatFloor::observe`] resolves for a workspace root —
    /// `<workspace>/.loom/api-keys` — which is what the registry helpers
    /// take as *their* root (`<root>/<provider>`).
    fn pool_root(ws: &tempfile::TempDir) -> std::path::PathBuf {
        ws.path().join(".loom").join("api-keys")
    }

    fn seeded_pool() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = pool_root(&tmp);
        registry::add(&root, "zai", "alpha", "ZAI_API_KEY", "k1", false).unwrap();
        registry::add(&root, "zai", "beta", "ZAI_API_KEY", "k2", false).unwrap();
        tmp
    }

    #[test]
    fn the_alias_set_matches_the_quota_normalizer() {
        for name in ZAI_POOL_NAMES {
            assert_eq!(
                crate::observability::ops::quota::opencode::pool_provider(name),
                "zai",
                "{name} should normalize to the zai pool"
            );
        }
        assert!(is_zai_pool("zai-coding-plan"));
        assert!(!is_zai_pool("kimi"));
        assert!(!is_zai_pool("openai"));
    }

    #[test]
    fn an_empty_or_absent_pool_yields_a_zero_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor, SeatFloor::default());
        assert_eq!(floor.raise(3, 0), 3);
    }

    #[test]
    fn free_seats_are_declared_capacity_minus_live_reservations() {
        let tmp = seeded_pool();
        let root = pool_root(&tmp);
        limits::set_max_concurrent(&root, "zai", "alpha", Some(3)).unwrap();
        limits::set_max_concurrent(&root, "zai", "beta", Some(2)).unwrap();
        let hold = inflight::register_for(&root, "zai", "alpha", std::process::id()).unwrap();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor.accounts, 2);
        assert_eq!(floor.capacity, 5);
        assert_eq!(floor.reserved, 1);
        assert_eq!(floor.free, 4);
        assert_eq!(floor.raise(2, 0), 4);
        assert_eq!(floor.raise(9, 0), 9);
        hold.release();
        assert_eq!(SeatFloor::observe(tmp.path()).free, 5, "released seat frees");
    }

    /// The #11284 second-review scenario: configured 2, two launches already
    /// running (their leases hold two seats), four seats still free. The
    /// ceiling must admit all four free seats on top of the two running —
    /// `occupancy + free`, not `free` alone, which would strand two seats
    /// behind the two launches already inside the cap.
    #[test]
    fn the_floor_admits_every_free_seat_on_top_of_running_launches() {
        let floor = SeatFloor {
            free: 4,
            accounts: 2,
            capacity: 6,
            reserved: 2,
            note: None,
        };
        assert_eq!(floor.ceiling_over(2), 6);
        assert_eq!(floor.raise(2, 2), 6, "four free seats admit four more launches");
        assert_eq!(floor.raise(2, 0), 4, "from idle the floor is the free count");
        assert_eq!(floor.raise(9, 2), 9, "a configured ceiling above the seats stands");
        // Metered (non-seat) launches count as occupancy too: they add on top
        // of seat capacity rather than eating the seats' admissions.
        assert_eq!(floor.raise(2, 7), 11);
    }

    #[test]
    fn a_floor_of_zero_never_lifts_even_under_occupancy() {
        let floor = SeatFloor::default();
        assert_eq!(floor.ceiling_over(9), 0);
        assert_eq!(floor.raise(2, 9), 2);
    }

    #[test]
    fn an_account_without_a_declared_cap_contributes_no_floor() {
        let tmp = seeded_pool();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor, SeatFloor::default(), "unbounded is not a seat count");
    }

    #[test]
    fn an_account_at_its_cap_contributes_zero_and_exhausted_accounts_are_excluded() {
        let tmp = seeded_pool();
        let root = pool_root(&tmp);
        limits::set_max_concurrent(&root, "zai", "alpha", Some(1)).unwrap();
        limits::set_max_concurrent(&root, "zai", "beta", Some(2)).unwrap();
        let _hold = inflight::register_for(&root, "zai", "alpha", std::process::id()).unwrap();
        // `beta` bad-marked: selectable() must drop it entirely.
        crate::api_keys_pool::mark_bad(&root, "zai", "beta", "exhausted", Some(600)).unwrap();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor.accounts, 1);
        assert_eq!(floor.capacity, 1);
        assert_eq!(floor.reserved, 1);
        assert_eq!(floor.free, 0, "alpha is at its cap, beta is exhausted");
    }

    #[test]
    fn a_disabled_account_is_excluded() {
        let tmp = seeded_pool();
        let root = pool_root(&tmp);
        limits::set_max_concurrent(&root, "zai", "alpha", Some(2)).unwrap();
        limits::set_max_concurrent(&root, "zai", "beta", Some(2)).unwrap();
        registry::set_enabled(&root, "zai", "alpha", false).unwrap();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor.accounts, 1);
        assert_eq!(floor.free, 2);
    }

    #[test]
    fn the_alternate_zai_namespace_and_foreign_providers_are_partitioned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = pool_root(&tmp);
        registry::add(&root, "zai-coding-plan", "seat", "ZAI_API_KEY", "k", false).unwrap();
        registry::add(&root, "kimi", "other", "KIMI_API_KEY", "k", false).unwrap();
        limits::set_max_concurrent(&root, "zai-coding-plan", "seat", Some(4)).unwrap();
        limits::set_max_concurrent(&root, "kimi", "other", Some(9)).unwrap();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor.capacity, 4, "only the zai namespace counts");
    }

    #[test]
    fn an_unreadable_provider_root_contributes_nothing_and_says_why() {
        let tmp = seeded_pool();
        let root = pool_root(&tmp);
        limits::set_max_concurrent(&root, "zai", "alpha", Some(2)).unwrap();
        // A file where the provider directory belongs makes every read of it
        // fail — the floor must degrade to the countable accounts.
        let dir = crate::api_keys_pool::paths::provider_dir(&root, "zai");
        let blocked = tempfile::tempdir().unwrap();
        std::fs::rename(&dir, blocked.path()).unwrap();
        std::fs::write(&dir, "not a directory").unwrap();
        let floor = SeatFloor::observe(tmp.path());
        assert_eq!(floor.free, 0);
        assert!(floor.note.is_some(), "the unreadable pool is named: {floor:?}");
    }
}
