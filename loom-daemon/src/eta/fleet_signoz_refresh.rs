//! Fetching and publishing the SigNoz in-sweep snapshot (#9758).
//!
//! [`super::fleet_signoz`] is the pure half (row admission, deduplication,
//! the snapshot); this module is everything with a side effect, and it never
//! runs inside an estimator: the `eta fleet signoz refresh` CLI and the
//! daemon's fleet refresh task (`observability::eta_fleet_refresh`) call it.
//!
//! # One refresh
//!
//! [`fetch`] walks the **whole** window `[as_of − RETENTION_DAYS, as_of]` by
//! keyset pages over `(observed_timestamp, record_id)` through a
//! [`SignozRead`], admits every row, and builds a sealed snapshot. There is
//! deliberately no incremental cursor: `observed_timestamp` is today a
//! producer-side value (`telemetry-replay.md`), so a late-delivered record
//! lands *behind* any cursor and an incremental walk would miss it forever.
//! A full window is a few hundred rows per repo per cycle.
//!
//! [`refresh`] publishes only a **complete** walk, atomically. An unavailable
//! backend, a failed page, an unparseable response, a cursor that does not
//! advance, or the page ceiling all stop the walk and leave the last valid
//! snapshot in service, untouched — a partial window is never published as
//! complete. A malformed *record* inside a valid response is different: it is
//! rejected, counted under its [`super::fleet_signoz::reject`] reason, and the
//! walk goes on.
//!
//! # The read contract
//!
//! [`OUTCOMES_SQL`] against the telemetry store's ClickHouse
//! (`signoz_logs.distributed_logs_v2`, the table the repo's other SigNoz
//! queries read), returned as `JSONEachRow`. Two transports carry it:
//! [`ClickhouseHttp`] (ClickHouse's HTTP interface, bound query parameters,
//! credential read from an owner-only file at call time and never logged) and
//! [`FileRows`] (the rows of an export an operator ran with `clickhouse-client`
//! — and the recorded-fixture reader the tests use).

use super::fleet_signoz::{self, parse_row, BuildStats, ParsedRow, RowCursor, SignozSnapshot};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The outcomes query: every exported `sweep.outcome` of `{repo}` knowable in
/// `[{since_ns}, {until_ns}]`, after the keyset position
/// `({after_ns}, {after_id})`, oldest knowable first.
///
/// The column names are [`fleet_signoz::parse_row`]'s input contract. Every
/// numeric attribute is read from the number map with a string-map fallback,
/// exactly as `cycle-time-extract.sql` does, because which map an integer
/// lands in is a property of the ingester.
pub const OUTCOMES_SQL: &str = "\
SELECT
    attributes_string['loom.record_id'] AS record_id,
    resources_string['host.id'] AS host_id,
    attributes_string['loom.repo'] AS repo,
    attributes_string['loom.sweep_id'] AS sweep_id,
    attributes_string['loom.result'] AS result,
    coalesce(
        if(mapContains(attributes_number, 'loom.total_duration_sec'),
           toInt64(attributes_number['loom.total_duration_sec']), NULL),
        toInt64OrNull(attributes_string['loom.total_duration_sec'])) AS total_duration_sec,
    if(mapContains(attributes_string, 'loom.phase_durations'),
       attributes_string['loom.phase_durations'], NULL) AS phase_durations,
    toString(timestamp) AS event_time_ns,
    toString(observed_timestamp) AS knowable_time_ns
FROM signoz_logs.distributed_logs_v2
WHERE (body = 'sweep.outcome' OR attributes_string['loom.kind'] = 'sweep.outcome')
  AND lower(attributes_string['loom.repo']) = lower({repo:String})
  AND observed_timestamp >= {since_ns:UInt64}
  AND observed_timestamp <= {until_ns:UInt64}
  AND (observed_timestamp, attributes_string['loom.record_id']) > ({after_ns:UInt64}, {after_id:String})
ORDER BY observed_timestamp, attributes_string['loom.record_id']
LIMIT {limit:UInt32}
FORMAT JSONEachRow
";

/// One page request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageQuery {
    /// `owner/repo`.
    pub repo: String,
    /// Oldest knowable-at, inclusive.
    pub since: DateTime<Utc>,
    /// Newest knowable-at, inclusive: the snapshot's `as_of`.
    pub until: DateTime<Utc>,
    /// Resume strictly after this `(observed_timestamp ns, record_id)`;
    /// `None` on the first page.
    pub after: Option<RowCursor>,
    /// Rows per page.
    pub limit: u32,
}

