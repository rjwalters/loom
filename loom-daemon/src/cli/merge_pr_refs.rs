//! `loom-daemon merge-pr-refs` — the closing-reference analysis behind
//! `merge-pr.sh`'s partial-increment guard (#8191, epic #7810 slice 1).
//!
//! # Why the body arrives on stdin
//!
//! A PR body is **untrusted external content** (`defaults/docs/
//! untrusted-external-content.md`) and routinely runs to tens of kilobytes.
//! Passing it as an argv element risks `E2BIG` on a long body and puts
//! attacker-controlled bytes through another layer of shell quoting. stdin has
//! neither problem and matches `_closing_refs_stdin`'s existing shape.
//!
//! # Output contract
//!
//! One value per line on stdout, exactly as the shell functions emitted, so
//! the caller's `while read` / `$(...)` substitution is unchanged. Exit is
//! always 0 for a well-formed invocation — "no references found" is an answer,
//! not an error, and the caller distinguishes them by an empty result. The
//! one exception is `has-unnegated-closing-ref`, a tri-state predicate: exit
//! 0 (unnegated found) / 1 (negated only) / `NO_REFERENCE_EXIT` (no
//! reference at all).

use std::io::Read;

use anyhow::Result;

use loom_daemon::merge_pr::refs;

#[derive(clap::Subcommand)]
pub(crate) enum MergePrRefsCommand {
    /// Issue numbers declared `Part of #N` / `Contributes to #N`, one per line,
    /// deduped and numerically ascending. Backs `_partial_increment_refs`.
    PartialIncrementRefs,

    /// Issue numbers referenced with a GitHub closing keyword, one per line,
    /// deduped and numerically ascending. Backs `_body_closing_refs` and
    /// `_closing_refs_stdin`.
    ClosingRefs,

    /// The literal closing-keyword snippets referencing `--issue`, rendered
    /// `snippet", "snippet`. Backs `_closing_ref_snippets`.
    ClosingRefSnippets {
        #[arg(long, value_name = "N")]
        issue: u64,
    },

    /// The literal `Part of #N` declaration snippets for `--issue`, rendered
    /// the same way. Backs `_partial_increment_ref_snippets`.
    PartialIncrementRefSnippets {
        #[arg(long, value_name = "N")]
        issue: u64,
    },

    /// The non-blocking pre-merge warning for a whole-line, code-span-wrapped
    /// `Part of #N` / `Contributes to #N` trailer (#5690, ported #8831 — backs
    /// the inline call site in `defaults/scripts/merge-pr.sh`'s
    /// `_check_partial_increment_close_conflict`, formerly the shell function
    /// `_warn_backticked_partial_increment_trailers`). Computes both the
    /// backticked and the plain-text declaration sets from the same body and
    /// diffs them itself, so the caller passes nothing but the PR number.
    /// Prints one finding (two lines) per undeclared backticked issue, empty
    /// when there is nothing to warn about; always exits 0.
    BackticksPartialIncrementWarnings {
        #[arg(long, value_name = "N")]
        pr: String,

        /// Prefix every line `[dry-run] `, matching the #4569/#4595 conflict
        /// warnings' contract.
        #[arg(long)]
        dry_run: bool,
    },

    /// Tri-state read of the text's closing-keyword references to `--issue`
    /// (#1057). Exit 0 when at least one such reference is NOT negated
    /// (`does not fix #N` does not count, but a later unnegated `fixes #N`
    /// does); exit 1 when every reference found is negated and there is at
    /// least one; exit `NO_REFERENCE_EXIT` (3) when the text carries no
    /// closing-keyword reference to `--issue` at all — this is deliberately
    /// distinct from 1 so a caller cannot treat "never mentioned" the same as
    /// "mentioned and disclaimed" (a body with no textual reference — e.g. an
    /// issue linked only through the PR's Development sidebar — must not be
    /// read as negated). Prints nothing. Backs Champion's "Verify Issue
    /// Auto-Close" cross-check; any other exit (e.g. clap's 2 on a daemon too
    /// old to know this verb) is "could not answer", never "negated".
    HasUnnegatedClosingRef {
        #[arg(long, value_name = "N")]
        issue: u64,
    },
}

/// Exit code for [`MergePrRefsCommand::HasUnnegatedClosingRef`] when the text
/// carries no closing-keyword reference to `--issue` at all. Distinct from
/// both 0 (unnegated found) and 1 (negated-only found) so a caller can tell
/// "never mentioned" apart from "mentioned and disclaimed" — conflating them
/// is what let `champion-pr-merge.md`'s Step 4 reopen issues GitHub had
/// closed correctly through channels this predicate's regex cannot see (the
/// Development sidebar, `Fixes owner/repo#N`, `Closes: #N`).
pub(crate) const NO_REFERENCE_EXIT: i32 = 3;

impl MergePrRefsCommand {
    pub(crate) fn run(self) -> Result<()> {
        let mut body = String::new();
        // Lossy rather than fatal: a PR body is whatever the forge returned,
        // and refusing to analyse one because it carries invalid UTF-8 would
        // fail the merge guard closed for a reason unrelated to the guard.
        // The shell's grep would have processed those bytes too.
        let mut raw = Vec::new();
        std::io::stdin().read_to_end(&mut raw)?;
        body.push_str(&String::from_utf8_lossy(&raw));

        match self {
            MergePrRefsCommand::PartialIncrementRefs => {
                for n in refs::partial_increment_refs(&body) {
                    println!("{n}");
                }
            }
            MergePrRefsCommand::ClosingRefs => {
                for n in refs::closing_refs(&body) {
                    println!("{n}");
                }
            }
            MergePrRefsCommand::ClosingRefSnippets { issue } => {
                let s = refs::closing_ref_snippets(&body, issue);
                if !s.is_empty() {
                    println!("{s}");
                }
            }
            MergePrRefsCommand::HasUnnegatedClosingRef { issue } => {
                match refs::closing_ref_negation_status(&body, issue) {
                    refs::ClosingRefNegationStatus::Unnegated => {}
                    refs::ClosingRefNegationStatus::NegatedOnly => std::process::exit(1),
                    refs::ClosingRefNegationStatus::NoReference => {
                        std::process::exit(NO_REFERENCE_EXIT)
                    }
                }
            }
            MergePrRefsCommand::PartialIncrementRefSnippets { issue } => {
                let s = refs::partial_increment_ref_snippets(&body, issue);
                if !s.is_empty() {
                    println!("{s}");
                }
            }
            MergePrRefsCommand::BackticksPartialIncrementWarnings { pr, dry_run } => {
                print!("{}", refs::backticked_partial_increment_warnings(&body, &pr, dry_run));
            }
        }
        Ok(())
    }
}
