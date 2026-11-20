//! The captain's fleet-wide issue-filing slot (#13918).
//!
//! One in-memory slot, `{host, holder, expires}`, on the server's injected
//! monotonic clock ([`Mono`]). It is the captain-hosted form of the filing
//! lock (#6714): two hosts filing at once serialise through it, where the
//! claims-room broadcast could only back off. Pure, like [`super::table`].
//!
//! - **Holder-scoped.** The caller names a `holder` id of its own making. An
//!   unlock frees the slot only for the same host *and* the same holder, so a
//!   stale or foreign unlock never frees someone else's hold.
//! - **TTL.** A holder that never unlocks loses the slot at [`FILING_LOCK_TTL`],
//!   so a crashed filer cannot wedge fleet-wide filing. Re-requesting with the
//!   same host and holder refreshes the TTL (a heartbeat for a long burst).
//! - **Soft state.** Nothing is persisted. A captain restart empties the slot,
//!   and the holder's next refresh is simply granted again.

use std::time::Duration;

use super::table::Mono;

/// How long a grant lives without a refresh: a lost unlock costs at most a
/// minute of extra queueing, never a wedge. A constant, not a knob.
pub const FILING_LOCK_TTL: Duration = Duration::from_secs(60);

/// The longest `retry_after_s` the slot suggests. A burst is a handful of API
/// calls, so a waiter asks again soon rather than after the whole TTL.
pub const MAX_RETRY_AFTER_S: u64 = 2;

/// The current holder.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    host: String,
    holder: String,
    label: String,
    since: Mono,
    expires: Mono,
}

/// The answer to a lock request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockDecision {
    /// The caller holds the slot for `ttl_s` more seconds. `refreshed` is
    /// `true` when it already held it and this only extended the TTL.
    Granted { ttl_s: u64, refreshed: bool },
    /// Another holder has it.
    Held { held_by: String, retry_after_s: u64 },
}

/// The answer to an unlock request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnlockOutcome {
    /// The caller held the slot and it is free now, after `hold`.
    Released { hold: Duration },
    /// Someone else holds the slot (another host, or a different holder id);
    /// nothing changed.
    NotHolder,
    /// Nobody held the slot (expired, or the captain restarted).
    Empty,
}

/// A hold that ran out its TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredFiling {
    pub host: String,
    pub holder: String,
    pub label: String,
    /// How long it was held, the TTL included.
    pub hold: Duration,
}

/// The slot.
#[derive(Debug, Clone, Default)]
pub struct FilingSlot {
    held: Option<Held>,
}

