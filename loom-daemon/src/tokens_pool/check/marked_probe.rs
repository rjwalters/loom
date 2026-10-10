//! Reporting a standing `.bad_tokens` mark **separately** from a live probe
//! result (issue #8972).
//!
//! Before this module, an account with a standing `auth` /
//! `malformed-timestamp` mark was never probed under *any* `--source`, and its
//! row echoed the stored reason in the same `error` field a live `401` uses.
//! An operator running `tokens check --source probe` precisely to ask "is this
//! mark still true?" got their own stale record back and read it as
//! confirmation.
//!
//! The contract now:
//!
//! - An explicitly resolved `--source probe` (flag or
//!   `LOOM_RANKING_SOURCE=probe`) probes such an account. `--source auto` /
//!   `monitor` and the overdue-reset re-probe keep skipping it — they run
//!   periodically, and probing a revoked credential every cycle is exactly the
//!   cost #7420's idempotence test guards against.
//! - Every row says structurally whether a request was made
//!   ([`AccountResult::probed`]) and carries the standing mark as data
//!   ([`AccountResult::bad_mark`]: class, reason, recorded timestamp).
//! - **Reporting only.** A healthy live probe never clears, rewrites, or
//!   appends to `.bad_tokens` — the selector keeps refusing the account until
//!   `tokens unblock`. So while a permanent mark stands, `status` stays
//!   `blocked` (it describes *selectability*, and is what `.ranking` records),
//!   and the live verdict is reported in [`AccountResult::probe_status`].
//!
//! A self-clearing `exhaustion` mark is unchanged apart from being reported:
//! it was already probed, and its `status` is already the live one (#7522).
//!
//! No value in this module is ever a credential: rows identify accounts by
//! name, and a mark carries only what `.bad_tokens` recorded.

use std::path::Path;

use super::{AccountResult, ProbeReport};
use crate::tokens_pool::bad_tokens::{self, BadReasonClass, BlockingEntry};

/// A `.bad_tokens` entry standing against an account, as a report row carries
/// it. The reporting-side projection of [`BlockingEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadMark {
    /// Auth (permanent) vs exhaustion (TTL) vs malformed timestamp (fail-closed).
    pub class: BadReasonClass,
    /// The reason text as recorded when the mark was written.
    pub reason: String,
    /// The recorded timestamp, **verbatim**: an unparseable first field is
    /// reported as the raw text it was, never coerced into an instant.
    pub marked_at: String,
}

impl From<&BlockingEntry> for BadMark {
    fn from(entry: &BlockingEntry) -> Self {
        Self {
            class: entry.class,
            reason: entry.reason.clone(),
            marked_at: entry.timestamp.clone(),
        }
    }
}

impl BadMark {
    /// The `"<class>: <reason>"` text an unprobed row's `error` has carried
    /// since #6030.
    fn echo(&self) -> String {
        format!("{}: {}", self.class.label(), self.reason)
    }

    /// `recorded <instant>` — or, when the stored first field is not an
    /// instant at all, the raw text, labelled as such.
    fn recorded(&self) -> String {
        let parses =
            chrono::NaiveDateTime::parse_from_str(&self.marked_at, "%Y-%m-%dT%H:%M:%SZ").is_ok();
        if parses {
            format!("recorded {}", self.marked_at)
        } else {
            format!("recorded at an unparseable timestamp {:?}", self.marked_at)
        }
    }

    fn describe(&self) -> String {
        format!("bad-mark {}: {}", self.recorded(), self.echo())
    }
}

/// Attach the standing mark (if any) to a freshly probed row.
///
/// A permanent mark on a row that **was** probed moves the live verdict into
/// `probe_status` and pins `status` to `blocked`: the selector will refuse the
/// account whatever the probe said, and `.ranking` must not advertise it.
pub(super) fn with_standing_mark(
    mut result: AccountResult,
    blocking: Option<&BlockingEntry>,
) -> AccountResult {
    let Some(entry) = blocking else {
        return result;
    };
    if result.probed && entry.class != BadReasonClass::Exhaustion {
        let live = std::mem::replace(&mut result.status, "blocked".to_string());
        result.probe_status = Some(live);
    }
    result.bad_mark = Some(BadMark::from(entry));
    result
}

