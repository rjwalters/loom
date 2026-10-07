//! The caller scope (#10752): which daemon pass a `gh` invocation serves,
//! stamped on its `invoke github` span as `github.caller`.
//!
//! Every facade execution is named by its [`super::Operation`] (`api.rest`,
//! `stale_blocked`, `comment.post`), which identifies the call **site**, not
//! the mechanism that drove it. The `loom:blocked` release pass (#10556)
//! reaches `gh` through four sites (the ETag store, the batched evidence
//! reader, the park-record writer and the comment poster), so its reads and
//! label writes were indistinguishable from every other caller's ~1.2M daily
//! spans. A pass now runs inside [`enter`]: every invocation made on this
//! thread meanwhile carries the pass's name (the `forge_call_stats` caller it
//! already accounts its ETag reads under), and the scope counts what the pass
//! spent so its own `pass.summary` record can report it.
//!
//! Thread-local on purpose: the facade runs each invocation on its caller's
//! thread (see [`super::own_writes`]), and a process-global would stamp a
//! concurrent tick's calls with the wrong pass. A nested scope shadows the
//! outer one until it is dropped; its calls count to it alone.
//!
//! The local ledger ([`crate::forge_call_stats`]) and the `loom.forge.calls`
//! metric keep their per-operation `caller` label unchanged: only the span
//! gains the attribute.

use std::cell::RefCell;
use std::marker::PhantomData;

/// What the invocations inside one scope spent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spent {
    /// `gh` executions (one per facade execution, so a page walk counts each
    /// page).
    pub calls: u64,
    /// Of those, write-intent executions.
    pub writes: u64,
    /// Of those, answered `304 Not Modified` (free on the core bucket).
    pub not_modified: u64,
}

#[derive(Debug, Clone)]
struct Frame {
    caller: &'static str,
    spent: Spent,
}

thread_local! {
    static CURRENT: RefCell<Option<Frame>> = const { RefCell::new(None) };
}

/// An active caller scope. Dropping it (or [`Scope::finish`]) restores the
/// scope that was active before. Not `Send`: it guards this thread's state.
#[derive(Debug)]
#[must_use = "the scope ends when this guard is dropped"]
pub struct Scope {
    previous: Option<Frame>,
    _thread: PhantomData<*const ()>,
}

/// Enter a scope named `caller` on this thread.
///
/// `caller` must be a valid [`super::Operation`] name (lowercase
/// `snake_case` segments), so the attribute stays bounded and joins the
/// `forge_call_stats` caller of the same name.
pub fn enter(caller: &'static str) -> Scope {
    debug_assert!(super::Operation::is_valid(caller), "invalid caller name: {caller:?}");
    let frame = Frame {
        caller,
        spent: Spent::default(),
    };
    let previous = CURRENT.with(|c| c.borrow_mut().replace(frame));
    Scope {
        previous,
        _thread: PhantomData,
    }
}

impl Scope {
    /// What this scope's invocations have spent so far.
    #[must_use]
    pub fn spent(&self) -> Spent {
        CURRENT.with(|c| c.borrow().as_ref().map(|f| f.spent).unwrap_or_default())
    }

    /// End the scope and return what it spent.
    #[must_use]
    pub fn finish(self) -> Spent {
        self.spent()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// The caller of the scope active on this thread, if any.
#[must_use]
pub fn current() -> Option<&'static str> {
    CURRENT.with(|c| c.borrow().as_ref().map(|f| f.caller))
}

/// Count one completed execution against the active scope (a no-op outside
/// one).
pub(super) fn note(write: bool, not_modified: bool) {
    CURRENT.with(|c| {
        if let Some(frame) = c.borrow_mut().as_mut() {
            frame.spent.calls += 1;
            frame.spent.writes += u64::from(write);
            frame.spent.not_modified += u64::from(not_modified);
        }
    });
}

#[cfg(test)]
#[path = "caller_scope_tests.rs"]
mod tests;
