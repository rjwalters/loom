//! Proactive base-conflict signal for PRs waiting in the review queue
//! (issue #8922).
//!
//! Before this pass, `loom:merge-conflict` was only ever applied *beside* a
//! Judge verdict (judge.md's "If DIRTY" fallback) or stripped by
//! [`super::forge::reconcile_pr_verdicts`] as a per-tree companion of a stale
//! verdict. Nothing applied it to a `loom:review-requested` PR whose **base**
//! moved underneath it, so a PR that could not land looked exactly like one
//! that could (#8909 sat `CONFLICTING` for ~25 minutes carrying only
//! `loom:review-requested`). A Judge that claimed it reviewed a tree that could
//! not merge; the rebase that followed moved the head, the verdict went stale,
//! and the PR made a full lap for nothing.
//!
//! # What the pass does, per tick
//!
//! - **Flag.** An open `loom:review-requested` PR GitHub reports as
//!   `mergeable=CONFLICTING` gets the exact transition Judge applies by hand
//!   when it finds a DIRTY PR: `loom:review-requested` → `loom:changes-requested`
//!   + `loom:merge-conflict`. That routes it to Doctor and, because Judge's
//!   find-work query is `--label loom:review-requested`, Judge no longer claims
//!   it. The comment written first carries both [`BASE_CONFLICT_MARKER`] and a
//!   `loom:verdict-sha … verdict=changes-requested` marker for the current head,
//!   so the stale-verdict pass reads the new `loom:changes-requested` as
//!   `Fresh` (never anchors it) and clears it on its own the moment a Doctor
//!   rebase moves the head.
//! - **Clear.** A PR carrying `loom:merge-conflict` (and not
//!   `loom:review-requested`) that GitHub now reports `MERGEABLE` is returned to
//!   `loom:review-requested` — but **only when this pass is the one that put it
//!   there**: the newest comment carrying any of [`BASE_CONFLICT_MARKER`],
//!   [`BASE_CONFLICT_CLEARED_MARKER`], or a verdict-sha marker must be our flag.
//!   A Judge's own DIRTY fallback (or any later real verdict) is never undone
//!   by this pass; Doctor owns those.
//!
//! # What it never does
//!
//! - `mergeable=UNKNOWN` (GitHub computes mergeability lazily and reports
//!   `UNKNOWN` while recomputing) is **no information**: no label changes in
//!   either direction; the next tick re-reads it. Same rule as
//!   `champion-held-pr-staleness.md` (#8552).
//! - A PR carrying [`VERDICT_HOLD_LABELS`] is never touched.
//! - A PR with an agent mid-flight on it (`loom:reviewing` / `loom:treating`)
//!   is left to that agent — Judge's own DIRTY path handles a conflict it
//!   discovers mid-review, and relabeling under it would race its verdict write.
//!
//! Kill switch: [`REVIEW_CONFLICT_ENABLED_ENV`] (default ON), nested inside the
//! master `LOOM_STALE_CLAIM_RECONCILE` switch like the verdict pass.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use super::{forge, MAX_ISSUES_PER_WORKSPACE, VERDICT_HOLD_LABELS, VERDICT_MARKER_PREFIX};

/// Kill switch for this pass (`0`/`false`/`no`/`off` disables). Defaults ON.
pub const REVIEW_CONFLICT_ENABLED_ENV: &str = "LOOM_REVIEW_CONFLICT_RECONCILE";

/// Stamped into the comment that flags a review-queue PR as base-conflicting.
pub const BASE_CONFLICT_MARKER: &str = "<!-- loom:base-conflict flagged -->";

/// Stamped into the comment that returns a formerly-conflicting PR to review.
pub const BASE_CONFLICT_CLEARED_MARKER: &str = "<!-- loom:base-conflict cleared -->";

const REVIEW_REQUESTED: &str = "loom:review-requested";
const CHANGES_REQUESTED: &str = "loom:changes-requested";
const MERGE_CONFLICT: &str = "loom:merge-conflict";

/// An agent is actively working the PR — leave it to them.
const IN_FLIGHT_LABELS: [&str; 2] = ["loom:reviewing", "loom:treating"];

/// Is this pass enabled? See [`REVIEW_CONFLICT_ENABLED_ENV`].
#[must_use]
pub fn review_conflict_enabled() -> bool {
    match std::env::var(REVIEW_CONFLICT_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// GitHub's `mergeable` field. Anything other than the two definite answers
/// (including a missing field) is [`Mergeable::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

impl Mergeable {
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("MERGEABLE") => Self::Mergeable,
            Some("CONFLICTING") => Self::Conflicting,
            _ => Self::Unknown,
        }
    }
}

/// An open PR from either listing, trimmed to what [`decide_review_conflict`]
/// needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictPr {
    pub number: u32,
    pub head_sha: Option<String>,
    pub mergeable: Mergeable,
    pub labels: Vec<String>,
}

