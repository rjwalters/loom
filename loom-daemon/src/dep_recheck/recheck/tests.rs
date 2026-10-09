//! Tests for the `dep-recheck` fingerprint (epic #7810, PR 4).

use super::*;

fn pr(number: i64, state: &str, labels: &[&str], mergeable: &str, status: &str) -> Pr {
    Pr {
        number,
        state: state.to_string(),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        mergeable: mergeable.to_string(),
        merge_state_status: status.to_string(),
    }
}

fn open_clean(number: i64) -> Pr {
    pr(number, "OPEN", &[], "MERGEABLE", "CLEAN")
}

fn hash_of(prs: &[Pr]) -> String {
    compute(prs, None, "", "").conclusion_hash
}

#[test]
fn the_same_state_hashes_the_same_twice() {
    let prs = [pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")];
    assert_eq!(hash_of(&prs), hash_of(&prs));
}

#[test]
fn no_prs_is_clear_with_empty_blockers() {
    let o = compute(&[], None, "", "");
    assert_eq!(o.verdict, "clear");
    assert_eq!(o.blockers, "");
}

// ---------------------------------------------------------------------------
// #7281: UNKNOWN fails safe to conflicting
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_mergeable_is_treated_as_conflicting() {
    // GitHub reports UNKNOWN while it is still computing. Reading it as "not
    // conflicting" let a PR clear for one pass and re-block the next, flipping
    // VERDICT with nothing about the PR actually changing.
    let unknown = [pr(7, "OPEN", &[], "UNKNOWN", "UNKNOWN")];
    let conflicting = [pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")];
    assert_eq!(verdict(&unknown), "blocked");
    assert_eq!(
        hash_of(&unknown),
        hash_of(&conflicting),
        "a value flickering through UNKNOWN and back must not change the hash"
    );
}

#[test]
fn an_unknown_on_either_field_alone_is_enough() {
    assert_eq!(verdict(&[pr(7, "OPEN", &[], "UNKNOWN", "CLEAN")]), "blocked");
    assert_eq!(verdict(&[pr(7, "OPEN", &[], "MERGEABLE", "UNKNOWN")]), "blocked");
}

#[test]
fn the_blockers_line_applies_the_same_unknown_rule_as_the_verdict() {
    // Both components must flip at the same boundary, or the hash churns even
    // when VERDICT holds steady — which is the bug, not the verdict itself.
    assert!(blockers(&[pr(7, "OPEN", &[], "UNKNOWN", "CLEAN")]).ends_with(":conflicting"));
}

#[test]
fn a_mergeable_open_pr_with_no_block_label_does_not_block() {
    assert_eq!(verdict(&[open_clean(7)]), "clear");
}

// ---------------------------------------------------------------------------
// #7362: the label component is narrow
// ---------------------------------------------------------------------------

#[test]
fn review_cycle_label_churn_does_not_change_the_hash() {
    // The #6805 incident: 28+ near-duplicate comments in 36 hours, every one
    // reporting the same unchanged verdict, because BLOCKERS folded in the
    // full label set.
    let a = [pr(7, "OPEN", &["loom:pr"], "MERGEABLE", "CLEAN")];
    let b = [pr(
        7,
        "OPEN",
        &["loom:reviewing", "loom:treating", "loom:operator"],
        "MERGEABLE",
        "CLEAN",
    )];
    assert_eq!(hash_of(&a), hash_of(&b));
}

#[test]
fn a_superseding_block_label_does_change_the_hash_and_the_verdict() {
    let plain = [open_clean(7)];
    for label in ["loom:changes-requested", "loom:blocked"] {
        let blocked = [pr(7, "OPEN", &[label], "MERGEABLE", "CLEAN")];
        assert_eq!(verdict(&blocked), "blocked", "{label}");
        assert_ne!(hash_of(&plain), hash_of(&blocked), "{label}");
    }
}

#[test]
fn a_closed_pr_never_blocks_whatever_labels_it_carries() {
    assert_eq!(
        verdict(&[pr(
            7,
            "MERGED",
            &["loom:changes-requested"],
            "CONFLICTING",
            "DIRTY"
        )]),
        "clear"
    );
}

// ---------------------------------------------------------------------------
// #8253: a MERGED/CLOSED PR's transient UNKNOWN mergeability must not move
// the hash
// ---------------------------------------------------------------------------

#[test]
fn a_merged_pr_flickering_between_a_concrete_mergeability_and_unknown_hashes_the_same() {
    // GitHub stops computing mergeability once a PR merges and can read back
    // mergeable/mergeStateStatus as UNKNOWN non-deterministically. is_conflicting()
    // fails UNKNOWN safe to "conflicting" — correct for an OPEN PR, but for an
    // already-MERGED PR that reading is meaningless noise and must not move
    // blockers()/conclusion_hash.
    let concrete = [pr(7, "MERGED", &[], "MERGEABLE", "CLEAN")];
    let unknown = [pr(7, "MERGED", &[], "UNKNOWN", "UNKNOWN")];
    assert_eq!(
        hash_of(&concrete),
        hash_of(&unknown),
        "a MERGED PR's mergeability flicker must not move CONCLUSION_HASH"
    );
    assert_eq!(verdict(&concrete), "clear");
    assert_eq!(verdict(&unknown), "clear");
}

#[test]
fn a_closed_pr_flickering_between_a_concrete_mergeability_and_unknown_hashes_the_same() {
    let concrete = [pr(7, "CLOSED", &[], "MERGEABLE", "CLEAN")];
    let unknown = [pr(7, "CLOSED", &[], "UNKNOWN", "UNKNOWN")];
    assert_eq!(hash_of(&concrete), hash_of(&unknown));
}

#[test]
fn a_non_open_pr_blocker_line_reports_a_fixed_placeholder_not_the_mergeability() {
    // The merge-state bucket must be a fixed placeholder for a non-OPEN PR,
    // never the outcome of is_conflicting() — otherwise the flicker is merely
    // reproduced with a different label.
    assert!(blockers(&[pr(7, "MERGED", &[], "CONFLICTING", "DIRTY")]).ends_with(":n/a"));
    assert!(blockers(&[pr(7, "CLOSED", &[], "UNKNOWN", "UNKNOWN")]).ends_with(":n/a"));
}

#[test]
fn an_open_prs_unknown_failsafe_is_unchanged_by_the_open_state_gate() {
    // The fix must not touch OPEN-PR behavior: UNKNOWN still fails safe to
    // conflicting, and it still differs from the n/a bucket a non-OPEN PR gets.
    let open_unknown = [pr(7, "OPEN", &[], "UNKNOWN", "UNKNOWN")];
    assert!(blockers(&open_unknown).ends_with(":conflicting"));
    assert_eq!(verdict(&open_unknown), "blocked");
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[test]
fn pr_input_order_does_not_change_the_hash() {
    let a = [open_clean(3), open_clean(9)];
    let b = [open_clean(9), open_clean(3)];
    assert_eq!(hash_of(&a), hash_of(&b));
}

#[test]
fn blocker_lines_sort_lexicographically_not_numerically() {
    // `jq sort_by(.number) | sort` — the trailing `sort` wins, and it is a
    // string sort. "More sensible" numeric ordering would change the hash and
    // invalidate every persisted marker.
    let out = blockers(&[open_clean(9), open_clean(10)]);
    assert_eq!(
        out.lines().next(),
        Some("10:OPEN:no-block-label:mergeable"),
        "10 sorts before 9: {out}"
    );
}

// ---------------------------------------------------------------------------
// The caller-supplied pass-throughs
// ---------------------------------------------------------------------------

#[test]
fn the_verdict_override_replaces_the_computed_one() {
    // For the case the script cannot infer: no linked PR at all, where the
    // verdict comes from curator.md's secondary heuristic.
    assert_eq!(compute(&[], Some("blocked"), "", "").verdict, "blocked");
}

#[test]
fn an_empty_override_is_a_no_op_and_the_computation_stands() {
    let prs = [pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")];
    assert_eq!(compute(&prs, Some(""), "", "").verdict, "blocked");
    assert_eq!(compute(&prs, None, "", "").verdict, "blocked");
}

#[test]
fn a_substantively_changed_block_reason_still_changes_the_hash() {
    let a = compute(&[], Some("blocked"), "doctor cycle exhausted", "");
    let b = compute(&[], Some("blocked"), "Sweep coordination: blocking", "");
    assert_ne!(a.conclusion_hash, b.conclusion_hash);
}

#[test]
fn the_orthogonal_identity_changes_the_hash_when_present_and_not_when_empty() {
    // #6516: an escalation to a diagnosed-but-orthogonal blocker must not be
    // suppressed as "unchanged" — and every existing fingerprint must be
    // unaffected when the field is empty, which is the ordinary case.
    let prs = [open_clean(7)];
    let plain = compute(&prs, None, "", "");
    let with = compute(&prs, None, "", "epic-open-but-complete:owner/repo#14");
    let empty = compute(&prs, None, "", "");
    assert_ne!(plain.conclusion_hash, with.conclusion_hash);
    assert_eq!(plain.conclusion_hash, empty.conclusion_hash);
}

// ---------------------------------------------------------------------------
// #8254: the free-text pass-throughs are canonicalized before hashing
// ---------------------------------------------------------------------------

#[test]
fn a_block_reason_differing_only_in_case_or_whitespace_hashes_the_same() {
    // The one hash input still typed as prose by an agent. Unnormalised, these
    // five spellings of one unchanged state were five CONCLUSION_HASH values,
    // i.e. five "changed conclusion -> always comment" rows in curator.md's
    // four-way decision: exactly the #557/#298 churn shape.
    let base = compute(&[], Some("blocked"), "doctor cycle exhausted", "");
    for variant in [
        "Doctor cycle exhausted",
        "DOCTOR CYCLE EXHAUSTED",
        "  doctor cycle exhausted  ",
        "doctor   cycle\texhausted",
        "doctor\ncycle exhausted",
    ] {
        assert_eq!(
            base.conclusion_hash,
            compute(&[], Some("blocked"), variant, "").conclusion_hash,
            "{variant:?} is the same conclusion, differently typed"
        );
    }
}

#[test]
fn a_whitespace_only_block_reason_hashes_as_if_it_were_absent() {
    assert_eq!(
        compute(&[], Some("blocked"), "", "").conclusion_hash,
        compute(&[], Some("blocked"), "  \t \n ", "").conclusion_hash
    );
}

#[test]
fn an_orthogonal_identity_is_canonicalized_the_same_way() {
    let prs = [open_clean(7)];
    let a = compute(&prs, None, "", "epic-open-but-complete:owner/repo#14");
    let b = compute(&prs, None, "", "  Epic-Open-But-Complete:owner/repo#14 ");
    assert_eq!(a.conclusion_hash, b.conclusion_hash);
    // ...and a genuinely different identity is still a different conclusion.
    let c = compute(&prs, None, "", "epic-open-but-complete:owner/repo#15");
    assert_ne!(a.conclusion_hash, c.conclusion_hash);
}

#[test]
fn the_echoed_pass_throughs_stay_verbatim_uncanonicalized() {
    // cli.rs prints these as BLOCK_REASON=/ORTHOGONAL= for curator.md to eval
    // into the comment body. Canonicalizing what is HASHED must not flatten
    // what is READ.
    let o = compute(
        &[],
        Some("blocked"),
        "  Doctor Cycle   Exhausted ",
        " Epic-Open-But-Complete:owner/repo#14 ",
    );
    assert_eq!(o.block_reason, "  Doctor Cycle   Exhausted ");
    assert_eq!(o.orthogonal, " Epic-Open-But-Complete:owner/repo#14 ");
}

#[test]
fn the_ordinary_empty_pass_through_case_hashes_exactly_as_it_did_before() {
    // CONCLUSION_HASH is a PERSISTED identifier: it sits in live marker
    // comments on `loom:blocked` issues and is compared on the next pass, so a
    // change to the hash input re-posts every issue it moves. Canonicalizing
    // the empty string yields the empty string, which is why the ordinary case
    // — no --block-reason, no --orthogonal, the overwhelming majority of live
    // markers — is untouched by #8254. These two values were computed from the
    // pre-#8254 formula; they must not move.
    assert_eq!(compute(&[], None, "", "").conclusion_hash, "d88c83f77b541ea4");
    assert_eq!(
        compute(&[pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")], None, "", "").conclusion_hash,
        "b869e5c416771254"
    );
}

// ---------------------------------------------------------------------------
// #9308: exact golden vectors for the PERSISTED CONCLUSION_HASH contract
//
// Every expected digest below was computed independently, outside this crate,
// from an explicit four-field payload:
//   printf '%s\n%s\n%s\n%s' VERDICT BLOCKERS REASON_KEY ORTHOGONAL_KEY \
//     | shasum -a 256 | cut -c1-16
// A self-equality assertion (`hash(x) == hash(x)`) only proves determinism
// within one build; these pin the value across releases. If one of them moves,
// every live `curator:dep-recheck` marker carrying it re-posts once.
// ---------------------------------------------------------------------------

/// The report's verbatim `--block-reason` (sky130-temp-por#40): uppercase repo
/// owner and `(OPEN)` are exactly what canonicalization folds.
const REPORT_REASON: &str =
    "depends on #64, itself blocked on cross-repo 2AMLogic/klayout-tools#1962 (OPEN)";

#[test]
fn the_reported_block_reason_hashes_to_its_pinned_canonical_digest() {
    // payload: "blocked\n\n<lowercased reason>\n" (fourth field empty)
    let o = compute(&[], Some("blocked"), REPORT_REASON, "");
    assert_eq!(o.conclusion_hash, "e8876061202241c6");
    assert_eq!(o.block_reason, REPORT_REASON, "the echoed reason stays verbatim");
    for variant in [
        "  Depends on #64, itself blocked on cross-repo 2amlogic/klayout-tools#1962 (open) ",
        "DEPENDS ON #64,\titself blocked on cross-repo 2AMLOGIC/KLAYOUT-TOOLS#1962\n(OPEN)",
    ] {
        assert_eq!(compute(&[], Some("blocked"), variant, "").conclusion_hash, "e8876061202241c6");
    }
}

#[test]
fn the_reports_other_digest_is_exactly_the_pre_8320_uncanonicalized_formula() {
    // #9308's two observed values are not build randomness: 41df30ee... is the
    // same payload hashed with the reason VERBATIM, i.e. what every build
    // before #8320 (4daa840bf, first tagged v0.19.187) computed — the shell
    // original and the 0.19.104..0.19.186 Rust port alike. A host still running
    // such a build alternates with a current one for any reason containing an
    // uppercase letter or a non-single-space whitespace run.
    let legacy = short_sha16(&format!("blocked\n\n{REPORT_REASON}\n"));
    assert_eq!(legacy, "41df30ee18089360");
    assert_ne!(compute(&[], Some("blocked"), REPORT_REASON, "").conclusion_hash, legacy);
}

#[test]
fn multiple_sorted_blockers_with_a_nonempty_orthogonal_identity_hash_to_a_pinned_digest() {
    // BLOCKERS sorts lexicographically ("10:" < "64:" < "9:"), and the
    // orthogonal identity is canonicalized like the reason:
    //   blocked
    //   10:OPEN:no-block-label:conflicting
    //   64:MERGED:no-block-label:n/a
    //   9:OPEN:block-label:mergeable
    //   <lowercased REPORT_REASON>
    //   epic-open-but-complete:2amlogic/klayout-tools#1962
    let a = pr(9, "OPEN", &["loom:blocked", "loom:pr"], "MERGEABLE", "CLEAN");
    let b = pr(10, "OPEN", &[], "UNKNOWN", "UNKNOWN");
    let c = pr(64, "MERGED", &[], "UNKNOWN", "UNKNOWN");
    let orthogonal = " Epic-Open-But-Complete:2AMLogic/klayout-tools#1962 ";
    for prs in [[a.clone(), b.clone(), c.clone()], [c, a, b]] {
        let o = compute(&prs, None, REPORT_REASON, orthogonal);
        assert_eq!(o.verdict, "blocked");
        assert_eq!(
            o.conclusion_hash,
            "7493cccdbd9a3918",
            "input order: {:?}",
            prs.map(|p| p.number)
        );
        assert_eq!(o.orthogonal, orthogonal, "the echoed identity stays verbatim");
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[test]
fn a_stdin_document_decodes_with_optional_fields_absent() {
    let i: Input = serde_json::from_str(r#"{"prs":[{"number":7}]}"#).expect("decodes");
    assert_eq!(i.prs[0].number, 7);
    assert!(i.prs[0].state.is_empty());
    // Absent merge state is neither CONFLICTING nor UNKNOWN, so it does not
    // manufacture a block on a PR the fixture said nothing about.
    assert_eq!(verdict(&i.prs), "clear");
}

#[test]
fn the_merge_state_status_field_decodes_by_its_camel_case_name() {
    let i: Input =
        serde_json::from_str(r#"{"prs":[{"number":7,"state":"OPEN","mergeStateStatus":"DIRTY"}]}"#)
            .expect("decodes");
    assert_eq!(verdict(&i.prs), "blocked", "a rename here silently clears every conflicting PR");
}
