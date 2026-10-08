//! Global issue-creation mutex (#3707) for the daemon-native epic supervisor.
//!
//! Several role transitions run `gh issue create` in a burst: the Architect
//! files a proposal, the Hermit files a simplification, the Auditor files a
//! runtime bug, and the Champion decomposes a `loom:epic` into phase issues.
//! These are the label-graph's `creates_issues=True` edges (#3841); the epic
//! one is authoritatively defined in `loom-daemon/src/epic_state.rs`'s
//! `epic_transition_table()` (#4310), while the three proposal-lane edges
//! (Architect/Hermit/Auditor) are hardcoded here directly — the proposal lane
//! has no other Rust representation.
//!
//! # The #3707 hazard
//!
//! GitHub assigns issue numbers server-side, at create time. If two
//! issue-creating transitions run concurrently their `gh issue create` calls
//! interleave: the numbers they observe race, and a burst that writes
//! cross-references (`Part of #N`, `Epic: #N`) into freshly-minted bodies can
//! bind the wrong number, cross-contaminating bodies. The fix is a single
//! **global** mutex the daemon holds across the *entire* burst of a
//! `creates_issues` transition — one filer must finish its full burst before
//! the next acquires the mutex.
//!
//! This module implements Phase 2 of epic #3842: the mutex primitive in
//! isolation. It does **not** dispatch roles or call the forge — the Phase 3
//! supervisor loop wires [`IssueCreationMutex::acquire`] around each real
//! dispatch. Here the guarantee is purely the mutual exclusion and the
//! enumeration of the transition shapes the mutex must cover.
//!
//! # Coverage (`#3707 coverage`)
//!
//! [`CREATES_ISSUES_TRANSITIONS`] is the authoritative, hand-maintained list of
//! every `creates_issues=True` edge across all label lanes (see
//! `test_creates_issues_transitions_are_exhaustive`). Only a transition from
//! that set may be passed to [`IssueCreationMutex::acquire`].

use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// The transitions currently holding an issue-creation mutex, across every
/// per-workspace instance (#10831). A held `tokio` mutex cannot be inspected
/// from outside, so each acquire and release is mirrored here; the
/// pause-and-roll H4 snapshot reads it through [`holder_snapshot`] to mark the
/// role run that holds the mutex (`holds_issue_creation_mutex`, design
/// `docs/design/daemon-roll-pause-resume.md` §4).
static HOLDERS: std::sync::Mutex<Vec<CreatesIssuesTransition>> = std::sync::Mutex::new(Vec::new());

fn holders() -> std::sync::MutexGuard<'static, Vec<CreatesIssuesTransition>> {
    HOLDERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Every transition whose burst holds an issue-creation mutex right now.
#[must_use]
pub fn holder_snapshot() -> Vec<CreatesIssuesTransition> {
    holders().clone()
}

/// Holds seeded for runs a daemon roll resumed (#10832), by workspace root.
/// See [`seed_hold`].
static SEEDED: std::sync::Mutex<Vec<(std::path::PathBuf, CreatesIssuesTransition)>> =
    std::sync::Mutex::new(Vec::new());
static SEEDED_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn seeds() -> std::sync::MutexGuard<'static, Vec<(std::path::PathBuf, CreatesIssuesTransition)>> {
    SEEDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A hold on a workspace's issue creation carried across a daemon roll
/// (#10832; design `docs/design/daemon-roll-pause-resume.md` §4 "Role run").
///
/// The mutex lives in memory, so a restart frees it. A role run that held it
/// when a roll paused it is resumed from its saved session and carries on with
/// its `gh issue create` burst, with nothing holding the mutex for it: the
/// next issue-creating transition would interleave with that burst, which is
/// the #3707 hazard. H5 therefore seeds a hold for the resumed run before it
/// relaunches it. While a seeded hold for a workspace is alive, that
/// workspace's [`IssueCreationMutex`] ([`IssueCreationMutex::for_root`])
/// refuses [`IssueCreationMutex::try_acquire`] and
/// [`IssueCreationMutex::acquire`] waits; dropping the hold (the resumed run
/// ended) lifts it.
#[derive(Debug)]
pub struct SeededHold {
    root: std::path::PathBuf,
    transition: CreatesIssuesTransition,
}

impl SeededHold {
    /// The transition this hold stands in for.
    #[must_use]
    pub fn transition(&self) -> CreatesIssuesTransition {
        self.transition
    }
}

