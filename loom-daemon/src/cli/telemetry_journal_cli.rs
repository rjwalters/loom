//! `loom-daemon telemetry append|verify` — the J2 telemetry journal's native
//! seam (Epic #9908, Phase 2d, #11348).
//!
//! Kept in its own module (one variant in [`super::telemetry`]) so sibling
//! Phase 2 slices that touch `journal_cli.rs` do not conflict with it.
//!
//! `append` reads one record (stdin by default), validates and bounds it, and
//! shadow-appends it. It is a no-op (exit 0) while shadowing is disabled and
//! no `--root` is given. Exit codes: `0` appended or disabled, `1` journal
//! failure, `2` invalid input. Shell call sites ignore every non-zero exit: a
//! shadow failure never fails the legacy write that preceded it.
//!
//! `verify` is the concern verify: structural check of the included streams,
//! normalized comparison with the legacy stores, a durable report. Exit codes:
//! `0` clean, `1` anomalies or unexplained divergence, `2` nothing to verify
//! (disabled, or no stream exists yet).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use loom_daemon::telemetry::journal_shadow::{
    append_with, input,
    verify::{verify_concern, ConcernOptions, ConcernReport},
    ShadowConfig, TelemetryStream,
};

/// `loom-daemon telemetry` arguments.
#[derive(Args)]
pub(crate) struct TelemetryJournalArgs {
    #[command(subcommand)]
    command: TelemetryJournalCommand,
}

