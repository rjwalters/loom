//! Stalled-head handling for the merge-sequencing pass (#10060).
//!
//! A sequencing chain's head can stop moving for reasons no agent can clear:
//! a `loom:operator` merge-risk hold waiting on a human decision, or a fork PR
//! whose CI never ran (`action_required`) so Judge cannot issue a verdict.
//! Before #10060 every approved PR ordered behind such a head waited for the
//! 72 h soft-hold expiry, and nothing told the operator that one human action
//! on the head was what held them.
//!
//! This module supplies three pure pieces and one write:
//!
//! - [`stall_cause`]: a PR is a **stalled head** when it has been quiet (its
//!   `updatedAt` has not moved) for [`stall_hours`] AND it is either on a
//!   [`super::super::VERDICT_HOLD_LABELS`] hold or carries no `loom:pr`
//!   verdict. Unknown freshness is never stalled.
//! - [`hold_action_with_stall`]: a SOFT (`source=pass`) hold on an APPROVED
//!   follower whose in-flight predecessor is a stalled head is released
//!   ([`HoldAction::ReleaseStalled`]), so ready work lands first; the head is
//!   rebased afterwards, as it already needs to be. Hard holds (no `source=`,
//!   the human-authored shape) never release here, and consolidation
//!   reservations (`cons-` plans, ADR-0023 §1) keep their own 72 h contract.
//! - [`stalled_for_ordering`]: stalled no-verdict PRs sort LAST among the
//!   placeable members of a component, so a released follower is not
//!   re-planned straight back behind the head on the next tick.
//! - [`escalate`]: one "operator needed" comment per stalled chain, on the
//!   head PR, naming the action it needs and the approved PRs it holds. The
//!   dedupe marker is the star-liveness escalation marker (#9244,
//!   [`crate::star_liveness::escalate::marker`]) with a `sequence-stall:` key,
//!   read through the same trusted-comment filter, so it is posted once per
//!   cause across ticks, hosts and restarts.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::{
    evaluate, HoldAction, KeepReason, PredecessorState, SequenceMarker, SequencePr, Verdict,
};
use crate::merge_pr::sequence::fetch_trusted_bodies;
use crate::star_liveness::escalate::marker as escalation_marker;
use crate::work_finder::operator_priority::OPERATOR_PRIORITY_LABEL;

/// How long a chain head may sit quiet on a human hold, or without a
/// verdict, before soft holds behind it are released and the chain is
/// escalated. Deliberately well below
/// [`super::MERGE_SEQUENCE_MAX_AGE_ENV`]'s 72 h general expiry.
pub const MERGE_SEQUENCE_STALL_ENV: &str = "LOOM_MERGE_SEQUENCE_STALL_HOURS";
const DEFAULT_STALL_HOURS: f64 = 12.0;

/// The verdict label an approved PR carries.
const APPROVED_LABEL: &str = "loom:pr";

/// Plan-id prefix of consolidation reservations (ADR-0023 §1). Their release
/// contract is the 72 h expiry only; the stall release never touches them.
pub const CONSOLIDATION_PLAN_PREFIX: &str = "cons-";

/// The stall bound, in hours. See [`MERGE_SEQUENCE_STALL_ENV`].
#[must_use]
pub fn stall_hours() -> f64 {
    std::env::var(MERGE_SEQUENCE_STALL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|h| *h > 0.0)
        .unwrap_or(DEFAULT_STALL_HOURS)
}

/// Is this PR starred (`loom:operator-priority`)?
#[must_use]
pub fn starred(pr: &SequencePr) -> bool {
    pr.has(OPERATOR_PRIORITY_LABEL)
}

/// Why a chain head is stalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallCause {
    /// The head carries this verdict-hold label (`loom:operator` is the
    /// Champion merge-risk hold): a human must rule.
    HumanHold(String),
    /// The head has no `loom:pr` verdict (fork CI `action_required`, or a
    /// review that never came).
    NoVerdict,
}