impl Drop for SeededHold {
    fn drop(&mut self) {
        let mut all = holders();
        if let Some(at) = all.iter().position(|t| *t == self.transition) {
            all.remove(at);
        }
        drop(all);
        let mut seeded = seeds();
        if let Some(at) = seeded
            .iter()
            .position(|(r, t)| *r == self.root && *t == self.transition)
        {
            seeded.remove(at);
        }
        SEEDED_COUNT.store(seeded.len(), std::sync::atomic::Ordering::SeqCst);
    }
}

/// Seed a hold for a resumed run of `role` in the workspace at `root` that
/// held the mutex when it was paused. `None` when `role` has no
/// issue-creating transition. The hold is listed by [`holder_snapshot`], so a
/// second roll records the resumed run as the holder again.
#[must_use]
pub fn seed_hold(root: &std::path::Path, role: &str) -> Option<SeededHold> {
    let transition = *CREATES_ISSUES_TRANSITIONS
        .iter()
        .find(|t| t.role.eq_ignore_ascii_case(role))?;
    holders().push(transition);
    let mut seeded = seeds();
    seeded.push((root.to_path_buf(), transition));
    SEEDED_COUNT.store(seeded.len(), std::sync::atomic::Ordering::SeqCst);
    Some(SeededHold {
        root: root.to_path_buf(),
        transition,
    })
}

/// An issue-creating (`creates_issues=True`) transition shape.
///
/// The three fields identify a label-graph edge (source state, destination
/// state, firing role). Instances are compared structurally, so a caller must
/// pass exactly one of the [`CREATES_ISSUES_TRANSITIONS`] entries to acquire
/// the mutex — this keeps the mutex's coverage set pinned to the known
/// `#3707 coverage` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreatesIssuesTransition {
    /// Source state id (e.g. `"new"`, `"epic:needs_decomp"`).
    pub src: &'static str,
    /// Destination state id (e.g. `"loom:architect"`, `"epic:designed"`).
    pub dst: &'static str,
    /// The firing role's canonical name (e.g. `"Architect"`, `"Champion"`).
    pub role: &'static str,
}

impl std::fmt::Display for CreatesIssuesTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}->{} [{}]", self.src, self.dst, self.role)
    }
}

/// The Architect proposal-filing edge (`new -> loom:architect`).
pub const ARCHITECT_PROPOSAL: CreatesIssuesTransition = CreatesIssuesTransition {
    src: "new",
    dst: "loom:architect",
    role: "Architect",
};

/// The Hermit simplification-proposal edge (`new -> loom:hermit`).
pub const HERMIT_PROPOSAL: CreatesIssuesTransition = CreatesIssuesTransition {
    src: "new",
    dst: "loom:hermit",
    role: "Hermit",
};

/// The Auditor runtime-bug edge (`new -> loom:auditor`).
pub const AUDITOR_PROPOSAL: CreatesIssuesTransition = CreatesIssuesTransition {
    src: "new",
    dst: "loom:auditor",
    role: "Auditor",
};

/// The Champion epic-decomposition edge (`epic:needs_decomp -> epic:designed`).
///
/// This is the epic-supervisor `creates_issues` edge: the Champion mints the
/// phase issues that decompose an epic. It is exactly the "Champion phase
/// expansion / decomposition dispatch" burst the mutex serializes.
pub const CHAMPION_EPIC_DECOMP: CreatesIssuesTransition = CreatesIssuesTransition {
    src: "epic:needs_decomp",
    dst: "epic:designed",
    role: "Champion",
};

/// Every `creates_issues=True` transition the #3707 mutex must serialize.
///
/// This is the authoritative, hand-maintained enumeration of every
/// issue-creating label-graph edge — one epic-lane edge (mirroring
/// `epic_state::epic_transition_table()`'s sole `creates_issues` entry) plus
/// three proposal-lane edges with no other Rust representation. Keep it
/// updated whenever a role gains or loses an issue-creating transition — the
/// `test_creates_issues_transitions_are_exhaustive` test pins the count and
/// membership.
pub const CREATES_ISSUES_TRANSITIONS: &[CreatesIssuesTransition] = &[
    ARCHITECT_PROPOSAL,
    HERMIT_PROPOSAL,
    AUDITOR_PROPOSAL,
    CHAMPION_EPIC_DECOMP,
];

