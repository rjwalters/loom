//! Merge-group-wide authorization: evaluate last, cover every member, re-fail
//! on revocation (#10256, Phase B4 of #9978).
//!
//! [`super::authz::merge_group_check`] authorizes one PR at one head. Three
//! properties of GitHub's merge queue make that insufficient on its own:
//!
//! 1. **A merge group carries every entry ahead of it.** The group built for
//!    the entry at position *k* contains the PRs at positions `1..=k`, and
//!    when its required checks pass GitHub can merge all of them together. A
//!    check that evaluates only the group's own PR lets a revoked PR ahead of
//!    it merge inside a later group. [`group_check`] therefore evaluates
//!    **every member** and fails if any member is unauthorized.
//! 2. **An early pass leaves a long window.** If the authorization status
//!    concludes while CI is still running, a hold added during the remaining
//!    CI time is invisible unless something re-evaluates. [`group_check`]
//!    **concludes last**: it reports [`GroupConclusion::Pending`], which
//!    blocks, until every *other* required check on the group commit has
//!    succeeded, and only then reads the live facts. Until then the live
//!    facts are not read at all, so a facts or grant-store outage while CI
//!    runs also leaves it pending; only a definite grant denial (no grant,
//!    grant for another head) fails early. Nothing re-running it (daemon or
//!    workflow outage) leaves it pending, so GitHub times the entry out
//!    instead of merging it.
//! 3. **GitHub may hold a fully green group** (minimum group size and wait
//!    time). [`revoke_refail_dequeue`] revokes the grant, then posts a
//!    `failure` status for [`REQUIRED_CHECK_CONTEXT`] on **every live group
//!    commit containing the PR** (a commit status is latest-wins per
//!    context), then dequeues. All three are attempted; each is idempotent.
//!
//! # What remains open
//!
//! The interval between GitHub's own merge decision on a fully green group
//! and its ref update, and any revocation whose status re-fail cannot reach
//! GitHub (status API outage) after the final evaluation passed. No
//! Loom-side step can sit inside GitHub's decision; the direct path has the
//! same decide-then-merge interval. `group_authz_tests.rs` pins this as
//! `known_gap_*`, and [`super::authz::INVARIANT_FULLY_DEMONSTRATED`] stays
//! `false`.
//!
//! # Production status
//!
//! Protocol and fake-forge demonstration only. The GitHub adapters (group
//! discovery from `gh-readonly-queue/*` refs, the commit-status write) and
//! the `merge_group` workflow that runs [`group_check`] are not wired yet.

use std::fmt;

use super::authz::{
    evaluate, AuthzDecision, AuthzFacts, DenyReason, Grant, GrantStore, Revocation,
    REQUIRED_CHECK_CONTEXT,
};
use super::mode::MergeMode;
use super::ops;

/// One PR inside a merge group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pr: u32,
    /// The PR head the group was built from.
    pub head: String,
}

/// A merge-group commit and the PRs it would merge, oldest first. The last
/// member is the entry the group was built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeGroup {
    pub commit: String,
    pub members: Vec<Member>,
}

impl MergeGroup {
    #[must_use]
    pub fn contains(&self, pr: u32) -> bool {
        self.members.iter().any(|m| m.pr == pr)
    }
}

/// State of one other required check on the group commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Success,
    Pending,
    Failure,
}

/// What [`group_check`] reports for [`REQUIRED_CHECK_CONTEXT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupConclusion {
    /// Every other required check succeeded and every member is authorized.
    Success,
    /// Not decided yet (blocks the merge). The reason names what it waits on.
    Pending(String),
    /// At least one member is unauthorized, or a needed read failed.
    Failure(Vec<(u32, DenyReason)>),
}

impl GroupConclusion {
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, GroupConclusion::Success)
    }
}

/// Parse a merge-group branch, `gh-readonly-queue/<base>/pr-<N>-<40 hex>`
/// (with or without `refs/heads/`), into the top entry's PR and head.
/// `None` for anything else: an unparseable ref identifies no member, and a
/// caller with no members must fail.
#[must_use]
pub fn parse_queue_ref(r: &str) -> Option<(u32, String)> {
    let r = r.strip_prefix("refs/heads/").unwrap_or(r);
    let rest = r.strip_prefix("gh-readonly-queue/")?;
    let (_base, last) = rest.rsplit_once('/')?;
    let tail = last.strip_prefix("pr-")?;
    let (n, sha) = tail.split_once('-')?;
    let pr = n.parse::<u32>().ok()?;
    (sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| (pr, sha.to_ascii_lowercase()))
}

