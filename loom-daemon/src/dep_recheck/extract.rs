//! `extract-refs` — the reference extraction behind "Checking Operator-Only
//! Premises" (#4963) (epic #7810, PR 4).
//!
//! # The self-perpetuating loop this closes
//!
//! The bot's own "premise possibly stale" comment quotes the matched phrase
//! back into the thread. A naive `[.body] + [.comments[].body]` scan — the old
//! inline `curator.md` shell — re-matches its own prior report forever, even
//! after the body itself is fixed. So references come from the issue **body**
//! always, plus only those comments that are neither authored by the automation
//! identity nor carrying one of its own markers.

use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Default `--bot-login`.
pub const DEFAULT_BOT_LOGIN: &str = "loom-fleet-dispatch";

/// Markers that identify the automation's own re-check comments. Excluded
/// belt-and-suspenders, on top of the author check: a comment carrying one was
/// written by this mechanism whatever login the forge reports for it.
const OWN_MARKERS: [&str; 2] = [
    "<!-- curator:dep-recheck:",
    "<!-- curator:operator-premise-recheck:",
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Author {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub body: String,
}

/// The `--stdin` document — the same shape `gh issue view --json body,comments`
/// returns, so a live-mode fixture can be captured verbatim.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Input {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

/// The fixed machine-readable phrasings, reused verbatim from
/// `detect-dependency-cycle.sh` / `warn-operator-gated.sh` rather than
/// inventing a second vocabulary. A bare prose mention (a backtick-quoted
/// `owner/repo#123`, say) deliberately does not count.
///
/// **Divergence from the pre-port shell (#8011, kept intentionally):** the
/// shell matched this pattern with `grep -oE`, which is line-oriented and
/// cannot span a newline. This regex runs over the whole concatenated
/// body-plus-comments text with `\s` (inside `[*_:\s]*`) matching `\n`, so a
/// phrase and its `#N` split across a line break — e.g. `"Blocked by\n#42"` —
/// now match where the shell found nothing. This is the same
/// slightly-too-permissive direction as [`super::named`]'s bullet-marker
/// divergence: missing a genuine declared reference is the worse failure
/// mode for a check whose whole job is finding one. Kept rather than
/// narrowed; changes `CONCLUSION_HASH` (via `operator-premise`'s `--refs`)
/// for any input where the phrase and reference are separated by a newline.
fn phrase_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(Blocked by|Depends on|Requires|\*\*Epic\*\*)[*_:\s]*#([0-9]+)")
            .expect("static dependency-phrase pattern")
    })
}

/// Normalise a login for comparison: lower-cased, with a leading `app/` or a
/// trailing `[bot]` stripped.
///
/// Different `gh` views normalise a GitHub App's login differently — compare
/// `judge-fallback-guard.sh`'s `app/loom-fleet-dispatch` PR-author check with
/// the bare `loom-fleet-dispatch` that `gh issue view --json comments` reports.
/// Matching only one spelling would let the loop back in through the other.
///
/// **Divergence from the pre-port shell (#8011, kept — arguably a fix):** the
/// shell only lower-cased the supplied `--bot-login`; it normalised the
/// `app/`/`[bot]` shape on the comment **author** only, not on the caller's
/// own flag. So a shell caller passing `--bot-login "app/loom-fleet-dispatch"`
/// would never match a plain `loom-fleet-dispatch` comment author — the
/// asymmetry defeated the very normalisation this function exists for. Here
/// both sides go through the same [`normalise_login`], so that case now
/// excludes such a comment as intended. No current caller passes
/// `--bot-login` (see [`DEFAULT_BOT_LOGIN`], already in normalised form), so
/// nothing live changes, but a future caller of the flag will observe this.
#[must_use]
pub fn normalise_login(login: &str) -> String {
    let lower = login.to_ascii_lowercase();
    let no_prefix = lower.strip_prefix("app/").unwrap_or(&lower);
    no_prefix
        .strip_suffix("[bot]")
        .unwrap_or(no_prefix)
        .to_string()
}

/// Whether a comment may contribute references.
#[must_use]
pub fn comment_counts(comment: &Comment, bot_login: &str) -> bool {
    if normalise_login(&comment.author.login) == normalise_login(bot_login) {
        return false;
    }
    !OWN_MARKERS.iter().any(|m| comment.body.contains(m))
}

/// The reference numbers, sorted numerically and deduplicated, space-joined.
///
/// `sort -un` — **numeric**, unlike the line sorts elsewhere in this port.
/// This list is an argument passed on to `operator-premise`, not a hashed
/// rendering, so its ordering is the shell's own and worth keeping exact.
///
/// # Two further divergences from the pre-port shell (#8072, both kept)
///
/// Found by differential-testing this function against the retired shell over
/// 700 generated inputs — see `tests/differential_extract_refs.rs`. Neither was
/// visible to the retained black-box suite that was 104/104 green at port time,
/// for the reason #8011 already recorded: a retained suite proves only what its
/// author thought to write down.
///
/// **Zero-padded references normalise.** The shell's `sort -un` sorts
/// numerically but prints the ORIGINAL token, so `Blocked by #007` came out as
/// `007`. The `u64` round-trip here prints `7`. GitHub resolves `#007` to issue
/// 7, so this spelling is the more correct one, and it makes `#7` and `#007`
/// deduplicate to a single reference where the shell also collapsed them (both
/// compare equal under `sort -n`). Kept. It does change `CONCLUSION_HASH` for
/// any text that zero-pads, which is a one-time change at the port boundary,
/// not an ongoing divergence — the shell is retired.
///
/// **A reference above `u64::MAX` is dropped, not kept.** `.parse().ok()`
/// below discards it; the shell kept the literal token. The boundary is exact:
/// `#18446744073709551615` survives, `#18446744073709551616` does not. No real
/// issue number is twenty digits, so this is reachable only from adversarial
/// forge text — and forge text IS untrusted input (see
/// `defaults/docs/untrusted-external-content.md`).
///
/// Kept deliberately, and note the direction carefully, because it is NOT the
/// same call as `parse_refs_arg` (in [`super::cli`]), which hard-errors on a token it
/// cannot parse (#8011). That flag carries an OPERATOR's explicit list, where
/// silently computing over fewer references than were asked for is the
/// "confident wrong answer". This function scans arbitrary issue bodies and
/// comments, where a hard error would let any commenter halt the Curator's
/// re-check by typing a twenty-digit `#N`. Dropping is the right failure here;
/// erroring is the right failure there.
///
/// Downstream the drop is usually fail-SAFE — losing every reference yields
/// `verdict: open`, i.e. still blocked. The one case worth knowing is a MIXED
/// set: one merged reference plus one dropped gives `stale-premise`, where the
/// shell would instead have hard-errored trying to fetch the unfetchable token.
#[must_use]
pub fn extract(input: &Input, bot_login: &str) -> String {
    let mut text = input.body.clone();
    for c in &input.comments {
        if comment_counts(c, bot_login) {
            text.push('\n');
            text.push_str(&c.body);
        }
    }

    let numbers: BTreeSet<u64> = phrase_re()
        .captures_iter(&text)
        .filter_map(|c| c.get(2)?.as_str().parse().ok())
        .collect();

    numbers
        .into_iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
