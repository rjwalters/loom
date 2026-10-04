//! Fleet-wide, forge-derived history (#9343).
//!
//! Pure: no daemon, no network, no clock. Every forge read a real snapshot
//! needs happens in `cli/eta_fleet_cmd.rs`; what is exercised here is the
//! producer, the cache and the properties #9343 asks for — determinism across
//! hosts, purity of the estimator over a snapshot, and an incremental update
//! that neither duplicates nor silently drops evidence.

use super::{as_of, provenance, subject};
use crate::eta::config::HistoryScopeMode;
use crate::eta::explanation::{Explanation, HistoryScope};
use crate::eta::fleet::{self, FleetSnapshot, FORGE_HOST, RETENTION_DAYS};
use crate::eta::heuristics::LandV1;
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::journal::JournalEntry;
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, NoEstimateReason, Stage,
};
use crate::pr_latency::history::fixtures::{labeled, pushed, t};
use crate::pr_latency::history::{PrEvent, PrHistory, PrState};
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::{DateTime, Duration, Utc};
use std::path::Path;

const REPO: &str = "rjwalters/loom";

// -- fixtures -------------------------------------------------------------

/// One merged PR's forge timeline.
///
/// `reject`: `loom:review-requested` → `loom:changes-requested` → push →
/// re-request → `loom:pr` → merged, i.e. one Doctor lap. Otherwise the
/// first-pass shape: request → `loom:pr` → merged. `base` offsets the whole
/// timeline so each PR occupies its own slice of the window, and `spread`
/// varies the durations so the quantile grids are not degenerate.
fn pr(number: u32, base: i64, reject: bool, spread: i64) -> PrHistory {
    let mut events = vec![labeled(REVIEW_REQUESTED, base)];
    let merged_at = if reject {
        events.push(labeled(CHANGES_REQUESTED, base + 1800 + spread));
        events.push(pushed(base + 5400 + spread));
        events.push(labeled(REVIEW_REQUESTED, base + 5500 + spread));
        events.push(labeled(APPROVED, base + 9000 + spread));
        base + 10_800 + spread
    } else {
        events.push(labeled(APPROVED, base + 2400 + spread));
        base + 3600 + spread
    };
    events.push(PrEvent::Merged { at: t(merged_at) });
    PrHistory::new(number, t(base), PrState::Merged, Some(t(merged_at)), Vec::new(), events, true)
}

/// Twenty merged PRs — ten with a Doctor lap, ten first-pass — which is enough
/// for `land-v1`'s three stages and its attempt-1 verdict branch to clear
/// [`crate::eta::MIN_SAMPLES`] at repo level.
fn fleet_prs() -> Vec<PrHistory> {
    (0..20_u32)
        .map(|i| {
            let base = i64::from(i) * 6 * 3600;
            pr(100 + i, base, i < 10, i64::from(i) * 120)
        })
        .collect()
}

/// A snapshot over [`fleet_prs`], as of the fixture estimate instant.
fn fleet_snapshot() -> FleetSnapshot {
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(&fleet_prs(), as_of());
    snapshot
}

/// A `land-v1` input sitting in `review_wait` with no age, so nothing is
/// age-conditioned and the estimate depends on history alone.
fn input() -> EstimateInput {
    EstimateInput {
        subject: subject(),
        as_of: as_of(),
        current: CurrentState::At(CurrentStage {
            stage: Stage::ReviewWait,
            entered_at: Some(as_of()),
            age_sec: 0,
            age_source: AgeSource::LabelEvent,
            rework_rounds: 0,
        }),
        features: crate::eta::explanation::Features::default(),
        features_omitted: Vec::new(),
        provenance: provenance(),
        dispatch: None,
    }
}

