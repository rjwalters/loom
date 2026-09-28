//! The bounded zero-row check-runs settle (#9091, a slice of #8191's family).
//!
//! # The incident
//!
//! `merge-pr.sh --auto` polls the head SHA's check-runs rollup until it
//! settles. #6169 taught that loop never to trust a *single* zero-row read: an
//! empty rollup is ambiguous between "this repo genuinely has no CI configured
//! for this commit" and "the forge returned a degraded/empty response for this
//! poll", and trusting the first one merged a PR six minutes into a 40-minute
//! board-test run. Its remedy was to re-poll until a nonzero rollup appeared
//! **or the whole `LOOM_AUTO_MERGE_TIMEOUT` elapsed**.
//!
//! That second escape hatch was catastrophic on the case the guard meets most
//! often. On a repo with no CI configured for the changed paths, every poll
//! returns zero rows forever, so *every* `--auto` merge there spent the full
//! 600s default before merging — long enough that the calling agent's own
//! process cap killed it first. A fleet repo's PR (2026-09-26; timeline in
//! rjwalters/loom#9091) got a "Proceeding with squash merge…" comment and then
//! no merge, no failure, and no label change: the merge died inside the wait.
//!
//! # The discriminator is the base branch's REQUIRED status-check set
//!
//! "How long is it safe to trust an empty rollup?" has a definite answer, and
//! it is not a timer:
//!
//! - **no required contexts** — nothing the forge would gate this merge on can
//!   still be registering, and a wrongly-empty read can at worst skip
//!   *informational* checks, which `_wait_for_checks_then_sync_merge` already
//!   merges over by design (#3486). Settle after
//!   [`DEFAULT_SETTLE_POLLS`] reads, spaced by the short
//!   [`DEFAULT_SETTLE_INTERVAL`] — about 10s of grace for a check-run that is
//!   merely slow to *register*, two orders of magnitude below the ceiling it
//!   replaces.
//! - **required contexts present** — a required context that has not
//!   registered yet is exactly #6169's danger, and merging early would bypass
//!   a gate that *can* block. The full wait stands, unchanged.
//! - **the lookup errored** — unknown protection is not evidence of absent
//!   protection. Fail closed onto the same full wait.
//!
//! # #6169 cannot be re-enabled by an operator
//!
//! [`settle_polls`] floors its answer at [`MIN_SETTLE_POLLS`]: settling on a
//! single empty read *is* the #6169 bug, so no value of
//! `LOOM_ZERO_CHECKS_SETTLE_POLLS` — including a non-numeric one — can restore
//! it. [`settle_interval`] is validated in the conservative direction too: a
//! garbage value falls back to the caller's ordinary poll interval (longer
//! spacing), never to an unusable `sleep` argument.
//!
//! # Why the caller carries the cache rather than this process
//!
//! The required-context lookup is two forge reads, and the loop may take many
//! zero-row polls. Each invocation of this subcommand is a fresh process, so
//! the resolved state is handed **back** to the caller on every decision line
//! and passed **in** on the next poll — which is what makes the lookup happen
//! exactly once per `--auto` wait, with the shell holding nothing but an opaque
//! token. See `cli::merge_pr_zero_checks` for the line format.

use std::fmt;

/// Settle now: trust the empty rollup and proceed to the synchronous merge.
pub const SETTLE: &str = "LOOM-ZERO-CHECKS-SETTLE";

/// The whole bounded wait elapsed while the rollup stayed empty — #6169's
/// original fallback, reached only when the bounded settle does not apply.
pub const TIMEOUT: &str = "LOOM-ZERO-CHECKS-TIMEOUT";

/// Keep waiting: sleep the decision's interval and poll again.
pub const WAIT: &str = "LOOM-ZERO-CHECKS-WAIT";

/// Zero-row polls required before an unprotected base branch is trusted.
pub const DEFAULT_SETTLE_POLLS: u64 = 3;

/// The floor no operator value may go below — 1 poll is the #6169 bug itself.
pub const MIN_SETTLE_POLLS: u64 = 2;

/// Seconds between the bounded settle's polls.
pub const DEFAULT_SETTLE_INTERVAL: u64 = 5;

/// What the base branch's required status-check set turned out to be.
///
/// [`Required::Unknown`] is the caller's "not resolved yet" token, and the only
/// value that makes this subcommand perform the forge lookup. Every other
/// value is a cache the caller replays, so an unrecognised token parses back to
/// `Unknown` (re-resolve) rather than to a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Required {
    /// Not looked up yet.
    Unknown,
    /// The lookup succeeded and the base branch requires no contexts.
    None,
    /// The lookup succeeded and at least one context is required.
    Present,
    /// The lookup itself failed — NOT the same as `None`.
    LookupFailed,
}

impl Required {
    /// Parse a caller-supplied cache token. Anything unrecognised is
    /// [`Required::Unknown`]: re-resolving costs two reads, guessing costs a
    /// merge.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "none" => Self::None,
            "present" => Self::Present,
            "lookup-failed" => Self::LookupFailed,
            _ => Self::Unknown,
        }
    }

    /// The token this state is rendered as on the decision line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::None => "none",
            Self::Present => "present",
            Self::LookupFailed => "lookup-failed",
        }
    }
}

