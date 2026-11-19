//! GitHub App installation-token refresh ticks, extracted verbatim from
//! `run_daemon` (Issue #13641, slice 2 of the `run_daemon` decomposition; same
//! shape as `credential_startup` / `observer_tasks`).
//!
//! Pure move — statement order, comments and log strings are unchanged. The
//! caller keeps the `if let` guards (so "spawned whenever the App script is
//! configured" stays visible in `run_daemon`) and invokes the primary tick,
//! then the per-owner tick, after `fleet_sync::start` and before
//! `spawn_reader_refresh`. Both must be called from within the tokio runtime.

use std::path::PathBuf;

use loom_daemon::credential_preflight;

use super::gh_token_needs_update;

/// Primary (workspace-root owner) refresh tick.
pub(super) fn spawn_primary_refresh(
    script_path: PathBuf,
    owner_repo: String,
    sweep_workspace: PathBuf,
    config_dir: PathBuf,
    minted_token: Option<String>,
    github_app_gh_config_dir_active: bool,
    mut cross_owner_retry: credential_preflight::cross_owner::CrossOwnerRetry,
) {
    // #4430: ticks every `GITHUB_APP_REFRESH_INTERVAL` (~5min) to keep the
    // minted installation token fresh across its ~1h lifetime for a
    // long-running daemon. Ticks that don't rotate the token
    // never *write* anything: the `NotConfigured` arm is a true no-op, and a
    // cache-hit tick (the shell helper's own cache still has plenty of life —
    // ~10 of every ~11 ticks) compares against `current_token` (a plain local
    // captured by this task, never the process env — #4456's original
    // intent, now trivially true since there is no shared `GH_TOKEN` env var
    // left to read) and skips the write because the minted value is
    // unchanged. Only a genuine rotation (value differs) rewrites
    // `hosts.yml`; a mint failure is logged and the loop keeps whatever
    // credential `GH_CONFIG_DIR` already has on disk (fail-open, matching the
    // startup preflight's posture).
    let cwd = sweep_workspace;
    let mut current_token = minted_token;
    let mut config_dir_active = github_app_gh_config_dir_active;
    let (pass_workspace, pass_script) = (cwd.clone(), script_path.clone());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
            let script_path = script_path.clone();
            let cwd = cwd.clone();
            let repo = owner_repo.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let minter = credential_preflight::RealGithubAppMinter { script_path, cwd };
                credential_preflight::GithubAppMinter::mint(&minter, &repo)
            })
            .await;
            let primary_minted =
                matches!(outcome, Ok(credential_preflight::GithubAppOutcome::Minted { .. }));
            match outcome {
                Ok(
                    ref minted @ credential_preflight::GithubAppOutcome::Minted {
                        ref token,
                        ref installation_id,
                        ref app_id,
                        ..
                    },
                ) => {
                    // #5630: a successful mint (fresh or cache hit) ends this
                    // source's credential-failure streak, so the main-health
                    // gate goes back to trusting its forge answers on the very
                    // next tick. A publish failure below re-opens it — an
                    // unpublished token is just as un-refreshed on disk.
                    credential_preflight::record_forge_credential_success(
                        credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                    );
                    // #4456/#4458: only rewrite when the value actually
                    // rotated, and do it as a pure file write — no
                    // `std::env::set_var` anywhere in this arm once
                    // `config_dir_active` is already true (the common
                    // case: activation happened at startup above).
                    if gh_token_needs_update(current_token.as_deref(), token) {
                        // Rare edge case: the app becomes configured
                        // *after* startup (e.g. the shell helper's own
                        // config appears mid-run) without the daemon
                        // restarting, so activation didn't happen above.
                        // This is a second, still-bounded (at most once
                        // per process) `std::env::set_var` window — the
                        // recurring, steady-state race this issue targets
                        // is the one eliminated; this cold-start
                        // transition is unchanged in kind from the
                        // pre-#4458 code's own equivalent one-time
                        // `set_var` in this same arm.
                        if !config_dir_active {
                            std::env::remove_var("GH_TOKEN");
                            std::env::remove_var("GITHUB_TOKEN");
                            std::env::set_var("GH_CONFIG_DIR", &config_dir);
                            config_dir_active = true;
                        }
                        match credential_preflight::publish_outcome(
                            &config_dir,
                            minted,
                            &owner_repo,
                        ) {
                            Ok(()) => {
                                current_token = Some(token.clone());
                                log::debug!(
                                    "credential_preflight: github-app refresh tick rotated \
                                     GH_TOKEN (app {app_id} installation {installation_id}) \
                                     — #4430"
                                );
                            }
                            Err(e) => {
                                // #5630: the mint succeeded but the token never
                                // reached disk, so `GH_CONFIG_DIR` still holds the
                                // aging one — same staleness as a failed mint.
                                credential_preflight::record_forge_credential_failure(
                                    credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                                    &format!("could not publish the minted token: {e}"),
                                );
                                log::warn!(
                                    "credential_preflight: github-app refresh tick failed to \
                                     publish token to {} ({e}); GH_CONFIG_DIR left unchanged \
                                     (app {app_id} installation {installation_id}) — #4458",
                                    config_dir.display()
                                );
                            }
                        }
                    } else {
                        log::debug!(
                            "credential_preflight: github-app refresh tick cache hit, \
                             GH_TOKEN unchanged (app {app_id} installation {installation_id}) \
                             — #4456"
                        );
                    }
                }
                Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
                    // Cheap no-op tick -- nothing to refresh.
                }
                Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
                    // #5630: the credential on disk is now aging with no
                    // successful refresh behind it. Record the streak so the
                    // main-health gate holds its previous verdicts instead of
                    // reading the resulting forge failures as "every repo's
                    // main just went red".
                    credential_preflight::record_forge_credential_failure(
                        credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                        &reason,
                    );
                    log::warn!(
                        "credential_preflight: github-app refresh tick failed ({reason}); \
                         GH_TOKEN left unchanged — main-health gate verdicts are held for up \
                         to {}s while this persists — #4430/#5630",
                        credential_preflight::resolve_forge_credential_stale_grace().as_secs()
                    );
                }
                Err(e) => {
                    // A join error means the mint never completed either, so
                    // the credential is just as un-refreshed as an Error arm.
                    credential_preflight::record_forge_credential_failure(
                        credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                        &format!("github-app refresh task join error: {e}"),
                    );
                    log::warn!(
                        "credential_preflight: github-app refresh task join error: {e} — #4430"
                    );
                }
            }
            // #11957: run the per-owner pass startup skipped or could not
            // finish, once the App mints again. After the primary publish
            // above, so the cached refusals it expires are filed under
            // the process credential as it now stands.
            if primary_minted && cross_owner_retry.due() {
                let (ws, repo, script) =
                    (pass_workspace.clone(), owner_repo.clone(), pass_script.clone());
                let pass = credential_preflight::cross_owner::establish_for_workspace_off_thread(
                    ws, repo, script,
                )
                .await;
                cross_owner_retry.record(&pass);
            }
        }
    });
}