/// A host-local history covering only the **in-sweep** phases — the half the
/// forge cannot see. `n` samples of each, recorded by `host`.
fn thin_local_history(host: &str, n: usize) -> StageSamples {
    let mut history = StageSamples::default();
    let mut rows: Vec<JournalEntry> = Vec::new();
    for stage in [Stage::SweepCurator, Stage::SweepBuilder] {
        for i in 0..n {
            let mut row = JournalEntry::new(
                "label.transition",
                REPO,
                as_of() - Duration::days(1),
                &provenance(),
            );
            row.stage = Some(stage);
            row.entered_at = Some(as_of() - Duration::days(1) - Duration::seconds(600));
            row.left_at = Some(as_of() - Duration::days(1));
            row.duration_sec = Some(600 + i64::try_from(i).unwrap_or(0));
            rows.push(row);
        }
    }
    history.push_journal(&rows, host);
    history
}

fn write_snapshot(root: &Path, snapshot: &FleetSnapshot) -> std::path::PathBuf {
    let path = fleet::snapshot_path(root, &snapshot.repo);
    fleet::write(&path, snapshot).expect("cache write");
    path
}

// -- producer -------------------------------------------------------------

#[test]
fn every_fleet_sample_is_attributed_to_the_forge_and_the_timeline() {
    let history = fleet_snapshot().stage_samples();
    assert_eq!(history.scope, HistoryScope::Fleet);
    assert!(!history.stages.is_empty());
    assert!(history
        .stages
        .iter()
        .chain(history.censored.iter())
        .all(|s| s.source == SampleSource::ForgeTimeline && s.host == FORGE_HOST));
    // No host recorded these, so the attribution must not look like a host id.
    assert_eq!(FORGE_HOST, "forge");
    // The forge cannot see inside a sweep: no in-sweep phase samples, and no
    // merged-in-sweep path evidence to fabricate a merge share from.
    assert!(history
        .stages
        .iter()
        .all(|s| !matches!(s.stage, Stage::SweepCurator | Stage::SweepBuilder)));
    assert!(history.paths.is_empty());
}

#[test]
fn the_producer_derives_the_same_three_segments_the_local_backfill_does() {
    // One rejecting PR: review_wait (to the rejection), doctor (to the push),
    // review_wait (to the approval), merge_wait (to the merge).
    let h = pr(7, 0, true, 0);
    let samples = fleet::samples_from_pr_history(&h, REPO, t(20_000));
    let mut by_stage: std::collections::BTreeMap<&str, Vec<i64>> =
        std::collections::BTreeMap::new();
    for s in &samples {
        by_stage
            .entry(s.stage.as_str())
            .or_default()
            .push(s.duration_sec);
    }
    assert_eq!(by_stage["review_wait"], vec![1800, 3500]);
    assert_eq!(by_stage["doctor"], vec![3600]);
    assert_eq!(by_stage["merge_wait"], vec![1800]);
    // The attempt-numbered verdicts ride on the review_wait rows, which is
    // what feeds `land-v1`'s changes-requested branch.
    let verdicts: Vec<(Option<&str>, Option<u32>)> = samples
        .iter()
        .filter(|s| s.stage == Stage::ReviewWait)
        .map(|s| (s.verdict.as_deref(), s.attempt))
        .collect();
    assert!(verdicts.contains(&(Some("fail"), Some(1))));
    assert!(verdicts.contains(&(Some("pass"), Some(2))));
    assert!(samples.iter().all(|s| !s.censored));
}

#[test]
fn an_open_segment_is_censored_and_never_reaches_a_v1_distribution() {
    // Approved but unmerged — the merge-risk-hold shape.
    let h = PrHistory::new(
        42,
        t(0),
        PrState::Open,
        None,
        vec![APPROVED.to_string()],
        vec![labeled(REVIEW_REQUESTED, 0), labeled(APPROVED, 3600)],
        true,
    );
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(&[h], t(10_800));
    let history = snapshot.stage_samples();
    // review_wait completed at the approval; merge_wait is still open.
    assert_eq!(history.stages.len(), 1);
    assert_eq!(history.stages[0].stage, Stage::ReviewWait);
    assert_eq!(history.censored.len(), 1);
    assert_eq!(history.censored[0].stage, Stage::MergeWait);
    assert_eq!(history.censored[0].duration_sec, 7200);
    // A censored row carries no verdict: it settled nothing.
    assert!(snapshot
        .samples
        .iter()
        .filter(|s| s.censored)
        .all(|s| s.verdict.is_none()));
}

