//! Tests for the verdict-label contradiction guard.
//!
//! The label set arrives from the forge, which means it is attacker-adjacent
//! (anyone who can label a PR writes it) and its ORDER is not a contract. Both
//! facts are load-bearing here: this guard is the last thing between a racing
//! approval and an irreversible merge, so "it happened to pass for the order
//! the test wrote" is not evidence of anything.

use super::*;

#[test]
fn a_clean_approval_is_not_a_contradiction() {
    assert_eq!(contradiction("loom:pr"), None);
    assert_eq!(contradiction("loom:pr\nloom:urgent\ntier:goal-advancing"), None);
}

#[test]
fn each_blocking_label_contradicts_an_approval() {
    for b in BLOCKING {
        let labels = format!("loom:pr\n{b}");
        assert_eq!(contradiction(&labels), Some(*b), "{b} must block");
    }
}

#[test]
fn without_an_approval_there_is_nothing_to_contradict() {
    // This guard is only ever about a PR CLAIMING approval. A PR carrying
    // `loom:changes-requested` alone is in a perfectly ordinary state, and
    // blocking it here would refuse merges that the missing-`loom:pr` guard
    // already handles with its own message and its own override.
    for b in BLOCKING {
        assert_eq!(contradiction(b), None, "{b} alone is not a contradiction");
    }
    assert_eq!(contradiction(""), None);
}

#[test]
fn the_verdict_does_not_depend_on_label_order() {
    // THE property. The forge's `labels` array order is not a contract, so a
    // guard that read "whichever came first" could be silently defeated by
    // relabelling — which is exactly the race (#8112) that produced the
    // contradictory state in the first place.
    let set = ["loom:pr", "loom:changes-requested", "loom:urgent"];
    // Every permutation of three elements.
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for order in orders {
        let labels = order.map(|i| set[i]).join("\n");
        assert_eq!(
            contradiction(&labels),
            Some("loom:changes-requested"),
            "order {order:?} must reach the same verdict: {labels:?}"
        );
    }
}

#[test]
fn several_blockers_report_the_first_in_the_fixed_order() {
    // Not the first in the INPUT — the first in BLOCKING. Two concurrent
    // reviewers plus a hold is a real state, and the message must be
    // reproducible from the label set alone.
    let labels = "loom:review-requested\nloom:pr\nloom:operator\nloom:blocked";
    assert_eq!(contradiction(labels), Some("loom:blocked"));
}

#[test]
fn a_label_that_merely_contains_a_blocker_is_not_one() {
    // Substring matching here would be a fail-CLOSED bug rather than a
    // fail-open one, but it would still refuse legitimate merges: the shell
    // this ports from used `grep -qx`, whole-line, and so does this.
    assert_eq!(contradiction("loom:pr\nloom:blocked-upstream"), None);
    assert_eq!(contradiction("loom:pr\nnot-loom:blocked"), None);
    assert_eq!(contradiction("loom:prime"), None);
}

#[test]
fn surrounding_whitespace_does_not_hide_a_blocker() {
    assert_eq!(
        contradiction("  loom:pr  \n\tloom:blocked\t"),
        Some("loom:blocked"),
        "a padded label is still that label"
    );
}

#[test]
fn blank_lines_in_the_label_set_are_harmless() {
    // `jq -r '.labels[]?.name // empty'` on a PR with no labels emits an empty
    // string, and the shell passed that straight through.
    assert_eq!(contradiction("\n\nloom:pr\n\nloom:operator\n\n"), Some("loom:operator"));
    assert_eq!(contradiction("\n\n\n"), None);
}

#[test]
fn the_message_names_both_labels_and_the_head_it_judged() {
    let m = message("8076", "loom:changes-requested", "loom:pr\nloom:changes-requested", "abc123");
    assert!(m.contains("#8076"), "{m}");
    assert!(m.contains("loom:pr"), "{m}");
    assert!(m.contains("loom:changes-requested"), "{m}");
    // The head SHA is what makes the refusal checkable later: the labels are
    // mutable, the tree they were about is not.
    assert!(m.contains("abc123"), "{m}");
    assert!(m.contains("no override flag"), "the absence of a bypass is stated: {m}");
}

#[test]
fn the_message_renders_an_empty_label_set_readably() {
    // Unreachable through `contradiction` (no `loom:pr` means no verdict), but
    // the CLI can be handed anything and a bare "Current labels: " reads as a
    // rendering bug to whoever is staring at a refused merge at 2am.
    let m = message("1", "loom:blocked", "", "sha");
    assert!(m.contains("Current labels: <none>"), "{m}");
}

#[test]
fn the_clean_sentinel_matches_the_string_merge_pr_sh_checks_for() {
    // merge-pr.sh hard-codes the sentinel, because the whole point is that a
    // pass needs a positive signal the caller can recognise. If the constant
    // moves and the script does not, the guard fails CLOSED forever — safe,
    // but it would refuse every merge in the fleet until someone noticed.
    let sh = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/merge-pr.sh"),
    );
    let Ok(sh) = sh else {
        return; // not a full checkout; nothing to compare against
    };
    let sentinel = CLEAN;
    assert!(
        sh.contains(sentinel),
        "merge-pr.sh must test for {sentinel:?}; if the constant moved, move it there too"
    );
}
