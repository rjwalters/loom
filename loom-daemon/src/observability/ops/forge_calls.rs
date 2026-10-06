//! `loom.forge.calls` (W1): the requests this process's `gh` facade sent,
//! by caller, operation, identity role, credential bucket, target owner and
//! outcome, as a delta counter. It counts request *observations*: the free
//! `gh api rate_limit` probe is a request too and is counted under
//! `resource = "other"`, which is never a billed GitHub bucket. What a bucket
//! was *charged* is the forge-call sink's rollup
//! ([`crate::forge_call_stats::buckets`]), which excludes known-free rows.
//!
//! The facade's accounting adds to an in-process map ([`record`]); the
//! collector's rate-limit tick drains it ([`drain_points`]) the same way it
//! flushes `github.ratelimit.breaker_skips`. Nothing is accumulated when no
//! ops sink is registered (OTLP only, the FLAGS-OFF posture), so a host
//! without an exporter pays one atomic check per call.
//!
//! Every label is a short, bounded token the accounting already sanitized —
//! an operation name, an `app-<id>`-style account, a lowercased owner — never
//! a path, a token or a repository-specific number.

use std::collections::BTreeMap;

use crate::telemetry::ops::{MetricName, MetricPoint};

/// Distinct label sets held between drains. A new set past this is folded
/// into the fixed [`OVERFLOW`] labels — only `outcome` survives, a closed
/// five-value enum — so a drain is never more than `MAX_SERIES + 5` points
/// whatever the labels vary in, and the folded counts keep the total.
const MAX_SERIES: usize = 2048;

/// The value of every string label of a folded (over-cap) series.
const OVERFLOW: &str = "overflow";

/// How a call ended, as the `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    Ok,
    NotModified,
    RateLimited,
    Error,
    /// Refused before it was sent by a budget gate (reserved: no producer
    /// yet; the per-bucket gate records it).
    Shed,
}

impl CallOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotModified => "not_modified",
            Self::RateLimited => "rate_limited",
            Self::Error => "error",
            Self::Shed => "shed",
        }
    }
}

/// The label set of one `loom.forge.calls` series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallLabels {
    /// The facade [`crate::gh_invocation::Operation`] name.
    pub caller: String,
    /// The inventoried operation, or `unknown`.
    pub op: String,
    /// `reader` / `writer` / `writer-fallback`.
    pub role: String,
    /// The credential's account label.
    pub account: String,
    /// The owner the credential's installation covers, or `unknown`.
    pub cred_owner: String,
    /// The App installation the credential was minted under (#10571), or
    /// `-`; one per `(account, cred_owner)`, so it adds no series.
    pub installation: String,
    /// The owner of the repository the call was for, or `unknown`.
    pub target_owner: String,
    /// The billed resource.
    pub resource: String,
    pub outcome: CallOutcome,
}

type SeriesKey = (String, String, String, String, String, String, String, String, &'static str);
type Series = BTreeMap<SeriesKey, u64>;

fn add(store: &mut Series, labels: CallLabels, value: u64) {
    let key = (
        labels.caller,
        labels.op,
        labels.role,
        labels.account,
        labels.cred_owner,
        labels.installation,
        labels.target_owner,
        labels.resource,
        labels.outcome.as_str(),
    );
    let key = if store.len() >= MAX_SERIES && !store.contains_key(&key) {
        let o = || OVERFLOW.to_string();
        (o(), o(), o(), o(), o(), o(), o(), o(), key.8)
    } else {
        key
    };
    let slot = store.entry(key).or_default();
    *slot = slot.saturating_add(value);
}

#[cfg(not(test))]
fn with_store<R>(f: impl FnOnce(&mut Series) -> R) -> Option<R> {
    static STORE: std::sync::Mutex<Series> = std::sync::Mutex::new(BTreeMap::new());
    STORE.lock().ok().map(|mut s| f(&mut s))
}

/// Test builds keep the series per thread, so parallel tests that make
/// facade calls never see each other's points.
#[cfg(test)]
fn with_store<R>(f: impl FnOnce(&mut Series) -> R) -> Option<R> {
    thread_local! {
        static STORE: std::cell::RefCell<Series> = const { std::cell::RefCell::new(BTreeMap::new()) };
    }
    Some(STORE.with(|s| f(&mut s.borrow_mut())))
}

/// Add `value` requests to the series `labels`. A no-op when ops signals
/// are not exported.
pub fn record(labels: CallLabels, value: u64) {
    if !super::spans_exported() || value == 0 {
        return;
    }
    with_store(|s| add(s, labels, value));
}

/// Drain every series into `loom.forge.calls` points (delta semantics: a
/// second drain with no calls in between is empty).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let drained = with_store(std::mem::take).unwrap_or_default();
    drained
        .into_iter()
        .map(
            |((caller, op, role, account, cred_owner, installation, target_owner, resource, outcome), n)| {
                MetricPoint::int(MetricName::ForgeCalls, i64::try_from(n).unwrap_or(i64::MAX))
                    .label("caller", caller)
                    .label("op", op)
                    .label("role", role)
                    .label("account", account)
                    .label("cred_owner", cred_owner)
                    .label("installation", installation)
                    .label("target_owner", target_owner)
                    .label("resource", resource)
                    .label("outcome", outcome)
            },
        )
        .collect()
}

