//! EC2 spot interruption and rebalance notices (#12876).
//!
//! A reclaimed spot host is rebuilt empty, so the spans its sweeps left open
//! are never closed by orphan recovery ([`crate::observability::lifecycle`]),
//! and the captain's assignment rows wait out their TTL. On an EC2 host a
//! watcher polls IMDSv2 for the two notices AWS gives ahead of a loss, and on
//! the first of each kind:
//!
//! - emits a `loom.host.interruption_notice` span ([`notice_span`]),
//! - closes every in-flight sweep's span as `loom.result =
//!   interrupted_by_host_loss` with `loom.host.loss_cause`, the way a planned
//!   roll closes as `interrupted_by_roll`, and
//! - releases the captain's assignment rows as `host_lost`.
//!
//! The watcher only polls while work is in flight, and is never started on a
//! host whose DMI vendor is not Amazon ([`ec2::is_ec2`]): a non-EC2 host
//! makes no IMDS request at all. A non-spot EC2 host answers 404 on both
//! endpoints and emits nothing. Moving the work elsewhere is #6704.

pub mod ec2;
pub mod imds;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use imds::Imds;

/// The `loom.result` of a sweep a host loss ended.
pub const RESULT: &str = "interrupted_by_host_loss";
/// Why the host was lost: [`Kind::as_str`].
pub const LOSS_CAUSE: &str = "loom.host.loss_cause";
const KIND: &str = "loom.host.interruption.kind";
const NOTICE_TIME: &str = "loom.host.interruption.notice_time";
const ACTION_TIME: &str = "loom.host.interruption.action_time";
const HOST_ID: &str = "loom.host.id";
const INSTANCE_TYPE: &str = "loom.host.instance_type";
const IN_FLIGHT: &str = "loom.host.in_flight_sweeps";
const ASSIGNMENTS_RELEASED: &str = "loom.host.assignments_released";
const SWEEPS_CLOSED: &str = "loom.host.interruption.sweeps_closed";
/// The exported span attributes this module adds.
pub const KEYS: [&str; 9] = [
    LOSS_CAUSE,
    KIND,
    NOTICE_TIME,
    ACTION_TIME,
    HOST_ID,
    INSTANCE_TYPE,
    IN_FLIGHT,
    ASSIGNMENTS_RELEASED,
    SWEEPS_CLOSED,
];

/// How often the watcher polls while work is in flight.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Which notice AWS gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `spot/instance-action`: the instance will be reclaimed at the action time.
    SpotReclaim,
    /// `events/recommendations/rebalance`: elevated risk, no deadline.
    Rebalance,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpotReclaim => "spot_reclaim",
            Self::Rebalance => "rebalance",
        }
    }
}

/// One notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: Kind,
    /// When the notice was given (rebalance) or first seen (spot reclaim,
    /// whose document carries only the action time).
    pub notice_time: DateTime<Utc>,
    /// When AWS will act; a rebalance recommendation has none.
    pub action_time: Option<DateTime<Utc>>,
}

