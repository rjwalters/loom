//! Account session command surface, including opt-in private workspaces.
use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

/// Sub-actions for `loom-daemon accounts session` (issue #6925).
#[derive(Subcommand)]
pub(crate) enum SessionAction {
    /// Launch (or reuse, if already running; resume, if stopped-but-present)
    /// the account's session container, then adopt its profile under the
    /// ownership rule (a session-managed profile refuses further
    /// host-direct `CODEX_HOME` use — see `accounts reauth`/`status`).
    Start {
        /// The account's short profile name, or its registered email
        /// (issue #7389 -- see `accounts add --email`).
        #[arg(value_name = "NAME")]
        name: String,
        /// Override the session image (default:
        /// `ghcr.io/rjwalters/loom-worker-session:latest`).
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
        /// Directory to bind-mount read-write at the identical absolute
        /// host path (`docker/worker/MOUNT-CONTRACT.md` §1) — normally the
        /// parent directory holding every checkout the container will
        /// serve, since dispatch execs with `--workdir` set to the repo.
        /// A parent is narrowed to the repositories registered in
        /// `~/.loom/workspaces.json` under it, each mounted on its own
        /// (issue #9979: Codex runs with its sandbox off in the container).
        /// Defaults to the `--workspace` this `loom-daemon` invocation
        /// itself resolved (issue #7389). Deliberately NOT named
        /// `--workspace`: `accounts --workspace` is a global `String`
        /// argument, and a nested arg under the same id with a different
        /// type makes clap panic at access time (issue #8517).
        #[arg(long = "mount-workspace", value_name = "PATH")]
        mount_workspace: Option<PathBuf>,
        /// Use an account-owned Docker volume containing a real independent clone.
        #[arg(long, conflicts_with = "mount_workspace")]
        private_clone: Option<String>,
        #[arg(long, default_value = "main", requires = "private_clone")]
        base: String,
        #[arg(long)]
        json: bool,
    },
    /// Run one supervised job with an exclusive account lease (private mode).
    Job(crate::tokens_pool::private_workspace::JobArgs),
    /// Tear down the container cleanly. Refuses an in-flight `docker exec`
    /// unless `--force` (the #5119 restart-safety contract: never a raw
    /// SIGKILL of active work).
    Stop {
        #[arg(value_name = "NAME")]
        name: String,
        /// Stop even if an in-flight `docker exec` is detected.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Report running/stopped and basic health (container id, uptime, mount
    /// paths).
    Status {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Attach to the container's tmux server for interactive `codex login` /
    /// inspection. Operator-only — never the dispatch path (headless
    /// dispatch is a plain `docker exec`, added by a later Phase 2 issue).
    Attach {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// "Start-if-absent, run Codex, attach" composite (issue #7389) —
    /// what the operator-facing `codex-agent <account>` shim execs into.
    /// Starts the session if not already running, launches `codex` in a
    /// tmux window cwd'd to the mounted workspace, and attaches. Re-running
    /// `shell` re-attaches to the same window rather than stacking a
    /// second Codex process.
    Shell {
        /// The account's short profile name, or its registered email.
        #[arg(value_name = "NAME")]
        name: String,
        /// Directory to bind-mount, same default and same naming rationale
        /// as `session start --mount-workspace` (issue #8517).
        #[arg(long = "mount-workspace", value_name = "PATH")]
        mount_workspace: Option<PathBuf>,
        /// Extra arguments passed to `codex` inside the tmux window, after
        /// a literal `--` (default when omitted: `--yolo`, the operator's
        /// own bare-metal invocation).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List, publish or discard the unpublished branches of the account's
    /// private clones — the cure a preparation refusal names (issue #14030).
    /// Runs under the same exclusive account lease as a job, so it cannot
    /// race a running step. Without `--publish`/`--discard` only the
    /// listing is printed.
    Recover {
        /// The account's short profile name, or its registered email.
        #[arg(value_name = "NAME")]
        name: String,
        /// Push the unpublished branches to origin (fast-forward only).
        #[arg(long, conflicts_with = "discard")]
        publish: bool,
        /// Delete the named branches with their unpublished commits;
        /// requires `--branch` or `--issue`.
        #[arg(long, conflicts_with = "publish")]
        discard: bool,
        /// Scope the action to one issue's own branch (`feature/issue-N`).
        #[arg(long)]
        issue: Option<u64>,
        /// Scope the action to a named branch (repeatable).
        #[arg(long, value_name = "BRANCH")]
        branch: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Move an additional private-clone registration to the repository's
    /// canonical URL after a forge transfer, keeping its clone, branches and
    /// local changes (issue #14357). Runs under the exclusive idle account
    /// lease; refuses (changing nothing) a busy or held account, a wrong
    /// repository, and two populated clones. Safe to repeat.
    Rebind {
        /// The account's short profile name, or its registered email.
        #[arg(value_name = "NAME")]
        name: String,
        /// The registered (pre-transfer) repository URL.
        #[arg(long, value_name = "URL")]
        from: String,
        /// The canonical repository URL after the transfer.
        #[arg(long, value_name = "URL")]
        to: String,
        /// Verify everything and report; change nothing.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn handle_session_command(
    action: SessionAction,
    workspace: std::path::PathBuf,
) -> Result<()> {
    use crate::tokens_pool::session_lifecycle::{
        ProcessContainerRunner, SessionLifecycle, SessionStatus,
    };

    /// The other roots of the one hold root set the reconcile pass also uses
    /// (the registered workspaces plus the daemon's recorded fallback root,
    /// #10661): an operator start lifts an operator hold in each one's
    /// profile for the account, and status reads them (issue #10453).
    fn peer_roots(workspace: &std::path::Path) -> Vec<PathBuf> {
        use crate::tokens_pool::session_hold_roots as roots;
        let registered = crate::workspace_registry::WorkspaceRegistry::load_default()
            .map(|registry| registry.roots())
            .unwrap_or_default();
        let fallback = roots::recorded_daemon_fallback_root();
        roots::cli_peer_roots(workspace, &registered, fallback.as_deref())
    }

    /// The lifecycle for an operator command that may create a container
    /// (`start`, and `shell`'s start-if-absent), plus the image to pull.
    ///
    /// Reads the resolved `accounts.session` settings (issue #14019); invalid
    /// config refuses the command outright, since a seat created from the
    /// stock image because its override failed to parse would be a fail-open.
    /// Precedence: the operator's explicit `--image` beats the configured
    /// override, which beats the stock image; the mounts ride either way. Only
    /// a container created from the *configured* image gets the bundle
    /// verification (`with_session_config` compares it against the effective
    /// image).
    fn configured_lifecycle(
        workspace: PathBuf,
        image: Option<String>,
    ) -> Result<(SessionLifecycle<ProcessContainerRunner>, Option<String>)> {
        let peers = peer_roots(&workspace);
        let session_config =
            crate::tokens_pool::session_image::read_session_image_config(&workspace)?;
        let image = image.or_else(|| session_config.image.clone());
        let pull = image.clone();
        let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, image)
            .with_peer_roots(peers)
            .with_session_config(Some(session_config));
        Ok((lifecycle, pull))
    }

    /// Operator `start`/`shell` (#10661): Ctrl-C/SIGTERM is forwarded to the
    /// running `docker` child, and a missing image is pulled under its own
    /// budget before a first `docker run` (the reconciler does neither).
    fn prepare_operator_start(
        lifecycle: &SessionLifecycle<ProcessContainerRunner>,
        name: &str,
        image: Option<&str>,
    ) -> Result<()> {
        use crate::tokens_pool::{docker_cli, operator_interrupt, session_lifecycle};
        if let Err(error) = operator_interrupt::install() {
            eprintln!("note: Ctrl-C will not reach the `docker` command ({error:#})");
        }
        if lifecycle.status(name)?.container_id.is_none() {
            docker_cli::ensure_image_for_operator(
                image.unwrap_or(session_lifecycle::DEFAULT_SESSION_IMAGE),
            )?;
        }
        Ok(())
    }

    fn print_session_status(status: &SessionStatus, json: bool) -> Result<()> {
        // The reconciler's fail-closed removal record (#10364), read from the
        // profile so `SessionStatus` itself is unchanged.
        let removal = crate::tokens_pool::session_drift_removal::read(std::slice::from_ref(
            &status.codex_home,
        ));
        if json {
            let mut value = serde_json::to_value(status)?;
            value["drift_removal"] = serde_json::to_value(&removal)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        let removed = removal.as_ref().map(|r| {
            let denied: Vec<String> = r.denied.iter().map(|p| p.display().to_string()).collect();
            format!(
                ", removed (denied mount: {}) at unix_ms={} by the session reconciler; it stays \
                 down until the denial no longer applies or an operator `session start`",
                denied.join(", "),
                r.removed_at_unix_ms
            )
        });
        {
            println!(
                "{}: {} (container={}, id={}, image={}, started_at={}, codex_home={}, \
                 mount={}, session_managed={}, workspace={}, workspace_mode={})",
                status.name,
                if status.running {
                    "running"
                } else if status.restarting {
                    "restarting"
                } else if status.held {
                    "stopped, held (operator stop)"
                } else {
                    "stopped"
                },
                status.container_name,
                status.container_id.as_deref().unwrap_or("-"),
                status.image.as_deref().unwrap_or("-"),
                status.started_at.as_deref().unwrap_or("-"),
                status.codex_home.display(),
                status.mount_path,
                status.session_managed,
                status
                    .workspace
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |w| w.display().to_string()),
                status.workspace_mode,
            );
            if let Some(removed) = removed {
                println!("{}: {}", status.name, removed.trim_start_matches(", "));
            }
            if !status.extra_mounts.is_empty() {
                let mounts: Vec<String> = status
                    .extra_mounts
                    .iter()
                    .map(|mount| {
                        format!("{} -> {}:ro", mount.host.display(), mount.container.display())
                    })
                    .collect();
                println!("{}: extra read-only mounts: {}", status.name, mounts.join(", "));
            }
        }
        Ok(())
    }

    use crate::tokens_pool::private_workspace as private;
    fn print_private(status: &private::Status, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(status)?);
        } else {
            println!(
                "{}: {} (workspace_mode={}, private_root={}, volume={}, repository={}, additional_repositories={}, lease={})",
                status.config.account,
                if status.running { "running" } else { "stopped" },
                status.workspace_mode,
                status.private_root,
                status.config.volume,
                status.config.repository,
                status.config.repositories.len(),
                status
                    .lease
                    .as_ref()
                    .map_or("idle", |job| job.owner.as_str())
            );
            println!("{}: image={}", status.config.account, status.image.as_deref().unwrap_or("-"));
            if !status.extra_mounts.is_empty() {
                let mounts: Vec<String> = status
                    .extra_mounts
                    .iter()
                    .map(|mount| {
                        format!("{} -> {}:ro", mount.host.display(), mount.container.display())
                    })
                    .collect();
                println!(
                    "{}: extra read-only mounts: {}",
                    status.config.account,
                    mounts.join(", ")
                );
            }
        }
        Ok(())
    }
    fn print_recover(report: &private::RecoverReport, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(report)?);
            return Ok(());
        }
        for clone in &report.clones {
            if !clone.cloned {
                println!(
                    "{}: {} is registered but not cloned yet; nothing to recover",
                    report.account, clone.repository
                );
                continue;
            }
            for branch in &clone.published {
                println!("{}: published {}", report.account, branch);
            }
            for branch in &clone.discarded {
                println!("{}: discarded {}", report.account, branch);
            }
            for name in &clone.ignored {
                println!(
                    "{}: no unpublished commits on {} in this clone; it was not touched",
                    report.account, name
                );
            }
            for branch in &clone.branches {
                println!(
                    "{}: still unpublished: {} ({} ahead)",
                    report.account, branch.branch, branch.ahead
                );
            }
            for head in &clone.detached {
                println!(
                    "{}: stranded on a detached HEAD: {} at {} ({} unpushed); \
                     name it with a branch (git switch -c) — recover cannot clear it",
                    report.account, head.path, head.revision, head.ahead
                );
            }
            if clone.published.is_empty()
                && clone.discarded.is_empty()
                && clone.branches.is_empty()
                && clone.detached.is_empty()
            {
                println!("{}: {} has no unpublished work", report.account, clone.repository);
            }
        }
        Ok(())
    }
    match action {
        SessionAction::Start {
            name,
            image,
            mount_workspace: workspace_arg,
            private_clone,
            base,
            json,
        } => {
            if let Some(repository) = private_clone {
                return print_private(
                    &private::start(&workspace, &name, &repository, &base, image.as_deref())?,
                    json,
                );
            }
            if private::configured(&workspace, &name)? {
                anyhow::bail!("account uses private-clone mode; repeat start --private-clone URL --base BRANCH; host-mount reuse is refused");
            }
            let (lifecycle, pull) = configured_lifecycle(workspace, image)?;
            prepare_operator_start(&lifecycle, &name, pull.as_deref())?;
            print_session_status(
                &lifecycle.start_with_workspace(&name, workspace_arg.as_deref())?,
                json,
            )
        }
        SessionAction::Job(args) => {
            let code = private::run_job(&workspace, args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        SessionAction::Recover {
            name,
            publish,
            discard,
            issue,
            branch,
            json,
        } => {
            if !private::configured(&workspace, &name)? {
                anyhow::bail!("account does not use a private workspace; nothing to recover");
            }
            let action = if publish {
                private::RecoverAction::Publish
            } else if discard {
                private::RecoverAction::Discard
            } else {
                private::RecoverAction::List
            };
            print_recover(
                &private::session_recover(&workspace, &name, action, issue, &branch)?,
                json,
            )
        }
        SessionAction::Rebind {
            name,
            from,
            to,
            dry_run,
            json,
        } => {
            if !private::configured(&workspace, &name)? {
                anyhow::bail!("account does not use a private workspace; nothing to rebind");
            }
            let report = private::rebind(&workspace, &name, &from, &to, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}: {:?}: {} -> {} (clone {}, registration {})",
                    report.account,
                    report.status,
                    report.from,
                    report.to,
                    report.clone_path,
                    if report.registration_changed {
                        "updated"
                    } else {
                        "unchanged"
                    },
                );
            }
            if report.status == private::RebindStatus::BothPopulated {
                anyhow::bail!(
                    "both {} and the old location hold a clone; nothing was changed. Publish or \
                     discard one with `accounts session recover {name}` (or attach and move the \
                     work by hand), then rerun",
                    report.clone_path
                );
            }
            Ok(())
        }
        SessionAction::Stop { name, force, json } => {
            if private::configured(&workspace, &name)? {
                return print_private(&private::stop(&workspace, &name, force)?, json);
            }
            let peers = peer_roots(&workspace);
            let lock = crate::tokens_pool::session_dispatch_lock::for_operator_stop;
            let workspace_for_lock = workspace.clone();
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None)
                .with_peer_roots(peers);
            // A dispatch that is only starting is invisible to `docker top`;
            // it holds this lock (#10364). Kept until the stop is done.
            let _no_dispatch = if force {
                None
            } else {
                // The lock is keyed by the resolved account's container. A
                // reference that does not resolve here is left to `stop`
                // itself to report; a failed `docker inspect` must not abort
                // the stop, so the container name is resolved from the
                // accounts registry, not from `status` (no docker call).
                let resolved = crate::tokens_pool::account_registry::account_inventory(
                    &workspace_for_lock,
                    crate::tokens_pool::account_registry::AccountProvider::Codex,
                )
                .ok()
                .and_then(|inventory| {
                    inventory.into_iter().find(|a| {
                        crate::tokens_pool::account_registry::account_matches_reference(a, &name)
                    })
                })
                .map_or_else(|| name.clone(), |account| account.id.name);
                let container = crate::tokens_pool::session_lifecycle::container_name(&resolved);
                Some(lock(&container)?)
            };
            print_session_status(&lifecycle.stop(&name, force)?, json)
        }
        SessionAction::Status { name, json } => {
            if private::configured(&workspace, &name)? {
                return print_private(&private::status(&workspace, &name)?, json);
            }
            let peers = peer_roots(&workspace);
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None)
                .with_peer_roots(peers);
            print_session_status(&lifecycle.status(&name)?, json)
        }
        SessionAction::Attach { name } => {
            if private::configured(&workspace, &name)? {
                anyhow::bail!("private sessions do not permit unleased tmux attach; use session job --kind interactive --owner NAME -- COMMAND (TTY input is unsupported)");
            }
            let peers = peer_roots(&workspace);
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None)
                .with_peer_roots(peers);
            let code = lifecycle.attach(&name)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        SessionAction::Shell {
            name,
            mount_workspace: workspace_arg,
            args,
        } => {
            if private::configured(&workspace, &name)? {
                anyhow::bail!("private sessions do not permit unleased shell; use session job --kind interactive --owner NAME -- COMMAND (TTY input is unsupported)");
            }
            // `shell` is start-if-absent, so it creates a container exactly
            // as `start` does and must carry the same configured image and
            // extra mounts (issue #14019).
            //
            // No unit test covers this arm (issue #14421): it builds a
            // `ProcessContainerRunner` and shells out to `docker`, so it is
            // not reachable without a real daemon. The guard is this call
            // going through `configured_lifecycle`: do not replace it with
            // `SessionLifecycle::new(.., None)`, which silently creates a
            // stock-image container with no extra mounts. The reconciler's
            // recreate paths (which carry the same preference) are covered
            // in `session_reconcile_tests.rs` / `session_reconcile_drift_tests.rs`.
            let (lifecycle, pull) = configured_lifecycle(workspace, None)?;
            prepare_operator_start(&lifecycle, &name, pull.as_deref())?;
            let code = lifecycle.shell(&name, workspace_arg.as_deref(), &args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}
