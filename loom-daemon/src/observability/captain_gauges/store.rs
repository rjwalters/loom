//! The captain gauges heartbeat in the fleet store: what the captain last
//! produced, and how a dispatcher reads it.
//!
//! # Why the store
//!
//! A dispatcher may stop producing a fleet gauge only while the captain's own
//! output is fresh, so it needs the captain's `as_of`. Hosts have no channel
//! to each other's telemetry; the one shared surface every host already reads
//! is the fleet store (`fleet.repo`), the same transport the captain's ETA fit
//! uses (`eta::fit::publish`, #10395). The heartbeat lives beside the fit: a
//! single JSON file, [`HEARTBEAT_PATH`], on a dedicated publication branch
//! (`fleet.captainGauges.ref`, default the fit's own `fleet.etaFitRef`
//! branch, so no new branch or ruleset exemption is needed). Never on the
//! store's reviewed branch or `main` ([`refuse_reviewed_branch`]).
//!
//! # Contract (`captain-gauges/v1`)
//!
//! ```json
//! {
//!   "schema": "captain-gauges/v1",
//!   "captain_host": "<host id>",
//!   "published_at": "<rfc3339>",
//!   "jobs": {
//!     "stage-dwell": { "as_of": "<rfc3339>", "repos": ["owner/repo", ...] },
//!     "star-facts": {
//!       "as_of": "<rfc3339>", "repos": ["owner/repo", ...],
//!       "labels": ["loom:operator-priority", ...],
//!       "counts": { "owner/repo": 2 }
//!     },
//!     "queue-blocked": {
//!       "as_of": "<rfc3339>", "repos": ["owner/repo", ...],
//!       "blocked": { "owner/repo": [ { "n": 12, "c": "<rfc3339>", "l": ["loom:blocked"] } ] }
//!     }
//!   }
//! }
//! ```
//!
//! `as_of` is when the captain last finished producing that job (its points
//! were handed to the OTLP sink, or its listings were read); `repos` are the
//! lowercased slugs that pass covered. A dispatcher stands down only for a
//! job and repo the declared captain covered within the staleness bound
//! ([`Heartbeat::fresh`]).
//!
//! `labels`, `counts` and `blocked` are the part 2 fact fields
//! ([`super::facts`]). They are optional and additive: a reader that predates
//! them ignores them, and a captain that predates them publishes no such job,
//! so the schema tag stays `v1`. A covered repo absent from `counts` has no
//! open starred issue; one absent from `blocked` has no blocked row.
//!
//! # Cost
//!
//! Captain: one contents `PUT` per publish interval, and one more when the
//! published content changes in between; the blob sha comes back
//! in the `PUT` reply and is reused, so the `GET` for it happens only on the
//! first publish of a process or after a conflict. Dispatcher: one
//! conditional `GET` per collector pass; an unchanged heartbeat is a `304`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::eta::fit::publish as fit_pub;
use crate::fleet_store::fetch::Transport;
use crate::fleet_store::propose::WriteTransport;
use crate::fleet_store::StoreLocation;

/// The heartbeat's schema tag.
pub const SCHEMA: &str = "captain-gauges/v1";
/// Store path of the heartbeat on the publication branch.
pub const HEARTBEAT_PATH: &str = "captain/gauges.json";
/// Config key of the publication branch. Unset: the ETA fit's branch.
pub const REF_KEY: &str = "fleet.captainGauges.ref";
/// Clock skew tolerated on a published `as_of`.
const FUTURE_SLACK_SECS: i64 = 300;

/// One open `loom:blocked` issue as the captain listed it (`queue-blocked`):
/// what a dispatcher needs to build its own `queue.snapshot` row, and nothing
/// else. Label names only, never free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedFact {
    /// The issue number.
    #[serde(rename = "n")]
    pub number: u32,
    /// RFC 3339 creation time, as the forge reported it.
    #[serde(rename = "c", default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Its `loom:*` and `tier:*` label names.
    #[serde(rename = "l", default)]
    pub labels: Vec<String>,
}

/// What the captain produced for one job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFacts {
    /// When the captain last finished the job.
    pub as_of: DateTime<Utc>,
    /// Lowercased `owner/repo` slugs that pass covered.
    #[serde(default)]
    pub repos: BTreeSet<String>,
    /// `star-facts`: every operator label the captain listed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub labels: BTreeSet<String>,
    /// `star-facts`: open starred issues per covered repo (absent: zero).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, u32>,
    /// `queue-blocked`: the blocked rows per covered repo (absent: none).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blocked: BTreeMap<String, Vec<BlockedFact>>,
}

