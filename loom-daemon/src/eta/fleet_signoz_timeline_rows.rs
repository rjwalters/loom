//! SigNoz timeline rows (#10519): the query, row admission and the page walk
//! for [`super::fleet_signoz_timeline`].
//!
//! # The read contract
//!
//! [`TIMELINE_SQL`] reads every record of the timeline kinds for one repo
//! from `signoz_logs.distributed_logs_v2` as `JSONEachRow`, oldest knowable
//! first, with the same keyset paging as the in-sweep reader
//! ([`super::fleet_signoz_refresh`]). Its column names are [`parse_row`]'s
//! input contract:
//!
//! | Column | Meaning |
//! |---|---|
//! | `record_id` | `loom.record_id`, else a content hash (webhook export rows carry none) |
//! | `kind` | `loom.kind`, else the body's `kind` on a webhook row, else the body (the OTLP event name) |
//! | `service` | resource `service.name`: [`SERVICE_WEBHOOK`] for the loom-ui export |
//! | `repo` | the queried repo on a `queue.snapshot` row (host-level, no repo); else `loom.repo`, else the body's `repo` on a webhook row |
//! | `attrs` / `nums` | the string / number attribute maps, as JSON |
//! | `body` | the log body: the record's JSON for the JSON-body kinds |
//! | `event_time_ns` / `knowable_time_ns` | `timestamp` / `observed_timestamp` |
//!
//! A field is looked up by name in `attrs`, then `nums`, then the body
//! object, then the body's `payload` and `raw` objects, so one reader takes
//! both an attribute-carrying daemon row and a webhook row whose fields are in
//! its JSON body. Which attribute names the two producers use is not pinned
//! by this repo for the webhook export, so every field has a short list of
//! names ([`Fields::get`]).
//!
//! # Knowable-at
//!
//! Every admitted row carries `observed_at`, the instant it became knowable,
//! and the timeline keeps only rows with `observed_at <= cutoff`
//! ([`super::point_in_time`]):
//!
//! - **Webhook rows**: the Worker's receipt time `at`. The D1 row existed from
//!   then (the `webhook-mirror` contract in [`super::fleet_events_webhook`]),
//!   whatever the SigNoz ingest time of a later export.
//! - **Daemon rows**: the record's own `observed_at` field when it has one
//!   (stage journal rows, `ci.*`, `pr.resolved`), else the SigNoz
//!   `observed_timestamp`. A row with neither has `observed_at = None` and is
//!   never knowable.
//!
//! A row whose knowable-at is before its own event time is rejected: no
//! genuine observation precedes the event.

use super::fleet_signoz::RowCursor;
use super::fleet_signoz_refresh::{
    ClickhouseHttp, Limits, PageQuery, ReadError, SignozRead, SignozStop,
};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// `service.name` of the loom-ui webhook export.
pub const SERVICE_WEBHOOK: &str = "loom-ui-d1-export";

/// The record kinds the timeline reads.
pub mod kind {
    /// A label change: a webhook transition, or a daemon label-set row.
    pub const LABEL_TRANSITION: &str = "label.transition";
    /// The daemon's first sight of an item's labels: a baseline set.
    pub const LABEL_FIRST_SEEN: &str = "label.first_seen";
    /// A PR's merge or close instant, from the daemon (#10519).
    pub const PR_RESOLVED: &str = "pr.resolved";
    /// A completed CI run.
    pub const CI_RUN: &str = "ci.run";
    /// A completed CI job.
    pub const CI_JOB: &str = "ci.job";
    /// A CI duration sample.
    pub const CI_DURATION: &str = "ci.duration";
    /// The work finder's ranked ready queue.
    pub const QUEUE_SNAPSHOT: &str = "queue.snapshot";
    /// Every kind above.
    pub const ALL: [&str; 7] = [
        LABEL_TRANSITION,
        LABEL_FIRST_SEEN,
        PR_RESOLVED,
        CI_RUN,
        CI_JOB,
        CI_DURATION,
        QUEUE_SNAPSHOT,
    ];
}