/// Hours since `pr`'s last activity at `now`; `None` when unparseable.
#[must_use]
pub fn quiet_hours(pr: &SequencePr, now: DateTime<Utc>) -> Option<f64> {
    let then = DateTime::parse_from_rfc3339(&pr.updated_at).ok()?;
    Some((now.timestamp_millis() - then.timestamp_millis()) as f64 / 3_600_000.0)
}

/// The stall cause for `pr` at `now`, or `None` when it is not stalled.
#[must_use]
pub fn stall_cause(pr: &SequencePr, now: DateTime<Utc>, bound_hours: f64) -> Option<StallCause> {
    if !quiet_hours(pr, now).is_some_and(|q| q > bound_hours) {
        return None;
    }
    if let Some(label) = super::super::VERDICT_HOLD_LABELS.iter().find(|l| pr.has(l)) {
        return Some(StallCause::HumanHold((*label).to_string()));
    }
    (!pr.has(APPROVED_LABEL)).then_some(StallCause::NoVerdict)
}

/// The members a component orders LAST among the placeable: stalled PRs
/// without a verdict. (Human-held PRs are already ineligible for ordering.)
#[must_use]
pub fn stalled_for_ordering(
    open: &[SequencePr],
    now: DateTime<Utc>,
    bound_hours: f64,
) -> BTreeSet<u32> {
    open.iter()
        .filter(|p| stall_cause(p, now, bound_hours) == Some(StallCause::NoVerdict))
        .map(|p| p.number)
        .collect()
}

/// [`super::hold_action`], plus the stalled-head release: a soft hold that
/// would stay ([`HoldAction::HoldSoft`]) on an approved follower whose
/// predecessor was read open at the recorded head AND is a stalled head
/// becomes [`HoldAction::ReleaseStalled`]. Every other decision —
/// including every hard hold and every consolidation reservation — is
/// returned unchanged, so a chain with no stalled head behaves exactly as
/// before.
#[must_use]
pub fn hold_action_with_stall(
    marker: &SequenceMarker,
    pred: Option<&PredecessorState>,
    follower_head: Option<&str>,
    follower_approved: bool,
    max_age_hours: f64,
    head_cause: Option<&StallCause>,
) -> HoldAction {
    let action = super::hold_action(marker, pred, follower_head, follower_approved, max_age_hours);
    let in_flight = pred.zip(follower_head).is_some_and(|(p, fh)| {
        matches!(evaluate(marker, p, fh), Verdict::Keep(KeepReason::InFlight))
    });
    if action == HoldAction::HoldSoft
        && follower_approved
        && in_flight
        && head_cause.is_some()
        && !marker.plan.starts_with(CONSOLIDATION_PLAN_PREFIX)
    {
        HoldAction::ReleaseStalled
    } else {
        action
    }
}

/// One stalled chain, as Phase 1 saw it this tick.
#[derive(Debug, Clone, PartialEq)]
pub struct StalledChain {
    pub head: u32,
    pub head_sha: String,
    pub cause: StallCause,
    pub quiet_hours: f64,
    /// Approved followers whose soft holds were released this tick.
    pub released: Vec<u32>,
    /// Approved followers still held (hard holds — never auto-released).
    pub held: Vec<u32>,
}

impl StalledChain {
    /// The dedupe key: stable for one cause (head number + head SHA — any
    /// push to the head voids its followers' holds anyway).
    #[must_use]
    pub fn key(&self) -> String {
        let sha = self.head_sha.get(..12).unwrap_or(&self.head_sha);
        format!("sequence-stall:{}:{sha}", self.head)
    }

    /// Whether this chain warrants an operator escalation. Something must
    /// still be held, or the head must be on a human hold. A no-verdict head
    /// whose soft followers were all released is re-planned instead: a
    /// comment on it would move its `updatedAt` and pull the released
    /// followers straight back behind it on the next tick.
    #[must_use]
    pub fn needs_escalation(&self) -> bool {
        !self.held.is_empty() || matches!(self.cause, StallCause::HumanHold(_))
    }

