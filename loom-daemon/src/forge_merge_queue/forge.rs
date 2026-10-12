//! The forge reads/writes the queue lifecycle needs beyond the queue API
//! (#10256, Phase B2 of #9978), and the derivation of [`AuthzFacts`] from
//! them.
//!
//! [`LifecycleForge`] is the seam: [`super::gh_lifecycle::GhLifecycleForge`]
//! is the only implementation that talks to GitHub, and the lifecycle tests
//! drive a fake. Every method answers or fails; none returns a permissive
//! default on failure.

use super::authz::{AuthzFacts, Fact};
use super::ops::PrState;
use crate::claim_reconciliation::{extract_latest_verdict_sha, VerdictKind, VERDICT_HOLD_LABELS};

/// Judge's approval label.
pub const APPROVED_LABEL: &str = "loom:pr";
/// Rework routed to Doctor (the existing conflict / CI-failure handler).
pub const CHANGES_REQUESTED_LABEL: &str = "loom:changes-requested";
/// "A human is needed" — the existing hold for an unexplained removal.
pub const OPERATOR_LABEL: &str = "loom:operator";
/// A Judge review claim or a Doctor treating claim is in progress.
pub const CLAIM_LABELS: [&str; 2] = ["loom:reviewing", "loom:treating"];
/// Present next to `loom:pr`, these contradict the approval.
pub const CONTRADICTION_LABELS: [&str; 2] = ["loom:changes-requested", "loom:review-requested"];

/// The PR fields the lifecycle decides on, from one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: u32,
    pub state: PrState,
    pub head_sha: String,
    pub labels: Vec<String>,
    /// RFC 3339; `Some` only for a merged PR.
    pub merged_at: Option<String>,
}

/// One `RemovedFromMergeQueueEvent` from the PR timeline (schema verified by
/// read-only introspection on 2026-10-06: `reason` is a free `String`, not an
/// enum, so it is classified conservatively — see [`super::removal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalEvent {
    /// GitHub's text, verbatim. `None` when GitHub gave none.
    pub reason: Option<String>,
    /// RFC 3339.
    pub created_at: String,
}

/// Forge operations outside the queue API. Implementations must not put a
/// credential in any error text.
pub trait LifecycleForge {
    /// # Errors
    ///
    /// The PR could not be read.
    fn snapshot(&self, pr: u32) -> Result<PrSnapshot, String>;
    /// Bodies of comments by **trusted** authors only, oldest first
    /// (`comment_trust`: an outsider's well-formed marker is prose).
    ///
    /// # Errors
    ///
    /// The listing could not be read or filtered.
    fn trusted_comments(&self, pr: u32) -> Result<Vec<String>, String>;
    /// # Errors
    ///
    /// The comment was not confirmed posted.
    fn post_comment(&self, pr: u32, body: &str) -> Result<(), String>;
    /// # Errors
    ///
    /// Any label write failed.
    fn edit_labels(&self, pr: u32, add: &[&str], remove: &[&str]) -> Result<(), String>;
    /// Queue removals on the PR timeline, oldest first.
    ///
    /// # Errors
    ///
    /// The timeline could not be read.
    fn removals(&self, pr: u32) -> Result<Vec<RemovalEvent>, String>;
}

fn has(labels: &[String], wanted: &[&str]) -> bool {
    labels.iter().any(|l| wanted.contains(&l.trim()))
}

/// Derive the live authorization facts from one snapshot and the trusted
/// comment bodies. Pure.
///
/// `verdict_current` requires the newest trusted `verdict=approved` marker
/// to name the current head (a 7–40 hex prefix, as Judge and the equivalence
/// re-anchor write it). No marker is `Unknown`, which denies: the direct
/// path's "unverifiable approval stands" fallback (#9548) is deliberately
/// not inherited, because a queued merge happens without Loom present.
#[must_use]
pub fn facts_from(snap: &PrSnapshot, bodies: &[String]) -> AuthzFacts {
    let verdict_current = match extract_latest_verdict_sha(bodies, VerdictKind::Approved) {
        Some(marker) if marker.len() >= 7 => Fact::Known(
            snap.head_sha
                .to_ascii_lowercase()
                .starts_with(&marker.to_ascii_lowercase()),
        ),
        Some(_) | None => Fact::Unknown("no trusted approved verdict-sha marker".to_string()),
    };
    AuthzFacts {
        head_sha: if snap.head_sha.is_empty() {
            Fact::Unknown("empty head sha".to_string())
        } else {
            Fact::Known(snap.head_sha.clone())
        },
        approved_label: Fact::Known(has(&snap.labels, &[APPROVED_LABEL])),
        verdict_current,
        reviewing_claim: Fact::Known(has(&snap.labels, &CLAIM_LABELS)),
        human_hold: Fact::Known(has(&snap.labels, &VERDICT_HOLD_LABELS)),
        contradiction: Fact::Known(has(&snap.labels, &CONTRADICTION_LABELS)),
    }
}

/// Read the live facts for `pr`. Any read failure is an `Err` (an outage),
/// which every caller treats as a denial.
///
/// # Errors
///
/// The snapshot or the comment listing could not be read.
pub fn read_facts(forge: &dyn LifecycleForge, pr: u32) -> Result<AuthzFacts, String> {
    let snap = forge.snapshot(pr)?;
    if snap.state != PrState::Open {
        return Err(format!("PR #{pr} is {}", snap.state.as_str()));
    }
    let bodies = forge.trusted_comments(pr)?;
    Ok(facts_from(&snap, &bodies))
}
