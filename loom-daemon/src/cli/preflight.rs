//! `loom-daemon preflight` — the Builder's in-session pre-PR gate
//! (Issue #10476). Logic lives in [`loom_daemon::preflight`].
//!
//! Exit codes: `0` pass / gate not configured; `1` failed, attempts remain
//! (output tail printed — fix and re-run); `4` attempts exhausted
//! (`preflight_unresolved`, claim released, open NO PR); `5` timed out
//! (#10860: not a failure, claim kept); `7` (`--check`) no passing receipt
//! for the current `HEAD`.

use std::path::PathBuf;

use loom_daemon::preflight::{self, Verdict};

#[derive(clap::Args)]
pub(crate) struct PreflightArgs {
    /// Issue number (used to release the claim on terminal failure).
    #[arg(long, value_name = "N")]
    issue: Option<u64>,

    /// Worktree to gate. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    worktree: Option<PathBuf>,

    /// Do not run the gate; exit 7 unless the current HEAD already passed.
    /// Used by `create-pr.sh` to refuse an un-gated PR.
    #[arg(long)]
    check: bool,
}

impl PreflightArgs {
    pub(crate) fn run(self) -> ! {
        let wt = self
            .worktree
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        if self.check {
            if preflight::check(&wt) {
                std::process::exit(0);
            }
            eprintln!(
                "preflight: HEAD has no passing pre-PR gate receipt — run `loom-daemon preflight --issue N` and fix failures first"
            );
            std::process::exit(preflight::EXIT_NOT_PASSED);
        }
        let verdict = preflight::run(&wt);
        match &verdict {
            Verdict::Disabled => eprintln!("preflight: no enabled buildGate — skipped"),
            Verdict::Pass => eprintln!("preflight: PASS"),
            Verdict::Failed { attempt, max, tail } => {
                println!("preflight: FAILED (attempt {attempt}/{max}) — fix the cause, commit, re-run.\n{tail}");
            }
            Verdict::Unresolved {
                attempts,
                max,
                tail,
            } => {
                println!(
                    "preflight: reason=preflight_unresolved after {attempts}/{max} attempts — open NO PR.\n{tail}"
                );
            }
            Verdict::TimedOut {
                attempt, max, tail, ..
            } if attempt >= max => {
                println!(
                    "preflight: reason=preflight_timeout after {attempt}/{max} timeouts — open NO PR (claim kept).\n{tail}"
                );
            }
            Verdict::TimedOut {
                attempt, max, tail, ..
            } => {
                println!(
                    "preflight: TIMED OUT (timeout {attempt}/{max}) — not a check failure; claim kept. Re-run when the host is less loaded.\n{tail}"
                );
            }
        }
        if let Some(n) = self.issue {
            // Fail closed on a terminal check failure: hand the issue back so
            // the pipeline can retry — through the protected, worktree-scoped
            // release path, and say so loudly if the forge call fails. A
            // timeout keeps the claim (#10860).
            let settled = match u32::try_from(n) {
                Ok(n) => preflight::settle_claim(&wt, n, &verdict),
                Err(e) => verdict.releases_claim().then(|| Err(e.into())),
            };
            match settled {
                None => {}
                Some(Ok(())) => eprintln!("preflight: claim on #{n} released"),
                Some(Err(e)) => eprintln!(
                    "preflight: FAILED to release claim on #{n}: {e:#} — release it manually (`loom-recover-orphans --recover`)"
                ),
            }
        }
        std::process::exit(verdict.exit_code());
    }
}