/// Why a well-formed row was not admitted. Every rejection is counted under
/// exactly one of these.
pub mod reject {
    pub const MISSING_RECORD_ID: &str = "missing_record_id";
    pub const UNKNOWN_KIND: &str = "unknown_kind";
    pub const REPO_MISMATCH: &str = "repo_mismatch";
    pub const MISSING_TARGET: &str = "missing_target";
    pub const MISSING_NUMBER: &str = "missing_number";
    pub const UNKNOWN_ACTION: &str = "unknown_action";
    pub const MISSING_LABELS: &str = "missing_labels";
    pub const UNKNOWN_STATE: &str = "unknown_state";
    pub const MISSING_EVENT_TIME: &str = "missing_event_time";
    pub const MISSING_CI_RUN: &str = "missing_ci_run";
    pub const MISSING_QUEUE_ROWS: &str = "missing_queue_rows";
    pub const KNOWABLE_BEFORE_EVENT: &str = "knowable_before_event";
}

/// The timeline query (see the module docs for its columns).
///
/// `kind` and `repo` are the **resolved** values, and the `WHERE` filters on
/// them, not on the raw attributes: a `loom.kind` / `loom.repo` attribute
/// wins; on a [`SERVICE_WEBHOOK`] row (empty attribute maps, the D1 record as
/// a JSON body) they fall back to the body's top-level `kind` / `repo`; any
/// other row's kind falls back to the body (the OTLP event name) and its repo
/// to nothing. The body fallback is scoped to the webhook service so a daemon
/// row's JSON body can never stand in for its missing attributes.
pub const TIMELINE_SQL: &str = "\
SELECT
    if(attributes_string['loom.record_id'] != '', attributes_string['loom.record_id'],
       concat('h:', toString(cityHash64(body, toJSONString(attributes_string), timestamp)))) AS record_id,
    resources_string['service.name'] AS service,
    multiIf(attributes_string['loom.kind'] != '', attributes_string['loom.kind'],
            service = 'loom-ui-d1-export', JSONExtractString(body, 'kind'),
            body) AS kind,
    multiIf(kind = 'queue.snapshot', {repo:String},
            attributes_string['loom.repo'] != '', attributes_string['loom.repo'],
            service = 'loom-ui-d1-export', JSONExtractString(body, 'repo'),
            '') AS repo,
    toJSONString(attributes_string) AS attrs,
    toJSONString(attributes_number) AS nums,
    body,
    toString(timestamp) AS event_time_ns,
    toString(observed_timestamp) AS knowable_time_ns
FROM signoz_logs.distributed_logs_v2
WHERE kind IN ('label.transition', 'label.first_seen', 'pr.resolved', 'ci.run', 'ci.job',
               'ci.duration', 'queue.snapshot')
  AND lower(repo) = lower({repo:String})
  AND observed_timestamp >= {since_ns:UInt64}
  AND observed_timestamp <= {until_ns:UInt64}
  AND (observed_timestamp, record_id) > ({after_ns:UInt64}, {after_id:String})
ORDER BY observed_timestamp, record_id
LIMIT {limit:UInt32}
FORMAT JSONEachRow
";

/// Which producer a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// The loom-ui webhook export: exact receipt times.
    Webhook,
    /// A Loom daemon: polling-time observations.
    Daemon,
}

/// Whether an item is an issue or a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Issue,
    Pr,
}

/// One issue or PR: `repo` is lower-cased.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemKey {
    pub repo: String,
    pub target: Target,
    pub number: u32,
}

impl ItemKey {
    /// `repo` is lower-cased here.
    #[must_use]
    pub fn new(repo: &str, target: Target, number: u32) -> Self {
        ItemKey {
            repo: repo.to_ascii_lowercase(),
            target,
            number,
        }
    }
}

/// A label was added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Transition {
    Added,
    Removed,
}

