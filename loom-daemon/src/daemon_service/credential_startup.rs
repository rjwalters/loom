//! Startup forge-credential preflight, extracted verbatim from `run_daemon`
//! (Issue #13017, slice 1 of the `run_daemon` decomposition; same shape as
//! `observer_tasks`).
//!
//! Pure move — statement order, comments and log strings are unchanged. The
//! caller invokes [`run_startup`] after `register_primary_workspace_root` /
//! `set_host_root` and before `fleet_sync::start`, and consumes the returned
//! values in the refresh-tick tasks and the IPC wiring.

use std::path::{Path, PathBuf};

use loom_daemon::credential_preflight;
use loom_daemon::types::CredentialPreflightReport;

/// Values later startup phases need from the credential preflight.
pub(super) struct CredentialStartup {
    pub github_app_script: Option<PathBuf>,
    pub github_app_owner_repo: Option<String>,
    pub github_app_config_dir: PathBuf,
    pub github_app_gh_config_dir_active: bool,
    /// The minted installation token, if any; seeds the primary refresh tick's
    /// `current_token`.
    pub minted_token: Option<String>,
    /// Passed on to the IPC/daemon wiring (#4005).
    pub credential_preflight: CredentialPreflightReport,
    pub cross_owner_retry: credential_preflight::cross_owner::CrossOwnerRetry,
}

