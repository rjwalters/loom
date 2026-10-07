//! One hygiene pass (W6 PR2): what a reaper or `clean` pass may hold, and
//! the fresh read every removal makes first.
//!
//! Hygiene removes worktrees and branches. Everything here fails toward
//! KEEP: an answer that is missing, unknown, gone, mismatched or merely
//! different from the one the pass held leaves the worktree in place.
//!
//! # Held for one pass
//!
//! A [`Pass`] remembers each issue (`issues/{n}`: state and `closed_at`),
//! each PR (`pulls/{n}`: status and head SHA) and each branch's PR status
//! (`pulls?head=`) the first time the pass reads it, and answers later
//! questions about the same item from memory. The reaper asks twice per
//! kept worktree — once to decide about removal, once to decide about
//! reclaiming its build artifacts — and the issue's `closed_at` is a third
//! question answered by the same body. Only real answers are held: an
//! unknown, a gone or a failed read is asked again. Nothing is persisted and
//! nothing outlives the [`Pass`].
//!
//! # Remembered across passes
//!
//! Only a merged PR ([`super::hygiene_terminal`]). A pass asks that store
//! before the forge, so a kept worktree whose PR merged costs no read.
//!
//! # The confirm
//!
//! A held answer, a remembered merge and a `304`-served body are all
//! discovery. None of them may be the last word before data is destroyed
//! (ADR-0021: a read that gates an action never comes from a held cache), so
//! immediately before a quarantine or a removal the caller asks for a
//! confirm, which makes ONE unconditional read of the numbered item the
//! decision rested on — the issue for an `issue-<N>` worktree or branch, the
//! PR for a `pr-<N>` worktree:
//!
//! - the fresh answer equals the held one: proceed;
//! - it differs, or is unknown or gone: KEEP, bump
//!   [`CONFIRM_DOWNGRADE`], and forget what was held.
//!
//! A decision that used no forge state for the item (an unregistered
//! orphan directory; `--aggressive` removing landed work whatever the issue
//! says) holds nothing, so there is nothing to confirm and no read is made.
//!
//! A branch listing has no item number to re-read. It is always
//! unconditional, and a held one is first-hand only for the question that
//! read it. [`Pass::confirm_issue`] therefore refuses a removal whose
//! unmerged PR status (closed without merge, or no PR) was answered from
//! memory, and forgets it so the next pass reads it again. A merged status
//! cannot revert and needs no such guard.
//!
//! # Rate-limit breaker
//!
//! While the global breaker is cooling a pass makes no forge call for an
//! item: not the read, not the store lookup (which may resolve the repo),
//! not the confirm. Every answer is unknown, so everything is kept.
//!
//! # `LOOM_HYGIENE_MEMO=0`
//!
//! Restores the pre-PR2 behaviour exactly: nothing is held (except the
//! reaper's per-pass `pr-<N>` probe, which predates this module), nothing is
//! remembered, no confirm is made, and `clean`'s issue-keyed probes go back
//! to `gh issue view` / `gh pr list`.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::Path;

use crate::forge_call_stats::counters;
use crate::forge_repo_facts::{self as facts, OwnerFact};

use super::clean::{self, PrProbe, PrStatus, WorktreeDecision};
use super::forge_state::{self, IssueFacts, IssueState, PullFacts, Read};
use super::{clean_owner, gh, hygiene_terminal as terminal, naming};

/// `0` restores the pre-PR2 behaviour (see the module docs).
pub(crate) const MEMO_ENV: &str = "LOOM_HYGIENE_MEMO";
/// Counter: a pre-removal fresh read did not confirm what the pass held,
/// and the worktree or branch was kept.
pub(crate) const CONFIRM_DOWNGRADE: &str = "hygiene.confirm_downgrade";
/// Counter: a question answered from the pass's own memory.
pub(crate) const MEMO_HIT: &str = "hygiene.memo_hit";
/// Counter: a merged PR answered from the store, with no forge read.
pub(crate) const TERMINAL_HIT: &str = "hygiene.terminal_hit";

