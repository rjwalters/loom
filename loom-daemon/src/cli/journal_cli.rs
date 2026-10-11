//! `loom-daemon journal` — the shared journal core's native seam (#11345).
//!
//! Thin clap → [`loom_daemon::journal`] wiring; `main.rs` is frozen by the
//! file-size ratchet, so the variant there is one line.
//!
//! `journal verify` exit codes: `0` every stream readable and clean, `1`
//! anomalies found (corruption, gap/regression, unknown major, unreadable
//! stream/segment/cursor), `2` nothing could be verified (missing,
//! unreadable or empty root). There is no false green.
//!
//! `journal verify --concern execution` (#11346) instead rebuilds the J1
//! execution projections and diffs them against the legacy stores they
//! shadow, read-only: `0` no unexplained divergence, `1` unexplained
//! divergence, `2` no readable execution journal. Its root defaults to the
//! machine-level journal root (`LOOM_JOURNAL_DIR`, else `~/.loom/journal`).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use loom_daemon::journal::execution::{self, ConcernReport};
use loom_daemon::journal::{verify, VerifyOptions, VerifyReport};

/// `loom-daemon journal` arguments.
#[derive(Args)]
pub(crate) struct JournalArgs {
    #[command(subcommand)]
    command: JournalCommand,
}

#[derive(Subcommand)]
enum JournalCommand {
    /// Structurally verify a journal root: corrupt lines, `seq` gaps and
    /// regressions, unknown envelope majors, unreadable streams. Read-only.
    Verify {
        /// The journal root (or `LOOM_JOURNAL_ROOT`). Required for a
        /// structural check; with `--concern execution` it defaults to the
        /// machine-level root (`LOOM_JOURNAL_DIR`, else `~/.loom/journal`).
        #[arg(long, value_name = "PATH", env = "LOOM_JOURNAL_ROOT")]
        root: Option<PathBuf>,
        /// Verify only this stream (repeatable). Default: every stream.
        #[arg(long = "stream", value_name = "NAME")]
        streams: Vec<String>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// Verify one concern. `execution` (J1, #11346) rebuilds the
        /// execution projections and diffs them against the legacy stores
        /// (read-only). `config` (J3, #11349) restricts to the `config`
        /// stream and also diffs the rebuilt C6/C7 projections against the
        /// legacy files in `--loom-dir`; exits 1 on divergence.
        #[arg(long, value_name = "CONCERN", value_parser = ["execution", "config"])]
        concern: Option<String>,
        /// Legacy state directory for `--concern config` (default: the
        /// daemon's `~/.loom`).
        #[arg(long, value_name = "PATH")]
        loom_dir: Option<PathBuf>,
    },
}

pub(crate) fn dispatch(args: JournalArgs) -> Result<()> {
    let code = match args.command {
        JournalCommand::Verify {
            root,
            concern,
            json,
            ..
        } if concern.as_deref() == Some("execution") => {
            let get = |k: &str| std::env::var(k).ok();
            let root = root.or_else(|| execution::default_root(&get, dirs::home_dir().as_deref()));
            match root {
                Some(root) => run_concern(&execution::verify_concern(&root), json)?,
                None => {
                    eprintln!("journal verify: no journal root (set --root or LOOM_JOURNAL_DIR)");
                    2
                }
            }
        }
        JournalCommand::Verify {
            root,
            mut streams,
            json,
            concern,
            loom_dir,
        } => {
            let Some(root) = root else {
                eprintln!("journal verify: --root (or LOOM_JOURNAL_ROOT) is required");
                std::process::exit(2);
            };
            if concern.is_some() {
                streams = vec![loom_daemon::config_journal::STREAM.to_owned()];
            }
            let report = verify(&root, &VerifyOptions { streams });
            let code = run_verify(&report, json)?;
            if concern.is_some() {
                let dir = loom_dir.or_else(loom_daemon::fleet_store::pending_restart::loom_dir);
                let div = dir.map(|d| loom_daemon::config_journal::divergence(&root, &d));
                if let Some(div) = div {
                    println!("{}", serde_json::to_string_pretty(&div)?);
                    if !div.is_clean() && code == 0 {
                        std::process::exit(1);
                    }
                }
            }
            code
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn run_verify(report: &VerifyReport, json: bool) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print_human(report);
    }
    Ok(if report.ok {
        0
    } else if report.streams.is_empty() {
        2
    } else {
        1
    })
}

fn print_human(report: &VerifyReport) {
    println!("journal {}", report.root.display());
    for error in &report.errors {
        println!("  error: {error}");
    }
    for s in &report.streams {
        let verdict = if s.is_clean() { "ok" } else { "FAIL" };
        println!(
            "  {verdict:4} {}: {} records in {} segments (seq {}..{}), corrupt {}, unknown-major {}, gaps {}, regressions {}{}{}",
            s.stream,
            s.records,
            s.segments,
            s.first_seq.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            s.last_seq.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            s.corrupt_lines,
            s.unknown_major,
            s.gaps,
            s.regressions,
            if s.needs_rebuild { ", needs rebuild" } else { "" },
            if s.torn_tail { ", torn tail (repaired by the next writer)" } else { "" },
        );
        for error in &s.errors {
            println!("       error: {error}");
        }
    }
    println!("{}", if report.ok { "OK" } else { "NOT OK" });
}

fn run_concern(report: &ConcernReport, json: bool) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!("journal {} (concern {})", report.root.display(), report.concern);
        for error in &report.errors {
            println!("  error: {error}");
        }
        println!(
            "  {} records, corrupt {}, unknown-major {}{}, recorded divergences {}",
            report.records,
            report.corrupt_lines,
            report.unknown_major,
            if report.torn_tail { ", torn tail" } else { "" },
            report.recorded_divergences
        );
        for s in &report.stores {
            let explained: Vec<String> = s
                .explained
                .iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect();
            println!(
                "  {:4} {} {}: compared {}, matched {}, unexplained {}{}",
                if s.unexplained == 0 { "ok" } else { "FAIL" },
                s.store,
                s.path,
                s.compared,
                s.matched,
                s.unexplained,
                if explained.is_empty() {
                    String::new()
                } else {
                    format!(" (explained: {})", explained.join(", "))
                }
            );
        }
        for d in &report.unexplained {
            println!("  divergence {} {} [{}]: {}", d.store, d.path, d.item, d.fields.join(","));
        }
        println!("{}", if report.ok { "OK" } else { "NOT OK" });
    }
    Ok(if report.ok {
        0
    } else if report.errors.is_empty() {
        1
    } else {
        2
    })
}
