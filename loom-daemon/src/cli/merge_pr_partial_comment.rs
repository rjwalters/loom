//! `loom-daemon merge-pr partial-comment` (#3667 / #4569, a slice of the
//! merge-pr port #8191): the two audit-comment bodies `merge-pr.sh`'s
//! post-merge partial-increment pass posts.
//!
//! # Protocol
//!
//! Prints a sentinel line, then the comment body VERBATIM:
//!
//! ```text
//! LOOM-MERGE-PR-COMMENT
//! ## Partial Increment Merged
//! …
//! ```
//!
//! The shell strips everything up to and including the first newline and posts
//! the rest through `forge_gh_comment_rl_safe`. The body itself carries no
//! trailing newline (the retired `comment="…"` ended at its closing quote) and
//! none is added, so a consumer that does not go through `$(...)` — which
//! would have stripped one anyway — still sees the retired bytes.
//!
//! # Why a sentinel at all, for a verb that only renders text
//!
//! Because the failure this wrapper has to survive is a loom-daemon PREDATING
//! the verb, which exits non-zero having printed nothing, and the consumer of
//! this output is `gh issue comment`. Without a positive marker the natural
//! shell shape (`comment="$(… || true)"`) posts an EMPTY comment over the
//! explanation an operator needs — a silent, permanent, forge-visible
//! corruption of the audit trail, produced by the degraded path rather than
//! the healthy one. With the marker, "no marker" is the only thing a caller
//! can conclude, and it warns instead.
//!
//! # Exit code
//!
//! 0 always, once clap has accepted the arguments. There is nothing here that
//! can fail at runtime: no I/O, no forge call, no parse. That is deliberate —
//! the seam fails OPEN because both comments are posted AFTER the mutation
//! they describe (see `merge_pr::partial_comment`), so the only thing a
//! missing body costs is the note, never the reopen, the label swap, or the
//! merge.
//!
//! # `--at`
//!
//! Defaults to now, in the retired `date -u +%Y-%m-%dT%H:%M:%SZ` format. It is
//! overridable so the body is renderable deterministically — by the
//! differential harness, and by anyone reproducing a posted comment by hand.
//! `merge-pr.sh` never passes it: the retired shell read its own clock at the
//! moment of the post, and so does this.

use anyhow::Result;
use loom_daemon::merge_pr::partial_comment::{
    now_timestamp, partial_merged_comment, premature_close_comment,
};
use std::io::Write;

/// Which of the pass's two comments to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum CommentKind {
    /// `## Partial Increment Merged` (#3667) — posted after the
    /// `loom:building` → `loom:issue` swap.
    PartialMerged,
    /// `## Premature Auto-Close Reverted` (#4569) — posted after a reopen.
    PrematureClose,
}

#[derive(clap::Args)]
pub(crate) struct PartialCommentArgs {
    /// Which comment body to render.
    #[arg(long, value_enum)]
    kind: CommentKind,

    /// The issue the comment is posted on. Used only by `premature-close`,
    /// whose text names it five times; accepted (and ignored) for
    /// `partial-merged` so the shell's two call sites stay the same shape.
    #[arg(long, default_value = "")]
    issue: String,

    /// The PR that merged. Named by both bodies.
    #[arg(long, default_value = "")]
    pr: String,

    /// `partial-merged` only: this pass reopened the issue first (#4569), so
    /// the body leads with the `**Reopened**` bullet.
    #[arg(long)]
    reopened: bool,

    /// Override the sign-off timestamp (`%Y-%m-%dT%H:%M:%SZ`). Defaults to
    /// now, which is what `merge-pr.sh` uses.
    #[arg(long)]
    at: Option<String>,
}

impl PartialCommentArgs {
    pub(crate) fn run(self) -> Result<()> {
        let timestamp = self.at.unwrap_or_else(now_timestamp);
        let body = match self.kind {
            CommentKind::PartialMerged => {
                partial_merged_comment(&self.pr, self.reopened, &timestamp)
            }
            CommentKind::PrematureClose => {
                premature_close_comment(&self.issue, &self.pr, &timestamp)
            }
        };
        let mut out = std::io::stdout().lock();
        // Sentinel, newline, body — and no trailing newline after the body.
        write!(out, "LOOM-MERGE-PR-COMMENT\n{body}")?;
        out.flush()?;
        Ok(())
    }
}