#[test]
fn a_snapshot_never_holds_a_sample_observed_after_its_own_as_of() {
    let snapshot = fleet_snapshot();
    assert!(snapshot
        .samples
        .iter()
        .all(|s| s.observed_at <= snapshot.as_of));
    // …and an estimate at an earlier instant cannot see the later ones, which
    // is what keeps #9325's replay leak-free over a fleet snapshot.
    let history = snapshot.stage_samples();
    let early = t(0) + Duration::hours(12);
    let selected = history.select(REPO, Stage::ReviewWait, early, &[SampleSource::StageJournal]);
    assert!(
        selected.is_none_or(|s| s.sorted.len() < history.stages.len()),
        "a replay at {early} must not see the whole snapshot"
    );
}

// -- the headline: fleet history answers where local history cannot ---------

#[test]
fn land_v1_refuses_on_an_empty_local_history_and_answers_on_the_fleet_snapshot() {
    let local = StageSamples::default();
    let refused = LandV1.estimate(&input(), &local);
    assert_eq!(refused.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
    assert!(refused.result.is_none());

    let fleet_history = fleet_snapshot().stage_samples();
    let answered = LandV1.estimate(&input(), &fleet_history);
    assert_eq!(answered.no_estimate_reason, None, "{answered:?}");
    let result = answered
        .result
        .as_ref()
        .expect("a fleet estimate has a result");
    assert!(result.p50_sec > 0);
    assert!(result.p25_sec <= result.p50_sec && result.p50_sec <= result.p75_sec);
}

#[test]
fn a_fleet_estimate_records_scope_sources_and_per_host_counts() {
    let explanation = LandV1.estimate(&input(), &fleet_snapshot().stage_samples());
    let record = explanation
        .history
        .as_ref()
        .expect("every estimate records its history");
    assert_eq!(record.scope, HistoryScope::Fleet);
    assert_eq!(HistoryScope::Fleet.as_str(), "fleet");
    // `sources` stays the heuristic's own filter, verbatim…
    assert_eq!(
        record.sources,
        vec![
            "sweep-outcome-telemetry.jsonl".to_string(),
            "eta-stage-samples.jsonl".to_string()
        ]
    );
    // …while the attribution says what was actually read.
    assert_eq!(record.samples_by_source.keys().collect::<Vec<_>>(), vec!["forge:pr-timeline"]);
    assert_eq!(record.samples_by_host.keys().collect::<Vec<_>>(), vec![FORGE_HOST]);
    let total: usize = record.samples_by_source.values().sum();
    assert_eq!(total, record.samples_by_host.values().sum::<usize>());
    assert!(total >= 3 * crate::eta::MIN_SAMPLES);
}

#[test]
fn augmenting_a_local_history_reports_fleet_scope_and_both_attributions() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &fleet_snapshot());
    let local = thin_local_history("host-worker-1", 10);
    let merged = fleet::apply_scope(HistoryScopeMode::Augment, dir.path(), local);
    assert_eq!(merged.scope, HistoryScope::Fleet);

    let curator_input = EstimateInput {
        current: CurrentState::At(CurrentStage {
            stage: Stage::SweepCurator,
            entered_at: Some(as_of()),
            age_sec: 0,
            age_source: AgeSource::LabelEvent,
            rework_rounds: 0,
        }),
        ..input()
    };
    let explanation = LandV1.estimate(&curator_input, &merged);
    let record = explanation.history.as_ref().unwrap();
    assert_eq!(record.scope, HistoryScope::Fleet);
    // `sweep.curator` came from the host, the rest from the forge — which is
    // exactly the question `samples_by_host` exists to answer.
    let hosts: Vec<&String> = record.samples_by_host.keys().collect();
    assert_eq!(hosts, vec!["forge", "host-worker-1"]);
}

