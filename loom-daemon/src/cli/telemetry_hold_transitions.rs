//! `loom-daemon telemetry-hold-transitions` (Issue #14094): one read-only
//! query answering, for a date range across repos, "how many Champion
//! merge-risk self-releases happened, and with what confidence?" — the
//! question the #13317 audit could not answer from any Builder seat,
//! because `hold.transition` was OTLP-write-only and the forge markers are
//! unsearchable HTML comments.
//!
//! One bound SQL statement over `hold.transition` log records through the
//! telemetry-reader ([`crate::signoz_read`]), modeled on
//! [`super::telemetry_join`]. The record rides whole in the log `body` (the
//! mapping writes the record JSON there), so every field is parsed in Rust
//! from the body and the SQL depends only on the two map reads every other
//! reader query already proves (`loom.kind`, `loom.repo`) plus `timestamp`.
//! Success prints per-repo counts with a self-release confidence breakout,
//! as JSON (`--json`) or text; every failure exits non-zero — unset
//! endpoint, unreachable reader, malformed rows, a row-capped window — and
//! never an empty-success result.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Serialize;

use crate::signoz_read::{EndpointConfig, TelemetryReader};
use crate::telemetry::kinds::hold_transition::{reason, HoldTransitionRecord};

/// The SigNoz log table the reader's store account serves; the same table
/// every shipped reader query reads (`host-export-queries.sql`, PROBE_QUERY).
const LOGS_TABLE: &str = "signoz_logs.distributed_logs_v2";
/// The record kind, as stamped on `loom.kind` (#10899).
const LOG_KIND: &str = "hold.transition";
/// Default row cap: holds are rare, so a fleet-quarter window fits; the cap
/// exists so a read can never be unbounded.
const DEFAULT_MAX_ROWS: usize = 5_000;
/// Hard ceiling on `--max-rows`.
const MAX_MAX_ROWS: usize = 100_000;

/// One fetched record plus its store-side timestamp.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    /// The parsed `hold.transition` record (the log body).
    pub record: HoldTransitionRecord,
    /// The store's `timestamp`, as unix nanoseconds.
    pub ts_ns: i64,
}

/// Confidence distribution over the self-release records of one scope.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct ConfidenceStats {
    /// Self-release records in the scope.
    pub count: usize,
    /// Confidence values present (`null` confidence is reported as absent,
    /// never as zero).
    pub values: Vec<u8>,
    pub min: Option<u8>,
    /// Middle value, or the mean of the two middle values on an even count.
    pub median: Option<f64>,
    pub max: Option<u8>,
}

/// Per-repo rollup.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct RepoRollup {
    /// All matching records for the repo.
    pub records: usize,
    /// Count per `reason_code`.
    pub by_reason_code: BTreeMap<String, usize>,
    /// The self-release breakout.
    pub self_release: ConfidenceStats,
}

/// The printed outcome.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct Outcome {
    /// The resolved window, RFC 3339 (`until` exclusive).
    pub since: String,
    pub until: String,
    /// The repo filter as passed (empty: no filter).
    pub repos: Vec<String>,
    /// The reason-code filter as passed (`null`: no filter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// Parsed records in the window.
    pub total_records: usize,
    /// Rows whose body did not decode as a `hold.transition` record; always
    /// printed, never folded into the counts.
    pub unparseable_records: usize,
    /// Per-repo rollups, keyed by `owner/repo`.
    pub by_repo: BTreeMap<String, RepoRollup>,
    /// Fleet-wide self-release breakout.
    pub overall_self_release: ConfidenceStats,
}

/// Parse `--since`/`--until`: RFC 3339, or a bare `YYYY-MM-DD` (UTC
/// midnight). A date-only `until` means *through* that UTC day, so it is
/// widened to the next midnight as the exclusive bound.
fn parse_bound(value: &str, date_only_until: bool) -> Result<DateTime<Utc>> {
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(at.with_timezone(&Utc));
    }
    let day = NaiveDate::parse_from_str(value, "%Y-%m-%d").with_context(|| {
        format!("--since/--until must be RFC 3339 or YYYY-MM-DD, got {value:?}")
    })?;
    let midnight = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight"));
    if date_only_until {
        return Ok(midnight + chrono::Duration::days(1));
    }
    Ok(midnight)
}

