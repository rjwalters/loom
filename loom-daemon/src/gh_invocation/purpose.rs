//! What a forge call was **for** (#12310): `loom.forge.purpose`.
//!
//! [`super::AccessIntent`] says whether a call reads or writes, and
//! [`ReadClass`] how a read may be shed. Neither says whether a read was
//! *discovery* — a listing the daemon makes to find work — which is the number
//! epic #12304 drives to zero. [`ForgePurpose`] is that tag, stamped on every
//! `invoke github` span and every `loom.forge.calls` series, and sent to the
//! read gateway as a request header so it can refuse discovery first when a
//! pool runs low.
//!
//! Derivation ([`super::GhInvocation::purpose`]): a write is `write`; a read
//! is `discovery` only when its site said so explicitly — the
//! [`super::GhInvocation::discovery`] builder, or a [`discovery`] scope
//! around a listing whose `gh` calls are made several helpers down — else its
//! [`ReadClass`] name (`gate` by default, `hygiene`, `observability`).
//!
//! The scope is thread-local like [`super::caller_scope`]: the facade builds
//! each invocation on its caller's thread, and [`super::GhInvocation::new`]
//! reads the scope there.

use crate::forge_identity::ReadClass;
use std::cell::Cell;
use std::marker::PhantomData;

/// The request header a `discovery` read sends on its `gh api` call.
pub const HEADER_NAME: &str = "loom.forge.purpose";

/// The header's value on a `discovery` read; no other purpose sends it.
pub const DISCOVERY_VALUE: &str = "discovery";

/// What a forge call was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgePurpose {
    /// A listing made to find work (the figure epic #12304 counts).
    Discovery,
    /// A read that gates a decision or a write.
    Gate,
    /// A mutation.
    Write,
    /// A deferrable housekeeping read that is not a listing.
    Hygiene,
    /// A deferrable telemetry read that is not a listing.
    Observability,
}

impl ForgePurpose {
    /// The `loom.forge.purpose` attribute / `purpose` label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Gate => "gate",
            Self::Write => "write",
            Self::Hygiene => "hygiene",
            Self::Observability => "observability",
        }
    }

    /// The purpose of a read of `class` that no site marked as discovery.
    #[must_use]
    pub fn of_read(class: ReadClass) -> Self {
        match class {
            ReadClass::Gate => Self::Gate,
            ReadClass::Hygiene => Self::Hygiene,
            ReadClass::Observability => Self::Observability,
        }
    }
}

/// The label value for a row whose purpose is not known (an agent `gh` front
/// row, which does not carry one).
pub const UNKNOWN: &str = "unknown";

/// The token a read gateway puts in the message of the `429` it answers a
/// `discovery` read with when the pool's remaining quota is below the
/// reserve held for gate reads and the mirror (#12310). `gh` prints the
/// message on stderr, which is where the daemon looks for it. The gateway is
/// the fleet operator's; this is the contract it keeps.
pub const RESERVE_MARKER: &str = "loom-gateway-reserve";

/// The `loom.discovery.fallback_reason` of a read the gateway refused for the
/// reserve.
pub const FALLBACK_RESERVE: &str = "reserve";

/// Whether `stderr` is a gateway reserve refusal rather than a real rate
/// limit: it must not trip the rate-limit breaker, because claims and writes
/// carry on ([`crate::rate_limit_breaker::indicates_rate_limit`] is false for
/// it) and it is counted as its own outcome.
#[must_use]
pub fn is_reserve_refusal(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains(RESERVE_MARKER)
}

/// How long a reserve refusal ends this thread's discovery reads: a work
/// tick's discovery is over once the gateway says the pool is low.
const ENDED_FOR: std::time::Duration = std::time::Duration::from_secs(30);

thread_local! {
    static ENDED_UNTIL: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
    static DISCOVERY_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// An active discovery scope; dropping it ends the scope. Not `Send`.
#[derive(Debug)]
#[must_use = "the scope ends when this guard is dropped"]
pub struct DiscoveryScope {
    _thread: PhantomData<*const ()>,
}

/// Mark every read **built** on this thread until the guard drops as
/// `discovery`. For a listing site whose `gh` calls are made inside shared
/// helpers (the ETag store, the mirror fallback); keep it as narrow as the
/// listing itself. Writes inside it stay `write`.
pub fn discovery() -> DiscoveryScope {
    DISCOVERY_DEPTH.with(|d| d.set(d.get() + 1));
    DiscoveryScope {
        _thread: PhantomData,
    }
}

impl Drop for DiscoveryScope {
    fn drop(&mut self) {
        DISCOVERY_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// A discovery read was refused for the reserve: end this thread's discovery
/// for the rest of the tick (a few more refusals would only spend the pool).
pub fn end_discovery() {
    ENDED_UNTIL.with(|e| e.set(Some(std::time::Instant::now() + ENDED_FOR)));
}

/// Whether a reserve refusal has ended this thread's discovery reads.
#[must_use]
pub fn discovery_ended() -> bool {
    ENDED_UNTIL
        .with(Cell::get)
        .is_some_and(|t| std::time::Instant::now() < t)
}

/// A new tick begins: discovery is open again.
pub fn reopen_discovery() {
    ENDED_UNTIL.with(|e| e.set(None));
}

/// Whether a discovery scope is active on this thread.
#[must_use]
pub fn in_discovery_scope() -> bool {
    DISCOVERY_DEPTH.with(|d| d.get() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_nests_and_ends() {
        assert!(!in_discovery_scope());
        {
            let _a = discovery();
            {
                let _b = discovery();
                assert!(in_discovery_scope());
            }
            assert!(in_discovery_scope());
        }
        assert!(!in_discovery_scope());
    }

    #[test]
    fn read_class_names_the_purpose() {
        assert_eq!(ForgePurpose::of_read(ReadClass::Gate).as_str(), "gate");
        assert_eq!(ForgePurpose::of_read(ReadClass::Hygiene).as_str(), "hygiene");
        assert_eq!(ForgePurpose::of_read(ReadClass::Observability).as_str(), "observability");
    }
}

#[cfg(test)]
#[path = "purpose_tests.rs"]
mod source_pins;
