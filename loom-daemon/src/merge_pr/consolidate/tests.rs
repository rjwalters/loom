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
    let group = [comp(1, H1, &["shared.rs"]), comp(2, H2, &["shared.rs"])];
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
    let group = [comp(1, H1, &["shared.rs"]), comp(2, H2, &["shared.rs"])];
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
