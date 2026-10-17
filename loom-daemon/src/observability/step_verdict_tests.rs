//! `step_verdict` (#12027): one test per projection row, the refusal store's
//! identity rules, the ledger's record-only view, and the no-effect guards.

use chrono::{Duration, TimeZone};

use super::*;
use crate::observability::pick_journal::JournalRow;

const REPO: &str = "acme/app";

fn t(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 3, minute, 0).unwrap()
}

fn ctx(now: DateTime<Utc>) -> Context<'static> {
    Context {
        role: "champion",
        repo: REPO,
        host: "host-a",
        now,
    }
}

fn marker(minute: u32, number: u32, is_pr: bool, text: &str, head: Option<&str>) -> JournalEntry {
    JournalEntry::Marker {
        at: t(minute),
        number,
        is_pr,
        marker: text.to_string(),
        head_sha: head.map(str::to_string),
    }
}

fn act(minute: u32, number: u32, action: &str) -> JournalEntry {
    JournalEntry::Act {
        at: t(minute),
        number,
        action: action.to_string(),
    }
}

fn applied(minute: u32, number: u32, action: &str, by: &str) -> JournalEntry {
    JournalEntry::Applied {
        at: t(minute),
        number,
        is_pr: false,
        action: action.to_string(),
        by: by.to_string(),
    }
}

fn journal(entries: Vec<JournalEntry>) -> Journal {
    Journal {
        entries,
        front_expected: true,
        read: true,
    }
}

fn step(role: StepRole, item: u32, outcome: Option<&str>) -> Source {
    let mode = if role.acts_on_pr() { "pr" } else { "issue" };
    let id = format!("step-{}-{item}-1", role.as_str());
    let mut text = format!("==== loom-daemon dispatch: t sweep_id={id} item={item} ====\n");
    if let Some(outcome) = outcome {
        text.push_str(&format!("CHAMPION-STEP mode={mode} item={item} outcome={outcome}\n"));
    }
    Source::Step(StepSource {
        sweep_id: id,
        step: role,
        item,
        log_text: Some(text),
    })
}

fn one(
    source: &Source,
    journal: Option<&Journal>,
    store: &mut RefusalStore,
    now: DateTime<Utc>,
) -> PassVerdictRecord {
    let mut records = records_for(&ctx(now), source, journal, store);
    assert_eq!(records.len(), 1, "{records:?}");
    records.remove(0)
}

fn class_reason(r: &PassVerdictRecord) -> (&str, &str) {
    (r.step.class.as_deref().unwrap(), r.reason.as_deref().unwrap())
}

const CI_MARKER: &str = "<!-- champion:reject:ci-status:Lint,Rust Unit Tests -->";

/// (a) The #11277 shape: `outcome=rejected` with a `ci-status` marker, PR
/// still `loom:pr`. The projection says why; the ledger's own view (an
/// unchanged item with a verdict line) is `failed unclassified`, shown, not
/// remapped.
#[test]
fn a_red_ci_rejection_is_refused_ci_failing_and_the_ledger_disagrees_visibly() {
    let mut store = RefusalStore::default();
    let j = journal(vec![marker(1, 11277, true, CI_MARKER, Some("abc1234"))]);
    let r = one(
        &step(StepRole::ChampionMerge, 11277, Some("rejected")),
        Some(&j),
        &mut store,
        t(5),
    );
    assert_eq!(class_reason(&r), ("refused", "ci_failing"));
    assert_eq!(r.verdict, "refused", "verdict mirrors class for passVerdictQuery");
    assert_eq!(r.mechanism, "step:champion");
    assert_eq!(r.artifact, "pr");
    assert_eq!(r.step.step.as_deref(), Some("champion_pr"));
    assert_eq!(r.step.reason_set.as_deref(), Some(champion::MERGE_REASON_SET));
    assert_eq!(r.step.ledger_class.as_deref(), Some("failed"));
    assert_eq!(r.step.ledger_reason.as_deref(), Some("unclassified"));
    assert_eq!(r.step.criterion_key.as_deref(), Some("ci-status"));
    assert_eq!(r.step.reason_key.as_deref(), Some("Lint,Rust Unit Tests"));
    assert_eq!(r.step.head_sha.as_deref(), Some("abc1234"));
    assert_eq!(r.step.first_refused_at, Some(t(5)));
    assert_eq!(r.step.evaluated_at, Some(t(5)));
    assert!(r.step.loom.is_some());
    assert!(r.applied);
}

