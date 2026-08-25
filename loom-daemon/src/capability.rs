//! Capability declarations for the `loom:operator-mechanical` dispatch lane
//! (#6885 Part 2, issue #6893).
//!
//! # What this module is for
//!
//! `loom:operator-only` is a **hard park**: every dispatch route refuses the
//! item, unconditionally, keyed on the base label
//! ([`crate::work_finder::PARK_LABELS`]). Its four sub-kind labels (#5671) are
//! additive metadata the skip logic never branched on, so a
//! `loom:operator-mechanical` item — "needs a credential/host access, but **no
//! judgement**" — was parked exactly like a genuine `loom:operator-decision`
//! item even though nothing about it requires a human *ruling*.
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
//! combines them into a routing decision; nothing here reads labels or touches
//! the forge.
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
//!    substring — the same anchoring `require-complexity-marker.sh` uses, for
//!    the same reason (#4840): prose that merely *quotes* the marker syntax as
//!    literal example text (as the convention doc itself does) must never be
//!    mistaken for a live marker. A `<name>` placeholder breaks the `-->`
//!    anchor and is therefore skipped.
//! 2. **Every match is collected, deduplicated** — unlike the complexity marker
//!    (`tail -1`, last match wins, one tier per item), the whole declared set
//!    matters because multiple markers are **ANDed**.
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
/// `loom:operator-mechanical` item parked exactly as before this module existed.
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

/// The `loom:operator-only` base label — the hard park all four sub-kinds
/// inherit.
pub const OPERATOR_ONLY_LABEL: &str = "loom:operator-only";

/// The one `loom:operator-only` sub-kind this lane applies to: mechanical work
/// requiring access, not a ruling (#5671).
pub const OPERATOR_MECHANICAL_LABEL: &str = "loom:operator-mechanical";

/// The other three `loom:operator-only` sub-kinds. Each stays hard-skipped
/// **unconditionally**, regardless of any capability marker in its body — the
/// convention doc is explicit that they ignore the marker entirely. Listed here
/// so [`labels_eligible_for_capability_lane`] can refuse an item that carries
/// one of them *alongside* `loom:operator-mechanical` (a contradictory
/// labelling, resolved conservatively in favour of the judgement sub-kind).
pub const JUDGEMENT_SUB_KIND_LABELS: &[&str] = &[
    "loom:operator-decision",
    "loom:operator-blocked",
    "loom:operator-objective",
];

