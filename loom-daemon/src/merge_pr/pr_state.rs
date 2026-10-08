//! The "is this PR still open for merging?" terminal-state gate that opens
//! `merge-pr.sh`'s flow right after the PR fetch (#8191 slice; the
//! `PR_MERGED == "true"` / `PR_STATE != "closed"` pair before this port).
//!
//! The two inputs are the raw `jq -r` renderings of `.merged` and `.state`
//! from the forge's PR object, so a missing field arrives as the text `null`
//! and is simply "not merged / not closed".
//!
//! Precedence is fixed and matches the retired shell: **merged wins over
//! closed** (a merged PR also reports `state: closed`, and must exit 0 as
//! "already merged", not fail as "closed (not merged)"). Comparisons are exact
//! and case-sensitive, as the shell's `==` was.
//!
//! # Fail direction
//!
//! The shell acts only on a positively received MERGED or CLOSED; anything
//! else (missing or older daemon, non-zero exit, off-protocol output) proceeds
//! as OPEN. That is safe: the gates that follow and the forge's own merge call
//! both refuse a PR that is already merged or closed, so a lost verdict costs a
//! different error message, never a wrong merge.

/// Where the PR is in its lifecycle, for the purposes of this gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    /// Already merged: the run is a no-op success.
    Merged,
    /// Closed without merging: the run is refused.
    Closed,
    /// Neither: continue to the merge gates.
    Open,
}

impl PrState {
    /// The protocol line the shell matches.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            PrState::Merged => "LOOM-PR-STATE MERGED",
            PrState::Closed => "LOOM-PR-STATE CLOSED",
            PrState::Open => "LOOM-PR-STATE OPEN",
        }
    }
}

/// Classify from the raw `.state` and `.merged` text.
#[must_use]
pub fn classify(state: &str, merged: &str) -> PrState {
    if merged == "true" {
        PrState::Merged
    } else if state == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    }
}

#[cfg(test)]
mod tests;