/// Drain the facade's named event counters
/// ([`crate::forge_call_stats::counters`]) into `loom.forge.facade.events`
/// points, one per counter that moved, labelled `reason` = the counter name
/// (a fixed, code-defined vocabulary — never a path or a repo). Delta
/// semantics, like [`drain_points`].
#[must_use]
pub fn drain_event_points() -> Vec<MetricPoint> {
    crate::forge_call_stats::counters::drain_deltas()
        .into_iter()
        .map(|(name, n)| {
            MetricPoint::int(MetricName::ForgeFacadeEvents, i64::try_from(n).unwrap_or(i64::MAX))
                .label("reason", name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_disagree_counter_is_exported_by_name() {
        let _ = drain_event_points();
        crate::forge_call_stats::buckets::bump_cwd_route_disagree();
        crate::forge_call_stats::buckets::bump_cwd_route_disagree();
        let points = drain_event_points();
        let p = points
            .iter()
            .find(|p| {
                p.labels.get("reason").map(String::as_str) == Some("facade.cwd_route.disagree")
            })
            .unwrap();
        assert_eq!(p.name, MetricName::ForgeFacadeEvents);
        assert_eq!(p.value, crate::telemetry::ops::MetricValue::Int(2));
        assert!(drain_event_points().is_empty(), "delta: nothing new");
    }

    fn labels(i: usize, outcome: CallOutcome) -> CallLabels {
        // Every label but `caller` varies, so only a fully fixed overflow
        // key bounds the store.
        CallLabels {
            caller: "issue.view".to_string(),
            op: format!("op-{}", i % 7),
            role: format!("role-{}", i % 3),
            account: format!("app-{i}"),
            cred_owner: format!("owner-{i}"),
            installation: format!("{i}"),
            target_owner: format!("target-{i}"),
            resource: format!("res-{}", i % 5),
            outcome,
        }
    }

    #[test]
    fn varying_non_caller_labels_past_the_cap_stays_bounded_and_keeps_the_total() {
        let outcomes = [
            CallOutcome::Ok,
            CallOutcome::NotModified,
            CallOutcome::RateLimited,
            CallOutcome::Error,
            CallOutcome::Shed,
        ];
        let mut store = Series::new();
        let n = MAX_SERIES * 3;
        for i in 0..n {
            add(&mut store, labels(i, outcomes[i % outcomes.len()]), 2);
        }
        assert!(store.len() <= MAX_SERIES + outcomes.len(), "{} series", store.len());
        assert_eq!(store.values().sum::<u64>(), 2 * n as u64, "no count is lost");

        let folded: Vec<_> = store.iter().filter(|(k, _)| k.0 == OVERFLOW).collect();
        assert_eq!(folded.len(), outcomes.len(), "one overflow series per outcome");
        for (k, _) in &folded {
            for v in [&k.0, &k.1, &k.2, &k.3, &k.4, &k.5, &k.6, &k.7] {
                assert_eq!(v, OVERFLOW, "{k:?}");
            }
        }

        // A series admitted before the cap keeps accumulating under its own key.
        let first = labels(0, CallOutcome::Ok);
        let before = store.len();
        add(&mut store, first.clone(), 5);
        assert_eq!(store.len(), before);
        let key = (
            first.caller,
            first.op,
            first.role,
            first.account,
            first.cred_owner,
            first.installation,
            first.target_owner,
            first.resource,
            first.outcome.as_str(),
        );
        assert_eq!(store.get(&key).copied(), Some(2 + 5));
    }
}