impl FilingSlot {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The host holding the slot, if any (not yet expired is the caller's
    /// business: call [`Self::expire`] first).
    #[must_use]
    pub fn holder_host(&self) -> Option<&str> {
        self.held.as_ref().map(|h| h.host.as_str())
    }

    /// Drop the hold if its TTL has run out at `now`; the hold dropped.
    pub fn expire(&mut self, now: Mono) -> Option<ExpiredFiling> {
        let due = self.held.as_ref().is_some_and(|h| h.expires <= now);
        if !due {
            return None;
        }
        self.held.take().map(|h| ExpiredFiling {
            hold: h.expires.0.saturating_sub(h.since.0),
            host: h.host,
            holder: h.holder,
            label: h.label,
        })
    }

    /// `host` asks for the slot as `holder`. The caller expires first.
    pub fn lock(&mut self, now: Mono, host: &str, holder: &str, label: &str) -> LockDecision {
        let ttl_s = FILING_LOCK_TTL.as_secs();
        match &mut self.held {
            Some(h) if h.host == host && h.holder == holder => {
                h.expires = now.plus(FILING_LOCK_TTL);
                LockDecision::Granted {
                    ttl_s,
                    refreshed: true,
                }
            }
            Some(h) => LockDecision::Held {
                held_by: h.host.clone(),
                retry_after_s: now.secs_until(h.expires, 1).min(MAX_RETRY_AFTER_S),
            },
            None => {
                self.held = Some(Held {
                    host: host.to_string(),
                    holder: holder.to_string(),
                    label: label.to_string(),
                    since: now,
                    expires: now.plus(FILING_LOCK_TTL),
                });
                LockDecision::Granted {
                    ttl_s,
                    refreshed: false,
                }
            }
        }
    }

    /// `host` releases the slot it holds as `holder`.
    pub fn unlock(&mut self, now: Mono, host: &str, holder: &str) -> UnlockOutcome {
        match &self.held {
            None => UnlockOutcome::Empty,
            Some(h) if h.host == host && h.holder == holder => {
                let hold = now.0.saturating_sub(h.since.0);
                self.held = None;
                UnlockOutcome::Released { hold }
            }
            Some(_) => UnlockOutcome::NotHolder,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Mono {
        Mono(Duration::from_secs(secs))
    }

    #[test]
    fn one_host_is_granted_and_the_other_is_held_until_it_unlocks() {
        let mut s = FilingSlot::new();
        assert_eq!(
            s.lock(at(10), "a", "h-1", "architect"),
            LockDecision::Granted {
                ttl_s: 60,
                refreshed: false
            }
        );
        assert_eq!(
            s.lock(at(11), "b", "h-2", "auditor"),
            LockDecision::Held {
                held_by: "a".into(),
                retry_after_s: MAX_RETRY_AFTER_S
            }
        );
        assert_eq!(
            s.unlock(at(12), "a", "h-1"),
            UnlockOutcome::Released {
                hold: Duration::from_secs(2)
            }
        );
        assert!(matches!(s.lock(at(12), "b", "h-2", "auditor"), LockDecision::Granted { .. }));
        assert_eq!(s.holder_host(), Some("b"));
    }

    #[test]
    fn an_unlock_from_a_non_holder_frees_nothing() {
        let mut s = FilingSlot::new();
        s.lock(at(0), "a", "h-1", "x");
        assert_eq!(s.unlock(at(1), "b", "h-1"), UnlockOutcome::NotHolder, "other host");
        assert_eq!(s.unlock(at(1), "a", "stale"), UnlockOutcome::NotHolder, "stale holder id");
        assert_eq!(s.holder_host(), Some("a"));
        assert_eq!(
            s.unlock(at(2), "a", "h-1"),
            UnlockOutcome::Released {
                hold: Duration::from_secs(2)
            }
        );
        assert_eq!(s.unlock(at(3), "a", "h-1"), UnlockOutcome::Empty, "a second unlock is a no-op");
    }

    #[test]
    fn a_holder_that_never_unlocks_loses_the_slot_at_its_ttl() {
        let mut s = FilingSlot::new();
        s.lock(at(0), "a", "h-1", "crashed");
        assert_eq!(s.expire(at(59)), None);
        let gone = s.expire(at(60)).unwrap();
        assert_eq!((gone.host.as_str(), gone.hold), ("a", Duration::from_secs(60)));
        assert_eq!(s.holder_host(), None);
        assert!(matches!(
            s.lock(at(60), "b", "h-2", "next"),
            LockDecision::Granted {
                refreshed: false,
                ..
            }
        ));
    }

    #[test]
    fn the_same_holder_refreshes_the_ttl_and_a_different_one_does_not() {
        let mut s = FilingSlot::new();
        s.lock(at(0), "a", "h-1", "burst");
        assert_eq!(
            s.lock(at(50), "a", "h-1", "burst"),
            LockDecision::Granted {
                ttl_s: 60,
                refreshed: true
            }
        );
        assert_eq!(s.expire(at(100)), None, "the refresh moved the deadline to 110");
        assert!(s.expire(at(110)).is_some());

        s.lock(at(200), "a", "h-1", "burst");
        // Same holder id from another host is a different holder.
        assert!(matches!(s.lock(at(201), "b", "h-1", "burst"), LockDecision::Held { .. }));
        assert_eq!(s.expire(at(259)), None, "a refused request never extends the hold");
    }

    #[test]
    fn retry_after_never_exceeds_the_remaining_ttl_and_is_at_least_one() {
        let mut s = FilingSlot::new();
        s.lock(at(0), "a", "h-1", "x");
        let retry = |s: &mut FilingSlot, now| match s.lock(at(now), "b", "h-2", "y") {
            LockDecision::Held { retry_after_s, .. } => retry_after_s,
            other => panic!("{other:?}"),
        };
        assert_eq!(retry(&mut s, 1), 2);
        assert_eq!(retry(&mut s, 59), 1);
    }

    #[test]
    fn a_restart_leaves_an_empty_slot_so_a_refresh_is_granted_again() {
        let mut fresh = FilingSlot::new();
        assert_eq!(fresh.unlock(at(5), "a", "h-1"), UnlockOutcome::Empty);
        assert_eq!(
            fresh.lock(at(5), "a", "h-1", "burst"),
            LockDecision::Granted {
                ttl_s: 60,
                refreshed: false
            }
        );
    }
}
