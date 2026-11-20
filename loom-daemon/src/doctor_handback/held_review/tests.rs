use super::*;
use serde_json::json;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

struct Fixture {
    labels: Vec<String>,
    head: String,
    trusted: Vec<Value>,
    park: String,
    owner: bool,
    changed: bool,
    idle: bool,
    fail_remove: bool,
    race: Option<Value>,
    writes: Vec<String>,
}

impl Fixture {
    fn new(kind: HoldKind) -> Self {
        let marker = match kind {
            HoldKind::CriticalFile => champion_hold::CRITICAL_FILE_HOLD_MARKER,
            HoldKind::MergeRisk => champion_hold::MERGE_RISK_HOLD_MARKER,
            HoldKind::Ratification => champion_hold::ratification::HOLD_MARKER,
        };
        Self {
            labels: [APPROVED, BLOCKED, CLAIM].map(String::from).to_vec(),
            head: B.into(),
            trusted: vec![
                json!({"id":1,"body":format!("<!-- loom:verdict-sha sha={A} verdict=approved -->")}),
                json!({"id":2,"body":format!("{marker}\n<!-- champion:hold-state head={A} -->")}),
            ],
            park: format!(
                "<!-- loom:park Blocked by: (unstated) by=champion reason=\"{}\" -->",
                kind.reason()
            ),
            owner: true,
            changed: true,
            idle: true,
            fail_remove: false,
            race: None,
            writes: Vec::new(),
        }
    }
}

impl Forge for Fixture {
    fn observe(&mut self, mode: Mode) -> Option<Observation> {
        Some(Observation {
            snapshot: self.snapshot()?,
            park_body: self.park.clone(),
            trusted: self.trusted.clone(),
            can_act: self.owner == (mode == Mode::Doctor),
        })
    }

    fn content_changed(&mut self, _: &Intent, _: &str) -> Option<bool> {
        Some(self.changed)
    }

    fn record(&mut self, intent: &Intent) -> bool {
        self.trusted.push(json!({"id":3,"body":intent.body()}));
        self.writes.push("intent".into());
        true
    }

    fn ensure_idle(&mut self) -> bool {
        self.idle
    }

    fn add(&mut self, label: &str) -> bool {
        self.writes.push(format!("+{label}"));
        self.labels.push(label.into());
        if let Some(race) = self.race.take() {
            self.trusted.push(race);
        }
        true
    }

    fn remove(&mut self, label: &str) -> bool {
        if label == APPROVED && self.fail_remove {
            return false;
        }
        self.writes.push(format!("-{label}"));
        self.labels.retain(|candidate| candidate != label);
        true
    }

    fn snapshot(&mut self) -> Option<Snapshot> {
        Some(Snapshot {
            labels: self.labels.clone(),
            head_sha: self.head.clone(),
        })
    }
}

#[test]
fn verified_held_repairs_take_the_existing_judge_queue_with_the_hold_intact() {
    for kind in [
        HoldKind::Ratification,
        HoldKind::MergeRisk,
        HoldKind::CriticalFile,
    ] {
        let mut forge = Fixture::new(kind);
        // The targeted critical-file incident has the trusted hold marker
        // and the label, with no body park record.
        if kind == HoldKind::CriticalFile {
            forge.park.clear();
        }
        assert!(matches!(
            super::super::decide(forge.snapshot().as_ref(), B),
            super::super::Plan::AlreadyAdvanced(_)
        ));
        assert_eq!(route(&mut forge, Mode::Doctor, B), Outcome::Requeued);
        assert_eq!(
            forge.writes,
            [
                "intent",
                "+loom:review-requested",
                "-loom:pr",
                "-loom:treating"
            ]
        );
        let trusted = crate::comment_trust::records::bodies(&forge.trusted);
        assert_eq!(standing_hold(&trusted), Some(kind));
        assert!(champion_hold::judge_may_review(&forge.labels, &trusted));
        assert_eq!(forge.labels, [BLOCKED, QUEUE]);
    }
}

