//! Unit tests for [`crate::sweep_outcome_summary`] (Issue #8057).
//!
//! Extracted to a sibling file rather than left as an inline `#[cfg(test)]`
//! module: `scripts/check-file-size-budget.sh` counts the whole file
//! including its tests, on the argument that an agent editing the production
//! half still pays for the test half sitting in the same buffer.

use super::*;

use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::{PhaseDuration, RepoVisibility};
use std::sync::atomic::{AtomicUsize, Ordering};

fn record(
    sweep_id: &str,
    repo: &str,
    model: Option<&str>,
    result: SweepResult,
    duration: i64,
) -> SweepOutcomeRecord {
    SweepOutcomeRecord {
        repo: repo.to_string(),
        visibility: RepoVisibility::Private,
        issue: 1,
        sweep_id: sweep_id.to_string(),
        model: model.map(String::from),
        effort: None,
        config: BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: duration,
        result,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
    }
}

fn entry(r: SweepOutcomeRecord, emitted_at: DateTime<Utc>, host: &str) -> SummaryRecord {
    SummaryRecord {
        emitted_at,
        host_id: host.to_string(),
        workspace: "/ws".to_string(),
        record: r,
    }
}

fn day(n: u32) -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(
        NaiveDate::from_ymd_opt(2026, 9, n)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        Utc,
    )
}

fn opts(group_by: GroupBy) -> SummaryOptions {
    SummaryOptions {
        group_by,
        since: None,
        exclude_spawn_deaths: false,
        merge_join_attempted: true,
    }
}

/// Answers `NotMerged` for everything — used when the merge join is not
/// what the test is about.
struct NoneMerged;
impl MergeLookup for NoneMerged {
    fn merge_state(&mut self, _repo: &str, _pr: u32) -> MergeState {
        MergeState::NotMerged
    }
}

struct AllMerged;
impl MergeLookup for AllMerged {
    fn merge_state(&mut self, _repo: &str, _pr: u32) -> MergeState {
        MergeState::Merged
    }
}

struct ForgeDown;
impl MergeLookup for ForgeDown {
    fn merge_state(&mut self, _repo: &str, _pr: u32) -> MergeState {
        MergeState::Unavailable
    }
}

#[test]
fn group_by_parses_the_five_documented_dimensions() {
    for (spec, expected) in [
        ("arm", GroupBy::Arm),
        ("MODEL", GroupBy::Model),
        ("repo", GroupBy::Repo),
        ("host", GroupBy::Host),
        ("Day", GroupBy::Day),
    ] {
        assert_eq!(GroupBy::parse(spec).unwrap(), expected);
    }
    assert!(GroupBy::parse("phase").is_err());
}

#[test]
fn since_accepts_relative_and_absolute_forms() {
    let now = day(20);
    assert_eq!(parse_since("7d", now).unwrap(), now - Duration::days(7));
    assert_eq!(parse_since("36h", now).unwrap(), now - Duration::hours(36));
    assert_eq!(parse_since("90m", now).unwrap(), now - Duration::minutes(90));
    assert_eq!(parse_since("2w", now).unwrap(), now - Duration::weeks(2));
    assert_eq!(
        parse_since("2026-09-22", now).unwrap(),
        DateTime::<Utc>::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(2026, 9, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            Utc,
        )
    );
    assert!(parse_since("7y", now).is_err());
    assert!(parse_since("", now).is_err());
    assert!(parse_since("soon", now).is_err());
}

