//! `loom-daemon noop-cooldown` handler (Issue #6670).
//!
//! Operator/script-reachable surface onto the per-issue no-op-cooldown
//! machinery, whose state lives entirely in the running daemon's in-memory
//! `SweepRegistry` — the same "connect over the Unix socket, no on-disk
//! state" shape as `loom-daemon dispatch-backoff`
//! ([`crate::cli::dispatch_backoff`]) and `loom-daemon quarantine`
//! ([`crate::cli::quarantine`]).
//!
//! Currently exposes only `record`: `Quarantine clear` already releases a
//! no-op cooldown window as a documented side effect, so no separate
//! `noop-cooldown clear` is added here — mirroring `dispatch-backoff`'s own
//! rationale.

use anyhow::Result;

use loom_daemon::types::{Request, Response};

use super::common::{query_daemon, resolve_socket_path};
use crate::NoopCooldownAction;

/// Handle the `noop-cooldown` subcommand.
pub(crate) async fn handle_noop_cooldown_command(action: NoopCooldownAction) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    match action {
        NoopCooldownAction::Record {
            issue,
            reason,
            workspace_root,
        } => {
            let request = Request::RecordNoopRelease {
                issue,
                reason,
                workspace_root,
            };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::NoopReleaseRecorded {
                    issue,
                    consecutive,
                    cooldown_secs,
                }) => {
                    match cooldown_secs {
                        Some(secs) => println!(
                            "Recorded no-op release for issue #{issue} — {consecutive} \
                             consecutive, next dispatch allowed in {secs}s (#6670)."
                        ),
                        None => println!(
                            "No-op re-dispatch cooldown is disabled — recorded issue #{issue}'s \
                             no-op release for observability only, no window armed."
                        ),
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("Daemon error: {message}");
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("Unexpected response from daemon: {other:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
                    eprintln!();
                    eprintln!("Is the daemon running? Start it with:");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
                    std::process::exit(1);
                }
            }
        }
    }
}
