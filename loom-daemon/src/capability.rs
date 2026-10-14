//! Capability declarations for the capability dispatch lane (#6885 Part 2,
//! issue #6893; re-keyed on the body by #11795, onto `loom:blocked` by #11861).
//!
//! # What this module is for
//!
//! `loom:blocked` is a **park**: every dispatch route refuses the item
//! ([`crate::work_finder::PARK_LABELS`]). Some blocked items wait on no other
//! change, only on access a host may hold — a credential, root on a host.
//! Such an item is told apart by its body, not a sub-label: it declares the
//! access it needs in `<!-- loom:capability=<name> -->` markers, names **no**
//! blocker in a park record (#8925) and carries **no** ```` ```decision ````
//! block (a decision is a ruling, never mechanical). It is what the retired
//! `loom:needs-capability` folded into (#11795).
//!
//! Before #11861 the lane read `loom:operator` + a marker. `loom:operator` is
//! now a no-op for every role and only a human removes it, so an operator
//! hold never enters the lane: it vetoes it, like `loom:frozen`.
//!
//! This module supplies the two halves of the check that lets a worker which
//! actually holds the needed access attempt such an item instead:
//!
//! 1. **The item's side** — [`extract_capability_markers`] parses the
//!    `<!-- loom:capability=<name> -->` markers an item declares in its body
//!    (the Part 1 convention, #6892).
//! 2. **The worker's side** — [`held_capabilities`] reads what *this host*
//!    declares it holds, from the environment.
//!
//! [`WorkItem::mechanical_routing`](crate::work_finder::WorkItem::mechanical_routing)
//! combines them into a routing decision; nothing here touches the forge.
//!
//! # Safety properties (all four are load-bearing)
//!
//! - **Fail-closed by default.** An empty held set (no `LOOM_WORKER_CAPABILITIES`
//!   in the environment — the default on every host) makes every mechanical item
//!   park exactly as it does today. This module is a no-op until a host opts in.
//! - **Fail-closed on anything unrecognized.** An item declaring an unknown or
//!   misspelled capability is exactly as undispatchable as one declaring nothing
//!   (see [`CapabilityDeclaration::is_satisfied_by`]). The same rule applies
//!   symmetrically to a *worker's* declared holds: an unknown value in
//!   `LOOM_WORKER_CAPABILITIES` grants nothing rather than being trusted.
//! - **Declared by the host, never by the repo.** The held set is read **only**
//!   from the process environment, deliberately *not* from `.loom/config.json`.
//!   A capability is a property of the machine and its credentials; a config file
//!   committed to git must never be able to assert that the host running it has
//!   root, an admin token, or a production cloud profile.
//! - **Propose-only.** Satisfying a declaration un-parks the item into
//!   [`MechanicalRouting::ProposeDispatch`] — a dry-run/propose lane whose
//!   contract is "produce the exact commands (or a PR) for an operator to
//!   approve". There is deliberately **no live-execution variant** (#6893 AC4);
//!   adding one is a separate, explicit opt-in and a deliberate edit to
//!   [`MechanicalRouting`].
//!
//! # Parser contract
//!
//! Mirrors `defaults/scripts/extract-capability-markers.sh` — the reference
//! implementation named by `defaults/docs/label-state-machine.md`
//! ("Capability-declaration convention") — so the Rust dispatch side and the
//! shell/markdown-orchestration side cannot silently diverge on what counts as
//! a valid marker:
//!
//! ```text
//! grep -oE '<!--[[:space:]]*loom:capability=[a-z0-9][a-z0-9:_-]*[[:space:]]*-->'
//! ```
//!
//! Two properties are reproduced exactly:
//!
//! 1. **Anchored to the full `<!-- ... -->` comment form**, never a bare
//!    substring — the #4840 anchoring rule: prose that merely *quotes* the
//!    marker syntax as literal example text (as the convention doc itself
//!    does) must never be mistaken for a live marker. A `<name>` placeholder breaks the `-->`
//!    anchor and is therefore skipped.
//! 2. **Every match is collected, deduplicated** — the whole declared set
//!    matters because multiple markers are **ANDed**, so there is no
//!    last-match-wins.
//!
//! Matching is line-oriented, like `grep`: a marker split across a newline is
//! recognized by neither implementation.

use std::collections::BTreeSet;

// ============================================================================
// Closed vocabulary
// ============================================================================

/// Environment variable naming the capabilities **this host/worker declares it
/// holds** — a comma- and/or whitespace-separated list, e.g.
/// `LOOM_WORKER_CAPABILITIES="host-sudo,cloud-profile:prod-aws"`.
///
/// Deliberately environment-only (see the module docs' "Declared by the host,
/// never by the repo"): a committed config file must not be able to claim the
/// machine running it has root or a production credential. Unset or empty — the
/// default everywhere — means "no capabilities", which keeps every
/// mechanical operator hold parked exactly as before this module existed.
pub const WORKER_CAPABILITIES_ENV: &str = "LOOM_WORKER_CAPABILITIES";

