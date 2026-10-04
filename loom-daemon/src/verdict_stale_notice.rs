//! The stale-verdict notice, shared by both stale-clear paths (Issue #9709).
//!
//! Two mechanisms clear a stale `loom:pr` / `loom:changes-requested` verdict:
//! the daemon's periodic `claim_reconciliation` pass (in-process) and
//! `defaults/scripts/verdict-staleness-guard.sh --clear` (agent-side, through
//! `loom-daemon forge verdict-stale-notice`). Both render their audit comment
//! with [`body`], so the two wordings cannot drift; they differ only in the
//! attribution footer (`automated_by`), which is how an investigator tells
//! which path posted a given notice (#9709's own forensics relied on it).
//!
//! # Why the notice can name an untrusted author
//!
//! The trust filter ([`crate::comment_trust`], #9548) drops every verdict
//! marker whose author is not trusted, and that is correct. But GitHub's
//! `author_association` reflects only *public* organization membership: a
//! repo admin whose org membership is private reads as `CONTRIBUTOR`, so their
//! fresh approval is dropped and the pass falls back to an OLDER trusted
//! marker. Before #9709 the notice then said "head SHA moved", which on
//! 2AMLogic/klayout-tools#2571 and 2AMLogic/2am#2114 was simply untrue; the
//! wording is what made the gap invisible.
//!
//! So, when a marker for the same verdict kind exists that is **newer** than
//! the newest trusted one and was dropped as untrusted, the notice (and the
//! daemon's log line) says so, names the login and its `author_association`,
//! and points at `forge.trustedCommenters`.
//!
//! **This changes wording only.** Trust is not widened: the dropped marker
//! still counts for nothing, the decision is still made from trusted markers
//! only, and the fail-safe re-queue still happens. [`untrusted_newer_marker`]
//! is attribution, never evidence.

use serde_json::Value;

use crate::claim_reconciliation::{extract_latest_verdict_sha, VerdictKind};
use crate::comment_trust::{Author, TrustPolicy, TRUSTED_COMMENTERS_KEY};

/// The machine-readable first line of every stale-verdict notice, recording
/// WHICH transition it covers (`<!-- loom:verdict-stale from=<m> to=<h> -->`).
/// The dedup (#9124) keys on it, so it is identical in both wordings.
pub const VERDICT_STALE_MARKER_PREFIX: &str = "<!-- loom:verdict-stale from=";

/// The daemon pass's attribution footer source.
pub const DAEMON_SOURCE: &str = "loom-daemon claim reconciliation";

/// The agent-side guard's attribution footer source.
pub const GUARD_SOURCE: &str = "verdict-staleness-guard.sh";

/// A verdict marker newer than the newest trusted one, dropped because its
/// author is not trusted (#9709). Attribution only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedVerdictMarker {
    /// The author's login as the forge spelled it (`ghost` for a deleted account).
    pub login: String,
    /// The forge-reported `author_association` (`unknown` when absent).
    pub association: String,
    /// The SHA the dropped marker records.
    pub sha: String,
}

/// The verdict kind a terminal verdict label names, if it names one.
#[must_use]
pub fn kind_for_label(label: &str) -> Option<VerdictKind> {
    [VerdictKind::Approved, VerdictKind::ChangesRequested]
        .into_iter()
        .find(|k| k.label() == label.trim())
}

/// The `verdict_kind` marker SHA a comment object carries, if any.
fn marker_sha(v: &Value, kind: VerdictKind) -> Option<String> {
    let body = v.get("body").and_then(Value::as_str)?;
    extract_latest_verdict_sha(std::slice::from_ref(&body.to_string()), kind)
}

/// The newest `kind` marker in `items` (a REST or `gh --json` comment listing,
/// oldest first) that sits AFTER the newest trusted `kind` marker, was dropped
/// by `policy`, and records a different SHA from that trusted marker. `None`
/// when there is no such marker — the ordinary "the head really moved" case.
#[must_use]
pub fn untrusted_newer_marker(
    policy: &TrustPolicy,
    items: &[Value],
    kind: VerdictKind,
) -> Option<UntrustedVerdictMarker> {
    let trusted_idx = items
        .iter()
        .rposition(|v| policy.trusts_json(v) && marker_sha(v, kind).is_some());
    let trusted_sha = trusted_idx.and_then(|i| marker_sha(&items[i], kind));
    let start = trusted_idx.map_or(0, |i| i + 1);
    items[start..].iter().rev().find_map(|v| {
        if policy.trusts_json(v) {
            return None;
        }
        let sha = marker_sha(v, kind)?;
        if trusted_sha.as_deref() == Some(sha.as_str()) {
            return None;
        }
        let author = Author::from_json(v);
        Some(UntrustedVerdictMarker {
            login: author.login.unwrap_or_else(|| "ghost".to_string()),
            association: author.association.unwrap_or_else(|| "unknown".to_string()),
            sha,
        })
    })
}

