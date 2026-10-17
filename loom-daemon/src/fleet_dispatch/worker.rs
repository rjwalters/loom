//! The worker side of fleet dispatch (#12450; design
//! `docs/design/fleet-captain-dispatch.md`).
//!
//! A worker in fleet mode asks one arbiter, the captain's fleet-dispatch
//! server, before it claims an item on the forge. The forge lease stays the
//! only ownership record; the server only keeps two hosts from racing for
//! the same item, so a lost race costs no writer call.
//!
//! # Flag
//!
//! `fleetDispatch.enabled` (bool) and `fleetDispatch.url` in the effective
//! config, normally the per-host tier fleet-gitops renders. Fleet dispatch
//! is a property of the host, not of a repo, so the flag is read from the
//! daemon's own workspace ([`set_host_root`]), whose host-local tier
//! (`<daemon workspace>/.loom-local/local.json`) carries it, whichever
//! managed repo the step is for ([`host_endpoint`], #12493). Before the
//! daemon names its workspace (tests, a step spawned outside the daemon)
//! the caller's root stands in. Both keys are read fresh on every use, so
//! they are live-reloadable; each change of the answer is logged once at
//! INFO. With the flag off (the default) nothing here makes a call, emits a
//! span or changes a budget: dispatch is exactly what it was.
//!
//! # The claim
//!
//! The step spawn asks [`claim`] before its forge pre-claim, and before the
//! Builder's landed check, which is a forge read too:
//!
//! - **grant**: today's path runs unchanged. The assignment id rides the
//!   `loom.dispatch.step` span (`loom.assignment.id`); a refusal further on
//!   (a pre-check, a forge `claim_lost`) sends [`refuse`] with that refusal's
//!   token, and the step's end sends [`release`] with its outcome class.
//! - **refusal** (`held`, `not_eligible`, `cap`): the step is refused with
//!   no forge call at all, a pass for the tick.
//! - **`grace`** (the server just started): this one claim takes today's
//!   path, and it does not count toward fallback.
//! - **no answer** (unreachable, a timeout, a non-2xx, a malformed body):
//!   this one claim takes today's path, and the failure counts toward
//!   fallback ([`fallback`]). A `403` (this host's tailnet node is not
//!   mapped to its `host_id`) is such a failure, logged at ERROR.
//!
//! # Heartbeat
//!
//! Each registry's reaper tick (30 s by default) records its repo's live steps
//! ([`on_reaper_tick`]); at most one heartbeat per [`HEARTBEAT_GAP`] goes out
//! for the whole host, carrying every repo's live steps, the free slots and
//! the runnable roles the last step tick noted ([`note_capacity`]). In
//! fallback the heartbeat is the probe that ends it.
//!
//! A repo whose live set has not been refreshed for [`live_stale_after`]
//! (its workspace was deregistered and its reaper aborted, so neither its
//! next tick nor its steps' ends will come) leaves the table on the next
//! tick of any other repo, and its grants are released `failed`. Without
//! that, every later heartbeat would renew its steps and the captain would
//! keep their items `held` fleet-wide until this daemon restarts: the
//! captain's TTL backstop (spec §5.4, §6) only fires on rows nobody renews.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::client::{self, CallError, CallFacts};
use super::fallback::{self, Hysteresis, Mode, Transition};
use super::proto::{
    self, AckResponse, ClaimRequest, ClaimResponse, HeartbeatRequest, HeartbeatResponse, LiveStep,
    RefusalReason, RefuseRequest, ReleaseRequest,
};
use crate::step_role::StepRole;

/// The flag.
pub const ENABLED_KEY: &str = "fleetDispatch.enabled";
/// The server's base URL (`http://<captain tailnet address>:<port>`).
pub const URL_KEY: &str = "fleetDispatch.url";

/// The least time between two heartbeats from this host. A little under the
/// reaper's 30 s, so every reaper round sends one even with jitter, while
/// the reapers of a host's other repos in the same round do not.
pub const HEARTBEAT_GAP: Duration = Duration::from_secs(25);

