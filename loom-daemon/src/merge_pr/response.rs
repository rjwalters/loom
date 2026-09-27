//! The merge-API error-response classifier behind `merge-pr.sh`'s retry ladder
//! (#8191 slice).
//!
//! When `forge_merge_pr` fails, the retry loop decides what to do next purely
//! from the forge's error TEXT. Three string matchers, evaluated in a fixed
//! order, choose between four mutually-exclusive routes:
//!
//! | route | what `merge-pr.sh` does next |
//! |---|---|
//! | [`MergeResponseKind::MergeInProgress`] | HTTP 405 — sleep, re-read `.merged`, then keep retrying |
//! | [`MergeResponseKind::HeadMismatch`] | #5579 — never retry-and-merge; `_head_moved_or_resync` either spends #8164's single self-sync retry or exits 3 |
//! | [`MergeResponseKind::BaseModified`] | `forge_update_branch`, wait, re-read the head SHA, retry |
//! | [`MergeResponseKind::Other`] | hard stop, exit 1, quoting the response |
//!
//! # Why this is the dangerous one
//!
//! The two middle routes are opposites over near-identical English. "Base
//! branch was modified" means the PR's BASE fell behind, and a
//! rebase-and-retry is correct. "Head branch was modified." means the PR's OWN
//! head moved past the SHA the approving review described — retrying there
//! would, on a repo where the precondition SHA were ever adopted blindly,
//! squash a diff no Judge approved. Misrouting the second as the first is the
//! one classification error in this function that spends an irreversible
//! operation on the wrong tree.
//!
//! In the shell the precedence that keeps them apart was **emergent**: two
//! `if` blocks five lines apart in a 70-line loop, with the ordering asserted
//! only by an `awk` scan over `merge-pr.sh`'s own source text looking for
//! which `grep` appeared first. Here it is a single ordered match in one
//! place, so reordering the routes is a visible one-line diff rather than a
//! property of statement layout. See `classify`.
//!
//! # Fidelity: this models `grep`, not "a sensible matcher"
//!
//! Per `defaults/docs/verification-recipes.md` §6 "Cause 3: a `grep -E`
//! pattern is not a regex", every pattern carried across the port boundary is
//! recorded here with the semantics the *tool* supplied, and each is pinned:
//!
//! - **All five patterns are pure literals.** Neither `Merge already in
//!   progress` nor `Base branch was modified` contains a BRE metacharacter,
//!   and the ERE alternation's three branches are `Head branch was modified\.`
//!   (an ESCAPED dot, so literal), `head out of date` and `expectedHeadOid`.
//!   Nothing here is a wildcard, so the port is substring search rather than a
//!   regex engine — which also removes the `regex` crate's Unicode defaults
//!   from the blast radius entirely.
//! - **Line-orientation is immaterial, and that is a fact to state rather than
//!   assume.** `grep` matches per line, so the shell asked "does any LINE
//!   contain this?" while this module asks "does the TEXT contain this?". The
//!   two agree for every pattern above *because a newline-free literal cannot
//!   match across a line break* — there is no `.` or class that could span
//!   one. Add a pattern containing `[[:space:]]`, `.` or an anchor and that
//!   equivalence is gone; see the same recipe's first table row for the defect
//!   that produced.
//! - **Case-insensitivity is ASCII, and only on the head-mismatch matcher.**
//!   `_is_head_mismatch_response` used `grep -Ei`; its two siblings used a bare
//!   `grep -q` and were case-SENSITIVE. That asymmetry is deliberate upstream
//!   behaviour, not an oversight, so it is preserved verbatim — see
//!   [`ascii_icontains`] for why the folding is `eq_ignore_ascii_case` and not
//!   `to_lowercase()` or `(?i)`.
//! - **The haystack is BYTES.** `grep` is byte-oriented and matches happily in
//!   a stream that is not valid UTF-8; a forge error body is external input and
//!   need not be. Classifying over `&[u8]` keeps that tolerance (recipe §6
//!   "Cause 4: the shell was TOLERANT by construction"), where a
//!   `read_to_string` would have turned a mojibake 409 into a read failure and
//!   a `String::from_utf8_lossy` would have inserted U+FFFD inside a candidate
//!   match.
//!
//! # What the shell tolerated, and still does
//!
//! - **Empty / absent input** — `echo "" | grep -q …` matches nothing, so the
//!   route is `Other`: a hard stop quoting an empty response, never a retry.
//! - **A multi-line response** — the 409 bodies carry `Error: …` prefixes and
//!   trailing `(HTTP 409)` suffixes, and `forge_merge_pr` is captured with
//!   `2>&1`, so the text routinely contains several lines of unrelated
//!   diagnostics. Substring search over the whole text finds the marker
//!   wherever it sits, exactly as the per-line `grep` did.
//! - **Both markers present at once** — `classify` resolves it by precedence
//!   rather than rejecting it, because the shell did. A body containing both
//!   "Merge already in progress" and "Head branch was modified." is
//!   `MergeInProgress`; one containing both head- and base-modified text is
//!   `HeadMismatch`. That second case is the load-bearing one and now has a
//!   direct unit test instead of a source-layout scan.