/// Members of the group built for `top`: every queued entry up to and
/// including it, in queue order. `None` when `top` is not in `queue`.
#[must_use]
pub fn members_for(top: u32, queue: &[Member]) -> Option<Vec<Member>> {
    let idx = queue.iter().position(|m| m.pr == top)?;
    Some(queue[..=idx].to_vec())
}

/// The member's grant, or why it does not authorize `m.head`. Reads only
/// the grant store, never the live PR state.
fn member_grant(store: &dyn GrantStore, m: &Member) -> Result<Grant, DenyReason> {
    let grant = match store.get(m.pr) {
        Ok(Some(g)) => g,
        Ok(None) => return Err(DenyReason::NoGrant),
        Err(e) => return Err(DenyReason::StoreUnavailable(e.0)),
    };
    if !grant.approved_sha.eq_ignore_ascii_case(&m.head) {
        return Err(DenyReason::GrantForOtherHead {
            granted: grant.approved_sha,
            actual: m.head.clone(),
        });
    }
    Ok(grant)
}

fn member_denials(
    store: &dyn GrantStore,
    m: &Member,
    facts_for: &dyn Fn(u32) -> Result<AuthzFacts, String>,
) -> Vec<DenyReason> {
    let grant = match member_grant(store, m) {
        Ok(g) => g,
        Err(r) => return vec![r],
    };
    match facts_for(m.pr) {
        Err(e) => vec![DenyReason::StoreUnavailable(e)],
        Ok(f) => match evaluate(&f, &grant.approved_sha) {
            AuthzDecision::Authorized => Vec::new(),
            AuthzDecision::Denied(why) => why,
        },
    }
}

/// Body of the required check for one merge-group commit.
///
/// `others` are the *other* required contexts on the group commit (an `Err`
/// is an outage). `facts_for` reads one PR's live facts. Order: the other
/// checks are read first. While any is not yet successful (or they are
/// unreadable) `facts_for` is **not called** and the result is
/// [`GroupConclusion::Pending`], so an outage of the live-facts read (or of
/// the grant store) never concludes early. The one exception is a definite
/// grant denial (no grant, or a grant for another head), which needs no
/// live read and fails at once (fail fast). Only once every other check
/// has succeeded are the members' facts read and the check concluded, so
/// the decision is as late as Loom can make it. An unknown at that point
/// is a `Failure`, never a pass.
#[must_use]
pub fn group_check(
    store: &dyn GrantStore,
    group: &MergeGroup,
    others: Result<Vec<(String, CheckState)>, String>,
    facts_for: &dyn Fn(u32) -> Result<AuthzFacts, String>,
) -> GroupConclusion {
    if group.members.is_empty() {
        return GroupConclusion::Failure(vec![(
            0,
            DenyReason::Undeterminable("merge-group membership"),
        )]);
    }
    let waiting = match others {
        Err(e) => Some(format!("other required checks unreadable: {e}")),
        Ok(list) => {
            let not_done: Vec<String> = list
                .iter()
                .filter(|(ctx, _)| ctx != REQUIRED_CHECK_CONTEXT)
                .filter(|(_, s)| *s != CheckState::Success)
                .map(|(ctx, s)| format!("{ctx}={s:?}"))
                .collect();
            (!not_done.is_empty()).then(|| format!("waiting on {}", not_done.join(", ")))
        }
    };
    if let Some(w) = waiting {
        // Others not done: no live-facts read. Fail fast only on a definite
        // grant denial; a grant-store outage waits like any other unknown.
        let denied: Vec<(u32, DenyReason)> = group
            .members
            .iter()
            .filter_map(|m| match member_grant(store, m) {
                Err(DenyReason::StoreUnavailable(_)) | Ok(_) => None,
                Err(r) => Some((m.pr, r)),
            })
            .collect();
        if denied.is_empty() {
            return GroupConclusion::Pending(w);
        }
        return GroupConclusion::Failure(denied);
    }
    let denied: Vec<(u32, DenyReason)> = group
        .members
        .iter()
        .flat_map(|m| {
            member_denials(store, m, facts_for)
                .into_iter()
                .map(move |r| (m.pr, r))
        })
        .collect();
    if denied.is_empty() {
        GroupConclusion::Success
    } else {
        GroupConclusion::Failure(denied)
    }
}