/// An item lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lifecycle {
    Opened,
    Reopened,
    Closed,
    Merged,
}

/// One `ci.run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunRow {
    pub run_id: u64,
    pub run_attempt: u32,
    pub workflow: String,
    pub git_ref: Option<String>,
    pub head_sha: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: Option<i64>,
}

/// One `ci.job`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiJobRow {
    pub run_id: u64,
    pub job_id: u64,
    pub job: String,
    pub conclusion: Option<String>,
    pub completed_at: DateTime<Utc>,
}

/// One `ci.duration` sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiDurationRow {
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: Option<u64>,
    pub duration_ms: i64,
    pub completed_at: DateTime<Utc>,
}

/// One ready-queue row of a `queue.snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub issue: u32,
    pub rank: Option<u64>,
    pub state: Option<String>,
    pub disposition: Option<String>,
    pub reason: Option<String>,
}

/// What a row says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowBody {
    /// One label change at `at`.
    Label {
        item: ItemKey,
        label: String,
        transition: Transition,
        at: DateTime<Utc>,
    },
    /// The daemon's whole label set for `item`, at its `observed_at`.
    LabelSet {
        item: ItemKey,
        labels: BTreeSet<String>,
    },
    /// An open, close, merge or reopen at `at`.
    Lifecycle {
        item: ItemKey,
        event: Lifecycle,
        at: DateTime<Utc>,
    },
    CiRun(CiRunRow),
    CiJob(CiJobRow),
    CiDuration(CiDurationRow),
    /// The queried repo's rows of one snapshot, ticked at `tick_at`.
    Queue {
        tick_at: DateTime<Utc>,
        entries: Vec<QueueEntry>,
    },
}

/// One admitted row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The delivery identity.
    pub record_id: String,
    /// `owner/repo`, lower-cased.
    pub repo: String,
    pub source: Source,
    /// When the row became knowable; `None` is never knowable.
    pub observed_at: Option<DateTime<Utc>>,
    pub body: RowBody,
}

impl super::point_in_time::Observed for Row {
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        self.observed_at
    }
}

/// What one line parsed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedRow {
    Admitted(Box<Row>),
    /// Well-formed but not admissible, for the named [`reject`] reason.
    Rejected(&'static str),
    /// Admissible, but says nothing the timeline uses (an issue `opened`, a
    /// PR resolution in the `open` state).
    Ignored,
}

/// The fields of one row, looked up by name (see the module docs).
struct Fields<'a> {
    columns: &'a Map<String, Value>,
    maps: Vec<Map<String, Value>>,
}

fn object(value: Option<&Value>) -> Option<Map<String, Value>> {
    match value? {
        Value::Object(map) => Some(map.clone()),
        Value::String(text) => match serde_json::from_str::<Value>(text).ok()? {
            Value::Object(map) => Some(map),
            _ => None,
        },
        _ => None,
    }
}

impl<'a> Fields<'a> {
    fn new(columns: &'a Map<String, Value>) -> Self {
        let mut maps = Vec::new();
        for name in ["attrs", "nums"] {
            maps.extend(object(columns.get(name)));
        }
        if let Some(body) = object(columns.get("body")) {
            let nested: Vec<Map<String, Value>> = ["payload", "raw"]
                .iter()
                .filter_map(|key| object(body.get(*key)))
                .collect();
            maps.push(body);
            maps.extend(nested);
        }
        Fields { columns, maps }
    }

    /// The first non-null, non-empty value under any of `names`.
    fn get(&self, names: &[&str]) -> Option<&Value> {
        names.iter().find_map(|name| {
            self.maps
                .iter()
                .filter_map(|map| map.get(*name))
                .find(|v| !v.is_null() && v.as_str().is_none_or(|s| !s.trim().is_empty()))
        })
    }

