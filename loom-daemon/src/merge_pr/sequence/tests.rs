//! Tests for the merge-sequencing marker parser and release evaluation.

use super::*;

// --- Marker parsing -----------------------------------------------------

fn marker_line(after: u32, pred: &str, follower: &str, plan: &str) -> String {
    format!("<!-- loom:sequence after={after} pred_head={pred} follower_head={follower} plan={plan} -->")
}

const PRED: &str = "a1b2c3d4e5a1b2c3d4e5a1b2c3d4e5a1b2c3d4e5";
const FOLLOWER: &str = "f0e1d2c3b4af0e1d2c3b4af0e1d2c3b4af0e1d2c";
const OTHER: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn a_well_formed_marker_parses() {
    let bodies = vec![marker_line(111, PRED, FOLLOWER, "plan-a")];
    assert_eq!(
        parse(&bodies),
        Some(SequenceMarker {
            after: 111,
            pred_head: PRED.into(),
            follower_head: FOLLOWER.into(),
            plan: "plan-a".into(),
            source: None,
        })
    );
}

#[test]
fn prose_and_documentation_mentions_of_the_format_never_match() {
    // The hold_state lesson: documentation lines, backticked examples and
    // prose all travel through comment streams. A span fails validation
    // (placeholder values here) and is therefore not a marker at all.
    let bodies = vec![
        "The applier writes `<!-- loom:sequence after=1 pred_head=<40-hex> follower_head=<40-hex> plan=<id> -->` on the follower.".to_string(),
        "Example (multi-line, so it never closes on one line):\n<!-- loom:sequence after=2\n   pred_head=aa3>\n".to_string(),
        "after=3 pred_head=... follower_head=... plan=... — a bare field list in prose".to_string(),
    ];
    assert_eq!(parse(&bodies), None);
}

#[test]
fn the_newest_marker_wins() {
    let old = marker_line(1, PRED, FOLLOWER, "plan-old");
    let new = marker_line(2, OTHER, FOLLOWER, "plan-new");
    match parse(&[old, new]) {
        Some(m) => {
            assert_eq!(m.after, 2);
            assert_eq!(m.plan, "plan-new");
        }
        None => panic!("expected a marker"),
    }
}

#[test]
fn a_malformed_newer_span_does_not_shadow_an_older_valid_marker() {
    // A truncated rewrite must not UN-own the hold: the older marker is
    // still a valid, head-pinned statement of order, and evaluate() re-checks
    // its pins against live state, so the worst case is a hold that stands.
    let old = marker_line(1, PRED, FOLLOWER, "plan-old");
    let truncated_new =
        "<!-- loom:sequence after=2 pred_head=abc follower_head=def plan=new -->".to_string();
    match parse(&[old, truncated_new]) {
        Some(m) => assert_eq!(m.plan, "plan-old"),
        None => panic!("the valid older marker must survive"),
    }
}

#[test]
fn short_or_uppercase_or_placeholder_shas_are_not_markers() {
    for bad in ["abc123", &PRED[..39], &PRED.to_uppercase(), "<40-hex>"] {
        let bodies = vec![marker_line(1, bad, FOLLOWER, "p")];
        assert_eq!(parse(&bodies), None, "pred_head {bad:?} must not parse");
        let bodies = vec![marker_line(1, PRED, bad, "p")];
        assert_eq!(parse(&bodies), None, "follower_head {bad:?} must not parse");
    }
}

#[test]
fn after_zero_or_nonnumeric_is_not_a_marker() {
    for bad in ["0", "-1", "abc", "1.5"] {
        let line = format!(
            "<!-- loom:sequence after={bad} pred_head={PRED} follower_head={FOLLOWER} plan=p -->"
        );
        assert_eq!(parse(&[line]), None, "after={bad:?}");
    }
}

#[test]
fn missing_fields_are_not_markers() {
    for line in [
        "<!-- loom:sequence after=1 pred_head={PRED} plan=p -->",
        "<!-- loom:sequence after=1 follower_head={FOLLOWER} plan=p -->",
        "<!-- loom:sequence after=1 pred_head={PRED} follower_head={FOLLOWER} -->",
    ] {
        let line = line.replace("{PRED}", PRED).replace("{FOLLOWER}", FOLLOWER);
        assert_eq!(parse(std::slice::from_ref(&line)), None, "{line}");
    }
}

#[test]
fn unknown_fields_make_the_span_not_a_marker() {
    let line = format!(
        "<!-- loom:sequence after=1 pred_head={PRED} follower_head={FOLLOWER} plan=p surprise=1 -->"
    );
    assert_eq!(parse(&[line]), None);
}

#[test]
fn a_different_marker_namespace_is_not_ours() {
    // `loom:sequence-x` must be ignored outright, not parsed as a malformed
    // sibling: other features may share the prefix without owning this gate.
    let bodies = ["<!-- loom:sequence-extra after=1 -->".to_string()];
    assert_eq!(parse(&bodies), None);
}