/// AC5: group counts sum to the records aggregated, and no record is
/// dropped for want of a group key.
#[test]
fn group_counts_sum_to_records_grouped() {
    let mut records = Vec::new();
    for i in 0..7u32 {
        let mut r =
            record(&format!("s{i}"), "o/r", Some("claude-opus-5"), SweepResult::Success, 600);
        if i % 2 == 0 {
            r.config.insert("arm".into(), "A".into());
        }
        records.push(entry(r, day(10 + i), "h1"));
    }
    let report = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Arm),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.records_read, 7);
    assert_eq!(report.records_grouped, 7);
    assert_eq!(report.rows.iter().map(|r| r.sweeps).sum::<usize>(), 7);
    // 4 stamped (i = 0,2,4,6) + 3 unstamped.
    let unknown = report
        .rows
        .iter()
        .find(|r| r.group == UNKNOWN_GROUP)
        .unwrap();
    assert_eq!(unknown.sweeps, 3);
    assert_eq!(unknown.arm_source, Some(ArmSource::Unknown));
}

/// AC2/AC4: an explicit stamp wins over the inferred one, and the source
/// is reported.
#[test]
fn explicit_arm_beats_inferred_and_is_marked() {
    let mut explicit = record("s1", "o/r", Some("claude-sonnet-5"), SweepResult::Success, 100);
    explicit.config.insert("arm".into(), "B".into());
    explicit
        .config
        .insert("experiment_arm".into(), "control".into());
    let (arm, source) = resolve_arm(&explicit);
    assert_eq!(arm, "control");
    assert_eq!(source, ArmSource::Explicit);

    let mut inferred = record("s2", "o/r", Some("claude-opus-5"), SweepResult::Success, 100);
    inferred.config.insert("arm".into(), "A".into());
    let (arm, source) = resolve_arm(&inferred);
    assert_eq!(arm, "A");
    assert_eq!(source, ArmSource::Inferred);

    let bare = record("s3", "o/r", None, SweepResult::Success, 100);
    let (arm, source) = resolve_arm(&bare);
    assert_eq!(arm, UNKNOWN_GROUP);
    assert_eq!(source, ArmSource::Unknown);

    let report = summarize(
        &[
            entry(explicit, day(10), "h1"),
            entry(inferred, day(10), "h1"),
            entry(bare, day(10), "h1"),
        ],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Arm),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let by = |g: &str| {
        report
            .rows
            .iter()
            .find(|r| r.group == g)
            .unwrap()
            .arm_source
    };
    assert_eq!(by("control"), Some(ArmSource::Explicit));
    assert_eq!(by("A"), Some(ArmSource::Inferred));
    assert_eq!(by(UNKNOWN_GROUP), Some(ArmSource::Unknown));
    assert!(report.notes.iter().any(|n| n.contains("INFERRED")));
}

/// AC3 + the sibling-journal fallback: a `< 60 s` failure and a
/// classified failure are both spawn deaths; a genuine 20-minute failure
/// is not, and neither is a fast success.
#[test]
fn spawn_death_detection_covers_class_and_duration() {
    let short = record("short", "o/r", None, SweepResult::Failure, 12);
    let classified = record("classified", "o/r", None, SweepResult::Failure, 1200);
    let genuine = record("genuine", "o/r", None, SweepResult::Failure, 1200);
    let fast_success = record("fast", "o/r", None, SweepResult::Success, 12);

    let mut index = SpawnDeathIndex::default();
    index.absorb(&[
        sweep_outcomes::OutcomeRecord {
            timestamp: day(10),
            repo: "o/r".into(),
            issue: 1,
            sweep_id: "classified".into(),
            outcome: "exited".into(),
            exit_code: None,
            death_class: Some("preflight-token-selection-failed".into()),
            crash_classification: None,
            token_name: "acct".into(),
            credential: None,
            duration_sec: 1200,
        },
        sweep_outcomes::OutcomeRecord {
            timestamp: day(10),
            repo: "o/r".into(),
            issue: 2,
            sweep_id: "genuine".into(),
            outcome: "crashed".into(),
            exit_code: Some(1),
            death_class: None,
            crash_classification: Some("execution-error".into()),
            token_name: "acct".into(),
            credential: None,
            duration_sec: 1200,
        },
    ]);

    assert_eq!(spawn_death_reason(&short, &index), Some(SpawnDeathReason::TooShort(12)));
    assert_eq!(
        spawn_death_reason(&classified, &index),
        Some(SpawnDeathReason::Class("preflight-token-selection-failed".into()))
    );
    assert_eq!(spawn_death_reason(&genuine, &index), None);
    assert_eq!(spawn_death_reason(&fast_success, &index), None);

    // account-exhausted:* matches by prefix.
    let mut exhausted_index = SpawnDeathIndex::default();
    exhausted_index.absorb(&[sweep_outcomes::OutcomeRecord {
        timestamp: day(10),
        repo: "o/r".into(),
        issue: 3,
        sweep_id: "exhausted".into(),
        outcome: "crashed".into(),
        exit_code: None,
        death_class: None,
        crash_classification: Some("account-exhausted:rate-limited".into()),
        token_name: "acct".into(),
        credential: None,
        duration_sec: 900,
    }]);
    let exhausted = record("exhausted", "o/r", None, SweepResult::Failure, 900);
    assert!(matches!(
        spawn_death_reason(&exhausted, &exhausted_index),
        Some(SpawnDeathReason::Class(_))
    ));
}

