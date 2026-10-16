//! How long items have been held (#12038): `loom.forge.hold_items` and
//! `loom.forge.hold_age_seconds`, from the GitHub state mirror's `labels_at`.
//!
//! Pure: [`points_from_mirror`] turns already-listed rows into points. It
//! reads no forge; the sampler (`stage_dwell::record`) lists the rows through
//! the mirror and gives nothing to this module for a repo the mirror cannot
//! serve fresh, so that repo emits no hold points (absent, never `0`).
//!
//! An age is never measured from `updated_at`, which Champion's own notices
//! bump (#9534). An item whose row carries no usable time for the hold label
//! counts under `basis=unknown` and is left out of the percentiles.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::forge_listing::RestIssue;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::work_finder::{HOLD_NAMES, PARK_LABELS};

/// Every label that holds or parks an item: the park labels, then the holds
/// not already among them, each once.
#[must_use]
pub fn hold_labels() -> Vec<&'static str> {
    let mut labels: Vec<&'static str> = Vec::new();
    for label in PARK_LABELS.iter().chain(HOLD_NAMES.iter()) {
        if !labels.contains(label) {
            labels.push(label);
        }
    }
    labels
}

/// The `hold` label value: the label without its namespace prefix and with
/// `-` → `_`.
fn hold_name(label: &str) -> String {
    label
        .split_once(':')
        .map_or(label, |(_, name)| name)
        .replace('-', "_")
}

/// The `p`th percentile (1..=100) of `sorted` (ascending, non-empty) by
/// nearest rank.
fn nearest_rank(sorted: &[i64], p: usize) -> i64 {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[derive(Default)]
struct Group {
    known: Vec<i64>,
    unknown: usize,
}

/// The hold points for one repo's listed rows. A row carrying two hold labels
/// counts once under each; a row listed twice (once per label) counts once.
/// No rows, no points.
#[must_use]
pub fn points_from_mirror(repo: &str, rows: &[RestIssue], now: DateTime<Utc>) -> Vec<MetricPoint> {
    let labels = hold_labels();
    let mut unique: BTreeMap<u32, &RestIssue> = BTreeMap::new();
    for row in rows {
        unique.entry(row.number).or_insert(row);
    }
    // (artifact, hold) -> ages and the unknown count.
    let mut groups: BTreeMap<(&'static str, String), Group> = BTreeMap::new();
    for row in unique.values() {
        let artifact = if row.is_pull_request { "pr" } else { "issue" };
        for label in labels.iter().filter(|l| row.labels.iter().any(|x| x == *l)) {
            let started = row
                .labels_at
                .as_ref()
                .and_then(|times| times.get(*label))
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok());
            let group = groups.entry((artifact, hold_name(label))).or_default();
            match started {
                Some(at) => group
                    .known
                    .push((now - at.with_timezone(&Utc)).num_seconds().max(0)),
                None => group.unknown += 1,
            }
        }
    }
    let repo = repo.to_ascii_lowercase();
    let count = |n: usize, artifact: &str, hold: &str, basis: &str| {
        MetricPoint::int(MetricName::ForgeHoldItems, i64::try_from(n).unwrap_or(i64::MAX))
            .label("repo", repo.as_str())
            .label("artifact", artifact)
            .label("hold", hold)
            .label("basis", basis)
    };
    let mut points = Vec::new();
    for ((artifact, hold), mut group) in groups {
        group.known.sort_unstable();
        points.push(count(group.known.len(), artifact, &hold, "labels_at"));
        if group.unknown > 0 {
            points.push(count(group.unknown, artifact, &hold, "unknown"));
        }
        if group.known.is_empty() {
            continue;
        }
        for (stat, p) in [("oldest", 100), ("p50", 50), ("p90", 90)] {
            points.push(
                MetricPoint::int(MetricName::ForgeHoldAgeSeconds, nearest_rank(&group.known, p))
                    .label("repo", repo.as_str())
                    .label("artifact", artifact)
                    .label("hold", hold.as_str())
                    .label("stat", stat),
            );
        }
    }
    points
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "hold_age_tests.rs"]
mod tests;
