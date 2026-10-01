//! Tests for combined-PR candidate preparation (#9688, contract ADR-0023).

use super::*;

// --- Identity -----------------------------------------------------------

#[test]
fn attempt_ids_are_deterministic_and_head_sensitive() {
    let a = attempt_id(&[(1, "aaa"), (2, "bbb")]);
    assert_eq!(a, attempt_id(&[(2, "bbb"), (1, "aaa")]), "member order must not matter");
    assert_ne!(a, attempt_id(&[(1, "ccc"), (2, "bbb")]), "a head change = a new attempt");
    assert!(a.starts_with("cons-"));
    assert_ne!(a, format!("seq-{}", &a[4..]), "distinct namespace from ordering plans");
}

#[test]
fn candidate_branch_lives_in_the_managed_namespace() {
    assert_eq!(candidate_branch("cons-ab12cd34"), "loom/consolidated/cons-ab12cd34");
}

// --- Eligibility --------------------------------------------------------

fn comp(number: u32, head: &str, files: &[&str]) -> ComponentState {
    ComponentState {
        number,
        state: "OPEN".to_string(),
        draft: false,
        head_sha: Some(head.to_string()),
        base_ref: "main".to_string(),
        labels: vec![],
        files: files.iter().map(|f| (*f).to_string()).collect(),
        additions: 10,
        deletions: 5,
    }
}

const H1: &str = "a111111111111111111111111111111111111111";
const H2: &str = "b222222222222222222222222222222222222222";

/// The component with the `loom:sequenced` hold label on it.
fn sequenced(mut c: ComponentState) -> ComponentState {
    c.labels.push(SEQUENCE_LABEL.to_string());
    c
}

fn clean_markers() -> std::collections::BTreeMap<u32, SequenceMarker> {
    std::collections::BTreeMap::new()
}

#[test]
fn a_clean_overlapping_group_is_eligible() {
    let group = [
        comp(1, H1, &["src/a.rs", "shared.rs"]),
        comp(2, H2, &["src/b.rs", "shared.rs"]),
    ];
    assert!(check_eligibility(
        &group,
        &clean_markers(),
        "main",
        "sibling tool fixes",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800
        }
    )
    .is_empty());
}

#[test]
fn every_operator_exclusion_and_hold_rejects() {
    for label in INELIGIBLE_LABELS {
        let mut c = comp(1, H1, &["a.rs", "shared.rs"]);
        c.labels.push(label.to_string());
        let group = [c, comp(2, H2, &["b.rs", "shared.rs"])];
        let failures = check_eligibility(
            &group,
            &clean_markers(),
            "main",
            "reason",
            &Bounds {
                max_components: 4,
                max_diff_lines: 800,
            },
        );
        assert!(
            matches!(&failures[..], [EligibilityFailure::Held { label: l, .. }] if l == label),
            "{label} must reject: {failures:?}"
        );
    }
}

#[test]
fn workflow_edits_are_rejected_per_the_operator_ruling() {
    let group = [
        comp(1, H1, &["a.rs", "shared.rs"]),
        comp(2, H2, &[".github/workflows/ci.yml", "shared.rs"]),
    ];
    let failures = check_eligibility(
        &group,
        &clean_markers(),
        "main",
        "reason",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800,
        },
    );
    assert!(
        matches!(
            &failures[..],
            [EligibilityFailure::WorkflowEdit { number: 2, file } ] if file == ".github/workflows/ci.yml"
        ),
        "{failures:?}"
    );
}

#[test]
fn bounds_reject_oversized_groups() {
    let group = [
        comp(1, H1, &["a.rs", "shared.rs"]),
        comp(2, H2, &["b.rs", "shared.rs"]),
        comp(3, "c333333333333333333333333333333333333333", &["c.rs", "shared.rs"]),
    ];
    let failures = check_eligibility(
        &group,
        &clean_markers(),
        "main",
        "reason",
        &Bounds {
            max_components: 2,
            max_diff_lines: 10_000,
        },
    );
    assert!(
        matches!(&failures[..], [EligibilityFailure::TooLarge { components: 3, .. }]),
        "{failures:?}"
    );

    let mut big = comp(1, H1, &["a.rs", "shared.rs"]);
    big.additions = 900;
    let group = [big, comp(2, H2, &["b.rs", "shared.rs"])];
    let failures = check_eligibility(
        &group,
        &clean_markers(),
        "main",
        "reason",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800,
        },
    );
    assert!(
        matches!(
            &failures[..],
            [EligibilityFailure::TooLarge {
                diff_lines: 920,
                ..
            }]
        ),
        "{failures:?}"
    );
}

