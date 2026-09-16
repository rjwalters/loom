//! Tests for the `--check-unescalate` decision (epic #7810, PR 3).

use super::*;

const REPO: &str = "o/r";
const SELF: &str = "o/r#5";
const OPERATOR_ONLY: &str = "loom:operator-only";
const CYCLE_PREFIX: &str = "<!-- champion:dependency-cycle ";
const UNESC_PREFIX: &str = "<!-- champion:unescalated:";

fn markers() -> Markers<'static> {
    Markers {
        operator_only_label: OPERATOR_ONLY,
        cycle_prefix: CYCLE_PREFIX,
        unescalate_prefix: UNESC_PREFIX,
    }
}

/// A parked issue whose escalation cites a dependency.
fn parked() -> Inputs {
    Inputs {
        body: String::new(),
        labels: vec![OPERATOR_ONLY.to_string()],
        comments: String::new(),
        escalation: "- Blocked by #3\n".to_string(),
        refs: ClassifiedRefs::default(),
    }
}

fn decide_it(i: &Inputs) -> Decision {
    decide(i, REPO, SELF, &markers())
}

#[test]
fn an_unparked_issue_is_not_unescalated() {
    let mut i = parked();
    i.labels.clear();
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "not-operator-only",
            still_open: Vec::new(),
        }
    );
}

#[test]
fn a_cycle_escalation_is_never_undone_by_this_path() {
    // Waiting never resolves a cycle, so it was not a timing finding and was
    // never this mechanism's to reverse.
    let mut i = parked();
    i.comments = format!("{CYCLE_PREFIX}abc -->");
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "cycle-escalation",
            still_open: Vec::new(),
        }
    );
}

#[test]
fn a_missing_escalation_record_stops_it() {
    let mut i = parked();
    i.escalation = "   \n".into();
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "no-escalation-record",
            still_open: Vec::new(),
        }
    );
}

#[test]
fn a_merits_escalation_is_never_undone() {
    let mut i = parked();
    i.escalation = "- The approach is wrong on the merits\n".into();
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "merits-finding",
            still_open: Vec::new(),
        }
    );
}

/// THE asymmetry with `check_defer`, which tolerates unknowns.
#[test]
fn an_unreadable_blocker_refuses_to_unescalate() {
    let mut i = parked();
    i.refs.unknown = vec!["o/r#3".into()];
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "unreadable-blocker",
            still_open: Vec::new(),
        },
        "releasing a human-parked proposal on blockers nobody could read is the \
         direction that does not recover"
    );
}

#[test]
fn an_unreadable_blocker_refuses_even_when_others_have_cleared() {
    let mut i = parked();
    i.refs.resolved = vec!["o/r#3".into()];
    i.refs.unknown = vec!["o/r#9".into()];
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "unreadable-blocker",
            still_open: Vec::new(),
        },
        "a partially-readable set must not be treated as cleared"
    );
}

#[test]
fn all_blockers_cleared_unescalates() {
    let mut i = parked();
    i.refs.resolved = vec!["o/r#3".into()];
    match decide_it(&i) {
        Decision::Cleared {
            cleared,
            blocker_fingerprint,
        } => {
            assert_eq!(cleared, vec!["o/r#3"]);
            assert_eq!(blocker_fingerprint.len(), 16);
        }
        other => panic!("expected Cleared, got {other:?}"),
    }
}

#[test]
fn a_still_open_blocker_with_no_subset_stops_it() {
    let mut i = parked();
    i.refs.open = vec!["o/r#3".into()];
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "blocker-still-open",
            // Named, not empty: this refusal is reached only after the blocker
            // was classified, and the shell reports it on the way past.
            still_open: vec!["o/r#3".to_string()],
        }
    );
}

#[test]
fn a_still_open_blocker_with_a_subset_carves_out() {
    let mut i = parked();
    i.refs.open = vec!["o/r#3".into()];
    i.body = "## Startable subset\n- independent work\n".into();
    match decide_it(&i) {
        Decision::Subset {
            still_open,
            blocker_fingerprint,
            subset,
        } => {
            assert_eq!(still_open, vec!["o/r#3"]);
            assert!(blocker_fingerprint.starts_with("subset-"), "{blocker_fingerprint}");
            assert!(subset.contains("independent work"), "{subset:?}");
        }
        other => panic!("expected Subset, got {other:?}"),
    }
}

#[test]
fn the_two_paths_never_share_a_marker() {
    // A subset carve-out and a blockers-cleared release on the same issue must
    // not collide: the `subset-` prefix keeps them distinct even if the node
    // sets hashed identically.
    let mut subset = parked();
    subset.refs.open = vec!["o/r#3".into()];
    subset.body = "## Startable subset\n- work\n".into();

    let mut cleared = parked();
    cleared.refs.resolved = vec!["o/r#3".into()];

    let (
        Decision::Subset {
            blocker_fingerprint: a,
            ..
        },
        Decision::Cleared {
            blocker_fingerprint: b,
            ..
        },
    ) = (decide_it(&subset), decide_it(&cleared))
    else {
        panic!("expected one of each");
    };
    assert_ne!(a, b);
    assert!(a.starts_with("subset-"));
}