#[test]
fn an_empty_comment_stream_has_no_marker() {
    assert_eq!(parse(&[]), None);
    assert_eq!(parse(&[String::new()]), None);
    assert_eq!(parse(&["<!-- something else -->".to_string()]), None);
}

// --- Render / round-trip (#9686's source field) -------------------------

#[test]
fn marker_text_round_trips_through_parse() {
    let m = SequenceMarker {
        after: 7,
        pred_head: PRED.into(),
        follower_head: FOLLOWER.into(),
        plan: "seq-ab12cd34".into(),
        source: Some("pass".into()),
    };
    assert_eq!(parse(&[marker_text(&m)]), Some(m.clone()));
    let hard = SequenceMarker { source: None, ..m };
    assert_eq!(parse(&[marker_text(&hard)]), Some(hard));
}

#[test]
fn a_source_pass_marker_is_soft_and_its_absence_is_hard() {
    // A marker with no source field parses as source: None — the HARD,
    // human-authored shape that never auto-expires.
    let bare = parse(&[marker_line(1, PRED, FOLLOWER, "seq-abc")]);
    match bare {
        Some(m) => assert_eq!(m.source, None, "missing source = hard hold"),
        None => panic!("a plain marker must parse"),
    }
    let line = format!(
        "<!-- loom:sequence after=1 pred_head={PRED} follower_head={FOLLOWER} plan=seq-abc source=pass -->"
    );
    match parse(&[line]) {
        Some(m) => assert_eq!(m.source.as_deref(), Some("pass")),
        None => panic!("source=pass marker must parse"),
    }
}

// --- Evaluation ---------------------------------------------------------

fn marker() -> SequenceMarker {
    SequenceMarker {
        after: 111,
        pred_head: PRED.into(),
        follower_head: FOLLOWER.into(),
        plan: "plan-a".into(),
        source: None,
    }
}

fn pred(open: bool, merged: bool, head: Option<&str>) -> PredecessorState {
    PredecessorState {
        open,
        merged,
        head_sha: head.map(str::to_string),
        updated_at: None,
    }
}

#[test]
fn merged_at_the_recorded_head_clears() {
    assert_eq!(evaluate(&marker(), &pred(false, true, Some(PRED)), FOLLOWER), Verdict::Clear);
}

#[test]
fn an_open_predecessor_at_the_recorded_head_keeps_in_flight() {
    assert_eq!(
        evaluate(&marker(), &pred(true, false, Some(PRED)), FOLLOWER),
        Verdict::Keep(KeepReason::InFlight)
    );
}

#[test]
fn a_moved_predecessor_head_requires_a_replan_not_a_release() {
    assert_eq!(
        evaluate(&marker(), &pred(true, false, Some(OTHER)), FOLLOWER),
        Verdict::Keep(KeepReason::PredecessorMoved)
    );
}

#[test]
fn a_closed_unmerged_predecessor_dissolves_the_condition() {
    assert_eq!(
        evaluate(&marker(), &pred(false, false, Some(PRED)), FOLLOWER),
        Verdict::Dissolved
    );
}

#[test]
fn a_moved_follower_head_is_a_replan_before_anything_else() {
    // Checked FIRST on purpose: a marker that no longer describes this tree
    // must not be honored even if the predecessor merged in the meantime.
    assert_eq!(
        evaluate(&marker(), &pred(false, true, Some(PRED)), OTHER),
        Verdict::Keep(KeepReason::FollowerMoved)
    );
}

#[test]
fn a_merge_at_an_unrecorded_head_never_releases() {
    // Merged, but the forge reports a head that is not the recorded one —
    // landed from a different tree, or pushed after the merge. Unknown fate
    // for the recorded tree means the hold stands for a human/pass look.
    assert_eq!(
        evaluate(&marker(), &pred(false, true, Some(OTHER)), FOLLOWER),
        Verdict::Keep(KeepReason::MergedAtUnknownHead)
    );
    assert_eq!(
        evaluate(&marker(), &pred(false, true, None), FOLLOWER),
        Verdict::Keep(KeepReason::MergedAtUnknownHead)
    );
}

#[test]
fn an_open_predecessor_with_withheld_head_keeps_closed() {
    // `head_sha: None` on an open PR is a failed/withheld read, which must
    // never compare equal to the recorded pin.
    assert_eq!(
        evaluate(&marker(), &pred(true, false, None), FOLLOWER),
        Verdict::Keep(KeepReason::PredecessorMoved)
    );
}

#[test]
fn sha_comparison_is_exact_not_case_insensitive() {
    // Different case is a different string. There is no forge where that
    // arises legitimately, but the gate must not silently treat it as a
    // match either — same rule redate's decide() pins.
    assert_eq!(
        evaluate(&marker(), &pred(false, true, Some(&PRED.to_uppercase())), FOLLOWER),
        Verdict::Keep(KeepReason::MergedAtUnknownHead)
    );
}

// --- Rendering ----------------------------------------------------------

