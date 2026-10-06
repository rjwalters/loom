//! Conditional, ETag-revalidated issue reads for the 2.5 closed-issue and 2.7
//! park-label dispatch guards (W9 of the forge API reduction plan).
//!
//! # Why
//!
//! Each dispatch attempt read `repos/{o}/{r}/issues/{n}` unconditionally up to
//! twice: 2.5 for state and PR-ness (`guard.issue_state`, reader-first) and 2.7
//! for labels (`guard.issue_labels`, pinned to the writer by W4-C). The
//! candidates that repeat are the ones a guard refused. The same issue is
//! re-attempted at every backoff step on every host, and the issue itself
//! rarely changes between attempts, yet each repeat paid in full.
//!
//! # What
//!
//! Both reads become conditional GETs through
//! [`crate::forge_etag_store::cached_read_pinned`]. Each has its own entry
//! (`guard-issue-` / `guard-labels-`) and keeps the identity it had:
//!
//! - 2.5 stays reader-first, like `guard.issue_state`;
//! - 2.7 stays on the writer, like `guard.issue_labels` (W4-C), so it still
//!   sees this daemon's own writes.
//!
//! A `304` serves the stored body, and the guard decides on that **body**
//! (state, PR-ness, labels), never on "nothing changed". A `304` says the
//! forge's current representation is the stored one, so the decision is the
//! one a `200` would give. Nothing here is a memo: no answer outlives the
//! read that produced it.
//!
//! # After our own write
//!
//! For [`OWN_WRITE_PIN`] after this process wrote issue N of the repo through
//! the `gh` facade ([`crate::gh_invocation::own_writes`], hooked once in
//! `GhInvocation::execute` rather than per write site), both reads of N are
//! sent **without** `If-None-Match`, so a lagging replica's `304` cannot
//! answer them. Each keeps its identity ([`guard_read_pin`]): 2.7 is on the
//! writer anyway (W4-C), and 2.5 stays on its reader. Moving 2.5 to the
//! writer would add writer-bucket spend, which W9 exists to cut, and 2.5
//! reads state and PR-ness, which the daemon's own label writes do not
//! change.
//!
//! # Fail-open
//!
//! Any failure (breaker, spawn error, timeout, a non-200/304 answer, a `404`,
//! an unparseable body, an unresolvable repo) returns `None`. The caller then
//! runs today's unconditional probe ([`SweepRegistry::issue_is_closed_or_pr`] /
//! [`SweepRegistry::current_labels_via_rest`]), so a failure costs at most
//! one extra call and never changes a verdict.
//! [`GUARD_ISSUE_SNAPSHOT_ENV`]`=0` restores the unconditional reads exactly.

use super::*;
use crate::forge_etag_store::{self as store, ConditionalRead, ReadPin};

/// Env kill switch for the conditional guard reads. `0`/`false`/`no`/`off`
/// restores the unconditional `guard.issue_state` / `guard.issue_labels`
/// reads. Defaults on.
pub const GUARD_ISSUE_SNAPSHOT_ENV: &str = "LOOM_GUARD_ISSUE_SNAPSHOT";

/// How long after this process wrote an issue its guard reads stay
/// unconditional. A replica lags a write by seconds; ten minutes is a wide
/// margin.
pub(crate) const OWN_WRITE_PIN: Duration = Duration::from_secs(600);

/// Store prefix of the 2.5 (reader-first) entry.
const STATE_PREFIX: &str = "guard-issue-";
/// Store prefix of the 2.7 (writer) entry.
const LABELS_PREFIX: &str = "guard-labels-";

/// The fallback counter, exported as `loom.forge.facade.events`.
pub(crate) const FALLBACK_COUNTER: &str = "guard.issue_view.fallback";

/// What the guards read off one `GET repos/{o}/{r}/issues/{n}` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueView {
    /// 2.5's verdict, as [`SweepRegistry::issue_is_closed_or_pr`] gives it:
    /// `Some(true)` closed or a PR, `Some(false)` an open issue, `None` an
    /// unrecognised state.
    pub(crate) closed_or_pr: Option<bool>,
    /// Label names, as [`SweepRegistry::current_labels_via_rest`] gives them.
    pub(crate) labels: Vec<String>,
}

