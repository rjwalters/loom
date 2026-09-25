//! The diagnostic attached to a failed `loom-daemon-update.sh` run (Issue
//! #8712) — and the tail truncation it is built on.
//!
//! # The incident
//!
//! On loom-worker-1 a test fixture's 472-byte bash fake was sitting at
//! `loom-daemon/target/release/loom-daemon`, the path
//! `loom_resolve_self_daemon_bin` prefers for the binary that *implements*
//! `release-fetch`. Every `auto_update` fetch failed for hours (attempts 1–5,
//! exponential backoff to 960s) and the host could not roll 0.19.269 →
//! 0.19.297.
//!
//! What the daemon log carried was the run's LAST line:
//!
//! ```text
//! loom_locate_daemon_bin: resolved /home/ubuntu/.local/bin/loom-daemon via $PATH (mtime …)
//! ```
//!
//! That is a *successful resolution* trace. The line that actually explained
//! the failure —
//!
//! ```text
//! fake loom-daemon: unsupported subcommand: release-fetch --repo-root … --tag v0.19.297
//! ```
//!
//! — was further up, and only surfaced when an operator ran the script by
//! hand. A tail alone is not enough: whether the reason survives depends on how
//! chatty the run happened to be *after* it, which is exactly the property a
//! diagnostic must not have.
//!
//! # What this module does
//!
//! [`failure_digest`] HOISTS the first failure-shaped line to the front, then
//! appends the ordinary tail. Both, never one or the other: the hoisted line
//! names the reason, the tail keeps the surrounding context that says which
//! stage produced it. When no line is failure-shaped — the common case for an
//! ordinary `cargo build` error, which the tail already carries — the output is
//! byte-identical to the pre-#8712 behaviour.
//!
//! The hoist is UNCONDITIONAL, not "only when the tail dropped the line".
//! Skipping it whenever the line already appears in the tail would save one
//! duplicated line and give up the only property that makes this worth doing:
//! that the reason is at a FIXED position. This digest is not printed on its
//! own — [`super::AutoUpdateState::record_roll`] embeds it at the end of a
//! prefix (`…failed (attempt 3, backing off 480s): {msg}`) and the whole thing
//! becomes one `log::warn!` entry, so a multi-line tail buries the reason
//! mid-entry exactly as it was buried on loom-worker-1. Hoisting always means
//! an operator (or a `grep` over the daemon log) finds the reason immediately
//! after the `: `, whatever the run's length or chattiness — which is the
//! precise property the incident showed was missing.

/// Max bytes of captured script output retained in a failure/roll log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

/// Prefixes that mark a line as the REASON a run failed, rather than narration
/// around it. Matched case-insensitively against the start of a trimmed line.
///
/// `fake` is in the list on purpose and is not hypothetical: it is the literal
/// first word of the stub `defaults/scripts/tests/lib/daemon-update-fixtures.sh`
/// writes, which is what was found on the host.
///
/// Deliberately a small, closed list of line *starts* rather than a substring
/// search for "error": the script prints plenty of prose containing the word
/// (`…exits 1 for "no artifact resolved"…`), and hoisting one of those to the
/// front would make the digest actively misleading — worse than the tail alone.
const FAILURE_LINE_PREFIXES: [&str; 6] = ["fake", "error", "[error]", "fatal", "err:", "err "];

/// The diagnostic attached to a non-success outcome — see the module docs.
pub(super) fn failure_digest(s: &str) -> String {
    let tail = truncate_tail(s);
    match first_failure_line(s) {
        Some(line) => format!("{line} | output tail: {tail}"),
        None => tail,
    }
}

/// The first line of `s` whose trimmed, lowercased form starts with one of
/// [`FAILURE_LINE_PREFIXES`]. The FIRST, not the last: a failing script tends
/// to report the root cause and then unwind, so the earliest failure-shaped
/// line is the most specific one.
fn first_failure_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|line| {
        let lowered = line.to_ascii_lowercase();
        FAILURE_LINE_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
    })
}

