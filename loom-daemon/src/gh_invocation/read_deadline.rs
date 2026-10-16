//! A wall-clock deadline over the forge reads of one work-finder tick (#12074).
//!
//! # Why
//!
//! The work finder's tick body is synchronous between its awaits: the
//! pre-dispatch holds, the step gather (listings, stage-entry events, CI at a
//! Judge head) and the draw all run inline. Each forge read carries its own
//! timeout, but nothing bounded their sum. After a fleet-wide restart every
//! cache was cold, the reader pools backed off, and a gateway attempt that
//! timed out is rerun on the readers without being
//! withdrawn, so each read could cost a full timeout before it even started
//! again. The tick sat in that gather for many minutes and reported nothing.
//!
//! # What
//!
//! [`enter`] installs a deadline for the current thread (the tick body never
//! yields between the guard and its drop, so it stays on one thread). While it
//! is in force, every [`AccessIntent::Read`](super::AccessIntent::Read) the
//! `gh` facade runs:
//!
//! - is bounded by the time left: its own timeout is clamped to it;
//! - once the deadline has passed, is not sent at all: it completes as
//!   [`GhCompletion::Shed`](super::GhCompletion::Shed), "no answer", which
//!   every caller already treats as a failed read (a listing falls back to its
//!   last good copy, a fact keeps the value that passes the item).
//!
//! Writes are never touched. (Reads never use the writer at all since
//! #12394, so the scope no longer carries a writer rule.)
//!
//! [`suspend`] lifts the scope for a stretch that must run normally (the
//! draw's spawns re-read the forge before a claim).

use std::cell::Cell;
use std::time::{Duration, Instant};

/// The scope in force on this thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scope {
    deadline: Instant,
}

thread_local! {
    static SCOPE: Cell<Option<Scope>> = const { Cell::new(None) };
}

/// What the scope allows a read that starts now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// No scope: the read runs under its own timeout.
    Unbounded,
    /// At most this long.
    Left(Duration),
    /// The deadline has passed: do not send it.
    Passed,
}

/// Restores the previous scope when dropped. Deliberately `!Send`: the scope
/// is this thread's, so a guard held across an `.await` (where the task may
/// resume on another worker) is a compile error rather than a scope leaked
/// onto whatever task runs next on this thread.
#[must_use = "the scope ends when the guard is dropped"]
pub struct Guard {
    previous: Option<Scope>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        SCOPE.with(|s| s.set(self.previous));
    }
}

