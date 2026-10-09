//! The production [`OutputSource`] (#10916, slice 3a): what the fleet
//! actually observed, read independently of whichever host owns each job.
//!
//! Two stores, both fed by every host, neither asked of the job's owner:
//!
//! - **SigNoz logs** ([`OUTPUTS_SQL`]): every registry row whose
//!   `record_kind` is a log kind (`eta.*`, `ci.run`). The newest record per
//!   `(loom.kind, loom.repo)` in a [`WINDOW`] look-back, by event time (the
//!   SigNoz rule's convention: `eta.backtest.fold` is stamped at the folded
//!   day's cutoff). The same read carries the inputs of
//!   [`OutputSource::expected_repos`] for `eta.estimate` ([`build`]).
//! - **The fleet store's `captain-gauges/v1` heartbeat**: the per-job `as_of`
//!   for the `captain-gauges/v1:<job>` rows. Read whoever published it.
//!
//! Everything here is pure: the read happens in `fleet_alert::output_feed`,
//! which hands over a [`Reading`]. A read that failed makes every row it
//! backs [`OutputSource::unreadable`], which `evaluate` judges missing.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{
    estimate_owed, OpenItem, OutputSource, CI_TELEMETRY_KEY, ETA_ENABLED_KEY, ETA_FIT_KEY,
    ETA_NIGHTLY_FOLDS_KEY, SINGLETON_OUTPUTS,
};
use crate::observability::captain_gauges::{self as gauges, store::Heartbeat};

/// Look-back of the SigNoz read: the SigNoz rule's `evalWindow`, above the
/// longest registry deadline (48 h).
pub const WINDOW: Duration = Duration::from_secs(72 * 3600);

/// `eta.estimate`, the one [`super::Scope::PerActiveRepo`] row.
pub const ESTIMATE: &str = "eta.estimate";
/// The open-item signal: a sweep started on the item, by any host.
pub const SWEEP_STARTED: &str = "sweep.started";
/// The closing signal: a `land` outcome.
pub const OUTCOME: &str = "eta.outcome";

/// The aggregate read: one row per `(kind, repo, issue)` over the window.
///
/// The kind list must hold every log-kind registry row plus
/// [`SWEEP_STARTED`] and [`OUTCOME`] (`observed_tests` checks it). The issue
/// number is read from the number map with a string-map fallback, as
/// `fleet_signoz_refresh::OUTCOMES_SQL` does.
pub const OUTPUTS_SQL: &str = "\
SELECT
    attributes_string['loom.kind'] AS kind,
    lower(attributes_string['loom.repo']) AS repo,
    coalesce(
        if(mapContains(attributes_number, 'loom.issue'),
           toUInt32(attributes_number['loom.issue']), NULL),
        toUInt32OrNull(attributes_string['loom.issue']), 0) AS issue,
    toString(min(timestamp)) AS first_ns,
    toString(max(timestamp)) AS last_ns,
    toUInt8(argMax(mapContains(attributes_string, 'loom.eta.no_estimate_reason'), timestamp)) AS refused,
    toUInt8(countIf(attributes_string['loom.eta.kind'] = 'land') > 0) AS landed
FROM signoz_logs.distributed_logs_v2
WHERE timestamp >= {since_ns:UInt64}
  AND timestamp <= {until_ns:UInt64}
  AND attributes_string['loom.kind'] IN ('eta.fleet_refresh', 'eta.backtest.fold', 'ci.run',
      'eta.fit', 'eta.estimate', 'eta.outcome', 'sweep.started')
GROUP BY kind, repo, issue
LIMIT 200000
FORMAT JSONEachRow
";

/// The bound parameters of [`OUTPUTS_SQL`] for a read at `now`.
#[must_use]
pub fn params(now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    let window = chrono::Duration::from_std(WINDOW).unwrap_or_else(|_| chrono::Duration::zero());
    let ns = |at: DateTime<Utc>| at.timestamp_nanos_opt().unwrap_or(0).max(0).to_string();
    vec![("since_ns", ns(now - window)), ("until_ns", ns(now))]
}

