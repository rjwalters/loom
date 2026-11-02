//! Queue-drop classification and routing (#10256, Phase B2).
//!
//! When GitHub removes a PR from the merge queue, the only evidence of why is
//! the `RemovedFromMergeQueueEvent.reason` text on the timeline — a free
//! `String` with no published vocabulary. So:
//!
//! - **Only explicit phrases classify.** A reason is `Conflict`,
//!   `CheckFailure`, `Timeout` or `HeadChanged` only when GitHub's own text
//!   says so ([`classify`]); GitHub's snake_case tokens (`checks_timed_out`,
//!   `merge_conflict`, `failed_checks`, `manual`) are matched too. Anything
//!   else — including no reason or an empty reason — is [`RemovalKind::Unknown`], reported as
//!   unknown with the raw text quoted, never guessed.
//! - **Routing reuses the existing handlers** ([`route`]): a conflict or a
//!   failed check goes to Doctor the way a direct-mode conflict or stale PR
//!   does (`loom:pr` → `loom:changes-requested`); a timeout re-queues on the
//!   next pass like `merge-pr.sh` exit 5; a head change is left to the
//!   verdict-staleness janitor, which owns head moves; an unknown reason is
//!   a human decision (`loom:operator`).

use super::forge::{GroupRun, APPROVED_LABEL, CHANGES_REQUESTED_LABEL, OPERATOR_LABEL};

/// Why GitHub removed the PR, as far as its own text says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalKind {
    Conflict,
    CheckFailure,
    /// A `failed_checks` drop whose every failed group check was
    /// infrastructure (#12573); never produced by [`classify`].
    DroppedInfra,
    Timeout,
    HeadChanged,
    /// An operator's own dequeue (GitHub's `manual` token); no human decision
    /// is pending, so it is not routed to `loom:operator`.
    Manual,
    /// GitHub cleared the whole queue (`queue_cleared`): a queue-wide event,
    /// not a verdict on this PR (e.g. an org-wide plan downgrade making the
    /// rules unreadable).
    QueueCleared,
    Unknown,
}

impl RemovalKind {
    /// Stable token (comment marker, telemetry, CLI sentinel).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RemovalKind::Conflict => "conflict",
            RemovalKind::CheckFailure => "check-failure",
            RemovalKind::DroppedInfra => "dropped-infra",
            RemovalKind::Timeout => "timeout",
            RemovalKind::HeadChanged => "head-changed",
            RemovalKind::Manual => "manual",
            RemovalKind::QueueCleared => "queue-cleared",
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
    // GitHub reports snake_case tokens (`checks_timed_out`, `merge_conflict`);
    // normalise so they match the prose needles below.
    let t = text.to_ascii_lowercase().replace('_', " ");
    let any = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    if t == "manual" {
        RemovalKind::Manual
    } else if t == "queue cleared" {
        RemovalKind::QueueCleared
    } else if any(&["merge conflict", "conflicts with", "conflicting"]) {
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

/// True only when the group commit's failures are all infrastructure: at
/// least one run failed, every failed run ended `cancelled` or `timed_out`,
/// and none had a step that ran and failed. A run still in progress, any other
/// conclusion, or no failure at all is not infrastructure (fail closed).
#[must_use]
pub fn infra_only(runs: &[GroupRun]) -> bool {
    let mut failed = 0;
    for r in runs {
        match r.conclusion.as_deref() {
            Some("success" | "neutral" | "skipped") => {}
            Some("cancelled" | "timed_out") if r.failed_steps == 0 => failed += 1,
            _ => return false,
        }
    }
    failed > 0
}

/// Names of the runs that did not succeed, for the revoke comment.
fn non_success_names(runs: &[GroupRun], pred: impl Fn(&GroupRun) -> bool) -> String {
    runs.iter()
        .filter(|r| pred(r))
        .map(|r| format!("`{}`", r.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Detail line for a `dropped-infra` revoke: the cancelled/timed-out jobs.
#[must_use]
pub fn infra_detail(runs: &[GroupRun]) -> String {
    let names = non_success_names(runs, |r| {
        matches!(r.conclusion.as_deref(), Some("cancelled" | "timed_out"))
    });
    format!("Cancelled or timed-out group job(s): {names}.")
}

/// Why [`infra_only`] declined a group that had cancelled/timed-out runs, or
/// `None` when no run was cancelled (a plain failure; nothing to explain).
#[must_use]
pub fn infra_decline_reason(runs: &[GroupRun]) -> Option<String> {
    let is_infra =
        |r: &GroupRun| matches!(r.conclusion.as_deref(), Some("cancelled" | "timed_out"));
    if !runs.iter().any(is_infra) {
        return None;
    }
    let cancelled = non_success_names(runs, is_infra);
    let step_failed = non_success_names(runs, |r| is_infra(r) && r.failed_steps > 0);
    let real = non_success_names(runs, |r| {
        matches!(r.conclusion.as_deref(), Some(c) if !is_infra(r)
            && !matches!(c, "success" | "neutral" | "skipped"))
    });
    let pending = non_success_names(runs, |r| r.conclusion.is_none());
    let why = if !step_failed.is_empty() {
        format!("a step of {step_failed} ended `failure`, so it is not a runner loss")
    } else if !real.is_empty() {
        format!("{real} ended with a real failure")
    } else if !pending.is_empty() {
        format!("{pending} was still running")
    } else {
        "the group's check runs were not infrastructure-only".to_string()
    };
    Some(format!("Infra refinement declined (cancelled/timed-out: {cancelled}): {why}."))
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
        RemovalKind::Timeout | RemovalKind::DroppedInfra => Route {
            add: Vec::new(),
            remove: Vec::new(),
            handler: "Not a failure: `loom:pr` is kept and the next Champion pass re-runs every \
                      guard and re-enqueues with a fresh authorization, like `merge-pr.sh` exit 5. \
                      (For `dropped-infra`: every failed group check was cancelled or timed out \
                      with no failing step, so this is infrastructure, retried once per head.)",
        },
        RemovalKind::Manual => Route {
            add: Vec::new(),
            remove: Vec::new(),
            handler: "Labels unchanged: the PR was dequeued manually, so no human decision is \
                      pending; the next Champion pass decides whether to re-enqueue.",
        },
        RemovalKind::QueueCleared => Route {
            add: Vec::new(),
            remove: Vec::new(),
            handler: "Not a verdict on this PR: GitHub cleared the whole merge queue, so \
                      `loom:pr` is kept and the next Champion pass re-checks the current head \
                      and re-enqueues it as usual.",
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
pub fn drop_prose(
    kind: RemovalKind,
    raw: Option<&str>,
    removed_at: Option<&str>,
    detail: Option<&str>,
) -> String {
    let said = match raw.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("GitHub's removal reason, verbatim: \"{}\".", t.replace('"', "'")),
        None => "GitHub recorded no removal reason.".to_string(),
    };
    let when = removed_at.map_or_else(String::new, |w| format!(" at {w}"));
    let detail = detail.map_or_else(String::new, |d| format!("\n\n{d}"));
    format!(
        "GitHub removed this PR from the merge queue{when}. {said} Classified as **{}**{}.{detail}\n\n{}",
        kind.as_str(),
        if kind == RemovalKind::Unknown {
            " (unknown: not one of the reasons Loom recognises)"
        } else {
            ""
        },
        route(kind).handler
    )
}