    fn text(&self, names: &[&str]) -> Option<String> {
        match self.get(names)? {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    fn int(&self, names: &[&str]) -> Option<i64> {
        int(self.get(names)?)
    }

    fn uint<T: TryFrom<i64>>(&self, names: &[&str]) -> Option<T> {
        self.int(names).and_then(|n| T::try_from(n).ok())
    }

    fn time(&self, names: &[&str]) -> Option<DateTime<Utc>> {
        match self.get(names)? {
            Value::String(s) => DateTime::parse_from_rfc3339(s.trim())
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            _ => None,
        }
    }

    fn column(&self, name: &str) -> Option<&str> {
        self.columns
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn column_time(&self, name: &str) -> Option<DateTime<Utc>> {
        self.columns
            .get(name)
            .and_then(int)
            .filter(|ns| *ns > 0)
            .map(DateTime::from_timestamp_nanos)
    }
}

/// An integer that ClickHouse may render as a JSON number (possibly a float
/// for the number map) or a quoted string.
fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                .map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

const NUMBER: &[&str] = &["number", "loom.number"];
const PR_NUMBER: &[&str] = &["loom.pr_number", "pr_number", "pr"];
const ISSUE: &[&str] = &["loom.issue", "issue"];
const OBSERVED_AT: &[&str] = &[
    "observed_at",
    "loom.eta.pr.observed_at",
    "loom.ci.observed_at",
];

/// Parse one `JSONEachRow` line of [`TIMELINE_SQL`] for `repo`.
///
/// `Err` means the **response** is invalid (not a JSON object, or the paging
/// columns are unreadable) and the whole fetch must fail. `Ok(Rejected)` is
/// one readable record that is not admissible; it is counted and skipped.
///
/// # Errors
///
/// The line is not a JSON object, or `knowable_time_ns` / `record_id` are
/// missing or of the wrong type.
pub fn parse_row(line: &str, repo: &str) -> Result<(RowCursor, ParsedRow), String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("row is not JSON: {e}"))?;
    let Value::Object(columns) = value else {
        return Err("row is not a JSON object".to_string());
    };
    let knowable_ns = match columns.get("knowable_time_ns") {
        Some(v) => int(v).ok_or("knowable_time_ns is not an integer")?,
        None => return Err("row has no knowable_time_ns column".to_string()),
    };
    let record_id = match columns.get("record_id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => String::new(),
        _ => return Err("row has no record_id column".to_string()),
    };
    let cursor = (knowable_ns, record_id.clone());
    Ok((cursor, admit(&Fields::new(&columns), record_id, repo)))
}

fn admit(fields: &Fields<'_>, record_id: String, repo: &str) -> ParsedRow {
    use ParsedRow::Rejected;
    if record_id.trim().is_empty() {
        return Rejected(reject::MISSING_RECORD_ID);
    }
    let Some(kind) = fields
        .column("kind")
        .map(str::to_string)
        .or_else(|| fields.text(&["kind", "loom.kind"]))
        .filter(|k| kind::ALL.contains(&k.as_str()))
    else {
        return Rejected(reject::UNKNOWN_KIND);
    };
    let row_repo = fields
        .column("repo")
        .map(str::to_string)
        .or_else(|| fields.text(&["loom.repo", "repo"]));
    if !row_repo.is_some_and(|r| r.eq_ignore_ascii_case(repo)) {
        return Rejected(reject::REPO_MISMATCH);
    }
    let source = if fields.column("service") == Some(SERVICE_WEBHOOK) {
        Source::Webhook
    } else {
        Source::Daemon
    };
    let event_at = fields.column_time("event_time_ns");
    let knowable_column = fields.column_time("knowable_time_ns");
    let body = match kind.as_str() {
        kind::LABEL_TRANSITION | kind::LABEL_FIRST_SEEN => label_body(fields, repo),
        kind::PR_RESOLVED => resolved_body(fields, repo),
        kind::QUEUE_SNAPSHOT => queue_body(fields, repo, event_at),
        _ => ci_body(fields, &kind),
    };
    let body = match body {
        Ok(Some(body)) => body,
        Ok(None) => return ParsedRow::Ignored,
        Err(reason) => return Rejected(reason),
    };
    let event_time = match &body {
        RowBody::Label { at, .. } | RowBody::Lifecycle { at, .. } => Some(*at),
        RowBody::CiRun(r) => Some(r.completed_at),
        RowBody::CiJob(j) => Some(j.completed_at),
        RowBody::CiDuration(d) => Some(d.completed_at),
        RowBody::Queue { tick_at, .. } => Some(*tick_at),
        RowBody::LabelSet { .. } => None,
    };
    let observed_at = match source {
        // The receipt time: the D1 row existed from then.
        Source::Webhook => event_time,
        Source::Daemon => fields.time(OBSERVED_AT).or(knowable_column),
    };
    if let (Some(observed), Some(event)) = (observed_at, event_time) {
        if observed < event {
            return Rejected(reject::KNOWABLE_BEFORE_EVENT);
        }
    }
    let body = match body {
        // A daemon's label change is dated by its observation.
        RowBody::Label {
            item,
            label,
            transition,
            at,
        } if source == Source::Daemon => RowBody::Label {
            item,
            label,
            transition,
            at: observed_at.unwrap_or(at),
        },
        other => other,
    };
    ParsedRow::Admitted(Box::new(Row {
        record_id,
        repo: repo.to_ascii_lowercase(),
        source,
        observed_at,
        body,
    }))
}

type Body = Result<Option<RowBody>, &'static str>;

fn label_body(fields: &Fields<'_>, repo: &str) -> Body {
    let action = fields.text(&["action", "loom.action"]);
    let label = fields.text(&["label", "loom.label"]);
    let target = match fields.text(&["target", "loom.target"]).as_deref() {
        Some("pr") => Some(Target::Pr),
        Some("issue") => Some(Target::Issue),
        Some(_) => return Err(reject::MISSING_TARGET),
        None => None,
    };
    if action.is_none() {
        // A daemon stage-journal row: the whole label set after the change.
        let item = match (fields.uint::<u32>(PR_NUMBER), fields.uint::<u32>(ISSUE)) {
            (Some(pr), _) if pr > 0 => ItemKey::new(repo, Target::Pr, pr),
            (_, Some(issue)) if issue > 0 => ItemKey::new(repo, Target::Issue, issue),
            _ => return Err(reject::MISSING_NUMBER),
        };
        let labels = match fields.get(&["labels", "labels_after"]) {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(Value::String(text)) => match serde_json::from_str::<Vec<String>>(text) {
                Ok(list) => list.into_iter().collect(),
                Err(_) => return Err(reject::MISSING_LABELS),
            },
            _ => return Err(reject::MISSING_LABELS),
        };
        return Ok(Some(RowBody::LabelSet { item, labels }));
    }
    let Some(target) = target else {
        return Err(reject::MISSING_TARGET);
    };
    let Some(number) = fields.uint::<u32>(NUMBER).filter(|n| *n > 0) else {
        return Err(reject::MISSING_NUMBER);
    };
    let item = ItemKey::new(repo, target, number);
    let at = fields
        .time(&["at", "emitted_at"])
        .or_else(|| fields.column_time("event_time_ns"))
        .ok_or(reject::MISSING_EVENT_TIME)?;
    let merged = matches!(fields.get(&["merged"]), Some(Value::Bool(true)))
        || fields.text(&["merged"]).as_deref() == Some("true");
    let lifecycle = |event| {
        Ok(Some(RowBody::Lifecycle {
            item: item.clone(),
            event,
            at,
        }))
    };
    match (action.as_deref(), label) {
        (Some("labeled"), Some(label)) => Ok(Some(RowBody::Label {
            item,
            label,
            transition: Transition::Added,
            at,
        })),
        (Some("unlabeled"), Some(label)) => Ok(Some(RowBody::Label {
            item,
            label,
            transition: Transition::Removed,
            at,
        })),
        (Some("opened"), _) => lifecycle(Lifecycle::Opened),
        (Some("reopened"), _) => lifecycle(Lifecycle::Reopened),
        (Some("closed"), _) if merged => lifecycle(Lifecycle::Merged),
        (Some("closed"), _) => lifecycle(Lifecycle::Closed),
        _ => Err(reject::UNKNOWN_ACTION),
    }
}

fn resolved_body(fields: &Fields<'_>, repo: &str) -> Body {
    let Some(pr) = fields.uint::<u32>(PR_NUMBER).filter(|n| *n > 0) else {
        return Err(reject::MISSING_NUMBER);
    };
    let event = match fields.text(&["state", "loom.eta.pr.state"]).as_deref() {
        Some("merged") => Lifecycle::Merged,
        Some("closed") => Lifecycle::Closed,
        Some("open") => return Ok(None),
        _ => return Err(reject::UNKNOWN_STATE),
    };
    let at = fields
        .time(&["resolved_at", "loom.eta.pr.resolved_at"])
        .or_else(|| fields.column_time("event_time_ns"))
        .ok_or(reject::MISSING_EVENT_TIME)?;
    Ok(Some(RowBody::Lifecycle {
        item: ItemKey::new(repo, Target::Pr, pr),
        event,
        at,
    }))
}

fn ci_body(fields: &Fields<'_>, kind: &str) -> Body {
    let completed_at = fields
        .time(&["loom.ci.completed_at", "completed_at"])
        .or_else(|| fields.column_time("event_time_ns"))
        .ok_or(reject::MISSING_EVENT_TIME)?;
    let run_id = fields
        .uint::<u64>(&["loom.ci.run_id", "run_id"])
        .ok_or(reject::MISSING_CI_RUN)?;
    let run_attempt = fields
        .uint::<u32>(&["loom.ci.run_attempt", "run_attempt"])
        .unwrap_or(1);
    let conclusion = fields.text(&["loom.ci.conclusion", "conclusion"]);
    let duration_ms = fields.int(&["loom.ci.duration_ms", "duration_ms"]);
    Ok(Some(match kind {
        kind::CI_RUN => RowBody::CiRun(CiRunRow {
            run_id,
            run_attempt,
            workflow: fields
                .text(&["loom.ci.workflow", "workflow"])
                .unwrap_or_default(),
            git_ref: fields.text(&["loom.ci.ref", "git_ref", "ref"]),
            head_sha: fields.text(&["loom.ci.head_sha", "head_sha"]),
            status: fields.text(&["loom.ci.status", "status"]),
            conclusion,
            completed_at,
            duration_ms,
        }),
        kind::CI_JOB => RowBody::CiJob(CiJobRow {
            run_id,
            job_id: fields
                .uint::<u64>(&["loom.ci.job_id", "job_id"])
                .ok_or(reject::MISSING_CI_RUN)?,
            job: fields.text(&["loom.ci.job", "job"]).unwrap_or_default(),
            conclusion,
            completed_at,
        }),
        _ => RowBody::CiDuration(CiDurationRow {
            run_id,
            run_attempt,
            job_id: fields.uint::<u64>(&["loom.ci.job_id", "job_id"]),
            duration_ms: duration_ms.ok_or(reject::MISSING_CI_RUN)?,
            completed_at,
        }),
    }))
}

fn queue_body(fields: &Fields<'_>, repo: &str, event_at: Option<DateTime<Utc>>) -> Body {
    let tick_at = fields
        .time(&["tick_at"])
        .or(event_at)
        .ok_or(reject::MISSING_EVENT_TIME)?;
    let rows = match fields.get(&["rows"]) {
        Some(Value::Array(rows)) => rows.clone(),
        Some(Value::String(text)) => match serde_json::from_str::<Vec<Value>>(text) {
            Ok(rows) => rows,
            Err(_) => return Err(reject::MISSING_QUEUE_ROWS),
        },
        _ => return Err(reject::MISSING_QUEUE_ROWS),
    };
    let text = |row: &Value, key: &str| match row.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v @ Value::Object(_)) => Some(v.to_string()),
        _ => None,
    };
    let entries = rows
        .iter()
        .filter(|row| {
            row.get("repo")
                .and_then(Value::as_str)
                .is_some_and(|r| r.eq_ignore_ascii_case(repo))
        })
        .filter_map(|row| {
            Some(QueueEntry {
                issue: row
                    .get("issue")
                    .and_then(int)
                    .and_then(|n| u32::try_from(n).ok())?,
                rank: row
                    .get("rank")
                    .and_then(int)
                    .and_then(|n| u64::try_from(n).ok()),
                state: text(row, "state"),
                disposition: text(row, "disposition"),
                reason: text(row, "reason"),
            })
        })
        .collect();
    Ok(Some(RowBody::Queue { tick_at, entries }))
}

