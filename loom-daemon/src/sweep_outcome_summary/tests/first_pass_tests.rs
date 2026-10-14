//! Issue #8542: the first-pass Judge approval rate every grouping carries. Extracted
//! to a sibling of `tests.rs` to keep it under the file-size ratchet
//! threshold (`scripts/check-file-size-budget.sh`, #7711).

use super::*;

fn with_judge_verdict(mut r: SweepOutcomeRecord, verdict: &str) -> SweepOutcomeRecord {
    r.judge_verdicts = Some(vec![crate::telemetry::JudgeVerdict {
        attempt: 1,
        verdict: verdict.to_string(),
    }]);
    r
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
