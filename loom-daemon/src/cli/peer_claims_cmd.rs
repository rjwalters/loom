//! `loom-daemon peer-claims` — a standalone view onto the peer-claim
//! observability surface `loom-daemon status` also renders (Issue #5921).
//!
//! Before this command the ONLY way to compare two hosts' peer-claim views
//! was to grep a `debug!`-level log line or attach a debugger — an
//! undiagnosable gap when a duplicate cross-host dispatch (e.g. #5789) needs
//! root-causing. This is deliberately a thin, single-purpose wrapper around
//! the same `DaemonStatus` IPC round-trip `status` already performs
//! ([`crate::cli::status::query_daemon_status`]) rather than a new request
//! type — `DaemonStatusReport::peer_claims` already carries everything this
//! command renders, and a fleet script comparing two hosts (`ssh host-a
//! loom-daemon peer-claims --json` vs `ssh host-b …`) is the whole point
//! (AC4).

use anyhow::Result;

use loom_daemon::types::PeerClaimStatus;

use super::common::resolve_socket_path;
use super::status::{query_daemon_status, resolve_status_timeout};

/// Handle the `peer-claims` subcommand: one IPC round-trip, then render just
/// the `peer_claims` slice of the report. Exits non-zero only when the daemon
/// itself is unreachable (same failure mode `status` reports) — an empty or
/// absent peer-claim view is a normal, healthy answer (safehouse disabled),
/// never an error.
pub(crate) async fn handle_peer_claims_command(json: bool) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    // #6011: same load-scaled default `status` itself uses — this thin
    // wrapper has no `--timeout-secs` of its own yet, so it always resolves
    // via the automatic (env/load) path.
    let timeout_info = resolve_status_timeout(None);

    let report = match query_daemon_status(&socket_path, &timeout_info).await {
        Ok(report) => report,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "error": format!(
                            "could not reach loom-daemon at {}: {e}",
                            socket_path.display()
                        ),
                    })
                );
            } else {
                eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            }
            std::process::exit(1);
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report.peer_claims)?);
    } else {
        print_peer_claims_human(report.peer_claims.as_ref());
    }

    Ok(())
}

/// Render the human-readable table: one line per tracked claim (`issue`,
/// `repo`, `host`, remaining TTL) plus the four transport counters —
/// mirroring `loom-daemon status`'s "Peer claims" section (same source data,
/// no `--pipeline`-style opt-in gh calls, just the already-fetched report).
fn print_peer_claims_human(peer_claims: Option<&PeerClaimStatus>) {
    let Some(pc) = peer_claims else {
        println!(
            "Peer claims: no coordination established (safehouse.enabled false, or no socket \
             resolved yet — restart to pick up #5921 if this daemon predates it)"
        );
        return;
    };

    println!(
        "Peer claims (self_host: {}, ttl: {}s, room: {})",
        pc.self_host,
        pc.ttl_secs,
        pc.claims_room.as_deref().unwrap_or("none")
    );
    if pc.entries.is_empty() {
        println!("  (no live or pending-prune peer claims)");
    } else {
        for e in &pc.entries {
            println!(
                "  issue #{:<6} repo={:<20} host={:<20} remaining_ttl={}s",
                e.issue, e.repo, e.host, e.remaining_ttl_secs
            );
        }
    }
    println!(
        "  counters: advertised={} received={} expired={} dispatch_skipped={}",
        pc.advertised, pc.received, pc.expired, pc.dispatch_skipped
    );
}
