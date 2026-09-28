//! `loom-daemon merge-pr partial-conflict` (#4569/#4595, a slice of the
//! merge-pr port #8191): the decision half of `merge-pr.sh`'s pre-merge
//! `_check_partial_increment_close_conflict`.
//!
//! # Protocol
//!
//! Reads one NUL-framed record on stdin — `printf '%s\0'` of the PR body, the
//! concatenated commit messages, `forge_pr_close_targets`'s output, then an
//! `(issue number, fresh gh api body)` pair per declared partial increment —
//! and prints the ordered steps the shell replays:
//!
//! | line | the shell does |
//! |---|---|
//! | `OPEN<TAB>n` | append `n` to `$PARTIAL_OPEN_BEFORE_MERGE` |
//! | `CONFLICT<TAB>n` | append `n` to `$PARTIAL_CONFLICT_ISSUES` |
//! | `WARNING<TAB>text` | `warning "$text"` |
//! | `LOOM-PARTIAL-CONFLICT-DONE` | nothing — the plan is complete |
//!
//! # Exit code
//!
//! 0 with a complete, `DONE`-terminated plan; 2 when the frame could not be
//! read or is malformed. The shell refuses the merge on anything but a `DONE`
//! line (a warning under `--dry-run`): both sets feed the post-merge reopen,
//! and an unanswered plan read as empty would leave a declared partial
//! increment closed by this merge with nothing recorded to revert it.
//!
//! Every field arrives on stdin, never in argv: the body and commit messages
//! are forge-controlled text that routinely runs to tens of kilobytes.

use anyhow::Result;
use loom_daemon::merge_pr::partial_conflict::{plan, render, Frame};
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct PartialConflictArgs {
    /// The PR about to merge, for the warning text.
    #[arg(long, value_name = "N")]
    pr: String,

    /// Prefix every warning `[dry-run] ` and phrase it as conditional.
    #[arg(long)]
    dry_run: bool,
}

impl PartialConflictArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = Vec::new();
        // Lossy rather than fatal on bad UTF-8, like `merge-pr-refs`: the
        // retired grep/jq pipelines processed those bytes too, and refusing a
        // merge over them would fail the guard for an unrelated reason.
        let frame = std::io::stdin()
            .read_to_end(&mut raw)
            .ok()
            .and_then(|_| Frame::parse(&String::from_utf8_lossy(&raw)));
        let Some(frame) = frame else {
            eprintln!(
                "merge-pr partial-conflict: stdin was not a NUL-framed \
                 body/commits/close-targets/[issue json]... record"
            );
            std::process::exit(2);
        };
        let mut out = std::io::stdout().lock();
        out.write_all(render(&plan(&frame, &self.pr, self.dry_run)).as_bytes())?;
        out.flush()?;
        Ok(())
    }
}