/// The #8056 forward seam: a `failure_class` on the record wins over the
/// sibling-journal join without any other change.
#[test]
fn failure_class_config_seam_supersedes_the_sibling_join() {
    let mut r = record("s1", "o/r", None, SweepResult::Failure, 1200);
    r.config
        .insert("failure_class".into(), "account-exhausted:model-limit".into());
    assert_eq!(
        spawn_death_reason(&r, &SpawnDeathIndex::default()),
        Some(SpawnDeathReason::Class("account-exhausted:model-limit".into()))
    );
}

/// AC3: exclusion is counted and reported, never silent — and the same
/// records are discounted from `real_failure_rate` when not excluded.
#[test]
fn exclusion_is_reported_and_real_failure_rate_discounts_spawn_deaths() {
    let records = vec![
        entry(record("a", "o/r", None, SweepResult::Success, 600), day(10), "h1"),
        entry(record("b", "o/r", None, SweepResult::Failure, 1200), day(10), "h1"),
        entry(record("c", "o/r", None, SweepResult::Failure, 10), day(10), "h1"),
        entry(record("d", "o/r", None, SweepResult::Failure, 5), day(10), "h1"),
    ];

    let kept = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(kept.spawn_deaths_excluded, 0);
    assert_eq!(kept.records_grouped, 4);
    let row = &kept.rows[0];
    assert_eq!(row.failure, 3);
    assert_eq!(row.spawn_deaths, 2);
    assert_eq!(row.real_failures, 1);
    assert!((row.real_failure_rate - 0.25).abs() < 1e-9);

    let excluded = summarize(
        &records,
        &SpawnDeathIndex::default(),
        SummaryOptions {
            exclude_spawn_deaths: true,
            ..opts(GroupBy::Repo)
        },
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(excluded.spawn_deaths_excluded, 2);
    assert_eq!(excluded.records_grouped, 2);
    assert_eq!(excluded.rows.iter().map(|r| r.sweeps).sum::<usize>(), excluded.records_grouped);
    assert!((excluded.rows[0].real_failure_rate - 0.5).abs() < 1e-9);
}

/// Median/p75 are computed over successes only, and both are `None` — not
/// `0` — for a group with no successes.
#[test]
fn median_and_p75_use_successes_only() {
    assert_eq!(median_sorted(&[]), None);
    assert_eq!(median_sorted(&[5]), Some(5));
    assert_eq!(median_sorted(&[1, 3]), Some(2));
    assert_eq!(median_sorted(&[1, 3, 100]), Some(3));
    assert_eq!(p75_sorted(&[]), None);
    assert_eq!(p75_sorted(&[5]), Some(5));
    assert_eq!(p75_sorted(&[10, 20, 30, 40]), Some(30));
    assert_eq!(p75_sorted(&[10, 20, 30, 40, 50]), Some(40));

    let records = vec![
        entry(record("a", "o/r", None, SweepResult::Success, 100), day(10), "h1"),
        entry(record("b", "o/r", None, SweepResult::Success, 300), day(10), "h1"),
        // A 9999s FAILURE must not drag the success median.
        entry(record("c", "o/r", None, SweepResult::Failure, 9999), day(10), "h1"),
    ];
    let report = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.rows[0].median_success_duration_sec, Some(200));
    assert_eq!(report.rows[0].p75_success_duration_sec, Some(300));

    let failures_only = vec![entry(
        record("z", "o/r", None, SweepResult::Failure, 4242),
        day(10),
        "h1",
    )];
    let report = summarize(
        &failures_only,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.rows[0].median_success_duration_sec, None);
    assert_eq!(report.rows[0].p75_success_duration_sec, None);
}

