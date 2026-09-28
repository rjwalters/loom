//! `loom-daemon merge-pr checks-failure` (#8191 slice): the failing-check
//! overlap classification inside `_wait_for_checks_then_sync_merge`'s poll
//! loop.
//!
//! # Protocol
//!
//! Reads one NUL-framed record on stdin — `printf '%s\0%s\0%s\0' "$failing"
//! "$required" "$pending"`, each a newline-separated check-name list (empty
//! for none) — and prints exactly one sentinel line:
//!
//! | stdout | exit | the shell does |
//! |---|---|---|
//! | `LOOM-CHECK-FAILURE-REQUIRED<TAB>names…` | 1 | `error` — refuse the merge, naming the required check(s) |
//! | `LOOM-CHECK-FAILURE-PROCEED` | 0 | `info` then `return 0` — synchronous merge is safe |
//! | `LOOM-CHECK-FAILURE-PENDING` | 0 | fall through unchanged to the pending-wait branch below |
//!
//! Anything else — a missing/older binary, a clap usage error, silence, a
//! malformed frame — must be read by the caller as "never classified" and
//! refuse the merge (exit 2 here, matching every other guard fault in this
//! family): a caller that cannot tell "only informational checks failing"
//! from "did not run" would let an unclassified required failure through.
//!
//! Every field arrives on stdin, never in argv — check names are
//! forge/workflow-author-controlled text with no length or character bound.

use anyhow::Result;
use loom_daemon::merge_pr::checks_failure::{classify, Verdict};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct ChecksFailureArgs {
    /// The PR about to merge, for the malformed-frame diagnostic (the
    /// decision itself does not otherwise need it, since the shell's `error`
    /// already names the PR when this refuses the merge).
    #[arg(long, value_name = "N")]
    pr: String,
}

impl ChecksFailureArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = Vec::new();
        let read_ok = std::io::stdin().read_to_end(&mut raw).is_ok();
        // Lossy rather than fatal on bad UTF-8, like `merge-pr-refs`/
        // `partial-conflict`: check names are display text, not something a
        // refusal needs byte-exact.
        let frame = if read_ok {
            parse_frame(&String::from_utf8_lossy(&raw))
        } else {
            None
        };
        let Some((failing, required, pending)) = frame else {
            eprintln!(
                "merge-pr checks-failure: PR #{}: stdin was not a NUL-framed \
                 failing/required/pending record",
                self.pr
            );
            std::process::exit(2);
        };

        match classify(&failing, &required, pending) {
            Verdict::RequiredFailed(overlap) => {
                println!("LOOM-CHECK-FAILURE-REQUIRED\t{}", overlap.join(" "));
                std::process::exit(1);
            }
            Verdict::InformationalOnly => {
                println!("LOOM-CHECK-FAILURE-PROCEED");
            }
            Verdict::StillPending => {
                println!("LOOM-CHECK-FAILURE-PENDING");
            }
        }
        Ok(())
    }
}

/// Parse `printf '%s\0%s\0%s\0' failing required pending` into the
/// failing/required name lists plus whether anything is pending. `None` when
/// the frame does not have exactly three NUL-terminated fields.
///
/// Fidelity to the retired shell, deliberately:
/// - names are whole lines, NOT trimmed — `comm -12` compared lines exactly,
///   so `" CI"` never matched a required `"CI"`; blank lines are dropped
///   (an empty line can never survive `comm -12` into a non-empty `$overlap`
///   once `$(...)` strips trailing newlines, so dropping them is equivalent);
/// - `pending` is the raw field's non-emptiness, exactly `[[ -z "$pending" ]]`
///   — deriving it from a blank-filtered list would read a whitespace-only
///   pending name as "nothing pending" and proceed early (fail-open).
fn parse_frame(raw: &str) -> Option<(Vec<String>, Vec<String>, bool)> {
    let mut fields: Vec<&str> = raw.split('\0').collect();
    // Every field is NUL-terminated, so the split leaves one empty tail.
    if fields.pop() != Some("") || fields.len() != 3 {
        return None;
    }
    let names = |s: &str| -> Vec<String> {
        s.split('\n')
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    };
    Some((names(fields[0]), names(fields[1]), !fields[2].is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_field_frame() {
        let raw = "A\nB\0C\0D\n\nE\0";
        let (failing, required, pending) = parse_frame(raw).unwrap();
        assert_eq!(failing, vec!["A", "B"]);
        assert_eq!(required, vec!["C"]);
        assert!(pending);
    }

    #[test]
    fn empty_fields_parse_as_empty_lists() {
        let (failing, required, pending) = parse_frame("\0\0\0").unwrap();
        assert!(failing.is_empty());
        assert!(required.is_empty());
        assert!(!pending);
    }

    #[test]
    fn names_are_not_trimmed_like_comm() {
        let (failing, required, _) = parse_frame(" CI\0CI\0\0").unwrap();
        assert_eq!(failing, vec![" CI"]);
        assert_eq!(required, vec!["CI"]);
    }

    #[test]
    fn whitespace_only_pending_still_counts_as_pending() {
        // `[[ -z " " ]]` is false: the retired shell kept waiting.
        let (_, _, pending) = parse_frame("Lint\0CI\0 \0").unwrap();
        assert!(pending);
    }

    #[test]
    fn wrong_field_count_is_rejected() {
        assert!(parse_frame("A\0B\0").is_none());
        assert!(parse_frame("A\0B\0C\0D\0").is_none());
        assert!(parse_frame("no nul terminator").is_none());
    }
}