#[test]
fn a_missing_rationale_is_a_failure_not_an_assumption() {
    let group = [comp(1, H1, &["shared.rs"]), comp(2, H2, &["shared.rs"])];
    let failures = check_eligibility(
        &group,
        &clean_markers(),
        "main",
        "  ",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800,
        },
    );
    assert!(matches!(&failures[..], [EligibilityFailure::NoReason]), "{failures:?}");
}

#[test]
fn path_disjointness_never_qualifies() {
    let group = [comp(1, H1, &["a.rs"]), comp(2, H2, &["b.rs"])];
    let failures = check_eligibility(
        &group,
        &clean_markers(),
        "main",
        "reason",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800,
        },
    );
    assert!(matches!(&failures[..], [EligibilityFailure::NoOverlap]), "{failures:?}");
}

#[test]
fn a_competing_consolidation_reservation_rejects() {
    let group = [
        sequenced(comp(1, H1, &["shared.rs"])),
        comp(2, H2, &["shared.rs"]),
    ];
    let mut markers = clean_markers();
    markers.insert(
        1,
        SequenceMarker {
            after: 99,
            pred_head: H1.to_string(),
            follower_head: H1.to_string(),
            plan: "cons-deadbeef".into(),
            source: Some("pass".into()),
        },
    );
    let failures = check_eligibility(
        &group,
        &markers,
        "main",
        "reason",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800,
        },
    );
    assert!(
        failures.iter().any(|f| matches!(f,
            EligibilityFailure::AlreadyReserved { number: 1, attempt } if attempt == "cons-deadbeef")),
        "the competing reservation must reject: {failures:?}"
    );
}

#[test]
fn an_out_of_group_ordering_predecessor_rejects_but_an_in_group_one_does_not() {
    let group = [
        comp(1, H1, &["shared.rs"]),
        sequenced(comp(2, H2, &["shared.rs"])),
    ];
    let mut markers = clean_markers();
    // #2 sequenced after #7 — outside the group ⇒ E7 failure.
    markers.insert(
        2,
        SequenceMarker {
            after: 7,
            pred_head: H1.to_string(),
            follower_head: H2.to_string(),
            plan: "seq-abc".into(),
            source: Some("pass".into()),
        },
    );
    let failures = check_eligibility(
        &group,
        &markers,
        "main",
        "reason",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800,
        },
    );
    assert!(
        matches!(
            &failures[..],
            [EligibilityFailure::SequencedOutsideGroup {
                number: 2,
                after: 7
            }]
        ),
        "{failures:?}"
    );

    // Same marker but after=1 — inside the group ⇒ fine (the group lands in order).
    markers.get_mut(&2).unwrap().after = 1;
    assert!(check_eligibility(
        &group,
        &markers,
        "main",
        "reason",
        &Bounds {
            max_components: 4,
            max_diff_lines: 800
        }
    )
    .is_empty());
}

// --- Mapping markers ----------------------------------------------------

#[test]
fn mapping_body_round_trips_through_parse() {
    let body = mapping_body(
        "cons-ab12cd34",
        "main",
        H1,
        &[(10, H1), (12, H2)],
        "sibling tool fixes sharing the dispatch table",
    );
    let m = parse_mapping(&body).expect("candidate body must parse");
    assert_eq!(m.attempt, "cons-ab12cd34");
    assert_eq!(m.base, "main");
    assert_eq!(m.candidate_head, H1);
    assert_eq!(m.components, vec![(10, H1.to_string()), (12, H2.to_string())]);
}

