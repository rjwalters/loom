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
//! not an error, and the caller distinguishes them by an empty result.

use std::io::Read;

use anyhow::Result;

use loom_daemon::merge_pr::{backticked_trailers, refs};

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

    /// Ready-to-print advisory warnings for whole-line `Part of #N` trailers
    /// that were written inside a code span and therefore parse as no
    /// declaration at all (#8796). One complete message per line, empty when
    /// there is nothing to say. Backs the advisory call folded into
    /// `_check_partial_increment_close_conflict` in `merge-pr.sh`.
    ///
    /// The caller re-emits each line through its own `warning`, so the whole
    /// message is composed here — `merge-pr.sh` is frozen by the file-size
    /// and shell-budget ratchets and cannot afford to carry the text.
    BacktickedTrailerWarnings {
        /// The PR the body belongs to, named in the warning.
        #[arg(long, value_name = "N")]
        pr: u64,
        /// Report the would-be outcome without claiming a merge is happening.
        #[arg(long)]
        dry_run: bool,
    },
}

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
            MergePrRefsCommand::PartialIncrementRefSnippets { issue } => {
                let s = refs::partial_increment_ref_snippets(&body, issue);
                if !s.is_empty() {
                    println!("{s}");
                }
            }
            MergePrRefsCommand::BacktickedTrailerWarnings { pr, dry_run } => {
                for line in backticked_trailers::backticked_trailer_warnings(&body, pr, dry_run) {
                    println!("{line}");
                }
            }
        }
        Ok(())
    }
}