impl JobFacts {
    /// Facts that cover `repos` at `as_of` and carry nothing else.
    #[must_use]
    pub fn covering(as_of: DateTime<Utc>, repos: BTreeSet<String>) -> Self {
        Self {
            as_of,
            repos,
            ..Self::default()
        }
    }
}

/// `captain-gauges/v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// [`SCHEMA`].
    pub schema: String,
    /// The declared captain that produced it.
    pub captain_host: String,
    /// When it was published.
    pub published_at: DateTime<Utc>,
    /// Job name -> what was produced.
    #[serde(default)]
    pub jobs: BTreeMap<String, JobFacts>,
}

impl Heartbeat {
    /// The heartbeat `captain` publishes at `now` for `produced`.
    #[must_use]
    pub fn new(captain: &str, now: DateTime<Utc>, produced: BTreeMap<String, JobFacts>) -> Self {
        Self {
            schema: SCHEMA.to_string(),
            captain_host: captain.to_string(),
            published_at: now,
            jobs: produced,
        }
    }

    /// `job`'s facts when they are usable at `now`: published by the declared
    /// `captain`, `as_of` not in the future (beyond a small skew) and no older
    /// than `max_age`. Anything else is `None`: the dispatcher computes locally.
    #[must_use]
    pub fn fresh(
        &self,
        job: &str,
        captain: &str,
        now: DateTime<Utc>,
        max_age: Duration,
    ) -> Option<&JobFacts> {
        if self.captain_host != captain {
            return None;
        }
        let facts = self.jobs.get(job)?;
        let future = facts.as_of > now + Duration::seconds(FUTURE_SLACK_SECS);
        (!future && now - facts.as_of <= max_age).then_some(facts)
    }
}

/// Parse a heartbeat and check its schema.
///
/// # Errors
///
/// When the bytes are not `captain-gauges/v1` JSON.
pub fn parse(bytes: &[u8]) -> Result<Heartbeat> {
    let hb: Heartbeat = serde_json::from_slice(bytes).context("heartbeat is not valid JSON")?;
    anyhow::ensure!(hb.schema == SCHEMA, "heartbeat schema `{}` is not {SCHEMA}", hb.schema);
    Ok(hb)
}

/// The publication branch (`fleet.captainGauges.ref`, else the ETA fit's).
#[must_use]
pub fn resolve_ref(effective: &Value) -> String {
    crate::config_resolver::get_path(effective, REF_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| fit_pub::resolve_ref(effective), str::to_string)
}

/// The store location (its `reference` is the publication branch) and the
/// store's reviewed branch, or `None` when `fleet.repo` is unset or anything
/// is invalid (logged): the feature is then off and every host is local.
#[must_use]
pub fn location_for(
    effective: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<(StoreLocation, String)> {
    let base = crate::fleet_store::resolve_location(effective, env)
        .map_err(|e| log::warn!("captain gauges: fleet store misconfigured: {e:#}"))
        .ok()??;
    let reference = resolve_ref(effective);
    if let Err(e) = fit_pub::validate_branch_for(REF_KEY, &reference) {
        log::warn!("captain gauges: heartbeat disabled, {e:#}");
        return None;
    }
    Some((
        StoreLocation {
            repo: base.repo,
            reference,
        },
        base.reference,
    ))
}

// ---------------------------------------------------------------------------
// Fetch (dispatcher)
// ---------------------------------------------------------------------------

/// The dispatcher's last good read, kept across passes so an unchanged
/// heartbeat costs a `304`.
#[derive(Debug, Default, Clone)]
pub struct FetchCache {
    etag: Option<String>,
    /// The last heartbeat read, while it is still the store's.
    pub heartbeat: Option<Heartbeat>,
}

/// What one fetch did, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// A new heartbeat was read.
    Updated,
    /// Unchanged since the last read (`304`).
    NotModified,
    /// The store has no heartbeat (no branch or no file).
    Absent,
    /// The read failed; the previous heartbeat (if any) is kept, and ages.
    Failed(String),
}

/// Read the heartbeat from `loc` into `cache`. A transport error or an
/// unexpected status keeps the cached heartbeat: its `as_of` keeps ageing, so
/// a dispatcher that cannot read the store still falls back once the
/// captain's last known output goes stale. A `404` or an unparseable body
/// clears it at once.
pub fn fetch(t: &dyn Transport, loc: &StoreLocation, cache: &mut FetchCache) -> Fetched {
    let etag = cache
        .heartbeat
        .as_ref()
        .and(cache.etag.as_deref())
        .map(str::to_string);
    let reply = match t.get(
        &fit_pub::contents_path(loc, HEARTBEAT_PATH),
        Some(fit_pub::RAW),
        etag.as_deref(),
    ) {
        Ok(reply) => reply,
        Err(e) => return Fetched::Failed(format!("{e:#}")),
    };
    match reply.status {
        304 if cache.heartbeat.is_some() => Fetched::NotModified,
        200 => match parse(reply.body.as_bytes()) {
            Ok(hb) => {
                cache.heartbeat = Some(hb);
                cache.etag = reply.etag;
                Fetched::Updated
            }
            Err(e) => {
                *cache = FetchCache::default();
                Fetched::Failed(format!("{e:#}"))
            }
        },
        404 => {
            *cache = FetchCache::default();
            Fetched::Absent
        }
        s => Fetched::Failed(format!("forge answered HTTP {s} reading {HEARTBEAT_PATH}")),
    }
}

