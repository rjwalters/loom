//! `loom-daemon telemetry-replay --check` (#10196 R7, #11128): does each
//! covered host's reconstructed view agree with the forge?
//!
//! The comparison is R3's committed SQL, not Rust: queries 6 (disagreement
//! runs) and 7 (the per-host agreement report) of
//! `defaults/observability/signoz/replay-queries.sql`, each run with the
//! replay prefix and the agreement block exactly as committed. Over the
//! sample instants `t, t - step, ... t - span` they rebuild every host's view,
//! compare each covered host's own rows with the webhook-derived label state,
//! and measure each disagreement as a run of consecutive covered instants.
//!
//! What this module adds is the reader's half: binding the parameters,
//! parsing the rows, and the verdict. The check **fails** when any row of
//! query 6 is `over_threshold`; only a covered host has rows there, so an
//! uncovered host is reported `unknown` and never fails it.
//!
//! It reports agreement, lag and coverage counts and exits by threshold. It
//! computes no estimate and no statistic (#11098), and depends on nothing in
//! the `eta` module.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::{
    field, flag, json, lines, prefix_and_statements, text, uint, ReplayParams, REPLAY_QUERIES,
};
use crate::signoz_read::{ClickhouseHttp, ReadError};

const AGREEMENT_BEGIN: &str = "-- >>> agreement-prefix";
const AGREEMENT_END: &str = "-- <<< agreement-prefix";

/// Default instants sampled: the hour before `t`.
pub const DEFAULT_SPAN_SEC: u32 = 3600;

/// Default disagreement a covered host may show before the check fails:
/// two `fleet.state` passes, so a normal listing lag never trips it.
pub const DEFAULT_THRESHOLD_SEC: u32 = 600;

/// Exit status: every covered host agrees with the forge within the threshold.
pub const EXIT_AGREE: i32 = 0;
/// Exit status: a covered host disagreed with the forge for longer.
pub const EXIT_DISAGREE: i32 = 1;
/// Exit status: the store or the export could not be read.
pub const EXIT_UNREADABLE: i32 = 2;

/// Queries 6 (disagreements) and 7 (per-host report) as a reader runs them:
/// the replay prefix, then (query 7 only) the agreement block, then the
/// statement. Query 6 already continues with the agreement block.
///
/// # Errors
///
/// `sql` no longer has the documented shape.
pub fn check_and_report_queries(sql: &str) -> Result<(String, String), String> {
    let (prefix, statements) = prefix_and_statements(sql)?;
    let block = super::marked(sql, AGREEMENT_BEGIN, AGREEMENT_END, "agreement-prefix")?;
    let (check, report) = (&statements[8], &statements[9]);
    if !check.starts_with(&block) {
        return Err("replay-queries.sql: query 6 does not begin with the agreement block".into());
    }
    Ok((format!("{prefix}\n{check}"), format!("{prefix}\n{block}\n{report}")))
}

/// The bound parameters of queries 6 and 7.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckParams {
    /// The newest instant (`as_of`), the window and the repo scope.
    pub replay: ReplayParams,
    /// Seconds before `as_of` to sample back to.
    pub span_sec: u32,
    /// Seconds between sample instants (at least 1).
    pub step_sec: u32,
    /// Seconds a covered host may disagree before the check fails.
    pub threshold_sec: u32,
}

impl CheckParams {
    /// As ClickHouse query parameters.
    #[must_use]
    pub fn params(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .replay
            .params()
            .into_iter()
            .filter(|(k, _)| k != "span" && k != "step")
            .collect();
        out.push(("span".to_string(), self.span_sec.to_string()));
        out.push(("step".to_string(), self.step_sec.max(1).to_string()));
        out.push(("threshold".to_string(), self.threshold_sec.to_string()));
        out
    }

    /// The number of instants sampled.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.span_sec / self.step_sec.max(1) + 1
    }
}

/// One run of a covered host disagreeing with the forge (query 6's row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Disagreement {
    pub emitter: String,
    pub repo: String,
    pub issue: u64,
    pub pr: u64,
    /// The stage the host's own view names at the run's last instant. A run
    /// is keyed on the host's item, not on a stage pair: it continues while
    /// either side changes stage and the two still disagree.
    pub host_stage: String,
    /// The stage the forge's labels name (`none`, `closed`, or a stage) at the
    /// run's last instant.
    pub forge_stage: String,
    /// Every forge stage seen during the run, sorted.
    pub forge_stages: Vec<String>,
    /// Consecutive covered instants disagreeing, times the step.
    pub disagree_sec: u64,
    pub first_at: String,
    pub last_at: String,
    pub host_entered_at: String,
    /// When the forge's labels last changed.
    pub forge_since: String,
    pub over_threshold: bool,
}

