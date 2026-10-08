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
//!
//! `agent` (#10607) is the agent role of a row the daemon ingested from an
//! agent `gh` front's sink rows ([`record_agent`],
//! [`crate::forge_call_stats::ingest`]); the daemon's own rows carry
//! [`NO_AGENT`]. Served vs passthrough rides `caller`: `agent_gh_front` is a
//! served read, `agent.gh.<command>` a passthrough. Agent series live in a
//! store of their own with a smaller cap, so ingested (untrusted) rows can
//! never push the daemon's own series into the overflow fold.

use std::collections::BTreeMap;

use crate::telemetry::ops::{MetricName, MetricPoint};

/// Distinct label sets held between drains. A new set past this is folded
/// into the fixed [`OVERFLOW`] labels — only `outcome` survives, a closed
/// five-value enum — so a drain is never more than `MAX_SERIES + 5` points
/// whatever the labels vary in, and the folded counts keep the total.
const MAX_SERIES: usize = 2048;

/// Distinct agent label sets held between drains ([`record_agent`]). Past
/// it a new set folds into one series per `(agent, served|passthrough,
/// outcome)`, so the agent store adds at most `AGENT_MAX_SERIES + 140`
/// points.
const AGENT_MAX_SERIES: usize = 512;

/// The value of every string label of a folded (over-cap) series.
const OVERFLOW: &str = "overflow";

/// The `agent` label of the daemon's own rows.
pub const NO_AGENT: &str = "-";

/// The `caller` a folded passthrough agent series keeps (a served one keeps
/// `agent_gh_front`), so served vs passthrough survives the fold.
const AGENT_OVERFLOW_CALLER: &str = "agent.gh.overflow";

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

/// `(caller, op, role, account, cred_owner, installation, target_owner,
/// resource, agent, outcome)`.
type Key = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    &'static str,
    &'static str,
);

type Series = BTreeMap<Key, u64>;

/// The daemon's own series and the ingested agent series, capped apart.
#[derive(Default)]
struct Stores {
    daemon: Series,
    agent: Series,
}

impl Stores {
    const fn new() -> Self {
        Self {
            daemon: BTreeMap::new(),
            agent: BTreeMap::new(),
        }
    }
}

fn add(store: &mut Series, cap: usize, labels: CallLabels, agent: &'static str, value: u64) {
    let key = (
        labels.caller,
        labels.op,
        labels.role,
        labels.account,
        labels.cred_owner,
        labels.installation,
        labels.target_owner,
        labels.resource,
        agent,
        labels.outcome.as_str(),
    );
    let key = if store.len() >= cap && !store.contains_key(&key) {
        let o = || OVERFLOW.to_string();
        let caller = if agent == NO_AGENT {
            o()
        } else if key.0 == crate::agent_gh::STATS_CALLER {
            key.0.clone()
        } else {
            AGENT_OVERFLOW_CALLER.to_string()
        };
        (caller, o(), o(), o(), o(), o(), o(), o(), agent, key.9)
    } else {
        key
    };
    let slot = store.entry(key).or_default();
    *slot = slot.saturating_add(value);
}

#[cfg(not(test))]
fn with_store<R>(f: impl FnOnce(&mut Stores) -> R) -> Option<R> {
    static STORE: std::sync::Mutex<Stores> = std::sync::Mutex::new(Stores::new());
    STORE.lock().ok().map(|mut s| f(&mut s))
}

/// Test builds keep the series per thread, so parallel tests that make
/// facade calls never see each other's points.
#[cfg(test)]
fn with_store<R>(f: impl FnOnce(&mut Stores) -> R) -> Option<R> {
    thread_local! {
        static STORE: std::cell::RefCell<Stores> = const { std::cell::RefCell::new(Stores::new()) };
    }
    Some(STORE.with(|s| f(&mut s.borrow_mut())))
}

/// Add `value` requests to the series `labels`. A no-op when ops signals
/// are not exported.
pub fn record(labels: CallLabels, value: u64) {
    if !super::spans_exported() || value == 0 {
        return;
    }
    with_store(|s| add(&mut s.daemon, MAX_SERIES, labels, NO_AGENT, value));
}

/// [`record`] for a row an agent `gh` front wrote, ingested by the daemon
/// (#10607): `agent` is its closed-vocabulary role. Kept in the agent store,
/// whose own cap leaves the daemon's series untouched.
pub fn record_agent(labels: CallLabels, agent: &'static str, value: u64) {
    if !super::spans_exported() || value == 0 {
        return;
    }
    with_store(|s| add(&mut s.agent, AGENT_MAX_SERIES, labels, agent, value));
}