/// (b) The next tick's guard posts nothing: the step still says
/// `rejected`, and the standing identity names it, unchanged.
#[test]
fn a_duplicate_suppressed_rejection_keeps_the_identity_and_first_refused_at() {
    let mut store = RefusalStore::default();
    let j = journal(vec![marker(1, 9, true, CI_MARKER, Some("abc1234"))]);
    let first =
        one(&step(StepRole::ChampionMerge, 9, Some("rejected")), Some(&j), &mut store, t(5));
    let again = one(
        &step(StepRole::ChampionMerge, 9, Some("rejected")),
        Some(&journal(vec![])),
        &mut store,
        t(20),
    );
    assert_eq!(class_reason(&again), ("refused", "ci_failing"));
    assert_eq!(again.step.first_refused_at, first.step.first_refused_at);
    assert_eq!(again.step.criterion_key, first.step.criterion_key);
    assert_eq!(again.step.reason_key, first.step.reason_key);
    assert_eq!(again.step.head_sha, first.step.head_sha);
    assert_eq!(again.step.fact_id, first.step.fact_id, "same inputs, same hour");
    assert!(!again.applied, "nothing was written this run");
}

/// (c) A new failing check set is a new identity.
#[test]
fn a_new_failing_check_set_restarts_first_refused_at() {
    let mut store = RefusalStore::default();
    let j1 = journal(vec![marker(1, 9, true, CI_MARKER, Some("abc1234"))]);
    one(&step(StepRole::ChampionMerge, 9, Some("rejected")), Some(&j1), &mut store, t(5));
    let other = "<!-- champion:reject:ci-status:Lint -->";
    let j2 = journal(vec![marker(30, 9, true, other, Some("abc1234"))]);
    let r = one(
        &step(StepRole::ChampionMerge, 9, Some("rejected")),
        Some(&j2),
        &mut store,
        t(31),
    );
    assert_eq!(r.step.first_refused_at, Some(t(31)));
    assert_eq!(r.step.reason_key.as_deref(), Some("Lint"));
}

/// (d) A merge is `verdict merged` and clears the standing refusal.
#[test]
fn a_merge_act_is_verdict_merged_and_clears_the_store() {
    let mut store = RefusalStore::default();
    let j1 = journal(vec![marker(1, 9, true, CI_MARKER, None)]);
    one(&step(StepRole::ChampionMerge, 9, Some("rejected")), Some(&j1), &mut store, t(5));
    let j2 = journal(vec![act(40, 9, "merged")]);
    let r = one(&step(StepRole::ChampionMerge, 9, Some("merged")), Some(&j2), &mut store, t(41));
    assert_eq!(class_reason(&r), ("verdict", "merged"));
    assert_eq!(r.step.ledger_class.as_deref(), Some("verdict"), "moved by the merge");
    assert!(store.entries.get(REPO).is_none_or(BTreeMap::is_empty));
    // The step-path fallback when the front is opted out: the line alone.
    let r = one(
        &step(StepRole::ChampionMerge, 10, Some("merged")),
        Some(&journal(vec![])),
        &mut store,
        t(42),
    );
    assert_eq!(class_reason(&r), ("verdict", "merged"));
}

/// (e) No readable journal: the ledger's probe failed, the projection is
/// still emitted from the step's line.
#[test]
fn an_unread_journal_is_probe_failed_and_the_projection_still_emits() {
    let mut store = RefusalStore::default();
    let r = one(&step(StepRole::ChampionMerge, 9, Some("requeued")), None, &mut store, t(5));
    assert_eq!(class_reason(&r), ("noop", "ci_pending"));
    assert_eq!(r.step.ledger_class.as_deref(), Some("failed"));
    assert_eq!(r.step.ledger_reason.as_deref(), Some("probe_failed"));
    let unread = Journal {
        read: false,
        ..journal(vec![])
    };
    let r = one(&step(StepRole::ChampionMerge, 9, Some("held")), Some(&unread), &mut store, t(5));
    assert_eq!(r.step.ledger_reason.as_deref(), Some("probe_failed"));
}

/// (f) `fact_id` is derived: two calls, two hosts, same inputs, one id.
#[test]
fn fact_id_is_deterministic_across_calls_and_hosts() {
    let j = journal(vec![marker(1, 9, true, CI_MARKER, Some("abc1234"))]);
    let source = step(StepRole::ChampionMerge, 9, Some("rejected"));
    let a = one(&source, Some(&j), &mut RefusalStore::default(), t(5));
    let b = records_for(
        &Context {
            host: "host-b",
            ..ctx(t(5))
        },
        &source,
        Some(&j),
        &mut RefusalStore::default(),
    )
    .remove(0);
    assert!(a.step.fact_id.is_some());
    assert_eq!(a.step.fact_id, b.step.fact_id);
    assert_eq!(a.pass_id, b.pass_id, "a step's pass id is its sweep id's");
}

