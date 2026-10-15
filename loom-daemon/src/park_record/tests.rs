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
    let got = blockers("<!-- loom:park Blocked by: example-org/tool-repo#202 by=human -->");
    assert_eq!(got, vec![q("example-org/tool-repo", 202)]);
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
    let park = render_park(
        &[q("example-org/tool-repo", 202), BlockerRef::local(5)],
        Some("human"),
        None,
        None,
    );
    assert!(park.contains("Blocked by: example-org/tool-repo#202"), "{park}");
    assert_eq!(park.lines().count(), 2);
    assert_eq!(blockers(&park), vec![BlockerRef::local(5), q("example-org/tool-repo", 202)]);
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

// --- #10556: re-park rewrite and the qualified-ref skip --------------------

#[test]
fn drop_blockers_removes_only_the_resolved_records() {
    let body = format!("Intro text.\n\n{}\n{}\n\nTrailer.\n", render(&rec(1)), render(&rec(3)));
    let out = drop_blockers(&body, &[1]);
    assert_eq!(blockers(&out), n(&[3]));
    assert_eq!(out, format!("Intro text.\n\n{}\n\nTrailer.\n", render(&rec(3))));
    // Nothing resolved: byte-for-byte unchanged.
    assert_eq!(drop_blockers(&body, &[99]), body);
}

#[test]
fn drop_blockers_rerenders_a_mixed_hand_written_marker() {
    let body = "x\n<!-- loom:park Blocked by: #1, #2 by=guide -->\ny\n";
    let out = drop_blockers(body, &[1]);
    assert_eq!(blockers(&out), n(&[2]));
    assert!(out.starts_with("x\n<!-- loom:park Blocked by: #2 by=guide -->"));
    assert!(out.ends_with("\ny\n"));
}

#[test]
fn drop_blockers_keeps_an_inline_marker_line() {
    let body = format!("See {} here.\n", render(&rec(1)));
    assert_eq!(drop_blockers(&body, &[1]), "See  here.\n");
}

#[test]
fn qualified_refs_are_detected_outside_the_reason_only() {
    assert!(has_qualified_ref("<!-- loom:park Blocked by: example-org/ui-repo#303 -->"));
    assert!(!has_qualified_ref(&render(&rec(5))));
    assert!(!has_qualified_ref(
        "<!-- loom:park Blocked by: #5 reason=\"after other/repo#9 lands\" -->"
    ));
    // Outside any marker: prose, not a park.
    assert!(!has_qualified_ref("Depends on other/repo#9"));
}

#[test]
fn drop_blockers_never_drops_a_qualified_record_by_its_number() {
    // A local `#1` resolving must not take `o/r#1` — another repo's artifact —
    // with it (#10443 x #10556).
    let body = "x\n<!-- loom:park Blocked by: #1, o/r#1 by=guide -->\ny\n";
    let out = drop_blockers(body, &[1]);
    assert_eq!(blockers(&out), vec![q("o/r", 1)]);
    assert!(has_qualified_ref(&out), "{out}");
}

/// #10837: a marker quoted in a fenced block or an inline code span renders as
/// visible text, so it is not a record — and a re-park never edits it.
#[test]
fn a_marker_quoted_in_code_is_not_a_record() {
    let quoted = "<!-- loom:park Blocked by: #1689 by=guide reason=\"x\" -->";
    for body in [
        format!("Report:\n\n```\n{quoted}\n```\n"),
        format!("~~~text\n{quoted}\n~~~\n"),
        format!("````md\n```\n{quoted}\n```\n````\n"),
        format!("Unclosed:\n```\n{quoted}\n"),
        format!("Inline: `{quoted}` and ``{quoted}``.\n"),
    ] {
        assert!(parse(&body).is_empty(), "{body}");
        assert!(!has_record(&body), "{body}");
        assert_eq!(drop_blockers(&body, &[1689]), body);
    }
    // Outside the code, the same body still declares its own records.
    let body = format!("```\n{quoted}\n```\n\n{}\n", render(&rec(7)));
    assert_eq!(blockers(&body), n(&[7]));
    assert_eq!(drop_blockers(&body, &[1689, 7]), format!("```\n{quoted}\n```\n\n"));
}

