//! `loom-daemon merge-pr chain-lock` (#10167): the chain-head merge lock's
//! pre-merge guard. Logic and contract: [`loom_daemon::merge_pr::chain_lock`].
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | no live lock on another PR on this base | `LOOM-CHAIN-LOCK-CLEAR` | 0 |
//! | `LOOM_CHAIN_LOCK_OVERRIDE` set (no reads) | `LOOM-CHAIN-LOCK-OVERRIDDEN …` | 0 |
//! | state unreadable for a whole cap | `LOOM-CHAIN-LOCK-FAIL-OPEN …` | 0 |
//! | a live lock on another PR holds this merge | `LOOM-CHAIN-LOCK-HELD …` | 6 |
//! | state unreadable inside the cap | `LOOM-CHAIN-LOCK-UNREADABLE …` | 6 |
//!
//! It never writes to the forge. Exit 6 is `merge-pr.sh`'s own deferral code:
//! nothing merged, nothing failed, re-queue. Any other exit (an old binary
//! without this verb) is treated by `merge-pr.sh` as "the guard did not run"
//! and the merge proceeds: the lock orders merges, it never judges a tree.

use anyhow::Result;
use chrono::SecondsFormat;
use loom_daemon::merge_pr::chain_lock::{
    self, decide_unreadable, Guard, GuardInputs, Unreadable, CLEAR, DEFER_EXIT, FAIL_OPEN, HELD,
    OVERRIDDEN, OVERRIDE_ENV, UNREADABLE,
};

#[derive(clap::Args)]
pub(crate) struct ChainLockArgs {
    /// The PR about to be merged.
    #[arg(long, value_name = "N")]
    pr: u32,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: String,

    /// The branch the merge lands on.
    #[arg(long, value_name = "REF")]
    base_ref: String,

    /// Repo root for config, trust roster and the local unreadable record
    /// (default: the repo containing the working directory).
    #[arg(long, value_name = "PATH")]
    repo_root: Option<std::path::PathBuf>,
}

fn ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

impl ChainLockArgs {
    pub(crate) fn run(self) -> Result<()> {
        if chain_lock::override_set(std::env::var(OVERRIDE_ENV).ok().as_deref()) {
            println!("{OVERRIDDEN} pr={} {OVERRIDE_ENV} is set: the chain-head merge lock (#10167) was not consulted.", self.pr);
            return Ok(());
        }
        let root = self
            .repo_root
            .clone()
            .or_else(loom_daemon::repo_root::find_repo_root_from_cwd)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let cap_secs = chain_lock::cap_for_root(&root);
        let now = chrono::Utc::now();
        let policy = loom_daemon::comment_trust::TrustPolicy::for_root(&root);
        let gh = loom_daemon::gh_invocation::gh_bin();
        let state = chain_lock::unreadable_state_path(&root, &self.base_ref);
        let read = chain_lock::read_guard(&GuardInputs {
            gh: &gh,
            root: &root,
            nwo: &self.repo,
            pr: self.pr,
            base: &self.base_ref,
            cap_secs,
            now,
            policy: &policy,
            cache_dir: None,
        });
        match read {
            Ok(Guard::Clear) => {
                chain_lock::clear_unreadable(&state);
                println!("{CLEAR}");
                Ok(())
            }
            Ok(Guard::Held(l)) => {
                chain_lock::clear_unreadable(&state);
                println!(
                    "{HELD} pr={} holder=#{} head={} expires={}\nPR #{}'s merge onto '{}' is deferred: \
chain head #{} was re-dated at {} and holds the chain-head merge lock (#10167) until it lands or \
closes, its head moves, a required check fails, or {} (the cap), whichever is first. Nothing was \
written. Re-attempt on a later pass, or set {OVERRIDE_ENV}=1 to merge anyway.",
                    self.pr,
                    l.holder,
                    l.head,
                    ts(l.expires_at),
                    self.pr,
                    self.base_ref,
                    l.holder,
                    &l.head[..l.head.len().min(7)],
                    ts(l.expires_at)
                );
                std::process::exit(DEFER_EXIT);
            }
            Err(why) => {
                let Some(first) = chain_lock::note_unreadable(&state, now, cap_secs) else {
                    println!(
                        "{FAIL_OPEN} pr={} the chain-head merge lock state could not be read ({why}) \
and the first-failure record at {} could not be written, so the cap cannot be measured; proceeding \
rather than deferring without bound.",
                        self.pr,
                        state.display()
                    );
                    return Ok(());
                };
                match decide_unreadable(first, cap_secs, now) {
                    Unreadable::Defer { until } => {
                        println!(
                            "{UNREADABLE} pr={} until={}\nThe chain-head merge lock state for '{}' \
could not be read ({why}). Deferring rather than guessing 'no lock'; this fails open at {} if it \
stays unreadable. Nothing was written.",
                            self.pr,
                            ts(until),
                            self.base_ref,
                            ts(until)
                        );
                        std::process::exit(DEFER_EXIT);
                    }
                    Unreadable::FailOpen { since } => {
                        println!(
                            "{FAIL_OPEN} pr={} the chain-head merge lock state has been unreadable \
since {} ({why}); a whole cap has passed, so no lock from before then can still be live. Proceeding.",
                            self.pr,
                            ts(since)
                        );
                        Ok(())
                    }
                }
            }
        }
    }
}
