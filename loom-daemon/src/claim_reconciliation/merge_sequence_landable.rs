//! "Landable now": live CI and mergeability feed [`super::ready::ready`]
//! (#12010).
//!
//! Readiness was read from labels only, and nothing labels a PR that is
//! already Judge-approved and later goes red or conflicting: the Judge's
//! CI-failure label is applied during a review, never afterwards. On
//! 2026-10-15 an all-green approved PR was sequenced behind an approved PR
//! with six red checks on its head, and stayed held until an operator
//! released it by hand.
//!
//! [`tag_unlandable`] probes every [`super::ready::ready`], pinnable row and
//! tags it in memory (never written to the forge, like
//! [`super::ready::NOT_LANDABLE_LABEL`]) with [`CI_RED_LABEL`] or
//! [`CONFLICTING_LABEL`]. Both are in [`super::ready::NOT_READY_LABELS`], so
//! the existing rules apply unchanged: ready-first ordering, no new hold
//! behind the PR, release of a soft hold behind it, transitivity, and the
//! stacked-base exemption.
//!
//! Only a DEFINITE negative tags: a completed `failure` / `timed_out` check
//! run on the current head, or GitHub's own `mergeable: false`. Pending CI
//! (every re-dated head for ~10-20 min) and anything unknown leave the row as
//! it was, so a re-date does not churn holds. `cancelled` is not red:
//! superseded twin runs and concurrency cancels conclude that way.
//!
//! Cost: at most two conditional REST reads per ready PR per tick, only in
//! repos past the pass's trigger, none while the rate-limit breaker is
//! suppressing. The check-runs read shares the on-disk ETag store that
//! `forge wait-checks`, the merge guard and the agent `gh pr checks` front
//! use, and mergeability is the review-conflict pass's ETag'd `pulls/{n}`, so
//! an unchanged head answers with free `304`s.

use std::path::Path;

use serde_json::Value;

use super::super::open_pr_listing::mergeable_of;
use super::super::review_conflict::Mergeable;
use super::{SequencePr, TRIGGER_OPEN_PRS};
use crate::forge_wait_checks::reads::GhReads;
use crate::rate_limit_breaker::report::{BreakerHandle, FailureContext, ProbeMode};

/// In-memory only: a definite check failure on the PR's current head.
pub const CI_RED_LABEL: &str = "loom-internal:ci-red";

/// In-memory only: GitHub reports the PR conflicts with its base.
pub const CONFLICTING_LABEL: &str = "loom-internal:conflicting";

/// In-memory only: one per failing check name, beside [`CI_RED_LABEL`], so
/// the release comment can name them.
pub const RED_CHECK_PREFIX: &str = "loom-internal:red-check:";

/// Forge-call accounting caller for the check-runs read.
const CI_CALLER: &str = "claim_reconciliation.sequence_ci";

/// The CI state of one head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ci {
    /// At least one completed check run failed or timed out (sorted names).
    Red(Vec<String>),
    Green,
    Pending,
    Unknown,
}

/// What one probe learned about a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landability {
    pub ci: Ci,
    pub mergeable: Mergeable,
}

impl Landability {
    /// No information: the row is left exactly as the listing shows it.
    pub const UNKNOWN: Self = Self {
        ci: Ci::Unknown,
        mergeable: Mergeable::Unknown,
    };
}

/// Tag every ready, pinnable row the probe finds red or conflicting. Rows
/// that are not ready are never probed (their CI cannot change the answer),
/// and a repo at or under the pass's trigger is never probed at all.
/// Non-mutating; returns the tagged copy.
#[must_use]
pub fn tag_unlandable(
    open: &[SequencePr],
    mut probe: impl FnMut(&SequencePr) -> Landability,
) -> Vec<SequencePr> {
    if open.len() <= TRIGGER_OPEN_PRS {
        return open.to_vec();
    }
    open.iter()
        .map(|p| {
            let mut out = p.clone();
            if !(super::ready::ready(p) && p.pinnable()) {
                return out;
            }
            let found = probe(p);
            if let Ci::Red(names) = found.ci {
                out.labels.push(CI_RED_LABEL.to_string());
                out.labels
                    .extend(names.iter().map(|n| format!("{RED_CHECK_PREFIX}{n}")));
            }
            if found.mergeable == Mergeable::Conflicting {
                out.labels.push(CONFLICTING_LABEL.to_string());
            }
            out
        })
        .collect()
}

/// `probe`, skipped (answering [`Landability::UNKNOWN`] with no call) while
/// `suppressed()` says the rate-limit breaker is holding reads off.
pub fn guarded(
    suppressed: impl Fn() -> bool,
    mut probe: impl FnMut(&SequencePr) -> Landability,
) -> impl FnMut(&SequencePr) -> Landability {
    move |p| {
        if suppressed() {
            Landability::UNKNOWN
        } else {
            probe(p)
        }
    }
}

