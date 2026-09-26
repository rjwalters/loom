//! Non-blocking startup reconciliation passes (Issue #7974).
//!
//! # Why
//!
//! `run_daemon` (`daemon_service.rs`) used to run the synchronous startup
//! claim-reconciliation pass (`claim_reconciliation::run_reconciliation_pass`,
//! `is_startup = true`, Issue #6615) and the co-located stranded-quarantine
//! reconciliation pass (`quarantine_reconciliation`, Issue #4110) as plain
//! blocking calls, *before* the IPC socket was bound, the pidfile written, or
//! the heartbeat task started. Both passes fan out a `gh` child per managed
//! workspace (`claim_reconciliation::forge::reconcile_workspace` /
//! `quarantine_reconciliation::forge::reconcile_workspace`), so their
//! duration scales with workspace count and forge latency — and under GitHub
//! rate limiting it can grow past the watchdog's startup grace
//! (`daemon_install_state::DEFAULT_STARTUP_GRACE_SECS`), making a perfectly
//! healthy (if slow) startup look wedged and get probed as unhealthy.
//!
//! [`spawn_startup_passes`] moves both passes onto a blocking thread
//! (`tokio::task::spawn_blocking`) so `run_daemon` can bind the socket, claim
//! the pidfile, and start the heartbeat almost immediately, while still
//! giving a caller with an ordering dependency — the work finder — a cheap
//! way to await the pass's *actual* completion before admitting its first
//! sweep. That preserves the reconcile-before-dispatch invariant (Issue
//! #6615): a stale `loom:building` claim's reclaim back to `loom:issue` must
//! land before the work finder can see and dispatch it, or a claim
//! legitimately orphaned by a crash between the label flip and the journal
//! write could otherwise race a fresh dispatch of the very issue it names.
//!
//! # Not gated on this
//!
//! The periodic reconciliation task
//! (`claim_reconciliation::spawn_periodic_reconciliation_task`) is a
//! completely separate concern (`is_startup = false`) and is unaffected —
//! it is spawned by `run_daemon` immediately after calling
//! [`spawn_startup_passes`], exactly as before.

use std::path::{Path, PathBuf};

use tokio::sync::watch;

/// Test-only startup delay (milliseconds), injected immediately before the
/// reconciliation passes run, so an integration test can observe the daemon
/// answering IPC calls — and its pidfile/heartbeat already live — while a
/// *slow* startup pass is still in flight, without needing a real
/// multi-workspace `gh` fan-out to actually run long (#7974). Never read
/// outside [`spawn_startup_passes`]; a production host has no reason to set
/// it.
pub const TEST_STARTUP_DELAY_MS_ENV: &str = "LOOM_TEST_STARTUP_RECONCILE_DELAY_MS";

/// Spawn the startup claim-reconciliation pass (`is_startup = true`) and the
/// stranded-quarantine reconciliation pass on a blocking thread, returning a
/// [`watch::Receiver<bool>`] that flips to `true` exactly once, when both
/// have finished.
///
/// The returned receiver's initial value is `false`; [`watch::Receiver::wait_for`]
/// resolves immediately once the value has flipped, so a caller with no
/// ordering dependency on the passes can simply drop the receiver, and a
/// caller that awaits it before its own first tick (the work finder) never
/// waits longer than the pass itself actually takes.
pub fn spawn_startup_passes(fallback_root: PathBuf) -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        let quarantine_root = fallback_root.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(raw) = std::env::var(TEST_STARTUP_DELAY_MS_ENV) {
                if let Ok(ms) = raw.parse::<u64>() {
                    log::info!(
                        "daemon_startup_reconciliation: test delay active — sleeping {ms}ms \
                         before the startup passes run ({TEST_STARTUP_DELAY_MS_ENV}, #7974)"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            crate::claim_reconciliation::run_reconciliation_pass(&fallback_root, true);
            run_startup_quarantine_pass(&quarantine_root);
            run_startup_profile_provisioning_pass(&quarantine_root);
        })
        .await;
        // A `send` error just means every receiver — including the one this
        // function returned — was already dropped, i.e. no caller ever cared
        // about the completion signal. Nothing to log or recover from.
        let _ = tx.send(true);
    });
    rx
}

