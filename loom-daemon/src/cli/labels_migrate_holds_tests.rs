//! Fake-forge tests for `labels migrate --holds` (#11861). The fake records
//! every call, so "no write" and "never touched `loom:pr`" are assertable.

use super::*;
use loom_daemon::champion_hold::{CRITICAL_FILE_HOLD_MARKER, MERGE_RISK_HOLD_MARKER};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// One fake item: labels, is_pr, trusted comment bodies.
type FakeItem = (BTreeSet<String>, bool, Vec<String>);

#[derive(Default)]
struct Fake {
    items: RefCell<BTreeMap<u64, FakeItem>>,
    /// What the dependency classifier answers per escalated issue.
    escalations: BTreeMap<u64, EscalationMigration>,
    calls: RefCell<Vec<String>>,
    /// Calls (by index) answered with a rate limit.
    rate_limit_at: Option<usize>,
}

impl Fake {
    fn item(self, n: u64, labels: &[&str], is_pr: bool, comments: &[&str]) -> Self {
        self.items.borrow_mut().insert(
            n,
            (
                labels.iter().map(|s| s.to_string()).collect(),
                is_pr,
                comments.iter().map(|s| s.to_string()).collect(),
            ),
        );
        self
    }

    fn escalation(mut self, n: u64, m: EscalationMigration) -> Self {
        self.escalations.insert(n, m);
        self
    }

    fn log(&self, call: String) -> Result<(), ForgeError> {
        let mut calls = self.calls.borrow_mut();
        calls.push(call);
        if self.rate_limit_at == Some(calls.len() - 1) {
            return Err(ForgeError::RateLimited("API rate limit exceeded".into()));
        }
        Ok(())
    }

    fn labels(&self, n: u64) -> BTreeSet<String> {
        self.items.borrow()[&n].0.clone()
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| !c.starts_with("list ") && !c.starts_with("comments "))
            .cloned()
            .collect()
    }
}

impl HoldsForge for Fake {
    fn open_page(&self, label: &str, page: u32) -> Result<Vec<HoldItem>, ForgeError> {
        self.log(format!("list {label} {page}"))?;
        if page > 1 {
            return Ok(Vec::new());
        }
        Ok(self
            .items
            .borrow()
            .iter()
            .filter(|(_, (l, _, _))| l.contains(label))
            .map(|(n, (l, pr, _))| HoldItem {
                number: *n,
                labels: l.iter().cloned().collect(),
                is_pr: *pr,
            })
            .collect())
    }

    fn trusted_comments(&self, number: u64) -> Result<Vec<String>, ForgeError> {
        self.log(format!("comments #{number}"))?;
        Ok(self.items.borrow()[&number].2.clone())
    }

    fn park_hold(&self, number: u64, kind: HoldKind) -> Result<(), ForgeError> {
        self.log(format!("park #{number} {}", kind.reason()))?;
        self.items
            .borrow_mut()
            .get_mut(&number)
            .unwrap()
            .0
            .insert("loom:blocked".into());
        Ok(())
    }

    fn park_noop_hold(&self, number: u64) -> Result<(), ForgeError> {
        self.log(format!("park-noop #{number}"))?;
        self.items
            .borrow_mut()
            .get_mut(&number)
            .unwrap()
            .0
            .insert("loom:blocked".into());
        Ok(())
    }

    fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        self.log(format!("remove #{number} {label}"))?;
        let mut items = self.items.borrow_mut();
        if items.get_mut(&number).unwrap().0.remove(label) {
            Ok(())
        } else {
            Err(ForgeError::NotFound("HTTP 404".into()))
        }
    }

    fn migrate_escalation(&self, number: u64, apply: bool) -> EscalationMigration {
        let _ = self.log(format!("escalation #{number} apply={apply}"));
        self.escalations
            .get(&number)
            .cloned()
            .unwrap_or_else(|| EscalationMigration::Leave("no-escalation-record".into()))
    }
}

fn mr_hold() -> String {
    format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc -->\nheld")
}

