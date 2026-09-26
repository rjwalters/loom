//! Issue #8542: `--group-by complexity` / `--group-by model-complexity`, and
//! the first-pass Judge approval rate every grouping now carries. Extracted
//! to a sibling of `tests.rs` to keep it under the file-size ratchet
//! threshold (`scripts/check-file-size-budget.sh`, #7711).

use super::*;

fn with_complexity(mut r: SweepOutcomeRecord, tier: &str) -> SweepOutcomeRecord {
    r.complexity = Some(tier.to_string());
    r
}

fn with_judge_verdict(mut r: SweepOutcomeRecord, verdict: &str) -> SweepOutcomeRecord {
    r.judge_verdicts = Some(vec![crate::telemetry::JudgeVerdict {
        attempt: 1,
        verdict: verdict.to_string(),
    }]);
    r
}

/// AC: `--group-by complexity` buckets by the Curator's tier, and a record
/// with no observed marker lands in `unknown` rather than being dropped.
#[test]
fn group_by_complexity_buckets_by_the_curator_tier() {
    let records = vec![
        entry(
            with_complexity(record("a", "o/r", None, SweepResult::Success, 100), "mechanical"),
            day(10),
            "h1",
        ),
        entry(
            with_complexity(record("b", "o/r", None, SweepResult::Success, 200), "routine"),
            day(10),
            "h1",
        ),
        entry(record("c", "o/r", None, SweepResult::Success, 300), day(10), "h1"),
    ];
    let report = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Complexity),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.records_grouped, 3);
    let groups: std::collections::BTreeSet<&str> =
        report.rows.iter().map(|r| r.group.as_str()).collect();
    assert!(groups.contains("mechanical"));
    assert!(groups.contains("routine"));
    assert!(groups.contains(UNKNOWN_GROUP));
    assert!(report
        .notes
        .iter()
        .any(|n| n.contains("no Curator complexity marker")));
}

/// AC: `--group-by model-complexity` forms the compound `<model>/<tier>` key,
/// with each side folding exactly as its own single-dimension grouping does.
#[test]
fn group_by_model_complexity_forms_the_compound_key() {
    let records = vec![
        entry(
            with_complexity(
                record("a", "o/r", Some("sonnet"), SweepResult::Success, 100),
                "routine",
            ),
            day(10),
            "h1",
        ),
        entry(
            with_complexity(record("b", "o/r", None, SweepResult::Success, 200), "routine"),
            day(10),
            "h1",
        ),
    ];
    let report = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::ModelComplexity),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let groups: std::collections::BTreeSet<&str> =
        report.rows.iter().map(|r| r.group.as_str()).collect();
    assert!(groups.contains("sonnet/routine"));
    assert!(groups.contains("default/routine"));
}

/// AC: first-pass approval rate is `judge_verdicts[0].verdict == "pass"` over
/// the judged sweeps in the group — the routing-evaluation headline.
#[test]
fn first_pass_approval_rate_uses_only_the_first_verdict() {
    let records = vec![
        entry(
            with_judge_verdict(record("a", "o/r", None, SweepResult::Success, 100), "pass"),
            day(10),
            "h1",
        ),
        entry(
            with_judge_verdict(record("b", "o/r", None, SweepResult::Success, 200), "fail"),
            day(10),
            "h1",
        ),
        // No judge_verdicts at all: not judged, must not affect the rate.
        entry(record("c", "o/r", None, SweepResult::Success, 300), day(10), "h1"),
    ];
    let report = summarize(
        &records,
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let row = &report.rows[0];
    assert_eq!(row.first_pass_judged, 2);
    assert!((row.first_pass_approval_rate.unwrap() - 0.5).abs() < 1e-9);
}

/// An observed-but-unjudged PR (`judge_verdicts: Some([])`, the sweep died
/// before Judge) is "not judged", the same "unknown != zero" contract
/// `judge_verdicts` itself uses — it must not deflate the denominator.
#[test]
fn an_observed_empty_verdict_list_does_not_count_as_judged() {
    let mut r = record("a", "o/r", None, SweepResult::Failure, 90);
    r.judge_verdicts = Some(Vec::new());
    let report = summarize(
        &[entry(r, day(10), "h1")],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let row = &report.rows[0];
    assert_eq!(row.first_pass_judged, 0);
    assert_eq!(row.first_pass_approval_rate, None);
}

/// A group with no judged sweeps at all reports `None`, never a fabricated
/// `0.0` — and the rendered table shows `-`, not `0.0%`.
#[test]
fn a_group_with_no_judged_sweeps_reports_none_not_zero() {
    let report = summarize(
        &[entry(
            record("a", "o/r", None, SweepResult::Success, 100),
            day(10),
            "h1",
        )],
        &SpawnDeathIndex::default(),
        opts(GroupBy::Repo),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    assert_eq!(report.rows[0].first_pass_approval_rate, None);
    let text = render_text(&report);
    assert!(text.contains("JDG1%"));
    assert!(text.contains("JDG_N"));
}
