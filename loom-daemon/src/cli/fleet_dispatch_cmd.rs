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
    table::{Policy, DEFAULT_GRACE, DEFAULT_NO_PROGRESS_LADDER},
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
        /// The no-progress cooldown ladder, seconds, comma-separated (#12541):
        /// the fleet-wide hold on an `(repo, item, step)` after its first,
        /// second, third and later consecutive releases that did not move
        /// the item. Up to four rungs; the last repeats. `0` turns it off.
        #[arg(
            long,
            value_name = "SECS[,SECS...]",
            default_value = "1800,7200,21600,86400"
        )]
        no_progress_cooldown_secs: String,
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
                no_progress_cooldown_secs,
                tailscale_socket,
                no_captain_gate,
            } => {
                let ladder = parse_ladder(&no_progress_cooldown_secs)?;
                serve(
                    &bind,
                    port,
                    &workspace,
                    grace_secs,
                    ladder,
                    tailscale_socket,
                    no_captain_gate,
                )
                .await
            }
        }
    }
}

/// The `--no-progress-cooldown-secs` ladder: one to four whole seconds,
/// the last repeated to fill four rungs.
fn parse_ladder(spec: &str) -> Result<[Duration; 4]> {
    let rungs: Vec<u64> = spec
        .split(',')
        .map(|r| r.trim().parse::<u64>())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("invalid --no-progress-cooldown-secs {spec:?}: {e}"))?;
    if rungs.is_empty() || rungs.len() > DEFAULT_NO_PROGRESS_LADDER.len() {
        bail!("--no-progress-cooldown-secs takes one to four values, got {spec:?}");
    }
    let mut ladder = [Duration::ZERO; 4];
    for (i, slot) in ladder.iter_mut().enumerate() {
        *slot = Duration::from_secs(rungs[i.min(rungs.len() - 1)]);
    }
    Ok(ladder)
}

async fn serve(
    bind: &str,
    port: u16,
    workspace: &str,
    grace_secs: u64,
    no_progress: [Duration; 4],
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
    let unresolved = roster.unresolved();
    if !unresolved.is_empty() {
        log::warn!(
            "fleet-dispatch: {} of {} dispatchers have no host record with a `host_id` and a \
             `tailnet_node` in fleet.json `hosts[]`, so their calls will be refused 403: {}",
            unresolved.len(),
            roster.host_ids().len(),
            unresolved.join(", ")
        );
    }

    let observability = loom_daemon::observability::read_config(&root);
    let _exporters =
        loom_daemon::observability::standalone_ops::start(&observability, &root, "fleet-dispatch");

    let policy = Policy::for_lease_ttl_minutes(
        loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(),
        Duration::from_secs(grace_secs),
    )
    .with_no_progress(no_progress);
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
         TTL {}s, grace {}s, no-progress cooldown {:?}s)",
        listener.local_addr()?,
        policy.assignment_ttl.as_secs(),
        policy.grace.as_secs(),
        policy.no_progress.map(|d| d.as_secs())
    );
    let _maintenance = server::spawn_maintenance(Arc::clone(&server), source, opts);
    server::run(listener, server, opts).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_parses_pads_and_rejects() {
        let secs = |s: &str| parse_ladder(s).unwrap().map(|d| d.as_secs());
        assert_eq!(secs("1800,7200,21600,86400"), [1800, 7200, 21600, 86400]);
        assert_eq!(secs("600"), [600; 4]);
        assert_eq!(secs("60, 120"), [60, 120, 120, 120]);
        assert_eq!(secs("0"), [0; 4]);
        assert!(parse_ladder("").is_err());
        assert!(parse_ladder("1,2,3,4,5").is_err());
        assert!(parse_ladder("ten").is_err());
    }
}
