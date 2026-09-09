//! Host-affinity constraint (#7456): let an issue pin itself to a specific
//! fleet host, or a small set of hosts, whose toolchain the work needs — so
//! the work-finder skips it on every other host **before claiming it**,
//! rather than claiming, discovering the toolchain is missing, and bailing.
//!
//! # The problem this fixes
//!
//! `2AMLogic/sg13g2-vco#9` needed `openEMS` (an FDTD EM solver provisioned on
//! one fleet host only). Every dispatcher in the mesh competed for the claim
//! on equal terms, so the issue landed on a host without the toolchain 7
//! times in one day (20+ overall): each time the Builder correctly bailed
//! with "wrong host, no changes made", but only *after* the work-finder had
//! already flipped `loom:issue` → `loom:building`, spawned a sweep, and
//! burned a token draw. See `defaults/docs/daemon-reference.md`'s
//! "Host-affinity constraint" section for the full worked example.
//!
//! # Two independent conventions, ORed together
//!
//! - **Label**: `loom:host:<host-id>` — repeatable. Visible/filterable in the
//!   forge UI without opening the issue body.
//! - **Body marker**: `<!-- loom:requires-host=<host-id> -->` — repeatable,
//!   anchored the same `<!-- ... -->` way
//!   [`crate::capability::extract_capability_markers`] is (see that module's
//!   doc for the anchoring rationale this mirrors), but with an **open**
//!   value grammar rather than a closed vocabulary: a host id is whatever
//!   [`crate::sweep_registry::host_identity`] resolves to on some machine in
//!   the fleet — an operator-chosen `$LOOM_HOST_ID`, or a `$HOSTNAME`/
//!   `hostname`-binary fallback that can be mixed-case and dotted (e.g.
//!   `Roberts-MacBook-Pro.local`) — and this repo has no way to enumerate
//!   that set up front the way [`crate::capability`]'s closed vocabulary can.
//!
//! Every value from **both** sources is unioned into one set with **any-of**
//! semantics (the issue's own proposal): the constraint is satisfied on a
//! host whose identity is literally in that set. Declaring nothing at all —
//! the overwhelmingly common case, and every issue that predates this
//! feature — leaves the set empty, which [`HostConstraint::matches`] treats
//! as "runs anywhere": **zero behavior change** for issues that never
//! reference this convention.
//!
//! # Fail-closed semantics (load-bearing)
//!
//! A **non-empty** constraint must never match a host whose identity is not
//! literally in the declared set. There is no "unknown value passes" path
//! here, unlike [`crate::capability`]'s closed-vocabulary model (host ids
//! *are* the open-ended quantity this feature exists to constrain against,
//! not a fixed list this repo could validate against) — every syntactically
//! valid declared value is trusted at face value, and the one property every
//! call site must uphold is: an issue naming host A is claimable/dispatchable
//! **only** by a process whose own [`crate::sweep_registry::host_identity`]
//! is exactly `"A"`.
//!
//! Two independent enforcement points consult this module:
//!
//! 1. [`crate::work_finder`]'s autonomous tick (the AC1/AC2 fix — a
//!    non-matching host never claims the issue in the first place).
//! 2. `loom-daemon dispatch <issue>` (the AC3 fix — an explicit operator
//!    dispatch on a non-matching host refuses with a clear message unless
//!    `--ignore-host-constraint` is passed).
//!
//! The Builder-side "landed on the wrong host" bail-out (already a backstop
//! for a mislabelled issue) is unchanged by this module — see the issue's
//! own "stays as the backstop" language.

use std::collections::BTreeSet;

// ============================================================================
// Conventions
// ============================================================================

/// The label convention's prefix: `loom:host:<host-id>` — repeatable, one
/// label per required host id.
pub const HOST_LABEL_PREFIX: &str = "loom:host:";

/// The body marker's key inside the `<!-- ... -->` comment, including the
/// `=` — mirrors [`crate::capability`]'s `CAPABILITY_MARKER_KEY` naming.
const REQUIRES_HOST_MARKER_KEY: &str = "loom:requires-host=";
/// The opening delimiter of the canonical HTML-comment marker form.
const COMMENT_OPEN: &str = "<!--";
/// The closing delimiter of the canonical HTML-comment marker form.
const COMMENT_CLOSE: &str = "-->";