/// [`TIMELINE_SQL`] over ClickHouse's HTTP interface: the same transport,
/// credential handling and parameter binding as the in-sweep reader.
#[derive(Debug, Clone)]
pub struct TimelineHttp(pub ClickhouseHttp);

impl SignozRead for TimelineHttp {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        self.0.post(TIMELINE_SQL, query)
    }
}

/// What one timeline walk read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkReport {
    pub repo: String,
    pub stop: SignozStop,
    pub pages: u32,
    pub rows: u64,
    pub ignored: u64,
    /// Rejected rows per [`reject`] reason.
    pub rejected: BTreeMap<String, usize>,
    /// The failure, for the log line. Never carries a credential.
    pub detail: Option<String>,
}

/// Walk `[since, until]` for `repo` page by page through `reader`, which
/// must serve [`TIMELINE_SQL`]'s columns. `Ok` only for a complete walk; a
/// partial window is never returned as complete.
///
/// # Errors
///
/// The walk stopped early (backend unavailable, invalid page, a cursor that
/// did not advance, the page ceiling); the report says why.
pub fn walk(
    repo: &str,
    reader: &mut dyn SignozRead,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    limits: Limits,
) -> Result<(Vec<Row>, WalkReport), Box<WalkReport>> {
    let mut report = WalkReport {
        repo: repo.to_string(),
        stop: SignozStop::Complete,
        pages: 0,
        rows: 0,
        ignored: 0,
        rejected: BTreeMap::new(),
        detail: None,
    };
    let fail = |mut report: WalkReport, stop: SignozStop, detail: String| {
        report.stop = stop;
        report.detail = Some(detail);
        Err(Box::new(report))
    };
    let limit = limits.page_size.max(1);
    let mut rows = Vec::new();
    let mut after: Option<RowCursor> = None;
    loop {
        if report.pages >= limits.max_pages {
            let detail = format!("window not exhausted after {} page(s)", limits.max_pages);
            return fail(report, SignozStop::PageLimit, detail);
        }
        let query = PageQuery {
            repo: repo.to_string(),
            since,
            until,
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
        let page_start = after.clone();
        let mut on_page = 0_u32;
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let (cursor, parsed) = match parse_row(line, repo) {
                Ok(row) => row,
                Err(why) => return fail(report, SignozStop::InvalidResponse, why),
            };
            if page_start.as_ref().is_some_and(|start| &cursor <= start)
                || after.as_ref().is_some_and(|prev| &cursor < prev)
            {
                let detail = "page did not advance past the previous cursor".to_string();
                return fail(report, SignozStop::InvalidResponse, detail);
            }
            after = Some(cursor);
            on_page += 1;
            report.rows += 1;
            match parsed {
                ParsedRow::Admitted(row) => rows.push(*row),
                ParsedRow::Ignored => report.ignored += 1,
                ParsedRow::Rejected(reason) => {
                    *report.rejected.entry(reason.to_string()).or_insert(0) += 1;
                }
            }
        }
        if on_page < limit {
            break;
        }
    }
    Ok((rows, report))
}