#[test]
fn every_merge_marker_row_projects_its_reason() {
    for (text, reason) in [
        ("<!-- champion:reject:merge-conflict:merge-conflict -->", "conflict"),
        ("<!-- champion:reject:label-check:loom:pr -->", "label"),
        ("<!-- champion:reject:size-check:size-check -->", "size"),
        ("<!-- champion:reject:recency-check:x -->", "unknown"),
        ("<!-- champion:critical-file-hold -->", "critical_file"),
        ("<!-- champion:stale-pr-notice:2026-10-01 -->", "stale"),
    ] {
        let j = journal(vec![marker(1, 9, true, text, None)]);
        let r = one(
            &step(StepRole::ChampionMerge, 9, Some("rejected")),
            Some(&j),
            &mut RefusalStore::default(),
            t(5),
        );
        assert_eq!(class_reason(&r), ("refused", reason), "{text}");
        assert!(champion::MERGE_REASONS.contains(&reason));
    }
    assert_eq!(
        champion::merge_marker("<!-- champion:reject:label-check:loom:pr,loom:blocked -->")
            .map(|(_, key, _)| key),
        Some("loom:pr,loom:blocked".to_string()),
        "a reason key keeps its own colons"
    );
    assert!(champion::merge_marker("<!-- champion:critical-file-hold-cleared -->").is_none());
    assert!(champion::merge_marker("<!-- champion:merge-risk-hold -->").is_none());
}

#[test]
fn every_merge_outcome_row_projects_its_reason() {
    for (outcome, class, reason) in [
        (Some("requeued"), "noop", "ci_pending"),
        (Some("held"), "noop", "held"),
        (Some("head-moved"), "refused", "head_moved"),
        (Some("not-eligible"), "refused", "not_eligible"),
        (Some("failed"), "failed", "failed"),
        (Some("rejected"), "refused", "unknown"),
        (None, "failed", "unclassified"),
    ] {
        let r = one(
            &step(StepRole::ChampionMerge, 9, outcome),
            Some(&journal(vec![])),
            &mut RefusalStore::default(),
            t(5),
        );
        assert_eq!(class_reason(&r), (class, reason), "{outcome:?}");
    }
}

#[test]
fn a_moved_head_clears_the_standing_refusal() {
    let mut store = RefusalStore::default();
    let j = journal(vec![marker(1, 9, true, CI_MARKER, Some("abc1234"))]);
    one(&step(StepRole::ChampionMerge, 9, Some("rejected")), Some(&j), &mut store, t(5));
    one(
        &step(StepRole::ChampionMerge, 9, Some("head-moved")),
        Some(&journal(vec![])),
        &mut store,
        t(9),
    );
    let r = one(
        &step(StepRole::ChampionMerge, 9, Some("rejected")),
        Some(&journal(vec![])),
        &mut store,
        t(12),
    );
    assert_eq!(class_reason(&r), ("refused", "unknown"));
    assert_eq!(r.step.first_refused_at, None);
}

