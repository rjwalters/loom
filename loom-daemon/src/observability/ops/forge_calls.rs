//! `loom.forge.calls` (W1): the requests this process's `gh` facade spent,
//! by caller, operation, identity role, credential bucket, target owner and
//! outcome, as a delta counter.
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

/// Distinct label sets held between drains; a new set past this is folded
/// into `caller = "overflow"` so the point count stays bounded.
const MAX_SERIES: usize = 2048;

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
    /// The owner of the repository the call was for, or `unknown`.
    pub target_owner: String,
    /// The billed resource.
    pub resource: String,
    pub outcome: CallOutcome,
}

type Series = BTreeMap<(String, String, String, String, String, String, String, &'static str), u64>;

fn add(store: &mut Series, labels: CallLabels, value: u64) {
    let mut key = (
        labels.caller,
        labels.op,
        labels.role,
        labels.account,
        labels.cred_owner,
        labels.target_owner,
        labels.resource,
        labels.outcome.as_str(),
    );
    if store.len() >= MAX_SERIES && !store.contains_key(&key) {
        key.0 = "overflow".to_string();
    }
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
            |((caller, op, role, account, cred_owner, target_owner, resource, outcome), n)| {
                MetricPoint::int(MetricName::ForgeCalls, i64::try_from(n).unwrap_or(i64::MAX))
                    .label("caller", caller)
                    .label("op", op)
                    .label("role", role)
                    .label("account", account)
                    .label("cred_owner", cred_owner)
                    .label("target_owner", target_owner)
                    .label("resource", resource)
                    .label("outcome", outcome)
            },
        )
        .collect()
}
