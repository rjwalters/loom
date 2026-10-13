//! Concern verify for the J2 telemetry shadow streams (§10.2 step 4).
//!
//! 1. Structural: the shared core's [`crate::journal::verify`], restricted to
//!    the included streams that exist (a stream directory appears on its
//!    first shadow append, so an absent one is "no shadow yet", not damage).
//! 2. Normalized comparison of every present stream with its legacy store
//!    ([`super::compare`]).
//! 3. A **durable** report: one record per run appended to a separate
//!    journal at [`ShadowConfig::report_root`] (stream `compare`; kind
//!    `journal.divergence` when anything is unexplained, else
//!    `telemetry.shadow_clean`) — outside the telemetry streams themselves.
//! 4. Optional bounded retention: a stream that compared clean has its
//!    `shadow-verify` cursor committed past the last compared record and is
//!    rotated. Rotation keeps every segment holding an unacknowledged cursor
//!    position of **any** consumer (the core's rule).
//!
//! There is no false green: `ok` needs at least one present stream, a clean
//! structural report, clean comparisons and a written report (when asked).

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use super::compare::{compare_stream, StreamComparison};
use super::{ShadowConfig, TelemetryStream};
use crate::journal::{self, Journal, Position, VerifyOptions, VerifyReport};

/// Consumer name of the acknowledging cursor `--rotate` commits.
pub const VERIFY_CURSOR: &str = "shadow-verify";
/// Stream (under [`ShadowConfig::report_root`]) holding the reports.
pub const REPORT_STREAM: &str = "compare";

#[derive(Debug, Clone, Default)]
pub struct ConcernOptions {
    /// Streams to include; empty means all six.
    pub streams: Vec<TelemetryStream>,
    /// Append the durable report.
    pub write_report: bool,
    /// Acknowledge and rotate streams that compared clean.
    pub rotate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RotationSummary {
    pub stream: String,
    pub deleted_segments: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retained_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConcernReport {
    pub root: PathBuf,
    pub ok: bool,
    /// Nothing could be verified: no included stream exists.
    pub nothing_to_verify: bool,
    pub absent_streams: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structural: Option<VerifyReport>,
    pub comparisons: Vec<StreamComparison>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_seq: Option<u64>,
    pub rotations: Vec<RotationSummary>,
    pub errors: Vec<String>,
}

/// Verify the telemetry concern. Writes only the report (and, with
/// `rotate`, cursors + rotation) — never a telemetry record.
#[must_use]
pub fn verify_concern(
    config: &ShadowConfig,
    workspace: &Path,
    options: &ConcernOptions,
) -> ConcernReport {
    let selected: Vec<TelemetryStream> = if options.streams.is_empty() {
        TelemetryStream::ALL.to_vec()
    } else {
        options.streams.clone()
    };
    let (present, absent): (Vec<_>, Vec<_>) = selected
        .into_iter()
        .partition(|s| config.root.join(s.name()).is_dir());
    let mut report = ConcernReport {
        root: config.root.clone(),
        ok: false,
        nothing_to_verify: present.is_empty(),
        absent_streams: absent.iter().map(|s| s.name().to_owned()).collect(),
        structural: None,
        comparisons: Vec::new(),
        report_seq: None,
        rotations: Vec::new(),
        errors: Vec::new(),
    };
    if present.is_empty() {
        report
            .errors
            .push(format!("no telemetry journal streams under {}", config.root.display()));
        return report;
    }
    let structural = journal::verify(
        &config.root,
        &VerifyOptions {
            streams: present.iter().map(|s| s.name().to_owned()).collect(),
        },
    );
    let mut ends: Vec<(TelemetryStream, Option<Position>)> = Vec::new();
    for stream in &present {
        let (comparison, end) = compare_stream(&config.root, workspace, *stream);
        let structurally_clean = structural
            .streams
            .iter()
            .any(|s| s.stream == stream.name() && s.is_clean());
        if comparison.is_clean() && structurally_clean {
            ends.push((*stream, end));
        }
        report.comparisons.push(comparison);
    }
    let structural_ok = structural.ok;
    report.structural = Some(structural);
    let mut ok = structural_ok && report.comparisons.iter().all(StreamComparison::is_clean);

    if options.write_report {
        match write_report(config, ok, &report) {
            Ok(seq) => report.report_seq = Some(seq),
            Err(e) => {
                report
                    .errors
                    .push(format!("writing divergence report: {e}"));
                ok = false;
            }
        }
    }
    if options.rotate {
        for (stream, end) in ends {
            match acknowledge_and_rotate(config, stream, end) {
                Ok(summary) => report.rotations.push(summary),
                Err(e) => report
                    .errors
                    .push(format!("rotating {}: {e}", stream.name())),
            }
        }
    }
    report.ok = ok && report.errors.is_empty();
    report
}

fn write_report(
    config: &ShadowConfig,
    ok: bool,
    report: &ConcernReport,
) -> Result<u64, journal::JournalError> {
    let structural: Vec<_> = report
        .structural
        .iter()
        .flat_map(|s| s.streams.iter())
        .map(|s| {
            json!({
                "stream": s.stream,
                "clean": s.is_clean(),
                "records": s.records,
                "corrupt_lines": s.corrupt_lines,
                "unknown_major": s.unknown_major,
                "gaps": s.gaps,
                "regressions": s.regressions,
                "errors": s.errors.len(),
            })
        })
        .collect();
    let data = json!({
        "concern": "telemetry",
        "ok": ok,
        "absent_streams": report.absent_streams,
        "structural": structural,
        "comparisons": report.comparisons,
    });
    let kind = if ok {
        "telemetry.shadow_clean"
    } else {
        "journal.divergence"
    };
    let appended = Journal::open(config.report_root())
        .stream(REPORT_STREAM)?
        .append(kind, None, data)?;
    Ok(appended.seq)
}

fn acknowledge_and_rotate(
    config: &ShadowConfig,
    stream: TelemetryStream,
    end: Option<Position>,
) -> Result<RotationSummary, journal::JournalError> {
    let handle = config.journal().stream(stream.name())?;
    if let Some(end) = end {
        handle.cursor(VERIFY_CURSOR)?.commit(end)?;
    }
    let outcome = handle.rotate()?;
    Ok(RotationSummary {
        stream: stream.name().to_owned(),
        deleted_segments: outcome.deleted,
        retained_reason: outcome.retained_reason,
    })
}
