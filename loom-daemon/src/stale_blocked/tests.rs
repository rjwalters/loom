//! Unit tests for the `loom:blocked` staleness classifier (#8927).
//!
//! The three evidence rows from #8927's own incident table are the three cases
//! that matter, and each is covered by name below: #178 (a prose-cited blocker
//! that closed the day after the block was applied), #179 (two checklist
//! prerequisites, both since closed), #180 (`loom:blocked` with no blocker
//! recorded anywhere). The fourth case — a genuinely still-open blocker —
//! is the non-event this check must stay quiet about, and is what stops the
//! advisory from crying wolf on every sweep.

use super::*;

fn dep(number: i64, checked: bool, state: Option<&str>) -> named::Dep {
    named::Dep {
        number,
        repo: None,
        checked,
        state: state.map(str::to_string),
    }
}

fn prose_ref(number: i64, state: &str) -> premise::Ref {
    premise::Ref {
        number,
        state: state.to_string(),
    }
}

fn pr(number: i64, state: &str) -> recheck::Pr {
    recheck::Pr {
        number,
        state: state.to_string(),
        labels: Vec::new(),
        mergeable: "MERGEABLE".to_string(),
        merge_state_status: "CLEAN".to_string(),
    }
}

// --- Undocumented: #180's row ------------------------------------------------

#[test]
fn no_reference_of_any_kind_is_undocumented() {
    assert_eq!(classify(&Evidence::default()), Verdict::Undocumented);
}

// --- Stale via a prose reference: #178's row ---------------------------------

#[test]
fn prose_cited_blocker_that_closed_is_stale() {
    let e = Evidence {
        prose: vec![prose_ref(7, "CLOSED")],
        ..Evidence::default()
    };
    match classify(&e) {
        Verdict::Stale(reasons) => {
            assert_eq!(reasons.len(), 1);
            assert!(reasons[0].contains("no longer open"), "{:?}", reasons);
            assert!(reasons[0].contains("7:CLOSED"), "{:?}", reasons);
        }
        other => panic!("expected Stale, got {other:?}"),
    }
}

