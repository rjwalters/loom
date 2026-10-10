use super::*;
use serde_json::json;

const PUSHED: &str = "846ed44c14e7dd4e87bf17efb554bca0d57c05b2";
const OTHER: &str = "ed058db8c0000000000000000000000000000000";

fn snap(labels: &[&str], head: &str) -> Snapshot {
    Snapshot {
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        head_sha: head.to_string(),
    }
}

/// An in-memory PR. `after_add` labels land (a rival's write) right after the
/// hand-back's add; `reads_fail_from` makes the Nth and later reads fail.
#[derive(Default)]
struct Fake {
    labels: Vec<String>,
    head: String,
    fail_add: bool,
    /// Only adds of this label fail.
    fail_add_of: Option<&'static str>,
    fail_remove: Vec<&'static str>,
    after_add: Vec<&'static str>,
    after_add_remove: Vec<&'static str>,
    /// `(removed, lands)`: a rival's label lands right after `removed` is deleted.
    after_remove: Vec<(&'static str, &'static str)>,
    /// What `rejected_at` answers: a trusted changes-requested marker at the head.
    rejected: Option<bool>,
    /// Answers for successive `rejected_at` calls, before falling back to `rejected`.
    rejected_seq: Vec<Option<bool>>,
    /// The head becomes this right after the hand-back's add.
    head_after_add: Option<&'static str>,
    reads: usize,
    reads_fail_from: Option<usize>,
    calls: Vec<String>,
}

impl Fake {
    fn new(labels: &[&str], head: &str) -> Self {
        Self {
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            head: head.to_string(),
            rejected: Some(false),
            ..Self::default()
        }
    }
    fn sorted(&self) -> Vec<String> {
        let mut l = self.labels.clone();
        l.sort();
        l
    }
    fn writes(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter(|c| c.as_str() != "read" && c.as_str() != "rejected_at")
            .map(String::as_str)
            .collect()
    }
}

impl Forge for Fake {
    fn read(&mut self) -> Option<Snapshot> {
        self.calls.push("read".into());
        self.reads += 1;
        if self.reads_fail_from.is_some_and(|n| self.reads >= n) {
            return None;
        }
        Some(Snapshot {
            labels: self.labels.clone(),
            head_sha: self.head.clone(),
        })
    }
    fn add(&mut self, label: &str) -> bool {
        self.calls.push(format!("add {label}"));
        if self.fail_add || self.fail_add_of == Some(label) {
            return false;
        }
        if !self.labels.iter().any(|l| l == label) {
            self.labels.push(label.to_string());
        }
        for l in std::mem::take(&mut self.after_add) {
            if !self.labels.iter().any(|x| x == l) {
                self.labels.push(l.to_string());
            }
        }
        if let Some(h) = self.head_after_add.take() {
            self.head = h.to_string();
        }
        for l in std::mem::take(&mut self.after_add_remove) {
            self.labels.retain(|x| x != l);
        }
        true
    }
    fn remove(&mut self, label: &str) -> bool {
        self.calls.push(format!("remove {label}"));
        if self.fail_remove.contains(&label) {
            return false;
        }
        self.labels.retain(|l| l != label);
        for (_, lands) in self.after_remove.iter().filter(|(r, _)| *r == label) {
            if !self.labels.iter().any(|x| x == lands) {
                self.labels.push((*lands).to_string());
            }
        }
        true
    }
    fn rejected_at(&mut self, _head: &str) -> Option<bool> {
        self.calls.push("rejected_at".into());
        if !self.rejected_seq.is_empty() {
            return self.rejected_seq.remove(0);
        }
        self.rejected
    }
}

fn names(v: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = v.iter().map(|s| (*s).to_string()).collect();
    v.sort();
    v
}

// --- decide: one test per outcome row ---------------------------------------

#[test]
fn claim_intact_same_head_nothing_advanced_hands_back() {
    let s = snap(&[CHANGES, CLAIM], PUSHED);
    assert_eq!(decide(Some(&s), PUSHED), Plan::HandBack);
    // A Doctor treating a CI failure with no changes-requested label.
    let s = snap(&[CLAIM, "loom:ci-failure"], PUSHED);
    assert_eq!(decide(Some(&s), &PUSHED.to_uppercase()), Plan::HandBack);
}

#[test]
fn claim_intact_but_advanced_releases_only_the_claim() {
    for on in ADVANCED {
        let s = snap(&[CLAIM, on], PUSHED);
        assert_eq!(
            decide(Some(&s), PUSHED),
            Plan::AlreadyAdvanced(vec![(*on).to_string()]),
            "{on}"
        );
    }
}

