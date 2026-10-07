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
    Job(loom_daemon::tokens_pool::private_workspace::JobArgs),
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
}

pub(crate) fn handle_session_command(
    action: SessionAction,
    workspace: std::path::PathBuf,
) -> Result<()> {
    use loom_daemon::tokens_pool::session_lifecycle::{
        ProcessContainerRunner, SessionLifecycle, SessionStatus,
    };

    /// The other registered workspaces: an operator start lifts an operator
    /// hold in each one's profile for the account, and status reads them
    /// (issue #10453).
    fn peer_roots(workspace: &std::path::Path) -> Vec<PathBuf> {
        loom_daemon::workspace_registry::WorkspaceRegistry::load_default()
            .map(|registry| registry.roots())
            .unwrap_or_default()
            .into_iter()
            .filter(|root| root != workspace)
            .collect()
    }

    fn print_session_status(status: &SessionStatus, json: bool) -> Result<()> {
        // The reconciler's fail-closed removal record (#10364), read from the
        // profile so `SessionStatus` itself is unchanged.
        let removal = loom_daemon::tokens_pool::session_drift_removal::read(std::slice::from_ref(
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
        }
        Ok(())
    }

    use loom_daemon::tokens_pool::private_workspace as private;
    fn print_private(status: &private::Status, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(status)?);
        } else {
            println!(
                "{}: {} (workspace_mode={}, private_root={}, volume={}, repository={}, lease={})",
                status.config.account,
                if status.running { "running" } else { "stopped" },
                status.workspace_mode,
                status.private_root,
                status.config.volume,
                status.config.repository,
                status
                    .lease
                    .as_ref()
                    .map_or("idle", |job| job.owner.as_str())
            );
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
            let peers = peer_roots(&workspace);
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, image)
                .with_peer_roots(peers);
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
        SessionAction::Stop { name, force, json } => {
            if private::configured(&workspace, &name)? {
                return print_private(&private::stop(&workspace, &name, force)?, json);
            }
            let peers = peer_roots(&workspace);
            let lock = loom_daemon::tokens_pool::session_dispatch_lock::for_operator_stop;
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
                let resolved = loom_daemon::tokens_pool::account_registry::account_inventory(
                    &workspace_for_lock,
                    loom_daemon::tokens_pool::account_registry::AccountProvider::Codex,
                )
                .ok()
                .and_then(|inventory| {
                    inventory.into_iter().find(|a| {
                        loom_daemon::tokens_pool::account_registry::account_matches_reference(
                            a, &name,
                        )
                    })
                })
                .map_or_else(|| name.clone(), |account| account.id.name);
                let container =
                    loom_daemon::tokens_pool::session_lifecycle::container_name(&resolved);
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
            let peers = peer_roots(&workspace);
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None)
                .with_peer_roots(peers);
            let code = lifecycle.shell(&name, workspace_arg.as_deref(), &args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}