/// Closed vocabulary, literal values. Kept in sync with the table in
/// `defaults/docs/label-state-machine.md` ("Capability-declaration convention")
/// and with `defaults/scripts/extract-capability-markers.sh`'s `KNOWN_LITERALS`.
/// Extend by adding a value in **all three** places, never by an item inventing
/// its own.
pub const KNOWN_CAPABILITY_LITERALS: &[&str] =
    &["host-sudo", "forge-admin-token", "tailnet-access"];

/// Closed vocabulary, colon-parameterized families (`cloud-profile:<name>`).
/// Mirrors `extract-capability-markers.sh`'s `KNOWN_PREFIXES`: the prefix alone
/// is **not** a valid value — something must follow the colon.
pub const KNOWN_CAPABILITY_PREFIXES: &[&str] = &["cloud-profile:"];

/// The retired judgement sub-labels (#11795). An unmigrated item carrying one
/// is a decision (or a wait on a human) by label, which vetoes the lane. Since
/// #11861 every operator-hold name vetoes it anyway (they are skip labels);
/// kept explicit for the migration window.
const LEGACY_JUDGEMENT_LABELS: [&str; 2] = ["loom:operator-decision", "loom:operator-blocked"];

/// Whether `label` vetoes the capability lane outright, even next to a
/// well-formed declaration: any skip label other than the blocked hold itself
/// (under any of its names) and the daemon's own claim, plus the legacy
/// judgement sub-labels.
///
/// Derived from [`crate::work_finder::SKIP_LABELS`], so it covers
/// `loom:operator` and its retired names (#11861: a human must act, so no role
/// does), `loom:frozen` ("don't touch"), `loom:decision-malformed` (a
/// decision under repair) and `loom:epic`. `loom:building` is not a veto: the
/// dispatch guard sees it on the daemon's own re-dispatch of a lane item.
fn is_lane_veto(label: &str) -> bool {
    use crate::work_finder::{BLOCKED_HOLD_NAMES, BUILDING_LABEL, SKIP_LABELS};
    LEGACY_JUDGEMENT_LABELS.contains(&label)
        || (SKIP_LABELS.contains(&label)
            && label != BUILDING_LABEL
            && !BLOCKED_HOLD_NAMES.contains(&label))
}

/// True when `value` is in the closed vocabulary above.
///
/// Everything else — a typo, a value someone invented locally, a
/// `cloud-profile:` with nothing after the colon — is **unknown**, and unknown
/// always fails closed (never "assume satisfied", never "silently drop").
#[must_use]
pub fn is_known_capability(value: &str) -> bool {
    KNOWN_CAPABILITY_LITERALS.contains(&value)
        || KNOWN_CAPABILITY_PREFIXES
            .iter()
            .any(|prefix| value.len() > prefix.len() && value.starts_with(prefix))
}

// ============================================================================
// Marker parsing
// ============================================================================

/// The marker's key inside the `<!-- ... -->` comment, including the `=`.
const CAPABILITY_MARKER_KEY: &str = "loom:capability=";
/// The opening delimiter of the canonical HTML-comment marker form.
const COMMENT_OPEN: &str = "<!--";
/// The closing delimiter of the canonical HTML-comment marker form.
const COMMENT_CLOSE: &str = "-->";

/// What an item's body declares it needs, as parsed by
/// [`extract_capability_markers`].
///
/// The `unknown` set is kept rather than discarded on purpose: it is the
/// difference between "this item declares nothing" and "this item declares
/// something we could not resolve", and only the latter is worth telling an
/// operator about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityDeclaration {
    /// Valid, deduplicated capabilities the item declares. **ANDed** — a worker
    /// must hold every one of them.
    pub required: BTreeSet<String>,
    /// Marker values that parsed as markers but are not in the closed
    /// vocabulary. Any entry here makes the whole declaration unsatisfiable
    /// (fail-closed), no matter what `required` contains.
    pub unknown: BTreeSet<String>,
}

impl CapabilityDeclaration {
    /// True when the body carried no capability marker at all — neither valid
    /// nor unknown. This is the common case for almost every item, including
    /// most mechanical operator holds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.unknown.is_empty()
    }

    /// True when `held` satisfies this declaration and the item may therefore
    /// leave the park.
    ///
    /// Three ways to be unsatisfied, all deliberate:
    ///
    /// 1. **Nothing declared** (`required` empty) — an item that names no
    ///    capability gives no evidence any worker can do it, so it stays parked.
    ///    "No requirement" is emphatically not "no requirement to *check*".
    /// 2. **Anything unknown declared** — fail-closed per the convention doc: a
    ///    typo'd value is exactly as undispatchable as no value.
    /// 3. **Some required capability not held** — all markers are ANDed.
    #[must_use]
    pub fn is_satisfied_by(&self, held: &BTreeSet<String>) -> bool {
        !self.required.is_empty() && self.unknown.is_empty() && self.required.is_subset(held)
    }

    /// The declared capabilities `held` does **not** cover, sorted — what a
    /// "you are missing X" message should name. Unknown values are reported
    /// separately via [`Self::unknown`]; they are missing in a different sense
    /// (nobody can hold them) and conflating the two would send an operator
    /// hunting for a credential that does not exist.
    #[must_use]
    pub fn missing_from(&self, held: &BTreeSet<String>) -> Vec<String> {
        self.required.difference(held).cloned().collect()
    }
}

