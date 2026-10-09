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
//! queries read), returned as `JSONEachRow`. The transports are the neutral
//! SigNoz read client in [`crate::signoz_read`] (#10196 R6):
//! [`ClickhouseHttp`](crate::signoz_read::ClickhouseHttp) (as a
//! [`SqlPages`](crate::signoz_read::SqlPages) over [`OUTCOMES_SQL`]) and
//! [`FileRows`](crate::signoz_read::FileRows) (an operator's
//! `clickhouse-client` export, and the tests' fixtures).

use super::fleet_signoz::{self, parse_row, BuildStats, ParsedRow, SignozSnapshot};
use crate::signoz_read::{PageQuery, ReadError, RowCursor, SignozRead};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::Path;

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

/// The page request for `repo`'s outcomes: the only filter [`OUTCOMES_SQL`]
/// binds is `{repo}`, which [`FileRows`](crate::signoz_read::FileRows) applies to the row's `repo` column.
#[must_use]
pub fn repo_query(
    repo: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    after: Option<RowCursor>,
    limit: u32,
) -> PageQuery {
    PageQuery {
        filters: vec![("repo".to_string(), repo.to_string())],
        since,
        until,
        after,
        limit,
    }
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
        let query = repo_query(repo, since, as_of, after.clone(), limit);
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