#[derive(Subcommand)]
enum TelemetryJournalCommand {
    /// Shadow-append one telemetry record (JSON, stdin by default) to a J2
    /// stream, after the caller's legacy write succeeded. No-op unless
    /// `LOOM_TELEMETRY_JOURNAL` is truthy or `--root` is given.
    Append {
        /// sweep | role_tick | rework | merge_queue | shell | experiment.
        #[arg(long, default_value = "shell")]
        stream: String,
        /// Journal record kind. Default: the envelope's `record.kind`
        /// (or a top-level `kind`).
        #[arg(long)]
        kind: Option<String>,
        /// Natural key for replay de-duplication.
        #[arg(long)]
        key: Option<String>,
        /// Workspace whose journal root applies. Default: current directory.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// Explicit telemetry journal root (enables the append).
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// Read the record from this file instead of stdin (`-`).
        #[arg(long, value_name = "PATH", default_value = "-")]
        input: PathBuf,
    },
    /// Verify the telemetry concern: structural check, normalized comparison
    /// with the legacy stores, durable divergence report.
    Verify {
        /// Workspace whose legacy stores are compared. Default: current directory.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// Explicit telemetry journal root. Default: the enabled configuration.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// Include only this stream (repeatable). Default: all six.
        #[arg(long = "stream", value_name = "NAME")]
        streams: Vec<String>,
        /// Do not append the durable report.
        #[arg(long)]
        no_report: bool,
        /// Acknowledge and rotate streams that compared clean.
        #[arg(long)]
        rotate: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
}

impl TelemetryJournalArgs {
    pub(crate) fn run(self) -> Result<()> {
        let code = match self.command {
            TelemetryJournalCommand::Append {
                stream,
                kind,
                key,
                workspace,
                root,
                input,
            } => run_append(&stream, kind.as_deref(), key.as_deref(), workspace, root, &input),
            TelemetryJournalCommand::Verify {
                workspace,
                root,
                streams,
                no_report,
                rotate,
                json,
            } => run_verify(workspace, root, &streams, !no_report, rotate, json)?,
        };
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
}

fn workspace_or_cwd(workspace: Option<PathBuf>) -> PathBuf {
    workspace
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_for(root: Option<PathBuf>, workspace: &Path) -> Option<ShadowConfig> {
    match root {
        Some(root) => Some(ShadowConfig::at(root)),
        None => ShadowConfig::resolve(workspace),
    }
}

fn parse_stream(name: &str) -> Result<TelemetryStream, String> {
    TelemetryStream::from_name(name).ok_or_else(|| {
        let names: Vec<_> = TelemetryStream::ALL.iter().map(|s| s.name()).collect();
        format!("unknown stream {name:?}; expected one of {names:?}")
    })
}

fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let limit = input::MAX_INPUT_BYTES as u64 + 1;
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        std::io::stdin()
            .lock()
            .take(limit)
            .read_to_end(&mut bytes)?;
    } else {
        std::fs::File::open(path)?
            .take(limit)
            .read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

fn run_append(
    stream: &str,
    kind: Option<&str>,
    key: Option<&str>,
    workspace: Option<PathBuf>,
    root: Option<PathBuf>,
    input_path: &Path,
) -> i32 {
    let refuse = |reason: String| {
        eprintln!("telemetry append: {reason}");
        2
    };
    let stream = match parse_stream(stream) {
        Ok(stream) => stream,
        Err(e) => return refuse(e),
    };
    let bytes = match read_bounded(input_path) {
        Ok(bytes) => bytes,
        Err(e) => return refuse(format!("cannot read input: {e}")),
    };
    let record = match input::validate(stream, &bytes, kind, key) {
        Ok(record) => record,
        Err(e) => return refuse(e),
    };
    let workspace = workspace_or_cwd(workspace);
    let Some(config) = config_for(root, &workspace) else {
        println!(r#"{{"appended":false,"reason":"disabled"}}"#);
        return 0;
    };
    match append_with(&config, stream, record) {
        Ok(seq) => {
            println!(
                "{}",
                serde_json::json!({"appended": true, "stream": stream.name(), "seq": seq})
            );
            0
        }
        Err(e) => {
            eprintln!("telemetry append: {} journal append failed: {e}", stream.name());
            1
        }
    }
}

fn run_verify(
    workspace: Option<PathBuf>,
    root: Option<PathBuf>,
    streams: &[String],
    write_report: bool,
    rotate: bool,
    json: bool,
) -> Result<i32> {
    let workspace = workspace_or_cwd(workspace);
    let streams = streams
        .iter()
        .map(|s| parse_stream(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::msg)?;
    let Some(config) = config_for(root, &workspace) else {
        eprintln!("telemetry verify: telemetry journal shadowing is disabled (set LOOM_TELEMETRY_JOURNAL or pass --root)");
        return Ok(2);
    };
    let report = verify_concern(
        &config,
        &workspace,
        &ConcernOptions {
            streams,
            write_report,
            rotate,
        },
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&report).context("encoding report")?);
    } else {
        print_human(&report);
    }
    Ok(if report.ok {
        0
    } else if report.nothing_to_verify {
        2
    } else {
        1
    })
}

fn print_human(report: &ConcernReport) {
    println!("telemetry journal {}", report.root.display());
    for error in &report.errors {
        println!("  error: {error}");
    }
    if let Some(structural) = &report.structural {
        for s in &structural.streams {
            let verdict = if s.is_clean() { "ok" } else { "FAIL" };
            println!(
                "  {verdict:4} {} structure: {} records, corrupt {}, unknown-major {}, gaps {}, regressions {}",
                s.stream, s.records, s.corrupt_lines, s.unknown_major, s.gaps, s.regressions
            );
        }
    }
    for c in &report.comparisons {
        let verdict = if c.is_clean() { "ok" } else { "DIVERGED" };
        println!(
            "  {verdict:4} {} vs legacy: matched {}, journal {}, legacy {} (legacy-only {} unexplained / {} pre-shadow, journal-only {} unexplained / {} pre-retention)",
            c.stream,
            c.matched,
            c.journal_records,
            c.legacy_records,
            c.legacy_only_unexplained,
            c.legacy_only_predating_shadow,
            c.journal_only_unexplained,
            c.journal_only_predating_legacy,
        );
        for e in &c.errors {
            println!("       error: {e}");
        }
    }
    if !report.absent_streams.is_empty() {
        println!("  absent (no shadow yet): {}", report.absent_streams.join(", "));
    }
    if let Some(seq) = report.report_seq {
        println!("  report recorded (seq {seq})");
    }
    for r in &report.rotations {
        println!(
            "  rotated {}: deleted {} segment(s){}",
            r.stream,
            r.deleted_segments.len(),
            r.retained_reason
                .as_deref()
                .map_or_else(String::new, |why| format!(" ({why})"))
        );
    }
    println!("{}", if report.ok { "OK" } else { "NOT OK" });
}