    /// What the head needs from a human.
    #[must_use]
    pub fn action(&self) -> String {
        match &self.cause {
            StallCause::HumanHold(l) if l == "loom:operator" => format!(
                "make the merge decision on #{} (it carries `loom:operator`, a merge-risk hold): \
                 merge it, close it, or release the hold",
                self.head
            ),
            StallCause::HumanHold(l) => {
                format!("clear #{}'s `{l}` hold, or close it", self.head)
            }
            StallCause::NoVerdict => format!(
                "get #{} a Judge verdict: if its CI shows `action_required` (fork-workflow \
                 approval outstanding), approve the workflow runs so CI and review can run",
                self.head
            ),
        }
    }

    /// The comment body, starting with the shared escalation marker.
    #[must_use]
    pub fn comment_body(&self, bound_hours: f64) -> String {
        let list = |v: &[u32]| {
            v.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !self.held.is_empty() {
            parts.push(format!(
                "Still held behind it: {} (Judge-approved; their `loom:sequenced` holds are \
                 human-authored, so they never release automatically).",
                list(&self.held)
            ));
        }
        if !self.released.is_empty() {
            parts.push(format!(
                "Released this tick: {} (Judge-approved soft holds; they may land first and \
                 #{} is rebased afterwards).",
                list(&self.released),
                self.head
            ));
        }
        format!(
            "{}\n**Operator needed** — merge-sequencing chain stalled at this PR.\n\n\
             #{} has had no activity for {:.0}h (stall bound {:.0}h) and approved work is \
             ordered behind it. Action needed: {}.\n\n{}\n\n\
             <sub>Posted once per cause by the loom-daemon merge-sequencing pass (#10060). \
             Resolving the cause clears it; nothing to acknowledge here.</sub>",
            escalation_marker(&self.key()),
            self.head,
            self.quiet_hours,
            bound_hours,
            self.action(),
            parts.join("\n\n")
        )
    }
}

/// Collects Phase 1's stalled-chain observations, one entry per head.
#[derive(Debug, Default)]
pub struct StallLedger {
    pub chains: Vec<StalledChain>,
}

impl StallLedger {
    /// Record an approved follower of stalled `head`: `released` when its
    /// soft hold was released this tick, otherwise still held (hard).
    pub fn record(
        &mut self,
        head: &SequencePr,
        cause: &StallCause,
        quiet: f64,
        follower: u32,
        released: bool,
    ) {
        let Some(head_sha) = head.head_sha.clone() else {
            return;
        };
        let idx = match self.chains.iter().position(|c| c.head == head.number) {
            Some(i) => i,
            None => {
                self.chains.push(StalledChain {
                    head: head.number,
                    head_sha,
                    cause: cause.clone(),
                    quiet_hours: quiet,
                    released: Vec::new(),
                    held: Vec::new(),
                });
                self.chains.len() - 1
            }
        };
        let c = &mut self.chains[idx];
        let list = if released {
            &mut c.released
        } else {
            &mut c.held
        };
        if !list.contains(&follower) {
            list.push(follower);
        }
    }
}

/// Whether a trusted comment among `bodies` already carries `chain`'s
/// escalation marker — the cross-host / cross-restart dedupe.
#[must_use]
pub fn already_escalated(bodies: &[String], chain: &StalledChain) -> bool {
    let m = escalation_marker(&chain.key());
    bodies.iter().any(|b| b.contains(&m))
}

/// Post the escalation for `chain` on its head PR unless already there.
/// `Ok(true)` when posted. A failed comment read never posts (fail toward
/// silence over duplicates; the next tick retries).
pub(super) fn escalate(
    gh_bin: &Path,
    root: &Path,
    chain: &StalledChain,
    bound_hours: f64,
) -> anyhow::Result<bool> {
    let bin = gh_bin.to_string_lossy().to_string();
    let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", chain.head) else {
        anyhow::bail!("could not read trusted comments on PR #{}", chain.head);
    };
    if already_escalated(&bodies, chain) {
        return Ok(false);
    }
    let n = chain.head.to_string();
    super::gh_pr(gh_bin, root, &["comment", &n, "--body", &chain.comment_body(bound_hours)])?;
    Ok(true)
}

#[cfg(test)]
#[path = "merge_sequence_stall_tests.rs"]
mod tests;