impl PageQuery {
    /// The bound query parameters, by the names [`OUTCOMES_SQL`] uses.
    #[must_use]
    pub fn params(&self) -> Vec<(&'static str, String)> {
        let (after_ns, after_id) = self.after.clone().unwrap_or((0, String::new()));
        let ns = |at: DateTime<Utc>| at.timestamp_nanos_opt().unwrap_or(0).max(0).to_string();
        vec![
            ("repo", self.repo.clone()),
            ("since_ns", ns(self.since)),
            ("until_ns", ns(self.until)),
            ("after_ns", after_ns.max(0).to_string()),
            ("after_id", after_id),
            ("limit", self.limit.to_string()),
        ]
    }
}

/// Why a page could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The backend did not answer (connection, timeout, missing credential).
    Unavailable(String),
    /// It answered with an error status.
    Refused(String),
}

/// The fetch seam: one page of [`OUTCOMES_SQL`]'s `JSONEachRow` output.
pub trait SignozRead {
    /// Read one page.
    ///
    /// # Errors
    ///
    /// The page could not be read; the walk stops and publishes nothing.
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError>;
}

/// Why a walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignozStop {
    /// Every page read; a snapshot was built.
    Complete,
    /// A page could not be read.
    Unavailable,
    /// A page was not valid `JSONEachRow`, or the cursor did not advance.
    InvalidResponse,
    /// The page ceiling was reached before the window ended.
    PageLimit,
    /// The published snapshot could not be written.
    WriteError,
}

impl SignozStop {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SignozStop::Complete => "complete",
            SignozStop::Unavailable => "unavailable",
            SignozStop::InvalidResponse => "invalid_response",
            SignozStop::PageLimit => "page_limit",
            SignozStop::WriteError => "write_error",
        }
    }
}

/// Page size and ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Rows per page.
    pub page_size: u32,
    /// Pages per walk.
    pub max_pages: u32,
}

/// What one walk did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    /// `owner/repo`.
    pub repo: String,
    /// Why it stopped.
    pub stop: SignozStop,
    /// Pages read.
    pub pages: u32,
    /// Rows read, admitted or not.
    pub rows: u64,
    /// Rejected rows per [`fleet_signoz::reject`] reason.
    pub rejected: BTreeMap<String, usize>,
    /// What canonicalisation dropped.
    pub stats: BuildStats,
    /// The failure, for the log line. Never carries a credential.
    pub detail: Option<String>,
    /// The snapshot published (or, for a dry run, built), if any.
    pub snapshot_id: Option<String>,
    /// Outcomes in that snapshot.
    pub outcomes: usize,
    /// Whether it was written.
    pub promoted: bool,
}

/// Walk the window as of `as_of` for `repo`. `Ok` only for a complete walk.
///
/// # Errors
///
/// The walk stopped early; the report says why. Nothing is built.
pub fn fetch(
    repo: &str,
    reader: &mut dyn SignozRead,
    as_of: DateTime<Utc>,
    limits: Limits,
) -> Result<(SignozSnapshot, FetchReport), Box<FetchReport>> {
    let since = SignozSnapshot::window_start(as_of);
    let mut report = FetchReport {
        repo: repo.to_string(),
        stop: SignozStop::Complete,
        pages: 0,
        rows: 0,
        rejected: BTreeMap::new(),
        stats: BuildStats::default(),
        detail: None,
        snapshot_id: None,
        outcomes: 0,
        promoted: false,
    };
    let fail = |mut report: FetchReport, stop: SignozStop, detail: String| {
        report.stop = stop;
        report.detail = Some(detail);
        Err(Box::new(report))
    };
    let limit = limits.page_size.max(1);
    let mut outcomes = Vec::new();
    let mut after: Option<RowCursor> = None;
    loop {
        if report.pages >= limits.max_pages {
            return fail(
                report,
                SignozStop::PageLimit,
                format!("window not exhausted after {} page(s)", limits.max_pages),
            );
        }
        let query = PageQuery {
            repo: repo.to_string(),
            since,
            until: as_of,
            after: after.clone(),
            limit,
        };
        let body = match reader.page(&query) {
            Ok(body) => body,
            Err(ReadError::Unavailable(why) | ReadError::Refused(why)) => {
                return fail(report, SignozStop::Unavailable, why);
            }
        };
        report.pages += 1;
        let mut rows_on_page = 0_u32;
        let page_start = after.clone();
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let (cursor, parsed) = match parse_row(line, repo) {
                Ok(row) => row,
                Err(why) => return fail(report, SignozStop::InvalidResponse, why),
            };
            // Every row is strictly after the page's start (the query's own
            // keyset) and never before its predecessor. Equal neighbours are
            // allowed: a redelivered record repeats its cursor exactly.
            if page_start.as_ref().is_some_and(|start| &cursor <= start)
                || after.as_ref().is_some_and(|prev| &cursor < prev)
            {
                return fail(
                    report,
                    SignozStop::InvalidResponse,
                    "page did not advance past the previous cursor".to_string(),
                );
            }
            after = Some(cursor);
            rows_on_page += 1;
            report.rows += 1;
            match parsed {
                ParsedRow::Outcome(outcome) => outcomes.push(outcome),
                ParsedRow::Rejected(reason) => {
                    *report.rejected.entry(reason.to_string()).or_insert(0) += 1;
                }
            }
        }
        if rows_on_page < limit {
            break;
        }
    }
    let (snapshot, stats) = SignozSnapshot::build(repo, since, as_of, outcomes);
    report.stats = stats;
    report.snapshot_id = Some(snapshot.snapshot_id.clone());
    report.outcomes = snapshot.outcomes.len();
    Ok((snapshot, report))
}

