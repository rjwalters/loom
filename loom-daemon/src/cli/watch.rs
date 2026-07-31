//! `loom-daemon watch` handler (Issue #4712 — split out of `main.rs`).

use anyhow::Result;

use loom_daemon::types::{Request, Response};
use loom_daemon::watch_registry;

use super::common::{query_daemon, resolve_socket_path};
use crate::WatchAction;

/// Handle the `watch` subcommand (Issue #3971). Connects to the running daemon
/// over its Unix socket and registers/lists/removes durable watches. The watches
/// are persisted machine-level (`~/.loom/watches.json`), so — like the daemon's
/// other file-backed state — they survive both this shell and a daemon restart;
/// the resolution report lands in `~/.loom/logs/watch-results.log`.
pub(crate) async fn handle_watch_command(action: WatchAction) -> Result<()> {
    use loom_daemon::watch_registry::WatchKind;

    let socket_path = resolve_socket_path()?;
    // `json_out` only applies to `list`; captured here so the request build below
    // does not have to smuggle it back out.
    let mut json_out = false;
    let request = match action {
        WatchAction::Add {
            number,
            pr,
            repo,
            workspace_root,
            note,
        } => Request::RegisterWatch {
            kind: if pr { WatchKind::Pr } else { WatchKind::Issue },
            number,
            repo,
            workspace_root,
            note,
        },
        WatchAction::List { json } => {
            json_out = json;
            Request::ListWatches
        }
        WatchAction::Remove { id } => Request::RemoveWatch { id },
    };

    match query_daemon(&socket_path, &request).await {
        Ok(Response::WatchRegistered {
            watch,
            already_present,
        }) => {
            if already_present {
                println!("Already watching {} (id {}) — no-op.", watch.target_label(), watch.id);
            } else {
                println!("Registered watch on {}", watch.target_label());
                println!("  id:   {}", watch.id);
                println!("Terminal state will be recorded to {}", watch_results_log_hint());
            }
            Ok(())
        }
        Ok(Response::WatchList { watches }) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&watches)?);
            } else if watches.is_empty() {
                println!("No durable watches registered.");
            } else {
                println!("{} durable watch(es):", watches.len());
                for w in &watches {
                    let note = w
                        .note
                        .as_deref()
                        .map(|n| format!(" — {n}"))
                        .unwrap_or_default();
                    println!("  {}  {}{}", w.id, w.target_label(), note);
                }
                println!();
                println!("Resolutions are recorded to {}", watch_results_log_hint());
            }
            Ok(())
        }
        Ok(Response::WatchRemoved { id, was_present }) => {
            if was_present {
                println!("Removed watch {id}.");
            } else {
                println!("No watch with id {id} — nothing to remove (no-op).");
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

/// Best-effort human hint for where watch results are recorded (honors the env
/// override). Purely cosmetic — never fails.
fn watch_results_log_hint() -> String {
    watch_registry::default_results_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.loom/logs/watch-results.log".to_string())
}
