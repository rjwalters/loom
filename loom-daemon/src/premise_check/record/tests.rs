//! The record's consistency rules — the gate's actual enforcement.

use super::*;
use std::fs;

/// A throwaway tree with the two citation targets the tests use.
fn fixture_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/watchdog.rs"), "// no automatic restart\n").unwrap();
    fs::create_dir_all(dir.path().join("docs/adr")).unwrap();
    fs::write(dir.path().join("docs/adr/0009-shepherd.md"), "# ADR 9\n").unwrap();
    dir
}

fn marker(fields: &str) -> String {
    format!("<!-- loom:premise-check {fields} -->\n")
}

#[test]
fn a_prose_mention_is_not_a_marker() {
    // premise-gate.md and this very file name the marker in prose; neither may
    // ever be read as a live record.
    assert!(last_chunk_with_marker(&["see the loom:premise-check marker"]).is_none());
    assert!(parse("the loom:premise-check record goes in a comment").is_err());
}

#[test]
fn the_last_marker_wins() {
    let a = marker("exists=yes deliberate=no reversal=no verdict=clear");
    let b = marker("exists=yes deliberate=yes reversal=yes verdict=operator-decision");
    let chunks = vec![a.as_str(), "unrelated comment", b.as_str()];
    assert_eq!(last_chunk_with_marker(&chunks), Some(b.as_str()));
}

#[test]
fn missing_keys_are_malformed() {
    for fields in [
        "deliberate=no reversal=no verdict=clear",
        "exists=yes reversal=no verdict=clear",
        "exists=yes deliberate=no verdict=clear",
        "exists=yes deliberate=no reversal=no",
    ] {
        assert!(parse(&marker(fields)).is_err(), "{fields}");
    }
}

/// A typo must fail loudly rather than default to "absent" — a silently
/// ignored key is a gate that disarms itself.
#[test]
fn an_unknown_key_is_rejected() {
    let e = parse(&marker("exists=yes delibrate=no reversal=no verdict=clear")).unwrap_err();
    assert!(e.contains("delibrate"), "{e}");
}

#[test]
fn bad_values_are_rejected() {
    assert!(parse(&marker("exists=maybe deliberate=no reversal=no verdict=clear")).is_err());
    assert!(parse(&marker("exists=yes deliberate=true reversal=no verdict=clear")).is_err());
    assert!(parse(&marker("exists=yes deliberate=no reversal=no verdict=ship-it")).is_err());
}

// ---- the load-bearing rule -------------------------------------------------