/// True if `transition` is one of the [`CREATES_ISSUES_TRANSITIONS`] the mutex
/// serializes.
#[must_use]
pub fn is_issue_creating(transition: CreatesIssuesTransition) -> bool {
    CREATES_ISSUES_TRANSITIONS.contains(&transition)
}

/// Internal state guarded by the mutex. Tracks the in-flight burst (if any) and
/// a monotonic count of completed bursts for observability / testing.
#[derive(Debug, Default)]
struct GateState {
    /// The transition whose burst currently holds the mutex, or `None` when
    /// idle.
    current: Option<CreatesIssuesTransition>,
    /// Number of issue-creating bursts that have run to completion (guard
    /// dropped). Strictly monotonic.
    completed_bursts: u64,
}

/// A daemon-global mutex that serializes every issue-creating (`creates_issues`)
/// transition (#3707).
///
/// Cheaply cloneable (`Arc` inside) so the single daemon-wide instance can be
/// shared across every dispatch site. Acquisition is FIFO-fair (tokio's
/// `Mutex`) and *async* — an issue-creating transition `.await`s the guard,
/// runs its full `gh issue create` burst, then drops the guard to release.
#[derive(Clone)]
pub struct IssueCreationMutex {
    inner: Arc<Mutex<GateState>>,
    /// The workspace this instance serializes, when it was built with
    /// [`Self::for_root`]. Only such an instance honours a [`SeededHold`].
    root: Option<std::path::PathBuf>,
}

impl IssueCreationMutex {
    /// Construct a fresh, unheld mutex.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateState::default())),
            root: None,
        }
    }

    /// [`Self::new`] for the workspace at `root` (#10832): this instance also
    /// honours a hold [`seed_hold`] placed for that workspace.
    #[must_use]
    pub fn for_root(root: &std::path::Path) -> Self {
        Self {
            root: Some(root.to_path_buf()),
            ..Self::new()
        }
    }

    /// Whether a run resumed after a roll holds this workspace's mutex.
    fn seeded(&self) -> bool {
        SEEDED_COUNT.load(std::sync::atomic::Ordering::SeqCst) > 0
            && self
                .root
                .as_ref()
                .is_some_and(|root| seeds().iter().any(|(r, _)| r == root))
    }

    /// Acquire the mutex for an issue-creating `transition`, waiting if another
    /// burst holds it.
    ///
    /// Returns an [`IssueCreationGuard`]; hold it for the *entire* burst of
    /// `gh issue create` calls and drop it (let it fall out of scope) only once
    /// the burst is complete. While held, no other issue-creating transition
    /// can proceed — this is the #3707 serialization guarantee.
    ///
    /// `transition` must be one of [`CREATES_ISSUES_TRANSITIONS`]; passing a
    /// non-issue-creating shape is a programming error and panics in debug
    /// builds (the type already constrains callers to the exported constants).
    pub async fn acquire(&self, transition: CreatesIssuesTransition) -> IssueCreationGuard {
        debug_assert!(
            is_issue_creating(transition),
            "acquire() called with a non-creates_issues transition: {transition}"
        );
        // #10832: a run resumed after a roll may still be mid-burst.
        while self.seeded() {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let mut guard = Arc::clone(&self.inner).lock_owned().await;
        guard.current = Some(transition);
        holders().push(transition);
        IssueCreationGuard { guard }
    }

    /// Try to acquire the mutex without waiting.
    ///
    /// Returns `Some(guard)` if the mutex was idle, or `None` if a burst is
    /// already in flight. Useful for a supervisor tick that must not block.
    #[must_use]
    pub fn try_acquire(&self, transition: CreatesIssuesTransition) -> Option<IssueCreationGuard> {
        debug_assert!(
            is_issue_creating(transition),
            "try_acquire() called with a non-creates_issues transition: {transition}"
        );
        if self.seeded() {
            return None; // #10832: held for a run resumed after a roll
        }
        match Arc::clone(&self.inner).try_lock_owned() {
            Ok(mut guard) => {
                guard.current = Some(transition);
                holders().push(transition);
                Some(IssueCreationGuard { guard })
            }
            Err(_) => None,
        }
    }

    /// True if an issue-creating burst currently holds the mutex (including a
    /// hold seeded for a run resumed after a roll, #10832).
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.seeded() || self.inner.try_lock().is_err()
    }

    /// Number of issue-creating bursts that have completed (guard dropped).
    ///
    /// Awaits the mutex to read a consistent value; returns `0` on a fresh
    /// mutex. Primarily for observability and tests.
    pub async fn completed_bursts(&self) -> u64 {
        self.inner.lock().await.completed_bursts
    }
}

