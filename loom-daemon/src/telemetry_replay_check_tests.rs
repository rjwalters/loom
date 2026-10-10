//! `telemetry-replay --check` without a store (#11128). The comparison is the
//! committed SQL; `tests/signoz_replay_queries.rs` and
//! `tests/telemetry_replay_fixture_store.rs` run it on the agreement fixture.
//! Here the reader is proven on that run's recorded output
//! (`tests/fixtures/signoz_replay/check_export.jsonl`, `FileRows`-style), and
//! the SQL's vocabulary is pinned to the stage enum and the label registry.
//! Every test name starts `telemetry_replay_check` so one filter runs them.

use super::*;
use chrono::TimeZone;

const EXPORT: &str = include_str!("../tests/fixtures/signoz_replay/check_export.jsonl");

fn params() -> CheckParams {
    CheckParams {
        replay: ReplayParams {
            as_of: Utc.with_ymd_and_hms(2026, 10, 4, 13, 0, 0).unwrap(),
            window_sec: crate::telemetry_replay::DEFAULT_WINDOW_SEC,
            repo: String::new(),
        },
        span_sec: DEFAULT_SPAN_SEC,
        step_sec: crate::telemetry_replay::DEFAULT_STEP_SEC,
        threshold_sec: DEFAULT_THRESHOLD_SEC,
    }
}