// -- determinism across hosts ---------------------------------------------

/// Serialize the way a comparison between two hosts would: whole explanation,
/// canonical JSON, no field skipped.
fn bytes(explanation: &Explanation) -> Vec<u8> {
    serde_json::to_vec(explanation).expect("an explanation serializes")
}

#[test]
fn two_hosts_at_the_same_snapshot_produce_byte_identical_output() {
    let prs = fleet_prs();

    // Host A backfills in one pass, in listing order.
    let host_a = tempfile::tempdir().unwrap();
    let mut snapshot_a = FleetSnapshot::empty(REPO);
    snapshot_a.merge(&prs, as_of());
    let path_a = write_snapshot(host_a.path(), &snapshot_a);

    // Host B gets there differently: newest-first, in two batches, on a host
    // that also has its own (irrelevant) local journal.
    let host_b = tempfile::tempdir().unwrap();
    let mut reversed = prs.clone();
    reversed.reverse();
    let (first, second) = reversed.split_at(7);
    let mut snapshot_b = FleetSnapshot::empty(REPO);
    snapshot_b.merge(first, as_of());
    snapshot_b.merge(second, as_of());
    let path_b = write_snapshot(host_b.path(), &snapshot_b);

    // Same snapshot id, and the same bytes on disk.
    assert_eq!(snapshot_a.snapshot_id, snapshot_b.snapshot_id);
    assert_eq!(snapshot_a, snapshot_b);
    assert_eq!(
        std::fs::read(&path_a).unwrap(),
        std::fs::read(&path_b).unwrap(),
        "a snapshot file is a portable artifact"
    );

    // Estimating at `fleet` scope, each host reads only the snapshot — host B's
    // own journal must not move the answer by a byte.
    let history_a =
        fleet::apply_scope(HistoryScopeMode::Fleet, host_a.path(), thin_local_history("host-a", 4));
    let history_b = fleet::apply_scope(
        HistoryScopeMode::Fleet,
        host_b.path(),
        thin_local_history("host-b", 31),
    );
    let a = LandV1.estimate(&input(), &history_a);
    let b = LandV1.estimate(&input(), &history_b);
    assert_eq!(a.history, b.history, "identical HistoryRecord");
    assert_eq!(bytes(&a), bytes(&b), "identical estimate output");
    assert!(a.result.is_some());
}

#[test]
fn the_snapshot_id_is_derived_from_content_not_from_the_host_or_the_clock() {
    let snapshot = fleet_snapshot();
    // Deriving twice from the same inputs gives the same id (no randomness).
    assert_eq!(snapshot.snapshot_id, fleet_snapshot().snapshot_id);
    assert_eq!(snapshot.snapshot_id.len(), 16);
    assert!(snapshot.snapshot_id.chars().all(|c| c.is_ascii_hexdigit()));

    // One more PR is a different snapshot.
    let mut more = snapshot.clone();
    more.merge(&[pr(999, 21 * 6 * 3600, false, 0)], as_of());
    assert_ne!(more.snapshot_id, snapshot.snapshot_id);

    // The same samples at a different as-of are a different snapshot too: the
    // as-of is what a censored sample is censored at.
    let mut later = FleetSnapshot::empty(REPO);
    later.merge(&fleet_prs(), as_of() + Duration::hours(1));
    assert_ne!(later.snapshot_id, snapshot.snapshot_id);
}

// -- the incremental cache -------------------------------------------------

