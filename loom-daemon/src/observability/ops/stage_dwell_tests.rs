//! Forge label-stage dwell tests (Issue #8929, part 2). No forge: the
//! per-item reads go through a scripted fetcher that counts its calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};

use super::{
    closing_refs, parse_label_times, stage_points, DwellSample, RepoInput, Sampler, StageFetcher,
    StageItem, BUILDING, CURATED, FETCH_BUDGET, ISSUE, REVIEW_REQUESTED, STAGE_LABELS,
};
use crate::telemetry::ops::{bounded_labels, MetricName, MetricPoint};

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

#[derive(Default)]
struct Fake {
    labels: BTreeMap<u32, BTreeMap<String, DateTime<Utc>>>,
    merged: BTreeMap<u32, Option<DateTime<Utc>>>,
    reads: usize,
}

impl StageFetcher for Fake {
    fn label_times(
        &mut self,
        _root: &Path,
        _slug: &str,
        number: u32,
    ) -> Option<BTreeMap<String, DateTime<Utc>>> {
        self.reads += 1;
        Some(self.labels.get(&number).cloned().unwrap_or_default())
    }
    fn merged_at(
        &mut self,
        _root: &Path,
        _slug: &str,
        number: u32,
    ) -> Option<Option<DateTime<Utc>>> {
        self.reads += 1;
        self.merged.get(&number).copied()
    }
}

fn issue(number: u32, created: i64) -> StageItem {
    StageItem {
        number,
        created_at: Some(at(created)),
        closes: Vec::new(),
    }
}

fn pr(number: u32, created: i64, closes: &[u32]) -> StageItem {
    StageItem {
        number,
        created_at: Some(at(created)),
        closes: closes.to_vec(),
    }
}

