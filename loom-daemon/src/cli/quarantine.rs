//! `loom-daemon quarantine` handler (Issue #4712 — split out of `main.rs`).

use anyhow::Result;
use std::path::Path;

use loom_daemon::types::{QuarantineEntry, Request, Response};

use super::common::{query_daemon, resolve_socket_path};
use crate::QuarantineAction;

/// Handle the `quarantine` subcommand (Issue #3939). Connects to the running
/// daemon over its Unix socket and dispatches the requested action. The
/// quarantine state is in the daemon's memory, so — unlike `workspace` — this
/// cannot operate on a file when the daemon is down.
pub(crate) async fn handle_quarantine_command(action: QuarantineAction) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    match action {
        QuarantineAction::Clear {
            issue,
            workspace_root,
        } => {
            let request = Request::ClearQuarantine {
                issue,
                workspace_root,
            };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::QuarantineCleared {
                    issue,
                    was_quarantined,
                }) => {
                    if was_quarantined {
                        println!(
                            "Cleared quarantine for issue #{issue} — it will re-qualify for \
                             dispatch and `loom:issue` has been restored on the forge."
                        );
                    } else {
                        // #4485: the clear ALSO releases any per-issue dispatch
                        // backoff window, which exists independently of
                        // quarantine — so "not quarantined" is not the same as
                        // "nothing happened".
                        println!(
                            "Issue #{issue} was not quarantined — nothing to clear (no-op). \
                             Any dispatch-backoff window (#4485) for it was released."
                        );
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

        QuarantineAction::List { workspace_root } => {
            let request = Request::ListQuarantines { workspace_root };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::QuarantineList { entries }) => {
                    render_quarantine_list(&entries);
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

/// Render `loom-daemon quarantine list` output (Issue #4215): one line per
/// entry, grouped by workspace root (in the all-workspaces case there may be
/// several), with a disambiguation footer whenever there is at least one
/// entry to show. Entries within a group are already issue-sorted by
/// [`SweepRegistry::quarantine_entries`]; `BTreeMap` keeps groups themselves
/// ordered (by path) for deterministic output across runs.
fn render_quarantine_list(entries: &[QuarantineEntry]) {
    if entries.is_empty() {
        println!("no active quarantines");
        return;
    }

    let mut by_root: std::collections::BTreeMap<&Path, Vec<&QuarantineEntry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        by_root
            .entry(entry.workspace_root.as_path())
            .or_default()
            .push(entry);
    }

    for (root, group) in &by_root {
        println!("{}:", root.display());
        for e in group {
            println!(
                "  #{}  insta-crash {}/{}  applied {}  ttl remaining {}s",
                e.issue,
                e.insta_crash_count,
                e.insta_crash_threshold,
                e.quarantined_at.to_rfc3339(),
                e.ttl_remaining_secs
            );
        }
    }

    println!();
    println!(
        "Note: `loom:blocked` on the forge may be a quarantine or a real dependency — this \
         command is the authority for quarantines."
    );
}