#[test]
fn claim_absent_writes_nothing() {
    let s = snap(&[CHANGES], PUSHED);
    assert_eq!(decide(Some(&s), PUSHED), Plan::ClaimLost);
    // Claim gone wins over a moved head: there is nothing of ours to release.
    let s = snap(&[QUEUE], OTHER);
    assert_eq!(decide(Some(&s), PUSHED), Plan::ClaimLost);
}

#[test]
fn head_moved_writes_no_state_label() {
    let s = snap(&[CHANGES, CLAIM], OTHER);
    assert_eq!(decide(Some(&s), PUSHED), Plan::HeadMoved(OTHER.into()));
    // An empty expected head never matches.
    let s = snap(&[CHANGES, CLAIM], PUSHED);
    assert_eq!(decide(Some(&s), "  "), Plan::HeadMoved(PUSHED.into()));
}

#[test]
fn unreadable_fails_closed() {
    assert_eq!(decide(None, PUSHED), Plan::Unreadable);
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.reads_fail_from = Some(1);
    let out = run(&mut f, PUSHED);
    assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
    assert!(f.writes().is_empty(), "wrote {:?}", f.writes());
    assert_eq!(out.render().1, EXIT_FAILED);
}

#[test]
fn snapshot_reads_labels_and_head_from_a_pull_document() {
    let doc = json!({"head": {"sha": PUSHED}, "labels": [{"name": CLAIM}, {"name": CHANGES}]});
    assert_eq!(snapshot_from_pull(&doc), Some(snap(&[CLAIM, CHANGES], PUSHED)));
    assert_eq!(snapshot_from_pull(&json!({"labels": []})), None, "no head");
    assert_eq!(snapshot_from_pull(&json!({"head": {"sha": PUSHED}})), None, "no labels");
    assert_eq!(snapshot_from_pull(&json!({"head": {"sha": ""}, "labels": []})), None);
}

// --- run: the writes --------------------------------------------------------

#[test]
fn hand_back_adds_before_it_removes() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    assert_eq!(run(&mut f, PUSHED), Outcome::HandedBack);
    assert_eq!(
        f.writes(),
        vec![
            format!("add {QUEUE}"),
            format!("remove {CHANGES}"),
            format!("remove {CLAIM}")
        ]
    );
    assert_eq!(f.sorted(), names(&[QUEUE]));
    assert_eq!(
        run(&mut Fake::new(&[CHANGES, CLAIM], PUSHED), PUSHED)
            .render()
            .1,
        0
    );
}

#[test]
fn a_failed_add_removes_nothing() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.fail_add = true;
    let out = run(&mut f, PUSHED);
    assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
    assert_eq!(f.writes(), vec![format!("add {QUEUE}")]);
    assert_eq!(f.sorted(), names(&[CHANGES, CLAIM]), "state left in place");
}

#[test]
fn every_intermediate_state_keeps_a_lifecycle_label() {
    // Replay the hand-back's own calls one at a time and check each prefix.
    let lifecycle = [QUEUE, CHANGES, CLAIM, "loom:reviewing", "loom:pr"];
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    run(&mut f, PUSHED);
    let mut state: Vec<String> = vec![CHANGES.into(), CLAIM.into()];
    for call in f.writes() {
        if let Some(l) = call.strip_prefix("add ") {
            state.push(l.into());
        } else if let Some(l) = call.strip_prefix("remove ") {
            state.retain(|x| x != l);
        }
        assert!(
            state.iter().any(|l| lifecycle.contains(&l.as_str())),
            "after `{call}` the PR had no lifecycle label: {state:?}"
        );
    }
}

#[test]
fn already_advanced_is_a_success_that_only_releases_the_claim() {
    let mut f = Fake::new(&[CLAIM, QUEUE], PUSHED);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::AlreadyAdvanced(vec![QUEUE.into()]));
    assert_eq!(f.writes(), vec![format!("remove {CLAIM}")]);
    assert_eq!(f.sorted(), names(&[QUEUE]));
    assert_eq!(out.render().1, EXIT_ALREADY_ADVANCED);
    assert!(out
        .render()
        .0
        .starts_with("LOOM-DOCTOR-HANDBACK ALREADY-ADVANCED"));
}

#[test]
fn claim_lost_writes_nothing_at_all() {
    let mut f = Fake::new(&[CHANGES], PUSHED);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::ClaimLost);
    assert!(f.writes().is_empty());
    assert_eq!(out.render().1, EXIT_CLAIM_LOST);
}

