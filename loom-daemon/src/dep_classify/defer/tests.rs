//! Tests for the `--check-defer` decision (epic #7810, PR 3).
//!
//! The ordering of the checks is the substance here — each early return is a
//! different answer with a different exit code, and Champion branches on both.

use super::*;

const REPO: &str = "o/r";
const SELF: &str = "o/r#5";

fn inputs(source_body: &str, body: &str) -> Inputs {
    Inputs {
        body: body.to_string(),
        source_body: source_body.to_string(),
        refs: ClassifiedRefs::default(),
        has_cycle: false,
    }
}

fn with_open(mut i: Inputs, open: &[&str]) -> Inputs {
    i.refs.open = open.iter().map(|s| (*s).to_string()).collect();
    i
}

#[test]
fn no_verdict_text_means_no_findings() {
    assert_eq!(
        decide(&inputs("", ""), REPO, SELF),
        Decision::NoDefer {
            reason: "no-findings"
        }
    );
    assert_eq!(
        decide(&inputs("   \n  ", ""), REPO, SELF),
        Decision::NoDefer {
            reason: "no-findings"
        }
    );
}

#[test]
fn verdict_prose_with_no_bullets_means_no_findings() {
    let d = decide(&inputs("Just prose, no list at all.\n", ""), REPO, SELF);
    assert_eq!(
        d,
        Decision::NoDefer {
            reason: "no-findings"
        }
    );
}

#[test]
fn a_merits_finding_blocks_the_defer() {
    // The most consequential outcome: deferring here would park work a human
    // rejected on substance, and waiting will never resolve it.
    let d = decide(&inputs("- The approach is wrong on the merits\n", ""), REPO, SELF);
    assert_eq!(
        d,
        Decision::NoDefer {
            reason: "merits-finding"
        }
    );
}

#[test]
fn a_mixed_set_counts_as_merits() {
    let d = decide(&inputs("- Blocked by #3\n- Scope is wrong\n", ""), REPO, SELF);
    assert_eq!(
        d,
        Decision::NoDefer {
            reason: "merits-finding"
        }
    );
}

#[test]
fn dependency_findings_with_no_reference_anywhere_means_no_recorded_blocker() {
    // `is_dependency_finding` needs a reference, so reaching this state
    // requires findings that qualify while resolve_blockers finds nothing —
    // e.g. the only reference is the issue itself.
    let d = decide(&inputs("- Blocked by #5\n", ""), REPO, SELF);
    assert_eq!(
        d,
        Decision::NoDefer {
            reason: "no-recorded-blocker"
        },
        "an issue listed as its own blocker is a typo, not a blocker"
    );
}

#[test]
fn all_blockers_closed_means_reevaluate() {
    let mut i = inputs("- Blocked by #3\n", "");
    i.refs.resolved = vec!["o/r#3".into()];
    match decide(&i, REPO, SELF) {
        Decision::Reevaluate { cleared } => assert_eq!(cleared, vec!["o/r#3"]),
        other => panic!("expected Reevaluate, got {other:?}"),
    }
}

#[test]
fn a_cycle_blocks_the_defer_but_only_while_blockers_are_open() {
    let mut i = with_open(inputs("- Blocked by #3\n", ""), &["o/r#3"]);
    i.has_cycle = true;
    assert_eq!(
        decide(&i, REPO, SELF),
        Decision::NoDefer {
            reason: "dependency-cycle"
        },
        "waiting on a cycle is futile"
    );

    // Order matters: a cycle whose members have since closed is not live, and
    // "all clear" must win. Reporting a cycle there would be misleading.
    let mut cleared = inputs("- Blocked by #3\n", "");
    cleared.refs.resolved = vec!["o/r#3".into()];
    cleared.has_cycle = true;
    assert!(
        matches!(decide(&cleared, REPO, SELF), Decision::Reevaluate { .. }),
        "blockers-cleared must be checked before dependency-cycle"
    );
}

#[test]
fn a_startable_subset_is_promoted_instead_of_parking_everything() {
    let body = "Some description.\n\n## Startable subset\n- the independent half\n";
    let i = with_open(inputs("- Blocked by #3\n", body), &["o/r#3"]);
    match decide(&i, REPO, SELF) {
        Decision::PromoteSubset { open, subset } => {
            assert_eq!(open, vec!["o/r#3"]);
            assert!(subset.contains("the independent half"), "{subset:?}");
        }
        other => panic!("expected PromoteSubset, got {other:?}"),
    }
}