#[test]
fn every_promotion_row_projects_its_reason() {
    let issue = |entries: Vec<JournalEntry>, outcome: Option<&str>| {
        one(
            &step(StepRole::ChampionPromote, 42, outcome),
            Some(&journal(entries)),
            &mut RefusalStore::default(),
            t(5),
        )
    };
    let r = issue(vec![act(1, 42, "promoted")], Some("promoted"));
    assert_eq!(class_reason(&r), ("verdict", "promoted"));
    assert_eq!(r.artifact, "issue");
    assert_eq!(r.step.step.as_deref(), Some("champion_issue"));
    assert_eq!(r.step.reason_set.as_deref(), Some(champion::PROMOTION_REASON_SET));

    let revise = vec![
        marker(1, 42, false, "<!-- champion:proposal-verdict:body-0123abcd -->", None),
        marker(1, 42, false, "<!-- champion:revise:body-0123abcd -->", None),
    ];
    let r = issue(revise, Some("revise"));
    assert_eq!(class_reason(&r), ("refused", "not_obviously_beneficial"));
    assert_eq!(r.step.body_hash.as_deref(), Some("0123abcd"));

    let escalate = vec![
        applied(1, 42, "escalated", "operator_decision"),
        marker(2, 42, false, "<!-- champion:proposal-escalated -->", None),
    ];
    assert_eq!(class_reason(&issue(escalate, Some("operator"))), ("refused", "needs_operator"));

    let close = vec![marker(
        1,
        42,
        false,
        "<!-- champion:premise-false-closed:deadbeef -->",
        None,
    )];
    assert_eq!(class_reason(&issue(close, Some("closed"))), ("refused", "premise_false"));

    let park = vec![applied(1, 42, "blocked", "park_record")];
    assert_eq!(class_reason(&issue(park, Some("blocked"))), ("refused", "blocked_dependency"));

    let evaluating = vec![JournalEntry::Listing {
        at: t(0),
        rows: vec![JournalRow {
            number: 42,
            stage: "loom:curated".into(),
            labels: vec!["loom:curated".into(), "loom:evaluating".into()],
            sort_key: None,
        }],
    }];
    let r = issue(evaluating, Some("not-eligible"));
    assert_eq!(class_reason(&r), ("noop", "evaluating_claimed"));
    assert_eq!(r.step.proposal_label.as_deref(), Some("loom:curated"));
    assert_eq!(r.step.pre_labels.len(), 2);

    assert_eq!(class_reason(&issue(vec![], Some("not-eligible"))), ("noop", "not_eligible"));
    assert_eq!(class_reason(&issue(vec![], Some("failed"))), ("failed", "failed"));
    assert_eq!(class_reason(&issue(vec![], None)), ("failed", "unclassified"));
    for reason in ["promoted", "needs_operator", "evaluating_claimed"] {
        assert!(champion::PROMOTION_REASONS.contains(&reason));
    }
}

/// A role-runner tick: one record per item with a projected write, none
/// for anything else (a claim, an unrelated marker).
#[test]
fn a_role_tick_emits_one_record_per_item_with_a_projected_write() {
    let mut store = RefusalStore::default();
    let j = journal(vec![
        marker(1, 5, true, CI_MARKER, None),
        act(2, 6, "merged"),
        act(3, 7, "claimed"),
        marker(4, 8, false, "<!-- champion:dep-defer:abc -->", None),
        act(5, 42, "promoted"),
    ]);
    let records = records_for(&ctx(t(9)), &Source::Tick { started_at: t(0) }, Some(&j), &mut store);
    let got: Vec<(u64, &str, &str)> = records
        .iter()
        .map(|r| (r.number, r.verdict.as_str(), r.reason.as_deref().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![
            (5, "refused", "ci_failing"),
            (6, "verdict", "merged"),
            (42, "verdict", "promoted")
        ]
    );
    assert_eq!(records[0].at, t(1), "stamped at the write, not the drain");
    assert_eq!(records[0].step.first_refused_at, Some(t(1)));
    assert!(records.iter().all(|r| r.step.ledger_class.is_none()), "no step, no ledger line");
    assert_eq!(records[0].pass_id, records[1].pass_id);
}

#[test]
fn a_role_with_no_projection_emits_nothing() {
    let j = journal(vec![marker(1, 5, true, CI_MARKER, None)]);
    let ctx = Context {
        role: "judge",
        ..ctx(t(5))
    };
    let source = Source::Tick { started_at: t(0) };
    assert!(records_for(&ctx, &source, Some(&j), &mut RefusalStore::default()).is_empty());
}

/// A `--pr` step on an unlabelled PR is a triage (#12499): its three
/// outcomes project onto their own reason set, never the merge gate's
/// `failed unclassified`.
#[test]
fn a_pr_triage_projects_onto_the_triage_reason_set() {
    for (word, class, reason) in [
        ("triaged", "verdict", "review_requested"),
        ("triage-blocked", "refused", "blocked"),
        ("triage-operator", "refused", "operator"),
    ] {
        let r = one(
            &step(StepRole::ChampionMerge, 12, Some(word)),
            Some(&journal(vec![])),
            &mut RefusalStore::default(),
            t(5),
        );
        assert_eq!(class_reason(&r), (class, reason), "{word}");
        assert_eq!(r.step.reason_set.as_deref(), Some(champion::TRIAGE_REASON_SET));
        assert_eq!(r.step.step.as_deref(), Some("champion_pr"));
        assert!(champion::TRIAGE_REASONS.contains(&reason));
    }
}

#[test]
fn the_staged_step_reads_only_its_own_log_section() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("step.log");
    std::fs::write(
        &log,
        "==== d sweep_id=step-champion_merge-9-1 item=9 ====\n\
         CHAMPION-STEP mode=pr item=9 outcome=merged\n\
         ==== d sweep_id=step-champion_merge-9-2 item=9 ====\n\
         CHAMPION-STEP mode=pr item=9 outcome=requeued\n",
    )
    .unwrap();
    let staged = StagedStep {
        sweep_id: "step-champion_merge-9-2".into(),
        step: StepRole::ChampionMerge,
        item: 9,
        log,
    };
    stage_step(staged.clone());
    assert_eq!(take_staged_step(), Some(staged.clone()));
    assert_eq!(take_staged_step(), None, "taken once");
    let source = staged.read();
    let text = source.log_text.unwrap();
    assert!(text.contains("outcome=requeued") && !text.contains("outcome=merged"));
}