#[test]
fn an_existing_marker_short_circuits_the_second_attempt() {
    let mut i = parked();
    i.refs.resolved = vec!["o/r#3".into()];
    let Decision::Cleared {
        blocker_fingerprint,
        ..
    } = decide_it(&i)
    else {
        panic!("expected Cleared");
    };

    i.comments = format!("earlier comment\n{UNESC_PREFIX}{blocker_fingerprint} -->\n");
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "already-unescalated",
            still_open: Vec::new(),
        }
    );
}

#[test]
fn a_marker_for_a_different_fingerprint_does_not_short_circuit() {
    let mut i = parked();
    i.refs.resolved = vec!["o/r#3".into()];
    i.comments = format!("{UNESC_PREFIX}some-other-fingerprint -->");
    assert!(
        matches!(decide_it(&i), Decision::Cleared { .. }),
        "a marker for a different blocker set must not suppress this one"
    );
}

#[test]
fn a_marker_must_match_the_whole_fingerprint_not_a_prefix() {
    // Without the trailing ` -->`, `abc` would match a marker for `abcdef` —
    // and a later, larger blocker set would be mistaken for one already handled.
    let mut i = parked();
    i.refs.resolved = vec!["o/r#3".into()];
    let Decision::Cleared {
        blocker_fingerprint: fp,
        ..
    } = decide_it(&i)
    else {
        panic!("expected Cleared");
    };
    i.comments = format!("{UNESC_PREFIX}{fp}EXTRA -->");
    assert!(
        matches!(decide_it(&i), Decision::Cleared { .. }),
        "a longer fingerprint sharing this prefix must not count as a match"
    );
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn the_cleared_path_renders_its_markers() {
    let (out, code) = render(
        &Decision::Cleared {
            cleared: vec!["o/r#3".into()],
            blocker_fingerprint: "abc".into(),
        },
        &[],
    );
    assert_eq!(out, "UNESCALATE\nCLEARED_BLOCKERS: o/r#3\nBLOCKER_FINGERPRINT: abc\n");
    assert_eq!(code, 0);
}

#[test]
fn the_subset_path_reports_what_remains_blocked_before_the_verdict() {
    let (out, code) = render(
        &Decision::Subset {
            still_open: vec!["o/r#3".into()],
            blocker_fingerprint: "subset-abc".into(),
            subset: "- work\n".into(),
        },
        &[],
    );
    assert!(out.starts_with("STILL_OPEN: o/r#3\n"), "got {out:?}");
    assert!(out.contains("SUBSET_CARVEOUT: yes\n"), "got {out:?}");
    assert_eq!(code, 0);
}

#[test]
fn a_refusal_exits_one() {
    let (out, code) = render(
        &Decision::NoUnescalate {
            reason: "blocker-still-open",
            still_open: Vec::new(),
        },
        &[],
    );
    assert_eq!(out, "NO_UNESCALATE\nREASON: blocker-still-open\n");
    assert_eq!(code, 1);
}

#[test]
fn a_refusal_that_found_open_blockers_still_reports_them() {
    // The shell prints STILL_OPEN as soon as it classifies an open blocker —
    // before it knows whether the verdict will be a refusal or a release. A
    // caller told only "blocker-still-open" would have to re-read the issue to
    // learn WHICH blocker.
    let (out, code) = render(
        &Decision::NoUnescalate {
            reason: "blocker-still-open",
            still_open: vec!["o/r#3".into(), "o/r#4".into()],
        },
        &[],
    );
    assert_eq!(out, "STILL_OPEN: o/r#3 o/r#4\nNO_UNESCALATE\nREASON: blocker-still-open\n");
    assert_eq!(code, 1);
}

#[test]
fn an_already_unescalated_subset_still_reports_what_is_open() {
    // The second refusal reachable past classification. It and the
    // cleared-path `already-unescalated` share a reason string but not a
    // position, which is exactly why the open set rides on the decision.
    let mut i = parked();
    i.refs.open = vec!["o/r#3".into()];
    i.body = "## Startable subset\n- work\n".into();
    let Decision::Subset {
        blocker_fingerprint: fp,
        ..
    } = decide_it(&i)
    else {
        panic!("expected a carve-out");
    };
    i.comments = format!("{UNESC_PREFIX}{fp} -->");
    assert_eq!(
        decide_it(&i),
        Decision::NoUnescalate {
            reason: "already-unescalated",
            still_open: vec!["o/r#3".to_string()],
        }
    );
}

#[test]
fn a_refusal_reached_before_classification_reports_nothing_open() {
    // The other direction: `not-operator-only` is decided before any blocker is
    // looked at, so claiming an open set there would be an invention.
    let mut i = parked();
    i.labels.clear();
    let Decision::NoUnescalate { still_open, .. } = decide_it(&i) else {
        panic!("expected a refusal");
    };
    assert!(still_open.is_empty());
}