/// Weighted tokens are asserted against `ModelPricing::for_model` itself,
/// never hardcoded constants — so a #8060 rate refresh does not break
/// this test, which is the point.
#[test]
fn weighted_tokens_price_through_the_named_rate_card() {
    let mut r = record("s1", "o/r", Some("claude-opus-5"), SweepResult::Success, 600);
    r.tokens_by_model = Some(vec![
        ModelUsageTotals {
            model: "claude-opus-5".into(),
            speed: "standard".into(),
            service_tier: "standard".into(),
            input: 10_000,
            cache_read: 5_000,
            cache_write_5m: 1_000,
            cache_write_1h: 2_000,
            output: 4_000,
        },
        ModelUsageTotals {
            model: "claude-haiku-4".into(),
            speed: "standard".into(),
            service_tier: "standard".into(),
            input: 1_000,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 500,
        },
    ]);
    let expected =
        ModelPricing::for_model("claude-opus-5").calculate_cost(
            10_000,
            4_000,
            Some(5_000),
            Some(3_000),
        ) + ModelPricing::for_model("claude-haiku-4").calculate_cost(1_000, 500, Some(0), Some(0));
    let got = weighted_tokens_usd(&r).unwrap();
    assert!((got - expected).abs() < 1e-12, "{got} != {expected}");

    // "unknown != zero": no tokens_by_model means None, not 0.0.
    let bare = record("s2", "o/r", None, SweepResult::Success, 600);
    assert_eq!(weighted_tokens_usd(&bare), None);
}

/// The hand-sync obligation documented at `resource_usage.rs:57-64`: the
/// experiment harvest's `model_pricing` must agree with the card this
/// summary names, or the two reports disagree for reasons that are not in
/// the data.
#[test]
fn the_two_rate_cards_agree() {
    for (alias, pinned) in [
        ("sonnet", "claude-sonnet-5"),
        ("opus", "claude-opus-5"),
        ("haiku", "claude-haiku-4"),
        ("fable", "claude-fable-1"),
    ] {
        let via_experiment = crate::script_helpers::sweep_experiment::model_pricing(Some(alias));
        let card = ModelPricing::for_model(pinned);
        assert!(
            (via_experiment.0 - card.input_cost_per_1k).abs() < 1e-12
                && (via_experiment.1 - card.output_cost_per_1k).abs() < 1e-12
                && (via_experiment.2 - card.cache_read_cost_per_1k).abs() < 1e-12
                && (via_experiment.3 - card.cache_write_cost_per_1k).abs() < 1e-12,
            "rate cards disagree for {alias}: sweep_experiment={via_experiment:?} \
             resource_usage={card:?}"
        );
    }
}

