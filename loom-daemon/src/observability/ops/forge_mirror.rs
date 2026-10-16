//! Mirror telemetry (2AMLogic/2am#4367 P5), as delta counters drained on the
//! collector's rate-limit tick next to `loom.forge.calls`:
//!
//! - `loom.forge.mirror.reads{caller, repo, outcome}`: one per discovery
//!   listing that consulted the mirror (`outcome` = `hit`, `stale`,
//!   `unknown`, `error`, `skipped`; every non-`hit` fell back to today's
//!   path), and one per change-stream request (`caller = wake`, `repo = -`,
//!   `outcome` = `hit` | `error`). With `loom.forge.calls{caller}` for the
//!   same callers this shows how much discovery still reaches GitHub.
//! - `loom.forge.mirror.changes{repo, kind}`: changes the change stream
//!   delivered.
//! - `loom.forge.mirror.wake_latency{repo}`: summed seconds from a change at
//!   the mirror to the pull that saw it; divided by `changes` it is the mean.
//!
//! Nothing accumulates without an ops sink (one atomic check per call), and
//! every label is a closed token or a lowercase `owner/name`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::forge_mirror::discovery::Outcome;
use crate::telemetry::ops::{MetricName, MetricPoint};

/// Distinct series held between drains; past it a new series is dropped
/// (and counted under `repo = overflow`).
const MAX_SERIES: usize = 1024;

const OVERFLOW: &str = "overflow";

#[derive(Default)]
struct Store {
    reads: BTreeMap<(String, String, &'static str), u64>,
    changes: BTreeMap<(String, String), u64>,
    latency: BTreeMap<String, f64>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn with_store<T>(f: impl FnOnce(&mut Store) -> T) -> T {
    let mut guard = STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Store::default))
}

fn series_key(len: usize, repo: &str) -> String {
    if len >= MAX_SERIES {
        OVERFLOW.to_string()
    } else {
        repo.to_string()
    }
}

/// Count one mirror read.
pub fn record_read(caller: &'static str, repo: &str, outcome: Outcome) {
    if !super::spans_exported() {
        return;
    }
    with_store(|s| {
        let repo = series_key(s.reads.len(), repo);
        *s.reads
            .entry((caller.to_string(), repo, outcome.as_str()))
            .or_default() += 1;
    });
}

/// Count one delivered change and its latency.
pub fn record_change(repo: &str, kind: &str, latency_secs: f64) {
    if !super::spans_exported() {
        return;
    }
    let kind = match kind {
        "issue" | "pr" | "label" | "check" | "pr_merged" => kind,
        _ => "other",
    };
    with_store(|s| {
        let repo = series_key(s.changes.len(), repo);
        *s.changes
            .entry((repo.clone(), kind.to_string()))
            .or_default() += 1;
        *s.latency.entry(repo).or_default() += latency_secs.max(0.0);
    });
}

/// Drain every series into points (delta semantics).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let drained = with_store(std::mem::take);
    let int = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let reads = drained
        .reads
        .into_iter()
        .map(|((caller, repo, outcome), n)| {
            MetricPoint::int(MetricName::ForgeMirrorReads, int(n))
                .label("caller", caller)
                .label("repo", repo)
                .label("outcome", outcome)
        });
    let changes = drained.changes.into_iter().map(|((repo, kind), n)| {
        MetricPoint::int(MetricName::ForgeMirrorChanges, int(n))
            .label("repo", repo)
            .label("kind", kind)
    });
    let latency = drained.latency.into_iter().map(|(repo, secs)| {
        MetricPoint::double(MetricName::ForgeMirrorWakeLatency, secs).label("repo", repo)
    });
    reads.chain(changes).chain(latency).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn reads_and_changes_drain_once_as_labelled_points() {
        let (points, _) = crate::observability::ops::capture::capture(|| {
            let _ = drain_points();
            record_read("work_finder", "o/a", Outcome::Hit);
            record_read("work_finder", "o/a", Outcome::Hit);
            record_read("step_engine", "o/a", Outcome::Stale);
            record_change("o/a", "label", 2.5);
            record_change("o/a", "label", 1.5);
            record_change("o/a", "weird", 0.0);
            let points = drain_points();
            assert!(drain_points().is_empty(), "delta: nothing new");
            points
        });
        let find = |name: MetricName, labels: &[(&str, &str)]| {
            points
                .iter()
                .find(|p| {
                    p.name == name
                        && labels
                            .iter()
                            .all(|(k, v)| p.labels.get(*k).map(String::as_str) == Some(*v))
                })
                .map(|p| p.value)
        };
        use crate::telemetry::ops::MetricValue::{Double, Int};
        assert_eq!(
            find(MetricName::ForgeMirrorReads, &[("caller", "work_finder"), ("outcome", "hit")]),
            Some(Int(2))
        );
        assert_eq!(
            find(MetricName::ForgeMirrorReads, &[("caller", "step_engine"), ("outcome", "stale")]),
            Some(Int(1))
        );
        assert_eq!(
            find(MetricName::ForgeMirrorChanges, &[("repo", "o/a"), ("kind", "label")]),
            Some(Int(2))
        );
        assert_eq!(find(MetricName::ForgeMirrorChanges, &[("kind", "other")]), Some(Int(1)));
        assert_eq!(find(MetricName::ForgeMirrorWakeLatency, &[("repo", "o/a")]), Some(Double(4.0)));
    }
}