#[test]
fn re_reading_a_pr_replaces_its_rows_rather_than_duplicating_them() {
    let prs = fleet_prs();
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(&prs, as_of());
    let once = snapshot.clone();

    // A refresh re-reads everything the search matched, including PRs whose
    // rows are already held.
    snapshot.merge(&prs, as_of());
    assert_eq!(snapshot, once, "an idempotent refresh changes nothing");

    // …and a PR that moved is updated in place, not appended to.
    let reopened = pr(prs[0].number, 0, true, 5000);
    snapshot.merge(&[reopened], as_of());
    assert_eq!(snapshot.prs, once.prs, "the census is unchanged");
    assert_eq!(
        snapshot
            .samples
            .iter()
            .filter(|s| s.pr_number == prs[0].number)
            .count(),
        4,
        "one PR's rows, once"
    );
    assert_ne!(snapshot.snapshot_id, once.snapshot_id);
}

#[test]
fn an_open_segment_becomes_a_completed_one_when_the_pr_lands() {
    let open = PrHistory::new(
        55,
        t(0),
        PrState::Open,
        None,
        vec![APPROVED.to_string()],
        vec![labeled(REVIEW_REQUESTED, 0), labeled(APPROVED, 3600)],
        true,
    );
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(&[open], t(10_800));
    assert_eq!(snapshot.stage_samples().censored.len(), 1);

    let landed = PrHistory::new(
        55,
        t(0),
        PrState::Merged,
        Some(t(14_400)),
        Vec::new(),
        vec![
            labeled(REVIEW_REQUESTED, 0),
            labeled(APPROVED, 3600),
            PrEvent::Merged { at: t(14_400) },
        ],
        true,
    );
    snapshot.merge(&[landed], t(18_000));
    let history = snapshot.stage_samples();
    assert!(history.censored.is_empty(), "the stale lower bound is gone");
    assert_eq!(
        history
            .stages
            .iter()
            .filter(|s| s.stage == Stage::MergeWait)
            .map(|s| s.duration_sec)
            .collect::<Vec<_>>(),
        vec![10_800],
        "replaced by the real duration"
    );
}

#[test]
fn an_unreadable_timeline_contributes_nothing_and_evicts_nothing() {
    let good = pr(77, 0, true, 0);
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(std::slice::from_ref(&good), as_of());
    let before = snapshot.clone();

    let partial = PrHistory::new(
        77,
        good.created_at,
        PrState::Merged,
        good.merged_at,
        Vec::new(),
        Vec::new(),
        false, // the timeline read did not answer
    );
    snapshot.merge(&[partial], as_of());
    assert_eq!(
        snapshot, before,
        "a partial log must never be mistaken for a fast PR, nor evict a good one"
    );
}

#[test]
fn samples_past_the_retention_horizon_are_pruned_with_their_pr() {
    let mut snapshot = fleet_snapshot();
    let kept = snapshot.samples.len();
    assert!(kept > 0);
    // Advance the as-of far past every sample's observation instant.
    snapshot.merge(&[], as_of() + Duration::days(RETENTION_DAYS + 2));
    assert!(snapshot.samples.is_empty(), "nothing inside the retention horizon");
    assert!(snapshot.prs.is_empty(), "the census drops a PR it no longer describes");
    assert_eq!(snapshot.cursor, None);
}

#[test]
fn the_cursor_is_the_newest_forge_event_the_snapshot_holds() {
    let snapshot = fleet_snapshot();
    let newest = snapshot
        .samples
        .iter()
        .map(|s| s.observed_at)
        .max()
        .expect("non-empty");
    assert_eq!(snapshot.cursor, Some(newest));
    assert!(newest <= snapshot.as_of);
}

// -- the cache file --------------------------------------------------------

#[test]
fn a_snapshot_round_trips_through_the_cache_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = fleet_snapshot();
    let path = write_snapshot(dir.path(), &snapshot);
    assert!(path.ends_with("fleet-rjwalters-loom.json"), "{}", path.display());
    assert_eq!(fleet::read(&path).as_ref(), Some(&snapshot));
    // …and the estimate over the round-tripped value is the same bytes.
    let direct = LandV1.estimate(&input(), &snapshot.stage_samples());
    let reloaded = LandV1.estimate(&input(), &fleet::read(&path).unwrap().stage_samples());
    assert_eq!(bytes(&direct), bytes(&reloaded));
}