/// Labels that veto the capability lane outright even next to a well-formed
/// mechanical declaration.
///
/// - `loom:blocked` is an independent park with its own release condition (a
///   named dependency landing); a capability declaration says nothing about it.
/// - `loom:needs-capability` is explicitly out of scope (#5817/#6885 non-goal):
///   it asserts the capability *does not exist yet*, which no worker can hold.
/// - `loom:operator` means "the engine has stopped on this artifact and a human
///   must act"; un-parking past it would defeat the hold.
const LANE_VETO_LABELS: &[&str] = &["loom:blocked", "loom:needs-capability", "loom:operator"];

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
    /// most `loom:operator-mechanical` ones.
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
/// Reproduces the reference's **two** steps, which are not the same regex and do
/// not always agree — matching that exactly is the point, since a disagreement
/// between the two surfaces about whether an item is dispatchable is the failure
/// mode the convention doc's "parse identically" rule exists to prevent:
///
/// 1. **Recognition** — `grep -oE '<!--…-->'` decides whether this is a marker
///    at all. `-` is both a legal value character and the head of the closing
///    delimiter, so a greedy value scan of `tailnet-access-->` eats the `--` and
///    then cannot find its own terminator; POSIX ERE's leftmost-longest matching
///    backtracks out of that, and locating the `-->` first is the equivalent.
/// 2. **Extraction** — `sed -E 's/.*capability=([a-z0-9][a-z0-9:_-]*).*/\1/'`
///    pulls the value back out of the matched text with a **greedy** capture and
///    no terminator anchor, so on that same `tailnet-access-->` it yields
///    `tailnet-access--` (two trailing dashes), which then fails the closed
///    vocabulary and parks the item. Surprising, but fail-closed, and reproduced
///    here deliberately rather than silently "fixed" on one side only. Fixing
///    it properly means changing BOTH sides in one PR — tracked as #6914.
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
    if !is_valid_marker_value(rest[value_start..close_at].trim_end_matches(is_marker_space)) {
        return None;
    }
    // Step 2: extraction — greedy over the value character class, which may run
    // past `close_at` into the delimiter's own dashes.
    let value_end = value_start
        + rest[value_start..]
            .find(|c: char| !is_value_class(c))
            .unwrap_or(rest.len() - value_start);
    Some((&rest[value_start..value_end], close_at + COMMENT_CLOSE.len()))
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
/// values) means every `loom:operator-mechanical` item stays parked, i.e. **zero
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
/// Requires **both** `loom:operator-only` and `loom:operator-mechanical`, and
/// refuses the item outright if it also carries any of the three judgement
/// sub-kinds ([`JUDGEMENT_SUB_KIND_LABELS`]) or any [`LANE_VETO_LABELS`] entry.
/// A body's capability markers are never consulted for an item this rejects —
/// which is what keeps `loom:operator-decision` / `-blocked` / `-objective`
/// hard-skipped exactly as today even if one of them happens to contain a
/// marker.
///
/// This is a pure predicate over label names: no forge I/O, no body read.
#[must_use]
pub fn labels_eligible_for_capability_lane<S: AsRef<str>>(labels: &[S]) -> bool {
    let has = |name: &str| labels.iter().any(|l| l.as_ref() == name);
    if !has(OPERATOR_ONLY_LABEL) || !has(OPERATOR_MECHANICAL_LABEL) {
        return false;
    }
    if JUDGEMENT_SUB_KIND_LABELS.iter().any(|l| has(l)) {
        return false;
    }
    if LANE_VETO_LABELS.iter().any(|l| has(l)) {
        return false;
    }
    true
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
    /// mechanical operator-only item (see
    /// [`labels_eligible_for_capability_lane`]). The item's normal skip/park
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
    /// item is exempt from the `loom:operator-only` park". Every other variant
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
    if !labels_eligible_for_capability_lane(labels) {
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
    fn collects_every_marker_deduplicated_unlike_the_complexity_marker() {
        // The complexity marker takes `tail -1`; capability markers are ANDed,
        // so all of them matter and repeats collapse.
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
    fn no_space_before_the_delimiter_fails_closed_exactly_as_the_reference_does() {
        // Cross-checked byte-for-byte against
        // `defaults/scripts/extract-capability-markers.sh`: its `sed` extraction
        // is greedy and unanchored, so a value abutting `-->` picks up the
        // delimiter's dashes and falls out of the closed vocabulary. Surprising
        // but fail-closed — and reproducing it is the point, because a Rust side
        // that "fixed" it unilaterally would disagree with the shell/markdown
        // side about whether the very same item is dispatchable. Fixing both
        // sides together is tracked as #6914.
        let decl = extract_capability_markers("<!--loom:capability=tailnet-access-->");
        assert_eq!(decl.unknown, held(&["tailnet-access--"]));
        assert!(decl.required.is_empty());
        assert!(!decl.is_satisfied_by(&held(&["tailnet-access"])));
        // The documented, spaced form is unaffected.
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
            "<!-- loom:complexity=complex -->\n<!-- just a note -->\n<!-- loom:capability=host-sudo -->",
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
            ("<!--loom:capability=host-sudo-->", &[], &["host-sudo--"]),
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
            ("<!-- loom:complexity=complex -->", &[], &[]),
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
    fn lane_requires_both_the_base_label_and_the_mechanical_sub_kind() {
        assert!(labels_eligible_for_capability_lane(&labels(&[
            "loom:issue",
            "loom:operator-only",
            "loom:operator-mechanical",
        ])));
        // Sub-kind without the base label: not the shape this lane un-parks.
        assert!(!labels_eligible_for_capability_lane(&labels(&["loom:operator-mechanical"])));
        // Base label alone: the ordinary hard park.
        assert!(!labels_eligible_for_capability_lane(&labels(&["loom:operator-only"])));
    }

    #[test]
    fn the_other_three_sub_kinds_are_never_eligible() {
        for sub_kind in JUDGEMENT_SUB_KIND_LABELS {
            assert!(
                !labels_eligible_for_capability_lane(&labels(&["loom:operator-only", sub_kind])),
                "{sub_kind} must stay hard-skipped"
            );
            // ...and a contradictory pairing resolves in favour of the
            // judgement sub-kind, never the mechanical one.
            assert!(
                !labels_eligible_for_capability_lane(&labels(&[
                    "loom:operator-only",
                    "loom:operator-mechanical",
                    sub_kind
                ])),
                "{sub_kind} alongside mechanical must still refuse"
            );
        }
    }

    #[test]
    fn veto_labels_refuse_the_lane() {
        for veto in LANE_VETO_LABELS {
            assert!(
                !labels_eligible_for_capability_lane(&labels(&[
                    "loom:operator-only",
                    "loom:operator-mechanical",
                    veto
                ])),
                "{veto} must veto the capability lane"
            );
        }
    }

    // ---- routing ---------------------------------------------------------

    const MECHANICAL: &[&str] = &[
        "loom:issue",
        "loom:operator-only",
        "loom:operator-mechanical",
    ];

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
    fn a_judgement_sub_kind_with_a_marker_and_a_capable_worker_still_parks() {
        // The convention doc's explicit rule: the other three sub-kinds ignore
        // the marker entirely.
        for sub_kind in JUDGEMENT_SUB_KIND_LABELS {
            let routing = route_mechanical(
                &labels(&["loom:issue", "loom:operator-only", sub_kind]),
                Some("<!-- loom:capability=host-sudo -->"),
                &held(&["host-sudo"]),
            );
            assert_eq!(routing, MechanicalRouting::NotApplicable, "{sub_kind}");
            assert!(!routing.is_dispatchable());
        }
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