fn repo(listings: &[(&'static str, Vec<StageItem>)]) -> RepoInput {
    let mut input = RepoInput {
        slug: "acme/app".into(),
        root: PathBuf::from("/w/app"),
        listings: STAGE_LABELS.iter().map(|l| (*l, Vec::new())).collect(),
    };
    for (label, items) in listings {
        input.listings.insert(label, items.clone());
    }
    input
}

fn stage(samples: &[DwellSample], name: &str) -> Vec<i64> {
    samples
        .iter()
        .filter(|s| s.stage == name)
        .map(|s| s.seconds)
        .collect()
}

#[test]
fn the_first_sample_is_a_baseline_and_emits_no_entries() {
    let mut fake = Fake::default();
    fake.labels
        .insert(1, BTreeMap::from([(CURATED.to_string(), at(50))]));
    let mut sampler = Sampler::default();
    let samples = sampler.sample(&[repo(&[(CURATED, vec![issue(1, 0)])])], at(100), &mut fake);
    assert!(samples.is_empty());
    // The spare budget warmed the curated time for a later exit sample.
    assert_eq!(fake.reads, 1);
}

#[test]
fn created_to_curated_and_curated_to_issue() {
    let mut fake = Fake::default();
    fake.labels
        .insert(2, BTreeMap::from([(CURATED.to_string(), at(400))]));
    let mut sampler = Sampler::default();
    sampler.sample(&[repo(&[])], at(0), &mut fake);
    // Issue 2 (created at 100) appears under loom:curated, labelled at 400.
    let samples = sampler.sample(&[repo(&[(CURATED, vec![issue(2, 100)])])], at(600), &mut fake);
    assert_eq!(stage(&samples, "created_to_curated"), [300]);
    // Promoted to loom:issue: dwell from the curated label to the observation.
    let samples = sampler.sample(&[repo(&[(ISSUE, vec![issue(2, 100)])])], at(1000), &mut fake);
    assert_eq!(stage(&samples, "curated_to_issue"), [600]);
    assert_eq!(fake.reads, 1, "the events read is cached");
}

#[test]
fn leaving_curated_without_promotion_is_not_approval() {
    let mut fake = Fake::default();
    fake.labels
        .insert(3, BTreeMap::from([(CURATED.to_string(), at(10))]));
    let mut sampler = Sampler::default();
    sampler.sample(&[repo(&[(CURATED, vec![issue(3, 0)])])], at(20), &mut fake);
    // Closed as not planned: it is under no stage label any more.
    let samples = sampler.sample(&[repo(&[])], at(500), &mut fake);
    assert!(samples.is_empty());
}

#[test]
fn building_to_review_requested_uses_the_linked_issue() {
    let mut fake = Fake::default();
    fake.labels
        .insert(4, BTreeMap::from([(BUILDING.to_string(), at(1000))]));
    let mut sampler = Sampler::default();
    sampler.sample(&[repo(&[(BUILDING, vec![issue(4, 0)])])], at(1100), &mut fake);
    let samples = sampler.sample(
        &[repo(&[
            (BUILDING, vec![issue(4, 0)]),
            (REVIEW_REQUESTED, vec![pr(40, 4600, &[4])]),
        ])],
        at(5000),
        &mut fake,
    );
    assert_eq!(stage(&samples, "building_to_review_requested"), [3600]);
}

#[test]
fn review_requested_to_merged_checks_the_pr_once_it_leaves_review() {
    let mut fake = Fake::default();
    fake.merged.insert(50, Some(at(9000)));
    fake.merged.insert(51, None);
    let mut sampler = Sampler::default();
    let review = vec![pr(50, 1000, &[]), pr(51, 2000, &[])];
    sampler.sample(&[repo(&[(REVIEW_REQUESTED, review)])], at(3000), &mut fake);
    let reads = fake.reads;
    let samples = sampler.sample(&[repo(&[])], at(9500), &mut fake);
    assert_eq!(stage(&samples, "review_requested_to_merged"), [8000]);
    assert_eq!(fake.reads - reads, 2, "one pulls read per PR that left review");
    // Nothing left to check on the next sample.
    let reads = fake.reads;
    assert!(sampler.sample(&[repo(&[])], at(9800), &mut fake).is_empty());
    assert_eq!(fake.reads, reads);
}

#[test]
fn per_item_reads_are_capped_per_sample() {
    let mut fake = Fake::default();
    let mut sampler = Sampler::default();
    sampler.sample(&[repo(&[])], at(0), &mut fake);
    let many: Vec<StageItem> = (1..=30).map(|n| issue(n, 0)).collect();
    for n in 1..=30 {
        fake.labels
            .insert(n, BTreeMap::from([(CURATED.to_string(), at(5))]));
    }
    let samples = sampler.sample(&[repo(&[(CURATED, many.clone())])], at(100), &mut fake);
    assert_eq!(fake.reads, FETCH_BUDGET);
    assert_eq!(stage(&samples, "created_to_curated").len(), FETCH_BUDGET);
    // The rest are sampled over the following samples, never re-read.
    let samples = sampler.sample(&[repo(&[(CURATED, many)])], at(400), &mut fake);
    assert_eq!(fake.reads, 2 * FETCH_BUDGET);
    assert_eq!(stage(&samples, "created_to_curated").len(), FETCH_BUDGET);
}

#[test]
fn a_repo_missing_from_a_sample_keeps_its_state() {
    let mut fake = Fake::default();
    fake.labels
        .insert(6, BTreeMap::from([(CURATED.to_string(), at(10))]));
    let mut sampler = Sampler::default();
    sampler.sample(&[repo(&[(CURATED, vec![issue(6, 0)])])], at(20), &mut fake);
    // A failed listing: the repo is absent, which is not a mass exit.
    assert!(sampler.sample(&[], at(300), &mut fake).is_empty());
    let samples = sampler.sample(&[repo(&[(ISSUE, vec![issue(6, 0)])])], at(600), &mut fake);
    assert_eq!(stage(&samples, "curated_to_issue"), [590]);
}

#[test]
fn points_are_delta_pairs_and_gauges_labelled_by_state_only() {
    let samples = [
        DwellSample {
            stage: "created_to_curated",
            seconds: 10,
        },
        DwellSample {
            stage: "created_to_curated",
            seconds: 30,
        },
    ];
    let input = repo(&[(CURATED, vec![issue(1, 0), issue(2, 0)])]);
    let points = stage_points(&samples, std::slice::from_ref(&input));
    assert!(points.contains(
        &MetricPoint::int(MetricName::ForgeStageDwell, 40).label("state", "created_to_curated")
    ));
    assert!(points.contains(
        &MetricPoint::int(MetricName::ForgeStageDwellSamples, 2)
            .label("state", "created_to_curated")
    ));
    assert!(points
        .contains(&MetricPoint::int(MetricName::ForgeStageItems, 2).label("state", "curated")));
    assert!(points.contains(
        &MetricPoint::int(MetricName::ForgeStageItems, 0).label("state", "review_requested")
    ));
    for point in &points {
        assert_eq!(bounded_labels(&point.labels), point.labels);
        assert!(point.labels.keys().all(|k| k == "state"));
    }
    assert!(stage_points(&[], &[]).is_empty(), "no repos listed: no gauges");
}

#[test]
fn closing_refs_reads_github_keywords_only() {
    let body = "Closes #8907\nFixes: #12, resolves #12 and see #99. Part of #5. fixed #7x";
    assert_eq!(closing_refs(body), [8907, 12, 7]);
    assert!(closing_refs("").is_empty());
}

#[test]
fn label_times_take_the_latest_labeled_event_per_label() {
    let events = serde_json::json!([
        {"event": "labeled", "label": {"name": "loom:curated"}, "created_at": "2026-09-01T00:00:00Z"},
        {"event": "unlabeled", "label": {"name": "loom:curated"}, "created_at": "2026-09-02T00:00:00Z"},
        {"event": "labeled", "label": {"name": "loom:curated"}, "created_at": "2026-09-03T00:00:00Z"},
        {"event": "commented", "created_at": "2026-09-04T00:00:00Z"}
    ]);
    let times = parse_label_times(&events);
    assert_eq!(times.len(), 1);
    assert_eq!(times["loom:curated"], Utc.with_ymd_and_hms(2026, 9, 3, 0, 0, 0).unwrap());
    assert!(parse_label_times(&serde_json::json!({"message": "Not Found"})).is_empty());
}