#[test]
fn emit_drain_emits_through_the_sink_only_for_a_projected_role() {
    let dir = tempfile::tempdir().unwrap();
    let j = journal(vec![marker(1, 5, true, CI_MARKER, None)]);
    let repo = "acme/emit-drain";
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        emit_drain(dir.path(), "champion", repo, t(0), None, Some(&j));
        emit_drain(dir.path(), "curator", repo, t(0), None, Some(&j));
    });
    let verdicts: Vec<&PassVerdictRecord> = captured
        .records
        .iter()
        .filter_map(|r| match r {
            crate::telemetry::TelemetryRecord::PassVerdict(v) => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].repo, repo);
    assert!(refusals::store_path(dir.path()).exists(), "the store is mirrored to disk");
}

/// Behaviour unchanged: draining a Champion step's verdict leaves the
/// registry's step cooldowns (local ledger plus peer broadcasts) as they
/// were. The exporter holds no registry handle; this pins that it stays so.
#[test]
fn the_exporter_leaves_the_registrys_step_cooldowns_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), "", 0);
    let now = Utc::now();
    let before = reg.step_cooldowns(now);
    let j = journal(vec![marker(1, 11277, true, CI_MARKER, None)]);
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        emit_drain(
            dir.path(),
            "champion",
            "acme/unchanged",
            t(0),
            Some(StagedStep {
                sweep_id: "step-champion_merge-11277-1".into(),
                step: StepRole::ChampionMerge,
                item: 11277,
                log: dir.path().join("missing.log"),
            }),
            Some(&j),
        );
    });
    assert_eq!(captured.records.len(), 1);
    assert_eq!(reg.step_cooldowns(now), before);
}

/// Record-only: the exporter never applies the ledger, writes the forge or
/// runs a command. A source check, so a later edit cannot quietly wire one.
#[test]
fn the_exporter_calls_no_ledger_effect_forge_write_or_command() {
    for (name, source) in [
        ("step_verdict.rs", include_str!("step_verdict.rs")),
        ("step_verdict/champion.rs", include_str!("step_verdict/champion.rs")),
        ("step_verdict/refusals.rs", include_str!("step_verdict/refusals.rs")),
    ] {
        let code: String = source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "OutcomeLedger",
            "record_step_outcome",
            "effect_of",
            ".record(",
            "Command::new",
            "GhInvocation",
            "gh_invocation",
            "forge_comment",
            "add_labels",
            "forge_mirror",
            "peer_claims",
        ] {
            assert!(!code.contains(forbidden), "{name} uses {forbidden}");
        }
    }
}

#[test]
fn the_store_ttl_drops_an_unseen_refusal() {
    let mut store = RefusalStore::default();
    let seen = refusals::Observed {
        head_sha: None,
        criterion_key: "ci-status",
        reason_key: "a",
        reason: "ci_failing",
        at: t(0),
    };
    store.observe(REPO, 1, &seen);
    store.prune(t(0) + Duration::hours(1));
    assert!(store.gauge(REPO, &champion::GAUGE_REASONS).unwrap()["ci_failing"].0 == 1);
    store.prune(t(0) + Duration::hours(7));
    assert_eq!(store.gauge(REPO, &champion::GAUGE_REASONS).unwrap()["ci_failing"], (0, None));
    assert!(store
        .gauge("other/repo", &champion::GAUGE_REASONS)
        .is_none());
}

/// A marker cut at the journal's bound still projects; the reason key is
/// the cut one (the same cut every time, so the identity is stable).
#[test]
fn a_marker_cut_at_the_journal_bound_still_projects() {
    let long = format!("<!-- champion:reject:ci-status:{} -->", "Check ".repeat(60));
    let cut: String = long
        .chars()
        .take(crate::observability::pick_journal::MAX_MARKER_LEN)
        .collect();
    let (criterion, key, reason) = champion::merge_marker(&cut).unwrap();
    assert_eq!((criterion.as_str(), reason), ("ci-status", "ci_failing"));
    assert!(key.starts_with("Check Check"));
}