/// Bound this thread's forge reads by `deadline` until the guard drops.
pub fn enter(deadline: Instant) -> Guard {
    // A new tick: a reserve refusal in the last one does not carry over.
    super::purpose::reopen_discovery();
    let previous = SCOPE.with(|s| s.replace(Some(Scope { deadline })));
    Guard {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

/// Narrow or widen the scope in force (if any) until the guard drops.
fn adjust(f: impl FnOnce(Scope) -> Scope) -> Guard {
    let previous = SCOPE.with(Cell::get);
    if let Some(scope) = previous {
        SCOPE.with(|s| s.set(Some(f(scope))));
    }
    Guard {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

/// Give the reads until the guard drops a deadline no earlier than `floor`
/// (#12074): the one repo a tick always gathers gets a real read budget even
/// when the tick's own deadline has passed. A no-op with no scope.
pub fn at_least(floor: Instant) -> Guard {
    adjust(|s| Scope {
        deadline: s.deadline.max(floor),
    })
}

/// The scope in force on this thread, to carry onto a worker thread
/// ([`install`]): the step gather reads repos on several threads (#12256),
/// and each must keep the tick's deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot(Option<Scope>);

/// This thread's scope, for [`install`] on another thread.
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot(SCOPE.with(Cell::get))
}

/// Put `snapshot` in force on this thread until the guard drops.
pub fn install(snapshot: Snapshot) -> Guard {
    Guard {
        previous: SCOPE.with(|s| s.replace(snapshot.0)),
        _not_send: std::marker::PhantomData,
    }
}

/// Lift the scope until the guard drops.
pub fn suspend() -> Guard {
    Guard {
        previous: SCOPE.with(Cell::take),
        _not_send: std::marker::PhantomData,
    }
}

/// The budget a read starting at `now` has.
#[must_use]
pub fn budget_at(now: Instant) -> Budget {
    match SCOPE.with(Cell::get) {
        None => Budget::Unbounded,
        Some(s) => match s.deadline.checked_duration_since(now) {
            Some(left) if !left.is_zero() => Budget::Left(left),
            _ => Budget::Passed,
        },
    }
}

/// [`budget_at`] now.
#[must_use]
pub fn budget() -> Budget {
    budget_at(Instant::now())
}

/// Whether the deadline in force has passed (`false` with no scope).
#[must_use]
pub fn passed() -> bool {
    budget() == Budget::Passed
}

/// `timeout` clamped to the time left, or `None` when the deadline passed.
#[must_use]
pub fn clamp(timeout: Duration) -> Option<Duration> {
    match budget() {
        Budget::Unbounded => Some(timeout),
        Budget::Left(left) => Some(timeout.min(left)),
        Budget::Passed => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_scope_is_unbounded() {
        assert_eq!(budget(), Budget::Unbounded);
        assert_eq!(clamp(Duration::from_secs(30)), Some(Duration::from_secs(30)));
        assert!(!passed());
    }

    #[test]
    fn a_scope_clamps_then_passes_and_restores() {
        let now = Instant::now();
        {
            let _g = enter(now + Duration::from_secs(5));
            assert_eq!(
                budget_at(now + Duration::from_secs(1)),
                Budget::Left(Duration::from_secs(4))
            );
            assert_eq!(budget_at(now + Duration::from_secs(5)), Budget::Passed);
            let c = clamp(Duration::from_secs(60)).unwrap();
            assert!(c <= Duration::from_secs(5));
            {
                let _s = suspend();
                assert_eq!(budget(), Budget::Unbounded);
            }
            assert!(matches!(budget(), Budget::Left(_)), "suspend restores the scope");
        }
        assert_eq!(budget(), Budget::Unbounded, "the guard restores no scope");
    }

    /// The guaranteed repo's floor revives a passed deadline; it restores on
    /// drop and is a no-op with no scope.
    #[test]
    fn at_least_adjusts_the_scope_and_restores() {
        let now = Instant::now();
        {
            let _none = at_least(now + Duration::from_secs(20));
            assert_eq!(budget(), Budget::Unbounded, "no scope: nothing to widen");
        }
        let _g = enter(now.checked_sub(Duration::from_secs(1)).unwrap());
        assert!(passed());
        {
            let _floor = at_least(now + Duration::from_secs(20));
            assert!(matches!(budget(), Budget::Left(d) if d > Duration::from_secs(15)));
        }
        assert!(passed(), "the floor ends with its guard");
    }

    #[test]
    fn a_passed_deadline_refuses_the_read() {
        let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let _g = enter(past);
        assert!(passed());
        assert_eq!(clamp(Duration::from_secs(30)), None);
    }

    #[test]
    fn a_snapshot_carries_the_scope_onto_another_thread() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let _scope = enter(deadline);
        let snap = snapshot();
        std::thread::spawn(move || {
            // A fresh thread has no scope of its own.
            assert_eq!(budget(), Budget::Unbounded);
            {
                let _g = install(snap);
                assert!(matches!(budget(), Budget::Left(_)));
            }
            assert_eq!(budget(), Budget::Unbounded);
        })
        .join()
        .unwrap();
        // An empty snapshot installs no scope.
        let none = std::thread::spawn(snapshot).join().unwrap();
        let _g = install(none);
        assert_eq!(budget(), Budget::Unbounded);
    }
}