/// The stranded-quarantine half of the startup pass (Issue #4110): the
/// insta-crash quarantine (#3939) is memory-only, so a restart drops the
/// in-memory pause while the `loom:blocked` label it applied survives on the
/// forge — with nothing left to release it, the issue is permanently
/// invisible to the work finder. Scans every registered workspace's open
/// `loom:blocked` issues and releases the ones carrying a daemon-authored
/// quarantine comment back to `loom:issue`; a human's manual `loom:blocked`
/// (no such comment) is never touched. Byte-for-byte the pre-#7974 logic,
/// just relocated off `daemon_service.rs`'s startup-blocking path.
fn run_startup_quarantine_pass(fallback_root: &Path) {
    if !crate::quarantine_reconciliation::reconciliation_enabled() {
        log::info!(
            "quarantine_reconciliation: startup pass disabled ({}=0)",
            crate::quarantine_reconciliation::RECONCILE_ENABLED_ENV
        );
        return;
    }
    let workspace_registry =
        crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let gh_bin = std::path::PathBuf::from("gh");
    let mut total_checked = 0usize;
    let mut total_released = 0usize;
    for root in &roots {
        let (checked, released) =
            crate::quarantine_reconciliation::forge::reconcile_workspace(&gh_bin, root);
        total_checked += checked;
        total_released += released;
    }
    if total_released > 0 {
        log::info!(
            "quarantine_reconciliation: startup pass checked {total_checked} loom:blocked \
             issue(s) across {} workspace(s), released {total_released} stranded \
             quarantine(s) (#4110)",
            roots.len()
        );
    } else {
        log::debug!(
            "quarantine_reconciliation: startup pass checked {total_checked} loom:blocked \
             issue(s) across {} workspace(s), nothing to release",
            roots.len()
        );
    }
}

