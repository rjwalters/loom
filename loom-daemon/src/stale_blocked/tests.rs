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
use crate::park_record::BlockerRef;

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
fn every_checklist_prerequisite_resolved_but_unticked_is_unticked_not_stale() {
    // #9274 classes 2/3: an unchecked box is unmet whatever its ref's state.
    let e = Evidence {
        named: vec![
            dep(176, false, Some("CLOSED")),
            dep(177, false, Some("MERGED")),
        ],
        ..Evidence::default()
    };
    assert_eq!(
        classify(&e),
        Verdict::Unticked {
            resolved_refs: vec!["#176".to_string(), "#177".to_string()],
            unparsed: 0
        }
    );
}

/// 2am#1325: the checklist cites a PR closed without merging.
#[test]
fn closed_unmerged_pr_in_checklist_is_still_blocked() {
    let e = Evidence {
        named: vec![dep(305, false, Some("CLOSED_UNMERGED"))],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

/// 2am#1344 / gf180-trng#256: an unparseable unchecked line beside resolved
/// entries is Unticked, never Stale.
#[test]
fn unparseable_unchecked_line_next_to_resolved_entries_is_unticked() {
    let e = Evidence {
        named: vec![dep(268, false, Some("CLOSED"))],
        unparsed_unchecked: 1,
        ..Evidence::default()
    };
    assert_eq!(
        classify(&e),
        Verdict::Unticked {
            resolved_refs: vec!["#268".to_string()],
            unparsed: 1
        }
    );
}

/// A checklist whose only unchecked lines are unreadable is documented and
/// unmet, but it has no open parseable ref: the empty parseable set is
/// vacuously resolved, so it goes to `Unticked` for a human to read the lines.
#[test]
fn only_unparseable_unchecked_lines_is_unticked() {
    let e = Evidence {
        unparsed_unchecked: 2,
        ..Evidence::default()
    };
    assert_eq!(
        classify(&e),
        Verdict::Unticked {
            resolved_refs: vec![],
            unparsed: 2
        }
    );
}

/// An open parseable unchecked ref beside an unparseable line keeps the
/// checklist `StillBlocked`: the open ref is a real, current blocker.
#[test]
fn open_parseable_ref_beside_unparseable_line_is_still_blocked() {
    for state in [Some("OPEN"), None] {
        let e = Evidence {
            named: vec![dep(42, false, state)],
            unparsed_unchecked: 1,
            ..Evidence::default()
        };
        assert_eq!(classify(&e), Verdict::StillBlocked, "state {state:?}");
    }
}

/// gf180-tmds-tx#186: `- [ ] #187: ... do not infer ratification`, #187 merged.
#[test]
fn merged_ref_on_an_unchecked_line_is_unticked() {
    let e = Evidence {
        named: vec![dep(187, false, Some("MERGED"))],
        ..Evidence::default()
    };
    assert!(matches!(classify(&e), Verdict::Unticked { .. }));
}

/// sky130-pll#98 / kicad-tools#5240: a merged closing PR is no blocker reference.
#[test]
fn prose_ref_closed_unmerged_is_still_blocked() {
    let e = Evidence {
        prose: vec![prose_ref(9, "CLOSED_UNMERGED")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
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

// --- A linked closing PR is not a blocker reference (#9274 class 4) ---------

#[test]
fn closing_pr_alone_is_undocumented_whatever_its_state() {
    for state in ["MERGED", "OPEN", "CLOSED", ""] {
        let mut p = pr(4743, state);
        p.labels = vec!["loom:changes-requested".to_string()];
        let e = Evidence {
            closing: vec![p],
            ..Evidence::default()
        };
        assert_eq!(classify(&e), Verdict::Undocumented, "{state}");
    }
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
        named: vec![dep(176, true, None)],
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
        prose: vec![prose_ref(4743, "")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
    assert!(!resolved("CLOSED_UNMERGED"));
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
        declared: vec![BlockerRef::local(8322)],
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
        declared: vec![BlockerRef::local(8322)],
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
        declared: vec![BlockerRef::local(8322)],
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
        declared: vec![BlockerRef::local(8322)],
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
        declared: vec![BlockerRef::local(8322)],
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

/// A checklist reference with no park record counts as prose-only
/// too: the park record is the declaration, not the reference shape.
#[test]
fn a_checklist_reference_without_a_record_is_undeclared() {
    let e = Evidence {
        named: vec![dep(1, false, Some("OPEN"))],
        ..Evidence::default()
    };
    assert!(undeclared(&e));
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

// --- cited_among: the close-triggered re-check's population filter (#9102) --

fn input(body: &str, comments: &[(&str, &str)]) -> extract::Input {
    extract::Input {
        body: body.to_string(),
        comments: comments
            .iter()
            .map(|(login, body)| extract::Comment {
                author: extract::Author {
                    login: (*login).to_string(),
                },
                body: (*body).to_string(),
            })
            .collect(),
    }
}

#[test]
fn cited_among_matches_a_prose_reference_in_the_body() {
    let i = input("Blocked by #180: needs that first.", &[]);
    assert_eq!(cited_among(Artifact::Issue, &i, &[180]), vec![180]);
    assert!(cited_among(Artifact::Issue, &i, &[181]).is_empty());
}

#[test]
fn cited_among_matches_a_reference_that_lives_only_in_a_comment() {
    let i = input("No blocker in the body.", &[("someone", "Depends on #180")]);
    assert_eq!(cited_among(Artifact::Issue, &i, &[180]), vec![180]);
}

#[test]
fn cited_among_ignores_the_bot_s_own_comments() {
    // The bot's own notification comment names the closed number; it must
    // never count as a citation, or a re-run would find its own output.
    let i = input("", &[(extract::DEFAULT_BOT_LOGIN, "Blocked by #180")]);
    assert!(cited_among(Artifact::Issue, &i, &[180]).is_empty());
}

#[test]
fn cited_among_matches_an_unchecked_same_repo_dependency_entry() {
    let i = input("## Dependencies\n\n- [ ] #180: the parser\n", &[]);
    assert_eq!(cited_among(Artifact::Issue, &i, &[180]), vec![180]);
}

#[test]
fn cited_among_skips_a_checked_dependency_entry() {
    let i = input("## Dependencies\n\n- [x] #180: the parser\n", &[]);
    assert!(cited_among(Artifact::Issue, &i, &[180]).is_empty());
}

#[test]
fn cited_among_does_not_match_a_cross_repo_entry_by_number_alone() {
    let i = input("## Dependencies\n\n- [ ] owner/other#180: elsewhere\n", &[]);
    assert!(
        cited_among(Artifact::Issue, &i, &[180]).is_empty(),
        "a cross-repo #180 is not this repo's #180"
    );
}

#[test]
fn cited_among_reads_no_dependencies_checklist_for_a_pr() {
    // Mirrors gather()'s PR arm, which never reads a checklist.
    let i = input("## Dependencies\n\n- [ ] #180: the parser\n", &[]);
    assert!(cited_among(Artifact::Pr, &i, &[180]).is_empty());
}

#[test]
fn cited_among_reports_every_closed_number_cited_once_each() {
    let i = input("Blocked by #180\nDepends on #200\nRequires #180", &[]);
    assert_eq!(cited_among(Artifact::Issue, &i, &[200, 180, 999]), vec![180, 200]);
}

#[test]
fn cited_among_is_empty_on_an_empty_input() {
    assert!(cited_among(Artifact::Issue, &input("", &[]), &[180]).is_empty());
}

fn remote(repo: &str, number: i64, state: &str) -> RemoteRef {
    RemoteRef {
        repo: repo.to_string(),
        number,
        state: state.to_string(),
    }
}

#[test]
fn a_closed_cross_repo_declared_blocker_is_stale() {
    let e = Evidence {
        declared: vec![BlockerRef {
            repo: Some("example-org/tool-repo".into()),
            number: 202,
        }],
        remote: vec![remote("example-org/tool-repo", 202, "CLOSED")],
        ..Evidence::default()
    };
    assert!(!undeclared(&e));
    match classify(&e) {
        Verdict::Stale(r) => assert!(r[0].contains("example-org/tool-repo#202:CLOSED"), "{r:?}"),
        v => panic!("{v:?}"),
    }
}

#[test]
fn an_open_cross_repo_declared_blocker_stays_blocked() {
    let e = Evidence {
        remote: vec![remote("o/r", 5, "OPEN")],
        ..Evidence::default()
    };
    assert_eq!(classify(&e), Verdict::StillBlocked);
}

#[test]
fn a_closed_local_number_does_not_clear_a_cross_repo_blocker() {
    // Local #5 closed (prose), but the park names o/r#5 which is open: the park
    // text, once masked, carries no local #5 at all.
    let body = "<!-- loom:park Blocked by: o/r#5 -->";
    let input = crate::dep_recheck::extract::Input {
        body: body.to_string(),
        comments: Vec::new(),
    };
    assert!(cited_among(Artifact::Issue, &input, &[5]).is_empty());
    let local = crate::dep_recheck::extract::Input {
        body: "<!-- loom:park Blocked by: #5 -->".to_string(),
        comments: Vec::new(),
    };
    assert_eq!(cited_among(Artifact::Issue, &local, &[5]), vec![5]);
}

// --- HeldWithReason (#10558) ---------------------------------------------------

fn held(by: &str, reason: &str) -> Option<Held> {
    Some(Held {
        by: Some(by.to_string()),
        reason: reason.to_string(),
        at: None,
    })
}

/// #9274 x #10558: an all-unparseable unticked checklist is a cited (if
/// unreadable) dependency, so it stays `Unticked` even beside a reason record.
#[test]
fn an_unparseable_checklist_beside_a_reason_record_stays_unticked() {
    let e = Evidence {
        held: held("curator", "waiting"),
        unparsed_unchecked: 1,
        ..Evidence::default()
    };
    assert_eq!(
        classify(&e),
        Verdict::Unticked {
            resolved_refs: vec![],
            unparsed: 1
        }
    );
}

#[test]
fn a_reason_only_record_is_held_with_reason_not_undocumented() {
    let e = Evidence {
        held: held("curator", "waiting on a ruling"),
        ..Evidence::default()
    };
    assert_eq!(
        classify(&e),
        Verdict::HeldWithReason {
            by: Some("curator".to_string()),
            reason: "waiting on a ruling".to_string(),
        }
    );
}

#[test]
fn an_unstated_record_with_no_reason_stays_undocumented() {
    // `held` is only ever populated from a non-empty reason (batch.rs), so an
    // `(unstated)` record with an empty reason arrives as `held: None`.
    assert_eq!(classify(&Evidence::default()), Verdict::Undocumented);
}

#[test]
fn a_reason_record_plus_a_numbered_ref_is_classified_by_the_ref() {
    let e = Evidence {
        prose: vec![prose_ref(7, "CLOSED")],
        held: held("curator", "x"),
        ..Evidence::default()
    };
    assert!(matches!(classify(&e), Verdict::Stale(_)));
    let open = Evidence {
        prose: vec![prose_ref(7, "OPEN")],
        held: held("curator", "x"),
        ..Evidence::default()
    };
    assert_eq!(classify(&open), Verdict::StillBlocked);
}

#[test]
fn a_daemon_hold_record_is_held_with_reason() {
    let e = Evidence {
        held: held("daemon", "pr-less hold"),
        ..Evidence::default()
    };
    assert!(matches!(
        classify(&e),
        Verdict::HeldWithReason { by: Some(b), .. } if b == "daemon"
    ));
}