/// Drain every series into `loom.forge.calls` points (delta semantics: a
/// second drain with no calls in between is empty).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let drained = with_store(std::mem::take).unwrap_or_default();
    drained
        .daemon
        .into_iter()
        .chain(drained.agent)
        .map(
            |(
                (
                    caller,
                    op,
                    role,
                    account,
                    cred_owner,
                    installation,
                    target_owner,
                    resource,
                    agent,
                    outcome,
                ),
                n,
            )| {
                MetricPoint::int(MetricName::ForgeCalls, i64::try_from(n).unwrap_or(i64::MAX))
                    .label("caller", caller)
                    .label("op", op)
                    .label("role", role)
                    .label("account", account)
                    .label("cred_owner", cred_owner)
                    .label("installation", installation)
                    .label("target_owner", target_owner)
                    .label("resource", resource)
                    .label("agent", agent)
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
    use crate::telemetry::ops::MetricValue;

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
            add(&mut store, MAX_SERIES, labels(i, outcomes[i % outcomes.len()]), NO_AGENT, 2);
        }
        assert!(store.len() <= MAX_SERIES + outcomes.len(), "{} series", store.len());
        assert_eq!(store.values().sum::<u64>(), 2 * n as u64, "no count is lost");

        let folded: Vec<_> = store.iter().filter(|(k, _)| k.0 == OVERFLOW).collect();
        assert_eq!(folded.len(), outcomes.len(), "one overflow series per outcome");
        for (k, _) in &folded {
            for v in [&k.0, &k.1, &k.2, &k.3, &k.4, &k.5, &k.6, &k.7] {
                assert_eq!(v, OVERFLOW, "{k:?}");
            }
            assert_eq!(k.8, NO_AGENT, "{k:?}");
        }

        // A series admitted before the cap keeps accumulating under its own key.
        let first = labels(0, CallOutcome::Ok);
        let before = store.len();
        add(&mut store, MAX_SERIES, first.clone(), NO_AGENT, 5);
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
            NO_AGENT,
            first.outcome.as_str(),
        );
        assert_eq!(store.get(&key).copied(), Some(2 + 5));
    }

    fn agent_labels(caller: &str, i: usize) -> CallLabels {
        CallLabels {
            caller: caller.to_string(),
            op: "unknown".to_string(),
            role: "agent-builder".to_string(),
            account: format!("app-{i}"),
            cred_owner: format!("owner-{i}"),
            installation: "-".to_string(),
            target_owner: "acme".to_string(),
            resource: "graphql".to_string(),
            outcome: CallOutcome::Ok,
        }
    }

    /// #10607: ingested agent rows drain once, labelled `agent`, beside the
    /// daemon's own rows (labelled `-`).
    #[test]
    fn agent_rows_drain_once_with_the_agent_label() {
        use crate::observability::ops::capture::capture;
        let (points, _) = capture(|| {
            let _ = drain_points();
            record(labels(1, CallOutcome::Ok), 1);
            record_agent(agent_labels("agent.gh.pr", 1), "builder", 2);
            record_agent(agent_labels("agent.gh.pr", 1), "builder", 1);
            record_agent(agent_labels("agent_gh_front", 1), "judge", 1);
            drain_points()
        });
        let agent = |p: &MetricPoint| p.labels["agent"].clone();
        assert_eq!(points.len(), 3, "{points:?}");
        let pr = points
            .iter()
            .find(|p| p.labels["caller"] == "agent.gh.pr")
            .unwrap();
        assert_eq!((agent(pr), pr.value), ("builder".into(), MetricValue::Int(3)));
        let served = points
            .iter()
            .find(|p| p.labels["caller"] == "agent_gh_front")
            .unwrap();
        assert_eq!(agent(served), "judge");
        let own = points
            .iter()
            .find(|p| p.labels["caller"] == "issue.view")
            .unwrap();
        assert_eq!(agent(own), NO_AGENT);
        assert!(points.iter().all(|p| p.labels.len() == 10), "{points:?}");
        let (again, _) = capture(drain_points);
        assert!(again.is_empty(), "delta semantics: {again:?}");
    }

    /// A flood of distinct agent label sets folds inside the agent store:
    /// the daemon's own store keeps its full cap, and served vs passthrough
    /// and the role survive the fold.
    #[test]
    fn agent_series_fold_apart_from_the_daemons() {
        let mut stores = Stores::new();
        for i in 0..AGENT_MAX_SERIES * 2 {
            let caller = if i % 2 == 0 {
                "agent.gh.api"
            } else {
                "agent_gh_front"
            };
            add(&mut stores.agent, AGENT_MAX_SERIES, agent_labels(caller, i), "hermit", 1);
        }
        assert!(stores.agent.len() <= AGENT_MAX_SERIES + 2, "{}", stores.agent.len());
        assert_eq!(stores.agent.values().sum::<u64>(), (AGENT_MAX_SERIES * 2) as u64);
        let folded: Vec<_> = stores.agent.keys().filter(|k| k.1 == OVERFLOW).collect();
        let callers: Vec<&str> = folded.iter().map(|k| k.0.as_str()).collect();
        assert_eq!(callers, [AGENT_OVERFLOW_CALLER, "agent_gh_front"], "{folded:?}");
        assert!(folded.iter().all(|k| k.8 == "hermit"));
        assert!(stores.daemon.is_empty());
    }
}