/// Parse every `<!-- loom:capability=<value> -->` marker out of an issue/PR
/// body, partitioned into known and unknown values.
///
/// See the module docs for the exact contract this mirrors
/// (`defaults/scripts/extract-capability-markers.sh`). An empty/absent body
/// yields an empty declaration, which is never an error — it just is not
/// dispatchable.
#[must_use]
pub fn extract_capability_markers(body: &str) -> CapabilityDeclaration {
    let mut decl = CapabilityDeclaration::default();
    for line in body.lines() {
        for value in markers_in_line(line) {
            if is_known_capability(value) {
                decl.required.insert(value.to_string());
            } else {
                decl.unknown.insert(value.to_string());
            }
        }
    }
    decl
}

/// Every `<!-- loom:capability=<value> -->` value on a single line, in
/// left-to-right, non-overlapping order — the same match stream `grep -o`
/// produces.
fn markers_in_line(line: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = line[cursor..].find(COMMENT_OPEN) {
        let open = cursor + rel;
        let after_open = open + COMMENT_OPEN.len();
        match parse_marker_body(&line[after_open..]) {
            Some((value, consumed)) => {
                found.push(value);
                cursor = after_open + consumed;
            }
            // Not a marker comment (prose, an unrelated HTML comment, a
            // `<name>` placeholder): resume just past this `<!--`.
            None => cursor = after_open,
        }
    }
    found
}

/// Parse `[[:space:]]*loom:capability=[a-z0-9][a-z0-9:_-]*[[:space:]]*-->` at
/// the start of `rest`, returning the value plus the bytes consumed through the
/// closing `-->`. `None` means the text is not a capability marker at all.
///
/// Mirrors the reference's **two** steps, anchored identically (#6914) so the
/// two surfaces cannot disagree about whether an item is dispatchable:
///
/// 1. **Recognition** — `grep -oE '<!--…-->'` decides whether this is a marker
///    at all. `-` is both a legal value character and the head of the closing
///    delimiter, so a greedy value scan of `tailnet-access-->` eats the `--` and
///    then cannot find its own terminator; POSIX ERE's leftmost-longest matching
///    backtracks out of that, and locating the `-->` first (`close_at`, below)
///    is the equivalent.
/// 2. **Extraction** — `sed -E
///    's/.*capability=([a-z0-9][a-z0-9:_-]*)[[:space:]]*-->.*/\1/'` — anchoring
///    the capture on the same `[[:space:]]*-->` terminator recognition already
///    located means the capture cannot cross into the delimiter's own dashes.
///    The trimmed span between `capability=` and `close_at` (computed once, for
///    both validation and extraction below) is exactly that anchored capture's
///    result: on `tailnet-access-->` it yields `tailnet-access`, not
///    `tailnet-access--`. A genuinely dash-suffixed value like `host-sudo-`
///    (distinct from the known literal `host-sudo`) still extracts as
///    `host-sudo-` and still fails closed as unknown — the anchor fixes the
///    delimiter-crossing bug without loosening the closed vocabulary.
///
/// The grammar's leading `[a-z0-9]` is a required first character (not a star),
/// so `<!-- loom:capability= -->` matches nothing at all — exactly as the shell
/// regex behaves.
fn parse_marker_body(rest: &str) -> Option<(&str, usize)> {
    let after_ws = rest.len() - rest.trim_start_matches(is_marker_space).len();
    if !rest[after_ws..].starts_with(CAPABILITY_MARKER_KEY) {
        return None;
    }
    let value_start = after_ws + CAPABILITY_MARKER_KEY.len();
    // Step 1: recognition.
    let close_at = value_start + rest[value_start..].find(COMMENT_CLOSE)?;
    let value = rest[value_start..close_at].trim_end_matches(is_marker_space);
    if !is_valid_marker_value(value) {
        return None;
    }
    // Step 2: extraction — anchored on the same `close_at` terminator step 1
    // already located, so it cannot run past it into the delimiter's dashes.
    Some((value, close_at + COMMENT_CLOSE.len()))
}

/// `[[:space:]]` as the shell regex means it — the ASCII whitespace class.
fn is_marker_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{0b}' | '\u{0c}' | '\r' | '\n')
}

/// The value grammar `[a-z0-9][a-z0-9:_-]*` — lowercase alphanumerics with
/// `:`/`_`/`-` separators, and never a separator first.
///
/// This is *syntax*, distinct from [`is_known_capability`]'s *vocabulary*: text
/// failing here is not a marker at all (grep would not match it either), whereas
/// a syntactically valid but unrecognized value IS a marker and fails closed as
/// an `unknown`.
fn is_valid_marker_value(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(is_value_class)
}