#[test]
fn a_held_pr_keeps_loom_pr_through_the_migration() {
    // The acceptance criterion: a Champion-held PR moves to loom:blocked and
    // keeps its approval, in the dry run's plan and in the applied result.
    let hold = mr_hold();
    let fake = Fake::default().item(7, &["loom:pr", "loom:operator"], true, &[&hold]);

    let dry = run(&fake, "o/r", false);
    assert_eq!(
        dry.events,
        vec![Event::Hold {
            item: 7,
            hold: "merge-risk",
            removed: vec!["loom:operator".into()],
        }]
    );
    assert_eq!(dry.exit, 3, "a dry run with work pending");
    assert!(fake.writes().is_empty(), "a dry run writes nothing: {:?}", fake.writes());
    let rendered = {
        let mut s = String::new();
        dry.render(&mut s);
        s
    };
    assert!(rendered.contains("loom:pr kept"), "{rendered}");

    let applied = run(&fake, "o/r", true);
    assert_eq!(applied.exit, 0);
    assert_eq!(
        fake.writes(),
        vec![
            "park #7 champion merge-risk hold",
            "remove #7 loom:operator"
        ],
        "add before remove, and never a write naming loom:pr"
    );
    let labels = fake.labels(7);
    assert!(labels.contains("loom:pr"), "{labels:?}");
    assert!(labels.contains("loom:blocked"), "{labels:?}");
    assert!(!labels.contains("loom:operator"), "{labels:?}");
    assert!(loom_daemon::champion_hold::is_champion_hold(
        &labels.iter().cloned().collect::<Vec<_>>(),
        &[hold]
    ));
}

#[test]
fn a_critical_file_hold_moves_and_retired_names_stay() {
    let cf = format!("{CRITICAL_FILE_HOLD_MARKER}\nheld");
    let fake =
        Fake::default().item(8, &["loom:pr", "loom:operator", "loom:operator-only"], true, &[&cf]);
    let r = run(&fake, "o/r", true);
    assert_eq!(r.summary.holds, 1);
    assert!(fake
        .writes()
        .contains(&"park #8 champion critical-file hold".to_string()));
    let labels = fake.labels(8);
    assert!(labels.contains("loom:pr") && labels.contains("loom:blocked"));
    assert!(!labels.contains("loom:operator"));
    // A retired name may be a human's: never removed here (#11861 review).
    assert!(labels.contains("loom:operator-only"), "{labels:?}");
}

#[test]
fn a_human_hold_is_never_touched() {
    // No standing marker (or one that was released): a human put the label
    // on, and since #11861 it parks the PR as it is. The #11795 Q1
    // "+ loom:blocked" pre-pass is not needed for these.
    let released = format!("{MERGE_RISK_HOLD_MARKER}\nheld",);
    let reversal = "merged\n<!-- champion:merge-risk-hold-cleared -->".to_string();
    let fake = Fake::default()
        .item(1, &["loom:pr", "loom:operator"], true, &["looks risky to me"])
        .item(2, &["loom:operator-only"], true, &[])
        .item(3, &["loom:pr", "loom:operator"], true, &[&released, &reversal])
        .item(4, &["loom:operator"], false, &[])
        .item(5, &["loom:pr", "loom:operator", "loom:frozen"], true, &[&mr_hold()]);
    let r = run(&fake, "o/r", true);
    assert!(r.events.is_empty(), "{:?}", r.events);
    assert_eq!(r.summary.untouched, 5);
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert_eq!(r.exit, 0);
}

#[test]
fn dependency_only_escalations_go_to_the_classifier() {
    let fake = Fake::default()
        .item(10, &["loom:curated", "loom:operator"], false, &[])
        .item(11, &["loom:architect", "loom:operator"], false, &[])
        .item(12, &["loom:hermit", "loom:operator"], false, &[])
        .item(13, &["loom:curated", "loom:operator"], false, &[])
        .escalation(10, EscalationMigration::Park(vec!["#3".into()]))
        .escalation(11, EscalationMigration::Release)
        .escalation(12, EscalationMigration::Leave("merits-finding".into()));
    let r = run(&fake, "o/r", false);
    assert_eq!(
        r.events,
        vec![
            Event::Park {
                item: 10,
                blocked_by: vec!["#3".into()]
            },
            Event::Release { item: 11 },
            Event::Left {
                item: 12,
                why: "merits-finding".into()
            },
        ]
    );
    assert_eq!((r.summary.parked, r.summary.released, r.summary.left), (1, 1, 1));
    assert_eq!(r.summary.untouched, 1, "#13 has no escalation record");
    assert!(fake
        .calls
        .borrow()
        .iter()
        .any(|c| c == "escalation #10 apply=false"));
}

