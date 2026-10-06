//! Tests for the park record (#8925).
//!
//! The load-bearing ones are
//! [`rendered_park_is_read_by_the_existing_rust_parser`] and
//! [`rendered_park_matches_the_shell_parse_dependencies_vocabulary`]: the whole
//! design claim is that a park record needs **no new parser**, and a claim like
//! that has to be executed, not asserted in a doc comment. The multi-blocker
//! case is asserted against both parsers on purpose — that disagreement (#4508's
//! two-stage grep captures every `#N`, `dep_recheck::extract`'s single regex
//! captures one) is precisely why a park renders one record per blocker.

use super::*;
use crate::dep_recheck::extract;

fn n(v: &[u64]) -> Vec<BlockerRef> {
    v.iter().map(|x| BlockerRef::local(*x)).collect()
}

fn rec(blocker: u64) -> ParkRecord {
    ParkRecord {
        blocker: Some(BlockerRef::local(blocker)),
        by: Some("doctor".to_string()),
        at: Some("2026-09-19T12:09:00Z".to_string()),
        reason: None,
    }
}

fn shell_stage1() -> Regex {
    // `guide.md`'s parse_dependencies, stage 1 (#4508).
    Regex::new(r"(Blocked by|Depends on|Requires|\- \[ \])[*_:[:space:]]*#[0-9]+")
        .expect("static test pattern")
}

#[test]
fn render_emits_the_documented_shape() {
    assert_eq!(
        render(&rec(8322)),
        "<!-- loom:park Blocked by: #8322 by=doctor at=2026-09-19T12:09:00Z -->"
    );
}

#[test]
fn a_park_with_two_blockers_renders_one_record_per_blocker() {
    let park = render_park(&n(&[8322, 8400]), Some("doctor"), None, None);
    assert_eq!(park.lines().count(), 2, "got {park}");
    assert_eq!(blockers(&park), n(&[8322, 8400]));
}

#[test]
fn render_park_with_no_blockers_declares_the_gap_rather_than_faking_a_reference() {
    let park = render_park(&[], Some("human"), None, Some("reason pending"));
    assert!(park.contains("Blocked by: (unstated)"), "got {park}");
    assert!(blockers(&park).is_empty());
    assert!(has_record(&park), "an unstated park is still a record");
}

#[test]
fn round_trips() {
    let original = ParkRecord {
        blocker: Some(BlockerRef::local(8322)),
        by: Some("champion".to_string()),
        at: Some("2026-09-25T16:52:03Z".to_string()),
        reason: Some("needs an architecture ruling".to_string()),
    };
    assert_eq!(parse(&render(&original)), vec![original]);
}

/// The compatibility claim, executed against the parser that actually runs.
///
/// `dep_recheck::extract` is what `check-stale-blocked`, `curator.md`'s premise
/// re-check and the operator-gated warning all use. A rendered park must be
/// readable by it verbatim — that is why [`RENDERED_PHRASE`] is `Blocked by:`
/// and not a new `blocked-by=` attribute.
#[test]
fn rendered_park_is_read_by_the_existing_rust_parser() {
    let body = format!(
        "Some PR description.\n\n{}\n",
        render_park(&n(&[8322, 8400]), Some("doctor"), None, None)
    );
    let input = extract::Input {
        body,
        comments: vec![],
    };
    assert_eq!(extract::extract(&input, extract::DEFAULT_BOT_LOGIN), "8322 8400");
}

/// The same claim against the shell side, whose stage-1 line selector must
/// select **every** rendered record's line.
#[test]
fn rendered_park_matches_the_shell_parse_dependencies_vocabulary() {
    let park = render_park(&n(&[8322, 8400]), None, None, None);
    let stage1 = shell_stage1();
    for line in park.lines() {
        assert!(stage1.is_match(line), "stage 1 did not select: {line}");
    }
}

/// Why one-record-per-blocker exists, stated as a test so the constraint cannot
/// be "simplified" back into a comma list by a later reader who only checks the
/// shell parser.
#[test]
fn a_comma_separated_record_would_lose_every_blocker_but_the_first() {
    let hand_written = "<!-- loom:park Blocked by: #8322, #8400 -->";
    // This module's own parser is tolerant and sees both …
    assert_eq!(blockers(hand_written), n(&[8322, 8400]));
    // … but the parser the fleet's checks run sees only the first, which is the
    // whole reason `render_park` never emits this shape.
    let input = extract::Input {
        body: hand_written.to_string(),
        comments: vec![],
    };
    assert_eq!(extract::extract(&input, extract::DEFAULT_BOT_LOGIN), "8322");
}

/// The #8925 failure mode in reverse: a reason mentioning an issue number must
/// not become a declared blocker. A park that waits on a mention never clears.
#[test]
fn a_reference_inside_the_reason_is_not_a_declared_blocker() {
    let record = ParkRecord {
        blocker: Some(BlockerRef::local(8322)),
        by: Some("doctor".to_string()),
        at: None,
        reason: Some("compounded by the #8940 dispatch bug".to_string()),
    };
    let line = render(&record);
    assert_eq!(blockers(&line), n(&[8322]));
    let parsed = parse(&line);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].reason.as_deref(), Some("compounded by the #8940 dispatch bug"));
}

#[test]
fn a_reason_containing_a_double_dash_cannot_terminate_the_comment_early() {
    let line = render(&ParkRecord {
        blocker: Some(BlockerRef::local(7)),
        by: None,
        at: None,
        reason: Some("blocked on --force being removed".to_string()),
    });
    assert_eq!(line.matches("-->").count(), 1, "got {line}");
    assert_eq!(blockers(&line), n(&[7]));
}

