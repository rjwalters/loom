use super::*;

#[test]
fn identical_text_reports_no_change() {
    assert_eq!(unified("f.yml", "a\nb\n", "a\nb\n"), "f.yml: no change\n");
}

#[test]
fn a_single_changed_line_shows_minus_plus_with_context() {
    let before = "p1\np2\np3\np4\nc\ns1\ns2\ns3\ns4\n";
    let after = "p1\np2\np3\np4\nX\ns1\ns2\ns3\ns4\n";
    let out = unified("f.yml", before, after);
    assert!(out.starts_with("--- a/f.yml\n+++ b/f.yml\n"));
    assert!(out.contains("-c\n"));
    assert!(out.contains("+X\n"));
    // Nearby context lines are kept, lines further away are not (#9599:
    // "minimal, reviewable diff" — this is what keeps it minimal).
    assert!(out.contains(" p3\n"));
    assert!(out.contains(" p4\n"));
    assert!(out.contains(" s1\n"));
    assert!(out.contains(" s2\n"));
    assert!(!out.contains(" p1\n"));
    assert!(!out.contains(" p2\n"));
    assert!(!out.contains(" s3\n"));
    assert!(!out.contains(" s4\n"));
}

#[test]
fn a_pure_append_has_no_removed_lines() {
    let before = "a\nb\n";
    let after = "a\nb\nc\n";
    let out = unified("f.yml", before, after);
    let has_removed_content_line = out
        .lines()
        .skip(2) // past the `--- a/…` / `+++ b/…` header
        .any(|l| l.starts_with('-') && !l.starts_with("---"));
    assert!(!has_removed_content_line, "unexpected removal in: {out}");
    assert!(out.contains("+c\n"));
}

#[test]
fn every_line_differing_is_still_a_single_hunk() {
    let out = unified("f.yml", "1\n2\n", "a\nb\n");
    assert!(out.contains("-1\n"));
    assert!(out.contains("-2\n"));
    assert!(out.contains("+a\n"));
    assert!(out.contains("+b\n"));
}