#[test]
fn a_rate_limit_stops_the_run() {
    let fake = Fake {
        rate_limit_at: Some(0),
        ..Fake::default()
    }
    .item(7, &["loom:pr", "loom:operator"], true, &[&mr_hold()]);
    let r = run(&fake, "o/r", true);
    assert_eq!(r.exit, 4);
    assert!(r.rate_limited);
    assert!(fake.writes().is_empty());
}

#[test]
fn a_refused_write_scope_makes_no_call() {
    let t = Target {
        repo: "o/r".into(),
        root: std::path::PathBuf::from("/nonexistent"),
    };
    let forge_for = |_: &Target| -> Box<dyn HoldsForge> { panic!("no forge call when refused") };
    let reports = run_targets(&[t], true, &forge_for, &|_| Err("not this repo".into()));
    assert_eq!(reports[0].exit, 2);
}

#[test]
fn item_lines_parse_with_the_pr_flag() {
    let items =
        parse_hold_items("[7,[\"loom:pr\",\"loom:operator\"],true]\n[8,[],false]\n").unwrap();
    assert_eq!(
        items,
        vec![
            HoldItem {
                number: 7,
                labels: vec!["loom:pr".into(), "loom:operator".into()],
                is_pr: true
            },
            HoldItem {
                number: 8,
                labels: vec![],
                is_pr: false
            }
        ]
    );
}

/// The Judge's live shape: a held PR out at Doctor (#6720 kept the label on the
/// round-trip): `loom:changes-requested` + `loom:operator` + a standing marker.
/// It gets the park record and `loom:blocked` too, which Doctor still works
/// (`champion_hold::doctor_may_work`), so nothing strands.
#[test]
fn a_held_pr_out_at_doctor_is_parked_and_stays_doctor_work() {
    let hold = mr_hold();
    let fake =
        Fake::default().item(9, &["loom:changes-requested", "loom:operator"], true, &[&hold]);
    let r = run(&fake, "o/r", true);
    assert_eq!(r.summary.holds, 1);
    assert_eq!(
        fake.writes(),
        vec![
            "park #9 champion merge-risk hold",
            "remove #9 loom:operator"
        ]
    );
    let labels: Vec<String> = fake.labels(9).into_iter().collect();
    assert!(labels.iter().any(|l| l == "loom:changes-requested"));
    assert!(loom_daemon::champion_hold::doctor_may_work(&labels, &[hold]));
}

/// The Judge's other live shape: `loom:operator-only` and
/// `loom:operator-mechanical` beside a merge-risk marker. The hold is recorded
/// and parked; the human-applied names are left for the retired-name
/// migration (which maps them to `loom:operator`, a park).
#[test]
fn human_hold_names_beside_a_marker_are_never_removed() {
    let hold = mr_hold();
    let fake = Fake::default().item(
        10,
        &["loom:pr", "loom:operator-only", "loom:operator-mechanical"],
        true,
        &[&hold],
    );
    let r = run(&fake, "o/r", true);
    assert_eq!(r.summary.holds, 1);
    assert_eq!(fake.writes(), vec!["park #10 champion merge-risk hold"], "no removal at all");
    let labels = fake.labels(10);
    for kept in [
        "loom:pr",
        "loom:operator-only",
        "loom:operator-mechanical",
        "loom:blocked",
    ] {
        assert!(labels.contains(kept), "{kept}: {labels:?}");
    }
}

/// An escalation still under a retired name is the retired-name migration's.
#[test]
fn an_escalation_without_loom_operator_itself_is_untouched() {
    let fake = Fake::default()
        .item(
            20,
            &[
                "loom:curated",
                "loom:operator-only",
                "loom:operator-blocked",
            ],
            false,
            &[],
        )
        .escalation(20, EscalationMigration::Park(vec!["#3".into()]));
    let r = run(&fake, "o/r", true);
    assert!(r.events.is_empty(), "{:?}", r.events);
    assert_eq!(r.summary.untouched, 1);
    assert!(fake.writes().is_empty());
}