/// The stale-verdict audit comment.
///
/// `disarm_line` is pre-formatted (leading newline included) or empty.
/// `untrusted` switches to the #9709 wording; `None` is the plain head-moved
/// wording, byte-for-byte what it was before #9709. `automated_by` is the
/// footer source ([`DAEMON_SOURCE`] / [`GUARD_SOURCE`]).
#[must_use]
pub fn body(
    label: &str,
    marker_sha: &str,
    head_sha: &str,
    disarm_line: &str,
    untrusted: Option<&UntrustedVerdictMarker>,
    automated_by: &str,
) -> String {
    let Some(u) = untrusted else {
        return format!(
            "{VERDICT_STALE_MARKER_PREFIX}{marker_sha} to={head_sha} -->\n\
             **Stale review verdict cleared — head SHA moved**\n\n\
             This PR's `{label}` verdict was rendered against `{marker_sha}`, but the current \
             head is `{head_sha}`. A review verdict is a statement about a specific tree, so it \
             does not survive a rebase, a force-push, or new commits.\n\n\
             - Verdict cleared: `{label}` (recorded for `{marker_sha}`)\n\
             - Returned to the review queue: `loom:review-requested` (current head `{head_sha}`)\
             {disarm_line}\n\n\
             Judge will re-evaluate the tree that is actually here now. No judgment about the new \
             tree is implied either way — the old verdict simply no longer describes it.\n\n\
             ---\n\
             *Automated by {automated_by} (#5686)*"
        );
    };
    let (login, assoc, sha) = (&u.login, &u.association, &u.sha);
    let at_head = if !sha.is_empty() && head_sha.starts_with(sha.as_str()) {
        " — which IS the current head"
    } else {
        ""
    };
    format!(
        "{VERDICT_STALE_MARKER_PREFIX}{marker_sha} to={head_sha} -->\n\
         **Stale review verdict cleared — a newer verdict marker from `{login}` was ignored as \
         untrusted**\n\n\
         This PR's `{label}` verdict was cleared because the newest verdict marker from a \
         **trusted** author records `{marker_sha}`, and the current head is `{head_sha}`. A \
         **newer** `{label}` marker exists — for `{sha}`{at_head} — posted by `{login}` \
         (`author_association={assoc}`), but Loom ignored it: a verdict marker counts only from a \
         trusted author, so to this pass that newer verdict does not exist.\n\n\
         - Verdict cleared: `{label}` (trusted marker recorded for `{marker_sha}`)\n\
         - Ignored as untrusted: marker for `{sha}` by `{login}` (`author_association={assoc}`)\n\
         - Returned to the review queue: `loom:review-requested` (current head `{head_sha}`)\
         {disarm_line}\n\n\
         **If `{login}` is a reviewer whose verdicts should count**, add that login to \
         `{TRUSTED_COMMENTERS_KEY}` in `.loom/config.json`, then re-apply the verdict. GitHub \
         reports a repo admin whose organization membership is *private* as `CONTRIBUTOR`, so \
         such an admin is untrusted until listed there (see `.loom/docs/comment-trust.md`). If \
         `{login}` is not such a reviewer, nothing is wrong: this is the trust filter working as \
         intended, and Judge will re-evaluate the tree that is here now.\n\n\
         ---\n\
         *Automated by {automated_by} (#5686, #9709)*"
    )
}

/// A log-line suffix naming the untrusted author, or empty (#9709).
#[must_use]
pub fn log_note(untrusted: Option<&UntrustedVerdictMarker>) -> String {
    untrusted.map_or_else(String::new, |u| {
        format!(
            "; NOTE a newer verdict marker for {} by `{}` (author_association={}) was IGNORED as \
             untrusted — if that login is a trusted reviewer, add it to {TRUSTED_COMMENTERS_KEY} \
             (#9709)",
            u.sha, u.login, u.association
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