#[test]
fn an_unknown_schema_is_a_refusal_not_a_partial_parse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet-x.json");
    let mut snapshot = fleet_snapshot();
    snapshot.schema = "eta-fleet-snapshot/v99".to_string();
    std::fs::write(&path, serde_json::to_string(&snapshot).unwrap()).unwrap();
    assert_eq!(fleet::read(&path), None);
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(fleet::read(&path), None);
}

#[test]
fn a_slug_can_never_escape_the_cache_directory() {
    assert_eq!(fleet::snapshot_slug("rjwalters/loom"), "rjwalters-loom");
    // `.` survives (it is a legal file-name character), but every separator
    // is folded, so no slug can name a path.
    assert_eq!(fleet::snapshot_slug("../../etc"), "..-..-etc");
    assert!(!fleet::snapshot_slug("a/b\\c:d").contains(['/', '\\', ':']));
    let dir = tempfile::tempdir().unwrap();
    let path = fleet::snapshot_path(dir.path(), "../../etc/passwd");
    assert_eq!(path.parent(), Some(fleet::snapshot_dir(dir.path()).as_path()));
}

#[test]
fn load_all_reads_every_cached_repo_and_load_history_merges_them() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &fleet_snapshot());
    let mut other = FleetSnapshot::empty("rjwalters/other");
    other.merge(&[pr(1, 0, true, 0)], as_of());
    write_snapshot(dir.path(), &other);

    let all = fleet::load_all(dir.path());
    assert_eq!(all.len(), 2);
    let history = fleet::load_history(dir.path()).expect("two snapshots are a history");
    assert_eq!(history.scope, HistoryScope::Fleet);
    let repos: std::collections::BTreeSet<&str> =
        history.stages.iter().map(|s| s.repo.as_str()).collect();
    assert_eq!(repos.into_iter().collect::<Vec<_>>(), vec!["rjwalters/loom", "rjwalters/other"]);
}

// -- scope resolution ------------------------------------------------------

#[test]
fn with_no_snapshot_every_scope_is_the_host_local_history() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(fleet::load_history(dir.path()), None);
    for mode in [
        HistoryScopeMode::Local,
        HistoryScopeMode::Augment,
        HistoryScopeMode::Fleet,
    ] {
        let resolved = fleet::apply_scope(mode, dir.path(), thin_local_history("host-a", 3));
        assert_eq!(resolved, thin_local_history("host-a", 3), "{mode:?}");
        assert_eq!(resolved.scope, HistoryScope::Local, "{mode:?} must not claim fleet scope");
    }
}

#[test]
fn local_scope_ignores_a_cached_snapshot_entirely() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &fleet_snapshot());
    let local = thin_local_history("host-a", 3);
    let resolved = fleet::apply_scope(HistoryScopeMode::Local, dir.path(), local.clone());
    assert_eq!(resolved, local);
    assert_eq!(resolved.scope, HistoryScope::Local);
}

#[test]
fn fleet_scope_drops_the_local_journals() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &fleet_snapshot());
    let resolved =
        fleet::apply_scope(HistoryScopeMode::Fleet, dir.path(), thin_local_history("host-a", 40));
    assert_eq!(resolved.scope, HistoryScope::Fleet);
    assert!(
        resolved
            .stages
            .iter()
            .all(|s| s.source == SampleSource::ForgeTimeline),
        "a named snapshot is the whole input"
    );
}

#[test]
fn the_history_scope_mode_vocabulary_round_trips_and_rejects_typos() {
    for mode in [
        HistoryScopeMode::Local,
        HistoryScopeMode::Augment,
        HistoryScopeMode::Fleet,
    ] {
        assert_eq!(HistoryScopeMode::parse(mode.as_str()), Some(mode));
    }
    assert_eq!(HistoryScopeMode::parse("FLEET"), Some(HistoryScopeMode::Fleet));
    assert_eq!(HistoryScopeMode::parse("global"), None);
    assert_eq!(HistoryScopeMode::default(), HistoryScopeMode::Augment);
}

