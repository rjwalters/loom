//! The `operator-premise` fingerprint — "Checking Operator-Only Premises"
//! (#6849) (epic #7810, PR 4).
//!
//! Answers: has the reference this issue was parked on since closed?

use crate::short_hash::short_sha16;
use serde::Deserialize;

/// One checked reference.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Ref {
    pub number: i64,
    #[serde(default)]
    pub state: String,
}

/// The `--stdin` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Input {
    pub refs: Vec<Ref>,
}

/// What this pass concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub verdict: String,
    pub refs: String,
    /// **Empty when `verdict == "open"`**, and that is a state rather than a
    /// missing value: nothing to report this pass, so there is nothing to
    /// compare either. [`super::decide`] turns it into `none`/no-claim.
    pub conclusion_hash: String,
}

/// One `<ref#>:<state>` line per reference, sorted.
///
/// Lexicographic, like [`super::recheck::blockers`] and for the same reason —
/// the shell's trailing `| sort` wins over the `jq sort_by`, and the ordering
/// feeds the hash.
#[must_use]
pub fn refs_lines(refs: &[Ref]) -> String {
    let mut lines: Vec<String> = refs
        .iter()
        .map(|r| format!("{}:{}", r.number, r.state))
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Compute the fingerprint.
///
/// `stale-premise` iff **any** reference is no longer OPEN — the premise the
/// issue was parked on has moved, which is the thing worth reporting. All-open
/// means the premise still holds, which is a non-event.
#[must_use]
pub fn compute(refs: &[Ref]) -> Outcome {
    let lines = refs_lines(refs);
    let stale = refs.iter().any(|r| r.state != "OPEN");
    if stale {
        // `printf '%s\n%s'` — no trailing newline.
        let hash = short_sha16(&format!("stale-premise\n{lines}"));
        Outcome {
            verdict: "stale-premise".to_string(),
            refs: lines,
            conclusion_hash: hash,
        }
    } else {
        Outcome {
            verdict: "open".to_string(),
            refs: lines,
            conclusion_hash: String::new(),
        }
    }
}

#[cfg(test)]
mod tests;