/// Attach standing marks to rows a monitor-sourced report produced. Data only:
/// no status changes (that report's `.ranking` is already written).
pub(super) fn attach_standing_marks(tokens_dir: &Path, accounts: &mut [AccountResult]) {
    for account in accounts.iter_mut().filter(|a| a.bad_mark.is_none()) {
        account.bad_mark = bad_tokens::blocking_entry_in_dir(tokens_dir, &account.name)
            .as_ref()
            .map(BadMark::from);
    }
}

/// The additive `--json` keys: `probed` on every row; `bad_mark` and
/// `probe_status` only when they apply.
pub(super) fn extend_json(
    row: &AccountResult,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    obj.insert("probed".into(), serde_json::Value::Bool(row.probed));
    if let Some(mark) = &row.bad_mark {
        obj.insert(
            "bad_mark".into(),
            serde_json::json!({
                "class": mark.class.label(),
                "reason": mark.reason,
                "marked_at": mark.marked_at,
            }),
        );
    }
    if let Some(live) = &row.probe_status {
        obj.insert("probe_status".into(), serde_json::Value::String(live.clone()));
    }
}

/// The `.ranking` line for `row`. A row whose `status` is pinned to `blocked`
/// by a standing mark is written bare (`<name>|blocked`), exactly as it was
/// before such accounts were probed: the live utilization describes a
/// credential the selector will not use.
pub(super) fn ranking_row(row: &AccountResult) -> String {
    if row.probe_status.is_some() {
        return super::ranking_line(&row.name, &row.status, None, None);
    }
    super::ranking_line(&row.name, &row.status, row.s5h_utilization, row.limit_reset())
}

/// The table's first line. It only claims "probed at" when a request was made.
pub(super) fn table_header(report: &ProbeReport) -> String {
    if report.accounts.is_empty() || report.accounts.iter().any(|a| a.probed) {
        format!("Token pool ranking (probed at {})", report.ranked_at)
    } else {
        format!(
            "Token pool ranking (ranked at {}; no account was probed in this run)",
            report.ranked_at
        )
    }
}

/// Whether a live verdict shows the credential authenticating (a quota
/// verdict is still an authenticated answer).
fn authenticates(live: &str) -> bool {
    matches!(live, "available" | "rate_limited" | "exhausted")
}

/// The parenthesised note that follows a table row.
///
/// Without a standing mark this is the plain `(<error>)` of #6030. With one,
/// the three cases an operator must be able to tell apart read differently:
/// not probed (a stored record only), probed and confirmed, probed and stale.
pub(super) fn row_note(row: &AccountResult) -> String {
    let Some(mark) = &row.bad_mark else {
        return row
            .error
            .as_ref()
            .map_or_else(String::new, |err| format!("  ({err})"));
    };
    let stored = mark.describe();
    // An error that is merely the stored reason echoed back is not repeated.
    let own_error = row.error.as_deref().filter(|err| *err != mark.echo());
    if !row.probed {
        return match own_error {
            Some(err) => format!("  (not probed: {err} — {stored})"),
            None => format!("  (not probed — {stored})"),
        };
    }
    let Some(live) = &row.probe_status else {
        // A self-clearing mark: `status` is already the live verdict.
        return match own_error {
            Some(err) => format!("  ({err} — {stored})"),
            None => format!("  ({stored})"),
        };
    };
    let verdict = match own_error {
        Some(err) => format!("{live}, {err}"),
        None => live.clone(),
    };
    if authenticates(live) {
        format!(
            "  (probed live: {verdict} — {stored} looks stale; still excluded until \
             `loom-daemon tokens unblock {}`)",
            row.name
        )
    } else if own_error == Some("auth_401") {
        format!("  (probed live: {verdict} — confirms {stored})")
    } else {
        format!("  (probed live: {verdict}, inconclusive — {stored} stands)")
    }
}

#[cfg(test)]
#[path = "marked_probe_tests.rs"]
mod tests;