#[test]
fn a_multiline_reason_is_flattened_to_one_line() {
    let line = render(&ParkRecord {
        blocker: Some(BlockerRef::local(7)),
        by: None,
        at: None,
        reason: Some("first line\nsecond line".to_string()),
    });
    assert_eq!(line.lines().count(), 1, "got {line}");
    assert_eq!(parse(&line)[0].reason.as_deref(), Some("first line second line"));
}

#[test]
fn an_attribute_value_with_spaces_cannot_truncate_the_next_attribute() {
    let parsed = &parse(&render(&ParkRecord {
        blocker: Some(BlockerRef::local(7)),
        by: Some("champion merge risk".to_string()),
        at: Some("2026-09-25T16:52:03Z".to_string()),
        reason: None,
    }))[0];
    assert_eq!(parsed.by.as_deref(), Some("champion_merge_risk"));
    assert_eq!(parsed.at.as_deref(), Some("2026-09-25T16:52:03Z"));
}

#[test]
fn prose_that_merely_mentions_a_blocker_is_not_a_record() {
    let body = "Blocked by #8322 - filed it to track the ratchet decision.";
    assert!(!has_record(body));
    assert!(blockers(body).is_empty());
    // …but the legacy prose parser still sees it, which is exactly the state
    // #8925 is about: readable as a dependency, not attributable as a park.
    let input = extract::Input {
        body: body.to_string(),
        comments: vec![],
    };
    assert_eq!(extract::extract(&input, extract::DEFAULT_BOT_LOGIN), "8322");
}

#[test]
fn a_truncated_marker_is_not_a_record() {
    assert!(!has_record("<!-- loom:park Blocked by: #8322"));
}

#[test]
fn a_lease_record_is_not_a_park_record() {
    // The sibling marker family must not cross-parse (`lease-record.md`).
    let lease = "<!-- loom:lease host=host-34209a6e sweep=sweep-issue-8925-1790435534 -->";
    assert!(!has_record(lease));
}

#[test]
fn two_parks_applied_at_different_times_both_survive() {
    let body = format!(
        "{}\n{}\n",
        render(&rec(8322)),
        render(&ParkRecord {
            blocker: Some(BlockerRef::local(8400)),
            by: Some("curator".to_string()),
            at: None,
            reason: None,
        })
    );
    let parsed = parse(&body);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].by.as_deref(), Some("doctor"));
    assert_eq!(parsed[1].by.as_deref(), Some("curator"));
    assert_eq!(blockers(&body), n(&[8322, 8400]));
}

#[test]
fn blockers_are_deduplicated_and_ordered() {
    assert_eq!(blockers("<!-- loom:park Blocked by: #9, #8322, #9 -->"), n(&[9, 8322]));
}

/// A record written by hand (an operator editing the body) rather than by
/// [`render`] must still parse — the grammar is the contract, not the renderer.
#[test]
fn a_hand_written_record_parses() {
    let parsed = parse("<!--  loom:park   Blocked by:#8322 by=human  -->");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].blocker, Some(BlockerRef::local(8322)));
    assert_eq!(parsed[0].by.as_deref(), Some("human"));
}

#[test]
fn render_park_deduplicates_its_input() {
    let park = render_park(&n(&[8322, 8322]), None, None, None);
    assert_eq!(park.lines().count(), 1, "got {park}");
}

fn q(repo: &str, number: u64) -> BlockerRef {
    BlockerRef {
        repo: Some(repo.to_string()),
        number,
    }
}

#[test]
fn a_qualified_blocker_keeps_its_repo() {
    let got = blockers("<!-- loom:park Blocked by: 2AMLogic/2am#1088 by=human -->");
    assert_eq!(got, vec![q("2AMLogic/2am", 1088)]);
}

#[test]
fn the_same_number_in_two_repos_is_two_records() {
    let body = "<!-- loom:park Blocked by: #9, o/r#9, o/r#9 -->";
    let recs = parse(body);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].blocker, Some(BlockerRef::local(9)));
    assert_eq!(recs[1].blocker, Some(q("o/r", 9)));
}

#[test]
fn a_qualified_ref_inside_the_reason_is_ignored() {
    let body = "<!-- loom:park Blocked by: #5 reason=\"see o/r#77\" -->";
    assert_eq!(blockers(body), n(&[5]));
}

#[test]
fn qualified_records_round_trip() {
    let park =
        render_park(&[q("2AMLogic/2am", 1088), BlockerRef::local(5)], Some("human"), None, None);
    assert!(park.contains("Blocked by: 2AMLogic/2am#1088"), "{park}");
    assert_eq!(park.lines().count(), 2);
    assert_eq!(blockers(&park), vec![BlockerRef::local(5), q("2AMLogic/2am", 1088)]);
}

#[test]
fn blocker_ref_from_str_shapes() {
    assert_eq!("7".parse::<BlockerRef>().unwrap(), BlockerRef::local(7));
    assert_eq!("#7".parse::<BlockerRef>().unwrap(), BlockerRef::local(7));
    assert_eq!("a/b#7".parse::<BlockerRef>().unwrap(), q("a/b", 7));
    for bad in ["", "a/b", "a#7", "a/b/c#7", "x/y#", "#x", "a b/c#1"] {
        assert!(bad.parse::<BlockerRef>().is_err(), "{bad}");
    }
}

#[test]
fn mask_qualified_removes_only_qualified_refs_inside_markers() {
    let t = "see o/r#4 and #3\n<!-- loom:park Blocked by: o/r#4, #3 -->";
    let m = mask_qualified(t);
    assert!(m.starts_with("see o/r#4 and #3"), "{m}");
    assert!(m.contains("Blocked by: , #3"), "{m}");
}