// ============================================================================
// The constraint
// ============================================================================

/// An issue's host-affinity constraint, as parsed from its labels and body
/// (#7456).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConstraint {
    /// Declared host ids, ORed together. Empty means "no constraint — runs
    /// anywhere", which is the default for every issue that declares
    /// nothing.
    pub required: BTreeSet<String>,
}

impl HostConstraint {
    /// True when this issue declares no host-affinity constraint at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }

    /// True when `host_id` may claim/dispatch this issue: either no
    /// constraint was declared at all, or `host_id` is literally one of the
    /// declared values. See the module doc's "Fail-closed semantics" for why
    /// there is no partial-match or case-insensitive fallback here.
    #[must_use]
    pub fn matches(&self, host_id: &str) -> bool {
        self.required.is_empty() || self.required.contains(host_id)
    }

    /// A human-readable rendering of the declared set for a skip/refusal
    /// message, e.g. `loom-worker-2` or `loom-worker-1 or loom-worker-2`.
    /// Empty only when [`Self::is_empty`] — callers only reach for this after
    /// already establishing the constraint is non-empty (a `matches` miss).
    #[must_use]
    pub fn describe(&self) -> String {
        self.required
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" or ")
    }
}

// ============================================================================
// Parsing: the `loom:host:<id>` label convention
// ============================================================================

/// Parse the `loom:host:<host-id>` label convention out of an issue's label
/// set. Every matching label contributes its suffix; a bare `loom:host:`
/// label (nothing after the prefix) contributes nothing rather than an empty
/// string.
#[must_use]
pub fn required_hosts_from_labels<S: AsRef<str>>(labels: &[S]) -> BTreeSet<String> {
    labels
        .iter()
        .filter_map(|l| l.as_ref().strip_prefix(HOST_LABEL_PREFIX))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ============================================================================
// Parsing: the `<!-- loom:requires-host=<id> -->` body marker
// ============================================================================

/// Parse every `<!-- loom:requires-host=<id> -->` marker out of an issue/PR
/// body. Mirrors [`crate::capability::extract_capability_markers`]'s
/// anchoring and dedup contract exactly (anchored to the full `<!-- ... -->`
/// comment form, line-oriented like `grep`, every match collected and
/// deduplicated since host ids are ORed) — but with an **open** value
/// grammar rather than a closed vocabulary: any non-empty run of
/// non-whitespace, non-`<`/`>` characters that stops before the closing
/// `[[:space:]]*-->` is accepted as a host id.
#[must_use]
pub fn required_hosts_from_body(body: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in body.lines() {
        let mut cursor = 0;
        while let Some(rel) = line[cursor..].find(COMMENT_OPEN) {
            let open = cursor + rel;
            let after_open = open + COMMENT_OPEN.len();
            match parse_marker_body(&line[after_open..]) {
                Some((value, consumed)) => {
                    found.insert(value.to_string());
                    cursor = after_open + consumed;
                }
                // Not a `requires-host` marker (prose, an unrelated HTML
                // comment, a `<host-id>` placeholder): resume just past this
                // `<!--`, same recovery `capability::markers_in_line` uses.
                None => cursor = after_open,
            }
        }
    }
    found
}

/// Parse `[[:space:]]*loom:requires-host=<value>[[:space:]]*-->` at the start
/// of `rest`, returning the value plus the bytes consumed through the closing
/// `-->`. `None` means the text is not a `requires-host` marker at all.
///
/// Locates the closing `-->` first (mirroring
/// [`crate::capability`]'s `parse_marker_body`) so a value ending in `-`
/// cannot swallow the delimiter's own dashes — the same delimiter-crossing
/// fix #6914 made for the capability marker.
fn parse_marker_body(rest: &str) -> Option<(&str, usize)> {
    let after_ws = rest.len() - rest.trim_start_matches(is_marker_space).len();
    if !rest[after_ws..].starts_with(REQUIRES_HOST_MARKER_KEY) {
        return None;
    }
    let value_start = after_ws + REQUIRES_HOST_MARKER_KEY.len();
    let close_at = value_start + rest[value_start..].find(COMMENT_CLOSE)?;
    let value = rest[value_start..close_at].trim_end_matches(is_marker_space);
    if !is_valid_host_id(value) {
        return None;
    }
    Some((value, close_at + COMMENT_CLOSE.len()))
}

/// `[[:space:]]` as the shell regex convention means it — the ASCII
/// whitespace class. Mirrors [`crate::capability::is_marker_space`].
fn is_marker_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{0b}' | '\u{0c}' | '\r' | '\n')
}