#[test]
fn open_blockers_with_nothing_startable_defer() {
    let i = with_open(inputs("- Blocked by #3\n", "No subset here."), &["o/r#3"]);
    match decide(&i, REPO, SELF) {
        Decision::Defer {
            open,
            blocker_fingerprint,
        } => {
            assert_eq!(open, vec!["o/r#3"]);
            assert_eq!(blocker_fingerprint.len(), 16, "got {blocker_fingerprint:?}");
        }
        other => panic!("expected Defer, got {other:?}"),
    }
}

#[test]
fn the_fingerprint_keys_on_the_open_set_only() {
    let a = with_open(inputs("- Blocked by #3\n", ""), &["o/r#3"]);
    let mut b = with_open(inputs("- Blocked by #3\n", ""), &["o/r#3"]);
    b.refs.resolved = vec!["o/r#99".into()]; // resolved ones must not change it

    let (
        Decision::Defer {
            blocker_fingerprint: fa,
            ..
        },
        Decision::Defer {
            blocker_fingerprint: fb,
            ..
        },
    ) = (decide(&a, REPO, SELF), decide(&b, REPO, SELF))
    else {
        panic!("both should defer");
    };
    assert_eq!(fa, fb, "a resolved blocker must not change the open-set fingerprint");
}

#[test]
fn findings_references_win_over_the_body() {
    // A verdict names the blocker it actually objected to; the body may declare
    // more. The finding is the more precise signal.
    let got = resolve_blockers("- Blocked by #3\n", "Blocked by #77\nBlocked by #88\n", REPO, SELF);
    assert_eq!(got, vec!["o/r#3"]);
}

#[test]
fn an_epic_parenthetical_does_not_wedge_a_phase_child_in_defer() {
    // kicad-tools#5520 (#8251): the verdict names the real blocker and then
    // annotates, for a human reader, which epic phase it belongs to. Capturing
    // the epic as a second blocker parked the phase child forever — an epic
    // stays open for its whole phase lifecycle by design.
    let bullet = "- Technical Feasibility: Blocked by #5519 (Epic #5510 Phase 1a — \
                  `RoutingPlan`, sidecar writer, `emit_routing_plan`), still open.\n";
    let mut i = inputs(bullet, "");

    let to_classify = blockers_to_classify(&i, REPO, SELF);
    assert_eq!(
        to_classify,
        vec!["o/r#5519"],
        "the epic mention is annotation, so the forge is never asked about it"
    );

    // Replay what the I/O boundary does with that list: the real blocker has
    // closed, the epic (had it been asked about) is still open.
    for r in to_classify {
        if r == "o/r#5510" {
            i.refs.open.push(r);
        } else {
            i.refs.resolved.push(r);
        }
    }
    match decide(&i, REPO, SELF) {
        Decision::Reevaluate { cleared } => assert_eq!(cleared, vec!["o/r#5519"]),
        other => panic!("expected Reevaluate once the real blocker closed, got {other:?}"),
    }
}

#[test]
fn the_body_is_the_fallback_when_findings_name_nothing() {
    // Older verdicts did not always cite a reference.
    let got = resolve_blockers("- something vague\n", "Blocked by #77\n", REPO, SELF);
    assert_eq!(got, vec!["o/r#77"]);
}

#[test]
fn the_self_reference_is_dropped_from_both_sources() {
    assert!(resolve_blockers("- Blocked by #5\n", "", REPO, SELF).is_empty());
    assert!(resolve_blockers("", "Blocked by #5\n", REPO, SELF).is_empty());
}

// ---------------------------------------------------------------------------
// Rendering: stdout markers and exit codes are contract
// ---------------------------------------------------------------------------

#[test]
fn each_decision_renders_its_marker_and_exit_code() {
    let cases: &[(Decision, &str, i32)] = &[
        (
            Decision::NoDefer {
                reason: "merits-finding",
            },
            "NO_DEFER\nREASON: merits-finding\n",
            1,
        ),
        (
            Decision::Reevaluate {
                cleared: vec!["o/r#3".into()],
            },
            "REEVALUATE\nREASON: blockers-cleared\nCLEARED_BLOCKERS: o/r#3\n",
            3,
        ),
        (
            Decision::PromoteSubset {
                open: vec!["o/r#3".into()],
                subset: "- work\n".into(),
            },
            "PROMOTE_SUBSET\nOPEN_BLOCKERS: o/r#3\nSTARTABLE_SUBSET:\n- work\n",
            4,
        ),
        (
            Decision::Defer {
                open: vec!["o/r#3".into()],
                blocker_fingerprint: "abc123".into(),
            },
            "DEFER\nOPEN_BLOCKERS: o/r#3\nBLOCKER_FINGERPRINT: abc123\n",
            0,
        ),
    ];
    for (decision, want_out, want_code) in cases {
        let (out, code) = render(decision, &[]);
        assert_eq!(&out, want_out, "for {decision:?}");
        assert_eq!(code, *want_code, "for {decision:?}");
    }
}