/// Pooled-profile provisioning (Issue #8672): re-populate every registered
/// account's profile from the operator's default one, so a profile that was
/// created before this shipped — or that has drifted since the operator
/// changed their own `config.toml`, `AGENTS.md`, or prompt set — is brought
/// back up to date without anyone having to remember to run a command.
///
/// Safe to run unconditionally on every start:
///
/// - It is a **no-op when there is nothing to do** — the ledger makes a
///   second pass over an unchanged pair write nothing at all.
/// - It **never overwrites an operator's own edit** inside a pooled profile,
///   and never reads or writes a credential.
/// - It **skips session-managed profiles**, whose container owns the
///   directory (#6925).
/// - A host with no default profile (no `~/.codex`) exits immediately.
///
/// `LOOM_PROFILE_PROVISION_ON_START=0` switches it off entirely.
fn run_startup_profile_provisioning_pass(fallback_root: &Path) {
    use crate::tokens_pool::profile_provisioning as provisioning;

    if !provisioning::provision_on_start_enabled() {
        log::info!(
            "profile_provisioning: startup pass disabled ({}=0)",
            provisioning::PROVISION_ON_START_ENV
        );
        return;
    }
    for rules in crate::tokens_pool::profile_sharing::PROVIDER_RULES {
        let Some(source) = provisioning::default_profile_home(rules) else {
            continue;
        };
        if !source.is_dir() {
            log::debug!(
                "profile_provisioning: no default {} profile at {} — nothing to provision from",
                rules.provider,
                source.display()
            );
            continue;
        }
        let options = provisioning::ProvisionOptions {
            source,
            // The hook bridge bakes a project root into the managed entry, so
            // the daemon's own sweep workspace is the right one to name here.
            workspace: fallback_root.to_path_buf(),
            dry_run: false,
            skip_hook_bridge: false,
        };
        let mut changed = 0usize;
        let mut failed = 0usize;
        let results = provisioning::provision_all(rules, fallback_root, &options);
        let total = results.len();
        for (account, result) in results {
            match result {
                Ok(report) if report.changed => changed += 1,
                Ok(_) => {}
                Err(error) => {
                    failed += 1;
                    log::warn!(
                        "profile_provisioning: {}/{account} could not be provisioned: {error:#}",
                        rules.provider
                    );
                }
            }
        }
        if changed > 0 || failed > 0 {
            log::info!(
                "profile_provisioning: startup pass checked {total} {} profile(s), provisioned \
                 {changed}, failed {failed} (#8672)",
                rules.provider
            );
        } else {
            log::debug!(
                "profile_provisioning: startup pass checked {total} {} profile(s), all up to date",
                rules.provider
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Issue #8672 AC 2, daemon half: the startup pass provisions **every**
    /// pooled profile, not just ones created after this shipped — and is a
    /// no-op on the next start.
    #[test]
    #[serial]
    fn the_startup_pass_provisions_every_pooled_profile() {
        use crate::tokens_pool::profile_provisioning::PROVISION_ON_START_ENV;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let profiles = tmp.path().join("codex-profiles");
        let default_home = tmp.path().join("operator").join(".codex");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(default_home.join("prompts")).unwrap();
        std::fs::write(default_home.join("AGENTS.md"), "# shared\n").unwrap();
        std::fs::write(default_home.join("auth.json"), "OPERATOR-CREDENTIAL").unwrap();
        // Two profiles that predate provisioning entirely: credential only.
        for name in ["agent-1", "agent-2"] {
            let profile = profiles.join(name);
            std::fs::create_dir_all(&profile).unwrap();
            std::fs::write(profile.join("auth.json"), format!("CRED-{name}")).unwrap();
        }

        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", &profiles);
        std::env::set_var("LOOM_CODEX_DEFAULT_HOME", &default_home);
        std::env::set_var("LOOM_CODEX_HOOKS_SCRIPT", "");
        std::env::remove_var(PROVISION_ON_START_ENV);

        run_startup_profile_provisioning_pass(&workspace);

        for name in ["agent-1", "agent-2"] {
            let profile = profiles.join(name);
            assert_eq!(
                std::fs::read_to_string(profile.join("AGENTS.md")).unwrap(),
                "# shared\n",
                "{name} was not provisioned"
            );
            assert!(std::fs::symlink_metadata(profile.join("prompts"))
                .unwrap()
                .file_type()
                .is_symlink());
            // The credential is exactly what it was.
            assert_eq!(
                std::fs::read_to_string(profile.join("auth.json")).unwrap(),
                format!("CRED-{name}")
            );
        }

        // Disabled: a run with the kill switch on must not resurrect a file
        // the operator deleted from a pooled profile.
        std::fs::remove_file(profiles.join("agent-1").join("AGENTS.md")).unwrap();
        std::env::set_var(PROVISION_ON_START_ENV, "0");
        run_startup_profile_provisioning_pass(&workspace);
        assert!(!profiles.join("agent-1").join("AGENTS.md").exists());

        std::env::remove_var(PROVISION_ON_START_ENV);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
        std::env::remove_var("LOOM_CODEX_DEFAULT_HOME");
        std::env::remove_var("LOOM_CODEX_HOOKS_SCRIPT");
    }

    /// The completion signal must not flip to `true` before the injected
    /// delay elapses, and must flip promptly once it does — the exact
    /// property the work finder's admission gate (`work_finder.rs`) relies
    /// on. Reconciliation itself is left disabled
    /// (`LOOM_STALE_CLAIM_RECONCILE=0` / `LOOM_QUARANTINE_RECONCILE=0`) so the
    /// pass never shells out to a real `gh`, keeping this test hermetic.
    /// `#[serial]` because the env vars it sets are process-global and shared
    /// with `claim_reconciliation`'s own `#[serial]`-guarded tests in this
    /// same test binary.
    #[tokio::test]
    #[serial]
    async fn completion_signal_waits_for_the_injected_delay() {
        let delay_ms: u64 = 200;
        std::env::set_var(TEST_STARTUP_DELAY_MS_ENV, delay_ms.to_string());
        std::env::set_var(crate::claim_reconciliation::RECONCILE_ENABLED_ENV, "0");
        std::env::set_var(crate::quarantine_reconciliation::RECONCILE_ENABLED_ENV, "0");

        let fallback_root = std::env::temp_dir();
        let started = Instant::now();
        let mut rx = spawn_startup_passes(fallback_root);

        // Immediately after spawning, the pass cannot possibly have finished.
        assert!(!*rx.borrow(), "completion signal must start false — the pass has not run yet");

        let observed_ready: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let observed_ready_task = observed_ready.clone();
        let waiter = tokio::spawn(async move {
            let _ = rx.wait_for(|ready| *ready).await;
            observed_ready_task.store(true, Ordering::SeqCst);
        });

        // While the delay is still in flight, the gate must not have opened.
        tokio::time::sleep(Duration::from_millis(delay_ms / 4)).await;
        assert!(
            !observed_ready.load(Ordering::SeqCst),
            "completion signal opened before the injected delay elapsed"
        );

        waiter.await.expect("waiter task panicked");
        assert!(
            started.elapsed() >= Duration::from_millis(delay_ms),
            "completion signal opened before the full injected delay elapsed"
        );
        assert!(observed_ready.load(Ordering::SeqCst));

        std::env::remove_var(TEST_STARTUP_DELAY_MS_ENV);
        std::env::remove_var(crate::claim_reconciliation::RECONCILE_ENABLED_ENV);
        std::env::remove_var(crate::quarantine_reconciliation::RECONCILE_ENABLED_ENV);
    }

    /// A receiver created via [`watch::channel`]-equivalent `wait_for` on an
    /// already-`true` value resolves instantly — the property
    /// `spawn_multi_work_finder_task` relies on for every tick *after* the
    /// first: once the pass has completed, the gate must never delay again.
    #[tokio::test]
    async fn wait_for_resolves_immediately_once_already_true() {
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).expect("at least one receiver is alive");
        let start = Instant::now();
        let _ = rx.wait_for(|ready| *ready).await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "an already-true gate must resolve near-instantly, not re-block"
        );
    }
}