#[test]
fn retry_after_the_queue_add_uses_the_same_intent_and_never_acts_with_a_peer() {
    let mut forge = Fixture::new(HoldKind::CriticalFile);
    forge.fail_remove = true;
    assert_eq!(route(&mut forge, Mode::Doctor, B), Outcome::Interrupted);
    assert_eq!(forge.labels, [APPROVED, BLOCKED, CLAIM, QUEUE]);
    let trusted = crate::comment_trust::records::bodies(&forge.trusted);
    assert!(!champion_hold::judge_may_review(&forge.labels, &trusted));
    assert_eq!(route(&mut forge, Mode::Recover, B), Outcome::Ineligible);
    forge.owner = false;
    forge.fail_remove = false;
    forge.labels.retain(|label| label != CLAIM);
    assert_eq!(route(&mut forge, Mode::Recover, B), Outcome::Requeued);
    assert_eq!(route(&mut forge, Mode::Recover, B), Outcome::Ineligible);
    assert_eq!(forge.writes, ["intent", "+loom:review-requested", "-loom:pr"]);
    assert_eq!(forge.labels, [BLOCKED, QUEUE]);
}

#[test]
fn blocked_approval_with_no_exact_intent_is_never_recovered() {
    let mut forge = Fixture::new(HoldKind::CriticalFile);
    forge.owner = false;
    assert_eq!(route(&mut forge, Mode::Recover, B), Outcome::Ineligible);
    assert!(forge.writes.is_empty());
}

#[test]
fn released_foreign_equivalent_parked_moved_and_unauthorized_repairs_are_inert() {
    for case in [
        "release",
        "foreign",
        "equivalent",
        "operator",
        "frozen",
        "moved",
        "peer",
        "no-claim",
        "unanchored",
    ] {
        let mut forge = Fixture::new(HoldKind::CriticalFile);
        match case {
            "release" => forge
                .trusted
                .push(json!({"id":3,"body":champion_hold::CRITICAL_FILE_CLEARED_MARKER})),
            "foreign" => {
                forge.park =
                    "<!-- loom:park Blocked by: #7 by=builder reason=\"dependency\" -->".into()
            }
            "equivalent" => forge.changed = false,
            "operator" => forge.labels.push("loom:operator".into()),
            "frozen" => forge.labels.push("loom:frozen".into()),
            "moved" => forge.head = C.into(),
            "peer" => forge.owner = false,
            "no-claim" => forge.labels.retain(|label| label != CLAIM),
            "unanchored" => {
                forge.trusted.pop();
            }
            _ => unreachable!(),
        }
        assert_eq!(route(&mut forge, Mode::Doctor, B), Outcome::Ineligible, "{case}");
        assert!(forge.writes.is_empty(), "{case}");
    }
}

#[test]
fn unreadable_or_occupied_merge_admission_leaves_the_hold_and_approval_in_place() {
    let mut forge = Fixture::new(HoldKind::CriticalFile);
    forge.idle = false;
    assert_eq!(route(&mut forge, Mode::Doctor, B), Outcome::Interrupted);
    assert_eq!(forge.writes, ["intent"]);
    assert_eq!(forge.labels, [APPROVED, BLOCKED, CLAIM]);
}

#[test]
fn a_new_verdict_during_the_write_prevents_removing_approval() {
    let mut forge = Fixture::new(HoldKind::CriticalFile);
    forge.race =
        Some(json!({"id":4,"body":format!("<!-- loom:verdict-sha sha={B} verdict=approved -->")}));
    assert_eq!(route(&mut forge, Mode::Doctor, B), Outcome::Interrupted);
    assert!(!forge.writes.iter().any(|op| op == "-loom:pr"));
    assert!(forge.labels.iter().any(|label| label == BLOCKED));
}
