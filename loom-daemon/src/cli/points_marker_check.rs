//! `check-points-marker`: validate the points marker of one issue body read
//! from stdin (Issue #9056; hardened by #11663).
//!
//! # Why this exists as a daemon subcommand, not inline shell
//!
//! New executable logic is a `loom-daemon` subcommand, not more portable
//! shell (`.loom/docs/shell-language-policy.md`). The gate itself lives in
//! `loom-daemon curation-complete`, which also reads trusted comments; this
//! subcommand checks a body alone.
//!
//! # Why the body arrives on stdin
//!
//! Same reasoning as [`super::merge_pr_refs`]: the issue body is untrusted
//! external content that can run to tens of kilobytes, so it does not belong
//! in argv.
//!
//! # Message parity
//!
//! The BLOCKED text is [`loom_daemon::points_marker::PointsError`]'s
//! `Display`, the same text `loom-daemon curation-complete` refuses with
//! (#11663), and the body is validated by the same hardened parser.

use loom_daemon::points_marker::scan;
use std::io::Read;

use anyhow::Result;

#[derive(clap::Args)]
pub(crate) struct CheckPointsMarkerArgs {
    /// The issue number — used only to label the success/failure messages
    /// (`ok: <label> is pointed <N>`), never to fetch anything. The body is
    /// read from stdin.
    #[arg(long)]
    issue: String,

    /// The `owner/repo` slug, prefixed onto `--issue` in the success message.
    /// Optional — the message degrades to the bare issue number without it.
    #[arg(long)]
    repo: Option<String>,
}

impl CheckPointsMarkerArgs {
    /// Exit 0 = valid marker (a `ok: … is pointed N` line on stdout); exit 1 =
    /// any [`loom_daemon::points_marker::PointsError`] (the BLOCKED guidance
    /// on stderr).
    pub(crate) fn run(self) -> Result<()> {
        let mut raw_body = Vec::new();
        std::io::stdin().read_to_end(&mut raw_body)?;
        // Lossy rather than fatal — an issue body is whatever the forge
        // returned, and refusing to validate one over invalid UTF-8 would
        // fail curation closed for a reason unrelated to the marker itself.
        let body = String::from_utf8_lossy(&raw_body);

        let label = self
            .repo
            .as_deref()
            .map_or_else(|| self.issue.clone(), |repo| format!("{repo}#{}", self.issue));

        match scan(&body).validate() {
            Ok(points) => println!("ok: {label} is pointed {points}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Ok(())
    }
}