/// One host's agreement over the instants (query 7's row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostAgreement {
    pub emitter: String,
    pub samples: u64,
    pub covered_samples: u64,
    /// `covered` (every instant), `partial`, or `unknown` (none).
    pub coverage: String,
    /// The chain states the host was not covered in.
    pub uncovered_states: Vec<String>,
    pub compared: u64,
    pub agreeing: u64,
    pub disagreeing: u64,
    /// Rows with no PR, no webhook record, or an unmapped stage.
    pub not_comparable: u64,
    /// The host's view lag: its longest disagreement.
    pub longest_disagreement_sec: u64,
    pub disagreements_over_threshold: u64,
    /// Anchors whose received chunks != `chunk_count` (never used).
    pub incomplete_anchors: u64,
    pub incomplete_deltas: u64,
}

/// The check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub as_of: DateTime<Utc>,
    pub span_sec: u32,
    pub step_sec: u32,
    pub threshold_sec: u32,
    pub window_sec: u32,
    /// Empty for all repositories.
    pub repo: String,
    pub hosts: Vec<HostAgreement>,
    pub disagreements: Vec<Disagreement>,
}

impl Check {
    /// The disagreements that fail the check.
    pub fn failures(&self) -> impl Iterator<Item = &Disagreement> {
        self.disagreements.iter().filter(|d| d.over_threshold)
    }

    /// [`EXIT_DISAGREE`] when any covered host disagreed past the threshold,
    /// else [`EXIT_AGREE`].
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.failures().next().is_some() {
            EXIT_DISAGREE
        } else {
            EXIT_AGREE
        }
    }
}

fn strings(row: &Value, key: &str) -> Result<Vec<String>, String> {
    match field(row, key)? {
        Value::Array(items) => Ok(items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()),
        other => Err(format!("column {key} is not an array: {other}")),
    }
}

fn disagreement(row: &Value) -> Result<Disagreement, String> {
    Ok(Disagreement {
        emitter: text(row, "emitter")?,
        repo: text(row, "repo")?,
        issue: uint(row, "issue")?,
        pr: uint(row, "pr")?,
        host_stage: text(row, "host_stage")?,
        forge_stage: text(row, "forge_stage")?,
        forge_stages: strings(row, "forge_stages")?,
        disagree_sec: uint(row, "disagree_sec")?,
        first_at: text(row, "first_at")?,
        last_at: text(row, "last_at")?,
        host_entered_at: text(row, "host_entered_at")?,
        forge_since: text(row, "forge_since")?,
        over_threshold: flag(row, "over_threshold")?,
    })
}

fn host(row: &Value) -> Result<HostAgreement, String> {
    Ok(HostAgreement {
        emitter: text(row, "emitter")?,
        samples: uint(row, "samples")?,
        covered_samples: uint(row, "covered_samples")?,
        coverage: text(row, "coverage")?,
        uncovered_states: strings(row, "uncovered_states")?,
        compared: uint(row, "compared")?,
        agreeing: uint(row, "agreeing")?,
        disagreeing: uint(row, "disagreeing")?,
        not_comparable: uint(row, "not_comparable")?,
        longest_disagreement_sec: uint(row, "longest_disagreement_sec")?,
        disagreements_over_threshold: uint(row, "disagreements_over_threshold")?,
        incomplete_anchors: uint(row, "incomplete_anchors")?,
        incomplete_deltas: uint(row, "incomplete_deltas")?,
    })
}