fn time_field(doc: &str, field: &str) -> Option<DateTime<Utc>> {
    let v: serde_json::Value = serde_json::from_str(doc).ok()?;
    DateTime::parse_from_rfc3339(v.get(field)?.as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Parse a `spot/instance-action` document (`{"action":"terminate","time":…}`),
/// first seen at `now`. A stop or hibernate is a loss of the work too.
#[must_use]
pub fn parse_spot(doc: &str, now: DateTime<Utc>) -> Notice {
    Notice {
        kind: Kind::SpotReclaim,
        notice_time: now,
        action_time: time_field(doc, "time"),
    }
}

/// Parse a rebalance document (`{"noticeTime":…}`), first seen at `now`.
#[must_use]
pub fn parse_rebalance(doc: &str, now: DateTime<Utc>) -> Notice {
    Notice {
        kind: Kind::Rebalance,
        notice_time: time_field(doc, "noticeTime").unwrap_or(now),
        action_time: None,
    }
}

/// Poll both endpoints once. Returns each kind not yet in `seen`, marking it
/// seen: a notice is handled once per kind per daemon run. A failed request is
/// logged at debug and treated as no notice.
pub fn poll(imds: &dyn Imds, seen: &mut HashSet<Kind>, now: DateTime<Utc>) -> Vec<Notice> {
    let mut out = Vec::new();
    for (kind, path, parse) in [
        (Kind::SpotReclaim, imds::SPOT_ACTION_PATH, parse_spot as fn(&str, _) -> Notice),
        (Kind::Rebalance, imds::REBALANCE_PATH, parse_rebalance),
    ] {
        if seen.contains(&kind) {
            continue;
        }
        match imds.get(path) {
            Ok(Some(doc)) => {
                seen.insert(kind);
                out.push(parse(&doc, now));
            }
            Ok(None) => {}
            Err(e) => log::debug!("host_loss: IMDS {path}: {e}"),
        }
    }
    out
}

/// A sweep in flight on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    pub root: PathBuf,
    pub sweep_id: String,
}

/// What a notice does to the host's state.
pub trait Effects {
    /// Every non-terminal sweep across the managed workspaces.
    fn in_flight(&self) -> Vec<InFlight>;
    /// Close `sweep`'s span as [`RESULT`] with the notice's [`LOSS_CAUSE`].
    fn close_span(&self, sweep: &InFlight, notice: &Notice);
    /// Release every captain assignment as `host_lost`; how many were sent.
    fn release_assignments(&self) -> usize;
    /// Emit the notice span.
    fn emit(&self, span: SpanRecord);
    /// This host's instance type, when known.
    fn instance_type(&self) -> Option<String>;
}

/// The `loom.host.interruption_notice` span: an instant, its own root.
#[must_use]
pub fn notice_span(
    notice: &Notice,
    host_id: &str,
    instance_type: Option<&str>,
    sweep_ids: &[String],
    released: usize,
) -> SpanRecord {
    let name = SpanName::HostInterruptionNotice;
    let at = notice.notice_time;
    let context = TraceContext::derived(
        name.as_str(),
        &[
            notice.kind.as_str(),
            host_id,
            &crate::telemetry::trace::instant(at),
        ],
    );
    let mut attributes = TraceAttributes::new();
    let mut put = |k: &str, v: String| {
        attributes.insert(k.to_string(), v);
    };
    put(KIND, notice.kind.as_str().to_string());
    put(LOSS_CAUSE, notice.kind.as_str().to_string());
    put(NOTICE_TIME, at.to_rfc3339());
    if let Some(t) = notice.action_time {
        put(ACTION_TIME, t.to_rfc3339());
    }
    put(HOST_ID, host_id.to_string());
    if let Some(t) = instance_type {
        put(INSTANCE_TYPE, t.to_string());
    }
    // Attribute values are bounded (256 bytes): cap the id list to fit.
    let mut ids = String::new();
    for id in sweep_ids {
        if ids.len() + id.len() + 1 > 240 {
            ids.push_str(",…");
            break;
        }
        if !ids.is_empty() {
            ids.push(',');
        }
        ids.push_str(id);
    }
    put(IN_FLIGHT, ids);
    put(SWEEPS_CLOSED, sweep_ids.len().to_string());
    put(ASSIGNMENTS_RELEASED, released.to_string());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// React to `notice`: close the in-flight spans, release the captain's rows,
/// then emit the notice span naming what was affected.
pub fn handle(notice: &Notice, effects: &dyn Effects, host_id: &str) {
    let sweeps = effects.in_flight();
    log::warn!(
        "host_loss: {} notice (action at {}); ending {} in-flight sweep span(s) as {RESULT} (#12876)",
        notice.kind.as_str(),
        notice.action_time.map_or_else(|| "n/a".to_string(), |t| t.to_rfc3339()),
        sweeps.len()
    );
    for sweep in &sweeps {
        effects.close_span(sweep, notice);
    }
    let released = effects.release_assignments();
    let ids: Vec<String> = sweeps.iter().map(|s| s.sweep_id.clone()).collect();
    effects.emit(notice_span(notice, host_id, effects.instance_type().as_deref(), &ids, released));
}

/// The watcher loop: until `stop`, poll every `interval` while work is in
/// flight, and [`handle`] each new notice.
pub fn run(
    imds: &dyn Imds,
    effects: &dyn Effects,
    host_id: &str,
    interval: Duration,
    stop: &AtomicBool,
) {
    let mut seen = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        if !effects.in_flight().is_empty() {
            for notice in poll(imds, &mut seen, Utc::now()) {
                handle(&notice, effects, host_id);
            }
        }
        std::thread::sleep(interval);
    }
}

/// The production effects: the daemon's registries and the captain worker.
pub struct Live {
    lister: Box<dyn Fn() -> Vec<InFlight> + Send + Sync>,
    instance_type: Option<String>,
}

impl Live {
    #[must_use]
    pub fn new(
        lister: impl Fn() -> Vec<InFlight> + Send + Sync + 'static,
        instance_type: Option<String>,
    ) -> Self {
        Self {
            lister: Box::new(lister),
            instance_type,
        }
    }
}

impl Effects for Live {
    fn in_flight(&self) -> Vec<InFlight> {
        (self.lister)()
    }

    fn close_span(&self, sweep: &InFlight, notice: &Notice) {
        let mut close = TraceAttributes::new();
        close.insert(LOSS_CAUSE.into(), notice.kind.as_str().into());
        crate::observability::lifecycle::finish_execution(
            &sweep.root,
            &sweep.sweep_id,
            RESULT,
            close,
        );
    }

    fn release_assignments(&self) -> usize {
        crate::fleet_dispatch::worker::release_all(crate::fleet_dispatch::table::HOST_LOST_OUTCOME)
    }

    fn emit(&self, span: SpanRecord) {
        if crate::observability::ops::spans_exported() {
            crate::observability::ops::emit_span(span);
        }
    }

    fn instance_type(&self) -> Option<String> {
        self.instance_type.clone()
    }
}

/// Start the watcher on a thread of its own, when this is an EC2 host. A
/// non-EC2 host returns `false` having made no IMDS request.
pub fn spawn(lister: impl Fn() -> Vec<InFlight> + Send + Sync + 'static) -> bool {
    if !ec2::is_ec2() {
        return false;
    }
    let started = std::thread::Builder::new()
        .name("host-loss-watcher".into())
        .spawn(move || {
            let http = imds::Http::new(imds::IMDS_ADDR);
            let instance_type = http.get(imds::INSTANCE_TYPE_PATH).ok().flatten();
            let live = Live::new(lister, instance_type);
            run(
                &http,
                &live,
                &crate::host_identity::host_identity(),
                POLL_INTERVAL,
                &AtomicBool::new(false),
            );
        });
    if let Err(e) = &started {
        log::warn!("host_loss: could not start the watcher: {e}");
    }
    started.is_ok()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