impl fmt::Display for Required {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `LOOM_ZERO_CHECKS_SETTLE_POLLS`, validated.
///
/// Unset or empty → [`DEFAULT_SETTLE_POLLS`]. Anything else is clamped up to
/// [`MIN_SETTLE_POLLS`], and a value that is not a plain non-negative integer
/// lands on that floor too — mirroring the `^[0-9]+$` test this replaces, so a
/// typo degrades to the safe end rather than to the default.
#[must_use]
pub fn settle_polls(raw: Option<&str>) -> u64 {
    match raw.map(str::trim) {
        None | Some("") => DEFAULT_SETTLE_POLLS,
        Some(s) => s
            .parse::<u64>()
            .map_or(MIN_SETTLE_POLLS, |n| n.max(MIN_SETTLE_POLLS)),
    }
}

/// `LOOM_ZERO_CHECKS_SETTLE_INTERVAL`, validated.
///
/// Unset or empty → [`DEFAULT_SETTLE_INTERVAL`]. A non-numeric value falls
/// back to `poll_interval` (the conservative direction: longer spacing), never
/// to an unusable `sleep` argument.
#[must_use]
pub fn settle_interval(raw: Option<&str>, poll_interval: u64) -> u64 {
    match raw.map(str::trim) {
        None | Some("") => DEFAULT_SETTLE_INTERVAL,
        Some(s) => s.parse::<u64>().unwrap_or(poll_interval),
    }
}

/// Everything one zero-row poll knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// PR number, for the narration only.
    pub pr: String,
    /// The base branch whose protection was (or was not) resolved.
    pub base_ref: String,
    /// How many consecutive zero-row polls have been taken, INCLUDING this one.
    pub polls: u64,
    /// The resolved required-context state (never `Unknown` by this point —
    /// the CLI resolves it first, so the decision is a pure function).
    pub required: Required,
    /// Has the caller's `LOOM_AUTO_MERGE_TIMEOUT` deadline passed?
    pub deadline_reached: bool,
    /// [`settle_polls`]'s answer.
    pub settle_polls: u64,
    /// [`settle_interval`]'s answer.
    pub settle_interval: u64,
    /// `LOOM_AUTO_MERGE_POLL_INTERVAL` — the spacing every other wait in the
    /// loop uses, and this one's when the bounded settle does not apply.
    pub poll_interval: u64,
    /// `LOOM_AUTO_MERGE_TIMEOUT`, for the narration only.
    pub timeout: u64,
}

/// What the caller must do with this zero-row poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Trust the empty rollup: fall through to the synchronous merge.
    Settle,
    /// The bounded wait is spent; #6169's own fallback applies (also a merge,
    /// but narrated as a warning because nothing was ever confirmed).
    TimedOut,
    /// Sleep [`Decision::sleep_secs`] and poll again.
    Wait,
}

impl Action {
    /// The sentinel token this action is rendered as.
    #[must_use]
    pub fn sentinel(self) -> &'static str {
        match self {
            Self::Settle => SETTLE,
            Self::TimedOut => TIMEOUT,
            Self::Wait => WAIT,
        }
    }
}

/// One poll's verdict, as the caller consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    /// Seconds to sleep before the next poll. Zero for the terminal actions,
    /// so the caller never has to special-case the field.
    pub sleep_secs: u64,
    /// The resolved required-context state, echoed for the caller to cache.
    pub required: Required,
    /// The single-line narration the caller replays through `info`/`warning`.
    pub message: String,
}

/// Decide one zero-row poll. Pure: every forge read already happened.
#[must_use]
pub fn decide(inp: &Inputs) -> Decision {
    let base = one_line(&inp.base_ref);
    let bounded = inp.required == Required::None;

    if bounded && inp.polls >= inp.settle_polls {
        return Decision {
            action: Action::Settle,
            sleep_secs: 0,
            required: inp.required,
            message: format!(
                "PR #{}: check-runs rollup empty (zero rows) on {} consecutive polls and {base} \
requires no status-check contexts; treating this repo as having no checks for this commit \
instead of waiting out the {}s ceiling (#9091)",
                one_line(&inp.pr),
                inp.polls,
                inp.timeout
            ),
        };
    }
    if inp.deadline_reached {
        return Decision {
            action: Action::TimedOut,
            sleep_secs: 0,
            required: inp.required,
            message: format!(
                "PR #{}: check-runs rollup remained empty (zero rows) for the entire {}s wait; \
proceeding on the assumption this repo genuinely has no checks configured for this commit",
                one_line(&inp.pr),
                inp.timeout
            ),
        };
    }
    let sleep_secs = if bounded {
        inp.settle_interval
    } else {
        inp.poll_interval
    };
    Decision {
        action: Action::Wait,
        sleep_secs,
        required: inp.required,
        message: format!(
            "PR #{}: check-runs rollup is empty (zero rows) -- ambiguous between 'no checks \
configured' and a transient forge read; re-polling in {sleep_secs}s before trusting it \
(required contexts on {base}: {})",
            one_line(&inp.pr),
            inp.required
        ),
    }
}

/// The one stdout line the caller parses: sentinel, sleep, cache token, prose.
///
/// Field order is deliberate. The three machine fields come first and are
/// single tokens, so `read -r action sleep required message` lands the whole
/// (space-bearing) narration in the last variable — and the sentinel leads, so
/// a caller can reject anything that is not this contract by prefix alone. The
/// line is guaranteed newline-free: see [`one_line`].
#[must_use]
pub fn render(d: &Decision) -> String {
    format!(
        "{} {} {} {}",
        d.action.sentinel(),
        d.sleep_secs,
        d.required,
        one_line(&d.message)
    )
}

/// Collapse anything that could break the one-line contract into spaces.
///
/// `base_ref` reaches this decision from the forge's PR JSON — untrusted
/// external content — and the caller reads exactly one line. A newline in an
/// interpolated field would let the tail of a message be parsed as a second
/// (missing) decision, so control characters are flattened here rather than
/// trusted not to appear.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests;