/// One row of [`OUTPUTS_SQL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: String,
    /// Lowercased `owner/repo`; empty for a fleet-wide record.
    pub repo: String,
    /// `0` when the record names no issue.
    pub issue: u32,
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
    /// The newest record of the group is an `eta.estimate` refusal.
    pub refused: bool,
    /// The group holds a `land` `eta.outcome`.
    pub landed: bool,
}

/// Whether `kind` is a captain-gauge heartbeat row, not a log kind.
#[must_use]
pub fn is_gauge_kind(kind: &str) -> bool {
    kind.strip_prefix(gauges::store::SCHEMA)
        .is_some_and(|rest| rest.starts_with(':'))
}

fn number(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// Parse [`OUTPUTS_SQL`]'s `JSONEachRow` body. One malformed line fails the
/// whole read: a partial view must never pass for a complete one.
///
/// # Errors
///
/// A line is not a JSON object with the query's columns.
pub fn parse_rows(text: &str) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = |what: &str| format!("invalid response, line {}: {what}", n + 1);
        let v: Value = serde_json::from_str(line).map_err(|e| bad(&format!("not JSON: {e}")))?;
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
        let at = |k: &str| {
            number(v.get(k))
                .map(|ns| Utc.timestamp_nanos(ns))
                .ok_or_else(|| bad(&format!("no integer {k}")))
        };
        let kind = text("kind");
        if kind.is_empty() {
            return Err(bad("no kind"));
        }
        rows.push(Row {
            kind: kind.to_string(),
            repo: text("repo").to_ascii_lowercase(),
            issue: number(v.get("issue"))
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(0),
            first: at("first_ns")?,
            last: at("last_ns")?,
            refused: number(v.get("refused")).is_some_and(|x| x != 0),
            landed: number(v.get("landed")).is_some_and(|x| x != 0),
        });
    }
    Ok(rows)
}

/// One refresh of both stores, as `fleet_alert::output_feed` read it.
#[derive(Debug, Clone)]
pub struct Reading {
    /// When the read finished.
    pub at: DateTime<Utc>,
    /// [`OUTPUTS_SQL`]'s rows, or why they could not be read.
    pub signoz: Result<Vec<Row>, String>,
    /// The heartbeat (`None`: the store has none), or why it could not be read.
    pub heartbeat: Result<Option<Heartbeat>, String>,
}

/// What the fleet produced, as an [`OutputSource`].
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// The fleet roster (lowercased slugs); empty when unknown.
    pub roster: Vec<String>,
    fleet: BTreeMap<String, DateTime<Utc>>,
    per_repo: BTreeMap<String, BTreeMap<String, DateTime<Utc>>>,
    expected: BTreeMap<String, Vec<String>>,
    disabled: BTreeSet<&'static str>,
    signoz_error: Option<String>,
    heartbeat_error: Option<String>,
}

impl Observed {
    /// Every row unreadable for `why` (no reading yet, or a stalled reader).
    #[must_use]
    pub fn unreadable_all(
        why: &str,
        roster: Vec<String>,
        disabled: BTreeSet<&'static str>,
    ) -> Self {
        Self {
            roster,
            disabled,
            signoz_error: Some(why.to_string()),
            heartbeat_error: Some(why.to_string()),
            ..Self::default()
        }
    }

    fn note(&mut self, kind: &str, repo: &str, at: DateTime<Utc>) {
        let newest = self.fleet.entry(kind.to_string()).or_insert(at);
        *newest = (*newest).max(at);
        if !repo.is_empty() {
            let seen = self
                .per_repo
                .entry(kind.to_string())
                .or_default()
                .entry(repo.to_string())
                .or_insert(at);
            *seen = (*seen).max(at);
        }
    }
}