#[test]
fn head_moved_releases_only_the_claim() {
    let mut f = Fake::new(&[CHANGES, CLAIM], OTHER);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::HeadMoved(OTHER.into()));
    assert_eq!(f.writes(), vec![format!("remove {CLAIM}")]);
    assert_eq!(f.sorted(), names(&[CHANGES]));
    assert_eq!(out.render().1, EXIT_HEAD_MOVED);
}

#[test]
fn a_claim_release_that_does_not_hold_fails() {
    let mut f = Fake::new(&[CLAIM, "loom:pr"], PUSHED);
    f.fail_remove = vec![CLAIM];
    assert!(matches!(run(&mut f, PUSHED), Outcome::Failed(_)));
}

/// The #9388 incident: the Doctor pushes, the stale-verdict guard re-queues
/// the new head, a Judge approves it, and only then does the Doctor's
/// hand-back run. The result must be `loom:pr` alone.
#[test]
fn incident_sequence_guard_requeues_judge_approves_then_doctor_hands_back() {
    // Doctor claimed a changes-requested PR and pushed PUSHED.
    let mut pr = vec![CHANGES.to_string(), CLAIM.to_string()];
    // Stale-verdict guard: head moved, so changes-requested -> review-requested.
    pr.retain(|l| l != CHANGES);
    pr.push(QUEUE.into());
    // Judge: verdict-labels approved (add loom:pr, drop the queue labels).
    pr.push("loom:pr".into());
    pr.retain(|l| l != QUEUE && l != "loom:reviewing");
    let refs: Vec<&str> = pr.iter().map(String::as_str).collect();
    let mut f = Fake::new(&refs, PUSHED);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::AlreadyAdvanced(vec!["loom:pr".into()]));
    assert_eq!(f.sorted(), names(&["loom:pr"]), "no review-requested re-added");
    assert!(!f.writes().iter().any(|w| w.starts_with("add ")));
}

/// Same incident, but the Judge's approval lands between the hand-back's
/// pre-read and its re-read: the hand-back withdraws its own add.
#[test]
fn a_verdict_landing_during_the_write_withdraws_the_own_add() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.after_add = vec!["loom:pr"];
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::Raced(vec!["loom:pr".into()]));
    assert_eq!(f.sorted(), names(&["loom:pr"]));
    assert_eq!(f.writes().last().copied(), Some(format!("remove {QUEUE}").as_str()));
    assert_eq!(out.render().1, EXIT_RACED);
}

#[test]
fn a_rival_that_already_displaced_the_add_is_reported_not_failed() {
    // A changes-requested verdict on the new head removes review-requested itself.
    let mut f = Fake::new(&[CLAIM, "loom:ci-failure"], PUSHED);
    f.after_add = vec![CHANGES];
    f.after_add_remove = vec![QUEUE];
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::Raced(vec![CHANGES.into()]));
    assert!(f.labels.iter().any(|l| l == CHANGES));
}

#[test]
fn a_withdrawal_that_fails_is_loud() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.after_add = vec!["loom:pr"];
    f.fail_remove = vec![QUEUE];
    assert!(matches!(run(&mut f, PUSHED), Outcome::Failed(_)));
}

/// A Judge claim is an overlay on `loom:review-requested`, not a verdict: one
/// landing mid-write must leave both labels on, never withdraw the queue label.
#[test]
fn a_judge_claim_landing_during_the_write_keeps_the_queue_label() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.after_add = vec!["loom:reviewing"];
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::HandedBack);
    assert_eq!(f.sorted(), names(&[QUEUE, "loom:reviewing"]));
    assert!(!f.writes().contains(&format!("remove {QUEUE}").as_str()));
}

#[test]
fn an_unverifiable_or_partial_hand_back_fails() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.reads_fail_from = Some(2);
    assert!(matches!(run(&mut f, PUSHED), Outcome::Failed(_)));
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.fail_remove = vec![CHANGES];
    let out = run(&mut f, PUSHED);
    assert!(
        matches!(&out, Outcome::Failed(why) if why.contains("still carries loom:changes-requested")),
        "{out:?}"
    );
}

/// A raced hand-back whose claim release failed is not "done": exit 13 tells
/// the Doctor to write nothing more, which would strand `loom:treating`.
#[test]
fn a_race_that_leaves_the_claim_on_is_a_failure_not_a_race() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.after_add = vec!["loom:pr"];
    f.fail_remove = vec![CLAIM];
    let out = run(&mut f, PUSHED);
    assert!(
        matches!(&out, Outcome::Failed(why) if why.contains("still carries loom:treating")),
        "{out:?}"
    );
    // The own add is still withdrawn, so the verdict label stands alone.
    assert!(!f.labels.iter().any(|l| l == QUEUE), "{:?}", f.labels);
    assert_eq!(out.render().1, EXIT_FAILED);
}