/// Build the one statement and its bound string params. `since_ns`/`until_ns`
/// and the row cap are generated integers, validated before they reach this
/// function, so they are inlined; the user-text filters (`--repo`,
/// `--reason-code`) are bound, never interpolated.
pub(crate) fn build_query(
    since_ns: i64,
    until_ns: i64,
    repos: &[String],
    reason_code: Option<&str>,
    max_rows: usize,
) -> (String, Vec<(String, String)>) {
    let mut sql = format!(
        "SELECT toUnixTimestamp64Nano(timestamp) AS ts_ns, body \
         FROM {LOGS_TABLE} \
         WHERE attributes_string['loom.kind'] = '{LOG_KIND}' \
           AND timestamp >= fromUnixTimestamp64Nano({since_ns}) \
           AND timestamp < fromUnixTimestamp64Nano({until_ns})"
    );
    let mut params = Vec::new();
    if !repos.is_empty() {
        let clauses: Vec<String> = (0..repos.len())
            .map(|i| format!("attributes_string['loom.repo'] = {{repo{i}:String}}"))
            .collect();
        sql.push_str(&format!(" AND ({})", clauses.join(" OR ")));
        for (i, repo) in repos.iter().enumerate() {
            params.push((format!("repo{i}"), repo.clone()));
        }
    }
    if let Some(code) = reason_code {
        sql.push_str(" AND attributes_string['loom.hold.reason_code'] = {reason_code:String}");
        params.push(("reason_code".to_string(), code.to_string()));
    }
    // One extra row: a full cap+1 fetch proves truncation (the caller
    // refuses rather than printing a capped total).
    sql.push_str(&format!(" ORDER BY ts_ns ASC LIMIT {}", max_rows + 1));
    (sql, params)
}

/// Parse one `JSONEachRow` line into a [`Row`].
fn parse_row(line: &str) -> std::result::Result<Row, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("malformed row: {e}"))?;
    let ts_ns = value
        .get("ts_ns")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| "row has no integer ts_ns".to_string())?;
    let body = value
        .get("body")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "row has no body string".to_string())?;
    let record: HoldTransitionRecord = serde_json::from_str(body)
        .map_err(|e| format!("body is not a hold.transition record: {e}"))?;
    Ok(Row { record, ts_ns })
}

/// Confidence stats over one bucket: `count` is the number of *records*
/// (a null confidence still counts), `values` only the confidences present.
fn confidence_stats(count: usize, values: impl Iterator<Item = Option<u8>>) -> ConfidenceStats {
    let mut values: Vec<u8> = values.flatten().collect();
    values.sort_unstable();
    if count == 0 {
        return ConfidenceStats::default();
    }
    let min = values.first().copied();
    let max = values.last().copied();
    let median = if values.is_empty() {
        None
    } else if values.len() % 2 == 1 {
        Some(f64::from(values[values.len() / 2]))
    } else {
        let lo = f64::from(values[values.len() / 2 - 1]);
        let hi = f64::from(values[values.len() / 2]);
        Some((lo + hi) / 2.0)
    };
    ConfidenceStats {
        count,
        values,
        min,
        median,
        max,
    }
}

/// Roll the parsed rows into the printed outcome. `unparseable` lines were
/// counted by the caller and only travel here.
pub(crate) fn aggregate(rows: &[Row], unparseable: usize, outcome: &mut Outcome) {
    #[derive(Default)]
    struct Bucket {
        records: usize,
        by_reason: BTreeMap<String, usize>,
        self_release_records: usize,
        self_release_values: Vec<u8>,
    }
    let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut overall_records = 0usize;
    let mut overall_values: Vec<u8> = Vec::new();
    for Row { record, .. } in rows {
        let bucket = buckets.entry(record.repo.clone()).or_default();
        bucket.records += 1;
        *bucket
            .by_reason
            .entry(record.reason_code.clone())
            .or_default() += 1;
        if record.reason_code == reason::MERGE_RISK_SELF_RELEASE {
            bucket.self_release_records += 1;
            bucket.self_release_values.extend(record.confidence_pct);
            overall_records += 1;
            overall_values.extend(record.confidence_pct);
        }
    }
    outcome.by_repo = buckets
        .into_iter()
        .map(|(repo, bucket)| {
            (
                repo,
                RepoRollup {
                    records: bucket.records,
                    by_reason_code: bucket.by_reason,
                    self_release: confidence_stats(
                        bucket.self_release_records,
                        bucket.self_release_values.into_iter().map(Some),
                    ),
                },
            )
        })
        .collect();
    outcome.total_records = rows.len();
    outcome.unparseable_records = unparseable;
    outcome.overall_self_release =
        confidence_stats(overall_records, overall_values.into_iter().map(Some));
}

#[derive(clap::Args)]
pub(crate) struct HoldTransitionsArgs {
    /// Window start: RFC 3339 or `YYYY-MM-DD` (UTC midnight).
    #[arg(long, value_name = "WHEN")]
    since: String,

    /// Window end, exclusive: RFC 3339, or `YYYY-MM-DD` meaning *through*
    /// that UTC day.
    #[arg(long, value_name = "WHEN")]
    until: String,

    /// Restrict to `owner/repo` (lowercased in the store); repeatable.
    #[arg(long = "repo", value_name = "OWNER/REPO")]
    repos: Vec<String>,

    /// Restrict to one `reason_code` (e.g. `merge-risk-self-release`);
    /// default reports every reason with the self-release breakout.
    #[arg(long, value_name = "CODE")]
    reason_code: Option<String>,

    /// Row cap on the read; a capped window is refused, never printed
    /// capped. Raise it (hard ceiling 100000) or narrow the window.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_ROWS)]
    max_rows: usize,

    /// Print one JSON document instead of text.
    #[arg(long)]
    json: bool,