#[test]
fn prose_and_foreign_markers_do_not_parse_as_mapping() {
    assert_eq!(parse_mapping("a normal PR body with no markers"), None);
    assert_eq!(
        parse_mapping("<!-- loom:sequence after=1 pred_head=x follower_head=y plan=p -->"),
        None
    );
    // A multi-line fake never closes on one line, so it never matches.
    assert_eq!(parse_mapping("<!-- loom:consolidation attempt=x\n base=y -->"), None);
}

#[test]
fn a_component_marker_missing_its_head_does_not_parse() {
    let body = "<!-- loom:consolidation attempt=a base=main candidate_head=x -->\n\
                <!-- loom:consolidation-component pr=10 -->\n";
    assert_eq!(parse_mapping(body), None);
}

#[test]
fn the_mapping_carries_the_rationale_for_the_independent_review() {
    let body = mapping_body("cons-a", "main", H1, &[(10, H1)], "dispatch-table siblings");
    assert!(body.contains("dispatch-table siblings"));
    assert!(
        body.contains("never approved this combination"),
        "the body must state that a green component is not an approval: {body}"
    );
}

// --- Construction (real git fixture) ------------------------------------

/// A scratch repo with a base commit and two branch heads, via real `git`.
fn fixture_repo(dir: &std::path::Path) -> (String, String, String) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git run");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("base.txt"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    // Branch A: own file.
    git(&["checkout", "-qb", "a"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "a"]);
    let a = git(&["rev-parse", "HEAD"]);
    // Branch B: own file, from base (disjoint from A).
    git(&["checkout", "-qb", "b", &base]);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "b"]);
    let b = git(&["rev-parse", "HEAD"]);
    (base, a, b)
}