/// Parse a REST issue body. `None` when it is not one (the caller falls back
/// to the unconditional read).
#[must_use]
pub(crate) fn parse_issue_view(body: &str) -> Option<IssueView> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let labels = v
        .get("labels")?
        .as_array()?
        .iter()
        .map(|l| l.get("name")?.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    // REST's structural PR discriminator (#4504): a PR in any state is terminal.
    let is_pr = v.get("pull_request").is_some_and(|p| !p.is_null());
    let state = v
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.trim().to_ascii_uppercase());
    let closed_or_pr = match (is_pr, state.as_deref()) {
        (true, _) | (false, Some("CLOSED" | "MERGED")) => Some(true),
        (false, Some("OPEN")) => Some(false),
        _ => None,
    };
    Some(IssueView {
        closed_or_pr,
        labels,
    })
}

/// Whether the conditional reads are on: the kill switch, and a daemon store
/// (always present in production; test builds opt in per thread, which keeps
/// the many fake-`gh` dispatch fixtures on the unconditional path).
fn enabled() -> bool {
    let on = std::env::var(GUARD_ISSUE_SNAPSHOT_ENV).map_or(true, |v| {
        !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    });
    on && store::daemon_store_dir().is_some()
}

/// The [`ReadPin`] of a guard read. `writer_site` is 2.7's W4-C writer pin;
/// a recent own write to the issue only drops the `If-None-Match`, and never
/// moves 2.5 off its reader. 2.5 was reader-first before W9 and reads state
/// and PR-ness, which the daemon's own label and comment writes do not
/// change; pinning it to the writer would add writer-bucket spend.
#[must_use]
pub(crate) fn guard_read_pin(writer_site: bool, own_write: bool) -> ReadPin {
    ReadPin {
        writer: writer_site,
        unconditional: own_write,
    }
}

impl SweepRegistry {
    /// Step 2.5: is `issue` closed or a PR? The conditional read, else
    /// [`Self::issue_is_closed_or_pr`].
    pub(crate) fn guard_closed_or_pr(&self, issue: u32) -> Option<bool> {
        match self.conditional_issue_view(issue, "guard.issue_view", STATE_PREFIX, false) {
            Some(view) => view.closed_or_pr,
            None => self.issue_is_closed_or_pr(issue),
        }
    }

    /// Step 2.7 (and 2.71): `issue`'s labels. The writer-pinned conditional
    /// read, else [`Self::current_labels_via_rest`].
    pub(crate) fn guard_issue_labels(&self, issue: u32) -> Option<Vec<String>> {
        let op = "guard.issue_labels_view";
        match self.conditional_issue_view(issue, op, LABELS_PREFIX, true) {
            Some(view) => Some(view.labels),
            None => self.current_labels_via_rest(issue),
        }
    }

    fn conditional_issue_view(
        &self,
        issue: u32,
        caller: &'static str,
        prefix: &'static str,
        writer: bool,
    ) -> Option<IssueView> {
        if !enabled() {
            return None;
        }
        let (owner, repo) = self.resolve_owner_repo()?;
        let (slug, url) =
            (format!("{owner}/{repo}"), format!("repos/{owner}/{repo}/issues/{issue}"));
        let own_write =
            crate::gh_invocation::own_writes::written_within(&slug, issue, OWN_WRITE_PIN);
        let pin = guard_read_pin(writer, own_write);
        let site = ConditionalRead::new(caller, crate::forge_call_stats::ops::ISSUE_VIEW_STATE)
            .within(Some(reap_gh_timeout()));
        let root = Some(self.config.workspace_root.as_path());
        let gh = self.resolved_gh();
        let read = store::cached_read_pinned(site, &gh, root, Some(&slug), &url, prefix, pin);
        let view = match read {
            Ok(r) => r.body.as_deref().and_then(parse_issue_view),
            Err(e) => {
                log::debug!("issue #{issue}: {caller} failed ({e:#}); unconditional read");
                None
            }
        };
        if view.is_none() {
            crate::forge_call_stats::counters::bump(FALLBACK_COUNTER);
        }
        view
    }
}

#[cfg(test)]
#[path = "issue_snapshot_tests.rs"]
mod tests;
