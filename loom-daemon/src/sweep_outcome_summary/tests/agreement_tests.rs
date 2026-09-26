//! Issue #8608: the Curator-vs-Jev tier agreement column on
//! `--group-by complexity` / `--group-by model-complexity`, joined by
//! `sweep_id` from the sibling `sweep-outcomes.jsonl`, and the agree/disagree
//! split of the first-pass Judge approval rate. Extracted to a sibling of
//! `tests.rs` to keep it under the file-size ratchet threshold.

use super::*;

use agreement::{resolve_agreement, Agreement};

fn curated(sweep_id: &str, tier: Option<&str>, verdict: Option<&str>) -> SweepOutcomeRecord {
    let mut r = record(sweep_id, "o/r", None, SweepResult::Success, 600);
    r.complexity = tier.map(String::from);
    r.judge_verdicts = verdict.map(|v| {
        vec![crate::telemetry::JudgeVerdict {
            attempt: 1,
            verdict: v.to_string(),
        }]
    });
    r
}

fn jev(sweep_id: &str, tier: Option<&str>) -> sweep_outcomes::OutcomeRecord {
    sweep_outcomes::OutcomeRecord {
        timestamp: day(10),
        repo: "o/r".into(),
        issue: 1,
        sweep_id: sweep_id.into(),
        outcome: "exited".into(),
        exit_code: Some(0),
        death_class: None,
        crash_classification: None,
        token_name: "acct".into(),
        credential: None,
        jev_tier: tier.map(String::from),
        jev_confidence: tier.map(|_| 0.9),
        tap_usage: None,
        tap_usage_all: Vec::new(),
        duration_sec: 600,
    }
}

/// The fixture shared by the fold tests. Expected, per tier:
/// - `routine`: agree x2 (pass, fail), disagree x1 (fail), unknown x1 (no
///   sibling record, pass) — per-tier JDG1% 2/4, agree 1/2, disagree 0/1.
/// - `mechanical`: agree x1 (pass; Jev tier differs only by case).
/// - `unknown` (no Curator marker, Jev present): unknown x1.
fn fixture() -> (Vec<SummaryRecord>, SpawnDeathIndex) {
    let records = [
        curated("agree-pass", Some("routine"), Some("pass")),
        curated("agree-fail", Some("routine"), Some("fail")),
        curated("disagree-fail", Some("routine"), Some("fail")),
        curated("no-jev", Some("routine"), Some("pass")),
        curated("mech", Some("mechanical"), Some("pass")),
        curated("no-curator", None, None),
    ]
    .into_iter()
    .map(|r| entry(r, day(10), "h1"))
    .collect();
    let mut index = SpawnDeathIndex::default();
    index.absorb(&[
        jev("agree-pass", Some("routine")),
        jev("agree-fail", Some("routine")),
        jev("disagree-fail", Some("complex")),
        jev("mech", Some("MECHANICAL")),
        jev("no-curator", Some("mechanical")),
        // A sweep_id the telemetry journal never saw: must not leak in.
        jev("orphan", Some("complex")),
    ]);
    (records, index)
}

fn row<'a>(report: &'a SummaryReport, group: &str) -> &'a GroupRow {
    report.rows.iter().find(|r| r.group == group).unwrap()
}

/// AC: the `sweep_id` join resolves agree / disagree / unknown, with
/// `unknown` for either side absent.
#[test]
fn join_resolves_agree_disagree_and_unknown() {
    let (records, index) = fixture();
    let label = |id: &str| {
        let r = records.iter().find(|r| r.record.sweep_id == id).unwrap();
        resolve_agreement(&r.record, &index)
    };
    assert_eq!(label("agree-pass"), Agreement::Agree);
    assert_eq!(label("mech"), Agreement::Agree);
    assert_eq!(label("disagree-fail"), Agreement::Disagree);
    // Curator tier present, no matching sibling record.
    assert_eq!(label("no-jev"), Agreement::Unknown);
    // Sibling record present, no Curator tier.
    assert_eq!(label("no-curator"), Agreement::Unknown);
    // A sibling record with no jev_tier (keyless dispatch) is also unknown.
    let mut keyless = SpawnDeathIndex::default();
    keyless.absorb(&[jev("agree-pass", None)]);
    assert_eq!(resolve_agreement(&records[0].record, &keyless), Agreement::Unknown);
    assert_eq!(serde_json::to_value(Agreement::Disagree).unwrap(), "disagree");
}

