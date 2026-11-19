//! Tests for the #14223 owner-drift detection: pure comparisons without git
//! or network, plus the [`DriftWatch`] once-only warn bookkeeping and cure
//! re-arming.

use std::cell::Cell;
use std::path::PathBuf;

use super::*;

fn root(name: &str) -> PathBuf {
    PathBuf::from("/workspaces").join(name)
}

fn pairs(entries: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
    entries
        .iter()
        .map(|(r, nwo)| (root(r), (*nwo).to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// detect_owner_drift
// ---------------------------------------------------------------------------

#[test]
fn unchanged_remotes_are_not_drift() {
    let startup = pairs(&[("a", "Acme/widget"), ("b", "Acme/gadget")]);
    let current = pairs(&[("a", "Acme/widget"), ("b", "Acme/gadget")]);
    assert!(detect_owner_drift(&startup, &current).is_empty());
}

#[test]
fn a_repointed_root_is_drift_naming_both_owners() {
    let startup = pairs(&[("a", "Acme/widget")]);
    let current = pairs(&[("a", "Globex/widget")]);
    assert_eq!(
        detect_owner_drift(&startup, &current),
        vec![OwnerDrift {
            root: root("a"),
            startup_nwo: "Acme/widget".into(),
            current_nwo: "Globex/widget".into(),
        }]
    );
}

#[test]
fn a_case_only_owner_rename_is_the_same_owner_not_drift() {
    // GitHub owner slugs are case-insensitive, and the read route's
    // `target_owner` lowercases its answer — a case-only difference must
    // not shed reads or warn.
    let startup = pairs(&[("a", "Acme/widget")]);
    let current = pairs(&[("a", "ACME/widget")]);
    assert!(detect_owner_drift(&startup, &current).is_empty());
    assert!(!owners_differ("Acme", "ACME"));
    assert!(owners_differ("Acme", "Globex"));
}

#[test]
fn a_root_added_since_startup_is_not_drift() {
    let startup = pairs(&[("a", "Acme/widget")]);
    let current = pairs(&[("a", "Acme/widget"), ("new", "Globex/new")]);
    assert!(detect_owner_drift(&startup, &current).is_empty());
}

#[test]
fn a_root_that_vanished_or_lost_its_remote_is_not_drift() {
    let startup = pairs(&[("a", "Acme/widget"), ("gone", "Acme/gone")]);
    let current = pairs(&[("a", "Acme/widget")]);
    assert!(detect_owner_drift(&startup, &current).is_empty());
}

#[test]
fn only_the_drifted_roots_are_reported() {
    let startup = pairs(&[
        ("a", "Acme/widget"),
        ("b", "Acme/gadget"),
        ("c", "Initech/ops"),
    ]);
    let current = pairs(&[
        ("a", "Globex/widget"),
        ("b", "Acme/gadget"),
        ("c", "Initech/ops"),
    ]);
    let drift = detect_owner_drift(&startup, &current);
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert_eq!(drift[0].root, root("a"));
}

// ---------------------------------------------------------------------------
// canonical_owner_differs / detect_canonical_drift_with
// ---------------------------------------------------------------------------

#[test]
fn canonical_owner_differs_compares_case_insensitively() {
    assert!(!canonical_owner_differs("Acme/widget", "Acme/widget"));
    assert!(!canonical_owner_differs("acme/widget", "Acme/widget"));
    assert!(canonical_owner_differs("Acme/widget", "Globex/widget"));
}

#[test]
fn a_transfer_redirect_is_canonical_drift() {
    // The remote still names the old owner; the forge's `GET repos/{old}`
    // answers the new canonical `full_name` via redirect.
    let pairs = pairs(&[("a", "Old-owner/widget")]);
    let drift = detect_canonical_drift_with(&pairs, |_| Some("New-owner/widget".to_string()));
    assert_eq!(
        drift,
        vec![CanonicalDrift {
            root: root("a"),
            remote_nwo: "Old-owner/widget".into(),
            canonical_nwo: "New-owner/widget".into(),
        }]
    );
}

#[test]
fn a_matching_canonical_name_is_not_drift() {
    let pairs = pairs(&[("a", "Acme/widget")]);
    let drift = detect_canonical_drift_with(&pairs, |_| Some("Acme/widget".to_string()));
    assert!(drift.is_empty());
}

#[test]
fn a_failed_canonical_lookup_yields_no_verdict_not_a_false_drift() {
    let pairs = pairs(&[("a", "Acme/widget"), ("b", "Globex/moved")]);
    let drift = detect_canonical_drift_with(&pairs, |nwo| {
        if nwo == "Acme/widget" {
            None // forge lookup failed
        } else {
            Some("Initech/moved".to_string())
        }
    });
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert_eq!(drift[0].root, root("b"), "the failed lookup is skipped, not reported");
}

#[test]
fn one_fetch_per_distinct_nwo_shared_across_roots() {
    let calls = Cell::new(0);
    let pairs = pairs(&[("a", "Acme/widget"), ("b", "Acme/widget")]);
    let drift = detect_canonical_drift_with(&pairs, |nwo| {
        assert_eq!(nwo, "Acme/widget");
        calls.set(calls.get() + 1);
        Some("Globex/widget".to_string())
    });
    assert_eq!(calls.get(), 1, "worktrees of one repo share one fetch");
    assert_eq!(drift.len(), 2, "both roots report the same drift");
}

// ---------------------------------------------------------------------------
// DriftWatch: once-only warns, cure re-arming
// ---------------------------------------------------------------------------

fn drift_a() -> OwnerDrift {
    OwnerDrift {
        root: root("a"),
        startup_nwo: "Acme/widget".into(),
        current_nwo: "Globex/widget".into(),
    }
}

#[test]
fn the_same_pair_warns_once_never_per_tick() {
    let mut watch = DriftWatch::new(pairs(&[("a", "Acme/widget")]));
    let d = drift_a();
    let entries = vec![(owner_drift_warn_key(&d), owner_drift_warn_line(&d))];
    let first = watch.take_warn_lines(entries.clone());
    assert_eq!(first.len(), 1, "first sighting warns");
    assert!(first[0].contains("/workspaces/a"), "{}", first[0]);
    assert!(first[0].contains("Acme/widget") && first[0].contains("Globex/widget"));
    let second = watch.take_warn_lines(entries);
    assert!(second.is_empty(), "the same pair never warns again");
}

#[test]
fn distinct_pairs_warn_independently() {
    let mut watch = DriftWatch::new(pairs(&[("a", "Acme/widget"), ("b", "Acme/gadget")]));
    let a = drift_a();
    let b = OwnerDrift {
        root: root("b"),
        startup_nwo: "Acme/gadget".into(),
        current_nwo: "Initech/gadget".into(),
    };
    let one = watch.take_warn_lines(vec![(owner_drift_warn_key(&a), owner_drift_warn_line(&a))]);
    assert_eq!(one.len(), 1);
    let two = watch.take_warn_lines(vec![(owner_drift_warn_key(&b), owner_drift_warn_line(&b))]);
    assert_eq!(two.len(), 1, "a different pair still warns");
    assert!(two[0].contains("/workspaces/b"), "{}", two[0]);
}

#[test]
fn case_only_key_variants_dedup() {
    let mut watch = DriftWatch::new(pairs(&[("a", "Acme/widget")]));
    let original = drift_a();
    let recase = OwnerDrift {
        startup_nwo: "ACME/widget".into(),
        ..drift_a()
    };
    let entries = |d: &OwnerDrift| vec![(owner_drift_warn_key(d), owner_drift_warn_line(d))];
    assert_eq!(watch.take_warn_lines(entries(&original)).len(), 1);
    assert!(
        watch.take_warn_lines(entries(&recase)).is_empty(),
        "a case-only rename is the same warn key"
    );
}

#[test]
fn a_cured_root_re_arms_and_uncured_roots_keep_their_baseline() {
    let mut watch = DriftWatch::new(pairs(&[("a", "Acme/widget"), ("b", "Acme/gadget")]));
    let a = drift_a();
    let current = pairs(&[("a", "Globex/widget"), ("b", "Initech/gadget")]);

    // Both drift; both warn once.
    let first = detect_owner_drift(&watch.startup_roots, &current);
    assert_eq!(first.len(), 2);
    let entries: Vec<(WarnKey, String)> = first
        .iter()
        .map(|d| (owner_drift_warn_key(d), owner_drift_warn_line(d)))
        .collect();
    assert_eq!(watch.take_warn_lines(entries).len(), 2);

    // Only `a` is repaired: its baseline adopts the current nwo and its
    // warn key re-arms; `b` keeps its startup baseline.
    watch.cure_roots(std::slice::from_ref(&a));
    assert_eq!(
        watch.startup_roots,
        pairs(&[("a", "Globex/widget"), ("b", "Acme/gadget")]),
        "cured root adopts the current nwo, uncured keeps the startup one"
    );

    // Next tick: `b` still drifts (warned already — silent), `a` no longer
    // drifts at all against its new baseline.
    let next = detect_owner_drift(&watch.startup_roots, &current);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].root, root("b"));

    // `a` drifts AGAIN (re-pointed elsewhere): it warns again.
    let again = OwnerDrift {
        current_nwo: "Initech/widget".into(),
        ..drift_a()
    };
    watch.cure_roots(std::slice::from_ref(&again));
    let entries = vec![(owner_drift_warn_key(&again), owner_drift_warn_line(&again))];
    assert_eq!(watch.take_warn_lines(entries).len(), 1, "a cured pair's recurrence warns again");
}

#[test]
fn warn_keys_name_the_repo_and_lowercase_owners() {
    let key = owner_drift_warn_key(&drift_a());
    assert_eq!(key.0, root("a"));
    assert_eq!(key.1, "acme");
    assert_eq!(key.2, "globex");
    let line = owner_drift_warn_line(&drift_a());
    assert!(line.contains("/workspaces/a"), "{line}");
    assert!(line.contains("Acme/widget"), "{line}");
    assert!(line.contains("Globex/widget"), "{line}");
    assert!(line.contains("#14223"), "{line}");
}

#[test]
fn canonical_warn_lines_name_the_repo_the_cure_and_the_issue() {
    let d = CanonicalDrift {
        root: root("a"),
        remote_nwo: "Old-owner/widget".into(),
        canonical_nwo: "New-owner/widget".into(),
    };
    let line = canonical_drift_warn_line(&d);
    assert!(line.contains("/workspaces/a"), "{line}");
    assert!(line.contains("Old-owner/widget"), "{line}");
    assert!(line.contains("New-owner/widget"), "{line}");
    assert!(line.contains("remote set-url"), "{line}");
    assert!(line.contains("#14223"), "{line}");
    let key = canonical_drift_warn_key(&d);
    assert_eq!(key.0, root("a"));
    assert_eq!(key.1, "old-owner");
    assert_eq!(key.2, "new-owner");
}

#[test]
fn a_default_watch_detects_nothing_and_warns_nothing() {
    let mut watch = DriftWatch::default();
    assert!(watch.startup_roots.is_empty());
    assert!(watch.take_warn_lines(vec![]).is_empty());
}