/// Ledger names of the pre-removal reads.
const CONFIRM_ISSUE: &str = "hygiene.confirm_issue";
const CONFIRM_PR: &str = "hygiene.confirm_pr";

/// Is the memo (and with it the terminal store and the confirm) on?
pub(crate) fn enabled() -> bool {
    std::env::var(MEMO_ENV).ok().is_none_or(|v| v.trim() != "0")
}

/// What a pre-removal confirm decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirm {
    /// The fresh read agreed (or nothing was held): go ahead.
    Proceed,
    /// Keep the worktree; the payload says why.
    Keep(String),
}

impl Confirm {
    /// The reason to keep, or `None` to proceed.
    pub(crate) fn keep_reason(self) -> Option<String> {
        match self {
            Confirm::Proceed => None,
            Confirm::Keep(reason) => Some(reason),
        }
    }
}

/// A branch's PR status as this pass holds it.
#[derive(Debug, Clone)]
struct HeldBranch {
    status: PrStatus,
    /// Answered from memory at least once since it was read.
    served: bool,
}

/// One hygiene pass over `root`. See the module docs.
pub(crate) struct Pass<'a> {
    root: &'a Path,
    on: bool,
    owner: OnceCell<Option<OwnerFact>>,
    issues: RefCell<HashMap<u32, IssueFacts>>,
    pulls: RefCell<HashMap<u32, PrProbe>>,
    branches: RefCell<HashMap<String, HeldBranch>>,
    /// Owner confirmations are memoised for the pass (W3a); scopes nest.
    _facts: facts::PassScope,
}

impl<'a> Pass<'a> {
    /// Start a pass over `root`. Hold it for the pass and drop it at the end.
    pub(crate) fn begin(root: &'a Path) -> Self {
        Self {
            root,
            on: enabled(),
            owner: OnceCell::new(),
            issues: RefCell::new(HashMap::new()),
            pulls: RefCell::new(HashMap::new()),
            branches: RefCell::new(HashMap::new()),
            _facts: facts::PassScope::enter(),
        }
    }

    /// The owner behind `head=<owner>:<branch>`, resolved once per pass.
    fn owner(&self) -> Option<&OwnerFact> {
        self.owner
            .get_or_init(|| clean_owner::repo_owner(self.root))
            .as_ref()
    }

    /// Issue `n`'s state and `closed_at`: held, else read (conditional).
    fn issue(&self, n: u32, caller: &'static str) -> Read<IssueFacts> {
        if !self.on {
            return forge_state::issue_facts(self.root, n, caller);
        }
        if let Some(hit) = self.issues.borrow().get(&n) {
            counters::bump(MEMO_HIT);
            return Read::Ok(hit.clone());
        }
        let read = forge_state::issue_facts(self.root, n, caller);
        if let Read::Ok(facts) = &read {
            self.issues.borrow_mut().insert(n, facts.clone());
        }
        read
    }

    /// `"OPEN"` / `"CLOSED"` / `"UNKNOWN"` — [`gh::issue_state_rest`].
    pub(crate) fn issue_state(&self, n: u32) -> String {
        state_name(self.issue(n, "worktree.issue_state_rest").ok().as_ref()).to_string()
    }

    /// The issue's `closed_at` — [`gh::issue_closed_at_rest`]. After
    /// [`Self::issue_state`] it is answered by the same body, with no read.
    pub(crate) fn issue_closed_at(&self, n: u32) -> Option<String> {
        self.issue(n, "worktree.issue_closed_at").ok()?.closed_at
    }

    /// `clean`'s issue-state probe: the REST item read; `gh issue view`
    /// with the memo off.
    pub(crate) fn clean_issue_state(&self, n: u32) -> String {
        if self.on {
            self.issue_state(n)
        } else {
            gh::issue_state(self.root, n)
        }
    }