/// AC: each complexity row carries the agreement counts and the first-pass
/// rate split by agree/disagree — alongside, not instead of, the per-tier
/// first-pass rate (#8542), which must be unchanged.
#[test]
fn complexity_rows_carry_the_split_beside_the_per_tier_rate() {
    let (records, index) = fixture();
    let report =
        summarize(&records, &index, opts(GroupBy::Complexity), &mut NoneMerged, vec!["/ws".into()]);

    let routine = row(&report, "routine");
    // The existing #8542 per-tier rate is untouched: 2 of 4 judged passed.
    assert_eq!(routine.first_pass_judged, 4);
    assert!((routine.first_pass_approval_rate.unwrap() - 0.5).abs() < 1e-9);
    let split = routine.agreement.as_ref().unwrap();
    assert_eq!((split.agree, split.disagree, split.unknown), (2, 1, 1));
    assert_eq!(split.agree + split.disagree + split.unknown, routine.sweeps);
    assert_eq!(split.agree_first_pass_judged, 2);
    assert!((split.agree_first_pass_approval_rate.unwrap() - 0.5).abs() < 1e-9);
    assert_eq!(split.disagree_first_pass_judged, 1);
    assert_eq!(split.disagree_first_pass_approval_rate, Some(0.0));

    let unknown = row(&report, UNKNOWN_GROUP).agreement.as_ref().unwrap();
    assert_eq!((unknown.agree, unknown.disagree, unknown.unknown), (0, 0, 1));
    // No judged sweep on either side: None, never a fabricated 0.0.
    assert_eq!(unknown.agree_first_pass_approval_rate, None);

    let total = report.agreement_totals.as_ref().unwrap();
    assert_eq!((total.agree, total.disagree, total.unknown), (3, 1, 2));
    assert_eq!(total.agree + total.disagree + total.unknown, report.records_grouped);
    assert_eq!((total.agree_first_pass_judged, total.agree_first_pass_approved), (3, 2));
    assert!(report.notes.iter().any(|n| n.contains("#8608")));
}

/// `model-complexity` carries the split too; every other grouping does not,
/// so their JSON output is byte-for-byte what it was before #8608.
#[test]
fn split_is_scoped_to_complexity_groupings() {
    let (records, index) = fixture();
    let cross = summarize(
        &records,
        &index,
        opts(GroupBy::ModelComplexity),
        &mut NoneMerged,
        vec!["/ws".into()],
    );
    let split = row(&cross, "default/routine").agreement.as_ref().unwrap();
    assert_eq!((split.agree, split.disagree, split.unknown), (2, 1, 1));

    let by_repo =
        summarize(&records, &index, opts(GroupBy::Repo), &mut NoneMerged, vec!["/ws".into()]);
    assert!(by_repo.rows.iter().all(|r| r.agreement.is_none()));
    assert!(by_repo.agreement_totals.is_none());
    let json = serde_json::to_string(&by_repo).unwrap();
    assert!(!json.contains("agreement"));
    assert!(!render_text(&by_repo).contains("Curator-vs-Jev"));
}

/// The text render adds the agreement table below the main table while the
/// main table keeps its JDG1% / JDG_N columns.
#[test]
fn render_text_shows_the_agreement_table_alongside_jdg1() {
    let (records, index) = fixture();
    let report =
        summarize(&records, &index, opts(GroupBy::Complexity), &mut NoneMerged, vec!["/ws".into()]);
    let text = render_text(&report);
    assert!(text.contains("JDG1%"));
    assert!(text.contains("Curator-vs-Jev tier agreement"));
    assert!(text.contains("AGR_JDG1%"));
    assert!(text.contains("DIS_JDG1%"));
    let total_line = text.lines().find(|l| l.starts_with("TOTAL")).unwrap();
    let cols: Vec<&str> = total_line.split_whitespace().collect();
    assert_eq!(cols, ["TOTAL", "3", "1", "2", "66.7%", "3", "0.0%", "1"]);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["agreement_totals"]["agree"], 3);
    assert_eq!(json["rows"][0]["agreement"]["disagree"], 1);
}

/// End to end over real on-disk journals: the join crosses from the
/// telemetry journal to the sibling journal by `sweep_id`.
#[test]
fn summarize_workspaces_joins_the_sibling_journal() {
    let ws = tempfile::tempdir().unwrap();
    write_envelope(ws.path(), day(10), "h1", &curated("s1", Some("routine"), Some("pass")));
    write_envelope(ws.path(), day(10), "h1", &curated("s2", Some("routine"), Some("fail")));
    write_sibling(ws.path(), &[jev("s1", Some("routine")), jev("s2", Some("complex"))]);

    let report = summarize_workspaces(
        &[ws.path().to_path_buf()],
        opts(GroupBy::Complexity),
        &mut NoneMerged,
    );
    let split = row(&report, "routine").agreement.as_ref().unwrap();
    assert_eq!((split.agree, split.disagree, split.unknown), (1, 1, 0));
    assert_eq!(split.agree_first_pass_approval_rate, Some(1.0));
    assert_eq!(split.disagree_first_pass_approval_rate, Some(0.0));
}