    /// The telemetry-reader's base URL (`http(s)://host[:port]`). Defaults
    /// to `TELEMETRY_READER_URL`, then `telemetry.signoz.endpoint`.
    #[arg(long, value_name = "URL")]
    reader_url: Option<String>,

    /// Per-request timeout in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    timeout: u64,
}

/// Endpoint resolution with the fail-closed message the AC names: no
/// `--reader-url`, no `TELEMETRY_READER_URL`, no config -> refuse, naming
/// the variable.
pub(crate) fn resolve_endpoint(
    override_url: Option<&str>,
    env: Option<&str>,
    config: &serde_json::Value,
) -> Result<String> {
    let configured = EndpointConfig::resolve(config, env);
    let endpoint = override_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or(configured.endpoint);
    endpoint.ok_or_else(|| {
        anyhow::anyhow!(
            "no telemetry-reader: pass --reader-url, set TELEMETRY_READER_URL, or set \
             telemetry.signoz.endpoint"
        )
    })
}

impl HoldTransitionsArgs {
    /// Run the read and print the rollup.
    ///
    /// # Errors
    ///
    /// Bad window bounds, an unknown repo shape, a missing reader endpoint,
    /// an unreachable or refusing reader, malformed rows, or a row-capped
    /// window — every failure path exits non-zero.
    pub(crate) fn run(self) -> Result<()> {
        let since = parse_bound(&self.since, false)?;
        let until = parse_bound(&self.until, true)?;
        if until <= since {
            bail!("--until must be after --since (got {since} .. {until})");
        }
        let since_ns = since
            .timestamp_nanos_opt()
            .context("window start is outside the representable range")?;
        let until_ns = until
            .timestamp_nanos_opt()
            .context("window end is outside the representable range")?;
        if self.max_rows == 0 || self.max_rows > MAX_MAX_ROWS {
            bail!("--max-rows must be between 1 and {MAX_MAX_ROWS}");
        }
        let mut repos = Vec::new();
        for repo in &self.repos {
            let repo = repo.trim().to_lowercase();
            if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
                bail!("--repo expects owner/repo, got {repo:?}");
            }
            repos.push(repo);
        }
        let reason_code = self
            .reason_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let env = std::env::var(crate::signoz_read::ENDPOINT_ENV).ok();
        let config = crate::config_resolver::resolve_effective_config(Path::new("."));
        let endpoint = resolve_endpoint(self.reader_url.as_deref(), env.as_deref(), &config)?;
        let reader = TelemetryReader {
            endpoint,
            timeout: Duration::from_secs(self.timeout),
        };
        let (sql, params) =
            build_query(since_ns, until_ns, &repos, reason_code.as_deref(), self.max_rows);
        let body = reader
            .post_with(&sql, &params)
            .map_err(|e| anyhow::anyhow!("telemetry-reader query failed: {e:?}"))?;

        let mut rows = Vec::new();
        let mut unparseable = 0usize;
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            match parse_row(line) {
                Ok(row) => rows.push(row),
                Err(_why) => unparseable += 1,
            }
        }
        if rows.len() > self.max_rows {
            bail!(
                "window holds more than {} records; refusing to print a capped count — \
                 raise --max-rows (up to {MAX_MAX_ROWS}) or narrow the window/repo filter",
                self.max_rows
            );
        }

        let mut outcome = Outcome {
            since: since.to_rfc3339(),
            until: until.to_rfc3339(),
            repos: self.repos.clone(),
            reason_code,
            ..Outcome::default()
        };
        aggregate(&rows, unparseable, &mut outcome);
        if self.json {
            println!("{}", serde_json::to_string(&outcome)?);
        } else {
            print_text(&outcome);
        }
        Ok(())
    }
}

/// The default human-readable rendering.
fn print_text(outcome: &Outcome) {
    println!(
        "hold.transition records {} .. {} (until exclusive): {} total, {} unparseable",
        outcome.since, outcome.until, outcome.total_records, outcome.unparseable_records
    );
    for (repo, roll) in &outcome.by_repo {
        let reasons: Vec<String> = roll
            .by_reason_code
            .iter()
            .map(|(code, n)| format!("{code}: {n}"))
            .collect();
        println!("  {repo}: {} records [{}]", roll.records, reasons.join(", "));
        let s = &roll.self_release;
        println!(
            "       self-release: {} records; confidence_pct min {:?} / median {:?} / max {:?} (values: {})",
            s.count,
            s.min,
            s.median.map(|m| format!("{m:.1}")),
            s.max,
            if s.values.is_empty() {
                "none".to_string()
            } else {
                s.values
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }
    let s = &outcome.overall_self_release;
    println!(
        "overall self-release: {} records; confidence_pct min {:?} / median {:?} / max {:?}",
        s.count, s.min, s.median, s.max
    );
    if outcome.total_records == 0 && outcome.unparseable_records == 0 {
        println!("(no hold.transition records in the window)");
    }
}

#[cfg(test)]
#[path = "telemetry_hold_transitions_tests.rs"]
mod tests;