/// The floor of how long a repo's live set stands without a refresh: three
/// default reaper periods (30 s each). Past it the repo's reaper is taken to
/// be gone (module docs). A longer configured reaper interval raises the
/// bar, see [`live_stale_after`].
pub const LIVE_STALE_AFTER: Duration = Duration::from_secs(90);

/// How long a live set stands without a refresh for a reaper ticking every
/// `reaper_interval`: three periods, never under [`LIVE_STALE_AFTER`]. A
/// fixed 90 s would age out a running step's repo between ticks once
/// `LOOM_SWEEP_REAPER_INTERVAL_SECS` exceeds it, and release its grant
/// `failed` while the step is live.
#[must_use]
pub fn live_stale_after_for(reaper_interval: Duration) -> Duration {
    reaper_interval.saturating_mul(3).max(LIVE_STALE_AFTER)
}

/// [`live_stale_after_for`] the effective (env-configured) reaper interval.
fn live_stale_after() -> Duration {
    live_stale_after_for(crate::sweep_registry::reaper::resolve_reaper_interval())
}

/// The server URL when `root`'s effective config enables fleet dispatch.
/// The dispatch paths ask [`host_endpoint`], which reads the host's tier.
#[must_use]
pub fn endpoint(root: &Path) -> Option<String> {
    let effective = crate::config_resolver::resolve_effective_config(root);
    endpoint_in(&effective)
}

/// [`endpoint`] over an already-resolved config. Pure.
#[must_use]
pub fn endpoint_in(effective: &Value) -> Option<String> {
    use crate::config_resolver::get_path;
    let enabled = get_path(effective, ENABLED_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let url = get_path(effective, URL_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())?;
    Some(url.trim_end_matches('/').to_string())
}

/// Name the daemon's own workspace root: the root whose effective config
/// (its host-local tier above all) says whether this host is in fleet mode
/// (#12493). Called once at daemon startup; a later call replaces it.
pub fn set_host_root(root: &Path) {
    state().host_root = Some(root.to_path_buf());
}

/// The server URL when this host is in fleet mode: [`endpoint`] of the
/// daemon workspace ([`set_host_root`]), or of `root` before one is named.
/// Read fresh on every call, so a flag flipped in the host tier takes effect
/// on the next claim, tick or heartbeat. A change of the answer (on, off, a
/// new URL) is logged once at INFO, so a host whose flag does not resolve
/// where it was set shows in its log.
#[must_use]
pub fn host_endpoint(root: &Path) -> Option<String> {
    let host_root = state().host_root.clone();
    let source = host_root.as_deref().unwrap_or(root);
    let url = endpoint(source);
    let mut s = state();
    if s.announced != url {
        match &url {
            Some(url) => log::info!(
                "fleet_dispatch: fleet mode is on for this host (captain {url}, from {}'s \
                 effective config); claims ask the captain first (#12493)",
                source.display()
            ),
            None if s.announced.is_some() => log::info!(
                "fleet_dispatch: fleet mode is off for this host ({}'s effective config); \
                 claiming without the captain (#12493)",
                source.display()
            ),
            None => {}
        }
        s.announced.clone_from(&url);
    }
    url
}

/// A grant, carried from the claim to its `refuse` or `release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub assignment_id: String,
    url: String,
    repo: String,
    item: u64,
    step: String,
}

/// What the captain said about one claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Fleet dispatch is off for this workspace.
    Off,
    /// No usable answer, `grace`, or this host is in fallback: take today's
    /// path.
    FailOpen,
    Granted(Grant),
    Refused(RefusalReason),
}

/// The `loom.dispatch.step` reason of a captain refusal: a closed token.
#[must_use]
pub fn refusal_token(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::Held => "captain_held",
        RefusalReason::Grace => "captain_grace",
        RefusalReason::NotEligible => "captain_not_eligible",
        RefusalReason::Cap => "captain_cap",
        RefusalReason::Cooldown => "captain_cooldown",
        RefusalReason::Other => "captain_refused",
    }
}

