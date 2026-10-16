//! `loom-daemon fleet-dispatch` (#12449): clap wiring only. The server is the
//! lib crate's `fleet_dispatch` module.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};

use loom_daemon::fleet_dispatch::{
    self,
    identity::{TailscaleWhois, DEFAULT_TAILSCALE_SOCKET},
    roster::{FleetStoreRoster, RosterSource},
    server::{self, Options, Server, SystemClock},
    table::{Policy, DEFAULT_GRACE},
};

/// `loom-daemon fleet-dispatch` arguments.
#[derive(Args)]
pub(crate) struct FleetDispatchArgs {
    #[command(subcommand)]
    command: FleetDispatchCommand,
}

#[derive(Subcommand)]
enum FleetDispatchCommand {
    /// Run the captain's claim arbiter: workers in fleet mode ask it before
    /// their forge pre-claim. Binds a tailnet address; callers are identified
    /// by the tailnet daemon's `whois` and the fleet store's dispatcher roster.
    Serve {
        /// Address to bind: the captain's tailnet address (never a wildcard).
        #[arg(long, value_name = "IP")]
        bind: String,
        /// TCP port.
        #[arg(long, default_value_t = server::DEFAULT_PORT)]
        port: u16,
        /// The workspace whose config names the fleet store, the captain and
        /// the telemetry exporters.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,
        /// Start-up grace window, seconds: claims are refused with `grace`
        /// until it ends or every fleet-mode host has heartbeated.
        #[arg(long, default_value_t = DEFAULT_GRACE.as_secs())]
        grace_secs: u64,
        /// The tailnet daemon's LocalAPI socket.
        #[arg(long, value_name = "PATH", default_value = DEFAULT_TAILSCALE_SOCKET)]
        tailscale_socket: PathBuf,
        /// Serve even when this host is not the declared `fleet.captain`.
        #[arg(long)]
        no_captain_gate: bool,
    },
}

impl FleetDispatchArgs {
    pub(crate) async fn run(self) -> Result<()> {
        match self.command {
            FleetDispatchCommand::Serve {
                bind,
                port,
                workspace,
                grace_secs,
                tailscale_socket,
                no_captain_gate,
            } => {
                serve(&bind, port, &workspace, grace_secs, tailscale_socket, no_captain_gate).await
            }
        }
    }
}

async fn serve(
    bind: &str,
    port: u16,
    workspace: &str,
    grace_secs: u64,
    tailscale_socket: PathBuf,
    no_captain_gate: bool,
) -> Result<()> {
    let addr: std::net::IpAddr = bind
        .parse()
        .map_err(|e| anyhow!("invalid --bind address {bind:?}: {e}"))?;
    loom_daemon::serve::validate_bind(addr, true).map_err(|e| anyhow!(e))?;
    let root = loom_daemon::worktree_ops::repo::resolve_repo_root(workspace)
        .with_context(|| format!("resolving workspace {workspace}"))?;

    let gate = loom_daemon::fleet_captain::resolve_gate_for_root(
        &root,
        &loom_daemon::sweep_registry::host_identity(),
    );
    if !gate.is_armed() {
        let message = gate.message("fleet-dispatch serve");
        if !no_captain_gate {
            bail!("{message} (pass --no-captain-gate to serve anyway)");
        }
        log::warn!("{message} — serving anyway (--no-captain-gate)");
    }

    let source: Arc<dyn RosterSource> = Arc::new(FleetStoreRoster::new(&root));
    let loader = Arc::clone(&source);
    let roster = tokio::task::spawn_blocking(move || loader.load())
        .await
        .map_err(|e| anyhow!("roster load task failed: {e}"))?
        .context("loading the dispatcher roster from the fleet store")?;
    if roster.callable() == 0 {
        log::warn!(
            "fleet-dispatch: no dispatcher entry names a `tailnet_node`, so every caller will \
             be refused 403"
        );
    }

    let observability = loom_daemon::observability::read_config(&root);
    let _exporters =
        loom_daemon::observability::standalone_ops::start(&observability, &root, "fleet-dispatch");

    let policy = Policy::for_lease_ttl_minutes(
        loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(),
        Duration::from_secs(grace_secs),
    );
    let epoch = fleet_dispatch::new_epoch();
    let server = Arc::new(Server::new(
        TailscaleWhois::new(tailscale_socket),
        Arc::new(SystemClock::start()),
        roster,
        policy,
        epoch.clone(),
    ));
    let opts = Options::default();
    let listener = tokio::net::TcpListener::bind((addr, port))
        .await
        .map_err(|e| anyhow!("failed to bind {addr}:{port}: {e}"))?;
    println!(
        "loom-daemon fleet-dispatch serve: listening on http://{} (epoch {epoch}, assignment \
         TTL {}s, grace {}s)",
        listener.local_addr()?,
        policy.assignment_ttl.as_secs(),
        policy.grace.as_secs()
    );
    let _maintenance = server::spawn_maintenance(Arc::clone(&server), source, opts);
    server::run(listener, server, opts).await
}