/// Commit-status state Loom writes for [`REQUIRED_CHECK_CONTEXT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusState {
    Success,
    Pending,
    Failure,
}

/// The commit-status write. GitHub keeps the newest status per context, so a
/// `failure` posted after a `success` replaces it.
pub trait StatusApi {
    /// # Errors
    ///
    /// The status was not confirmed written.
    fn post_status(
        &self,
        commit: &str,
        state: StatusState,
        description: &str,
    ) -> Result<(), String>;
}

/// Map a conclusion to the status Loom posts.
#[must_use]
pub fn status_for(c: &GroupConclusion) -> (StatusState, String) {
    match c {
        GroupConclusion::Success => (StatusState::Success, "authorized".to_string()),
        GroupConclusion::Pending(w) => (StatusState::Pending, w.clone()),
        GroupConclusion::Failure(why) => (
            StatusState::Failure,
            why.iter()
                .map(|(pr, r)| format!("#{pr}: {r}"))
                .collect::<Vec<_>>()
                .join("; "),
        ),
    }
}

/// Result of [`revoke_refail_dequeue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRevocation {
    pub revocation: Revocation,
    /// Group commits re-failed, or why that could not be confirmed (group
    /// discovery failed, or a status write failed).
    pub refailed: Result<Vec<String>, String>,
}

impl fmt::Display for GroupRevocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.refailed {
            Ok(c) => write!(f, "re-failed {} merge-group commit(s)", c.len()),
            Err(e) => write!(f, "merge-group re-fail NOT confirmed ({e})"),
        }
    }
}

impl GroupRevocation {
    /// May the Loom-owned transition proceed? Same rule as
    /// [`Revocation::safe_to_transition`]: future evaluations deny.
    #[must_use]
    pub fn safe_to_transition(&self) -> bool {
        self.revocation.safe_to_transition()
    }

    /// Is an already-passed authorization withdrawn everywhere the PR could
    /// still merge? True when every live group containing it was confirmed
    /// re-failed (zero groups counts). False means a green group may still
    /// merge it: the residual window, which the caller must report.
    #[must_use]
    pub fn passed_checks_withdrawn(&self) -> bool {
        self.refailed.is_ok()
    }

    #[must_use]
    pub fn already_merged(&self) -> bool {
        self.revocation.already_merged()
    }
}

/// Revoke the grant, re-fail every live merge-group commit that contains
/// `pr`, then dequeue. Every step is attempted regardless of the others;
/// all are idempotent, so duplicate events are safe.
///
/// Re-failing needs no execution gate: it only ever withdraws authority. The
/// dequeue goes through [`ops::guarded_dequeue`] as before.
pub fn revoke_refail_dequeue(
    mode: MergeMode,
    execution_enabled: bool,
    api: &dyn ops::QueueApi,
    store: &dyn GrantStore,
    statuses: &dyn StatusApi,
    groups: &dyn Fn() -> Result<Vec<MergeGroup>, String>,
    pr: u32,
) -> GroupRevocation {
    let grant_revoked = store.revoke(pr);
    let refailed = groups().and_then(|gs| {
        let desc = format!("authorization for #{pr} revoked");
        let mut done = Vec::new();
        let mut errs = Vec::new();
        for g in gs.iter().filter(|g| g.contains(pr)) {
            match statuses.post_status(&g.commit, StatusState::Failure, &desc) {
                Ok(()) => done.push(g.commit.clone()),
                Err(e) => errs.push(format!("{}: {e}", g.commit)),
            }
        }
        if errs.is_empty() {
            Ok(done)
        } else {
            Err(errs.join("; "))
        }
    });
    let dequeue = ops::guarded_dequeue(mode, execution_enabled, api, pr);
    GroupRevocation {
        revocation: Revocation {
            grant_revoked,
            dequeue,
        },
        refailed,
    }
}