/// The code mask must not hide a real record: a backtick in its reason, a
/// code span beside it, or non-ASCII text before it on the line.
#[test]
fn a_real_record_beside_code_still_parses() {
    let body = "ünïcode `span` <!-- loom:park Blocked by: #5 reason=\"the ` flag\" --> `tail`\n";
    assert_eq!(blockers(body), n(&[5]));
    assert_eq!(parse(body)[0].reason.as_deref(), Some("the ` flag"));
    // An inline triple backtick is code, not a fence opener.
    let body = "Use ```x``` here.\n<!-- loom:park Blocked by: #6 -->\n";
    assert_eq!(blockers(body), n(&[6]));
}

/// #10837 review: Markdown code boundaries. A 4-space-indented triple backtick
/// is indented code, not a fence opener, so the real record after it parses.
#[test]
fn indented_backticks_do_not_open_a_fence() {
    let body = "Evidence:\n\n    ```\n\n<!-- loom:park Blocked by: #7 -->\n";
    assert_eq!(blockers(body), n(&[7]));
    let body = "Evidence:\n\n\t```\n\n<!-- loom:park Blocked by: #7 -->\n";
    assert_eq!(blockers(body), n(&[7]));
    // Up to 3 spaces still opens a fence.
    let body = "   ```\n<!-- loom:park Blocked by: #7 -->\n```\n";
    assert!(parse(body).is_empty());
}

/// A backslash-escaped backtick is literal text, so it cannot open a code span
/// that hides a genuine record after it.
#[test]
fn escaped_backtick_does_not_open_a_code_span() {
    let body = r#"\` <!-- loom:park Blocked by: #5 reason="the ` flag" -->"#;
    assert_eq!(blockers(body), n(&[5]));
    // An unescaped span still hides a quoted marker.
    let body = "`<!-- loom:park Blocked by: #5 -->`\n";
    assert!(parse(body).is_empty());
}

/// #10837 review: backslashes pair off. An odd run escapes the backtick (no
/// span, so the real record parses); an even run leaves it active (a span
/// that quotes the marker).
#[test]
fn backslash_parity_decides_whether_a_backtick_is_escaped() {
    let q = "<!-- loom:park Blocked by: #7 -->";
    // Two or four backslashes: the backtick is a live span opener, so the
    // marker is code.
    for body in [format!("\\\\`{q}`\n"), format!("\\\\\\\\`{q}`\n")] {
        assert!(parse(&body).is_empty(), "{body}");
        assert!(!has_record(&body), "{body}");
        assert_eq!(drop_blockers(&body, &[7]), body);
    }
    // One or three backslashes: escaped, so the marker is a real record.
    for body in [format!("\\`{q}`\n"), format!("\\\\\\`{q}`\n")] {
        assert_eq!(blockers(&body), n(&[7]), "{body}");
    }
}

/// #10837 review: a heading is not an open paragraph, so a 4-space-indented
/// marker directly under it is indented code; a real record after is a control.
#[test]
fn indented_marker_after_a_heading_is_code() {
    let q = "<!-- loom:park Blocked by: #1689 -->";
    for body in [
        format!("## Evidence\n    {q}\n"),
        format!("# Evidence #\n\t{q}\n"),
        format!("---\n    {q}\n"),
        format!("Title\n===\n    {q}\n"),
    ] {
        assert!(parse(&body).is_empty(), "{body}");
        assert!(!has_record(&body), "{body}");
        assert_eq!(drop_blockers(&body, &[1689]), body);
        let after = format!("{body}\n<!-- loom:park Blocked by: #7 -->\n");
        assert_eq!(blockers(&after), n(&[7]), "{after}");
    }
    // Plain paragraph text (or a `#tag` without a space) stays a paragraph, so
    // an indented line is a continuation and the marker is a real record.
    let body = format!("Evidence\n    {q}\n");
    assert_eq!(blockers(&body), n(&[1689]));
    let body = format!("#Evidence\n    {q}\n");
    assert_eq!(blockers(&body), n(&[1689]));
}