#[derive(Debug)]
struct Assigned {
    root: PathBuf,
    grant: Grant,
    /// When the step was noted spawned: ages out a grant whose repo never
    /// recorded a live set (deregistered before its first reaper tick).
    since: Instant,
}

/// One repo's live steps (by sweep id) and when its reaper last wrote them.
#[derive(Debug)]
struct LiveSet {
    refreshed: Instant,
    steps: Vec<(String, LiveStep)>,
}

#[derive(Debug, Default)]
struct State {
    /// The daemon's own workspace root ([`set_host_root`]).
    host_root: Option<PathBuf>,
    /// The endpoint [`host_endpoint`] last logged.
    announced: Option<String>,
    hysteresis: Hysteresis,
    /// Why the current fallback began (a failed call's token).
    entry_reason: &'static str,
    /// Live granted steps, by sweep id.
    assignments: HashMap<String, Assigned>,
    /// Each repo's live steps at its last reaper tick.
    live_by_root: HashMap<PathBuf, LiveSet>,
    /// `(free_slots, roles_ok)` at the last step tick.
    capacity: Option<(u32, Vec<String>)>,
    last_heartbeat: Option<Instant>,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record one call's outcome in the hysteresis; the mode after it. A `403`
/// is a failure like any other, and it is logged at ERROR: the server does
/// not map this host's tailnet node to its `host_id`, a fleet config error
/// that must be loud (#12450 refinement 7).
fn observe(ok: bool, failure: Option<&CallError>) -> Mode {
    if let Some(CallError::Status(403)) = failure {
        log::error!(
            "fleet_dispatch: the captain refused this host's identity (HTTP 403): its tailnet \
             node is not mapped to host_id {} in the fleet config; claiming without the captain",
            host_id()
        );
    }
    let mut s = state();
    match s.hysteresis.observe(ok, Instant::now()) {
        Some(Transition::Entered) => {
            let reason = failure.map_or("unknown", CallError::token);
            s.entry_reason = reason;
            log::warn!(
                "fleet_dispatch: {} calls in a row failed (last: {reason}); claiming without \
                 the captain, at the reduced fallback budget, until it answers for {}s (#12450)",
                fallback::ENTER_AFTER_FAILURES,
                fallback::LEAVE_AFTER.as_secs()
            );
        }
        Some(Transition::Left) => {
            log::info!("fleet_dispatch: the captain answered steadily; fallback ends (#12450)");
        }
        None => {}
    }
    s.hysteresis.mode()
}

/// The current mode.
#[must_use]
pub fn mode() -> Mode {
    state().hysteresis.mode()
}

fn host_id() -> String {
    crate::host_identity::host_identity()
}

/// `owner/name` of `root`, without a forge call.
fn repo_of(root: &Path) -> String {
    crate::observability::ops::dispatch_facts::repo_for_root(root)
        .unwrap_or_else(|| crate::peer_claims::repo_slug(root))
}

/// A claim answer, typed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Granted(String),
    Refused(RefusalReason),
}

/// Read a claim answer; `None` for a grant with no `assignment_id` (a
/// malformed answer, read as a failed call). A refusal with no reason reads
/// as [`RefusalReason::Other`]. Pure.
fn decide(r: &ClaimResponse) -> Option<Decision> {
    if r.granted {
        let id = r.assignment_id.clone().filter(|id| !id.is_empty())?;
        Some(Decision::Granted(id))
    } else {
        Some(Decision::Refused(r.reason.unwrap_or(RefusalReason::Other)))
    }
}

