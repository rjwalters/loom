//! Coverage for the step classifier (#11895).
//!
//! The table cases come from the shared fixture
//! `loom-daemon/tests/fixtures/step-item-classifier.json`, which a TypeScript
//! twin will pin too. What the fixture cannot express (the body adapters, the
//! trusted-comment markers, that every table row has a case) is tested here.

use super::*;
use crate::champion_hold::{CRITICAL_FILE_HOLD_MARKER, DOCTOR_CYCLE_CAP_MARKER};
use crate::sweep_registry::park_hold::{
    render_hold_record, PRLESS_HOLD_REASON, QUARANTINE_HOLD_REASON,
};

/// The fixture, verbatim (`include_str!`: a moved fixture is a compile error).
const FIXTURE: &str = include_str!("../../tests/fixtures/step-item-classifier.json");

#[derive(serde::Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    inputs: Inputs,
    expected: Vec<Expected>,
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
struct Expected {
    issue: Option<u32>,
    pr: Option<u32>,
    state: ItemState,
    verdict: Verdict,
    #[serde(default)]
    lane: Lane,
    #[serde(default)]
    send_back: bool,
}

impl Expected {
    fn of(item: &Item) -> Self {
        Expected {
            issue: item.issue,
            pr: item.pr,
            state: item.state,
            verdict: item.verdict,
            lane: item.lane,
            send_back: item.send_back,
        }
    }
}

fn fixture() -> Fixture {
    serde_json::from_str(FIXTURE).expect("the shared step-item fixture parses")
}

#[test]
fn every_fixture_case_classifies_to_its_pinned_items() {
    for case in fixture().cases {
        let got: Vec<Expected> = classify(&case.inputs).iter().map(Expected::of).collect();
        assert_eq!(got, case.expected, "case: {}", case.name);
    }
}

/// Every step, every pass reason and every park kind appears in the fixture,
/// so a new table row cannot land without a case.
#[test]
fn the_fixture_covers_every_step_pass_and_park_kind() {
    let cases = fixture().cases;
    let verdicts: Vec<Verdict> = cases
        .iter()
        .flat_map(|c| c.expected.iter().map(|e| e.verdict))
        .collect();
    let steps = [
        Step::Curator,
        Step::CuratorKeepOrClose,
        Step::ChampionPromote,
        Step::ChampionReevaluate,
        Step::Builder,
        Step::Judge,
        Step::Doctor,
        Step::ChampionMerge,
        Step::ChampionHoldReevaluate,
        Step::ChampionTriage,
    ];
    for s in steps {
        assert!(verdicts.contains(&Verdict::Step(s)), "no fixture case for {s:?}");
    }
    let passes = [
        PassReason::Frozen,
        PassReason::Operator,
        PassReason::HardExclusion,
        PassReason::Claimed,
        PassReason::Queued,
        PassReason::SharedPr,
        PassReason::Sequenced,
        PassReason::Blocked,
        PassReason::CapabilityMissing,
        PassReason::Epic,
        PassReason::Proposal,
        // `NoStage` is only the defensive answer for an item with neither an
        // issue nor a PR (#12499: an unlabelled PR is a Champion triage now).
        PassReason::Draft,
    ];
    for p in passes {
        assert!(verdicts.contains(&Verdict::Pass(p)), "no fixture case for {p:?}");
    }
    let parks: Vec<ParkKind> = cases
        .iter()
        .flat_map(|c| c.inputs.issues.iter().map(|i| i.park))
        .collect();
    for k in [
        ParkKind::Dependency,
        ParkKind::Quarantine,
        ParkKind::PrlessHold,
        ParkKind::ChampionHold,
        ParkKind::Capability,
        ParkKind::Unstated,
    ] {
        assert!(parks.contains(&k), "no fixture case for park kind {k:?}");
    }
}

#[test]
fn steps_name_their_role_and_target() {
    assert_eq!(Step::CuratorKeepOrClose.role(), "curator");
    assert_eq!(Step::ChampionHoldReevaluate.role(), "champion");
    assert_eq!(Step::ChampionTriage.role(), "champion");
    assert!(Step::ChampionTriage.acts_on_pr(), "a triage works the PR");
    let inputs = Inputs {
        issues: vec![IssueInput {
            number: 7,
            labels: vec!["loom:building".into()],
            ..IssueInput::default()
        }],
        pulls: vec![PullInput {
            number: 70,
            labels: vec!["loom:pr".into()],
            body: Some("Closes #7".into()),
            same_repo: true,
            ..PullInput::default()
        }],
        ..Inputs::default()
    };
    let items = classify(&inputs);
    assert_eq!(items[0].step(), Some(Step::ChampionMerge));
    assert_eq!(items[0].target(), Some(70));
    let ready = classify(&Inputs {
        issues: vec![IssueInput {
            number: 8,
            labels: vec!["loom:issue".into()],
            ..IssueInput::default()
        }],
        ..Inputs::default()
    });
    assert_eq!(ready[0].target(), Some(8));
}