/// #7855, reconstructed: deliberate, and the issue reverses it. No record that
/// lets curation proceed can be written.
#[test]
fn deliberate_reversal_cannot_be_cleared() {
    let root = fixture_root();
    let chunk = format!(
        "{}premise-evidence: src/watchdog.rs:1 — no automatic kill/restart\n",
        marker("exists=yes deliberate=yes reversal=yes verdict=clear")
    );
    let rec = parse(&chunk).expect("parses");
    match check(&rec, root.path()) {
        Outcome::Malformed(why) => {
            assert!(why.contains("verdict=operator-decision"), "{why}");
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn deliberate_reversal_routed_to_the_operator_is_accepted() {
    let root = fixture_root();
    let chunk = format!(
        "{}premise-evidence: src/watchdog.rs:1 — no automatic kill/restart\n",
        marker("exists=yes deliberate=yes reversal=yes verdict=operator-decision")
    );
    let rec = parse(&chunk).unwrap();
    assert_eq!(check(&rec, root.path()), Outcome::RouteOperator);
}

/// Routing to a human is never refused. Only the permissive direction is
/// constrained.
#[test]
fn routing_without_a_deliberateness_finding_is_allowed() {
    let root = fixture_root();
    let chunk = format!(
        "{}premise-searched: src/watchdog.rs\n",
        marker("exists=yes deliberate=no reversal=no verdict=operator-decision")
    );
    let rec = parse(&chunk).unwrap();
    assert_eq!(check(&rec, root.path()), Outcome::RouteOperator);
}

// ---- citations -------------------------------------------------------------

#[test]
fn deliberate_yes_needs_a_resolving_citation() {
    let root = fixture_root();
    let none =
        parse(&marker("exists=yes deliberate=yes reversal=no verdict=operator-decision")).unwrap();
    assert!(
        matches!(check(&none, root.path()), Outcome::Malformed(w) if w.contains("premise-evidence"))
    );

    let bogus = parse(&format!(
        "{}premise-evidence: src/nope.rs:12 — allegedly\n",
        marker("exists=yes deliberate=yes reversal=no verdict=operator-decision")
    ))
    .unwrap();
    assert!(
        matches!(check(&bogus, root.path()), Outcome::Malformed(w) if w.contains("does not resolve"))
    );
}

/// #8310 was closed `premise-false` for exactly this: a citation to a file
/// that no longer held what it claimed. A locator that does not resolve at all
/// is the cheap half of that check, and it is mechanical.
#[test]
fn citation_forms_that_resolve() {
    let root = fixture_root();
    for c in [
        "src/watchdog.rs",
        "src/watchdog.rs:12",
        "src/watchdog.rs:12-40",
        "./src/watchdog.rs",
        "`src/watchdog.rs`",
        "ADR-0009",
        "ADR-9",
        "adr-0009",
    ] {
        assert!(resolves(c, root.path()), "{c}");
    }
    for c in ["src/gone.rs", "/etc/passwd", "../escape.rs", "ADR-0404", ""] {
        assert!(!resolves(c, root.path()), "{c}");
    }
}

#[test]
fn deliberate_no_must_show_what_was_read() {
    let root = fixture_root();
    let bare = parse(&marker("exists=yes deliberate=no reversal=no verdict=clear")).unwrap();
    assert!(
        matches!(check(&bare, root.path()), Outcome::Malformed(w) if w.contains("premise-searched"))
    );

    let shown = parse(&format!(
        "{}premise-searched: src/watchdog.rs\n",
        marker("exists=yes deliberate=no reversal=no verdict=clear")
    ))
    .unwrap();
    assert_eq!(check(&shown, root.path()), Outcome::Proceed);
}

#[test]
fn deliberate_but_not_reversing_needs_the_extends_line() {
    let root = fixture_root();
    let without = parse(&format!(
        "{}premise-evidence: src/watchdog.rs\n",
        marker("exists=yes deliberate=yes reversal=no verdict=clear")
    ))
    .unwrap();
    assert!(
        matches!(check(&without, root.path()), Outcome::Malformed(w) if w.contains("premise-extends"))
    );

    let with = parse(&format!(
        "{}premise-evidence: src/watchdog.rs\npremise-extends: adds a report line, keeps the no-restart posture\n",
        marker("exists=yes deliberate=yes reversal=no verdict=clear")
    ))
    .unwrap();
    assert_eq!(check(&with, root.path()), Outcome::Proceed);
}

#[test]
fn an_empty_extends_line_does_not_count() {
    let root = fixture_root();
    let rec = parse(&format!(
        "{}premise-evidence: src/watchdog.rs\npremise-extends:   \n",
        marker("exists=yes deliberate=yes reversal=no verdict=clear")
    ))
    .unwrap();
    assert!(matches!(check(&rec, root.path()), Outcome::Malformed(_)));
}

// ---- exists ----------------------------------------------------------------

#[test]
fn premise_false_is_its_own_outcome() {
    let root = fixture_root();
    let rec = parse(&format!(
        "{}premise-searched: src/watchdog.rs\n",
        marker("exists=no deliberate=no reversal=no verdict=clear")
    ))
    .unwrap();
    assert_eq!(check(&rec, root.path()), Outcome::PremiseFalse);
}

#[test]
fn nonexistent_behaviour_cannot_be_deliberate() {
    let root = fixture_root();
    let rec = parse(&format!(
        "{}premise-evidence: src/watchdog.rs\n",
        marker("exists=no deliberate=yes reversal=yes verdict=operator-decision")
    ))
    .unwrap();
    assert!(matches!(check(&rec, root.path()), Outcome::Malformed(_)));
}

// ---- companion-line shapes -------------------------------------------------

#[test]
fn companion_lines_tolerate_list_bullets_and_backticks() {
    let chunk = format!(
        "{}- premise-evidence: `src/watchdog.rs:12` — the rationale\n  * premise-searched: docs/adr/0009-shepherd.md\n",
        marker("exists=yes deliberate=yes reversal=yes verdict=operator-decision")
    );
    let rec = parse(&chunk).unwrap();
    assert_eq!(rec.evidence, vec!["src/watchdog.rs:12".to_string()]);
    assert_eq!(rec.searched, vec!["docs/adr/0009-shepherd.md".to_string()]);
}

#[test]
fn cited_paths_strips_line_suffixes() {
    let chunk = format!(
        "{}premise-evidence: src/watchdog.rs:902 — x\npremise-searched: docs/adr/0009-shepherd.md\n",
        marker("exists=yes deliberate=yes reversal=yes verdict=operator-decision")
    );
    let rec = parse(&chunk).unwrap();
    let paths = cited_paths(&rec);
    assert!(paths.contains("src/watchdog.rs"), "{paths:?}");
    assert!(paths.contains("docs/adr/0009-shepherd.md"), "{paths:?}");
}