/// Classify a `{check_runs: [...]}` document for one head.
#[must_use]
pub fn ci_of(runs: &Value) -> Ci {
    let Some(rows) = runs.get("check_runs").and_then(Value::as_array) else {
        return Ci::Unknown;
    };
    let field = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut red: Vec<String> = rows
        .iter()
        .filter(|r| matches!(field(r, "conclusion").as_str(), "failure" | "timed_out"))
        .map(|r| field(r, "name"))
        .collect();
    red.sort();
    red.dedup();
    if !red.is_empty() {
        Ci::Red(red)
    } else if rows.is_empty() {
        Ci::Unknown
    } else if rows.iter().any(|r| field(r, "status") != "completed") {
        Ci::Pending
    } else {
        Ci::Green
    }
}

/// The concrete cause for a release comment, when `head` was tagged here.
#[must_use]
pub fn cause(head: &SequencePr) -> Option<String> {
    if head.has(CI_RED_LABEL) {
        let names: Vec<&str> = head
            .labels
            .iter()
            .filter_map(|l| l.strip_prefix(RED_CHECK_PREFIX))
            .collect();
        let sha: String = head
            .head_sha
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(9)
            .collect();
        let shown = names.iter().take(6).copied().collect::<Vec<_>>().join(", ");
        let more = if names.len() > 6 { ", …" } else { "" };
        return Some(format!("red checks on its head `{sha}` ({shown}{more})"));
    }
    head.has(CONFLICTING_LABEL)
        .then(|| format!("it conflicts with its base `{}`", head.base_ref))
}

/// [`tag_unlandable`] with the live forge probe for `root`. A host that does
/// not own the workspace (`planning == false`) only evaluates existing holds,
/// so with no hold in the listing it reads nothing (#10634's silence).
#[must_use]
pub fn tag_live(
    gh_bin: &Path,
    root: &Path,
    open: &[SequencePr],
    planning: bool,
) -> Vec<SequencePr> {
    if !planning && !open.iter().any(|p| p.has(super::SEQUENCE_LABEL)) {
        return open.to_vec();
    }
    let breaker = BreakerHandle::global(ProbeMode::Inline);
    let mut reads: Option<Option<GhReads>> = None;
    let report = |why: &str| {
        if let Some(h) = breaker.as_ref() {
            let ctx = FailureContext::for_root(root, gh_bin.to_string_lossy());
            h.report(why, "claim_reconciliation", ctx);
        }
    };
    let live = |pr: &SequencePr| {
        let sha = pr.head_sha.as_deref().unwrap_or_default();
        let ci = match reads.get_or_insert_with(|| ci_reads(gh_bin, root)) {
            Some(r) => r.check_runs(sha).map_or_else(
                |e| {
                    log::debug!("merge sequence: CI of PR #{} unknown: {}", pr.number, e.reason());
                    report(e.reason());
                    Ci::Unknown
                },
                |v| ci_of(&v),
            ),
            None => Ci::Unknown,
        };
        if matches!(ci, Ci::Red(_)) {
            return Landability {
                ci,
                mergeable: Mergeable::Unknown,
            };
        }
        // A mergeability computed for another head says nothing about this one.
        let (m, at) = mergeable_of(gh_bin, root, pr.number, breaker.as_ref());
        let mergeable = if at.as_deref().is_some_and(|h| h != sha) {
            Mergeable::Unknown
        } else {
            m
        };
        Landability { ci, mergeable }
    };
    let suppressed = || breaker.as_ref().is_some_and(BreakerHandle::is_suppressed);
    tag_unlandable(open, guarded(suppressed, live))
}

/// The conditional check-runs reader, or `None` when the repo cannot be
/// resolved (every probe then answers [`Ci::Unknown`]).
fn ci_reads(gh_bin: &Path, root: &Path) -> Option<GhReads> {
    let repo = std::env::var("LOOM_REPO").ok().filter(|s| !s.is_empty());
    let store = crate::forge_etag_store::daemon_store_dir();
    let reads = GhReads::new(
        gh_bin.to_path_buf(),
        Some(root.to_path_buf()),
        store.clone().unwrap_or_default(),
        repo.as_deref(),
    )
    .map_err(|e| log::debug!("merge sequence: no CI reads for {}: {e}", root.display()))
    .ok()?
    .with_caller(CI_CALLER);
    Some(if store.is_some() {
        reads
    } else {
        reads.ephemeral()
    })
}

#[cfg(test)]
#[path = "merge_sequence_landable_tests.rs"]
mod tests;