/// The value grammar's repeated character class, `[a-z0-9:_-]`.
fn is_value_class(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '_' | '-')
}

// ============================================================================
// Worker-side declaration
// ============================================================================

/// The capabilities this host declares it holds, read from
/// [`WORKER_CAPABILITIES_ENV`].
///
/// Empty (the default — the variable unset, empty, or naming only unrecognized
/// values) means every mechanical operator hold stays parked, i.e. **zero
/// behavior change** from before this lane existed.
#[must_use]
pub fn held_capabilities() -> BTreeSet<String> {
    std::env::var(WORKER_CAPABILITIES_ENV)
        .ok()
        .map(|raw| parse_held_capabilities(&raw))
        .unwrap_or_default()
}

/// Parse a `LOOM_WORKER_CAPABILITIES` value into the set of capabilities this
/// worker may be credited with.
///
/// Separators are commas and/or whitespace. **Unknown values are dropped**, not
/// carried: the convention doc requires the fail-closed rule to apply
/// symmetrically to the worker's own declaration, so a typo'd `host-sud` grants
/// nothing rather than accidentally matching a typo'd requirement.
#[must_use]
pub fn parse_held_capabilities(raw: &str) -> BTreeSet<String> {
    raw.split([',', ' ', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| is_known_capability(s))
        .map(str::to_string)
        .collect()
}

// ============================================================================
// Label eligibility
// ============================================================================

/// True when `labels` describe an item the capability lane may even *consider*.
///
/// Requires the blocked hold ([`crate::work_finder::is_blocked_hold`], so an
/// unmigrated `loom:needs-capability` item qualifies) and refuses the item
/// outright if any label is a lane veto (see `is_lane_veto`), which includes
/// the operator hold under every name (#11861). The body is checked separately
/// by [`route_mechanical`]: a capability marker is required, and a decision
/// block or a park record naming a blocker refuses, so a dependency park or a
/// Champion merge-risk hold, which carry no marker, can never enter the lane.
///
/// This is a pure predicate over label names: no forge I/O, no body read.
#[must_use]
pub fn labels_eligible_for_capability_lane<S: AsRef<str>>(labels: &[S]) -> bool {
    crate::work_finder::is_blocked_hold(labels) && !labels.iter().any(|l| is_lane_veto(l.as_ref()))
}

// ============================================================================
// Routing decision
// ============================================================================

/// How the capability lane routes one item.
///
/// **AC4 (#6893): there is no live-execution variant, on purpose.** The only
/// variant that lets an item leave the park is
/// [`ProposeDispatch`](Self::ProposeDispatch), whose contract is dry-run /
/// propose-mode — produce the exact commands (or a PR) for an operator to
/// review, never execute against credentials. Gating a future live mode is an
/// explicit, separate opt-in; adding a variant here is the deliberate edit that
/// would represent it, and the exhaustive `match` in this module's tests will
/// fail to compile until that decision is made consciously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MechanicalRouting {
    /// Not a capability-lane item at all — the labels do not describe a
    /// blocked hold the lane may consider (see
    /// [`labels_eligible_for_capability_lane`]), or the body carries a
    /// decision block or a park record naming a blocker (it waits on another
    /// change, which no capability satisfies). The item's normal skip/park
    /// behavior applies, completely unchanged.
    NotApplicable,
    /// Eligible labels, but the body declares no resolvable capability — either
    /// no marker at all, or at least one value outside the closed vocabulary.
    /// **Stays parked**, silently, exactly as today: there is nothing to ask an
    /// operator for and nothing a worker could claim to hold.
    NoDeclaration {
        /// Unrecognized values found, if any — non-empty distinguishes "declared
        /// a typo" from "declared nothing", which is worth logging even though
        /// both park.
        unknown: Vec<String>,
    },
    /// Eligible labels and a well-formed declaration, but this worker does not
    /// hold every declared capability. **Stays parked** — and, per AC1, the
    /// caller should turn the park into a *capability request* by naming the
    /// missing capabilities rather than stalling silently.
    MissingCapabilities {
        /// The declared capabilities this worker does not hold, sorted.
        missing: Vec<String>,
    },
    /// Eligible labels, well-formed declaration, every capability held: the item
    /// may leave the park and be worked **in propose mode only** — see this
    /// enum's AC4 note.
    ProposeDispatch,
}

impl MechanicalRouting {
    /// True only for [`ProposeDispatch`](Self::ProposeDispatch) — i.e. "this
    /// item is exempt from the `loom:blocked` park". Every other variant
    /// leaves the park in force.
    #[must_use]
    pub fn is_dispatchable(&self) -> bool {
        matches!(self, Self::ProposeDispatch)
    }
}