/// #10837 review: a short (`--`) setext underline closes the paragraph like a
/// long one, so an indented marker after it is code; a real record after a
/// blank line is the control. An underline-looking line with inner spaces, or
/// with no paragraph above it, does not change how the next line reads.
#[test]
fn indented_marker_after_a_short_setext_underline_is_code() {
    let q = "<!-- loom:park Blocked by: #1689 -->";
    for body in [
        format!("Evidence\n--\n    {q}\n"),
        format!("Evidence\n-\n\t{q}\n"),
        format!("Evidence\n----  \n    {q}\n"),
    ] {
        assert!(parse(&body).is_empty(), "{body}");
        assert!(!has_record(&body), "{body}");
        assert_eq!(drop_blockers(&body, &[1689]), body);
        let after = format!("{body}\n<!-- loom:park Blocked by: #7 -->\n");
        assert_eq!(blockers(&after), n(&[7]), "{after}");
    }
    // `- -` is not an underline (inner space), so the paragraph stays open and
    // the indented marker is a continuation: a real record.
    let body = format!("Evidence\n- -\n    {q}\n");
    assert_eq!(blockers(&body), n(&[1689]));
}

/// #10837 review: a fence opened directly inside a list item (bulleted,
/// numbered, nested) quotes its content, even across blank lines; a genuine
/// record after the list is still read.
#[test]
fn fences_inside_list_items_quote_their_markers() {
    let q = "<!-- loom:park Blocked by: #1689 -->";
    for body in [
        format!("- ```\n\n  {q}\n\n  ```\n"),
        format!("* ```\n  {q}\n  ```\n"),
        format!("1. ```\n\n   {q}\n\n   ```\n"),
        format!("1) ~~~text\n   {q}\n   ~~~\n"),
        format!("- outer\n  - ```\n\n    {q}\n\n    ```\n"),
        format!("- item\n\n  ```\n\n  {q}\n\n  ```\n"),
        // Unclosed: runs to the end of the item (the end of the body here).
        format!("- ```\n  {q}\n"),
    ] {
        assert!(parse(&body).is_empty(), "{body}");
        assert!(!has_record(&body), "{body}");
        assert_eq!(drop_blockers(&body, &[1689]), body);
        // A genuine record after the list is the control.
        let after = format!("{body}\n<!-- loom:park Blocked by: #7 -->\n");
        if !body.ends_with(&format!("{q}\n")) {
            assert_eq!(blockers(&after), n(&[7]), "{after}");
        }
    }
    // A line dedented past the item ends its fence, so the record is real.
    let body = "- ```\n  code\n<!-- loom:park Blocked by: #7 -->\n";
    assert_eq!(blockers(body), n(&[7]));
    // A marker on a list item's own line is still a record.
    let body = "- <!-- loom:park Blocked by: #7 -->\n";
    assert_eq!(blockers(body), n(&[7]));
}

/// Indented code, blockquotes (plain, indented, fenced, lazy), and multiline
/// inline code spans never declare a record; a real record after each does.
#[test]
fn markers_in_indented_quoted_or_multiline_code_are_not_records() {
    let q = "<!-- loom:park Blocked by: #1689 -->";
    for body in [
        format!("Evidence:\n\n    {q}\n"),
        format!("Evidence:\n\n\t{q}\n"),
        format!("> {q}\n"),
        format!("  > {q}\n"),
        format!("> > {q}\n"),
        format!("> ```\n> {q}\n> ```\n"),
        format!("Span `start\n{q}\nend` done\n"),
        format!("Span ``start\n{q}\nend`` done\n"),
    ] {
        assert!(parse(&body).is_empty(), "{body}");
        assert!(!has_record(&body), "{body}");
    }
    // A real record right after a quote or code still parses.
    for body in [
        "> quoted\n\n<!-- loom:park Blocked by: #7 -->\n",
        "> quoted\n<!-- loom:park Blocked by: #7 -->\n",
        "    code\n\n<!-- loom:park Blocked by: #7 -->\n",
        "Span `start\nend` done\n<!-- loom:park Blocked by: #7 -->\n",
    ] {
        assert_eq!(blockers(body), n(&[7]), "{body}");
    }
    // 4-space indent inside a paragraph is a continuation, not code.
    let body = "para\n    <!-- loom:park Blocked by: #7 -->\n";
    assert_eq!(blockers(body), n(&[7]));
}
