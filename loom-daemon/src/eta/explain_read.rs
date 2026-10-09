//! Reading an *emitted* `eta.estimate` explanation back (#10930, slice 2).
//!
//! `eta explain` replays an explanation; this module finds the one to replay.
//! Every `eta.estimate` log record's body is the full `eta-explanation/v1`
//! record, and its attributes carry `loom.eta.estimate_id`, `loom.repo`,
//! `loom.issue`, `loom.eta.kind` and `loom.eta.heuristic` (`mapping/eta.rs`).
//! A [`Selector`] names the estimate by id, or the subject
//! (`repo#issue` plus optional kind / heuristic) and an optional `--at`.
//!
//! # `--at` is a lookup, not a recompute
//!
//! `at` returns the newest estimate that was **emitted** with
//! `as_of <= at`. It does not rebuild the history as of `at` and re-run the
//! heuristic: that needs walk-forward fits the local machine cannot
//! reproduce. If no estimate was emitted by `at` (the authority was not
//! running, the item did not exist yet), the answer is "none", never a
//! fabricated one.
//!
//! # Transports
//!
//! The same [`SignozRead`] seam as the in-sweep snapshot: [`ExplainHttp`]
//! (the shared [`ClickhouseHttp`] with [`ESTIMATE_SQL`]'s extra bound
//! parameters) and [`FileRows`] (an operator's `clickhouse-client` export of
//! [`ESTIMATE_SQL`], and the tests' recorded fixture). A row's body is
//! re-parsed and re-filtered here, so a transport that ignores a selector
//! (the file reader) still gives the right answer.

use super::explanation::Explanation;
use super::fleet_signoz_refresh::{ClickhouseHttp, FileRows, PageQuery, ReadError, SignozRead};
use super::Kind;
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Rows per page.
pub const PAGE_LIMIT: u32 = 200;

/// Pages before the read refuses rather than answer from a partial window.
pub const MAX_PAGES: u32 = 10;

/// The estimates query. Every selector is optional: an empty string (or `0`
/// for `issue`) matches anything. Keyset-paged over
/// `(observed_timestamp, record_id)` like `OUTCOMES_SQL`; `at_ns` bounds the
/// record's *event* time (`timestamp`, the estimate's `as_of`).
pub const ESTIMATE_SQL: &str = "\
SELECT
    if(attributes_string['loom.record_id'] != '', attributes_string['loom.record_id'],
       concat('h:', toString(cityHash64(body, timestamp)))) AS record_id,
    attributes_string['loom.repo'] AS repo,
    attributes_string['loom.eta.estimate_id'] AS estimate_id,
    body,
    toString(timestamp) AS event_time_ns,
    toString(observed_timestamp) AS knowable_time_ns
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.eta.estimate_id'] != ''
  AND mapContains(attributes_bool, 'loom.eta.primary')
  AND ({estimate_id:String} = '' OR attributes_string['loom.eta.estimate_id'] = {estimate_id:String})
  AND ({repo:String} = '' OR lower(attributes_string['loom.repo']) = lower({repo:String}))
  AND ({issue:UInt32} = 0 OR toUInt32(attributes_number['loom.issue']) = {issue:UInt32})
  AND ({kind:String} = '' OR attributes_string['loom.eta.kind'] = {kind:String})
  AND ({heuristic:String} = '' OR attributes_string['loom.eta.heuristic'] = {heuristic:String})
  AND timestamp <= {at_ns:UInt64}
  AND observed_timestamp >= {since_ns:UInt64}
  AND observed_timestamp <= {until_ns:UInt64}
  AND (observed_timestamp, record_id) > ({after_ns:UInt64}, {after_id:String})
ORDER BY observed_timestamp, record_id
LIMIT {limit:UInt32}
FORMAT JSONEachRow
";

/// Which emitted estimate(s) to read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    /// One estimate by `loom.eta.estimate_id`.
    pub estimate_id: Option<String>,
    /// `owner/repo` of the subject.
    pub repo: Option<String>,
    /// The subject's issue number.
    pub issue: Option<u32>,
    /// Only this kind.
    pub kind: Option<Kind>,
    /// Only this heuristic.
    pub heuristic: Option<String>,
    /// Only estimates with `as_of <= at`.
    pub at: Option<DateTime<Utc>>,
}

impl Selector {
    /// The extra bound parameters [`ESTIMATE_SQL`] names.
    #[must_use]
    pub fn params(&self) -> Vec<(&'static str, String)> {
        vec![
            ("estimate_id", self.estimate_id.clone().unwrap_or_default()),
            ("issue", self.issue.unwrap_or(0).to_string()),
            (
                "kind",
                self.kind
                    .map(|k| k.as_str().to_string())
                    .unwrap_or_default(),
            ),
            ("heuristic", self.heuristic.clone().unwrap_or_default()),
            (
                "at_ns",
                self.at
                    .and_then(|t| t.timestamp_nanos_opt())
                    .map_or(u64::MAX, |n| n.max(0) as u64)
                    .to_string(),
            ),
        ]
    }

