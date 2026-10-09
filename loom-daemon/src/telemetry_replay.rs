//! Fleet state at an instant `t`, as a daemon running at `t` could have known
//! it (#10196 R6, `loom-daemon telemetry-replay --as-of <t>`).
//!
//! This module owns no reconstruction logic of its own. The reconstruction is
//! R3's committed replay SQL (`defaults/observability/signoz/replay-queries.sql`,
//! #11125), included verbatim: query 1 (fleet state at `t`) and query 3
//! (coverage at `t`), each with the shared replay prefix exactly as committed
//! between its markers. A change to that file changes what this command runs;
//! nothing here re-types it. See `defaults/docs/telemetry-replay.md`.
//!
//! What this module adds is the reader's half: binding the parameters,
//! parsing the `JSONEachRow` results, and assembling one [`Replay`] in which a
//! host whose state is not reconstructable at `t` is reported as **unknown**
//! (with the reason the SQL gave), never as an empty host.
//!
//! Rules it keeps (#10196): no row caps (every row the queries return is
//! reported), every host's own view is reported (nothing is elected; the
//! per-item merge is the SQL's own documented rule), and no estimate or
//! statistic is computed — this is a reconstruction of exported facts only.
//! Nothing here may depend on the `eta` module (#11098).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::signoz_read::{ClickhouseHttp, ReadError};

/// R3's committed replay SQL, verbatim.
pub const REPLAY_QUERIES: &str =
    include_str!("../../defaults/observability/signoz/replay-queries.sql");

const PREFIX_BEGIN: &str = "-- >>> replay-prefix";
const PREFIX_END: &str = "-- <<< replay-prefix";

/// The documented statement count: query 0, 1-3, 4a-4c and 5.
const STATEMENTS: usize = 8;

/// Default lookback for the base anchor and its chain, seconds: one anchor
/// interval plus one pass, as `replay-queries.sql` documents.
pub const DEFAULT_WINDOW_SEC: u32 = 3900;

fn strip_comments(sql: &str) -> String {
    sql.lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Queries 1 (state) and 3 (coverage) as a reader runs them: query 1 already
/// begins with the shared prefix; query 3 has it prepended.
///
/// # Errors
///
/// `sql` no longer has the documented shape (markers, statement count, or
/// query 1 not starting with the prefix): the file changed under this reader.
pub fn state_and_coverage_queries(sql: &str) -> Result<(String, String), String> {
    let begin = sql
        .find(PREFIX_BEGIN)
        .ok_or("replay-queries.sql: no replay-prefix begin marker")?
        + PREFIX_BEGIN.len();
    let end = sql
        .find(PREFIX_END)
        .ok_or("replay-queries.sql: no replay-prefix end marker")?;
    if end < begin {
        return Err("replay-queries.sql: replay-prefix markers out of order".to_string());
    }
    let prefix = strip_comments(&sql[begin..end]).trim().to_string();
    let statements: Vec<String> = strip_comments(sql)
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if statements.len() != STATEMENTS {
        return Err(format!(
            "replay-queries.sql: expected {STATEMENTS} statements (0, 1-3, 4a-4c, 5), found {}",
            statements.len()
        ));
    }
    if !statements[1].starts_with(&prefix) {
        return Err("replay-queries.sql: query 1 does not begin with the replay prefix".into());
    }
    Ok((statements[1].clone(), format!("{prefix}\n{}", statements[3])))
}

/// The bound parameters of queries 1-3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayParams {
    /// The replay instant.
    pub as_of: DateTime<Utc>,
    /// Lookback for the base anchor and its chain, seconds.
    pub window_sec: u32,
    /// `owner/name` to scope to one repository; empty for all.
    pub repo: String,
}

impl ReplayParams {
    /// As ClickHouse query parameters (`t` is a `DateTime64(3)` in UTC).
    #[must_use]
    pub fn params(&self) -> Vec<(String, String)> {
        vec![
            ("t".to_string(), self.as_of.format("%Y-%m-%d %H:%M:%S%.3f").to_string()),
            ("window".to_string(), self.window_sec.to_string()),
            ("repo".to_string(), self.repo.clone()),
        ]
    }
}

/// One item of the reconstructed fleet state (query 1's row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateRow {
    pub repo: String,
    pub issue: u64,
    pub stage: String,
    /// The host whose sweep holds the item; empty when no complete host named one.
    pub host: String,
    pub pr: u64,
    pub entered_at: String,
    /// Complete hosts whose own view includes this item.
    pub reporting_hosts: u64,
}

/// One emitting host's coverage at `t` (query 3's row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCoverage {
    pub emitter: String,
    /// The SQL's state: `complete`, or why the host is unknown
    /// (`missing_anchor`, `incomplete_anchor`, `incomplete_delta`,
    /// `broken_chain`, `no_anchor`).
    pub state: String,
    pub covered: bool,
    pub anchor_as_of: Option<String>,
    pub last_as_of: Option<String>,
    pub anchor_age_sec: Option<i64>,
}

impl HostCoverage {
    /// `covered`, or `unknown` — never "empty": silence from an uncovered
    /// host says nothing about its work.
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        if self.covered {
            "covered"
        } else {
            "unknown"
        }
    }
}

/// Fleet state at `as_of`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Replay {
    pub as_of: DateTime<Utc>,
    pub window_sec: u32,
    /// Empty for all repositories.
    pub repo: String,
    pub hosts: Vec<HostCoverage>,
    pub state: Vec<StateRow>,
}

fn field<'a>(row: &'a Value, key: &str) -> Result<&'a Value, String> {
    row.get(key)
        .ok_or_else(|| format!("row lacks column {key}: {row}"))
}