// -- the estimator stays pure over a snapshot ------------------------------

/// Every shipped heuristic's own source, compiled in. A source scan is a
/// blunt instrument, but it is the only check that fails at the moment a
/// hidden read is *written* rather than at the moment someone notices an
/// estimate stopped being reproducible.
const HEURISTIC_SOURCES: &[(&str, &str)] = &[
    ("heuristics/mod.rs", include_str!("../heuristics/mod.rs")),
    ("heuristics/start_v1.rs", include_str!("../heuristics/start_v1.rs")),
    ("heuristics/finish_v1.rs", include_str!("../heuristics/finish_v1.rs")),
    ("heuristics/land_v1.rs", include_str!("../heuristics/land_v1.rs")),
    ("heuristics/land_v2.rs", include_str!("../heuristics/land_v2.rs")),
    ("heuristics/land_v3.rs", include_str!("../heuristics/land_v3.rs")),
    (
        "heuristics/land_amber_heron.rs",
        include_str!("../heuristics/land_amber_heron.rs"),
    ),
    // #10207: the recalibration fit and transform the heuristic calls.
    ("recalibrate.rs", include_str!("../recalibrate.rs")),
];

/// The whole point of #9343's "the estimator stays pure over a snapshot": a
/// heuristic may read `(input, history)` and nothing else. If it could reach
/// the filesystem, the forge, the environment or the clock, then handing it a
/// fleet snapshot would no longer be the *only* difference between two hosts'
/// answers, and #9325's backtest could not replay one without leaking.
///
/// Fetching and caching live in `eta::fleet` and `cli/eta_fleet_cmd.rs`, on
/// the far side of the `StageSamples` value — which is exactly why this scan
/// covers `heuristics/` and not `history.rs` (whose *local* producer reads
/// this host's journal files by design, outside any estimate).
#[test]
fn no_heuristic_reaches_outside_its_two_arguments() {
    // Substrings, not identifiers: the point is that none of this vocabulary
    // appears at all, so an aliased import cannot sneak past.
    const FORBIDDEN: &[(&str, &str)] = &[
        ("std::fs", "the filesystem"),
        ("fs::", "the filesystem"),
        ("File::", "the filesystem"),
        ("read_to_string", "the filesystem"),
        ("include_str!", "the filesystem"),
        ("Command", "a subprocess (a `gh` call)"),
        ("reqwest", "the network"),
        ("std::env", "the environment"),
        ("env::var", "the environment"),
        ("Utc::now", "the wall clock"),
        ("SystemTime", "the wall clock"),
        ("Instant::now", "the wall clock"),
        ("tokio", "an async runtime"),
        ("fleet::", "the snapshot cache (fetching belongs outside the estimator)"),
    ];
    for (name, source) in HEURISTIC_SOURCES {
        for (needle, what) in FORBIDDEN {
            assert!(
                !source.contains(needle),
                "{name} mentions `{needle}`: a heuristic must not reach {what}. \
                 An estimate is a pure function of (input, history); move the read \
                 into `eta::fleet` or the CLI and hand the result in as StageSamples.",
            );
        }
    }
}

/// The same property from the outside: the *only* thing that moved between
/// these two estimates is the history handed in, so any hidden input a
/// heuristic read would have to show up as a difference here.
#[test]
fn the_same_input_and_history_estimate_identically_twice_over() {
    let history = fleet_snapshot().stage_samples();
    let first = LandV1.estimate(&input(), &history);
    let second = LandV1.estimate(&input(), &history);
    assert_eq!(bytes(&first), bytes(&second));
    // …and a snapshot that round-trips through a *different* host's cache
    // directory is still the same estimate, so nothing path-shaped leaks in.
    let elsewhere = tempfile::tempdir().unwrap();
    write_snapshot(elsewhere.path(), &fleet_snapshot());
    let reloaded = fleet::load_history(elsewhere.path()).expect("one snapshot is a history");
    assert_eq!(bytes(&LandV1.estimate(&input(), &reloaded)), bytes(&first));
}

