//! `loom-daemon merge-pr loom-pr-override-comment` (#7419, a slice of #8191).
//!
//! Renders the audit-comment body `_check_loom_pr_label` posts to the PR on a
//! REAL (non-dry-run) `--allow-unapproved` override — byte-frozen from the
//! retired shell, see
//! [`loom_daemon::merge_pr::loom_pr_guard::override_comment`]. Shares the
//! `LOOM-MERGE-PR-COMMENT` sentinel protocol [`super::merge_pr_partial_comment`]
//! established: a sentinel line, then the body verbatim with NO trailing
//! newline, so a daemon predating this verb prints nothing recognisable and
//! the shell warns rather than posting silence over the audit trail.
//!
//! # Labels arrive on stdin, with one caller-added newline stripped
//!
//! Same convention as [`super::merge_pr_loom_pr_guard`]'s own input: a label
//! is forge-controlled text and belongs on a stream, not in argv. The shell
//! call site is `printf '%s\n' "$PR_LABELS" | …`, which appends exactly one
//! newline that `$PR_LABELS` itself never carried (command substitution
//! already stripped it there) — so exactly one trailing `\n` is stripped back
//! off here before rendering, to reproduce `$PR_LABELS`'s own bytes rather
//! than the transport artifact. Reading stdin at all can fail; that is not
//! the same as an empty label set, so it exits 2 rather than rendering with
//! `<none>` silently substituted for "unknown".
//!
//! # Exit code
//!
//! 0 always, once stdin was read and clap has accepted the arguments. There
//! is nothing else here that can fail at runtime — no forge call, no parse —
//! and this body is posted AFTER the mutation it describes (the merge, and
//! the override itself), so a missing body costs only the note (see the
//! module docs on `override_comment`).

use anyhow::Result;
use loom_daemon::merge_pr::loom_pr_guard::override_comment;
use loom_daemon::merge_pr::partial_comment::now_timestamp;
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct LoomPrOverrideCommentArgs {
    /// The merged PR number. Named by the body.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The head SHA the override was recorded against.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// Override the sign-off timestamp (`%Y-%m-%dT%H:%M:%SZ`). Defaults to
    /// now, which is what `merge-pr.sh` uses.
    #[arg(long)]
    at: Option<String>,
}

impl LoomPrOverrideCommentArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = String::new();
        if std::io::stdin().read_to_string(&mut raw).is_err() {
            // Unreadable stdin means an unknown label set, which is not the
            // same as an empty one — do not render `<none>` over a read that
            // never happened.
            eprintln!("merge-pr loom-pr-override-comment: could not read labels from stdin");
            std::process::exit(2);
        }
        let labels = raw.strip_suffix('\n').unwrap_or(&raw);
        let timestamp = self.at.unwrap_or_else(now_timestamp);
        let body = override_comment(&self.pr, &self.head_sha, labels, &timestamp);
        let mut out = std::io::stdout().lock();
        // Sentinel, newline, body — and no trailing newline after the body.
        write!(out, "LOOM-MERGE-PR-COMMENT\n{body}")?;
        out.flush()?;
        Ok(())
    }
}