/// AC6: a forge failure yields `null`, never `0`, and says so.
#[test]
fn forge_failure_reports_unavailable_never_zero() {
    let mut r = record("s1", "o/r", None, SweepResult::Success, 600);
    r.pr_number = Some(42);
    let records = vec![entry(r, day(10), "h1")];

    let down = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut ForgeDown,
        vec!["/ws".into()],
    );
    assert_eq!(down.rows[0].prs_opened, 1);
    assert_eq!(down.rows[0].merged_prs, None);
    assert_eq!(down.rows[0].merge_state_unknown, 1);
    assert_eq!(down.rows[0].merges_per_weighted_token, None);
    assert!(down.merge_join.degraded);
    assert!(down.notes.iter().any(|n| n.contains("NOT zero")));

    let skipped = summarize(
        &records,
        &SpawnDeathIndex::default(),
        SummaryOptions {
            merge_join_attempted: false,
            ..opts(GroupBy::Repo)
        },
        &mut SkipMergeJoin,
        vec!["/ws".into()],
    );
    assert_eq!(skipped.rows[0].merged_prs, None);
    assert!(skipped.notes.iter().any(|n| n.contains("--no-merge-join")));
}

/// The headline metric and the over-building guardrail, end to end.
#[test]
fn merges_per_weighted_token_and_lines_per_merged_pr() {
    let mut a = record("a", "o/r", Some("claude-opus-5"), SweepResult::Success, 600);
    a.pr_number = Some(1);
    a.lines_added = Some(300);
    a.lines_deleted = Some(100);
    a.tokens_by_model = Some(vec![ModelUsageTotals {
        model: "claude-opus-5".into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input: 100_000,
        cache_read: 0,
        cache_write_5m: 0,
        cache_write_1h: 0,
        output: 0,
    }]);
    // Second merged PR with NO lines fields — must shrink the mean's
    // denominator, not be averaged in as a zero.
    let mut b = record("b", "o/r", Some("claude-opus-5"), SweepResult::Success, 600);
    b.pr_number = Some(2);

    let report = summarize(
        &[entry(a, day(10), "h1"), entry(b, day(10), "h1")],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut AllMerged,
        vec!["/ws".into()],
    );
    let row = &report.rows[0];
    assert_eq!(row.merged_prs, Some(2));
    assert_eq!(row.lines_per_merged_pr_denominator, 1);
    assert_eq!(row.lines_added_per_merged_pr, Some(300.0));
    assert_eq!(row.lines_deleted_per_merged_pr, Some(100.0));
    assert_eq!(row.weighted_tokens_records, 1);
    let usd = ModelPricing::for_model("claude-opus-5").calculate_cost(100_000, 0, Some(0), Some(0));
    assert!((row.weighted_tokens_usd.unwrap() - usd).abs() < 1e-12);
    assert!((row.merges_per_weighted_token.unwrap() - 2.0 / usd).abs() < 1e-9);
}

#[test]
fn doctor_rate_reads_phase_durations_and_the_cycles_seam() {
    let mut phased = record("a", "o/r", None, SweepResult::Success, 600);
    phased.phase_durations = vec![
        PhaseDuration {
            phase: "builder".into(),
            duration_sec: 300,
        },
        PhaseDuration {
            phase: "doctor".into(),
            duration_sec: 120,
        },
    ];
    assert!(doctor_engaged(&phased));

    let clean = record("b", "o/r", None, SweepResult::Success, 600);
    assert!(!doctor_engaged(&clean));

    let mut seam = record("c", "o/r", None, SweepResult::Success, 600);
    seam.config.insert("doctor_cycles".into(), "2".into());
    assert!(doctor_engaged(&seam));
    seam.config.insert("doctor_cycles".into(), "0".into());
    assert!(!doctor_engaged(&seam));

    let report = summarize(
        &[entry(phased, day(10), "h1"), entry(clean, day(10), "h1")],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert!((report.rows[0].doctor_phase_rate - 0.5).abs() < 1e-9);
}

#[test]
fn since_window_filters_and_day_grouping_uses_emitted_at() {
    let records = vec![
        entry(record("a", "o/r", None, SweepResult::Success, 600), day(10), "h1"),
        entry(record("b", "o/r", None, SweepResult::Success, 600), day(20), "h1"),
        entry(record("c", "o/r", None, SweepResult::Success, 600), day(20), "h1"),
    ];
    let report = summarize(
        &records,
        &SpawnDeathIndex::default(),
        SummaryOptions {
            since: Some(day(15)),
            ..opts(GroupBy::Day)
        },
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.records_read, 3);
    assert_eq!(report.records_in_window, 2);
    assert_eq!(report.records_grouped, 2);
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].group, "2026-09-20");
}

