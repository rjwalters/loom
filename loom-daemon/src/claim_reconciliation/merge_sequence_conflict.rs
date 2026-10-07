//! Ordering edges require a REAL merge conflict, not just a shared filename
//! (#10350).
//!
//! A shared changed filename is a cheap prefilter: two PRs that each add one
//! `pub mod` line to `loom-daemon/src/lib.rs` share a file, but their edits
//! merge cleanly, so ordering them only queues ready work behind an unrelated
//! head (the 2026-10-05 incident: ~28 PRs chained through `lib.rs` behind a
//! red head and a human hold). For a pair that already shares a file, this
//! module asks git whether the two heads actually conflict:
//!
//! ```text
//! git merge-tree --write-tree --no-messages <head A> <head B>
//! ```
//!
//! — a three-way merge of the two heads against their merge base. Exit 0 is a
//! clean merge (no edge); exit 1 is a conflict (keep the edge). The command
//! writes objects only: no working tree, index or ref of the workspace is
//! touched.
//!
//! **Fail closed.** Every unknown counts as a conflict and keeps the edge: an
//! unpinned head, a failed or timed-out fetch, a head SHA that does not
//! resolve, an old git without `--write-tree`, any exit code other than 0/1,
//! and any pair past the per-tick evaluation budget or the shared wall-clock
//! [`Deadline`]. A "no conflict" is never fabricated.
//!
//! **Time-bounded.** The pair budget caps the *count* of evaluations, not the
//! time they take, and the pass runs synchronously ahead of every later
//! workspace's claim/verdict recovery. A [`Deadline`] shared by both phases
//! (the head fetches and the merge-trees) bounds the whole pass: each
//! subprocess timeout is clamped to the time remaining, and once it is spent no
//! further git command is launched — the remaining pairs keep their edge/hold
//! and are answered on a later tick.
//!
//! **Cached.** A verdict is a function of the two head SHAs alone (the merge
//! base is derived from them), so it is cached across ticks in
//! [`read_cache::PAIR_CONFLICT`] keyed on the root and the sorted pair of
//! SHAs; a moved head is a new key and is re-evaluated. Within a tick each pair
//! is evaluated at most once even with the cross-tick cache off. Only answered
//! verdicts are stored — a failure is retried next tick.
//!
//! Stacked-base edges (follower base == predecessor head) never consult this
//! predicate: the planner keeps them unconditionally.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::SequencePr;
use crate::claim_reconciliation::read_cache;

/// Uncached pair evaluations allowed per tick per root. A pair past the
/// budget counts as a conflict (fail closed); the cross-tick cache means a
/// large component converges over a few ticks.
pub const PAIR_EVALUATIONS_PER_TICK: usize = 256;

/// Wall-clock bound on one workspace's whole live conflict-check pass: every
/// fetch and merge-tree together. Past it the remaining pairs fail closed.
pub const PASS_DEADLINE: Duration = Duration::from_secs(120);

/// Bound on one `git fetch` of a PR head.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on one `git merge-tree` / `cat-file` run.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

/// The remote PR heads are fetched from (`refs/pull/<n>/head`).
const REMOTE: &str = "origin";

/// A shared wall-clock budget for a whole pass. The clock is injectable so a
/// test can exhaust it without sleeping.
pub struct Deadline {
    total: Duration,
    elapsed: Box<dyn Fn() -> Duration>,
}

impl Deadline {
    /// A real-time deadline of `total`, starting now.
    #[must_use]
    pub fn new(total: Duration) -> Rc<Self> {
        let start = Instant::now();
        Self::with_clock(total, move || start.elapsed())
    }

    /// A deadline of `total` measured by `elapsed` (time since the pass began).
    #[must_use]
    pub fn with_clock(total: Duration, elapsed: impl Fn() -> Duration + 'static) -> Rc<Self> {
        Rc::new(Self {
            total,
            elapsed: Box::new(elapsed),
        })
    }

    /// No bound at all (the filename-free test checkers).
    #[must_use]
    pub fn unbounded() -> Rc<Self> {
        Self::with_clock(Duration::MAX, || Duration::ZERO)
    }