#[test]
fn clean_construction_merges_pins_in_order_and_produces_a_head() {
    let tmp = std::env::temp_dir().join(format!("cons-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let (base, a, b) = fixture_repo(&tmp);
    let worktree = tmp.join("scratch");
    let head = construct("git", &tmp, &worktree, &base, &[(10, &a), (12, &b)])
        .expect("disjoint heads construct cleanly");
    // Both components' content present.
    assert!(worktree.join("a.txt").exists());
    assert!(worktree.join("b.txt").exists());
    assert!(!head.is_empty());
    remove_worktree("git", &tmp, &worktree);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn a_construction_conflict_aborts_naming_the_component() {
    let tmp = std::env::temp_dir().join(format!("conf-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(tmp.join("shared.txt"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    git(&["checkout", "-qb", "a"]);
    std::fs::write(tmp.join("shared.txt"), "from a\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "a"]);
    let a = git(&["rev-parse", "HEAD"]);
    git(&["checkout", "-qb", "b", &base]);
    std::fs::write(tmp.join("shared.txt"), "from b\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "b"]);
    let b = git(&["rev-parse", "HEAD"]);

    let worktree = tmp.join("scratch");
    let err = construct("git", &tmp, &worktree, &base, &[(10, &a), (12, &b)])
        .expect_err("overlapping edits must conflict");
    assert_eq!(err.number, 12, "the SECOND pin is the one that fails to merge");
    assert!(!worktree.exists(), "the scratch worktree is cleaned up on abort");
    let _ = std::fs::remove_dir_all(&tmp);
}

// --- Reservations -------------------------------------------------------

#[test]
fn reservations_are_sequencing_holds_pointing_at_the_candidate() {
    let source = comp(12, H2, &[]);
    let m = reservation_marker(99, H1, &source, "cons-ab12cd34");
    assert_eq!(m.after, 99, "the predecessor is the candidate PR");
    assert_eq!(m.pred_head, H1);
    assert_eq!(m.follower_head, H2);
    assert_eq!(m.plan, "cons-ab12cd34");
    assert_eq!(m.source.as_deref(), Some("pass"), "soft: expiry bounds abandoned attempts");
    // And the gate machinery accepts it: the parser reads back what we write.
    assert_eq!(
        crate::merge_pr::sequence::parse(&[crate::merge_pr::sequence::marker_text(&m)]),
        Some(m.clone())
    );
}

#[test]
fn reservation_presence_check_is_exact() {
    let source = comp(12, H2, &[]);
    let m = reservation_marker(99, H1, &source, "cons-ab12cd34");
    let with = vec![reservation_comment_body(&m, "cons-ab12cd34")];
    assert!(reservation_present(&with, &m));
    // A DIFFERENT pin (e.g. the candidate head moved) is not "present" —
    // convergence is on the exact marker, not on any reservation.
    let moved = SequenceMarker {
        pred_head: H2.to_string(),
        ..m.clone()
    };
    assert!(!reservation_present(&with, &moved));
}

#[test]
fn the_reservation_comment_tells_the_source_what_will_happen() {
    let source = comp(12, H2, &[]);
    let m = reservation_marker(99, H1, &source, "cons-ab12cd34");
    let body = reservation_comment_body(&m, "cons-ab12cd34");
    assert!(body.contains("#99"), "{body}");
    assert!(body.contains("never modified beyond the label"), "{body}");
    let release = reservation_release_body(&m, "cons-ab12cd34");
    assert!(release.contains("loom:sequence released"), "{release}");
    assert!(release.contains("aborted"), "{release}");
}

// --- Abort → re-consolidation (Judge finding on #9744) --------------------

fn reservation_on(source: &ComponentState, attempt: &str) -> SequenceMarker {
    reservation_marker(99, H1, source, attempt)
}

fn default_bounds() -> Bounds {
    Bounds {
        max_components: 4,
        max_diff_lines: 800,
    }
}

#[test]
fn the_release_marker_is_not_a_sequence_marker() {
    // It must never parse as a hold — and on its own it would never supersede
    // one, which is why eligibility needs `live_marker`.
    let source = comp(12, H2, &[]);
    let m = reservation_on(&source, "cons-ab12cd34");
    let release = reservation_release_body(&m, "cons-ab12cd34");
    assert_eq!(crate::merge_pr::sequence::parse(&[release]), None);
}

#[test]
fn a_live_reservation_is_seen() {
    let source = sequenced(comp(12, H2, &["shared.rs"]));
    let m = reservation_on(&source, "cons-ab12cd34");
    let bodies = vec![reservation_comment_body(&m, "cons-ab12cd34")];
    assert_eq!(live_marker(&source, &bodies), Some(m));
}

#[test]
fn an_aborted_reservation_is_not_live_once_the_label_is_gone() {
    // Abort removes the label and posts the release comment. Either signal
    // alone retires the marker; this is the label half.
    let source = comp(12, H2, &["shared.rs"]); // no loom:sequenced
    let m = reservation_on(&source, "cons-ab12cd34");
    let bodies = vec![reservation_comment_body(&m, "cons-ab12cd34")];
    assert_eq!(live_marker(&source, &bodies), None);
}

#[test]
fn a_newer_release_marker_retires_the_reservation_even_if_the_label_lingers() {
    let source = sequenced(comp(12, H2, &["shared.rs"]));
    let m = reservation_on(&source, "cons-ab12cd34");
    let bodies = vec![
        reservation_comment_body(&m, "cons-ab12cd34"),
        reservation_release_body(&m, "cons-ab12cd34"),
    ];
    assert_eq!(live_marker(&source, &bodies), None);
}

#[test]
fn a_release_for_a_different_attempt_does_not_retire_this_one() {
    let source = sequenced(comp(12, H2, &["shared.rs"]));
    let old = reservation_on(&source, "cons-00000000");
    let current = reservation_on(&source, "cons-ab12cd34");
    let bodies = vec![
        reservation_comment_body(&current, "cons-ab12cd34"),
        reservation_release_body(&old, "cons-00000000"),
    ];
    assert_eq!(live_marker(&source, &bodies), Some(current));
}

#[test]
fn a_reservation_made_after_a_release_is_live_again() {
    let source = sequenced(comp(12, H2, &["shared.rs"]));
    let first = reservation_on(&source, "cons-00000000");
    let second = reservation_on(&source, "cons-ab12cd34");
    let bodies = vec![
        reservation_comment_body(&first, "cons-00000000"),
        reservation_release_body(&first, "cons-00000000"),
        reservation_comment_body(&second, "cons-ab12cd34"),
    ];
    assert_eq!(live_marker(&source, &bodies), Some(second));
}

#[test]
fn a_source_from_an_aborted_attempt_is_eligible_again() {
    // The end-to-end shape of the Judge's finding: #1 was a source of an
    // aborted attempt. Its transcript still carries the reservation marker,
    // then the release comment; abort removed the label.
    let aborted = "cons-deadbeef";
    let one = comp(1, H1, &["shared.rs"]);
    let two = comp(2, H2, &["shared.rs"]);
    let reservation = reservation_on(&one, aborted);
    let bodies = vec![
        reservation_comment_body(&reservation, aborted),
        reservation_release_body(&reservation, aborted),
    ];

    let mut markers = clean_markers();
    if let Some(m) = live_marker(&one, &bodies) {
        markers.insert(1, m);
    }
    let failures = check_eligibility(
        &[one.clone(), two.clone()],
        &markers,
        "main",
        "reason",
        &default_bounds(),
    );
    assert!(failures.is_empty(), "an aborted attempt must not reserve forever: {failures:?}");

    // Even a caller that skips `live_marker` and hands over the raw newest
    // marker gets the right answer, because the label is gone.
    let mut raw = clean_markers();
    raw.insert(1, crate::merge_pr::sequence::parse(&bodies).expect("the old marker parses"));
    let failures = check_eligibility(&[one, two], &raw, "main", "reason", &default_bounds());
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn a_source_still_reserved_by_a_live_attempt_stays_ineligible() {
    let live = "cons-deadbeef";
    let one = sequenced(comp(1, H1, &["shared.rs"]));
    let two = comp(2, H2, &["shared.rs"]);
    let bodies = vec![reservation_comment_body(&reservation_on(&one, live), live)];
    let mut markers = clean_markers();
    if let Some(m) = live_marker(&one, &bodies) {
        markers.insert(1, m);
    }
    let failures = check_eligibility(&[one, two], &markers, "main", "reason", &default_bounds());
    assert!(
        failures.iter().any(|f| matches!(f,
            EligibilityFailure::AlreadyReserved { number: 1, attempt } if attempt == live)),
        "{failures:?}"
    );
}

// --- Push abort (ADR-0023 §3, operator ruling 2026-10-01) ----------------

const CAND: &str = "c333333333333333333333333333333333333333";
const MOVED: &str = "d444444444444444444444444444444444444444";
const ATTEMPT: &str = "cons-ab12cd34";

fn ledger() -> CandidateMapping {
    CandidateMapping {
        attempt: ATTEMPT.to_string(),
        base: "main".to_string(),
        candidate_head: CAND.to_string(),
        components: vec![(10, H1.to_string()), (12, H2.to_string())],
    }
}

/// A source carrying this attempt's live reservation against candidate #99.
fn reserved(number: u32, head: &str) -> (ComponentState, Vec<String>) {
    let c = sequenced(comp(number, head, &["shared.rs"]));
    let m = reservation_marker(99, CAND, &c, ATTEMPT);
    (c, vec![reservation_comment_body(&m, ATTEMPT)])
}

#[test]
fn the_ordering_pass_void_tombstone_retires_a_reservation() {
    // The pass's real VoidAndReplan note, not a copy: if its marker text
    // drifts, this test fails rather than the abort silently missing voids.
    let (source, mut bodies) = reserved(12, H2);
    bodies.push(crate::claim_reconciliation::merge_sequence::REPLAN_NOTE_BODY.to_string());
    assert_eq!(live_marker(&source, &bodies), None, "label re-added by hand does not revive it");
}

#[test]
fn the_ordering_pass_landing_release_retires_a_reservation() {
    use crate::claim_reconciliation::merge_sequence::{release_comment_body, HoldAction};
    let (source, mut bodies) = reserved(12, H2);
    let m = reservation_marker(99, CAND, &source, ATTEMPT);
    bodies.push(release_comment_body(&m, HoldAction::Release));
    assert_eq!(live_marker(&source, &bodies), None);
}

#[test]
fn reservation_state_tells_never_written_from_lost() {
    let (source, bodies) = reserved(12, H2);
    let m = reservation_marker(99, CAND, &source, ATTEMPT);
    assert_eq!(reservation_state(&source, &bodies, &m), ReservationState::Live);
    // Never written: backfill may apply it.
    assert_eq!(reservation_state(&source, &[], &m), ReservationState::Missing);
    // Written, then the label came off (void, expiry or a human): lost.
    let unlabeled = comp(12, H2, &["shared.rs"]);
    assert_eq!(reservation_state(&unlabeled, &bodies, &m), ReservationState::Lost);
    // Written, then superseded by a newer ordering hold: lost, not live.
    let mut superseded = bodies.clone();
    let newer = SequenceMarker {
        after: 10,
        pred_head: H1.to_string(),
        follower_head: H2.to_string(),
        plan: "seq-00000000".to_string(),
        source: Some("pass".to_string()),
    };
    superseded.push(crate::merge_pr::sequence::marker_text(&newer));
    assert_eq!(reservation_state(&source, &superseded, &m), ReservationState::Lost);
}

#[test]
fn a_live_attempt_passes_the_pin_check() {
    let sources = vec![reserved(10, H1), reserved(12, H2)];
    assert_eq!(pin_check(&ledger(), 99, CAND, &sources), None);
}

#[test]
fn a_push_to_the_candidate_aborts_the_attempt() {
    // A Doctor fix, a merge of main, a CI fix: all are pushes to the
    // candidate, and none is repaired in place.
    let sources = vec![reserved(10, H1), reserved(12, H2)];
    assert_eq!(
        pin_check(&ledger(), 99, MOVED, &sources),
        Some(AbortReason::CandidatePush {
            recorded: CAND.to_string(),
            live: MOVED.to_string(),
        })
    );
}

#[test]
fn a_push_to_any_source_aborts_the_attempt() {
    // Worked example 5/9: the source moved; its reservation pins the old
    // head. The reason names the push, not the void it causes.
    let (mut moved, mut bodies) = reserved(12, H2);
    moved.head_sha = Some(MOVED.to_string());
    moved.labels.clear();
    bodies.push(crate::claim_reconciliation::merge_sequence::REPLAN_NOTE_BODY.to_string());
    let sources = vec![reserved(10, H1), (moved, bodies)];
    assert_eq!(
        pin_check(&ledger(), 99, CAND, &sources),
        Some(AbortReason::SourcePush(vec![12]))
    );
}

#[test]
fn a_lost_reservation_aborts_the_attempt() {
    let (mut voided, bodies) = reserved(10, H1);
    voided.labels.clear();
    let sources = vec![(voided, bodies), reserved(12, H2)];
    assert_eq!(
        pin_check(&ledger(), 99, CAND, &sources),
        Some(AbortReason::ReservationLost(vec![10]))
    );
    // A reservation that never landed counts too once preparation has
    // applied them all: the check runs after the apply step.
    let sources = vec![
        (sequenced(comp(10, H1, &["shared.rs"])), vec![]),
        reserved(12, H2),
    ];
    assert_eq!(
        pin_check(&ledger(), 99, CAND, &sources),
        Some(AbortReason::ReservationLost(vec![10]))
    );
    // And an unreadable source is not assumed live.
    assert_eq!(
        pin_check(&ledger(), 99, CAND, &[reserved(12, H2)]),
        Some(AbortReason::ReservationLost(vec![10]))
    );
}

#[test]
fn the_abort_comment_records_the_adr_cause() {
    for (reason, cause) in [
        (AbortReason::Operator, "operator"),
        (AbortReason::CiFailure, "ci-failure"),
        (
            AbortReason::CandidatePush {
                recorded: CAND.to_string(),
                live: MOVED.to_string(),
            },
            "candidate-push",
        ),
        (AbortReason::SourcePush(vec![12]), "source-push"),
        (AbortReason::ReservationLost(vec![10]), "reservation-lost"),
    ] {
        assert_eq!(reason.cause(), cause);
        let body = abort_comment(ATTEMPT, &reason);
        assert!(
            body.starts_with(&format!("<!-- {ABORT_PREFIX} attempt={ATTEMPT} cause={cause} -->")),
            "{body}"
        );
        // The abort marker must never be read as a mapping or a hold.
        assert_eq!(parse_mapping(&body), None);
        assert_eq!(crate::merge_pr::sequence::parse(&[body]), None);
    }
}
