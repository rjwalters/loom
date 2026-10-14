//! Stage-entry coverage (#11897), as a delta counter drained on the
//! collector's rate-limit tick next to `loom.forge.mirror.reads`:
//!
//! - `loom.stage_entry.lookups{repo, kind}`: one per
//!   [`crate::step_dispatch::stage_entry::StageEntryProvider::entry`] answer,
//!   by the source that gave it in `kind` (`mirror`, `events`, `created`), or
//!   `none` when not even a creation time was known. The share of `mirror`
//!   and `events` is the stage-age coverage #11894 §10 gates on. The label is
//!   `kind`, already allowlisted, so the collector's `keep_keys` is unchanged.
//!
//! Nothing accumulates without an ops sink (one atomic check per call), and
//! every label is a closed token or a lowercase `owner/name`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::step_dispatch::stage_entry::Source;
use crate::telemetry::ops::{MetricName, MetricPoint};

/// Distinct series held between drains; past it a new series is counted
/// under `repo = overflow`.
const MAX_SERIES: usize = 1024;

static LOOKUPS: Mutex<BTreeMap<(String, &'static str), u64>> = Mutex::new(BTreeMap::new());

/// Count one stage-entry answer for `repo` (`None`: no source knew).
pub fn record_lookup(repo: &str, source: Option<Source>) {
    if !super::spans_exported() {
        return;
    }
    let source = source.map_or("none", Source::as_str);
    let mut lookups = LOOKUPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let repo = repo.to_ascii_lowercase();
    let repo = if lookups.len() >= MAX_SERIES && !lookups.contains_key(&(repo.clone(), source)) {
        "overflow".to_string()
    } else {
        repo
    };
    *lookups.entry((repo, source)).or_default() += 1;
}

/// Drain every series into points (delta semantics).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let drained = std::mem::take(
        &mut *LOOKUPS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    drained
        .into_iter()
        .map(|((repo, source), n)| {
            MetricPoint::int(MetricName::StageEntryLookups, i64::try_from(n).unwrap_or(i64::MAX))
                .label("repo", repo)
                .label("kind", source)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn lookups_drain_once_by_repo_and_source() {
        let (points, _) = crate::observability::ops::capture::capture(|| {
            let _ = drain_points();
            record_lookup("Acme/A", Some(Source::Mirror));
            record_lookup("acme/a", Some(Source::Mirror));
            record_lookup("acme/a", Some(Source::Created));
            record_lookup("acme/a", None);
            let points = drain_points();
            assert!(drain_points().is_empty(), "delta: nothing new");
            points
        });
        let find = |source: &str| {
            points
                .iter()
                .find(|p| {
                    p.name == MetricName::StageEntryLookups
                        && p.labels.get("repo").map(String::as_str) == Some("acme/a")
                        && p.labels.get("kind").map(String::as_str) == Some(source)
                })
                .map(|p| p.value)
        };
        use crate::telemetry::ops::MetricValue::Int;
        assert_eq!(find("mirror"), Some(Int(2)));
        assert_eq!(find("created"), Some(Int(1)));
        assert_eq!(find("none"), Some(Int(1)));
        assert_eq!(find("events"), None);
    }
}