/// Ask the captain whether this host may claim `item` for `role` in `root`
/// (module docs). Blocks for at most about [`client::CALL_TIMEOUT`].
#[must_use]
pub fn claim(root: &Path, item: u32, role: StepRole) -> Ask {
    let Some(url) = host_endpoint(root) else {
        return Ask::Off;
    };
    if mode() == Mode::Fallback {
        return Ask::FailOpen;
    }
    let request = ClaimRequest {
        host_id: host_id(),
        repo: repo_of(root),
        item: u64::from(item),
        step: role.as_str().to_string(),
    };
    let started = Instant::now();
    let answer = client::post_blocking::<_, ClaimResponse>(&url, proto::CLAIM_PATH, &request)
        .and_then(|r| {
            decide(&r)
                .map(|d| (r.server_epoch.clone(), d))
                .ok_or_else(|| CallError::Malformed("a grant with no assignment_id".into()))
        });
    let mut facts = CallFacts {
        endpoint: proto::CLAIM_PATH,
        latency: started.elapsed(),
        repo: Some(request.repo.clone()),
        item: Some(request.item),
        step: Some(request.step.clone()),
        ..CallFacts::default()
    };
    let ask = match answer {
        Ok((epoch, Decision::Granted(assignment_id))) => {
            observe(true, None);
            facts.status = Some(200);
            facts.result = client::RESULT_GRANTED;
            facts.server_epoch = Some(epoch);
            facts.assignment_id = Some(assignment_id.clone());
            Ask::Granted(Grant {
                assignment_id,
                url,
                repo: request.repo,
                item: request.item,
                step: request.step,
            })
        }
        Ok((epoch, Decision::Refused(reason))) => {
            // `grace` is expected after every captain roll: it neither
            // counts as a failure nor resets one (spec §9.1).
            if reason != RefusalReason::Grace {
                observe(true, None);
            }
            facts.status = Some(200);
            facts.result = client::RESULT_REFUSED;
            facts.reason = Some(reason.as_str().to_string());
            facts.server_epoch = Some(epoch);
            if reason == RefusalReason::Grace {
                Ask::FailOpen
            } else {
                Ask::Refused(reason)
            }
        }
        Err(e) => {
            observe(false, Some(&e));
            facts.status = e.status();
            facts.result = client::RESULT_ERROR;
            facts.reason = Some(e.token().to_string());
            log::info!(
                "fleet_dispatch: claim of {} #{item} got no answer ({e}); claiming as today",
                request.repo
            );
            Ask::FailOpen
        }
    };
    client::record(&facts);
    ask
}

/// Send `body` to `path` without waiting; the answer feeds the hysteresis
/// and the call span.
fn send_detached<Req, Resp>(url: &str, path: &str, body: Req, facts: CallFacts)
where
    Req: serde::Serialize + Send + Sync + 'static,
    Resp: serde::de::DeserializeOwned + Send + 'static + Epoch,
{
    client::post_detached::<Req, Resp>(url, path, body, move |answer, latency| {
        let mut facts = CallFacts { latency, ..facts };
        match answer {
            Ok(resp) => {
                observe(true, None);
                facts.status = Some(200);
                facts.result = client::RESULT_OK;
                facts.server_epoch = Some(resp.epoch().to_string());
            }
            Err(e) => {
                observe(false, Some(&e));
                facts.status = e.status();
                facts.result = client::RESULT_ERROR;
                facts.reason = Some(e.token().to_string());
            }
        }
        client::record(&facts);
    });
}

/// An answer's `server_epoch`.
trait Epoch {
    fn epoch(&self) -> &str;
}

impl Epoch for AckResponse {
    fn epoch(&self) -> &str {
        &self.server_epoch
    }
}

impl Epoch for HeartbeatResponse {
    fn epoch(&self) -> &str {
        &self.server_epoch
    }
}

/// The granted step was refused before it ran (a pre-check, or the forge
/// claim: `reason` is that refusal's token, `claim_lost` included).
pub fn refuse(grant: &Grant, reason: &str) {
    let body = RefuseRequest {
        host_id: host_id(),
        assignment_id: grant.assignment_id.clone(),
        reason: reason.to_string(),
    };
    let facts = CallFacts {
        endpoint: proto::REFUSE_PATH,
        reason: Some(reason.to_string()),
        repo: Some(grant.repo.clone()),
        item: Some(grant.item),
        step: Some(grant.step.clone()),
        assignment_id: Some(grant.assignment_id.clone()),
        ..CallFacts::default()
    };
    send_detached::<_, AckResponse>(&grant.url, proto::REFUSE_PATH, body, facts);
}

