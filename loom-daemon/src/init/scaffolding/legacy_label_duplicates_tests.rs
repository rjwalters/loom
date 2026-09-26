//! Tests for `merge_labels_block`'s pre-#4187 legacy duplicate absorption
//! (issue #8875) — split into its own sibling file per
//! `.loom/docs/file-size-policy.md` rather than growing `tests.rs`, which is
//! already over the frozen threshold.

use super::*;

/// Local copy of `tests.rs`'s `SHIPPED_LABELS` fixture — a private `const` in
/// a sibling file is not visible via `use super::*`, and duplicating this
/// three-line literal is simpler than making it `pub(super)` just for this.
const SHIPPED_LABELS: &str = "# BEGIN LOOM LABELS\n# managed by Loom\n- name: loom:issue\n  color: \"3B82F6\"\n- name: loom:building\n  color: \"F59E0B\"\n# END LOOM LABELS\n";

#[test]
fn test_merge_labels_block_absorbs_legacy_duplicates_in_markerless_file() {
    // A pre-#4187 install wrote Loom's own labels unmarked, mixed in with a
    // genuine consumer label. Installing a modern marker-aware Loom over it
    // must absorb the stale copies of `loom:issue`/`loom:building` (they
    // collide by name with the shipped managed block) while preserving the
    // consumer's own `team:frontend` label untouched.
    let existing = "- name: loom:issue\n  description: \"legacy description\"\n  color: \"1d76db\"\n\n- name: team:frontend\n  color: \"00ff00\"\n  description: consumer label\n\n- name: loom:building\n  description: \"legacy building\"\n  color: \"1d76db\"\n";
    let merged = merge_labels_block(existing, SHIPPED_LABELS).unwrap();

    // Legacy duplicate content (stale color/description) must be gone.
    assert!(!merged.contains("1d76db"), "legacy duplicate colors must be absorbed");
    assert!(!merged.contains("legacy description"));
    assert!(!merged.contains("legacy building"));
    // Genuine consumer label survives untouched.
    assert!(merged.contains("- name: team:frontend"));
    assert!(merged.contains("description: consumer label"));
    // The managed block carries the current (non-duplicated) definitions.
    assert!(merged.contains("color: \"3B82F6\""));
    assert!(merged.contains("- name: loom:building"));
    // Each Loom-owned name now appears exactly once in the whole file.
    assert_eq!(merged.matches("- name: loom:issue").count(), 1);
    assert_eq!(merged.matches("- name: loom:building").count(), 1);
}

#[test]
fn test_merge_labels_block_absorbs_legacy_duplicates_outside_existing_marked_range() {
    // Less common, but handled the same way: a same-named entry sitting
    // outside an *already-marked* block (e.g. hand pasted back in after a
    // previous migration) is also absorbed rather than preserved as a
    // duplicate.
    let existing = "- name: loom:issue\n  color: \"1d76db\"\n\n# BEGIN LOOM LABELS\n- name: loom:issue\n  color: \"000000\"\n# END LOOM LABELS\n\n- name: team:below\n  color: \"222222\"\n";
    let merged = merge_labels_block(existing, SHIPPED_LABELS).unwrap();

    assert!(!merged.contains("1d76db"));
    assert!(merged.contains("- name: team:below"));
    assert!(merged.contains("color: \"3B82F6\""));
    assert_eq!(merged.matches("- name: loom:issue").count(), 1);
}

#[test]
fn test_strip_legacy_loom_label_entries_preserves_non_colliding_content() {
    let loom_names: std::collections::HashSet<&str> = ["loom:issue"].into_iter().collect();
    let text = "# a comment\n- name: team:only\n  color: \"abcdef\"\n\n- name: loom:issue\n  color: \"1d76db\"\n";
    let stripped = strip_legacy_loom_label_entries(text, &loom_names);
    assert!(stripped.contains("# a comment"));
    assert!(stripped.contains("- name: team:only"));
    assert!(!stripped.contains("loom:issue"));
}