/// Per-owner credential refresh tick.
pub(super) fn spawn_owner_refresh(script_path: PathBuf, sweep_workspace: PathBuf) {
    // #5401: keep each per-owner credential fresh across its ~1h lifetime,
    // ticking every `GITHUB_APP_REFRESH_INTERVAL` (~5min) — the same cadence
    // as the workspace-root owner's tick above. Re-mint the
    // owner's representative repo and republish into its `GH_CONFIG_DIR` — a
    // pure file rewrite via #4458's delivery path; the root->dir registration
    // is path-stable and never needs re-registering. A fail-open loop
    // (matching the startup posture): a mint/publish miss logs and keeps
    // whatever token that owner's dir already holds.
    //
    // #6171: spawned whenever the App mechanism itself is configured
    // (`github_app_script.is_some()`), NOT only when a per-owner plan
    // happened to exist at startup — the source list
    // (`credential_preflight::owner_refresh_sources()`) is re-read from the
    // shared registry on every tick, so an owner discovered later via
    // `force_refresh_owner_credential`'s 404-recovery path (including a
    // fleet that started single-owner and only later grew a cross-owner
    // workspace) is picked up automatically, without a second task to spawn.
    // An empty registry makes a tick a cheap no-op loop, same cost as the
    // pre-#6171 early return.
    let cwd = sweep_workspace;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
            let owners = credential_preflight::owner_refresh_sources();
            for (owner_repo, config_dir) in &owners {
                let script_path = script_path.clone();
                let cwd = cwd.clone();
                let owner_repo_for_mint = owner_repo.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let minter = credential_preflight::RealGithubAppMinter { script_path, cwd };
                    credential_preflight::GithubAppMinter::mint(&minter, &owner_repo_for_mint)
                })
                .await;
                // #5630: each owner's credential is its own refresh
                // source. A saturated host times these out alongside the
                // primary tick, and their repos' forge calls go just as
                // wrong — so they feed the same staleness tracker, keyed
                // separately so a healthy owner's success cannot clear a
                // failing owner's streak.
                let source = credential_preflight::credential_source_for_owner(owner_repo);
                match outcome {
                    Ok(ref minted @ credential_preflight::GithubAppOutcome::Minted { .. }) => {
                        if let Err(e) =
                            credential_preflight::publish_outcome(config_dir, minted, owner_repo)
                        {
                            credential_preflight::record_forge_credential_failure(
                                &source,
                                &format!("could not publish the minted token: {e}"),
                            );
                            log::warn!(
                                "credential_preflight: per-owner github-app refresh could \
                                     not publish the token for {owner_repo} to {} ({e}); its \
                                     credential left unchanged — #5401",
                                config_dir.display()
                            );
                        } else {
                            credential_preflight::record_forge_credential_success(&source);
                        }
                    }
                    Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
                        // Cheap no-op — nothing to refresh for this owner.
                    }
                    Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
                        credential_preflight::record_forge_credential_failure(&source, &reason);
                        log::warn!(
                            "credential_preflight: per-owner github-app refresh for \
                                 {owner_repo} failed ({reason}); its credential left \
                                 unchanged — main-health gate verdicts are held for up to \
                                 {}s while this persists — #5401/#5630",
                            credential_preflight::resolve_forge_credential_stale_grace().as_secs()
                        );
                    }
                    Err(e) => {
                        credential_preflight::record_forge_credential_failure(
                            &source,
                            &format!("per-owner refresh task join error: {e}"),
                        );
                        log::warn!(
                            "credential_preflight: per-owner github-app refresh task join \
                                 error for {owner_repo}: {e} — #5401"
                        );
                    }
                }
            }
        }
    });
}