#[test]
fn host_grouping_ships_with_its_degeneracy_note() {
    let report = summarize(
        &[entry(
            record("a", "o/r", None, SweepResult::Success, 600),
            day(10),
            "mac-mini-1",
        )],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Host),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.rows[0].group, "mac-mini-1");
    assert!(report
        .notes
        .iter()
        .any(|n| n.contains("degenerate on a local read")));
}

#[test]
fn json_output_names_the_rate_card_and_carries_arm_source() {
    let mut r = record("a", "o/r", Some("claude-opus-5"), SweepResult::Success, 600);
    r.config.insert("arm".into(), "A".into());
    let report = summarize(
        &[entry(r, day(10), "h1")],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Arm),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
    assert_eq!(json["rate_card"]["id"], RATE_CARD_ID);
    assert_eq!(json["rate_card"]["as_of"], RATE_CARD_AS_OF);
    assert_eq!(json["rows"][0]["arm_source"], "inferred");
    assert_eq!(json["group_by"], "arm");
}

static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn counting_probe(_repo: &str, pr: u32) -> Option<bool> {
    PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
    Some(pr.is_multiple_of(2))
}

/// Separate counter: these tests run in parallel, so one shared static
/// would make each test's assertion depend on the others' scheduling.
static FAILING_PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn failing_probe(_repo: &str, _pr: u32) -> Option<bool> {
    FAILING_PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
    None
}

/// Uncounted probe for tests that care about the answer, not the traffic.
fn plain_probe(_repo: &str, pr: u32) -> Option<bool> {
    Some(pr.is_multiple_of(2))
}

#[test]
fn merge_cache_serves_repeat_lookups_without_touching_the_forge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("merge-cache.json");
    PROBE_CALLS.store(0, Ordering::SeqCst);

    let mut lookup = CachedMergeLookup::new(path.clone(), counting_probe);
    assert_eq!(lookup.merge_state("o/r", 2), MergeState::Merged);
    assert_eq!(lookup.merge_state("o/r", 3), MergeState::NotMerged);
    // Repeat of the MERGED pr is served from memory; the negative one is
    // still inside its TTL, so it is too.
    assert_eq!(lookup.merge_state("o/r", 2), MergeState::Merged);
    assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(lookup.cache_hits(), 1);
    lookup.flush();
    assert!(path.exists());

    // A fresh lookup over the same file makes NO probe calls at all.
    PROBE_CALLS.store(0, Ordering::SeqCst);
    let mut reloaded = CachedMergeLookup::new(path, counting_probe);
    assert_eq!(reloaded.merge_state("o/r", 2), MergeState::Merged);
    assert_eq!(PROBE_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(reloaded.probe_calls(), 0);
}

#[test]
fn merge_cache_latches_a_dead_forge_after_one_failure() {
    let dir = tempfile::tempdir().unwrap();
    FAILING_PROBE_CALLS.store(0, Ordering::SeqCst);
    let mut lookup = CachedMergeLookup::new(dir.path().join("c.json"), failing_probe);
    assert_eq!(lookup.merge_state("o/r", 1), MergeState::Unavailable);
    assert_eq!(lookup.merge_state("o/r", 2), MergeState::Unavailable);
    assert_eq!(lookup.merge_state("o/r", 3), MergeState::Unavailable);
    assert_eq!(
        FAILING_PROBE_CALLS.load(Ordering::SeqCst),
        1,
        "one dead forge must cost exactly one subprocess, not one per PR"
    );
    assert!(lookup.forge_failed());
}