// ---------------------------------------------------------------------------
// Publish (captain)
// ---------------------------------------------------------------------------

/// The captain's write state, kept across publishes.
#[derive(Debug, Default, Clone)]
pub struct PublishCache {
    branch_ok: bool,
    sha: Option<String>,
}

/// This autonomous write lands only on the dedicated publication branch,
/// never the store's reviewed branch or `main` (the same guard and
/// normalization as the ETA fit's; `write_scope::tests` asserts it is the
/// first statement of [`publish`]).
fn refuse_reviewed_branch(loc: &StoreLocation, base_ref: &str) -> Result<()> {
    fit_pub::refuse_reviewed_branch_for(REF_KEY, "the captain gauges heartbeat", loc, base_ref)
}

fn put(
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    body: &[u8],
    sha: Option<&str>,
    message: &str,
) -> Result<crate::fleet_store::fetch::Reply> {
    let mut payload = json!({
        "message": message,
        "content": general_purpose::STANDARD.encode(body),
        "branch": loc.reference,
    });
    if let Some(sha) = sha {
        payload["sha"] = Value::String(sha.to_string());
    }
    wt.write("PUT", &format!("repos/{}/contents/{HEARTBEAT_PATH}", loc.repo), &payload)
}

/// Write `hb` to `loc` (its `reference` is the publication branch, created
/// from `base_ref` if missing). The previous blob sha is reused from `cache`;
/// a stale one (`409`/`422`) is re-read once and the write retried.
///
/// # Errors
///
/// Any store failure; `cache` then forgets the sha so the next publish
/// re-reads it.
pub(super) fn publish(
    t: &dyn Transport,
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    base_ref: &str,
    hb: &Heartbeat,
    cache: &mut PublishCache,
) -> Result<()> {
    refuse_reviewed_branch(loc, base_ref)?;
    if !cache.branch_ok {
        fit_pub::ensure_branch(t, wt, loc, base_ref)?;
        cache.branch_ok = true;
    }
    // Compact: the part 2 facts make this file a few hundred rows on a big
    // fleet, and it is rewritten on every change.
    let body = serde_json::to_string(hb).context("encoding the heartbeat")? + "\n";
    let message = format!("captain gauges heartbeat ({})", hb.published_at.to_rfc3339());
    let sha = match cache.sha.take() {
        Some(sha) => Some(sha),
        None => fit_pub::blob_sha(t, loc, HEARTBEAT_PATH)?,
    };
    let mut reply = put(wt, loc, body.as_bytes(), sha.as_deref(), &message)?;
    if matches!(reply.status, 409 | 422) {
        let sha = fit_pub::blob_sha(t, loc, HEARTBEAT_PATH)?;
        reply = put(wt, loc, body.as_bytes(), sha.as_deref(), &message)?;
    }
    fit_pub::ensure_ok(&reply, &format!("writing {HEARTBEAT_PATH}"), &loc.repo)?;
    cache.sha = serde_json::from_str::<Value>(&reply.body)
        .ok()
        .and_then(|v| v["content"]["sha"].as_str().map(str::to_string));
    Ok(())
}

/// Publish `hb` through the store's own transport (the writer App for the
/// write, as the ETA fit). The caller reaches this only as the armed captain.
///
/// # Errors
///
/// Any store failure (see [`publish`]).
pub fn publish_heartbeat(
    workspace_root: &std::path::Path,
    loc: &StoreLocation,
    base_ref: &str,
    hb: &Heartbeat,
    cache: &mut PublishCache,
) -> Result<()> {
    let transport = crate::fleet_store::gh::GhTransport::new(workspace_root, &loc.repo);
    publish(&transport, &transport, loc, base_ref, hb, cache)
}

/// Read the heartbeat through the store's own transport (a reader App first).
pub fn fetch_heartbeat(
    workspace_root: &std::path::Path,
    loc: &StoreLocation,
    cache: &mut FetchCache,
) -> Fetched {
    let transport = crate::fleet_store::gh::GhTransport::new(workspace_root, &loc.repo);
    fetch(&transport, loc, cache)
}