impl Default for IssueCreationMutex {
    fn default() -> Self {
        Self::new()
    }
}

/// An RAII guard proving exclusive hold of the [`IssueCreationMutex`].
///
/// Held for the duration of one issue-creating burst. Dropping it releases the
/// mutex and bumps the completed-burst counter, letting the next waiting
/// transition proceed.
pub struct IssueCreationGuard {
    guard: OwnedMutexGuard<GateState>,
}

impl IssueCreationGuard {
    /// The transition whose burst this guard authorizes.
    #[must_use]
    pub fn transition(&self) -> CreatesIssuesTransition {
        // `current` is set to `Some` at acquire time and only cleared on drop,
        // so it is always present while the guard is live.
        self.guard
            .current
            .expect("live guard always has a current transition")
    }
}

impl Drop for IssueCreationGuard {
    fn drop(&mut self) {
        // Mark the burst complete and clear the in-flight marker before the
        // underlying lock is released.
        self.guard.completed_bursts = self.guard.completed_bursts.saturating_add(1);
        if let Some(held) = self.guard.current.take() {
            let mut all = holders();
            if let Some(at) = all.iter().position(|t| *t == held) {
                all.remove(at);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // ===== coverage-set exhaustiveness =====

    #[test]
    fn test_creates_issues_transitions_are_exhaustive() {
        // Exactly these four label-graph edges are creates_issues=True.
        assert_eq!(CREATES_ISSUES_TRANSITIONS.len(), 4);

        let expected: Vec<(&str, &str, &str)> = vec![
            ("new", "loom:architect", "Architect"),
            ("new", "loom:hermit", "Hermit"),
            ("new", "loom:auditor", "Auditor"),
            ("epic:needs_decomp", "epic:designed", "Champion"),
        ];
        let actual: Vec<(&str, &str, &str)> = CREATES_ISSUES_TRANSITIONS
            .iter()
            .map(|t| (t.src, t.dst, t.role))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_is_issue_creating() {
        assert!(is_issue_creating(ARCHITECT_PROPOSAL));
        assert!(is_issue_creating(CHAMPION_EPIC_DECOMP));

        // A non-creating shape (e.g. Builder opening a PR) is not covered.
        let non_creating = CreatesIssuesTransition {
            src: "loom:building",
            dst: "loom:review-requested",
            role: "Builder",
        };
        assert!(!is_issue_creating(non_creating));
    }

    // ===== basic acquire / release =====

    #[tokio::test]
    async fn test_acquire_sets_transition_and_release_counts() {
        let m = IssueCreationMutex::new();
        assert!(!m.is_held());
        assert_eq!(m.completed_bursts().await, 0);

        {
            let g = m.acquire(ARCHITECT_PROPOSAL).await;
            assert_eq!(g.transition(), ARCHITECT_PROPOSAL);
            assert!(m.is_held());
        }
        assert!(!m.is_held());
        assert_eq!(m.completed_bursts().await, 1);
    }

    /// #10831: the holder is visible to the pause-and-roll snapshot while the
    /// burst runs. (Only presence is asserted: other tests in this binary hold
    /// the same transitions concurrently.)
    #[tokio::test]
    async fn test_holder_snapshot_lists_a_live_holder() {
        let mutex = IssueCreationMutex::new();
        let guard = mutex.acquire(CHAMPION_EPIC_DECOMP).await;
        assert!(holder_snapshot().contains(&CHAMPION_EPIC_DECOMP));
        drop(guard);
    }

    /// #10832: a hold seeded for a run resumed after a roll keeps its
    /// workspace's mutex held until the run ends, and no other workspace's.
    #[tokio::test]
    async fn test_a_seeded_hold_keeps_its_workspaces_mutex_held_until_dropped() {
        let root = std::path::PathBuf::from("/seed-test/repo-a");
        let mine = IssueCreationMutex::for_root(&root);
        let other = IssueCreationMutex::for_root(std::path::Path::new("/seed-test/repo-b"));
        assert!(seed_hold(&root, "judge").is_none(), "judge creates no issues");

        let hold = seed_hold(&root, "champion").expect("champion decomposes epics");
        assert_eq!(hold.transition(), CHAMPION_EPIC_DECOMP);
        assert!(mine.is_held());
        assert!(mine.try_acquire(ARCHITECT_PROPOSAL).is_none());
        assert!(holder_snapshot().contains(&CHAMPION_EPIC_DECOMP));
        assert!(other.try_acquire(ARCHITECT_PROPOSAL).is_some(), "another workspace is free");
        assert!(IssueCreationMutex::new()
            .try_acquire(HERMIT_PROPOSAL)
            .is_some());

        // `acquire` waits for the resumed run to end.
        let waiter = tokio::spawn({
            let mine = mine.clone();
            async move { mine.acquire(AUDITOR_PROPOSAL).await.transition() }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        drop(hold);
        assert_eq!(waiter.await.unwrap(), AUDITOR_PROPOSAL);
        assert!(!mine.is_held());
    }

    #[tokio::test]
    async fn test_try_acquire_none_while_held() {
        let m = IssueCreationMutex::new();
        let held = m.acquire(HERMIT_PROPOSAL).await;
        // A second attempt cannot proceed while the first burst holds it.
        assert!(m.try_acquire(AUDITOR_PROPOSAL).is_none());
        drop(held);
        // Once released, try_acquire succeeds.
        let g = m.try_acquire(AUDITOR_PROPOSAL);
        assert!(g.is_some());
    }

    // ===== the core #3707 property: provable serialization =====

    /// Spawn many concurrent issue-creating bursts through one shared mutex and
    /// assert at most one holds it at any instant. Each burst bumps a shared
    /// "active holders" counter on entry and records the running max; if the
    /// mutex ever let two in, `max` would exceed 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_mutex_serializes_concurrent_bursts() {
        let m = IssueCreationMutex::new();
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let transitions = [
            ARCHITECT_PROPOSAL,
            HERMIT_PROPOSAL,
            AUDITOR_PROPOSAL,
            CHAMPION_EPIC_DECOMP,
        ];

        let mut handles = Vec::new();
        for i in 0..40u32 {
            let m = m.clone();
            let active = Arc::clone(&active);
            let max_seen = Arc::clone(&max_seen);
            let transition = transitions[(i as usize) % transitions.len()];
            handles.push(tokio::spawn(async move {
                let _guard = m.acquire(transition).await;
                // Entering the critical section.
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                // Record the running maximum of concurrent holders.
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Simulate a multi-call `gh issue create` burst.
                tokio::time::sleep(Duration::from_millis(2)).await;
                // The invariant: never more than one holder.
                assert_eq!(
                    active.load(Ordering::SeqCst),
                    1,
                    "more than one issue-creating burst held the mutex concurrently"
                );
                active.fetch_sub(1, Ordering::SeqCst);
                // _guard dropped here → release.
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "mutex admitted concurrent issue-creating bursts"
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(m.completed_bursts().await, 40);
    }

    /// The next burst must wait for the prior burst to *finish* — not merely to
    /// start. A holder that sleeps before releasing blocks a concurrently
    /// launched acquirer until the sleep completes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_next_burst_waits_for_prior_to_complete() {
        let m = IssueCreationMutex::new();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let first = {
            let m = m.clone();
            let order = Arc::clone(&order);
            tokio::spawn(async move {
                let _g = m.acquire(ARCHITECT_PROPOSAL).await;
                order.lock().await.push("first-enter");
                tokio::time::sleep(Duration::from_millis(20)).await;
                order.lock().await.push("first-exit");
            })
        };

        // Give `first` a head start so it holds the lock before `second` tries.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let second = {
            let m = m.clone();
            let order = Arc::clone(&order);
            tokio::spawn(async move {
                let _g = m.acquire(CHAMPION_EPIC_DECOMP).await;
                order.lock().await.push("second-enter");
            })
        };

        first.await.unwrap();
        second.await.unwrap();

        let seq = order.lock().await.clone();
        // second-enter must come strictly after first-exit.
        let first_exit = seq.iter().position(|s| *s == "first-exit").unwrap();
        let second_enter = seq.iter().position(|s| *s == "second-enter").unwrap();
        assert!(
            first_exit < second_enter,
            "second burst entered before the first burst completed: {seq:?}"
        );
    }
}