impl ConflictPr {
    fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// Why [`decide_review_conflict`] left a PR alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKeepReason {
    /// `mergeable=UNKNOWN` — no information; re-read next tick.
    Unknown,
    /// Carries a [`VERDICT_HOLD_LABELS`] hold.
    Held,
    /// `loom:reviewing` / `loom:treating` — an agent owns it right now.
    InFlight,
    /// Conflicting, but no resolvable head SHA to record the verdict against.
    NoHeadSha,
    /// Nothing to change (clean review-queue PR, a conflict already flagged,
    /// or a PR outside this pass's label set).
    NoChange,
}

/// The decision for one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    /// Move `loom:review-requested` → `loom:changes-requested` +
    /// `loom:merge-conflict`, recording `head_sha`.
    Flag {
        head_sha: String,
    },
    /// Mergeable again and carrying `loom:merge-conflict`: return it to review
    /// **if** [`flag_is_latest`] confirms this pass flagged it.
    ClearIfOurs,
    Keep(ConflictKeepReason),
}

/// Pure decision function — no I/O.
#[must_use]
pub fn decide_review_conflict(pr: &ConflictPr) -> ConflictAction {
    if VERDICT_HOLD_LABELS.iter().any(|l| pr.has(l)) {
        return ConflictAction::Keep(ConflictKeepReason::Held);
    }
    if IN_FLIGHT_LABELS.iter().any(|l| pr.has(l)) {
        return ConflictAction::Keep(ConflictKeepReason::InFlight);
    }
    match pr.mergeable {
        Mergeable::Unknown => ConflictAction::Keep(ConflictKeepReason::Unknown),
        Mergeable::Conflicting if pr.has(REVIEW_REQUESTED) => match pr.head_sha.as_deref() {
            Some(sha) if !sha.is_empty() => ConflictAction::Flag {
                head_sha: sha.to_string(),
            },
            _ => ConflictAction::Keep(ConflictKeepReason::NoHeadSha),
        },
        Mergeable::Mergeable if pr.has(MERGE_CONFLICT) && !pr.has(REVIEW_REQUESTED) => {
            ConflictAction::ClearIfOurs
        }
        _ => ConflictAction::Keep(ConflictKeepReason::NoChange),
    }
}

/// Did THIS pass put the PR in its current conflict state? Scans `bodies`
/// (oldest first) newest-first for the first comment carrying any
/// state-changing marker; `true` only if that comment is our flag. A Judge
/// verdict (verdict-sha marker without our flag), or our own cleared marker,
/// means the current labels are not ours to undo.
#[must_use]
pub fn flag_is_latest(bodies: &[String]) -> bool {
    bodies
        .iter()
        .rev()
        .find(|b| {
            b.contains(BASE_CONFLICT_MARKER)
                || b.contains(BASE_CONFLICT_CLEARED_MARKER)
                || b.contains(VERDICT_MARKER_PREFIX)
        })
        .is_some_and(|b| b.contains(BASE_CONFLICT_MARKER))
}