/// The four routes `merge-pr.sh`'s retry loop can take from a failed merge.
///
/// Deliberately closed and exhaustive: `Other` is a real classification ("no
/// marker matched, stop and quote the response"), not an error value. A caller
/// that cannot obtain a classification at all must distinguish that from
/// `Other` on its own — the shell does, via the missing sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeResponseKind {
    /// HTTP 405 "Merge already in progress": a concurrent merger holds the PR.
    MergeInProgress,
    /// The PR's OWN head moved past the gated SHA (#5579). Never
    /// retry-and-merge; `_head_moved_or_resync` adjudicates (#8164).
    HeadMismatch,
    /// The PR's BASE branch fell behind. Sync the head, re-read its SHA, retry.
    BaseModified,
    /// No marker matched. The shell's terminal `error`, quoting the response.
    Other,
}

impl MergeResponseKind {
    /// The stable wire token `merge-pr.sh` branches on.
    ///
    /// Hyphenated lowercase so the shell can compare it with `[[ … == … ]]`
    /// without quoting games, and stable across releases: it is a protocol
    /// between two files that roll independently (see the `requires-daemon:`
    /// block in `merge-pr.sh`).
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::MergeInProgress => "merge-in-progress",
            Self::HeadMismatch => "head-mismatch",
            Self::BaseModified => "base-modified",
            Self::Other => "other",
        }
    }
}

/// `grep -q "Merge already in progress"` — case-SENSITIVE, literal.
const MERGE_IN_PROGRESS: &[u8] = b"Merge already in progress";

/// `grep -q "Base branch was modified"` — case-SENSITIVE, literal.
///
/// Note the absence of the trailing period the head-mismatch marker carries:
/// the shell's pattern stopped at `modified`, so a body reading "Base branch
/// was modified," or "…modified and cannot…" still matches. Preserved.
const BASE_MODIFIED: &[u8] = b"Base branch was modified";

/// The three branches of `grep -Eiq 'Head branch was modified\.|head out of
/// date|expectedHeadOid'`, case-INSENSITIVE (ASCII), all literal.
///
/// String provenance is documented on `forge_merge_pr` in
/// `lib/forge-helpers.sh`: GitHub REST and Gitea are verified against each
/// forge's own source/spec, and `expectedHeadOid` is the GitHub GraphQL
/// (retired server-side auto-merge, #8427) spelling, kept so an
/// operator-armed merge's error still classifies.
///
/// The FIRST branch keeps its trailing `.` — the shell wrote it `modified\.`,
/// an escaped literal dot — while [`BASE_MODIFIED`] has none. Dropping it here
/// would widen the head-mismatch route at the base-modified route's expense,
/// which is the dangerous direction.
const HEAD_MISMATCH: [&[u8]; 3] = [
    b"Head branch was modified.",
    b"head out of date",
    b"expectedHeadOid",
];

