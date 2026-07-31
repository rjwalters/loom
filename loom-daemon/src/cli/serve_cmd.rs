//! `loom-daemon serve` handler (Issue #4712 — split out of `main.rs`). The
//! actual HTTP listener/dashboard implementation lives in the lib crate's
//! `serve` module (`loom_daemon::serve`); this is thin clap→module wiring.

use anyhow::{anyhow, Result};

use loom_daemon::serve;

use super::common::resolve_socket_path;

/// Handle the `serve` subcommand (Issue #4391, dashboard phase 1 of #4329):
/// validate the requested bind address against the non-negotiable security
/// posture (loopback by default; non-loopback requires the explicit
/// `--allow-non-loopback` opt-in; a wildcard bind is refused unconditionally
/// — see [`serve::validate_bind`]), bind the TCP listener, and hand off to
/// [`serve::run`] for the accept loop. Each request re-fetches a fresh
/// snapshot over the daemon's existing Unix socket — this function never
/// touches daemon state directly.
pub(crate) async fn handle_serve_command(
    port: u16,
    bind: &str,
    allow_non_loopback: bool,
    peers: &str,
) -> Result<()> {
    let addr: std::net::IpAddr = bind
        .parse()
        .map_err(|e| anyhow!("invalid --bind address {bind:?}: {e}"))?;
    serve::validate_bind(addr, allow_non_loopback).map_err(|e| anyhow!(e))?;

    let socket_path = resolve_socket_path()?;
    let listener = tokio::net::TcpListener::bind((addr, port))
        .await
        .map_err(|e| anyhow!("failed to bind {addr}:{port}: {e}"))?;
    let local_addr = listener.local_addr()?;
    let peer_list = serve::parse_peer_list(peers);
    println!(
        "loom-daemon serve: listening on http://{local_addr}/ (dashboard), \
         http://{local_addr}/api/status, http://{local_addr}/api/events, \
         http://{local_addr}/api/pipeline, http://{local_addr}/api/tokens, \
         http://{local_addr}/api/peers (proxying {}){}",
        socket_path.display(),
        if peer_list.is_empty() {
            String::new()
        } else {
            format!(" — {} configured peer(s)", peer_list.len())
        }
    );

    let state = serve::ServeState::new(socket_path).with_peers(peer_list);
    serve::run_with_state(listener, state).await
}