    /// Time left, zero once exhausted.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.total.saturating_sub((self.elapsed)())
    }

    /// Has the budget been spent?
    #[must_use]
    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

/// The pair predicate the planner and Phase 1 take: `true` = the two PRs
/// really conflict (or it is unknown).
pub type Conflicts<'a> = &'a dyn Fn(&SequencePr, &SequencePr) -> bool;

/// The filename-only behaviour: every shared-file pair conflicts. Used by the
/// pure planner entry points that take no predicate.
#[must_use]
pub fn assume_conflict(_: &SequencePr, _: &SequencePr) -> bool {
    true
}

/// Per-tick pair-conflict predicate over an evaluator `E`, which returns
/// `Some(true)` for a conflict, `Some(false)` for a clean merge and `None`
/// when it could not tell.
pub struct PairChecker<E> {
    root_key: String,
    budget: Cell<usize>,
    deadline: Rc<Deadline>,
    tick: RefCell<BTreeMap<(String, String), bool>>,
    eval: E,
}

impl<E: Fn(&SequencePr, &SequencePr) -> Option<bool>> PairChecker<E> {
    /// A checker for `root` allowing `budget` uncached evaluations, with no
    /// time bound.
    pub fn with_eval(root: &Path, budget: usize, eval: E) -> Self {
        Self::with_deadline(root, budget, Deadline::unbounded(), eval)
    }

    /// As [`Self::with_eval`], but no uncached evaluation starts once
    /// `deadline` has expired (a cached verdict is still served). `eval` is
    /// expected to clamp its own subprocesses to the same deadline.
    pub fn with_deadline(root: &Path, budget: usize, deadline: Rc<Deadline>, eval: E) -> Self {
        Self {
            root_key: root.display().to_string(),
            budget: Cell::new(budget),
            deadline,
            tick: RefCell::new(BTreeMap::new()),
            eval,
        }
    }

    /// Do `a` and `b` really conflict? `true` on any unknown.
    pub fn conflicts(&self, a: &SequencePr, b: &SequencePr) -> bool {
        let head = |p: &SequencePr| p.head_sha.clone().filter(|s| !s.is_empty());
        let (Some(ha), Some(hb)) = (head(a), head(b)) else {
            return true;
        };
        if ha == hb {
            return true;
        }
        let ((x, hx), (y, hy)) = if ha < hb {
            ((a, ha), (b, hb))
        } else {
            ((b, hb), (a, ha))
        };
        let pair = (hx, hy);
        if let Some(v) = self.tick.borrow().get(&pair) {
            return *v;
        }
        let key = read_cache::key_of(&[&self.root_key, "merge-tree-pair", &pair.0, &pair.1]);
        let answer = read_cache::PAIR_CONFLICT.get_or(key, || {
            if self.deadline.expired() {
                return None;
            }
            let left = self.budget.get().checked_sub(1)?;
            self.budget.set(left);
            (self.eval)(x, y)
        });
        let verdict = answer.unwrap_or(true);
        self.tick.borrow_mut().insert(pair, verdict);
        verdict
    }
}

/// The live checker for one workspace `root`: fetches PR heads that are not
/// already local (at most once per PR per tick) and runs `git merge-tree`.
pub fn live(root: &Path) -> PairChecker<impl Fn(&SequencePr, &SequencePr) -> Option<bool>> {
    let dir: PathBuf = root.to_path_buf();
    let deadline = Deadline::new(PASS_DEADLINE);
    let eval_deadline = Rc::clone(&deadline);
    let present: RefCell<BTreeMap<u32, bool>> = RefCell::new(BTreeMap::new());
    let eval = move |a: &SequencePr, b: &SequencePr| {
        let ready = |p: &SequencePr| {
            *present
                .borrow_mut()
                .entry(p.number)
                .or_insert_with(|| ensure_head(&dir, p, &eval_deadline))
        };
        if !(ready(a) && ready(b)) {
            return None;
        }
        merge_tree_within(&dir, a.head_sha.as_deref()?, b.head_sha.as_deref()?, &eval_deadline)
    };
    PairChecker::with_deadline(root, PAIR_EVALUATIONS_PER_TICK, deadline, eval)
}