#[test]
fn corrupt_cache_file_is_treated_as_empty_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    std::fs::write(&path, "{not json").unwrap();
    let mut lookup = CachedMergeLookup::new(path, plain_probe);
    assert_eq!(lookup.merge_state("o/r", 2), MergeState::Merged);
}

#[test]
fn empty_input_renders_without_panicking() {
    let report = summarize(
        &[],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Model),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.records_grouped, 0);
    assert!(report.rows.is_empty());
    let text = render_text(&report);
    assert!(text.contains("No sweep.outcome records matched."));
}

/// Write one `sweep.outcome` envelope into `<root>/.loom/logs/`, exactly
/// as the daemon's exporter does, so the fixture exercises the real
/// reader rather than a constructor.
fn write_envelope(root: &Path, emitted_at: DateTime<Utc>, host: &str, r: &SweepOutcomeRecord) {
    let path = sweep_outcomes::default_outcome_telemetry_path(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let envelope = serde_json::json!({
        "schema_version": crate::telemetry::CURRENT_SCHEMA_VERSION,
        "emitted_at": emitted_at,
        "host_id": host,
        "record": TelemetryRecord::SweepOutcome(r.clone()),
    });
    let mut line = serde_json::to_string(&envelope).unwrap();
    line.push('\n');
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    existing.push_str(&line);
    std::fs::write(&path, existing).unwrap();
}

fn write_sibling(root: &Path, records: &[sweep_outcomes::OutcomeRecord]) {
    let path = sweep_outcomes::default_outcomes_path(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = String::new();
    for r in records {
        text.push_str(&serde_json::to_string(r).unwrap());
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
}

/// The fleet case, end to end over two real on-disk workspaces: journals
/// are read through the shipped reader, the sibling-journal spawn-death
/// join crosses workspace boundaries, and the group counts still sum.
#[test]
fn two_workspace_fixture_aggregates_across_both_journals() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();

    let mut opus = record("s-opus", "o/alpha", Some("claude-opus-5"), SweepResult::Success, 900);
    opus.config.insert("arm".into(), "A".into());
    opus.pr_number = Some(11);
    opus.lines_added = Some(120);
    opus.lines_deleted = Some(20);
    opus.tokens_by_model = Some(vec![ModelUsageTotals {
        model: "claude-opus-5".into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input: 50_000,
        cache_read: 0,
        cache_write_5m: 0,
        cache_write_1h: 0,
        output: 10_000,
    }]);
    write_envelope(a.path(), day(20), "host-1", &opus);

    let mut sonnet =
        record("s-sonnet", "o/beta", Some("claude-sonnet-5"), SweepResult::Failure, 1800);
    sonnet.config.insert("arm".into(), "B".into());
    write_envelope(b.path(), day(21), "host-1", &sonnet);

    // A spawn death in workspace B, classified only in the SIBLING
    // journal — the fallback path this ships with until #8056.
    let mut dead = record("s-dead", "o/beta", Some("claude-sonnet-5"), SweepResult::Failure, 900);
    dead.config.insert("arm".into(), "B".into());
    write_envelope(b.path(), day(21), "host-1", &dead);
    write_sibling(
        b.path(),
        &[sweep_outcomes::OutcomeRecord {
            timestamp: day(21),
            repo: "o/beta".into(),
            issue: 9,
            sweep_id: "s-dead".into(),
            outcome: "exited".into(),
            exit_code: None,
            death_class: Some("preflight-token-selection-failed".into()),
            crash_classification: None,
            token_name: "acct".into(),
            credential: None,
            duration_sec: 900,
        }],
    );

    // An out-of-window record that --since must drop.
    let old = record("s-old", "o/alpha", Some("claude-opus-5"), SweepResult::Success, 100);
    write_envelope(a.path(), day(1), "host-1", &old);

    let roots = vec![a.path().to_path_buf(), b.path().to_path_buf()];
    let report = summarize_workspaces(
        &roots,
        SummaryOptions {
            group_by: GroupBy::Arm,
            since: Some(day(15)),
            exclude_spawn_deaths: true,
            merge_join_attempted: true,
        },
        &mut AllMerged,
    );

    assert_eq!(report.workspaces.len(), 2);
    assert_eq!(report.records_read, 4);
    assert_eq!(report.records_in_window, 3);
    assert_eq!(report.spawn_deaths_excluded, 1, "the classified death is excluded");
    assert_eq!(report.records_grouped, 2);
    assert_eq!(report.rows.iter().map(|r| r.sweeps).sum::<usize>(), 2);

    let arm_a = report.rows.iter().find(|r| r.group == "A").unwrap();
    assert_eq!(arm_a.success, 1);
    assert_eq!(arm_a.merged_prs, Some(1));
    assert_eq!(arm_a.lines_added_per_merged_pr, Some(120.0));
    assert_eq!(arm_a.arm_source, Some(ArmSource::Inferred));
    assert!(arm_a.weighted_tokens_usd.unwrap() > 0.0);
    assert!(arm_a.merges_per_weighted_token.unwrap() > 0.0);

    let arm_b = report.rows.iter().find(|r| r.group == "B").unwrap();
    assert_eq!(arm_b.failure, 1);
    assert_eq!(arm_b.real_failures, 1, "the genuine 30-minute failure still counts");
    assert!((arm_b.real_failure_rate - 1.0).abs() < 1e-9);
    assert_eq!(arm_b.weighted_tokens_usd, None, "unknown tokens are not zero");

    // Without the exclusion, the death is kept but discounted.
    let kept = summarize_workspaces(
        &roots,
        SummaryOptions {
            group_by: GroupBy::Arm,
            since: Some(day(15)),
            exclude_spawn_deaths: false,
            merge_join_attempted: true,
        },
        &mut AllMerged,
    );
    assert_eq!(kept.spawn_deaths_excluded, 0);
    assert_eq!(kept.records_grouped, 3);
    let arm_b = kept.rows.iter().find(|r| r.group == "B").unwrap();
    assert_eq!(arm_b.sweeps, 2);
    assert_eq!(arm_b.failure, 2);
    assert_eq!(arm_b.spawn_deaths, 1);
    assert_eq!(arm_b.real_failures, 1);
    assert!((arm_b.real_failure_rate - 0.5).abs() < 1e-9);
}

/// A workspace root with no journal at all contributes nothing and is not
/// an error — the 48-workspace host has plenty of these.
#[test]
fn a_workspace_with_no_journal_is_not_an_error() {
    let empty = tempfile::tempdir().unwrap();
    let report = summarize_workspaces(
        &[empty.path().to_path_buf()],
        SummaryOptions {
            group_by: GroupBy::Model,
            since: None,
            exclude_spawn_deaths: false,
            merge_join_attempted: true,
        },
        &mut NoneMerged,
    );
    assert_eq!(report.records_read, 0);
    assert!(report.rows.is_empty());
}

#[test]
fn rendered_text_names_the_card_and_the_relationship_to_the_old_summary() {
    let mut r = record("a", "o/r", Some("claude-opus-5"), SweepResult::Success, 600);
    r.config.insert("arm".into(), "A".into());
    let report = summarize(
        &[entry(r, day(10), "h1")],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Arm),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let text = render_text(&report);
    assert!(text.contains(RATE_CARD_ID));
    assert!(text.contains("as of 2025-01"));
    assert!(text.contains("ARM_SRC"));
    assert!(text.contains("inferred"));
    assert!(text.contains("superset"));
}
