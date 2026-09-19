//! The `dep-recheck` fingerprint — `VERDICT` + `BLOCKERS` over the PRs that
//! close an issue (epic #7810, PR 4).

use crate::short_hash::short_sha16;
use serde::Deserialize;

/// One `closedByPullRequestsReferences` entry, as `gh pr view` reports it.
///
/// `labels` is a plain string array here: `gh pr view --json labels` returns
/// label *objects*, and the shell normalised them to names once at the fetch
/// boundary so every downstream consumer — and every `--stdin` fixture — could
/// assume this one shape. [`crate::dep_recheck::forge`] does the same.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Pr {
    pub number: i64,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub mergeable: String,
    #[serde(default, rename = "mergeStateStatus")]
    pub merge_state_status: String,
}

/// The `--stdin` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Input {
    pub prs: Vec<Pr>,
}

/// Labels that make an open PR still supersede the blocked verdict.
///
/// Deliberately just these two (#7362). `BLOCKERS` used to fold in a PR's FULL
/// label set, so every incidental transition on an actively-reviewed PR —
/// `loom:pr` ↔ `loom:review-requested` ↔ `loom:reviewing` ↔ `loom:operator` ↔
/// `loom:changes-requested`/`loom:treating`/`loom:ci-failure` — changed
/// `CONCLUSION_HASH`, though none of them changes the answer the re-check
/// reports. That produced 28+ near-duplicate comments on #6805 in 36 hours.
const SUPERSEDING_BLOCK_LABELS: [&str; 2] = ["loom:changes-requested", "loom:blocked"];

impl Pr {
    fn has_superseding_block_label(&self) -> bool {
        self.labels
            .iter()
            .any(|l| SUPERSEDING_BLOCK_LABELS.contains(&l.as_str()))
    }

    /// Whether this PR's merge state counts as conflicting.
    ///
    /// **`UNKNOWN` fails safe to conflicting** (#7281). GitHub reports
    /// `UNKNOWN` while it is still computing mergeability; reading that as "not
    /// conflicting" let a PR blocking purely on merge state appear to clear for
    /// one pass and re-block the next, flipping `VERDICT` with nothing about
    /// the PR actually changing. Treating "we do not know yet" the same as
    /// "still conflicting" is the codebase's standing convention for missing
    /// data, and it is what keeps the hash stable across the flicker.
    fn is_conflicting(&self) -> bool {
        self.mergeable == "CONFLICTING"
            || self.mergeable == "UNKNOWN"
            || self.merge_state_status == "DIRTY"
            || self.merge_state_status == "CONFLICTING"
            || self.merge_state_status == "UNKNOWN"
    }

    /// `<pr#>:<state>:<block-label|no-block-label>:<conflicting|mergeable|n/a>`.
    ///
    /// The merge-state bucket is only evaluated for an **OPEN** PR, mirroring
    /// [`Pr::blocks`]'s existing gate (#8253). Once a PR merges or closes,
    /// GitHub stops computing mergeability and `mergeable`/`mergeStateStatus`
    /// can read back as `UNKNOWN` non-deterministically — which
    /// [`Pr::is_conflicting`]'s fail-safe (correctly) treats as conflicting for
    /// an OPEN PR, but for an already-MERGED/CLOSED PR that reading is
    /// meaningless noise, not a signal. Evaluating it anyway let the
    /// meaningless flicker move `blockers()` — and therefore
    /// `conclusion_hash` — with nothing substantive changed, churning
    /// duplicate `curator:dep-recheck` comments on issues whose linked PR had
    /// already merged.
    fn blocker_line(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.number,
            self.state,
            if self.has_superseding_block_label() {
                "block-label"
            } else {
                "no-block-label"
            },
            if self.state != "OPEN" {
                "n/a"
            } else if self.is_conflicting() {
                "conflicting"
            } else {
                "mergeable"
            }
        )
    }

    /// Whether this PR blocks: OPEN **and** (superseding label or conflicting).
    fn blocks(&self) -> bool {
        self.state == "OPEN" && (self.has_superseding_block_label() || self.is_conflicting())
    }
}

/// What this pass concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub verdict: String,
    pub blockers: String,
    pub block_reason: String,
    pub orthogonal: String,
    pub conclusion_hash: String,
}

/// One line per PR, sorted.
///
/// Sorted twice in the shell (`jq sort_by(.number)` then `| sort`), and the
/// second one wins: it is a **lexicographic** sort of the rendered lines, so
/// `#10` precedes `#9`. Reproduced as-is — the ordering feeds the hash, and
/// "more sensible" numeric ordering would invalidate every persisted marker.
#[must_use]
pub fn blockers(prs: &[Pr]) -> String {
    let mut lines: Vec<String> = prs.iter().map(Pr::blocker_line).collect();
    lines.sort();
    lines.join("\n")
}

/// `blocked` iff any PR blocks.
#[must_use]
pub fn verdict(prs: &[Pr]) -> &'static str {
    if prs.iter().any(Pr::blocks) {
        "blocked"
    } else {
        "clear"
    }
}

/// Canonicalize a caller-supplied free-text hash input: trim, collapse every
/// internal whitespace run to a single space, and casefold.
///
/// **Hash input only** (#8254). `--block-reason` is the documented escape hatch
/// for `curator.md`'s secondary heuristic (no linked PR at all), where the
/// agent passes *its own prose* for the cited justification. Folded verbatim,
/// `doctor cycle exhausted` / `Doctor cycle exhausted` / a stray trailing space
/// were three different `CONCLUSION_HASH` values for one unchanged state — the
/// #557/#298 comment-churn shape, surviving on the one input still open to it
/// after the PR-derived components were made deterministic. `--orthogonal` gets
/// the same treatment: it is a structured identity in practice, but it is typed
/// by an agent too.
///
/// The [`Outcome`] fields keep the **original** text, because `cli.rs` echoes
/// them back as `BLOCK_REASON=`/`ORTHOGONAL=` for `curator.md` to `eval` into
/// the comment it posts. Canonicalizing what is hashed must not flatten what is
/// read by a human.
///
/// An all-whitespace value canonicalizes to the empty string, so it hashes
/// identically to passing nothing at all — which is what it means.
fn canonical_hash_input(s: &str) -> String {
    // `split_whitespace` trims and collapses in one pass, over Unicode
    // whitespace rather than just ASCII.
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Compute the fingerprint.
///
/// `verdict_override` is for the case the script cannot infer: no linked PR at
/// all, where the true verdict comes from `curator.md`'s secondary heuristic
/// rather than from PR state.
#[must_use]
pub fn compute(
    prs: &[Pr],
    verdict_override: Option<&str>,
    block_reason: &str,
    orthogonal: &str,
) -> Outcome {
    let blockers = blockers(prs);
    let verdict = verdict_override
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| verdict(prs))
        .to_string();
    // Canonicalized for the hash, verbatim in the Outcome — see
    // `canonical_hash_input`.
    let reason_key = canonical_hash_input(block_reason);
    let orthogonal_key = canonical_hash_input(orthogonal);
    // `printf '%s\n%s\n%s\n%s'` — newline-separated, no trailing newline.
    let hash = short_sha16(&format!("{verdict}\n{blockers}\n{reason_key}\n{orthogonal_key}"));
    Outcome {
        verdict,
        blockers,
        block_reason: block_reason.to_string(),
        orthogonal: orthogonal.to_string(),
        conclusion_hash: hash,
    }
}

#[cfg(test)]
mod tests;