/// [`fetch`], then publish the snapshot under `root` — only when the walk was
/// complete. On any stop the previously published file is left exactly as it
/// was.
pub fn refresh(
    root: &Path,
    repo: &str,
    reader: &mut dyn SignozRead,
    as_of: DateTime<Utc>,
    limits: Limits,
) -> FetchReport {
    match fetch(repo, reader, as_of, limits) {
        Err(report) => *report,
        Ok((snapshot, mut report)) => {
            match fleet_signoz::write(&fleet_signoz::signoz_path(root, repo), &snapshot) {
                Ok(()) => report.promoted = true,
                Err(e) => {
                    report.stop = SignozStop::WriteError;
                    report.detail = Some(format!("could not write the snapshot: {e}"));
                }
            }
            report
        }
    }
}

/// One cycle over `repos` (the daemon's cadence): a refresh each, logged.
pub fn run_cycle(
    root: &Path,
    repos: &[String],
    reader: &mut dyn SignozRead,
    as_of: DateTime<Utc>,
    limits: Limits,
) -> Vec<FetchReport> {
    let mut reports = Vec::new();
    for repo in repos {
        let report = refresh(root, repo, reader, as_of, limits);
        let rejected: usize = report.rejected.values().sum();
        let line = format!(
            "eta fleet signoz: {repo}: {} (pages={} rows={} outcomes={} rejected={rejected} \
             duplicates={}/{} snapshot={})",
            report.stop.as_str(),
            report.pages,
            report.rows,
            report.outcomes,
            report.stats.duplicate_records,
            report.stats.duplicate_sweeps,
            report.snapshot_id.as_deref().unwrap_or("-"),
        );
        if report.stop == SignozStop::Complete {
            log::info!("{line}");
        } else {
            log::warn!(
                "{line}; kept the last valid snapshot: {}",
                report.detail.as_deref().unwrap_or("")
            );
        }
        let unavailable = report.stop == SignozStop::Unavailable;
        reports.push(report);
        // One unreachable backend is unreachable for every repo.
        if unavailable {
            break;
        }
    }
    reports
}

/// Rows from a `JSONEachRow` export, served page by page under the same
/// filter, order and keyset [`OUTCOMES_SQL`] applies. The import path for an
/// operator's `clickhouse-client` export, and the tests' recorded fixture.
#[derive(Debug, Clone)]
pub struct FileRows {
    rows: Vec<(RowCursor, Option<String>, String)>,
}

impl FileRows {
    /// Parse `text`. Rows the query would never return in any order (no
    /// `knowable_time_ns` / `record_id`) are a malformed export.
    ///
    /// # Errors
    ///
    /// A line is not a JSON object or lacks the paging columns.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut rows = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(line).map_err(|e| format!("line {}: not JSON: {e}", n + 1))?;
            let ns = match value.get("knowable_time_ns") {
                Some(Value::Number(v)) => v.as_i64(),
                Some(Value::String(s)) => s.trim().parse().ok(),
                _ => None,
            }
            .ok_or_else(|| format!("line {}: no integer knowable_time_ns", n + 1))?;
            let id = match value.get("record_id") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) => String::new(),
                _ => return Err(format!("line {}: no record_id column", n + 1)),
            };
            let repo = value
                .get("repo")
                .and_then(Value::as_str)
                .map(str::to_string);
            rows.push(((ns, id), repo, line.to_string()));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(FileRows { rows })
    }

    /// [`Self::parse`] the file at `path`.
    ///
    /// # Errors
    ///
    /// Unreadable, or malformed as for [`Self::parse`].
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        Self::parse(&text)
    }
}

