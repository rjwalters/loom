//! `require-complexity-marker.sh`'s points-marker validation (Issue #9056),
//! epic #7810 shell-budget family.
//!
//! # Why this exists as a daemon subcommand, not inline shell
//!
//! Adding the `<!-- loom:points=<N> -->` check as inline bash grew
//! `require-complexity-marker.sh`'s `contract`-category line count, which the
//! epic's `shell-budget` CI gate ratchets DOWN, never up (`ScriptPortCommand::ShellBudget`).
//! `.loom/docs/shell-language-policy.md`'s answer is unconditional: new
//! executable logic is a `loom-daemon` subcommand, not more portable shell.
//! This subcommand is that move for the points half of the gate; the
//! complexity-tier half stays inline shell, unchanged, exactly as it was
//! before #9056.
//!
//! # Why the body arrives on stdin
//!
//! Same reasoning as [`super::merge_pr_refs`]: the issue body is untrusted
//! external content that can run to tens of kilobytes, so it does not belong
//! in argv. It is also already in the shell script's `$body` variable (fetched
//! once, for the complexity check) — piping it in costs no second `gh` call.
//!
//! # Message parity
//!
//! Deliberately mirrors `require-complexity-marker.sh`'s pre-#9056 message
//! text (its three-way `case`: valid / absent / out-of-vocabulary) so its
//! stdout/stderr shape — and this repo's own shell-test assertions — are
//! unaffected by which language implements the check.

use loom_daemon::points_marker::{extract_points_marker_raw, POINTS_VALUES};
use std::io::Read;

use anyhow::Result;

#[derive(clap::Args)]
pub(crate) struct CheckPointsMarkerArgs {
    /// The issue number — used only to label the success/failure messages
    /// (`ok: <label> is pointed <N>`), never to fetch anything. The body is
    /// read from stdin.
    #[arg(long)]
    issue: String,

    /// The `owner/repo` slug, prefixed onto `--issue` in the success message
    /// the same way `require-complexity-marker.sh`'s own `$REPO#$ISSUE`
    /// already reads. Optional — the message degrades to the bare issue
    /// number without it.
    #[arg(long)]
    repo: Option<String>,
}

impl CheckPointsMarkerArgs {
    /// Exit 0 = valid marker (a `ok: … is pointed N` line on stdout); exit 1 =
    /// missing or out-of-vocabulary (the BLOCKED guidance on stderr, matching
    /// the shell case it replaces).
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

        match extract_points_marker_raw(&body) {
            None => {
                eprintln!(
                    "BLOCKED: issue has no points estimate marker.\n\n\
                     Add exactly one of these to the issue body before applying loom:curated:\n\n  \
                     <!-- loom:points=1 -->    <!-- loom:points=2 -->    <!-- loom:points=3 -->\n  \
                     <!-- loom:points=5 -->    <!-- loom:points=8 -->    <!-- loom:points=13 -->\n\n\
                     Loose guidance: mechanical -> 1-2, routine -> 3-5, complex -> 8-13."
                );
                std::process::exit(1);
            }
            Some(raw) if POINTS_VALUES.contains(&raw) => {
                println!("ok: {label} is pointed {raw}");
            }
            Some(raw) => {
                eprintln!(
                    "BLOCKED: issue {} has an invalid points value '{raw}' (expected one of 1, 2, 3, 5, 8, 13)",
                    self.issue
                );
                std::process::exit(1);
            }
        }
        Ok(())
    }
}