/// The recorded export, keeping only the named hosts' rows.
fn only(hosts: &[&str]) -> String {
    EXPORT
        .lines()
        .filter(|l| {
            let row: Value = serde_json::from_str(l).unwrap();
            hosts.contains(&row["emitter"].as_str().unwrap())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check(export: &str) -> Check {
    assemble_export(&params(), export).unwrap()
}

fn host<'a>(c: &'a Check, emitter: &str) -> &'a HostAgreement {
    c.hosts.iter().find(|h| h.emitter == emitter).unwrap()
}

#[test]
fn telemetry_replay_check_queries_are_the_committed_sql_with_the_agreement_block() {
    let (check, report) = check_and_report_queries(REPLAY_QUERIES).unwrap();
    for q in [&check, &report] {
        assert!(q.starts_with("WITH samples AS"), "{q}");
        assert!(q.contains("FROM live"), "{q}");
        assert!(q.contains("'loom-ui-d1-export'"), "{q}");
        assert!(!q.contains("--"), "{q}");
        // No row caps: the only LIMIT is the dedupe `LIMIT 1 BY`.
        assert_eq!(q.matches("LIMIT").count(), q.matches("LIMIT 1 BY").count(), "{q}");
    }
    assert!(check.contains("FROM runs"), "{check}");
    assert!(report.contains("FROM host_samples GROUP BY emitter"), "{report}");
    // Query 7 gets the block prepended; query 6 already continues with it.
    assert_eq!(report.matches(", forge_log AS (").count(), 1);
    assert_eq!(check.matches(", forge_log AS (").count(), 1);
}

#[test]
fn telemetry_replay_check_an_unexpected_sql_shape_is_refused() {
    let unmarked = REPLAY_QUERIES.replace(AGREEMENT_BEGIN, "-- moved");
    assert!(check_and_report_queries(&unmarked)
        .unwrap_err()
        .contains("agreement-prefix begin marker"));
    // Something between query 5 and the block: query 6 no longer begins with it.
    let detached =
        REPLAY_QUERIES.replacen(AGREEMENT_BEGIN, &format!("SELECT 9\n{AGREEMENT_BEGIN}"), 1);
    assert!(check_and_report_queries(&detached)
        .unwrap_err()
        .contains("query 6 does not begin with the agreement block"));
    let extra =
        REPLAY_QUERIES.replacen(AGREEMENT_BEGIN, &format!("SELECT 9;\n{AGREEMENT_BEGIN}"), 1);
    assert!(check_and_report_queries(&extra)
        .unwrap_err()
        .contains("expected 10 statements"));
}

#[test]
fn telemetry_replay_check_params_bind_the_samples_and_the_threshold() {
    let mut p = params();
    p.replay.repo = "rjwalters/loom".to_string();
    p.span_sec = 86_400;
    assert_eq!(
        p.params(),
        [
            ("t".to_string(), "2026-10-04 13:00:00.000".to_string()),
            ("window".to_string(), "3900".to_string()),
            ("repo".to_string(), "rjwalters/loom".to_string()),
            ("span".to_string(), "86400".to_string()),
            ("step".to_string(), "300".to_string()),
            ("threshold".to_string(), "600".to_string()),
        ]
    );
    assert_eq!(p.samples(), 289);
    p.step_sec = 0;
    assert!(p.params().contains(&("step".to_string(), "1".to_string())));
}

#[test]
fn telemetry_replay_check_full_agreement_exits_0() {
    let c = check(&only(&["h-agree"]));
    assert_eq!(c.exit_code(), EXIT_AGREE);
    assert!(c.disagreements.is_empty());
    let h = host(&c, "h-agree");
    assert_eq!((h.coverage.as_str(), h.covered_samples, h.samples), ("covered", 13, 13));
    // 1 ready_wait, 2 review_wait, 9 merge_hold, 10 sweep.builder at 13
    // instants; 7 (no PR) and 8 (no webhook record) are not compared.
    assert_eq!((h.compared, h.agreeing, h.not_comparable), (52, 52, 26));
    assert!(render(&c).contains("agree: no covered host"));
}

#[test]
fn telemetry_replay_check_a_covered_host_disagreeing_past_the_threshold_exits_1() {
    let c = check(EXPORT);
    assert_eq!(c.exit_code(), EXIT_DISAGREE);
    let failures: Vec<_> = c.failures().collect();
    assert_eq!(failures.len(), 2, "{failures:#?}");
    let d = failures[0];
    assert_eq!(
        (d.emitter.as_str(), d.repo.as_str(), d.issue, d.pr),
        ("h-stuck", "rjwalters/other", 5, 500)
    );
    assert_eq!((d.host_stage.as_str(), d.forge_stage.as_str()), ("review_wait", "merge_wait"));
    assert_eq!(d.disagree_sec, 2700);
    let h = host(&c, "h-stuck");
    assert_eq!((h.longest_disagreement_sec, h.disagreements_over_threshold), (2700, 1));
    let text = render(&c);
    assert!(
        text.contains(
            "FAIL h-stuck rjwalters/other#5 pr=#500 host=review_wait forge=merge_wait for 2700s"
        ),
        "{text}"
    );
    assert!(text.contains("DISAGREE: 2 disagreement(s)"), "{text}");
    assert!(text.contains("hosts: 7 (4 covered, 0 partial, 3 unknown)"), "{text}");
}

#[test]
fn telemetry_replay_check_one_disagreement_across_forge_stage_changes_is_one_run() {
    // h-drift holds review_wait for PR 400 while the forge moves it to
    // merge_wait (2 instants) and then doctor (2 instants). Each stage alone
    // is 600 s, not over the threshold; the one run about the item is 1200 s.
    let c = check(&only(&["h-drift"]));
    assert_eq!(c.exit_code(), EXIT_DISAGREE);
    assert_eq!(c.disagreements.len(), 1, "{:#?}", c.disagreements);
    let d = &c.disagreements[0];
    assert_eq!((d.repo.as_str(), d.issue, d.pr), ("rjwalters/drift", 4, 400));
    assert_eq!((d.host_stage.as_str(), d.forge_stage.as_str()), ("review_wait", "doctor"));
    assert_eq!(d.forge_stages, ["doctor", "merge_wait"]);
    assert_eq!((d.disagree_sec, d.over_threshold), (1200, true));
    assert_eq!(
        (d.first_at.as_str(), d.last_at.as_str()),
        ("2026-10-04 12:25:00.000", "2026-10-04 12:40:00.000")
    );
    let h = host(&c, "h-drift");
    assert_eq!((h.longest_disagreement_sec, h.disagreements_over_threshold), (1200, 1));
    assert_eq!((h.agreeing, h.disagreeing), (9, 4));
    let text = render(&c);
    assert!(
        text.contains(
            "FAIL h-drift rjwalters/drift#4 pr=#400 host=review_wait forge=doctor \
             (forge seen doctor,merge_wait) for 1200s"
        ),
        "{text}"
    );
}

#[test]
fn telemetry_replay_check_the_same_disagreement_on_an_uncovered_host_is_unknown_and_exits_0() {
    // h-gap holds the same stale review_wait for PR 500 as h-stuck, but its
    // chain is broken at every instant: it is never compared.
    let c = check(&only(&["h-gap", "h-agree"]));
    assert_eq!(c.exit_code(), EXIT_AGREE);
    assert!(c.disagreements.is_empty());
    let h = host(&c, "h-gap");
    assert_eq!(h.coverage, "unknown");
    assert_eq!(h.uncovered_states, ["broken_chain"]);
    assert_eq!((h.covered_samples, h.compared), (0, 0));
    // In the full run too: the only failure is the covered host's.
    let full = check(EXPORT);
    assert!(full.disagreements.iter().all(|d| d.emitter != "h-gap"));
    assert_eq!(host(&full, "h-silent").uncovered_states, ["no_anchor"]);
}

#[test]
fn telemetry_replay_check_an_incomplete_anchor_is_reported_and_skipped() {
    let c = check(&only(&["h-chunk"]));
    let h = host(&c, "h-chunk");
    assert_eq!((h.incomplete_anchors, h.incomplete_deltas), (2, 0));
    assert_eq!(h.coverage, "unknown");
    assert_eq!(h.uncovered_states, ["incomplete_anchor"]);
    // Its row (6 ready_wait, closed on the forge) is never compared.
    assert_eq!(h.compared, 0);
    assert_eq!(c.exit_code(), EXIT_AGREE);
    assert!(render(&c).contains("incomplete_anchors=2"));
}

#[test]
fn telemetry_replay_check_a_disagreement_shorter_than_the_threshold_exits_0() {
    let c = check(&only(&["h-lag"]));
    assert_eq!(c.exit_code(), EXIT_AGREE);
    assert_eq!(c.disagreements.len(), 1);
    let d = &c.disagreements[0];
    assert_eq!((d.host_stage.as_str(), d.forge_stage.as_str()), ("ready_wait", "building"));
    assert_eq!((d.disagree_sec, d.over_threshold), (300, false));
    assert_eq!(host(&c, "h-lag").longest_disagreement_sec, 300);
    assert!(render(&c).contains("ok   h-lag rjwalters/loom#3"));
}

#[test]
fn telemetry_replay_check_a_row_missing_a_column_is_refused() {
    let err = assemble(&params(), "{\"emitter\":\"h\"}", "").unwrap_err();
    assert!(err.starts_with("disagreements:") && err.contains("repo"), "{err}");
    let err = assemble(&params(), "", "{\"emitter\":\"h\",\"samples\":1}").unwrap_err();
    assert!(err.starts_with("report:") && err.contains("covered_samples"), "{err}");
}

/// The agreement block's vocabulary is pinned to its authorities: every
/// `fleet.state` wire stage is mapped, the hold labels are the registry's
/// `merge_hold` set, and the webhook body keys are the loom-ui
/// `label.transition` contract `telemetry-replay.md` documents.
#[test]
fn telemetry_replay_check_vocabulary_matches_the_stage_enum_registry_and_webhook_contract() {
    use crate::telemetry::kinds::fleet_state::FleetStage;
    let block = crate::telemetry_replay::marked(
        REPLAY_QUERIES,
        AGREEMENT_BEGIN,
        AGREEMENT_END,
        "agreement-prefix",
    )
    .unwrap();
    for stage in [
        FleetStage::ReadyWait,
        FleetStage::SweepCurator,
        FleetStage::SweepBuilder,
        FleetStage::ReviewWait,
        FleetStage::Doctor,
        FleetStage::MergeWait,
        FleetStage::MergeHold,
    ] {
        assert!(
            block.contains(&format!("'{}'", stage.as_str())),
            "stage {} is not mapped",
            stage.as_str()
        );
    }
    // The pre-ready stages (#11368) are `eta.stage_outcome` only: they carry
    // wire names but are not `fleet.state` stages the SQL has to map.
    for (stage, wire) in [
        (FleetStage::TriageWait, "triage_wait"),
        (FleetStage::ApprovalWait, "approval_wait"),
    ] {
        assert_eq!(stage.as_str(), wire);
        assert_eq!(serde_json::from_value::<FleetStage>(wire.into()).unwrap(), stage);
    }
    let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut holds: Vec<String> = crate::label_registry::embedded_set("merge_hold")
        .iter()
        .map(|l| format!("'{l}'"))
        .collect();
    holds.sort();
    assert!(!holds.is_empty());
    let in_sql = format!("hasAny(i.labels, [{}])", holds.join(", "));
    assert!(
        squash(&block).contains(&in_sql),
        "the SQL's hold labels are not the registry's merge_hold set: {in_sql}"
    );
    let keys: std::collections::BTreeSet<&str> = block
        .match_indices("JSONExtractString(body, '")
        .chain(block.match_indices("JSONExtractUInt(body, '"))
        .map(|(at, m)| {
            let rest = &block[at + m.len()..];
            &rest[..rest.find('\'').unwrap()]
        })
        .collect();
    assert_eq!(
        keys.into_iter().collect::<Vec<_>>(),
        ["action", "at", "kind", "label", "number", "repo", "target"]
    );
}