    /// PR `n`'s status and head SHA: held, else the remembered merge, else
    /// read (conditional). Unknown on any failure.
    pub(crate) fn pull(&self, n: u32) -> PrProbe {
        if let Some(hit) = self.pulls.borrow().get(&n) {
            if self.on {
                counters::bump(MEMO_HIT);
            }
            return hit.clone();
        }
        if !self.on {
            // The reaper's pre-PR2 `pr_cache`: one probe per PR per pass,
            // whatever it answered.
            let probed = clean::check_pr_by_number_rest(self.root, n);
            self.pulls.borrow_mut().insert(n, probed.clone());
            return probed;
        }
        // A cooling breaker means no forge call at all, and the store
        // lookup may resolve the repo: let the read below answer Unknown.
        if !crate::rate_limit_breaker::global_is_suppressed() {
            if let Some(merged) = terminal::lookup(self.root, n) {
                counters::bump(TERMINAL_HIT);
                return self.hold_pull(n, merged);
            }
        }
        match forge_state::pull_facts(self.root, n, "clean.pr_by_number_rest") {
            Read::Ok(facts) if facts.status != PrStatus::Unknown => {
                terminal::record(self.root, n, &facts);
                self.hold_pull(n, facts)
            }
            Read::Ok(facts) => PrProbe {
                status: facts.status,
                head_sha: facts.head_sha,
            },
            Read::Gone | Read::Unknown => PrProbe::unknown(),
        }
    }

    fn hold_pull(&self, n: u32, facts: PullFacts) -> PrProbe {
        let probe = PrProbe {
            status: facts.status,
            head_sha: facts.head_sha,
        };
        self.pulls.borrow_mut().insert(n, probe.clone());
        probe
    }

    /// The PR status of issue `n`'s branch (`feature/issue-<n>`): held, else
    /// the owner-confirmed REST listing. With no resolvable owner — and,
    /// when `graphql_on_unknown`, when REST cannot answer — the GraphQL
    /// `gh pr list` probe is the last resort.
    pub(crate) fn issue_pr_status(&self, n: u32, graphql_on_unknown: bool) -> PrStatus {
        let branch = naming::branch_name(n);
        if self.on {
            if let Some(held) = self.branches.borrow_mut().get_mut(&branch) {
                held.served = true;
                counters::bump(MEMO_HIT);
                return held.status.clone();
            }
        }
        let rest = self
            .owner()
            .map(|owner| clean_owner::pr_status_confirmed(self.root, owner, &branch));
        let status = match rest {
            Some(PrStatus::Unknown) if graphql_on_unknown => clean::check_pr_merged(self.root, n),
            Some(status) => status,
            None => clean::check_pr_merged(self.root, n),
        };
        if self.on && status != PrStatus::Unknown {
            let held = HeldBranch {
                status: status.clone(),
                served: false,
            };
            self.branches.borrow_mut().insert(branch, held);
        }
        status
    }

    /// `clean`'s PR-status probe for issue `n`: REST first; `gh pr list`
    /// alone with the memo off.
    pub(crate) fn clean_pr_status(&self, n: u32) -> PrStatus {
        if self.on {
            self.issue_pr_status(n, true)
        } else {
            clean::check_pr_merged(self.root, n)
        }
    }

    /// The fresh read before removing (or quarantining) anything decided
    /// from issue `n`'s state. See the module docs.
    pub(crate) fn confirm_issue(&self, n: u32) -> Confirm {
        if !self.on {
            return Confirm::Proceed;
        }
        let Some(held) = self.issues.borrow().get(&n).cloned() else {
            return Confirm::Proceed;
        };
        let fresh = forge_state::issue_facts_fresh(self.root, n, CONFIRM_ISSUE);
        if fresh != Read::Ok(held.clone()) {
            self.issues.borrow_mut().remove(&n);
            return downgrade(&format!(
                "issue #{n}: this pass held {held:?}, a fresh read answered {fresh:?}"
            ));
        }
        let branch = naming::branch_name(n);
        let stale = self
            .branches
            .borrow()
            .get(&branch)
            .is_some_and(|h| h.served && !matches!(h.status, PrStatus::Merged { .. }));
        if stale {
            self.branches.borrow_mut().remove(&branch);
            return downgrade(&format!(
                "issue #{n}: the PR status of {branch} was answered from this pass's memory, not \
                 read for this removal"
            ));
        }
        Confirm::Proceed
    }

