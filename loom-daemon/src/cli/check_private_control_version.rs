//! `loom-daemon check-private-control-version` (Issue #8858): fail when the
//! private-control `POLICY` changes without a strictly greater
//! `CONTROL_VERSION` relative to the merge base. Logic and tests live in
//! [`loom_daemon::private_control_gate`].
//!
//! Exit codes: `0` = ok, `1` = violation or unreadable/ambiguous declaration.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::private_control_gate::{self, Verdict};

#[derive(clap::Args)]
pub(crate) struct CheckPrivateControlVersionArgs {
    /// Base ref (the PR's target); the comparison uses its merge base with `--head`.
    #[arg(long, value_name = "REF")]
    base: String,
    /// Head ref (the PR tip).
    #[arg(long, value_name = "REF", default_value = "HEAD")]
    head: String,
    /// Repository root. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    repo_root: Option<PathBuf>,
    /// Repo-relative file declaring `CONTROL_VERSION` and `POLICY`.
    #[arg(long, value_name = "PATH", default_value = private_control_gate::DEFAULT_SOURCE_PATH)]
    source_path: String,
}

impl CheckPrivateControlVersionArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        match private_control_gate::check(&root, &self.base, &self.head, &self.source_path) {
            Ok(Verdict::Ok(m) | Verdict::OkBumped(m)) => {
                println!("check-private-control-version: OK - {m}");
                Ok(())
            }
            Ok(Verdict::Violation(m)) => {
                eprintln!("check-private-control-version: FAIL - {m}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("check-private-control-version: ERROR - {e:#}");
                std::process::exit(1);
            }
        }
    }
}