/// Classify a failed merge's response text into exactly one route.
///
/// The `match` order below IS `merge-pr.sh`'s `if` order, and it is the whole
/// safety property of this module:
///
/// 1. `Merge already in progress` — checked first because it is a *liveness*
///    condition (someone else is mid-merge) rather than a statement about
///    either branch, and the shell resolved it before looking at SHAs.
/// 2. head-mismatch — before base-modified, so a body mentioning both is never
///    routed into the rebase-and-retry path (#5579).
/// 3. `Base branch was modified` — the retryable one.
/// 4. everything else — stop.
#[must_use]
pub fn classify(response: &[u8]) -> MergeResponseKind {
    if contains(response, MERGE_IN_PROGRESS) {
        return MergeResponseKind::MergeInProgress;
    }
    if is_head_mismatch(response) {
        return MergeResponseKind::HeadMismatch;
    }
    if contains(response, BASE_MODIFIED) {
        return MergeResponseKind::BaseModified;
    }
    MergeResponseKind::Other
}

/// Does this merge-API response say "your head-SHA precondition is stale"?
///
/// **The single definition of the head-mismatch predicate.** It has two
/// callers that must never disagree:
///
/// 1. [`classify`] above, choosing the retry loop's route.
/// 2. [`super::head_sync::is_head_mismatch`], gating #8164's self-sync retry
///    authorization — which must not let a caller authorize a retry by
///    mislabelling an arbitrary error as a head mismatch.
///
/// Until this slice those were three separate copies of the same three
/// literals — one in `merge-pr.sh`, one in `head_sync`, one here — policed by
/// a drift test (`tests/merge_pr_head_sync_differential.rs`) rather than
/// prevented. The shell copy is now gone and `head_sync` delegates here, so
/// there is one copy and the drift is unrepresentable. The differential
/// survives, re-pointed at the FROZEN retired shell as its oracle, which is
/// what it was really proving all along.
///
/// **Not the same question as `classify(x) == HeadMismatch`**, and the
/// difference is load-bearing: a response naming both a 405 and a head
/// mismatch is `MergeInProgress` by the ladder's precedence, but IS still a
/// head mismatch as far as #8164's authorization is concerned. Delegating
/// `head_sync` to the ladder rather than to this predicate would have
/// narrowed that authorization silently.
#[must_use]
pub fn is_head_mismatch(response: &[u8]) -> bool {
    HEAD_MISMATCH
        .iter()
        .any(|needle| ascii_icontains(response, needle))
}

/// Case-sensitive byte substring search — a bare `grep -q <literal>`.
///
/// `windows()` yields nothing when the needle is longer than the haystack, so
/// a short or empty response is simply "no match" rather than a panic. Every
/// needle in this module is a non-empty `const`, which is what makes
/// `windows(needle.len())` safe (it panics on 0).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    debug_assert!(!needle.is_empty(), "windows(0) panics");
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// ASCII-case-insensitive byte substring search — `grep -Ei <literal>`.
///
/// **Why `eq_ignore_ascii_case` and not `to_lowercase()` / a `(?i)` regex.**
/// `grep -i` folds case according to the locale, which for these ASCII-only
/// patterns means ASCII folding on every locale Loom runs under, and exactly
/// ASCII folding under the `LC_ALL=C` the differential harness pins. Rust's
/// `str::to_lowercase` and the `regex` crate's `(?i)` are both **Unicode**:
/// `to_lowercase` maps U+212A KELVIN SIGN to `k` and expands U+0130 to two
/// scalars (changing byte offsets as well as content), and `(?i)` adds the
/// full Unicode simple-case-folding table. Either would match inputs `grep`
/// does not — widening the head-mismatch route on attacker-influenced forge
/// text. `defaults/docs/verification-recipes.md` §6 records this exact class
/// costing an earlier slice a silent per-host divergence (`Cloſes #1`).
///
/// It is also byte-for-byte safe on non-UTF-8 input: no decoding happens, and
/// `eq_ignore_ascii_case` leaves every byte ≥ 0x80 compared exactly.
fn ascii_icontains(haystack: &[u8], needle: &[u8]) -> bool {
    debug_assert!(!needle.is_empty(), "windows(0) panics");
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests;