impl SignozRead for FileRows {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        let since = query.since.timestamp_nanos_opt().unwrap_or(0);
        let until = query.until.timestamp_nanos_opt().unwrap_or(i64::MAX);
        let lines: Vec<&str> = self
            .rows
            .iter()
            .filter(|(cursor, repo, _)| {
                repo.as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&query.repo))
                    && cursor.0 >= since
                    && cursor.0 <= until
                    && query.after.as_ref().is_none_or(|after| cursor > after)
            })
            .take(query.limit as usize)
            .map(|(_, _, line)| line.as_str())
            .collect();
        Ok(lines.join("\n"))
    }
}

/// [`OUTCOMES_SQL`] over ClickHouse's HTTP interface: `POST` the query, bind
/// the parameters as `param_*`, authenticate with `X-ClickHouse-User` /
/// `X-ClickHouse-Key`.
///
/// The password is read from `credential_file` on every page and dropped
/// immediately; it never reaches a log line, an error string or a file. A
/// credential file readable by group or others is refused (unix), per the
/// owner-only rule in `credential-storage.md`.
#[derive(Debug, Clone)]
pub struct ClickhouseHttp {
    /// `http(s)://host:port`.
    pub endpoint: String,
    /// ClickHouse user.
    pub user: Option<String>,
    /// Owner-only password file.
    pub credential_file: Option<PathBuf>,
    /// Per-request timeout.
    pub timeout: std::time::Duration,
}

impl ClickhouseHttp {
    fn password(&self) -> Result<Option<String>, ReadError> {
        let Some(path) = &self.credential_file else {
            return Ok(None);
        };
        let unavailable = |why: &str| {
            ReadError::Unavailable(format!("credential file {}: {why}", path.display()))
        };
        let meta = std::fs::metadata(path).map_err(|_| unavailable("unreadable"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(unavailable("readable by group or others; chmod 600 it"));
            }
        }
        let _ = meta;
        let secret = std::fs::read_to_string(path).map_err(|_| unavailable("unreadable"))?;
        let secret = secret.trim().to_string();
        if secret.is_empty() {
            return Err(unavailable("empty"));
        }
        Ok(Some(secret))
    }

    /// The request URL: the endpoint plus every bound parameter.
    ///
    /// # Errors
    ///
    /// The endpoint is not an `http(s)` URL.
    pub fn url(&self, query: &PageQuery) -> Result<reqwest::Url, ReadError> {
        let mut url = reqwest::Url::parse(&self.endpoint)
            .map_err(|e| ReadError::Unavailable(format!("invalid endpoint: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ReadError::Unavailable("endpoint is not http(s)".to_string()));
        }
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in query.params() {
                pairs.append_pair(&format!("param_{name}"), &value);
            }
        }
        Ok(url)
    }
}

impl SignozRead for ClickhouseHttp {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        self.post(OUTCOMES_SQL, query)
    }
}

impl ClickhouseHttp {
    /// `POST` `sql` with `query`'s parameters bound; the response body.
    /// [`OUTCOMES_SQL`] here, and the timeline reader's own query
    /// (`fleet_signoz_timeline_rows::TIMELINE_SQL`, #10519).
    ///
    /// # Errors
    ///
    /// The backend is unreachable or answered with an error status.
    pub fn post(&self, sql: &'static str, query: &PageQuery) -> Result<String, ReadError> {
        let url = self.url(query)?;
        let password = self.password()?;
        let user = self.user.clone();
        let timeout = self.timeout;
        // A private current-thread runtime on its own thread: callers are
        // synchronous (the CLI, or the refresh task's `spawn_blocking`), and
        // this must not depend on — or block — whichever runtime they are in.
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| ReadError::Unavailable(format!("runtime: {e}")))?;
                    runtime.block_on(async move {
                        let client = reqwest::Client::builder()
                            .timeout(timeout)
                            .build()
                            .map_err(|e| ReadError::Unavailable(format!("client: {e}")))?;
                        let mut request = client.post(url).body(sql);
                        if let Some(user) = user {
                            request = request.header("X-ClickHouse-User", user);
                        }
                        if let Some(password) = password {
                            request = request.header("X-ClickHouse-Key", password);
                        }
                        let response = request.send().await.map_err(|e| {
                            ReadError::Unavailable(format!("request failed: {}", e.without_url()))
                        })?;
                        let status = response.status();
                        let body = response.text().await.map_err(|e| {
                            ReadError::Unavailable(format!("body: {}", e.without_url()))
                        })?;
                        if !status.is_success() {
                            let head: String = body.chars().take(200).collect();
                            return Err(ReadError::Refused(format!("HTTP {status}: {head}")));
                        }
                        Ok(body)
                    })
                })
                .join()
                .unwrap_or_else(|_| Err(ReadError::Unavailable("reader thread panicked".into())))
        })
    }
}