fn text(row: &Value, key: &str) -> Result<String, String> {
    match field(row, key)? {
        Value::String(s) => Ok(s.clone()),
        other => Err(format!("column {key} is not a string: {other}")),
    }
}

fn opt_text(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_string)
}

/// ClickHouse may quote 64-bit integers in `JSONEachRow`; accept either.
fn int(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn uint(row: &Value, key: &str) -> Result<u64, String> {
    let value = field(row, key)?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .ok_or_else(|| format!("column {key} is not an unsigned integer: {value}"))
}

fn flag(row: &Value, key: &str) -> Result<bool, String> {
    match field(row, key)? {
        Value::Bool(b) => Ok(*b),
        other => int(other)
            .map(|n| n != 0)
            .ok_or_else(|| format!("column {key} is not a boolean: {other}")),
    }
}

fn state_row(row: &Value) -> Result<StateRow, String> {
    Ok(StateRow {
        repo: text(row, "repo")?,
        issue: uint(row, "issue")?,
        stage: text(row, "stage")?,
        host: text(row, "host")?,
        pr: uint(row, "pr")?,
        entered_at: text(row, "entered_at")?,
        reporting_hosts: uint(row, "reporting_hosts")?,
    })
}

fn coverage_row(row: &Value) -> Result<HostCoverage, String> {
    Ok(HostCoverage {
        emitter: text(row, "emitter")?,
        state: text(row, "state")?,
        covered: flag(row, "covered")?,
        anchor_as_of: opt_text(row, "anchor_as_of"),
        last_as_of: opt_text(row, "last_as_of"),
        anchor_age_sec: row.get("anchor_age_sec").and_then(int),
    })
}

fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(n, l)| (n + 1, l))
}

fn json(n: usize, line: &str) -> Result<Value, String> {
    serde_json::from_str(line).map_err(|e| format!("line {n}: not JSON: {e}"))
}

/// Assemble a [`Replay`] from query 1's and query 3's `JSONEachRow` output.
///
/// # Errors
///
/// A line is not JSON or lacks a column the committed query selects.
pub fn assemble(params: &ReplayParams, state: &str, coverage: &str) -> Result<Replay, String> {
    let state = lines(state)
        .map(|(n, l)| json(n, l).and_then(|v| state_row(&v)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("state: {e}"))?;
    let hosts = lines(coverage)
        .map(|(n, l)| json(n, l).and_then(|v| coverage_row(&v)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("coverage: {e}"))?;
    Ok(Replay {
        as_of: params.as_of,
        window_sec: params.window_sec,
        repo: params.repo.clone(),
        hosts,
        state,
    })
}

/// [`assemble`] from one export holding both queries' rows, as an operator
/// gets by running the two printed queries with `clickhouse-client --format
/// JSONEachRow` into one file: a row with an `emitter` column is coverage,
/// any other row is state.
///
/// # Errors
///
/// As for [`assemble`].
pub fn assemble_export(params: &ReplayParams, export: &str) -> Result<Replay, String> {
    let (mut state, mut coverage) = (String::new(), String::new());
    for (n, line) in lines(export) {
        let target = if json(n, line)?.get("emitter").is_some() {
            &mut coverage
        } else {
            &mut state
        };
        target.push_str(line);
        target.push('\n');
    }
    assemble(params, &state, &coverage)
}

/// Run queries 1 and 3 against the live store and assemble the result.
///
/// # Errors
///
/// The committed SQL has an unexpected shape, the store could not be read, or
/// it returned rows this reader cannot parse.
pub fn fetch(http: &ClickhouseHttp, params: &ReplayParams) -> Result<Replay, String> {
    let (state_sql, coverage_sql) = state_and_coverage_queries(REPLAY_QUERIES)?;
    let bound = params.params();
    let post = |sql: &str, which: &str| {
        http.post_with(&format!("{sql}\nFORMAT JSONEachRow"), &bound)
            .map_err(|e| match e {
                ReadError::Unavailable(why) => format!("{which} query: store unavailable: {why}"),
                ReadError::Refused(why) => format!("{which} query: store refused: {why}"),
            })
    };
    let state = post(&state_sql, "state")?;
    let coverage = post(&coverage_sql, "coverage")?;
    assemble(params, &state, &coverage)
}

/// Human-readable report: every host with its verdict, then every item.
#[must_use]
pub fn render(replay: &Replay) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let scope = if replay.repo.is_empty() {
        "all repos"
    } else {
        replay.repo.as_str()
    };
    let _ = writeln!(
        out,
        "fleet state at {} ({scope}, window {}s)",
        replay.as_of.to_rfc3339(),
        replay.window_sec
    );
    let covered = replay.hosts.iter().filter(|h| h.covered).count();
    let _ = writeln!(out, "hosts: {} ({covered} covered)", replay.hosts.len());
    for h in &replay.hosts {
        let _ = write!(out, "  {:<24} {:<8} {}", h.emitter, h.verdict(), h.state);
        if let Some(anchor) = &h.anchor_as_of {
            let _ = write!(out, " anchor={anchor}");
        }
        if let Some(last) = &h.last_as_of {
            let _ = write!(out, " last={last}");
        }
        out.push('\n');
    }
    let _ = writeln!(out, "items: {}", replay.state.len());
    for r in &replay.state {
        let holder = if r.host.is_empty() {
            "-"
        } else {
            r.host.as_str()
        };
        let pr = if r.pr == 0 {
            "-".to_string()
        } else {
            format!("#{}", r.pr)
        };
        let _ = writeln!(
            out,
            "  {}#{:<6} {:<16} holder={holder} pr={pr} entered={} hosts={}",
            r.repo, r.issue, r.stage, r.entered_at, r.reporting_hosts
        );
    }
    out
}

#[cfg(test)]
#[path = "telemetry_replay_tests.rs"]
mod tests;
