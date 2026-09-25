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

    fn print_session_status(status: &SessionStatus, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(status)?);
        } else {
            println!(
                "{}: {} (container={}, id={}, image={}, started_at={}, codex_home={}, \
                 mount={}, session_managed={}, workspace={}, workspace_mode={})",
                status.name,
                if status.running { "running" } else { "stopped" },
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
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, image);
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
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            print_session_status(&lifecycle.stop(&name, force)?, json)
        }
        SessionAction::Status { name, json } => {
            if private::configured(&workspace, &name)? {
                return print_private(&private::status(&workspace, &name)?, json);
            }
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            print_session_status(&lifecycle.status(&name)?, json)
        }
        SessionAction::Attach { name } => {
            if private::configured(&workspace, &name)? {
                anyhow::bail!("private sessions do not permit unleased tmux attach; use session job --kind interactive --owner NAME -- COMMAND (TTY input is unsupported)");
            }
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
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
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            let code = lifecycle.shell(&name, workspace_arg.as_deref(), &args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}