/// The normal feedback path starts with CHANGES + CLAIM. A Judge that rejected
/// the pushed head meanwhile left CHANGES on: it is the new rejection, so the
/// hand-back must keep it and report a race, not delete it and report failure.
#[test]
fn a_new_rejection_during_a_hand_back_that_starts_with_changes_is_kept() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.rejected = Some(true);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::Raced(vec![CHANGES.into()]));
    assert_eq!(f.sorted(), names(&[CHANGES]), "rejection kept, claim and own add gone");
    assert!(!f.writes().contains(&format!("remove {CHANGES}").as_str()));
    assert_eq!(out.render().1, EXIT_RACED);
}

/// The Judge's rejection lands just after the hand-back deleted the old
/// CHANGES: the survivor is a raced verdict, not a failed transition.
#[test]
fn a_rejection_landing_right_after_the_changes_removal_is_a_race() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.after_remove = vec![(CHANGES, CHANGES)];
    f.after_add_remove = vec![];
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::Raced(vec![CHANGES.into()]));
    assert_eq!(f.sorted(), names(&[CHANGES]));
}

/// Verdict evidence that cannot be read must not be guessed: nothing but the
/// own add is touched, and that is withdrawn.
#[test]
fn unreadable_verdict_evidence_leaves_changes_and_claim_alone() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.rejected = None;
    let out = run(&mut f, PUSHED);
    assert!(matches!(&out, Outcome::Failed(why) if why.contains("withdrew own")), "{out:?}");
    assert_eq!(f.sorted(), names(&[CHANGES, CLAIM]));
}

/// `rejected_at` said no, then a Judge on another host rejected the pushed head
/// before the DELETE: the DELETE took the new rejection, so it is put back.
#[test]
fn a_rejection_landing_between_the_evidence_read_and_the_delete_is_restored() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.rejected_seq = vec![Some(false), Some(true)];
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::Raced(vec![CHANGES.into()]));
    assert_eq!(f.sorted(), names(&[CHANGES]), "rejection restored, claim and own add gone");
    assert_eq!(out.render().1, EXIT_RACED);
}

#[test]
fn a_rejection_that_cannot_be_restored_is_loud() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.rejected_seq = vec![Some(false), Some(true)];
    f.fail_add_of = Some(CHANGES);
    let out = run(&mut f, PUSHED);
    assert!(
        matches!(&out, Outcome::Failed(why) if why.contains("could not be restored")),
        "{out:?}"
    );
}

#[test]
fn unreadable_evidence_after_the_delete_restores_changes_and_withdraws_the_add() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.rejected_seq = vec![Some(false), None];
    let out = run(&mut f, PUSHED);
    assert!(matches!(&out, Outcome::Failed(why) if why.contains("restored it")), "{out:?}");
    assert_eq!(
        f.sorted(),
        names(&[CHANGES, CLAIM]),
        "pre-state restored, claim kept for a retry"
    );
}

/// A push between the first read and the verifying read: the hand-back is not
/// verified for that head, so it stands down and restores what it touched.
#[test]
fn a_head_that_moves_during_the_write_is_not_a_verified_hand_back() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.head_after_add = Some(OTHER);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::HeadMoved(OTHER.into()));
    assert_eq!(out.render().1, EXIT_HEAD_MOVED);
    assert_eq!(f.sorted(), names(&[CHANGES]), "pre-state restored minus the claim");
}

#[test]
fn a_head_move_with_no_changes_label_only_withdraws_the_add() {
    let mut f = Fake::new(&[CLAIM, "loom:ci-failure"], PUSHED);
    f.head_after_add = Some(OTHER);
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::HeadMoved(OTHER.into()));
    assert_eq!(f.sorted(), names(&["loom:ci-failure"]));
}

/// The old rejection was removed, then a newer head was pushed and approved:
/// the stand-down must not put the old rejection back beside `loom:pr`.
#[test]
fn a_head_move_with_a_newer_approval_does_not_restore_the_old_rejection() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.head_after_add = Some(OTHER);
    f.after_remove = vec![(CHANGES, "loom:pr")];
    let out = run(&mut f, PUSHED);
    assert_eq!(out, Outcome::HeadMoved(OTHER.into()));
    assert_eq!(f.sorted(), names(&["loom:pr"]), "the newer approval stands alone");
}

#[test]
fn a_head_move_with_a_stuck_claim_is_a_failure_not_a_stand_down() {
    let mut f = Fake::new(&[CHANGES, CLAIM], PUSHED);
    f.head_after_add = Some(OTHER);
    f.fail_remove = vec![CLAIM];
    assert!(matches!(run(&mut f, PUSHED), Outcome::Failed(_)));
}