#[test]
fn prose_cited_blocker_still_open_is_not_reported() {
    let e = Evidence {
        prose: vec![prose_ref(7, "OPEN")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

// --- Stale via a `## Dependencies` checklist: #179's row ---------------------

#[test]
fn every_checklist_prerequisite_resolved_is_stale() {
    let e = Evidence {
        named: vec![
            dep(176, false, Some("CLOSED")),
            dep(177, false, Some("MERGED")),
        ],
        ..Evidence::default()
    };
    match classify(&e) {
        Verdict::Stale(reasons) => {
            assert_eq!(reasons.len(), 1);
            assert!(reasons[0].contains("checklist"), "{:?}", reasons);
            assert!(reasons[0].contains("176:CLOSED"), "{:?}", reasons);
            assert!(reasons[0].contains("177:MERGED"), "{:?}", reasons);
        }
        other => panic!("expected Stale, got {other:?}"),
    }
}

#[test]
fn checklist_with_one_still_open_prerequisite_is_not_reported() {
    let e = Evidence {
        named: vec![
            dep(176, false, Some("CLOSED")),
            dep(177, false, Some("OPEN")),
        ],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

#[test]
fn all_ticked_checklist_is_stale() {
    // Nobody looked up a ticked item's state, and nothing needs to: the
    // checklist itself says the prerequisite is done, yet `loom:blocked`
    // survives.
    let e = Evidence {
        named: vec![dep(176, true, None)],
        ..Evidence::default()
    };
    assert!(matches!(classify(&e), Verdict::Stale(_)));
}

// --- Stale via a linked closing PR ------------------------------------------

#[test]
fn merged_closing_pr_is_stale() {
    let e = Evidence {
        closing: vec![pr(4743, "MERGED")],
        ..Evidence::default()
    };
    match classify(&e) {
        Verdict::Stale(reasons) => {
            assert_eq!(reasons.len(), 1);
            assert!(reasons[0].contains("closing PR"), "{:?}", reasons);
        }
        other => panic!("expected Stale, got {other:?}"),
    }
}

#[test]
fn open_closing_pr_is_not_reported() {
    let e = Evidence {
        closing: vec![pr(4743, "OPEN")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

#[test]
fn open_closing_pr_carrying_a_block_label_is_still_not_stale() {
    // `recheck::verdict` would call this `blocked`. That is a fact about the
    // PR, not about the issue's `loom:blocked` label, and either way the PR is
    // open — so the block is not stale. Asserted because reaching for
    // `recheck::verdict` here is the obvious wrong shortcut.
    let mut p = pr(4743, "OPEN");
    p.labels = vec!["loom:changes-requested".to_string()];
    let e = Evidence {
        closing: vec![p],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

// --- Mixed and multi-signal cases -------------------------------------------

#[test]
fn a_partially_resolved_prose_set_is_reported() {
    // #8927's Test Plan edge case: multiple blocker references where only one
    // has closed. `premise::compute`'s own "any reference no longer OPEN" rule,
    // applied here rather than re-invented.
    let e = Evidence {
        prose: vec![prose_ref(7, "CLOSED"), prose_ref(8, "OPEN")],
        ..Evidence::default()
    };
    assert!(matches!(classify(&e), Verdict::Stale(_)));
}

#[test]
fn two_independent_signals_produce_two_reasons() {
    let e = Evidence {
        named: vec![dep(176, false, Some("CLOSED"))],
        prose: vec![prose_ref(7, "MERGED")],
        ..Evidence::default()
    };
    match classify(&e) {
        Verdict::Stale(reasons) => assert_eq!(reasons.len(), 2, "{reasons:?}"),
        other => panic!("expected Stale, got {other:?}"),
    }
}

#[test]
fn a_reference_in_any_shape_prevents_the_undocumented_verdict() {
    // An issue whose ONLY blocker mention is a still-open prose reference in a
    // comment is documented and genuinely blocked — neither category.
    for e in [
        Evidence {
            named: vec![dep(1, false, Some("OPEN"))],
            ..Evidence::default()
        },
        Evidence {
            prose: vec![prose_ref(1, "OPEN")],
            ..Evidence::default()
        },
        Evidence {
            closing: vec![pr(1, "OPEN")],
            ..Evidence::default()
        },
    ] {
        assert_eq!(classify(&e), Verdict::StillBlocked);
    }
}

// --- Unrecognised state must never manufacture staleness --------------------

#[test]
fn an_unrecognised_state_is_treated_as_open() {
    // An empty or unknown state string is a read this check did not understand.
    // Reporting it as resolved would be the "confident wrong answer" the
    // dep_recheck module's own fail-safe rule exists to prevent.
    let e = Evidence {
        closing: vec![pr(4743, "")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
    assert!(!resolved(""));
    assert!(!resolved("DRAFT"));
    assert!(resolved("MERGED"));
    assert!(resolved("CLOSED"));
}

// --- The PR population and the park record (#8925) ---------------------------

/// A parked PR whose blocker has closed and which nothing else holds back. The
/// #8314 shape once its blocker #8322 lands: ready to unpark.
#[test]
fn a_parked_pr_whose_blocker_closed_is_stale() {
    let e = Evidence {
        prose: vec![prose_ref(8322, "CLOSED")],
        declared: vec![8322],
        self_block: None,
        ..Evidence::default()
    };
    assert!(matches!(classify(&e), Verdict::Stale(_)));
    assert!(!undeclared(&e), "a declared park is not prose-only");
}

/// The #4634/#7267 gate, transposed. Cleared dependency, but a human decision is
/// pending on the PR itself — reported, never reported as ready to unpark.
#[test]
fn a_parked_pr_with_a_cleared_blocker_but_an_operator_hold_is_superseded() {
    let e = Evidence {
        prose: vec![prose_ref(8322, "CLOSED")],
        declared: vec![8322],
        self_block: park_self_block(&{
            let mut p = pr(8314, "OPEN");
            p.labels = vec!["loom:operator".to_string()];
            p
        }),
        ..Evidence::default()
    };
    match classify(&e) {
        Verdict::Superseded { cleared, block } => {
            assert_eq!(cleared.len(), 1, "{cleared:?}");
            assert!(block.contains("loom:operator"), "{block}");
        }
        other => panic!("expected Superseded, got {other:?}"),
    }
}

/// A conflicting PR cannot land, so the cleared dependency is not sufficient
/// (#7267's rule, same direction).
#[test]
fn a_parked_pr_that_cannot_land_is_superseded() {
    let mut p = pr(8314, "OPEN");
    p.mergeable = "CONFLICTING".to_string();
    p.merge_state_status = "DIRTY".to_string();
    let e = Evidence {
        prose: vec![prose_ref(8322, "MERGED")],
        declared: vec![8322],
        self_block: park_self_block(&p),
        ..Evidence::default()
    };
    assert!(matches!(classify(&e), Verdict::Superseded { .. }));
}

/// The labels that are a PR's **normal lane** must not act as a self-block —
/// this is the exact label set PR #8314 carries, and folding any of it in would
/// re-create the stall this check exists to surface.
#[test]
fn a_prs_own_review_state_labels_are_not_a_self_block() {
    let mut p = pr(8314, "OPEN");
    p.labels = vec![
        "loom:blocked".to_string(),
        "loom:changes-requested".to_string(),
        "loom:ci-failure".to_string(),
        "loom:review-requested".to_string(),
        "loom:pr".to_string(),
    ];
    assert_eq!(park_self_block(&p), None);

    let e = Evidence {
        prose: vec![prose_ref(8322, "CLOSED")],
        declared: vec![8322],
        self_block: park_self_block(&p),
        ..Evidence::default()
    };
    assert!(
        matches!(classify(&e), Verdict::Stale(_)),
        "removing loom:blocked must hand #8314 back to its loom:changes-requested lane"
    );
}

/// A merged or closed PR has no self-block: GitHub stops computing mergeability,
/// so reading it would be the meaningless-flicker signal #8253 already removed
/// from `recheck::blocker_line`.
#[test]
fn a_non_open_pr_has_no_self_block() {
    let mut p = pr(8314, "MERGED");
    p.mergeable = "UNKNOWN".to_string();
    p.merge_state_status = "UNKNOWN".to_string();
    p.labels = vec!["loom:operator".to_string()];
    assert_eq!(park_self_block(&p), None);
}

/// The superseding gate can only downgrade, never invent. A still-blocked
/// artifact stays quiet even when it also carries a self-block.
#[test]
fn a_self_block_alone_never_manufactures_a_finding() {
    let mut p = pr(8314, "OPEN");
    p.labels = vec!["loom:operator".to_string()];
    let e = Evidence {
        prose: vec![prose_ref(8322, "OPEN")],
        declared: vec![8322],
        self_block: park_self_block(&p),
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

/// #8852's live instance: a real, correctly-reasoned dependency stated only in a
/// comment. `undeclared` is what makes it visible BEFORE the blocker closes.
#[test]
fn a_blocker_cited_only_in_prose_is_undeclared() {
    let e = Evidence {
        prose: vec![prose_ref(8860, "OPEN")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
    assert!(undeclared(&e), "prose-only park must be flagged");
}

/// A checklist or closing-PR reference with no park record counts as prose-only
/// too: the park record is the declaration, not the reference shape.
#[test]
fn a_checklist_or_closing_pr_reference_without_a_record_is_undeclared() {
    for e in [
        Evidence {
            named: vec![dep(1, false, Some("OPEN"))],
            ..Evidence::default()
        },
        Evidence {
            closing: vec![pr(1, "OPEN")],
            ..Evidence::default()
        },
    ] {
        assert!(undeclared(&e));
    }
}

/// An artifact with nothing cited at all is `Undocumented`, the louder finding —
/// and deliberately NOT also counted as prose-only, which would double-report it.
#[test]
fn an_undocumented_block_is_not_also_undeclared() {
    let e = Evidence::default();
    assert_eq!(classify(&e), Verdict::Undocumented);
    assert!(!undeclared(&e));
}

#[test]
fn artifact_labels_name_both_populations() {
    assert_eq!(Artifact::Issue.label(), "issue");
    assert_eq!(Artifact::Pr.label(), "PR");
}