/// #11861 review: a marker PR already back at review (`loom:review-requested`
/// + `loom:operator`) is parked too, and stays Judge's work after the move.
#[test]
fn a_held_pr_at_review_is_parked_and_stays_judge_work() {
    let hold = mr_hold();
    let fake =
        Fake::default().item(11, &["loom:review-requested", "loom:operator"], true, &[&hold]);
    let r = run(&fake, "o/r", true);
    assert_eq!(r.summary.holds, 1);
    assert_eq!(
        fake.writes(),
        vec![
            "park #11 champion merge-risk hold",
            "remove #11 loom:operator"
        ]
    );
    let labels: Vec<String> = fake.labels(11).into_iter().collect();
    assert!(loom_daemon::champion_hold::judge_may_review(&labels, &[hold]));
}

#[test]
fn a_capped_hold_routed_to_a_human_keeps_its_operator_label() {
    // #11905: the Capped-PR Recovery Pass adds loom:operator to a capped
    // Champion hold so it reaches the operator inbox. That is a routing to a
    // human, not the pre-#11861 hold label: the migration must not take it off.
    let rec = format!(
        "{}\nreview, close or release",
        loom_daemon::champion_hold::CAPPED_HOLD_RECOMMENDATION_MARKER
    );
    let labels = ["loom:changes-requested", "loom:blocked", "loom:operator"];
    let fake = Fake::default().item(9, &labels, true, &[&mr_hold(), &rec]);
    let r = run(&fake, "o/r", true);
    assert!(r.events.is_empty(), "{:?}", r.events);
    assert_eq!(r.summary.untouched, 1);
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(fake.labels(9).contains("loom:operator"));
}

fn noop_notice(released: bool) -> Vec<String> {
    use loom_daemon::sweep_registry::{
        NOOP_HOLD_APPLIED_COMMENT_MARKER, NOOP_HOLD_COMMENT_MARKER,
        NOOP_HOLD_RELEASED_COMMENT_MARKER,
    };
    let mut v = vec![format!(
        "{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_APPLIED_COMMENT_MARKER}\n**Held after 3 \
         consecutive no-op sweeps.**\n\nFingerprint compared: `labels=[loom:curated];open=true`"
    )];
    if released {
        v.push(format!(
            "{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_RELEASED_COMMENT_MARKER}\nreleased"
        ));
    }
    v
}

/// #12024: the old no-op hold shape (`loom:operator` + a standing hold
/// notice) moves to `loom:blocked` + the `no-op hold` park record, and loses
/// `loom:operator`; it never reaches the escalation classifier.
#[test]
fn a_pre_12024_noop_hold_moves_off_the_operator_label() {
    let held = noop_notice(false);
    let held: Vec<&str> = held.iter().map(String::as_str).collect();
    let fake = Fake::default().item(20, &["loom:curated", "loom:operator"], false, &held);

    let dry = run(&fake, "o/r", false);
    assert_eq!(dry.events, vec![Event::NoopHold { item: 20 }]);
    assert_eq!((dry.summary.noop_holds, dry.exit), (1, 3));
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());

    let applied = run(&fake, "o/r", true);
    assert_eq!(applied.summary.noop_holds, 1);
    assert_eq!(fake.writes(), vec!["park-noop #20", "remove #20 loom:operator"]);
    let labels = fake.labels(20);
    assert!(labels.contains("loom:blocked") && labels.contains("loom:curated"));
    assert!(!labels.contains("loom:operator"));
    assert!(!fake
        .calls
        .borrow()
        .iter()
        .any(|c| c.starts_with("escalation")));
    assert_eq!(applied.exit, 0);
}

/// A released no-op hold, or none at all, is not this shape: a human's
/// `loom:operator` beside it stays, and the escalation pass still sees it.
#[test]
fn a_released_noop_hold_is_left_to_the_other_shapes() {
    let released = noop_notice(true);
    let released: Vec<&str> = released.iter().map(String::as_str).collect();
    let fake = Fake::default().item(21, &["loom:curated", "loom:operator"], false, &released);
    let r = run(&fake, "o/r", true);
    assert!(r.events.is_empty(), "{:?}", r.events);
    assert_eq!(r.summary.untouched, 1);
    assert_eq!(fake.writes(), vec!["escalation #21 apply=true"], "only the classifier's look");
}