/// Keep only the last [`MAX_OUTPUT_TAIL_BYTES`] bytes of captured output,
/// trimmed, on a char boundary.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    let start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s[start..].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported output shape, verbatim in its essentials: the real reason
    /// in the middle, a benign successful-resolution trace last.
    const REPORTED_OUTPUT: &str = "\
==> Checking for updates
fake loom-daemon: unsupported subcommand: release-fetch --repo-root /home/ubuntu/loom --tag v0.19.297
[ERROR] artifact fetch failed
loom_locate_daemon_bin: resolved /home/ubuntu/.local/bin/loom-daemon via $PATH (mtime: 2026-09-22 13:45:00)
";

    #[test]
    fn hoists_the_real_reason_ahead_of_a_benign_final_line() {
        let digest = failure_digest(REPORTED_OUTPUT);
        assert!(
            digest.starts_with("fake loom-daemon: unsupported subcommand: release-fetch"),
            "digest should LEAD with the failure reason, got: {digest}"
        );
    }

    #[test]
    fn short_output_keeps_every_line_so_the_reason_is_never_alone() {
        // The whole point of keeping the tail as well: the hoisted line says
        // WHAT failed, the tail says where in the run it happened.
        let digest = failure_digest(REPORTED_OUTPUT);
        assert!(digest.contains("==> Checking for updates"), "{digest}");
        assert!(digest.contains("loom_locate_daemon_bin: resolved"), "{digest}");
    }

    #[test]
    fn reason_survives_an_output_longer_than_the_tail_window() {
        // The regression that matters: with the reason pushed out of the tail
        // window by chatter, a tail-only digest loses it entirely.
        let mut log = String::from(
            "fake loom-daemon: unsupported subcommand: release-fetch --tag v0.19.297\n",
        );
        log.push_str(&"filler line that says nothing useful\n".repeat(200));
        assert!(log.len() > MAX_OUTPUT_TAIL_BYTES);
        let tail_only = truncate_tail(&log);
        assert!(!tail_only.contains("unsupported subcommand"));

        let digest = failure_digest(&log);
        assert!(digest.starts_with("fake loom-daemon: unsupported subcommand"), "{digest}");
        assert!(digest.contains("| output tail: "), "{digest}");
    }

    #[test]
    fn matches_the_documented_prefixes_case_insensitively() {
        for line in [
            "ERROR: something broke",
            "[ERROR] something broke",
            "Fatal: something broke",
            "err: something broke",
        ] {
            let log = format!("noise\n{line}\n{}", "trailing\n".repeat(400));
            let digest = failure_digest(&log);
            assert!(digest.starts_with(line), "{line} was not hoisted: {digest}");
        }
    }

    #[test]
    fn no_failure_shaped_line_is_byte_identical_to_the_plain_tail() {
        let log = "cargo build failed: could not compile `loom-daemon`\nwarning: unused import\n";
        assert_eq!(failure_digest(log), truncate_tail(log));
    }

    #[test]
    fn a_word_containing_error_mid_line_is_not_mistaken_for_the_reason() {
        // `…exits 1 for "no artifact resolved"…`-style prose must not be
        // hoisted; only a line that STARTS failure-shaped counts.
        let log = "resolve-json exits 1 on an error, which is data, not a failure\nrc=1\n";
        assert_eq!(failure_digest(log), truncate_tail(log));
    }

    #[test]
    fn truncate_tail_round_trips_short_text_unchanged() {
        let raw = "short output";
        assert_eq!(truncate_tail(raw), raw);
    }

    #[test]
    fn truncate_tail_cuts_on_a_char_boundary() {
        // A multi-byte char straddling the byte window start must not panic or
        // produce invalid UTF-8.
        let raw = "é".repeat(MAX_OUTPUT_TAIL_BYTES);
        let tail = truncate_tail(&raw);
        assert!(tail.len() <= MAX_OUTPUT_TAIL_BYTES);
        assert!(tail.chars().all(|c| c == 'é'));
    }
}