/// The granted step `sweep_id` in `root` is running: list it until it ends.
pub fn note_spawned(root: &Path, sweep_id: &str, grant: Grant) {
    state().assignments.insert(
        sweep_id.to_string(),
        Assigned {
            root: root.to_path_buf(),
            grant,
            since: Instant::now(),
        },
    );
}

/// Step `sweep_id` ended with `outcome` (its outcome class): release its
/// assignment, if it had one. A no-op for a step the captain never granted.
pub fn release(sweep_id: &str, outcome: &str) {
    let Some(assigned) = state().assignments.remove(sweep_id) else {
        return;
    };
    send_release(&assigned.grant, outcome);
}

fn send_release(grant: &Grant, outcome: &str) {
    let body = ReleaseRequest {
        host_id: host_id(),
        assignment_id: grant.assignment_id.clone(),
        outcome: outcome.to_string(),
    };
    let facts = CallFacts {
        endpoint: proto::RELEASE_PATH,
        reason: Some(outcome.to_string()),
        repo: Some(grant.repo.clone()),
        item: Some(grant.item),
        step: Some(grant.step.clone()),
        assignment_id: Some(grant.assignment_id.clone()),
        ..CallFacts::default()
    };
    send_detached::<_, AckResponse>(&grant.url, proto::RELEASE_PATH, body, facts);
}

/// The step tick's view of this host: its free slots and the roles whose
/// pools are not exhausted (`exhausted`, role names), for the heartbeat.
///
/// Phase 1 reports pool exhaustion only. Spec §5.2's `roles_ok` also leaves
/// out roles behind the load brake or a disk back-off; those still run
/// through today's admission after a grant, and the captain does not yet
/// use `roles_ok` to pick hosts.
pub fn note_capacity(free_slots: usize, exhausted: &BTreeSet<String>) {
    // Several steps share a role name (the Champion's three): a set, so
    // each role is listed once, in a stable order.
    let roles: Vec<String> = StepRole::ALL
        .iter()
        .map(|r| r.role_name().to_string())
        .filter(|name| !exhausted.contains(name))
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    state().capacity = Some((u32::try_from(free_slots).unwrap_or(u32::MAX), roles));
}