#[test]
fn every_verdict_renders_its_sentinel_and_reason() {
    let m = marker();
    for (verdict, sentinel, needle) in [
        (Verdict::Clear, CLEAR, "#111"),
        (Verdict::Dissolved, DISSOLVED, "#111"),
        (Verdict::Keep(KeepReason::InFlight), KEEP, "#111"),
        (Verdict::Keep(KeepReason::PredecessorMoved), REPLAN, "#111"),
        // The follower-side REPLAN names the plan, not the predecessor: the
        // problem is on this PR's side of the ordering.
        (Verdict::Keep(KeepReason::FollowerMoved), REPLAN, "plan-a"),
        (Verdict::Keep(KeepReason::MergedAtUnknownHead), REPLAN, "#111"),
    ] {
        let line = verdict_line(verdict, &m);
        assert!(line.starts_with(sentinel), "{verdict:?} → {line}");
        assert!(line.contains(needle), "{verdict:?} → {line}");
    }
}

#[test]
fn the_sentinels_are_distinct_positive_signals() {
    // A caller releases only on CLEAR/DISSOLVED; if any two sentinels ever
    // collided, one release reason would impersonate another.
    let all = [CLEAR, DISSOLVED, KEEP, REPLAN, NONE];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

// --- Live-hold parsing (release / replan tombstones, #9745 review) --------

#[test]
fn a_release_tombstone_ends_the_hold_it_names() {
    let bodies = vec![
        marker_line(111, PRED, FOLLOWER, "cons-a"),
        format!("{}\nreleased — prose", release_marker_text("cons-a")),
    ];
    // `parse` still reports what the newest marker said (the gate reads the
    // label, not this) …
    assert_eq!(parse(&bodies).map(|m| m.plan), Some("cons-a".to_string()));
    // … but the history-only question "is this hold still in force?" is no.
    assert_eq!(parse_live(&bodies), None);
}

#[test]
fn a_release_of_another_plan_does_not_void_a_newer_hold() {
    let bodies = vec![
        marker_line(111, PRED, FOLLOWER, "seq-old"),
        marker_line(222, OTHER, FOLLOWER, "cons-new"),
        release_marker_text("seq-old"),
    ];
    assert_eq!(parse_live(&bodies).map(|m| m.plan), Some("cons-new".to_string()));
}

#[test]
fn a_hold_re_applied_after_its_release_is_live_again() {
    // Newest wins in both directions: an abort then a fresh reservation under
    // the same deterministic attempt id is a live reservation.
    let bodies = vec![
        marker_line(111, PRED, FOLLOWER, "cons-a"),
        release_marker_text("cons-a"),
        marker_line(333, OTHER, FOLLOWER, "cons-a"),
    ];
    assert_eq!(parse_live(&bodies).map(|m| m.after), Some(333));
}

#[test]
fn a_replan_void_ends_any_hold() {
    let bodies = vec![
        marker_line(111, PRED, FOLLOWER, "seq-a"),
        "<!-- loom:sequence replanned -->".to_string(),
    ];
    assert_eq!(parse_live(&bodies), None);
}

#[test]
fn tombstone_look_alikes_are_not_tombstones() {
    for fake in [
        "<!-- loom:sequence released -->",
        "<!-- loom:sequence released plan= -->",
        "<!-- loom:sequence released plan=cons-a extra=1 -->",
        "<!-- loom:sequence-x released plan=cons-a -->",
        "`<!-- loom:sequence released plan=cons-a` -->",
    ] {
        let bodies = vec![marker_line(111, PRED, FOLLOWER, "cons-a"), fake.to_string()];
        assert!(parse_live(&bodies).is_some(), "{fake:?} must not release the hold");
    }
}

#[test]
fn every_hold_releasing_writer_emits_a_tombstone_parse_live_reads() {
    // Drift guard: the pass (#9686) and both consolidation verbs render their
    // own release bodies; each must end the hold under `parse_live`.
    use crate::claim_reconciliation::merge_sequence::{
        release_comment_body, HoldAction, REPLAN_NOTE_BODY,
    };
    use crate::merge_pr::consolidate::{landing_release_body, reservation_release_body};
    let m = SequenceMarker {
        after: 111,
        pred_head: PRED.into(),
        follower_head: FOLLOWER.into(),
        plan: "cons-ab12cd34".into(),
        source: Some("pass".into()),
    };
    let writers = [
        release_comment_body(&m, HoldAction::Release),
        release_comment_body(&m, HoldAction::ReleaseDissolved),
        release_comment_body(&m, HoldAction::Expire),
        REPLAN_NOTE_BODY.to_string(),
        reservation_release_body(&m, "cons-ab12cd34"),
        landing_release_body(&m, "cons-ab12cd34"),
    ];
    for release in writers {
        let bodies = vec![marker_text(&m), release.clone()];
        assert_eq!(parse_live(&bodies), None, "not recognized as a release:\n{release}");
    }
}
