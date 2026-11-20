//! `loom-daemon merge-pr release-hold` (#13517): the eligibility decision for
//! `merge-pr.sh <N> --release-hold`.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | releasable | `LOOM-HOLD-RELEASE-OK`, then the audit-comment body | 0 |
//! | refused | `Merge blocked: --release-hold refused ...` | 1 |
//! | could not read inputs | — | 2 |
//!
//! Fail-closed like its siblings: the caller proceeds only on exit 0 AND the
//! sentinel. Trusted-author comment bodies arrive on stdin; labels as an arg.

use anyhow::Result;
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct ReleaseHoldArgs {
    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,
    /// The current head SHA.
    #[arg(long, value_name = "SHA")]
    head_sha: String,
    /// The authenticated forge login releasing the hold.
    #[arg(long, value_name = "LOGIN")]
    operator: String,
    /// Labels, newline-separated.
    #[arg(long, value_name = "LABELS")]
    labels: String,
}

impl ReleaseHoldArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut comments = String::new();
        if std::io::stdin().read_to_string(&mut comments).is_err() {
            eprintln!("merge-pr release-hold: could not read trusted comments from stdin");
            std::process::exit(2);
        }
        use crate::merge_pr::hold_release::{evaluate, Decision, OK};
        match evaluate(&self.pr, &self.labels, &comments, &self.head_sha, &self.operator) {
            Decision::Release(body) => {
                println!("{OK}\n{body}");
                std::process::exit(0)
            }
            Decision::Refuse(msg) => {
                println!("{msg}");
                std::process::exit(1)
            }
        }
    }
}