/// Syntax check only — never a vocabulary check, since every syntactically
/// valid value here IS a legitimate host id (see the module doc's "open
/// value grammar"). Rejects the empty string and anything containing
/// whitespace or `<`/`>`, so a `<host-id>` placeholder in prose that merely
/// quotes the marker syntax (as this module's own doc does) is never mistaken
/// for a live marker — the same placeholder-safety property #4840 established
/// for the complexity marker and #6892 reused for the capability marker.
fn is_valid_host_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| !c.is_whitespace() && c != '<' && c != '>')
}

// ============================================================================
// Combining both conventions
// ============================================================================

/// Combine the label and body conventions into one [`HostConstraint`] (#7456).
/// `body` absent (not fetched) is treated as "no body markers", never as an
/// error — the label convention alone is still fully honored.
#[must_use]
pub fn host_constraint<S: AsRef<str>>(labels: &[S], body: Option<&str>) -> HostConstraint {
    let mut required = required_hosts_from_labels(labels);
    required.extend(required_hosts_from_body(body.unwrap_or_default()));
    HostConstraint { required }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    fn hosts(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    // ---- label convention --------------------------------------------

    #[test]
    fn extracts_a_single_host_label() {
        let required =
            required_hosts_from_labels(&labels(&["loom:issue", "loom:host:loom-worker-2"]));
        assert_eq!(required, hosts(&["loom-worker-2"]));
    }

    #[test]
    fn multiple_host_labels_are_ored_and_deduplicated() {
        let required = required_hosts_from_labels(&labels(&[
            "loom:host:loom-worker-1",
            "loom:host:loom-worker-2",
            "loom:host:loom-worker-1",
        ]));
        assert_eq!(required, hosts(&["loom-worker-1", "loom-worker-2"]));
    }

    #[test]
    fn a_bare_host_label_with_nothing_after_the_prefix_contributes_nothing() {
        assert!(required_hosts_from_labels(&labels(&["loom:host:"])).is_empty());
    }

    #[test]
    fn unrelated_labels_are_ignored() {
        assert!(required_hosts_from_labels(&labels(&["loom:issue", "loom:urgent"])).is_empty());
    }

    // ---- body marker convention ---------------------------------------

    #[test]
    fn extracts_a_single_requires_host_marker() {
        let required =
            required_hosts_from_body("## Task\n\n<!-- loom:requires-host=loom-worker-2 -->\n");
        assert_eq!(required, hosts(&["loom-worker-2"]));
    }

    #[test]
    fn collects_every_marker_deduplicated() {
        let body = "<!-- loom:requires-host=loom-worker-1 -->\n\
                    text\n\
                    <!-- loom:requires-host=loom-worker-2 -->\n\
                    <!-- loom:requires-host=loom-worker-1 -->\n";
        assert_eq!(required_hosts_from_body(body), hosts(&["loom-worker-1", "loom-worker-2"]));
    }

    #[test]
    fn several_markers_on_one_line_all_match() {
        let body =
            "<!-- loom:requires-host=loom-worker-1 --><!-- loom:requires-host=loom-worker-2 -->";
        assert_eq!(required_hosts_from_body(body), hosts(&["loom-worker-1", "loom-worker-2"]));
    }

    #[test]
    fn quoted_placeholder_prose_is_not_a_live_marker() {
        // #4840's rule, inherited via `crate::capability`: a `<host-id>`
        // placeholder breaks the `-->` anchor.
        let body = "Declare it via `<!-- loom:requires-host=<host-id> -->` in the body.";
        assert!(required_hosts_from_body(body).is_empty());
    }

    #[test]
    fn bare_substring_without_the_comment_form_is_not_a_marker() {
        assert!(required_hosts_from_body("loom:requires-host=loom-worker-2").is_empty());
    }

    #[test]
    fn empty_value_is_not_a_marker() {
        assert!(required_hosts_from_body("<!-- loom:requires-host= -->").is_empty());
    }

    #[test]
    fn marker_split_across_lines_is_not_matched() {
        assert!(required_hosts_from_body("<!-- loom:requires-host=loom-worker-2\n-->").is_empty());
    }

    #[test]
    fn no_space_before_the_delimiter_matches_exactly() {
        let required = required_hosts_from_body("<!--loom:requires-host=loom-worker-2-->");
        assert_eq!(required, hosts(&["loom-worker-2"]));
    }

    #[test]
    fn mixed_case_dotted_hostnames_are_accepted() {
        // Open value grammar: host ids can be a real $HOSTNAME fallback, e.g.
        // a mixed-case, dotted macOS default.
        let required =
            required_hosts_from_body("<!-- loom:requires-host=Roberts-MacBook-Pro.local -->");
        assert_eq!(required, hosts(&["Roberts-MacBook-Pro.local"]));
    }

    #[test]
    fn unrelated_html_comments_are_ignored() {
        let body = "<!-- loom:complexity=complex -->\n<!-- just a note -->\n\
                    <!-- loom:requires-host=loom-worker-2 -->";
        assert_eq!(required_hosts_from_body(body), hosts(&["loom-worker-2"]));
    }

    // ---- combining both conventions ------------------------------------

    #[test]
    fn label_and_marker_conventions_union_into_one_any_of_set() {
        let constraint = host_constraint(
            &labels(&["loom:issue", "loom:host:loom-worker-1"]),
            Some("<!-- loom:requires-host=loom-worker-2 -->"),
        );
        assert_eq!(constraint.required, hosts(&["loom-worker-1", "loom-worker-2"]));
    }

    #[test]
    fn no_body_fetched_still_honors_the_label_convention() {
        let constraint = host_constraint(&labels(&["loom:host:loom-worker-2"]), None);
        assert_eq!(constraint.required, hosts(&["loom-worker-2"]));
    }

    #[test]
    fn no_declaration_at_all_is_the_default_empty_constraint() {
        let constraint = host_constraint(&labels(&["loom:issue"]), Some("no markers here"));
        assert!(constraint.is_empty());
    }

    // ---- matching (fail-closed semantics) -------------------------------

    #[test]
    fn an_empty_constraint_matches_every_host() {
        let constraint = HostConstraint::default();
        assert!(constraint.matches("loom-worker-1"));
        assert!(constraint.matches("anything-at-all"));
    }

    #[test]
    fn a_non_empty_constraint_matches_only_declared_hosts() {
        let constraint = host_constraint(&labels(&["loom:host:loom-worker-2"]), None);
        assert!(constraint.matches("loom-worker-2"));
        assert!(!constraint.matches("loom-worker-1"));
        assert!(!constraint.matches("mac-studio"));
    }

    #[test]
    fn any_of_semantics_match_either_declared_host() {
        let constraint =
            host_constraint(&labels(&["loom:host:loom-worker-1", "loom:host:loom-worker-2"]), None);
        assert!(constraint.matches("loom-worker-1"));
        assert!(constraint.matches("loom-worker-2"));
        assert!(!constraint.matches("mac-studio"));
    }

    #[test]
    fn matching_is_case_sensitive_and_exact() {
        // Fail-closed: no normalization rescues a near-miss.
        let constraint = host_constraint(&labels(&["loom:host:Loom-Worker-2"]), None);
        assert!(!constraint.matches("loom-worker-2"));
        assert!(constraint.matches("Loom-Worker-2"));
    }

    #[test]
    fn describe_joins_multiple_hosts_with_or() {
        let constraint =
            host_constraint(&labels(&["loom:host:loom-worker-1", "loom:host:loom-worker-2"]), None);
        assert_eq!(constraint.describe(), "loom-worker-1 or loom-worker-2");
    }

    #[test]
    fn describe_a_single_host_is_just_the_host() {
        let constraint = host_constraint(&labels(&["loom:host:loom-worker-2"]), None);
        assert_eq!(constraint.describe(), "loom-worker-2");
    }
}