fn labels(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn park_kind_reads_each_record_kind_from_the_body() {
    let blocked = labels(&["loom:curated", "loom:blocked"]);
    let at = "2026-10-14T00:00:00Z";
    let dep = "<!-- loom:park Blocked by: #8322 by=curator at=2026-10-14T00:00:00Z -->";
    // A fast-fail park writes the quarantine's own record (#11854).
    let quarantine = render_hold_record(QUARANTINE_HOLD_REASON, at);
    let prless = render_hold_record(PRLESS_HOLD_REASON, at);
    let champion = r#"<!-- loom:park Blocked by: (unstated) by=champion reason="champion merge-risk hold" -->"#;
    let unstated = r#"<!-- loom:park Blocked by: (unstated) by=guide reason="waiting" -->"#;
    assert_eq!(park_kind(&blocked, Some(dep)), ParkKind::Dependency);
    assert_eq!(park_kind(&blocked, Some(&quarantine)), ParkKind::Quarantine);
    assert_eq!(park_kind(&blocked, Some(&prless)), ParkKind::PrlessHold);
    assert_eq!(park_kind(&blocked, Some(champion)), ParkKind::ChampionHold);
    assert_eq!(park_kind(&blocked, Some(unstated)), ParkKind::Unstated);
    assert_eq!(park_kind(&blocked, Some("no record")), ParkKind::None);
    assert_eq!(park_kind(&blocked, None), ParkKind::None);
    // A hold beside a dependency record keeps the hold's kind: fail closed.
    let both = format!("{dep}\n{quarantine}");
    assert_eq!(park_kind(&blocked, Some(&both)), ParkKind::Quarantine);
}

#[test]
fn park_kind_and_capabilities_read_a_capability_only_park() {
    let blocked = labels(&["loom:issue", "loom:blocked"]);
    let body = "<!-- loom:capability=host-sudo -->\nneeds root";
    assert_eq!(park_kind(&blocked, Some(body)), ParkKind::Capability);
    assert_eq!(declared_capabilities(body), vec!["host-sudo".to_string()]);
    // An unknown value is no declaration: nothing can hold it.
    assert!(declared_capabilities("<!-- loom:capability=host-sud -->").is_empty());
    // A named blocker beside the marker is a dependency park, not the lane.
    let named = "<!-- loom:capability=host-sudo -->\n\
                 <!-- loom:park Blocked by: #12 by=builder at=2026-10-14T00:00:00Z -->";
    assert_eq!(park_kind(&blocked, Some(named)), ParkKind::Dependency);
    // A hold record beside capability markers keeps the hold's kind.
    let held =
        format!("{body}\n{}", render_hold_record(QUARANTINE_HOLD_REASON, "2026-10-14T00:00:00Z"));
    assert_eq!(park_kind(&blocked, Some(&held)), ParkKind::Quarantine);
}

fn trusted(bodies: &[&str]) -> TrustedComments {
    TrustedComments(bodies.iter().map(|b| (*b).to_string()).collect())
}

#[test]
fn pull_markers_come_from_trusted_comments() {
    assert_eq!(PullMarkers::from_trusted(&trusted(&[])), PullMarkers::default());
    let hold = format!("{CRITICAL_FILE_HOLD_MARKER}\nheld");
    let marked = PullMarkers::from_trusted(&trusted(&[&hold]));
    assert!(marked.champion_hold && !marked.doctor_cycle_capped);
    let cap = format!("{DOCTOR_CYCLE_CAP_MARKER}\ncapped");
    let capped = PullMarkers::from_trusted(&trusted(&[&hold, &cap]));
    assert!(capped.champion_hold && capped.doctor_cycle_capped);
}

#[test]
fn trusted_comments_drop_untrusted_authors() {
    let policy = crate::comment_trust::TrustPolicy::new(
        crate::forge_identity::FleetLogins::of(&crate::forge_identity::Roster::default()),
        None,
        Vec::new(),
    );
    let comments = vec![
        serde_json::json!({"body": "insider", "user": {"login": "a", "type": "User"}, "author_association": "MEMBER"}),
        serde_json::json!({"body": "outsider", "user": {"login": "b", "type": "User"}, "author_association": "NONE"}),
    ];
    assert_eq!(TrustedComments::filtered(&policy, comments).bodies(), ["insider".to_string()]);
}

#[test]
fn issue_markers_read_the_send_back_and_kept_landings() {
    let marker = revise_marker("Title", "Body\n");
    // The shell's `$(...)` strips trailing newlines before hashing.
    assert_eq!(marker, revise_marker("Title\n", "Body"));
    assert!(marker.starts_with("<!-- champion:revise:body-") && marker.len() == 26 + 16 + 4);
    let review = format!("Champion Review: NEEDS REVISION\n{marker}");
    let route = format!("{}\nback to Curator", super::super::landed_prefilter::route_marker(902));
    let m = IssueMarkers::from_trusted(&trusted(&[&review, &route]), "Title", "Body");
    assert!(m.send_back);
    assert_eq!(m.landed_kept, vec![902]);
    // A revision changes the body, so the send-back is answered.
    let revised = IssueMarkers::from_trusted(&trusted(&[&review]), "Title", "Body\n## Revision");
    assert!(!revised.send_back);
}

#[test]
fn retired_labels_fold_through_the_registry() {
    assert_eq!(
        canonical_labels(&labels(&["loom:operator-only", "loom:issue"])),
        labels(&["loom:issue", "loom:operator"])
    );
    // The revise label maps to triage and takes curated with it (#11860).
    assert_eq!(
        canonical_labels(&labels(&["loom:needs-revision", "loom:curated"])),
        labels(&["loom:triage"])
    );
    // A retired name with no replacement is dropped.
    assert_eq!(
        canonical_labels(&labels(&["loom:urgent", "loom:issue"])),
        labels(&["loom:issue"])
    );
}