    /// The fresh read before removing a `pr-<N>` worktree: PR `n` must
    /// still be exactly what this pass held (status and head SHA).
    pub(crate) fn confirm_pull(&self, n: u32) -> Confirm {
        if !self.on {
            return Confirm::Proceed;
        }
        let held = self.pulls.borrow().get(&n).cloned();
        let fresh = forge_state::pull_facts_fresh(self.root, n, CONFIRM_PR);
        let agrees = match (&held, &fresh) {
            (Some(h), Read::Ok(f)) => h.status == f.status && h.head_sha == f.head_sha,
            _ => false,
        };
        if agrees {
            return Confirm::Proceed;
        }
        self.pulls.borrow_mut().remove(&n);
        if matches!(fresh, Read::Ok(_)) {
            // The forge answered and disagrees: whatever was remembered
            // across passes is wrong. An unknown says nothing about it.
            terminal::forget(self.root, n);
        }
        downgrade(&format!("PR #{n}: this pass held {held:?}, a fresh read answered {fresh:?}"))
    }

    /// `clean`'s gate: a decision that would remove (or quarantine, or ask
    /// to remove) issue `n`'s worktree is confirmed first, and comes back
    /// as a skip when the confirm says keep. A dry run removes nothing and
    /// confirms nothing.
    pub(crate) fn gate(
        &self,
        n: u32,
        dry_run: bool,
        decision: WorktreeDecision,
    ) -> WorktreeDecision {
        let removes = matches!(
            decision,
            WorktreeDecision::Remove
                | WorktreeDecision::RemoveWithQuarantine
                | WorktreeDecision::ConfirmClosedIssue
        );
        if dry_run || !removes {
            return decision;
        }
        match self.confirm_issue(n) {
            Confirm::Proceed => decision,
            Confirm::Keep(reason) => WorktreeDecision::SkipNotMerged(reason),
        }
    }
}

/// What `clean`'s stale-branch pass reports for an unconfirmed `CLOSED`.
pub(crate) const UNCONFIRMED: &str = "UNCONFIRMED";

/// The issue state `clean`'s stale-branch pass acts on: `"CLOSED"` deletes
/// the branch, so a `CLOSED` is confirmed by a fresh read first (unless
/// `dry_run`) and reported as [`UNCONFIRMED`] when the confirm says keep.
/// With the memo off: `gh issue view`, as before.
pub(crate) fn branch_issue_state(root: &Path, n: u32, dry_run: bool) -> String {
    let pass = Pass::begin(root);
    let state = pass.clean_issue_state(n);
    if state == "CLOSED" && !dry_run && pass.confirm_issue(n) != Confirm::Proceed {
        return UNCONFIRMED.to_string();
    }
    state
}

fn state_name(facts: Option<&IssueFacts>) -> &'static str {
    match facts.map(|f| f.state) {
        Some(IssueState::Open) => "OPEN",
        Some(IssueState::Closed) => "CLOSED",
        None => "UNKNOWN",
    }
}

/// Count and log one confirm that kept a worktree.
fn downgrade(what: &str) -> Confirm {
    let n = counters::bump(CONFIRM_DOWNGRADE);
    log::warn!(
        "hygiene: keeping, the pre-removal read did not confirm — {what} ({CONFIRM_DOWNGRADE}={n})"
    );
    Confirm::Keep(format!("fresh forge read did not confirm ({what})"))
}

#[cfg(test)]
#[path = "hygiene_pass_tests.rs"]
mod tests;