/// Assemble a [`Check`] from query 6's and query 7's `JSONEachRow` output.
///
/// # Errors
///
/// A line is not JSON or lacks a column the committed query selects.
pub fn assemble(params: &CheckParams, disagreements: &str, report: &str) -> Result<Check, String> {
    let disagreements = lines(disagreements)
        .map(|(n, l)| json(n, l).and_then(|v| disagreement(&v)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("disagreements: {e}"))?;
    let hosts = lines(report)
        .map(|(n, l)| json(n, l).and_then(|v| host(&v)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("report: {e}"))?;
    Ok(Check {
        as_of: params.replay.as_of,
        span_sec: params.span_sec,
        step_sec: params.step_sec,
        threshold_sec: params.threshold_sec,
        window_sec: params.replay.window_sec,
        repo: params.replay.repo.clone(),
        hosts,
        disagreements,
    })
}

/// [`assemble`] from one export holding both queries' rows (`--print-sql
/// --check` output run with `clickhouse-client --format JSONEachRow`): a row
/// with a `samples` column is the report, any other row a disagreement.
///
/// # Errors
///
/// As for [`assemble`].
pub fn assemble_export(params: &CheckParams, export: &str) -> Result<Check, String> {
    let (mut runs, mut report) = (String::new(), String::new());
    for (n, line) in lines(export) {
        let target = if json(n, line)?.get("samples").is_some() {
            &mut report
        } else {
            &mut runs
        };
        target.push_str(line);
        target.push('\n');
    }
    assemble(params, &runs, &report)
}

/// Run queries 6 and 7 against the live store and assemble the result.
///
/// # Errors
///
/// The committed SQL has an unexpected shape, the store could not be read, or
/// it returned rows this reader cannot parse.
pub fn fetch(http: &ClickhouseHttp, params: &CheckParams) -> Result<Check, String> {
    let (check_sql, report_sql) = check_and_report_queries(REPLAY_QUERIES)?;
    let bound = params.params();
    let post = |sql: &str, which: &str| {
        http.post_with(&format!("{sql}\nFORMAT JSONEachRow"), &bound)
            .map_err(|e| match e {
                ReadError::Unavailable(why) => format!("{which} query: store unavailable: {why}"),
                ReadError::Refused(why) => format!("{which} query: store refused: {why}"),
            })
    };
    let runs = post(&check_sql, "disagreement")?;
    let report = post(&report_sql, "report")?;
    assemble(params, &runs, &report)
}

/// Human-readable report: every host, every disagreement, then the verdict.
#[must_use]
pub fn render(check: &Check) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let scope = if check.repo.is_empty() {
        "all repos"
    } else {
        check.repo.as_str()
    };
    let first = check.as_of - chrono::Duration::seconds(i64::from(check.span_sec));
    let _ = writeln!(
        out,
        "agreement with the forge, {} to {} every {}s ({scope}, window {}s, threshold {}s)",
        first.to_rfc3339(),
        check.as_of.to_rfc3339(),
        check.step_sec.max(1),
        check.window_sec,
        check.threshold_sec
    );
    let count = |c: &str| check.hosts.iter().filter(|h| h.coverage == c).count();
    let _ = writeln!(
        out,
        "hosts: {} ({} covered, {} partial, {} unknown)",
        check.hosts.len(),
        count("covered"),
        count("partial"),
        count("unknown")
    );
    for h in &check.hosts {
        let _ = write!(
            out,
            "  {:<24} {:<8} {}/{} instants",
            h.emitter, h.coverage, h.covered_samples, h.samples
        );
        if !h.uncovered_states.is_empty() {
            let _ = write!(out, " uncovered={}", h.uncovered_states.join(","));
        }
        let _ = write!(
            out,
            " compared={} agree={} disagree={} not_comparable={} longest={}s",
            h.compared, h.agreeing, h.disagreeing, h.not_comparable, h.longest_disagreement_sec
        );
        if h.incomplete_anchors + h.incomplete_deltas > 0 {
            let _ = write!(
                out,
                " incomplete_anchors={} incomplete_deltas={}",
                h.incomplete_anchors, h.incomplete_deltas
            );
        }
        out.push('\n');
    }
    let failing = check.failures().count();
    let _ = writeln!(
        out,
        "disagreements: {} ({failing} over {}s)",
        check.disagreements.len(),
        check.threshold_sec
    );
    for d in &check.disagreements {
        let mark = if d.over_threshold { "FAIL" } else { "ok  " };
        let pr = if d.pr == 0 {
            "-".to_string()
        } else {
            format!("#{}", d.pr)
        };
        let seen = if d.forge_stages.len() > 1 {
            format!(" (forge seen {})", d.forge_stages.join(","))
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {mark} {} {}#{} pr={pr} host={} forge={}{seen} for {}s ({} .. {}, forge since {})",
            d.emitter,
            d.repo,
            d.issue,
            d.host_stage,
            d.forge_stage,
            d.disagree_sec,
            d.first_at,
            d.last_at,
            d.forge_since
        );
    }
    let verdict = if failing == 0 {
        "agree: no covered host disagreed with the forge past the threshold".to_string()
    } else {
        format!("DISAGREE: {failing} disagreement(s) on covered hosts past the threshold")
    };
    let _ = writeln!(out, "{verdict}");
    out
}

#[cfg(test)]
#[path = "telemetry_replay_check_tests.rs"]
mod tests;
