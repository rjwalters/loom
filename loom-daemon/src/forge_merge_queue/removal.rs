//! Queue-drop classification and routing (#10256, Phase B2).
//!
//! When GitHub removes a PR from the merge queue, the only evidence of why is
//! the `RemovedFromMergeQueueEvent.reason` text on the timeline — a free
//! `String` with no published vocabulary. So:
//!
//! - **Only explicit phrases classify.** A reason is `Conflict`,
//!   `CheckFailure`, `Timeout` or `HeadChanged` only when GitHub's own text
//!   says so ([`classify`]). Anything else — including no reason, an empty
//!   reason, or a manual removal — is [`RemovalKind::Unknown`], reported as
//!   unknown with the raw text quoted, never guessed.
//! - **Routing reuses the existing handlers** ([`route`]): a conflict or a
//!   failed check goes to Doctor the way a direct-mode conflict or stale PR
//!   does (`loom:pr` → `loom:changes-requested`); a timeout re-queues on the
//!   next pass like `merge-pr.sh` exit 5; a head change is left to the
//!   verdict-staleness janitor, which owns head moves; an unknown reason is
//!   a human decision (`loom:operator`).

use super::forge::{APPROVED_LABEL, CHANGES_REQUESTED_LABEL, OPERATOR_LABEL};

/// Why GitHub removed the PR, as far as its own text says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalKind {
    Conflict,
    CheckFailure,
    Timeout,
    HeadChanged,
    Unknown,
}

impl RemovalKind {
    /// Stable token (comment marker, telemetry, CLI sentinel).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RemovalKind::Conflict => "conflict",
            RemovalKind::CheckFailure => "check-failure",
            RemovalKind::Timeout => "timeout",
            RemovalKind::HeadChanged => "head-changed",
            RemovalKind::Unknown => "unknown",
        }
    }
}

/// Classify GitHub's removal text. Pure; conservative by construction.
#[must_use]
pub fn classify(reason: Option<&str>) -> RemovalKind {
    let Some(text) = reason.map(str::trim).filter(|t| !t.is_empty()) else {
        return RemovalKind::Unknown;
    };
    let t = text.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    if any(&["merge conflict", "conflicts with", "conflicting"]) {
        RemovalKind::Conflict
    } else if any(&["timed out", "timeout", "time limit"]) {
        RemovalKind::Timeout
    } else if any(&["check"]) && any(&["fail", "error", "cancel"]) {
        RemovalKind::CheckFailure
    } else if any(&["head"]) && any(&["changed", "updated", "pushed", "moved"]) {
        RemovalKind::HeadChanged
    } else {
        RemovalKind::Unknown
    }
}

/// Label transition for a removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub add: Vec<&'static str>,
    pub remove: Vec<&'static str>,
    /// One sentence naming the handler, for the PR comment.
    pub handler: &'static str,
}

/// Route a removal to the existing handler. Pure.
#[must_use]
pub fn route(kind: RemovalKind) -> Route {
    match kind {
        RemovalKind::Conflict => Route {
            add: vec![CHANGES_REQUESTED_LABEL],
            remove: vec![APPROVED_LABEL],
            handler: "Routed to Doctor for a rebase (`loom:pr` → `loom:changes-requested`), the \
                      same route as a direct-mode merge conflict.",
        },
        RemovalKind::CheckFailure => Route {
            add: vec![CHANGES_REQUESTED_LABEL],
            remove: vec![APPROVED_LABEL],
            handler: "Routed to Doctor (`loom:pr` → `loom:changes-requested`): a required check \
                      failed on the combined merge-group tree, the same route as a CI failure.",
        },
        RemovalKind::Timeout => Route {
            add: Vec::new(),
            remove: Vec::new(),
            handler: "Not a failure: `loom:pr` is kept and the next Champion pass re-runs every \
                      guard and re-enqueues with a fresh authorization, like `merge-pr.sh` exit 5.",
        },
        RemovalKind::HeadChanged => Route {
            add: Vec::new(),
            remove: Vec::new(),
            handler: "Labels unchanged: a head move is the verdict-staleness janitor's to \
                      resolve; the revoked authorization cannot be reused for the new head.",
        },
        RemovalKind::Unknown => Route {
            add: vec![OPERATOR_LABEL],
            remove: Vec::new(),
            handler: "The removal reason is not one Loom can verify, so it is not guessed: \
                      `loom:operator` is applied for a human to decide.",
        },
    }
}

/// Explanation posted with the revoke marker for a drop.
#[must_use]
pub fn drop_prose(kind: RemovalKind, raw: Option<&str>, removed_at: Option<&str>) -> String {
    let said = match raw.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("GitHub's removal reason, verbatim: \"{}\".", t.replace('"', "'")),
        None => "GitHub recorded no removal reason.".to_string(),
    };
    let when = removed_at.map_or_else(String::new, |w| format!(" at {w}"));
    format!(
        "GitHub removed this PR from the merge queue{when}. {said} Classified as **{}**{}.\n\n{}",
        kind.as_str(),
        if kind == RemovalKind::Unknown {
            " (unknown: not one of the reasons Loom recognises)"
        } else {
            ""
        },
        route(kind).handler
    )
}