#[test]
fn reevaluate_omits_cleared_blockers_when_there_are_none() {
    let (out, code) = render(&Decision::Reevaluate { cleared: vec![] }, &[]);
    assert_eq!(out, "REEVALUATE\nREASON: blockers-cleared\n");
    assert_eq!(code, 3);
}

#[test]
fn unreadable_references_are_reported_before_the_verdict() {
    // An operator must see that the answer was formed with incomplete
    // information, and must see it first.
    let (out, _) = render(
        &Decision::Defer {
            open: vec!["o/r#3".into()],
            blocker_fingerprint: "abc".into(),
        },
        &["o/r#9".to_string()],
    );
    assert!(out.starts_with("UNREADABLE: o/r#9\n"), "got {out:?}");
    assert!(out.contains("DEFER\n"), "got {out:?}");
}

#[test]
fn a_subset_without_a_trailing_newline_still_renders_one() {
    let (out, _) = render(
        &Decision::PromoteSubset {
            open: vec!["o/r#3".into()],
            subset: "- work".into(),
        },
        &[],
    );
    assert!(out.ends_with("- work\n"), "got {out:?}");
}

// ---------------------------------------------------------------------------
// premise-false (#7904)
// ---------------------------------------------------------------------------

/// The motivating case: #7657's close gate requires that a MIXED set still
/// defers through this gate rather than falling through to escalation.
#[test]
fn a_premise_false_bullet_does_not_disqualify_an_otherwise_deferrable_set() {
    let i = with_open(
        inputs(
            "**Champion Review: NEEDS REVISION**\n\n\
             - [premise-false] Criterion 8: the cited path is not on `origin/main`.\n\
             - Technical Feasibility: depends on #3, still open.\n",
            "A proposal. Blocked by #3.",
        ),
        &["o/r#3"],
    );
    assert!(
        matches!(decide(&i, REPO, SELF), Decision::Defer { .. }),
        "a premise-false bullet is not dependency-shaped, but it must not make \
         the whole set read as a merits objection"
    );
}

#[test]
fn an_all_premise_false_set_gets_its_own_reason_not_merits_finding() {
    // The distinction is what the caller routes on: `merits-finding` means
    // escalate, `premise-false-only` means run the close gate against the
    // full, unfiltered set.
    let i = inputs(
        "**Champion Review: NEEDS REVISION**\n\n\
         - [premise-false] Criterion 6: the cited test file does not exist.\n\
         - [premise-false] Criterion 8: the cited line range does not exist.\n",
        "A proposal.",
    );
    assert_eq!(
        decide(&i, REPO, SELF),
        Decision::NoDefer {
            reason: "premise-false-only"
        }
    );
}

#[test]
fn a_real_merits_finding_is_never_masked_by_a_co_occurring_premise_false_one() {
    // The regression guard. Stripping first must not swallow a genuine merits
    // objection into `premise-false-only`, which would close a proposal a human
    // rejected on substance.
    let i = inputs(
        "**Champion Review: NEEDS REVISION**\n\n\
         - [premise-false] Criterion 8: the cited path does not exist.\n\
         - Scope Appropriateness: this is three issues in one.\n",
        "A proposal.",
    );
    assert_eq!(
        decide(&i, REPO, SELF),
        Decision::NoDefer {
            reason: "merits-finding"
        }
    );
}

#[test]
fn stripping_matches_only_a_tagged_bullet_not_the_tag_anywhere() {
    // `grep -v '^[[:space:]]*[-*][[:space:]]*\[premise-false\]'` is anchored at
    // the bullet marker. Prose that merely mentions the tag is kept.
    assert_eq!(
        strip_premise_false("- [premise-false] gone\n  * [premise-false] also gone\n- kept"),
        "- kept"
    );
    assert_eq!(
        strip_premise_false("- the reviewer called it [premise-false] mid-sentence"),
        "- the reviewer called it [premise-false] mid-sentence"
    );
    assert_eq!(strip_premise_false("not a bullet at all"), "not a bullet at all");
}

#[test]
fn the_fetch_schedule_agrees_with_the_decision_on_a_premise_false_only_set() {
    // `blockers_to_classify` and `decide` must strip the same way, or the CLI
    // pays for forge reads on a set the decision has already refused.
    let i = inputs("- [premise-false] Criterion 6: the cited file does not exist.\n", "");
    assert!(blockers_to_classify(&i, REPO, SELF).is_empty());
}