/// One reaper tick of the registry at `root`, whose live steps are `live`
/// (`(sweep_id, item, role)`): record them, release any assignment whose
/// step is gone without an end, and send the host's heartbeat when one is
/// due. A no-op while the flag is off and nothing was ever granted.
pub fn on_reaper_tick(root: &Path, live: &[(String, u32, StepRole)]) {
    let url = host_endpoint(root);
    let (vanished, heartbeat) = {
        let mut s = state();
        if url.is_none() && s.assignments.is_empty() && s.live_by_root.is_empty() {
            return;
        }
        let ids: BTreeSet<&str> = live.iter().map(|(id, ..)| id.as_str()).collect();
        let gone: Vec<String> = s
            .assignments
            .iter()
            .filter(|(id, a)| a.root == root && !ids.contains(id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        let mut vanished: Vec<Grant> = gone
            .iter()
            .filter_map(|id| s.assignments.remove(id))
            .map(|a| a.grant)
            .collect();
        let repo = repo_of(root);
        let steps: Vec<(String, LiveStep)> = live
            .iter()
            .map(|(id, item, role)| {
                let assignment_id = s.assignments.get(id).map(|a| a.grant.assignment_id.clone());
                (
                    id.clone(),
                    LiveStep {
                        repo: repo.clone(),
                        item: u64::from(*item),
                        step: role.as_str().to_string(),
                        assignment_id,
                    },
                )
            })
            .collect();
        let now = Instant::now();
        s.live_by_root.insert(
            root.to_path_buf(),
            LiveSet {
                refreshed: now,
                steps,
            },
        );
        vanished.extend(prune_stale(&mut s, now));
        let due = s
            .last_heartbeat
            .is_none_or(|at| now.saturating_duration_since(at) >= HEARTBEAT_GAP);
        let heartbeat = match (&url, &s.capacity) {
            (Some(url), Some((free_slots, roles_ok))) if due => {
                let request = HeartbeatRequest {
                    host_id: host_id(),
                    live: s
                        .live_by_root
                        .values()
                        .flat_map(|set| &set.steps)
                        .map(|(_, step)| step.clone())
                        .collect(),
                    free_slots: *free_slots,
                    roles_ok: roles_ok.clone(),
                };
                s.last_heartbeat = Some(now);
                Some((url.clone(), request))
            }
            _ => None,
        };
        (vanished, heartbeat)
    };
    for grant in &vanished {
        send_release(grant, "failed");
    }
    if let Some((url, request)) = heartbeat {
        let facts = CallFacts {
            endpoint: proto::HEARTBEAT_PATH,
            ..CallFacts::default()
        };
        send_detached::<_, HeartbeatResponse>(&url, proto::HEARTBEAT_PATH, request, facts);
    }
}

/// Drop every repo whose live set is older than [`live_stale_after`] at
/// `now`, and take out its grants, to be released `failed` (module docs).
/// A grant whose repo never recorded a live set ages out the same way from
/// when it was noted spawned. The repo ticking now was just refreshed, so it
/// is never dropped. Pure over `state`.
fn prune_stale(state: &mut State, now: Instant) -> Vec<Grant> {
    let stale_after = live_stale_after();
    let stale = |at: Instant| now.saturating_duration_since(at) > stale_after;
    let dropped: BTreeSet<PathBuf> = state
        .live_by_root
        .iter()
        .filter(|(_, set)| stale(set.refreshed))
        .map(|(root, _)| root.clone())
        .collect();
    for root in &dropped {
        state.live_by_root.remove(root);
        log::warn!(
            "fleet_dispatch: {} has not refreshed its live steps for over {}s (its workspace was deregistered?); dropping them from the heartbeat and releasing their grants",
            root.display(),
            stale_after.as_secs()
        );
    }
    let orphaned: Vec<String> = state
        .assignments
        .iter()
        .filter(|(_, a)| {
            dropped.contains(&a.root)
                || (!state.live_by_root.contains_key(&a.root) && stale(a.since))
        })
        .map(|(id, _)| id.clone())
        .collect();
    orphaned
        .iter()
        .filter_map(|id| state.assignments.remove(id))
        .map(|a| a.grant)
        .collect()
}

/// The slots this tick may fill, and whether it runs in fallback: the
/// reduced budget ([`fallback::draw_budget`]) when the flag is on for the
/// host ([`host_endpoint`]) and the host is in fallback, counted on `loom.fleet_dispatch.fallback_ticks`
/// with the reason fallback began. Unchanged when the flag is off.
#[must_use]
pub fn tick_budget(root: &Path, free_slots: usize, spawn_concurrency: usize) -> (usize, bool) {
    if host_endpoint(root).is_none() {
        return (free_slots, false);
    }
    let (mode, reason) = {
        let s = state();
        (s.hysteresis.mode(), s.entry_reason)
    };
    if mode == Mode::Fallback {
        use crate::telemetry::ops::{MetricName, MetricPoint};
        crate::observability::ops::emit_metrics(vec![MetricPoint::int(
            MetricName::FleetDispatchFallbackTicks,
            1,
        )
        .label("reason", reason)]);
    }
    (
        fallback::draw_budget(mode, free_slots, spawn_concurrency),
        mode == Mode::Fallback,
    )
}

/// Forget every grant, live set and mode (tests).
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *state() = State::default();
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