/// Build the source from a [`Reading`] at `now`.
///
/// `eta.estimate`'s expected repos come from [`estimate_owed`] over the open
/// items, never from `eta.estimate` itself: an item is open while a
/// [`SWEEP_STARTED`] record (any host) is newer than its last `land`
/// [`OUTCOME`]. The estimates only say whether an item's newest answer is a
/// refusal. With a known roster, a repo outside it owes nothing.
#[must_use]
pub fn build(
    reading: &Reading,
    roster: Vec<String>,
    disabled: BTreeSet<&'static str>,
    now: DateTime<Utc>,
) -> Observed {
    let mut o = Observed {
        roster,
        disabled,
        ..Observed::default()
    };
    match &reading.heartbeat {
        Ok(Some(hb)) => {
            for (job, facts) in &hb.jobs {
                o.note(&format!("{}:{job}", gauges::store::SCHEMA), "", facts.as_of);
            }
        }
        Ok(None) => {}
        Err(e) => o.heartbeat_error = Some(format!("captain-gauge heartbeat: {e}")),
    }
    let rows = match &reading.signoz {
        Ok(rows) => rows,
        Err(e) => {
            o.signoz_error = Some(format!("SigNoz: {e}"));
            return o;
        }
    };
    type Item = (String, u32);
    let mut started: BTreeMap<Item, DateTime<Utc>> = BTreeMap::new();
    let mut landed: BTreeMap<Item, DateTime<Utc>> = BTreeMap::new();
    let mut refused: BTreeMap<Item, bool> = BTreeMap::new();
    for r in rows {
        o.note(&r.kind, &r.repo, r.last);
        if r.repo.is_empty() || r.issue == 0 {
            continue;
        }
        let key = (r.repo.clone(), r.issue);
        match r.kind.as_str() {
            SWEEP_STARTED => {
                started.insert(key, r.last);
            }
            OUTCOME if r.landed => {
                landed.insert(key, r.last);
            }
            ESTIMATE => {
                refused.insert(key, r.refused);
            }
            _ => {}
        }
    }
    let open: Vec<OpenItem> = started
        .into_iter()
        .filter(|(key, at)| landed.get(key).is_none_or(|closed| closed < at))
        .map(|(key, at)| OpenItem {
            newest_refused: refused.get(&key).copied(),
            repo: key.0,
            opened_at: at,
        })
        .collect();
    let grace = SINGLETON_OUTPUTS
        .iter()
        .find(|row| row.record_kind == ESTIMATE)
        .map_or(Duration::from_secs(3600), super::SingletonOutput::deadline);
    let mut owed = estimate_owed(&open, now, grace);
    if !o.roster.is_empty() {
        owed.retain(|repo| o.roster.contains(repo));
    }
    o.expected.insert(ESTIMATE.to_string(), owed);
    o
}

impl OutputSource for Observed {
    fn last_seen(&self, record_kind: &str) -> Option<DateTime<Utc>> {
        self.fleet.get(record_kind).copied()
    }
    fn last_seen_per_repo(&self, record_kind: &str) -> BTreeMap<String, DateTime<Utc>> {
        self.per_repo.get(record_kind).cloned().unwrap_or_default()
    }
    fn expected_repos(&self, record_kind: &str) -> Option<Vec<String>> {
        self.expected.get(record_kind).cloned()
    }
    fn disabled(&self) -> BTreeSet<&'static str> {
        self.disabled.clone()
    }
    fn unreadable(&self, record_kind: &str) -> Option<String> {
        if is_gauge_kind(record_kind) {
            self.heartbeat_error.clone()
        } else {
            self.signoz_error.clone()
        }
    }
}

/// The registry toggles that are **off** in `effective` (with `env`), the
/// same resolution each job applies: `autonomous.eta` (`eta::config`),
/// `fleet.captainGauges`, and the CI poller's resolved `enabled`.
///
/// This is the watchdog host's view; a toggle set only in another host's
/// host-local tier is not seen here.
#[must_use]
pub fn disabled_from(
    effective: &Value,
    env: &dyn Fn(&str) -> Option<String>,
    ci_telemetry_enabled: bool,
) -> BTreeSet<&'static str> {
    let eta = crate::eta::config::resolve(effective, env);
    let g = gauges::Config::from_effective(effective, env);
    [
        (ETA_ENABLED_KEY, eta.enabled),
        (ETA_NIGHTLY_FOLDS_KEY, eta.nightly_folds_enabled),
        (ETA_FIT_KEY, eta.fit_enabled),
        (CI_TELEMETRY_KEY, ci_telemetry_enabled),
        (gauges::ENABLED_KEY, g.enabled),
        (gauges::STAR_FACTS_KEY, g.star_facts),
        (gauges::QUEUE_BLOCKED_KEY, g.queue_blocked),
    ]
    .into_iter()
    .filter_map(|(key, on)| (!on).then_some(key))
    .collect()
}