pub(super) fn run_startup(sweep_workspace: &Path) -> CredentialStartup {
    // Startup forge-credential preflight (Issue #4005; GitHub App identity
    // mechanism added by #4430). Resolved once, here — immediately before the
    // claim-reconciliation pass below, the daemon's first `gh` consumer — so
    // a headless/SSH-only start with neither an exported GH_TOKEN/GITHUB_TOKEN
    // nor an unlockable GUI login keychain is diagnosed loudly at boot
    // instead of surfacing as silent per-tick 401s for the life of the
    // process. Non-fatal and bounded (same posture as the reconciliation pass
    // itself): a `gh` hiccup here never blocks startup.
    //
    // #4430: when a GitHub App id + private key are configured (env or
    // `forge.githubApp.*` config — see `defaults/scripts/lib/github-app-token.sh`),
    // this mints a short-lived installation token and exports it as this
    // process's own `GH_TOKEN`, so every `gh`/`git` child the daemon spawns
    // from this point on inherits it. Absent that config, `owner_repo`
    // resolves fine but the shell helper reports "not_configured" and
    // `run_with_github_app` falls through to the byte-identical pre-#4430
    // `run(...)` path below — no behavior change for any host that hasn't
    // opted in.
    let credential_preflight_probe = credential_preflight::RealGhAuthProbe {
        gh_bin: "gh".to_string(),
        cwd: sweep_workspace.to_path_buf(),
    };
    let github_app_script = credential_preflight::resolve_github_app_script(sweep_workspace);
    let github_app_owner_repo = credential_preflight::nwo_from_git_remote(sweep_workspace);
    let github_app_preflight = match &github_app_script {
        Some(script_path) => {
            let minter = credential_preflight::RealGithubAppMinter {
                script_path: script_path.clone(),
                cwd: sweep_workspace.to_path_buf(),
            };
            credential_preflight::run_with_github_app(
                &credential_preflight_probe,
                &minter,
                github_app_owner_repo.as_deref(),
            )
        }
        // No `github-app-token.sh` on disk at all (stale install predating
        // #4430, or a workspace root with no `.loom`/`defaults` tree) —
        // exactly the pre-#4430 path, with zero extra subprocess overhead.
        None => credential_preflight::GithubAppPreflight {
            report: credential_preflight::run(&credential_preflight_probe),
            minted: None,
        },
    };
    // #4458: deliver the minted installation token via a daemon-owned
    // `GH_CONFIG_DIR` (`credential_preflight::publish_minted`, #10571 sidecar)
    // instead of exporting it as this process's `GH_TOKEN`. Every `gh` child
    // this daemon spawns (`Command::new("gh")` without `env_clear`, ~76
    // call sites — see the issue body's census) re-reads
    // `$GH_CONFIG_DIR/hosts.yml` fresh on each invocation, so the refresh
    // tick below becomes a pure file rewrite with **zero** recurring
    // `std::env::set_var` calls — the UB race (concurrent `setenv` vs. the
    // `environ` reads inside every concurrently spawned `Command`) this issue
    // eliminates. `github_app_gh_config_dir_active` tracks whether the
    // one-time env activation below has happened yet, so it fires **at most
    // once** for the life of the process (see the tick closure further down
    // for the rare case where activation happens there instead of here).
    //
    // The one remaining `std::env::set_var` (`GH_CONFIG_DIR` itself, plus the
    // paired `remove_var` calls) is that single activation: it runs here,
    // before the refresh-tick task (or any other tokio task that spawns
    // `gh`/`git`) has been created, so there is no concurrent `Command::spawn`
    // in this process yet to race the `environ` write — the residual window
    // this issue's acceptance criteria allow a startup site to keep, given a
    // comment naming the race and the rationale for accepting it. Clearing
    // `GH_TOKEN`/`GITHUB_TOKEN` here (not just setting `GH_CONFIG_DIR`)
    // matters too: `gh` checks the env vars *before* `GH_CONFIG_DIR`'s
    // `hosts.yml`, so an operator-exported ambient token would otherwise
    // silently outrank the just-minted app token — the pre-#4458 code
    // achieved the same override by unconditionally overwriting `GH_TOKEN`.
    let github_app_config_dir = credential_preflight::github_app_gh_config_dir(sweep_workspace);
    let mut github_app_gh_config_dir_active = false;
    if let Some(minted) = &github_app_preflight.minted {
        match credential_preflight::publish_minted(&github_app_config_dir, minted) {
            Ok(()) => {
                std::env::remove_var("GH_TOKEN");
                std::env::remove_var("GITHUB_TOKEN");
                std::env::set_var("GH_CONFIG_DIR", &github_app_config_dir);
                github_app_gh_config_dir_active = true;
            }
            Err(e) => {
                // Fail open, matching the mint-failure posture elsewhere in
                // this preflight: never block startup, never leave the
                // daemon worse off than the pre-#4430 ambient path.
                log::warn!(
                    "credential_preflight: failed to publish github-app token to {} ({e}); \
                     falling back to ambient gh auth — #4458",
                    github_app_config_dir.display()
                );
            }
        }
    }
    // #9872: name the rate-limit pool the credential spends (WARN on a
    // personal one: shared with every PAT/OAuth/`gh` login of that user).
    let credential_preflight = credential_preflight::attach_pool(github_app_preflight.report);
    credential_preflight::loom_ui::startup(&credential_preflight); // #11295

    // Per-owner managed-repo credentials (#5401). The App preflight above
    // mints ONE installation token, for the workspace-root owner; every
    // managed repo owned by another account/org needs its own, published into
    // a per-owner `GH_CONFIG_DIR` and registered against its checkout roots
    // (`credential_preflight::cross_owner`). Only when the App is the active
    // credential this run; a single-owner fleet plans nothing.
    //
    // #11957: a startup mint that fails (a timeout on a saturated host) skips
    // this pass, and `cross_owner_retry` makes the primary refresh tick below
    // run it once its own mint succeeds, then with bounded backoff while any
    // owner still fails — instead of leaving every cross-owner root on the
    // root owner's credential, refused by the write-scope gate, until restart.
    let mut cross_owner_retry =
        credential_preflight::cross_owner::CrossOwnerRetry::after_startup(None);
    if let (Some(root_owner_repo), Some(script_path), true) = (
        &github_app_owner_repo,
        &github_app_script,
        github_app_preflight.minted.is_some(),
    ) {
        let pass = credential_preflight::cross_owner::establish_for_workspace(
            sweep_workspace,
            root_owner_repo,
            script_path,
        );
        cross_owner_retry =
            credential_preflight::cross_owner::CrossOwnerRetry::after_startup(Some(&pass));
    }

    CredentialStartup {
        github_app_script,
        github_app_owner_repo,
        github_app_config_dir,
        github_app_gh_config_dir_active,
        minted_token: github_app_preflight.minted.map(|m| m.token),
        credential_preflight,
        cross_owner_retry,
    }
}
