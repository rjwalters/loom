//! `loom-daemon merge-pr head-sync-retry` (#8164, slice 4 of #8191).
//!
//! # Authorizing a retry requires a POSITIVE signal
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | the head moved by our own base-sync | `LOOM-HEAD-SELF-SYNC-RETRY <sha>` | 0 |
//! | the head moved for any other reason | the refusal reason | 1 |
//! | attribution could not be established | the reason | 2 |
//!
//! The caller retries **only** on exit 0 *and* the sentinel. Everything else —
//! exit 1, exit 2, an old binary that does not know this subcommand, no binary
//! at all, a silent success — falls through to `error_head_moved`, which exits
//! 3 and re-queues the PR.
//!
//! That is why this guard's helper-missing behaviour needs no new exit-code
//! convention (the `LOOM_SCRIPT_HELPER_MISSING_RC` question #8191 raises):
//! **an unresolvable binary lands on the pre-#8164 behaviour exactly**, which
//! is a re-queue. It cannot be mistaken for "merged" (nothing merges without
//! the forge accepting a precondition this guard never supplies) and it is not
//! a permanent refusal (a re-queue is re-evaluated on the next pass). The
//! degraded mode is the status quo ante, so the fix can only ever add merges
//! that would otherwise have been re-queued — never remove a refusal.
//!
//! `--from-stdin` takes the whole evidence as JSON and does no forge reads —
//! the deterministic seam the shell suite drives, and a debugging facility
//! (paste a real head's parents, see which clause refused).

use anyhow::Result;
use loom_daemon::merge_pr::head_sync::{
    classify, foreign_message, retry_message, Evidence, Verdict, RETRY,
};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct HeadSyncRetryArgs {
    /// The PR number, for the messages.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo. Unused with `--from-stdin`.
    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    /// The head SHA the refused merge was gated on.
    #[arg(long, value_name = "SHA", default_value = "")]
    precondition_sha: String,

    /// This run called `forge_update_branch` (pushed to the head branch)
    /// before the refused attempt. Without it no head move is attributable.
    #[arg(long)]
    self_synced: bool,

    /// The single re-read-and-retry is already spent.
    #[arg(long)]
    retry_used: bool,

    /// The caller established the head-mismatch by exit code rather than by
    /// the response text (`loom-daemon forge auto-merge`'s exit 4).
    #[arg(long)]
    mismatch_confirmed: bool,

    /// Read the complete evidence as JSON from stdin instead of querying the
    /// forge (suites/debug). Without it, stdin carries the forge's refusal
    /// text — untrusted external content, which belongs on a stream rather
    /// than in an argument vector.
    #[arg(long)]
    from_stdin: bool,
}

impl HeadSyncRetryArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            // Unreadable stdin is an unknown response, not an empty one.
            println!("could not read the merge response from stdin");
            std::process::exit(2);
        }

        let evidence = if self.from_stdin {
            match self.stdin_evidence(&buf) {
                Ok(ev) => ev,
                Err(why) => {
                    println!("{}", foreign_message(&self.pr, &why));
                    std::process::exit(2);
                }
            }
        } else {
            match loom_daemon::merge_pr::head_sync::fetch::live_inputs(&self.repo, &self.pr) {
                Ok(live) => Evidence {
                    response: buf,
                    mismatch_confirmed: self.mismatch_confirmed,
                    self_synced: self.self_synced,
                    retry_used: self.retry_used,
                    precondition_sha: self.precondition_sha.clone(),
                    current_head_sha: live.current_head_sha,
                    head_parents: live.head_parents,
                    second_parent_in_base: live.second_parent_in_base,
                },
                Err(why) => {
                    println!("{}", foreign_message(&self.pr, &why));
                    std::process::exit(2);
                }
            }
        };

        match classify(&evidence) {
            Verdict::SelfSyncRetry { new_head } => {
                // Sentinel FIRST on the line: the caller matches the prefix
                // and takes the SHA from the end, so neither half can be
                // satisfied by incidental output.
                println!("{RETRY} {new_head}");
                eprintln!("{}", retry_message(&self.pr, &evidence.precondition_sha, &new_head));
                std::process::exit(0);
            }
            Verdict::Foreign(why) => {
                println!("{}", foreign_message(&self.pr, &why));
                std::process::exit(1);
            }
        }
    }

    /// The `--from-stdin` payload: everything [`live_inputs`] would gather,
    /// supplied offline. Flags still come from argv, so one payload can be
    /// replayed against several caller states.
    fn stdin_evidence(&self, raw: &str) -> std::result::Result<Evidence, String> {
        let v: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("stdin is not valid JSON: {e}"))?;
        let str_at = |k: &str| -> String {
            v.get(k)
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let head_parents: Vec<String> = v
            .get("head_parents")
            .and_then(|p| p.as_array())
            .ok_or("stdin payload has no head_parents array")?
            .iter()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        // Tri-state on purpose: absent/null is "could not determine", which
        // classify() refuses rather than assuming either way.
        let second_parent_in_base = v.get("second_parent_in_base").and_then(|b| b.as_bool());
        let precondition_sha = if self.precondition_sha.is_empty() {
            str_at("precondition_sha")
        } else {
            self.precondition_sha.clone()
        };
        Ok(Evidence {
            response: str_at("response"),
            mismatch_confirmed: self.mismatch_confirmed
                || v.get("mismatch_confirmed")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
            self_synced: self.self_synced
                || v.get("self_synced")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
            retry_used: self.retry_used
                || v.get("retry_used")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
            precondition_sha,
            current_head_sha: str_at("current_head_sha"),
            head_parents,
            second_parent_in_base,
        })
    }
}
