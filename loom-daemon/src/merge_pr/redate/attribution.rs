//! Why a re-date happened, recorded on the re-date commit itself (#9746).
//!
//! 52% of merged PRs paid a second full CI cycle to the #8248/#8919 freshness
//! guard on 2026-09-30, and nothing recorded *which* check and *which* paths
//! forced each re-date — so narrowing the guard's coupled inputs (#9748) would
//! have been guesswork. This module turns the guard's verdict into git
//! trailers on the re-date commit's BODY:
//!
//! ```text
//! Stale-Check: Structural Checks (Role Prompt Prefix Ratchet)
//! Stale-Clause: the base move and this PR both touch this check's coupled inputs
//! Coupled-Base-Path: CLAUDE.md
//! Coupled-PR-Path: defaults/docs/eta.md
//! ```
//!
//! The subject line is untouched — [`super::is_redate_commit_subject`] matches
//! it exactly — and `loom-daemon merge-pr redate-report` reads the trailers
//! back out of `git log`. Re-date commits reach `main` because this repo merges
//! with merge commits (verified 2026-09-30: 62 re-date commits on
//! `origin/main` in 24h), so the trailers survive; a squash merge would drop
//! them, which the report documents rather than papers over.
//!
//! # Never a new failure mode
//!
//! The verdict is recomputed through the same fetch/assess path
//! `merge-pr stale-checks` uses. Every way that can fail — the PR's base ref
//! unreadable, the evidence fetch failing, the verdict now `Fresh`/`Unknown` —
//! yields `None`, and the remedy then writes today's generic body unchanged.
//! Attribution is telemetry; the re-date itself must never wait on it.

use crate::merge_pr::stale_checks::{assess_scoped, fetch, Verdict};

/// The trailer keys, in the order they are written.
pub const TRAILER_CHECK: &str = "Stale-Check";
pub const TRAILER_CLAUSE: &str = "Stale-Clause";
pub const TRAILER_BASE_PATH: &str = "Coupled-Base-Path";
pub const TRAILER_PR_PATH: &str = "Coupled-PR-Path";

/// The value written for an absent path (time-rule verdicts, and clauses that
/// name only one side).
pub const NONE: &str = "none";

/// The `Stale-Clause` value for the #8248 `started_at` time rule, which has a
/// check but no clause and no paths.
pub const TIME_RULE_CLAUSE: &str = "time rule (#8248 started_at fallback)";

/// Longest trailer value kept, in chars — a path or check name longer than this
/// is truncated rather than letting forge-controlled text bloat the commit.
const MAX_VALUE_CHARS: usize = 300;

/// The attribution one re-date commit carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// The required check context the guard refused on (composite components
    /// are named as `<context> (<component>)`, exactly as the refusal names
    /// them).
    pub check: String,
    /// Which #8919 clause fired, or [`TIME_RULE_CLAUSE`].
    pub clause: String,
    /// `D`'s participating path, when the clause names one.
    pub base_path: Option<String>,
    /// `P`'s participating path, when the clause names one.
    pub pr_path: Option<String>,
}

impl Attribution {
    /// The attribution a stale verdict carries; `None` for a verdict that is
    /// not a refusal on stale evidence (`Fresh`, `Unknown`).
    #[must_use]
    pub fn from_verdict(verdict: &Verdict) -> Option<Self> {
        match verdict {
            Verdict::StaleInputs { check, reason, .. } => Some(Self {
                check: check.clone(),
                clause: reason.clause.to_string(),
                base_path: reason.base_path.clone(),
                pr_path: reason.pr_path.clone(),
            }),
            Verdict::Stale { check, .. } => Some(Self {
                check: check.clone(),
                clause: TIME_RULE_CLAUSE.to_string(),
                base_path: None,
                pr_path: None,
            }),
            Verdict::Fresh | Verdict::Unknown(_) => None,
        }
    }

    /// The trailer block: one `Key: value` line per trailer, every value
    /// [`sanitize`]d, no trailing newline.
    #[must_use]
    pub fn trailers(&self) -> String {
        let path = |p: &Option<String>| p.as_deref().map_or_else(|| NONE.to_string(), sanitize);
        [
            format!("{TRAILER_CHECK}: {}", sanitize(&self.check)),
            format!("{TRAILER_CLAUSE}: {}", sanitize(&self.clause)),
            format!("{TRAILER_BASE_PATH}: {}", path(&self.base_path)),
            format!("{TRAILER_PR_PATH}: {}", path(&self.pr_path)),
        ]
        .join("\n")
    }
}

/// A trailer value that cannot break out of its own line.
///
/// Check names and paths come from forge/PR content, so a crafted path such as
/// `"a.md\nStale-Check: forged"` must not become a second trailer. Every
/// control character (newlines, CR, tabs, NUL, …) becomes a space, runs of
/// whitespace collapse to one, the result is trimmed and length-capped, and an
/// empty result reads as [`NONE`].
#[must_use]
pub fn sanitize(value: &str) -> String {
    let flattened: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(MAX_VALUE_CHARS).collect();
    if capped.is_empty() {
        NONE.to_string()
    } else {
        capped
    }
}

/// Recompute the guard's verdict for `pr` at `head_sha` and turn it into an
/// attribution, via the live forge.
///
/// # Errors
/// A reason string whenever no attribution can be produced; the caller falls
/// back to the generic body.
pub fn recompute(nwo: &str, pr: &str, head_sha: &str) -> Result<Attribution, String> {
    recompute_with(&super::gh_bin(), nwo, pr, head_sha)
}

/// [`recompute`], parameterized on the `gh` binary (the tests' seam).
///
/// # Errors
/// See [`recompute`].
pub fn recompute_with(
    gh: &str,
    nwo: &str,
    pr: &str,
    head_sha: &str,
) -> Result<Attribution, String> {
    let base_ref =
        super::gh_api_with(gh, &[&format!("repos/{nwo}/pulls/{pr}"), "--jq", ".base.ref"])
            .map_err(|e| format!("could not read PR #{pr}'s base ref: {e}"))?;
    if base_ref.is_empty() {
        return Err(format!("PR #{pr}'s base ref resolved empty"));
    }
    let inputs = fetch::live_inputs_with(gh, nwo, pr, &base_ref, head_sha)?;
    let (verdict, _warnings) =
        assess_scoped(inputs.base_tip, &inputs.required, &inputs.runs, inputs.scoped.as_ref());
    Attribution::from_verdict(&verdict).ok_or_else(|| match verdict {
        Verdict::Unknown(why) => format!("the recomputed verdict is undeterminable: {why}"),
        _ => "the recomputed verdict is no longer stale".to_string(),
    })
}