    /// Whether `e` is one this selector asks for.
    #[must_use]
    pub fn matches(&self, e: &Explanation) -> bool {
        self.estimate_id
            .as_deref()
            .is_none_or(|id| e.estimate_id == id)
            && self
                .repo
                .as_deref()
                .is_none_or(|r| e.subject.repo.eq_ignore_ascii_case(r))
            && self.issue.is_none_or(|n| e.subject.issue == n)
            && self.kind.is_none_or(|k| e.kind == k)
            && self.heuristic.as_deref().is_none_or(|h| e.heuristic == h)
            && self.at.is_none_or(|t| e.as_of <= t)
    }
}

/// [`ESTIMATE_SQL`] over ClickHouse's HTTP interface: the shared transport,
/// credential handling and parameter binding, plus the selector.
#[derive(Debug, Clone)]
pub struct ExplainHttp {
    /// The shared transport.
    pub http: ClickhouseHttp,
    /// What to select.
    pub selector: Selector,
}

impl SignozRead for ExplainHttp {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        self.http
            .post_with(ESTIMATE_SQL, query, &self.selector.params())
    }
}

/// Why the read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainReadError {
    /// A page could not be read.
    Read(ReadError),
    /// A page was not valid `JSONEachRow`, or the cursor did not advance.
    Malformed(String),
    /// More than [`MAX_PAGES`] pages: the answer would come from a partial
    /// window, so none is given. Narrow the selector or the lookback.
    TooManyRows,
}

impl std::fmt::Display for ExplainReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExplainReadError::Read(ReadError::Unavailable(m)) => {
                write!(f, "SigNoz unavailable: {m}")
            }
            ExplainReadError::Read(ReadError::Refused(m)) => write!(f, "SigNoz refused: {m}"),
            ExplainReadError::Malformed(m) => write!(f, "malformed SigNoz response: {m}"),
            ExplainReadError::TooManyRows => write!(
                f,
                "more than {} estimates match; narrow the selection (--kind, --heuristic, \
                 --lookback-days)",
                PAGE_LIMIT * MAX_PAGES
            ),
        }
    }
}

/// Every emitted explanation `selector` matches within `[since, until]`
/// (knowable-at), oldest first, de-duplicated by `estimate_id`.
///
/// # Errors
///
/// See [`ExplainReadError`].
pub fn read(
    reader: &mut dyn SignozRead,
    selector: &Selector,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<Explanation>, ExplainReadError> {
    let mut out: Vec<Explanation> = Vec::new();
    let mut after = None;
    for _ in 0..MAX_PAGES {
        let query = PageQuery {
            repo: selector.repo.clone().unwrap_or_default(),
            since,
            until,
            after: after.clone(),
            limit: PAGE_LIMIT,
        };
        let text = reader.page(&query).map_err(ExplainReadError::Read)?;
        let mut rows = 0u32;
        let mut last = after.clone();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let row: Value = serde_json::from_str(line)
                .map_err(|e| ExplainReadError::Malformed(format!("not JSON: {e}")))?;
            rows += 1;
            let ns = match row.get("knowable_time_ns") {
                Some(Value::Number(v)) => v.as_i64(),
                Some(Value::String(s)) => s.trim().parse().ok(),
                _ => None,
            }
            .ok_or_else(|| ExplainReadError::Malformed("no knowable_time_ns".into()))?;
            let id = row
                .get("record_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            last = Some((ns, id));
            let Some(body) = row.get("body").and_then(Value::as_str) else {
                continue;
            };
            // A row that is not an explanation (an `eta.outcome` sharing the
            // estimate id attribute, a truncated body) is not an answer.
            let Ok(e) = serde_json::from_str::<Explanation>(body) else {
                continue;
            };
            if selector.matches(&e) && !out.iter().any(|o| o.estimate_id == e.estimate_id) {
                out.push(e);
            }
        }
        if rows < PAGE_LIMIT {
            out.sort_by_key(|e| e.as_of);
            return Ok(out);
        }
        if last == after {
            return Err(ExplainReadError::Malformed("cursor did not advance".into()));
        }
        after = last;
    }
    Err(ExplainReadError::TooManyRows)
}

/// The estimate(s) to explain out of `emitted`: the one named by id, else the
/// newest per kind (the newest emitted with `as_of <= at`).
#[must_use]
pub fn newest_per_kind(mut emitted: Vec<Explanation>) -> Vec<Explanation> {
    emitted.sort_by(|a, b| {
        b.as_of
            .cmp(&a.as_of)
            .then_with(|| a.estimate_id.cmp(&b.estimate_id))
    });
    let mut out: Vec<Explanation> = Vec::new();
    for e in emitted {
        if !out.iter().any(|o| o.kind == e.kind) {
            out.push(e);
        }
    }
    out.sort_by_key(|e| e.kind);
    out
}

/// [`FileRows`] over an operator's export, for a caller that holds the path.
///
/// # Errors
///
/// Unreadable or malformed export.
pub fn file_reader(path: &std::path::Path) -> Result<FileRows, String> {
    FileRows::read(path)
}

#[cfg(test)]
#[path = "tests/explain_read.rs"]
mod tests;