/// Decide how the capability lane routes an item, given its labels, its body,
/// and the capabilities this worker holds.
///
/// Pure: no environment read (callers pass `held` explicitly, so the decision is
/// testable and a tick reads the environment once rather than per candidate),
/// no forge I/O.
#[must_use]
pub fn route_mechanical<S: AsRef<str>>(
    labels: &[S],
    body: Option<&str>,
    held: &BTreeSet<String>,
) -> MechanicalRouting {
    if !labels_eligible_for_capability_lane(labels)
        || body.is_some_and(|b| {
            crate::operator_decision::render::has_decision_block(b)
                || !crate::park_record::blockers(b).is_empty()
        })
    {
        return MechanicalRouting::NotApplicable;
    }
    // No body fetched is not "no declaration we can trust" — it is "we could not
    // read the declaration", which fails closed the same way.
    let decl = extract_capability_markers(body.unwrap_or_default());
    if !decl.unknown.is_empty() || decl.required.is_empty() {
        return MechanicalRouting::NoDeclaration {
            unknown: decl.unknown.into_iter().collect(),
        };
    }
    let missing = decl.missing_from(held);
    if missing.is_empty() {
        MechanicalRouting::ProposeDispatch
    } else {
        MechanicalRouting::MissingCapabilities { missing }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    // ---- vocabulary ------------------------------------------------------

    #[test]
    fn closed_vocabulary_accepts_documented_values() {
        for value in ["host-sudo", "forge-admin-token", "tailnet-access"] {
            assert!(is_known_capability(value), "{value} is documented");
        }
        assert!(is_known_capability("cloud-profile:prod-aws"));
        assert!(is_known_capability("cloud-profile:x"));
    }

    #[test]
    fn closed_vocabulary_fails_closed_on_anything_else() {
        for value in [
            "host-sud",       // typo
            "root",           // invented
            "cloud-profile:", // family prefix with no name
            "cloud-profile",  // family prefix without the colon
            "",               // empty
            "HOST-SUDO",      // wrong case (grammar is lowercase-only)
        ] {
            assert!(!is_known_capability(value), "{value:?} must be unknown");
        }
    }

    // ---- marker parsing --------------------------------------------------

    #[test]
    fn extracts_a_single_marker() {
        let decl = extract_capability_markers("## Task\n\n<!-- loom:capability=host-sudo -->\n");
        assert_eq!(decl.required, held(&["host-sudo"]));
        assert!(decl.unknown.is_empty());
        assert!(!decl.is_empty());
    }

    #[test]
    fn collects_every_marker_deduplicated() {
        // Capability markers are ANDed, so all of them matter and repeats
        // collapse.
        let body = "<!-- loom:capability=host-sudo -->\n\
                    text\n\
                    <!-- loom:capability=cloud-profile:prod-aws -->\n\
                    <!-- loom:capability=host-sudo -->\n";
        let decl = extract_capability_markers(body);
        assert_eq!(decl.required, held(&["cloud-profile:prod-aws", "host-sudo"]));
    }

    #[test]
    fn several_markers_on_one_line_all_match() {
        let decl = extract_capability_markers(
            "<!-- loom:capability=host-sudo --><!-- loom:capability=tailnet-access -->",
        );
        assert_eq!(decl.required, held(&["host-sudo", "tailnet-access"]));
    }

    #[test]
    fn quoted_placeholder_prose_is_not_a_live_marker() {
        // #4840's rule, inherited: the convention doc (and the issue that
        // introduced it) quote the syntax as example text. A `<name>`
        // placeholder breaks the `-->` anchor.
        let decl = extract_capability_markers(
            "Declare it via `<!-- loom:capability=<name> -->` in the body.",
        );
        assert!(decl.is_empty(), "{decl:?}");
    }

    #[test]
    fn bare_substring_without_the_comment_form_is_not_a_marker() {
        let decl = extract_capability_markers("loom:capability=host-sudo");
        assert!(decl.is_empty(), "{decl:?}");
    }

    #[test]
    fn empty_value_is_not_a_marker() {
        assert!(extract_capability_markers("<!-- loom:capability= -->").is_empty());
    }

    #[test]
    fn marker_split_across_lines_is_not_matched() {
        // grep is line-oriented; so is this parser.
        assert!(extract_capability_markers("<!-- loom:capability=host-sudo\n-->").is_empty());
    }

    #[test]
    fn no_space_before_the_delimiter_matches_exactly_as_the_reference_does() {
        // Cross-checked byte-for-byte against
        // `defaults/scripts/extract-capability-markers.sh` (#6914): both sides'
        // extraction is now anchored on the same `[[:space:]]*-->` terminator
        // their recognition step already located, so a value abutting `-->`
        // with no space no longer picks up the delimiter's own dashes.
        let decl = extract_capability_markers("<!--loom:capability=tailnet-access-->");
        assert_eq!(decl.required, held(&["tailnet-access"]));
        assert!(decl.unknown.is_empty());
        assert!(decl.is_satisfied_by(&held(&["tailnet-access"])));
        // The documented, spaced form behaves identically.
        assert_eq!(
            extract_capability_markers("<!-- loom:capability=tailnet-access -->").required,
            held(&["tailnet-access"])
        );
    }

    #[test]
    fn a_genuinely_dash_suffixed_value_fails_closed_as_unknown() {
        let decl = extract_capability_markers("<!-- loom:capability=host-sudo- -->");
        assert_eq!(decl.unknown, held(&["host-sudo-"]));
        assert!(decl.required.is_empty());
    }

    #[test]
    fn a_genuinely_dash_suffixed_value_with_no_space_still_fails_closed() {
        // Distinct from the no-space case above: here the trailing dash is
        // part of the *declared value* itself, not the closing delimiter, so
        // the anchor must extract exactly `host-sudo-` (one dash) rather than
        // either swallowing it into `host-sudo` (which would be a false
        // positive) or over-capturing extra delimiter dashes (#6914).
        let decl = extract_capability_markers("<!--loom:capability=host-sudo--->");
        assert_eq!(decl.unknown, held(&["host-sudo-"]));
        assert!(decl.required.is_empty());
    }

    #[test]
    fn a_syntactically_invalid_value_is_not_a_marker_at_all() {
        // Distinct from "unknown": text that fails the *grammar* never becomes a
        // marker, exactly as grep would not match it.
        for body in [
            "<!-- loom:capability=Host-Sudo -->", // uppercase
            "<!-- loom:capability=-leading-dash -->",
            "<!-- loom:capability= host-sudo -->", // space between `=` and value
        ] {
            let decl = extract_capability_markers(body);
            assert!(decl.is_empty(), "{body:?} -> {decl:?}");
        }
    }

    #[test]
    fn unknown_values_are_partitioned_not_dropped() {
        let decl = extract_capability_markers(
            "<!-- loom:capability=host-sudo -->\n<!-- loom:capability=root-access -->",
        );
        assert_eq!(decl.required, held(&["host-sudo"]));
        assert_eq!(decl.unknown, held(&["root-access"]));
        // ...and the co-occurring valid marker does NOT rescue it.
        assert!(!decl.is_satisfied_by(&held(&["host-sudo", "root-access"])));
    }

    #[test]
    fn unrelated_html_comments_are_ignored() {
        let decl = extract_capability_markers(
            "<!-- loom:points=3 -->\n<!-- just a note -->\n<!-- loom:capability=host-sudo -->",
        );
        assert_eq!(decl.required, held(&["host-sudo"]));
    }

    /// Parity table against `defaults/scripts/extract-capability-markers.sh`.
    ///
    /// Every row below was executed through the shell reference and this parser
    /// side by side while #6893 was built; the outputs matched byte for byte
    /// (`<valid, comma-joined>` / `<unknown, comma-joined>`). Keeping the table
    /// here means a future change to either side that breaks the agreement is
    /// caught by `cargo test` rather than by a fleet-wide behavior divergence.
    #[test]
    fn parity_with_the_shell_reference_parser() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("<!-- loom:capability=host-sudo -->", &["host-sudo"], &[]),
            ("<!--loom:capability=host-sudo-->", &["host-sudo"], &[]),
            ("<!--loom:capability=tailnet-access-->", &["tailnet-access"], &[]),
            // No-space form of a genuinely dash-suffixed (thus unknown) value:
            // the anchor must not silently "help" this into the known literal
            // by trimming the intentional trailing dash away.
            ("<!--loom:capability=host-sudo--->", &[], &["host-sudo-"]),
            (
                "<!--  loom:capability=cloud-profile:prod-aws  -->",
                &["cloud-profile:prod-aws"],
                &[],
            ),
            (
                "<!-- loom:capability=host-sudo --><!-- loom:capability=tailnet-access -->",
                &["host-sudo", "tailnet-access"],
                &[],
            ),
            (
                "<!-- loom:capability=host-sudo -->\n<!-- loom:capability=host-sudo -->",
                &["host-sudo"],
                &[],
            ),
            ("Use `<!-- loom:capability=<name> -->`", &[], &[]),
            ("<!-- loom:capability= -->", &[], &[]),
            ("<!-- loom:capability=Host-Sudo -->", &[], &[]),
            ("<!-- loom:capability=host_sudo -->", &[], &["host_sudo"]),
            ("<!-- loom:capability=host-sudo- -->", &[], &["host-sudo-"]),
            ("loom:capability=host-sudo", &[], &[]),
            ("<!-- loom:points=3 -->", &[], &[]),
            (
                "<!-- loom:capability=forge-admin-token --> and <!-- loom:capability=root -->",
                &["forge-admin-token"],
                &["root"],
            ),
            ("<!-- loom:capability=cloud-profile: -->", &[], &["cloud-profile:"]),
            ("<!-- loom:capability=9abc -->", &[], &["9abc"]),
        ];
        for (body, required, unknown) in cases {
            let decl = extract_capability_markers(body);
            assert_eq!(decl.required, held(required), "required for {body:?}");
            assert_eq!(decl.unknown, held(unknown), "unknown for {body:?}");
        }
    }

    // ---- satisfaction ----------------------------------------------------

    #[test]
    fn markers_are_anded_not_ored() {
        let decl = extract_capability_markers(
            "<!-- loom:capability=host-sudo -->\n<!-- loom:capability=tailnet-access -->",
        );
        assert!(!decl.is_satisfied_by(&held(&["host-sudo"])), "one of two is not enough");
        assert!(decl.is_satisfied_by(&held(&["host-sudo", "tailnet-access"])));
        assert_eq!(decl.missing_from(&held(&["host-sudo"])), vec!["tailnet-access"]);
    }

    #[test]
    fn an_empty_declaration_is_never_satisfied_however_capable_the_worker() {
        let decl = extract_capability_markers("no markers here");
        assert!(!decl.is_satisfied_by(&held(&["host-sudo", "tailnet-access"])));
    }

    // ---- worker-side declaration -----------------------------------------

    #[test]
    fn held_capabilities_parse_on_commas_and_whitespace() {
        assert_eq!(
            parse_held_capabilities("host-sudo, tailnet-access\ncloud-profile:prod-aws"),
            held(&["cloud-profile:prod-aws", "host-sudo", "tailnet-access"])
        );
    }

    #[test]
    fn held_capabilities_drop_unknown_values_symmetrically() {
        // The fail-closed rule applies to the worker's own claim too: a typo'd
        // hold must not accidentally satisfy a typo'd requirement.
        assert_eq!(parse_held_capabilities("host-sud,root"), BTreeSet::new());
        assert_eq!(parse_held_capabilities(""), BTreeSet::new());
        assert_eq!(parse_held_capabilities("   ,,  "), BTreeSet::new());
    }

    // ---- label eligibility -----------------------------------------------

    #[test]
    fn lane_requires_the_blocked_hold_under_any_of_its_names() {
        assert!(labels_eligible_for_capability_lane(&labels(&["loom:issue", "loom:blocked"])));
        // Unmigrated items (#11795): needs-capability folds into loom:blocked.
        assert!(labels_eligible_for_capability_lane(&labels(&["loom:needs-capability"])));
        assert!(labels_eligible_for_capability_lane(&labels(&[
            "loom:blocked",
            "loom:needs-capability",
        ])));
        // No blocked hold: not the shape this lane un-parks.
        assert!(!labels_eligible_for_capability_lane(&labels(&["loom:issue"])));
    }

    /// #11861: `loom:operator` is a no-op for every role, so an operator hold
    /// never enters the lane, under any of its names and beside any label.
    #[test]
    fn the_operator_hold_never_enters_the_lane() {
        for hold in [
            "loom:operator",
            "loom:operator-only",
            "loom:operator-mechanical",
            "loom:operator-decision",
            "loom:operator-blocked",
        ] {
            assert!(!labels_eligible_for_capability_lane(&labels(&[hold])), "{hold}");
            assert!(
                !labels_eligible_for_capability_lane(&labels(&["loom:blocked", hold])),
                "{hold} beside loom:blocked"
            );
        }
        let routing = route_mechanical(
            &labels(&["loom:issue", "loom:operator"]),
            Some("<!-- loom:capability=host-sudo -->"),
            &held(&["host-sudo"]),
        );
        assert_eq!(routing, MechanicalRouting::NotApplicable);
    }

    #[test]
    fn a_decision_by_label_is_never_eligible() {
        assert!(!labels_eligible_for_capability_lane(&labels(&[
            "loom:blocked",
            "loom:decision-malformed",
        ])));
    }

    #[test]
    fn veto_labels_refuse_the_lane() {
        for veto in ["loom:operator", "loom:frozen", "loom:epic"] {
            assert!(
                !labels_eligible_for_capability_lane(&labels(&["loom:blocked", veto])),
                "{veto} must veto the capability lane"
            );
        }
        // The daemon's own claim is not a veto (its re-dispatch of a lane item).
        assert!(labels_eligible_for_capability_lane(&labels(&["loom:blocked", "loom:building"])));
    }

    /// A park record naming a blocker means the item waits on another change,
    /// which no capability satisfies (#11861): only a capability-only park
    /// (no record, or a reason-only one) enters the lane.
    #[test]
    fn a_named_blocker_keeps_the_item_out_of_the_lane() {
        let marker = "<!-- loom:capability=host-sudo -->";
        let named = format!("{marker}\n<!-- loom:park Blocked by: #12 by=curator -->");
        let routing = route_mechanical(&labels(MECHANICAL), Some(&named), &held(&["host-sudo"]));
        assert_eq!(routing, MechanicalRouting::NotApplicable);
        let reason_only = format!(
            "{marker}\n<!-- loom:park Blocked by: (unstated) by=curator reason=\"needs root\" -->"
        );
        let routing =
            route_mechanical(&labels(MECHANICAL), Some(&reason_only), &held(&["host-sudo"]));
        assert_eq!(routing, MechanicalRouting::ProposeDispatch);
    }

    const MECHANICAL: &[&str] = &["loom:issue", "loom:blocked"];

    #[test]
    fn routes_to_propose_when_every_capability_is_held() {
        let routing = route_mechanical(
            &labels(MECHANICAL),
            Some("<!-- loom:capability=host-sudo -->"),
            &held(&["host-sudo"]),
        );
        assert_eq!(routing, MechanicalRouting::ProposeDispatch);
        assert!(routing.is_dispatchable());
    }

    #[test]
    fn routes_to_missing_capabilities_naming_what_is_absent() {
        let routing = route_mechanical(
            &labels(MECHANICAL),
            Some("<!-- loom:capability=host-sudo -->\n<!-- loom:capability=tailnet-access -->"),
            &held(&["host-sudo"]),
        );
        assert_eq!(
            routing,
            MechanicalRouting::MissingCapabilities {
                missing: vec!["tailnet-access".to_string()]
            }
        );
        assert!(!routing.is_dispatchable());
    }

    #[test]
    fn routes_to_no_declaration_for_a_bare_mechanical_item() {
        let routing =
            route_mechanical(&labels(MECHANICAL), Some("no markers"), &held(&["host-sudo"]));
        assert_eq!(routing, MechanicalRouting::NoDeclaration { unknown: vec![] });
        assert!(!routing.is_dispatchable());
    }

    #[test]
    fn routes_to_no_declaration_when_the_body_was_not_fetched() {
        let routing = route_mechanical(&labels(MECHANICAL), None, &held(&["host-sudo"]));
        assert_eq!(routing, MechanicalRouting::NoDeclaration { unknown: vec![] });
    }

    #[test]
    fn an_unknown_value_fails_closed_even_when_the_known_half_is_held() {
        let routing = route_mechanical(
            &labels(MECHANICAL),
            Some("<!-- loom:capability=host-sudo -->\n<!-- loom:capability=root -->"),
            &held(&["host-sudo"]),
        );
        assert_eq!(
            routing,
            MechanicalRouting::NoDeclaration {
                unknown: vec!["root".to_string()]
            }
        );
        assert!(!routing.is_dispatchable());
    }

    #[test]
    fn an_empty_held_set_never_dispatches() {
        // The default on every host: no LOOM_WORKER_CAPABILITIES, no change.
        let routing = route_mechanical(
            &labels(MECHANICAL),
            Some("<!-- loom:capability=host-sudo -->"),
            &BTreeSet::new(),
        );
        assert_eq!(
            routing,
            MechanicalRouting::MissingCapabilities {
                missing: vec!["host-sudo".to_string()]
            }
        );
    }

    #[test]
    fn a_decision_block_with_a_marker_and_a_capable_worker_still_parks() {
        // #11795: a decision is a ruling, never mechanical, whatever markers the
        // body also carries.
        let fenced = "<!-- loom:capability=host-sudo -->\n\n```decision\n{}\n```\n";
        let managed = "<!-- loom:operator-decision:start -->\n<!-- loom:capability=host-sudo -->";
        for body in [fenced, managed] {
            let routing = route_mechanical(&labels(MECHANICAL), Some(body), &held(&["host-sudo"]));
            assert_eq!(routing, MechanicalRouting::NotApplicable, "{body:?}");
        }
    }

    #[test]
    fn an_unmigrated_needs_capability_item_routes() {
        let routing = route_mechanical(
            &labels(&["loom:needs-capability"]),
            Some("<!-- loom:capability=host-sudo -->"),
            &held(&["host-sudo"]),
        );
        assert_eq!(routing, MechanicalRouting::ProposeDispatch);
    }

    /// AC4 tripwire (#6893). `MechanicalRouting` has exactly **one**
    /// dispatchable variant and it is propose-mode. This exhaustive `match`
    /// stops compiling the moment a variant is added, which is precisely the
    /// point: introducing a live-execution lane must be a deliberate, reviewed
    /// decision with its own explicit opt-in, never something that arrives as a
    /// side effect of an unrelated change.
    #[test]
    fn the_only_dispatchable_variant_is_propose_mode() {
        let all = [
            MechanicalRouting::NotApplicable,
            MechanicalRouting::NoDeclaration { unknown: vec![] },
            MechanicalRouting::MissingCapabilities { missing: vec![] },
            MechanicalRouting::ProposeDispatch,
        ];
        for routing in &all {
            let expected = match routing {
                MechanicalRouting::NotApplicable
                | MechanicalRouting::NoDeclaration { .. }
                | MechanicalRouting::MissingCapabilities { .. } => false,
                MechanicalRouting::ProposeDispatch => true,
            };
            assert_eq!(routing.is_dispatchable(), expected, "{routing:?}");
        }
    }
}