// -- the source equivalence -------------------------------------------------

#[test]
fn a_stage_journal_filter_admits_a_forge_sample_and_nothing_else_does() {
    use SampleSource::{ForgeTimeline, StageJournal, SweepOutcome};
    assert!(StageJournal.admits(ForgeTimeline));
    assert!(StageJournal.admits(StageJournal));
    assert!(!StageJournal.admits(SweepOutcome));
    assert!(!SweepOutcome.admits(ForgeTimeline));
    assert!(!ForgeTimeline.admits(StageJournal));
    assert!(ForgeTimeline.admits(ForgeTimeline));
    assert_eq!(ForgeTimeline.journal(), "forge:pr-timeline");
}

#[test]
fn a_local_history_is_unchanged_by_the_equivalence() {
    // `finish-v1` reads `sweep.outcome` only, so a fleet snapshot is invisible
    // to it — a forge timeline cannot see inside a sweep and must not be
    // smuggled into an in-sweep distribution.
    let history = fleet_snapshot().stage_samples();
    assert!(history
        .select(REPO, Stage::ReviewWait, as_of(), &[SampleSource::SweepOutcome])
        .is_none());
    // The local fixture history is untouched by the new variant.
    let local = super::history_a();
    let before = local.select(REPO, Stage::ReviewWait, as_of(), &[SampleSource::SweepOutcome]);
    assert!(before.is_some());
    assert!(before
        .unwrap()
        .by_source
        .keys()
        .all(|k| k == "sweep-outcome-telemetry.jsonl"));
}

#[test]
fn merging_raises_the_scope_to_fleet_and_never_lowers_it() {
    let mut local = thin_local_history("host-a", 2);
    assert_eq!(local.scope, HistoryScope::Local);
    local.merge(fleet_snapshot().stage_samples());
    assert_eq!(local.scope, HistoryScope::Fleet);
    local.merge(thin_local_history("host-b", 2));
    assert_eq!(local.scope, HistoryScope::Fleet, "scope never goes back down");
}

// -- a sanity check on the fixture itself ----------------------------------

#[test]
fn the_fixture_clears_the_sample_floor_on_every_stage_land_v1_walks() {
    let history = fleet_snapshot().stage_samples();
    for stage in [Stage::ReviewWait, Stage::Doctor, Stage::MergeWait] {
        let selection = history
            .select(REPO, stage, as_of(), &[SampleSource::StageJournal])
            .unwrap_or_else(|| panic!("{stage} must clear the floor"));
        assert!(selection.sorted.len() >= crate::eta::MIN_SAMPLES, "{stage}");
    }
    let counts = history.verdict_counts(REPO, crate::eta::history::Level::Repo, as_of(), 2);
    assert!(counts[0].0 >= crate::eta::MIN_SAMPLES, "attempt-1 verdicts: {counts:?}");
    assert!(counts[0].1 > 0 && counts[0].1 < counts[0].0, "a mix of verdicts: {counts:?}");
}

/// A guard on the fixture's own instants: every sample must sit inside the
/// history window, or the tests above would pass for the wrong reason.
#[test]
fn every_fixture_sample_sits_inside_the_history_window() {
    let window_from: DateTime<Utc> = crate::eta::history::window_from(as_of());
    let snapshot = fleet_snapshot();
    assert!(snapshot
        .samples
        .iter()
        .all(|s| s.observed_at >= window_from && s.observed_at < as_of()));
}

#[test]
fn merging_into_an_empty_snapshot_at_the_start_of_time_does_not_panic() {
    // `FleetSnapshot::empty` starts at `MIN_UTC`, and the retention floor is a
    // subtraction chrono panics on rather than saturating.
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(&[], DateTime::<Utc>::MIN_UTC);
    assert!(snapshot.samples.is_empty());
    assert_eq!(snapshot.cursor, None);
}
