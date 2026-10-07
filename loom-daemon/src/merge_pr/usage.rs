//! `merge-pr.sh --help`'s usage text, a slice of the merge-pr port #8191.
//!
//! # What this owns
//!
//! The 147-line `show_help` heredoc: the option list, the cleanup /
//! local-branch / primary-checkout semantics, the cleanup precedence table,
//! the **exit-code table** and the examples. It was the largest remaining
//! block of pure data in `merge-pr.sh` — about an eighth of the script — and
//! nothing about it needs a shell: it reads no state, expands no variable
//! (the heredoc delimiter was unquoted, but the body contains no `$`,
//! backtick or backslash, so the bytes `cat` printed are exactly the bytes
//! written), and runs before any git/forge initialization.
//!
//! The text lives in `usage.txt` beside this file, pulled in with
//! [`include_str!`], so it stays diffable as plain text and is never subject
//! to `rustfmt` reflowing or raw-string escaping.
//!
//! # Why the exit-code table matters most
//!
//! Role prompts and `champion-pr-merge.md` branch on `merge-pr.sh`'s exit
//! codes (0 / 1 / 3 / 4 / 5 / 6). This text is where an operator reads what
//! each one means, so it is byte-frozen against the retired heredoc by
//! `tests/merge_pr_usage_differential.rs` rather than paraphrased.
//!
//! # Fail direction
//!
//! Open, and toward exit 0. A daemon that predates this verb answers clap's
//! `unrecognized subcommand` on stderr with exit 2 and no
//! [`USAGE_SENTINEL`] line; `merge-pr.sh` then prints a short usage line plus
//! a pointer to this verb and still exits 0. `--help` never gates anything:
//! no caller branches on it beyond "it printed usage", and a degraded help
//! screen must not be mistaken for a failed merge (exit 1) by a caller that
//! passed `--help` while probing the script.

/// First line of `loom-daemon merge-pr usage`'s stdout. The shell prints the
/// remainder only when it sees this line, so an older daemon, an unrelated
/// binary at `$LOOM_DAEMON_BIN`, or a stub on `PATH` can never have its own
/// output passed off as `merge-pr.sh`'s help.
pub const USAGE_SENTINEL: &str = "LOOM-MERGE-PR-USAGE";

/// The usage text, byte-for-byte what the retired `show_help` heredoc
/// printed: 147 lines, ending in exactly one newline.
pub const USAGE: &str = include_str!("usage.txt");

/// The whole stdout of `loom-daemon merge-pr usage`: the sentinel, a
/// newline, then [`USAGE`] verbatim.
#[must_use]
pub fn render() -> String {
    format!("{USAGE_SENTINEL}\n{USAGE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_ends_in_exactly_one_newline() {
        // `cat << EOF` prints every body line newline-terminated and nothing
        // after the last one; the shell restores exactly that one newline
        // after `$(...)` strips it, so a second one would print a blank line.
        assert!(USAGE.ends_with('\n'));
        assert!(!USAGE.ends_with("\n\n"));
        assert_eq!(USAGE.lines().count(), 147);
    }

    #[test]
    fn usage_has_no_shell_expandable_bytes() {
        // The retired heredoc was UNQUOTED (`<< EOF`). It only printed these
        // bytes literally because none of them was special; if one is ever
        // added here the frozen-fixture differential would still pass while
        // describing a text the shell never could have printed.
        assert!(!USAGE.contains('$'));
        assert!(!USAGE.contains('`'));
        assert!(!USAGE.contains('\\'));
    }

    #[test]
    fn usage_documents_the_flags_and_exit_codes_callers_rely_on() {
        for needle in [
            "Usage: ./.loom/scripts/merge-pr.sh <pr-number> [options]",
            "--auto",
            "--dry-run",
            "--no-cleanup-worktree",
            "--worktree-path <dir>",
            "--allow-unapproved",
            "--merge-method M",
            "-h, --help",
            "Exit codes:",
            "0 = merged (or --help) · 1 = failed",
            "3 = PR head moved past the SHA",
            "4 = stale required checks",
            "6 = deferred behind another PR's chain-head merge lock",
        ] {
            assert!(USAGE.contains(needle), "usage text lost {needle:?}");
        }
    }

    #[test]
    fn render_is_sentinel_line_then_usage_verbatim() {
        let out = render();
        let (first, rest) = out.split_once('\n').expect("sentinel line");
        assert_eq!(first, USAGE_SENTINEL);
        assert_eq!(rest, USAGE);
        assert!(rest.starts_with("Loom PR Merge - Worktree-safe merge"));
    }
}