/// The flag comment. Carries a changes-requested verdict-sha marker so the
/// stale-verdict pass treats the new label as a fresh verdict on `head_sha`.
#[must_use]
pub fn flag_comment_body(head_sha: &str) -> String {
    format!(
        "{BASE_CONFLICT_MARKER}\n\
         {VERDICT_MARKER_PREFIX}{head_sha} verdict=changes-requested -->\n\
         **Merge conflict with the base branch — moved out of the review queue**\n\n\
         GitHub reports this PR as `mergeable=CONFLICTING` at head `{head_sha}`: the base \
         branch moved and this tree no longer merges cleanly. Reviewing it now would review a \
         tree that cannot land, and the rebase that must follow would invalidate that review \
         anyway.\n\n\
         - Removed: `{REVIEW_REQUESTED}`\n\
         - Added: `{CHANGES_REQUESTED}` + `{MERGE_CONFLICT}` (routes to Doctor)\n\n\
         This implies no judgment about the code. Rebase onto the base branch and resolve the \
         conflicts; a head move re-queues it for review automatically, and if the conflict \
         clears without one (e.g. the conflicting change is reverted) this pass returns it to \
         `{REVIEW_REQUESTED}` itself.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#8922)*"
    )
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhConflictPr {
    number: u32,
    #[serde(rename = "headRefOid", default)]
    head_ref_oid: Option<String>,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

/// Parse one `gh pr list --json number,headRefOid,mergeable,labels` payload.
///
/// # Errors
/// Malformed JSON.
pub fn parse_pr_list(stdout: &[u8]) -> Result<Vec<ConflictPr>> {
    let rows: Vec<GhConflictPr> =
        serde_json::from_slice(stdout).context("parse gh pr list JSON")?;
    Ok(rows
        .into_iter()
        .map(|r| ConflictPr {
            number: r.number,
            head_sha: r.head_ref_oid,
            mergeable: Mergeable::parse(r.mergeable.as_deref()),
            labels: r.labels.into_iter().map(|l| l.name).collect(),
        })
        .collect())
}

/// Run `gh pr <args…>` in `root` with the per-root credential and `LOOM_REPO`
/// applied, returning stdout on success.
fn gh_pr(gh_bin: &Path, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr").args(args);
    cmd.current_dir(root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr {} failed in {}: {}",
            args.first().copied().unwrap_or_default(),
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn list_prs(gh_bin: &Path, root: &Path, label: &str) -> Result<Vec<ConflictPr>> {
    let limit = MAX_ISSUES_PER_WORKSPACE.to_string();
    let stdout = gh_pr(
        gh_bin,
        root,
        &[
            "list",
            "--state",
            "open",
            "--label",
            label,
            "--limit",
            &limit,
            "--json",
            "number,headRefOid,mergeable,labels",
        ],
    )?;
    parse_pr_list(&stdout)
}

/// Comment first, then relabel — a failed comment aborts before any label is
/// touched, so the transition never happens without its audit trail.
fn flag(gh_bin: &Path, root: &Path, number: u32, head_sha: &str) -> Result<()> {
    let n = number.to_string();
    let body = flag_comment_body(head_sha);
    gh_pr(gh_bin, root, &["comment", &n, "--body", &body])?;
    gh_pr(
        gh_bin,
        root,
        &[
            "edit",
            &n,
            "--remove-label",
            REVIEW_REQUESTED,
            "--add-label",
            CHANGES_REQUESTED,
            "--add-label",
            MERGE_CONFLICT,
        ],
    )?;
    Ok(())
}

/// Relabel first, then comment — the cleared marker must not land unless the
/// labels actually moved, or the next tick would read it as "not ours" and
/// strand the PR.
fn clear(gh_bin: &Path, root: &Path, number: u32) -> Result<()> {
    let n = number.to_string();
    gh_pr(
        gh_bin,
        root,
        &[
            "edit",
            &n,
            "--remove-label",
            MERGE_CONFLICT,
            "--remove-label",
            CHANGES_REQUESTED,
            "--add-label",
            REVIEW_REQUESTED,
        ],
    )?;
    let body = format!(
        "{BASE_CONFLICT_CLEARED_MARKER}\n\
         **Base conflict resolved — returned to the review queue**\n\n\
         GitHub now reports this PR as `mergeable=MERGEABLE`. The `{MERGE_CONFLICT}` / \
         `{CHANGES_REQUESTED}` labels this pass applied are removed and `{REVIEW_REQUESTED}` is \
         restored.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#8922)*"
    );
    gh_pr(gh_bin, root, &["comment", &n, "--body", &body])?;
    Ok(())
}

/// Counters for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReviewConflictStats {
    pub checked: usize,
    pub flagged: usize,
    pub cleared: usize,
}

/// Run the pass over one workspace `root`. Best effort: any `gh` failure is
/// logged at `warn` and contributes nothing.
pub fn reconcile_review_conflicts(gh_bin: &Path, root: &Path) -> ReviewConflictStats {
    let mut stats = ReviewConflictStats::default();
    if !review_conflict_enabled() {
        return stats;
    }
    // A PR carrying both labels appears in both listings; key by number.
    let mut prs: BTreeMap<u32, ConflictPr> = BTreeMap::new();
    for label in [REVIEW_REQUESTED, MERGE_CONFLICT] {
        match list_prs(gh_bin, root, label) {
            Ok(v) => prs.extend(v.into_iter().map(|p| (p.number, p))),
            Err(e) => {
                log::warn!("claim_reconciliation (review conflicts): {}: {e}", root.display());
                crate::rate_limit_breaker::global_observe_failure(
                    &e.to_string(),
                    "claim_reconciliation",
                );
            }
        }
    }
    stats.checked = prs.len();

    for pr in prs.values() {
        match decide_review_conflict(pr) {
            ConflictAction::Flag { head_sha } => match flag(gh_bin, root, pr.number, &head_sha) {
                Ok(()) => {
                    stats.flagged += 1;
                    log::warn!(
                        "claim_reconciliation: PR #{} in {} is CONFLICTING with its base at \
                         {head_sha} — moved from loom:review-requested to \
                         loom:changes-requested + loom:merge-conflict (#8922)",
                        pr.number,
                        root.display()
                    );
                }
                Err(e) => log::warn!(
                    "claim_reconciliation: failed to flag base conflict on PR #{} in {}: {e}",
                    pr.number,
                    root.display()
                ),
            },
            ConflictAction::ClearIfOurs => {
                // A failed fetch is "unknown", never "ours".
                let ours = forge::fetch_comment_bodies(gh_bin, root, pr.number)
                    .is_some_and(|bodies| flag_is_latest(&bodies));
                if !ours {
                    continue;
                }
                match clear(gh_bin, root, pr.number) {
                    Ok(()) => {
                        stats.cleared += 1;
                        log::info!(
                            "claim_reconciliation: PR #{} in {} is MERGEABLE again — returned to \
                             loom:review-requested (#8922)",
                            pr.number,
                            root.display()
                        );
                    }
                    Err(e) => log::warn!(
                        "claim_reconciliation: failed to clear base conflict on PR #{} in {}: {e}",
                        pr.number,
                        root.display()
                    ),
                }
            }
            ConflictAction::Keep(_) => {}
        }
    }
    stats
}

#[cfg(test)]
#[path = "review_conflict_tests.rs"]
mod tests;