/// Make `pr`'s pinned head commit available locally: already present, or
/// fetched from `refs/pull/<n>/head` (into `FETCH_HEAD`; no ref is created). `false` when the pinned SHA still does not resolve —
/// including when the PR head moved past the listing's SHA.
fn ensure_head(root: &Path, pr: &SequencePr, deadline: &Deadline) -> bool {
    let Some(sha) = pr.head_sha.as_deref().filter(|s| !s.is_empty()) else {
        return false;
    };
    let commit = format!("{sha}^{{commit}}");
    let has = || git_code(root, &["cat-file", "-e", &commit], GIT_TIMEOUT, deadline) == Some(0);
    if has() {
        return true;
    }
    // Fetch to FETCH_HEAD rather than a private ref: the objects land either
    // way, and there is no ref to delete afterwards, so no git command can start
    // after the deadline has expired.
    let spec = format!("refs/pull/{}/head", pr.number);
    let fetch = ["fetch", "--quiet", "--no-tags", REMOTE, "--", &spec];
    let fetched = git_code(root, &fetch, FETCH_TIMEOUT, deadline) == Some(0);
    fetched && has()
}

/// `git merge-tree --write-tree` of two commits in `root`: `Some(false)` on a
/// clean merge, `Some(true)` on a conflict, `None` on anything else (old git,
/// unrelated histories, a missing object, a timeout).
#[must_use]
pub fn merge_tree_conflicts(root: &Path, a: &str, b: &str) -> Option<bool> {
    merge_tree_within(root, a, b, &Deadline::unbounded())
}

/// [`merge_tree_conflicts`] with its timeout clamped to `deadline`; `None`
/// without launching git once the deadline is spent.
fn merge_tree_within(root: &Path, a: &str, b: &str, deadline: &Deadline) -> Option<bool> {
    let args = ["merge-tree", "--write-tree", "--no-messages", a, b];
    match git_code(root, &args, GIT_TIMEOUT, deadline)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// Run `git args…` in `root` with every stream closed, killing it after
/// `timeout` clamped to what remains of `deadline`. The exit code, or `None` on
/// spawn failure, signal, timeout, or an already-spent deadline (nothing is
/// launched then).
fn git_code(root: &Path, args: &[&str], timeout: Duration, deadline: &Deadline) -> Option<i32> {
    let timeout = timeout.min(deadline.remaining());
    if timeout.is_zero() {
        return None;
    }
    let mut child = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// [`overlap_components`] where a shared-file pair is an edge only when the
/// two PRs are stacked or `conflicts` says they really conflict (#10350).
#[must_use]
pub fn overlap_components_with(
    eligible: &[&SequencePr],
    files: &BTreeMap<u32, BTreeSet<String>>,
    conflicts: Conflicts<'_>,
) -> Vec<Vec<u32>> {
    let nums: Vec<u32> = eligible.iter().map(|p| p.number).collect();
    let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (i, a) in eligible.iter().enumerate() {
        for b in &eligible[i + 1..] {
            let shared = files
                .get(&a.number)
                .zip(files.get(&b.number))
                .is_some_and(|(fa, fb)| fa.iter().any(|f| fb.contains(f)));
            let stacked = a.base_ref == b.head_ref || b.base_ref == a.head_ref;
            if shared && (stacked || conflicts(a, b)) {
                adj.entry(a.number).or_default().push(b.number);
                adj.entry(b.number).or_default().push(a.number);
            }
        }
    }
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut out = Vec::new();
    for start in &nums {
        if seen.contains(start) {
            continue;
        }
        let mut component = Vec::new();
        let mut queue = VecDeque::from([*start]);
        seen.insert(*start);
        while let Some(n) = queue.pop_front() {
            component.push(n);
            for next in adj.get(&n).into_iter().flatten() {
                if seen.insert(*next) {
                    queue.push_back(*next);
                }
            }
        }
        out.push(component);
    }
    out
}

#[cfg(test)]
#[path = "merge_sequence_conflict_tests.rs"]
mod tests;
