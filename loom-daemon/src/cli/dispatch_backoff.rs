//! `loom-daemon dispatch-backoff` handler (Issue #6192).
//!
//! Operator/script-reachable surface onto the per-issue dispatch-backoff
//! machinery (Issue #4485), whose state lives entirely in the running
//! daemon's in-memory `SweepRegistry` — the same "connect over the Unix
//! socket, no on-disk state" shape as `loom-daemon quarantine`
//! ([`crate::cli::quarantine`]).
//!
//! Currently exposes only `record`: `Quarantine clear` already releases a
//! dispatch-backoff window as a documented side effect, so no separate
//! `dispatch-backoff clear` is added here.

use anyhow::Result;

use loom_daemon::types::{Request, Response};

use super::common::{query_daemon, resolve_socket_path};
use crate::DispatchBackoffAction;

/// Handle the `dispatch-backoff` subcommand.
pub(crate) async fn handle_dispatch_backoff_command(action: DispatchBackoffAction) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    match action {
        DispatchBackoffAction::Record {
            issue,
            reason,
            workspace_root,
        } => {
            let request = Request::RecordDispatchFailure {
                issue,
                reason,
                workspace_root,
            };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::DispatchFailureRecorded {
                    issue,
                    consecutive,
                    backoff_secs,
                }) => {
                    match backoff_secs {
                        Some(secs) => println!(
                            "Recorded dispatch failure for issue #{issue} — {consecutive} \
                             consecutive, next attempt allowed in {secs}s (#4485)."
                        ),
                        None => println!(
                            "Dispatch backoff is disabled — recorded issue #{issue}'s failure \
                             for observability only, no window armed."
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
