//! The cross-host completion-narration dedup — the one piece of the retired
//! peer-claim channel that survives #13945 (operator ruling 2026-11-15, #13920).
//!
//! Dispatch coordination no longer rides the safehouse room: the fleet runs on
//! captain assignment plus forge leases. What is left here is only the
//! **completion** ad (Issue #6352): after a host narrates a merged PR's
//! public-feed completion it tells its peers, and a peer that hears it skips its
//! own narration of the same `(repo, issue, merged PR)`. Child 2 of #13920 (the
//! merged-PR report) replaces this with captain-owned state, at which point this
//! module goes too. Until then it is a deliberately minimal shim:
//!
//! - [`CompletedAd`] keeps the wire shape older peers already parse (`loom_claim`
//!   marker, `kind: "completed"`), so a mixed-version fleet keeps deduping.
//! - [`CompletionView`] is a TTL map keyed on local receipt time — a peer's wall
//!   clock is never trusted.
//! - [`CompletionSink`] folds inbound room events into the view and ignores every
//!   other ad kind an older peer may still publish.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::InboundEventSink;

/// Marker key present in every ad body, so an inbound room event that is a human
/// chat message (or any other `task` envelope) is cheaply told apart and ignored.
const AD_MARKER: &str = "loom_claim";

/// The single ad schema version this daemon speaks; a body carrying another
/// version is treated as not-an-ad rather than mis-parsed.
const AD_SCHEMA_VERSION: u64 = 1;

/// The `kind` value of a completion ad on the wire.
const COMPLETED_KIND: &str = "completed";

/// Env var overriding the completion dedup TTL, in seconds. Precedence
/// **env > config (`safehouse.peerCompletionTtlSecs`) > default**.
pub const COMPLETION_TTL_ENV: &str = "LOOM_PEER_COMPLETION_TTL_SECS";

/// Default completion dedup TTL: 24 hours. A completion is a one-shot historical
/// fact with no heartbeat to refresh it, and the race it guards against (two
/// hosts' `SweepExited` / `reconcile_merges_since` paths both observing the same
/// merge, or a slow host catching up after a connection hiccup) needs a window
/// well beyond one reconciliation tick. A rare double-post after it lapses is an
/// accepted paper-cut, not a correctness gate.
pub const DEFAULT_COMPLETION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Resolve the completion dedup TTL: **env > config > default**. A zero or
/// unparseable value at any layer falls through (a zero TTL would expire every
/// entry instantly and defeat the dedup).
#[must_use]
pub fn resolve_completion_ttl(repo_root: &Path) -> Duration {
    if let Some(secs) = std::env::var(COMPLETION_TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
    {
        return Duration::from_secs(secs);
    }
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let from_config = crate::config_resolver::get_path(&effective, "safehouse")
        .and_then(|s| s.get("peerCompletionTtlSecs"))
        .and_then(Value::as_u64)
        .filter(|&s| s > 0);
    from_config.map_or(DEFAULT_COMPLETION_TTL, Duration::from_secs)
}

/// "I just narrated the public-feed completion for issue #N, merged as PR #`pr`"
/// (Issue #6352, PR-keyed per Issue #6062), carried in the `body` of a
/// `task`-typed safehouse envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedAd {
    /// The issue the merged PR closed or advanced.
    pub issue: u32,
    /// A **cross-host-stable** repo identity ([`crate::host_identity::repo_slug`]),
    /// never a local absolute path.
    pub repo: String,
    /// The advertising host's identity; a daemon ignores its own ads.
    pub host: String,
    /// The advertising daemon's PID (diagnostic).
    pub pid: u32,
    /// The advertiser's RFC3339 wall-clock timestamp. **Diagnostic only**: the
    /// TTL is measured against local receipt time (clock skew).
    pub ts: String,
    /// The merged PR number. Absent on the wire from a pre-#6062 peer, which
    /// parses as `None` and is keyed as `0`.
    pub pr: Option<u32>,
}

impl CompletedAd {
    #[must_use]
    pub fn new(issue: u32, repo: String, host: String, pid: u32, ts: String, pr: u32) -> Self {
        Self {
            issue,
            repo,
            host,
            pid,
            ts,
            pr: Some(pr),
        }
    }

    /// Serialize to the JSON string carried in the envelope `body`.
    #[must_use]
    pub fn to_body_json(&self) -> String {
        json!({
            AD_MARKER: AD_SCHEMA_VERSION,
            "kind": COMPLETED_KIND,
            "issue": self.issue,
            "repo": self.repo,
            "host": self.host,
            "pid": self.pid,
            "ts": self.ts,
            "pr": self.pr,
        })
        .to_string()
    }

    /// Parse a completion ad from the raw `body` string of an inbound room event.
    /// `None` for anything else — a human chat message, a malformed payload, a
    /// wrong schema version, or any other ad `kind` an older peer still sends.
    #[must_use]
    pub fn from_body_str(body: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(body).ok()?;
        let obj = v.as_object()?;
        if obj.get(AD_MARKER).and_then(Value::as_u64)? != AD_SCHEMA_VERSION {
            return None;
        }
        if obj.get("kind").and_then(Value::as_str)? != COMPLETED_KIND {
            return None;
        }
        let issue = u32::try_from(obj.get("issue").and_then(Value::as_u64)?).ok()?;
        let repo = obj.get("repo").and_then(Value::as_str)?.to_owned();
        let host = obj.get("host").and_then(Value::as_str)?.to_owned();
        if repo.is_empty() || host.is_empty() {
            return None;
        }
        let pid = obj
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or(0);
        let ts = obj
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let pr = obj
            .get("pr")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok());
        Some(Self {
            issue,
            repo,
            host,
            pid,
            ts,
            pr,
        })
    }
}

/// Peer-narrated completions this daemon has heard, keyed `(repo, issue, pr)`,
/// each aged against the local [`Instant`] it was received.
pub struct CompletionView {
    self_host: String,
    ttl: Duration,
    completions: HashMap<(String, u32, u32), Instant>,
}

impl CompletionView {
    #[must_use]
    pub fn new(self_host: String, ttl: Duration) -> Self {
        Self {
            self_host,
            ttl,
            completions: HashMap::new(),
        }
    }

    /// Record a peer's completion ad received at local time `now`. Returns
    /// `true` when applied, `false` when ignored as this host's own ad — except
    /// that an unresolved identity ([`crate::sweep_registry::UNKNOWN_HOST`]) is
    /// never treated as "self", so two such hosts still dedup each other.
    ///
    /// A missing `pr` (a pre-#6062 peer) is keyed as `0`.
    pub fn observe_at(&mut self, ad: &CompletedAd, now: Instant) -> bool {
        let unresolved = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !unresolved {
            return false;
        }
        self.completions
            .insert((ad.repo.clone(), ad.issue, ad.pr.unwrap_or(0)), now);
        true
    }

    /// Whether `(repo, issue, pr)` has a live (non-expired) peer-narrated
    /// completion at local time `now`. Keyed on the merged PR, not the issue
    /// alone (#6062): an issue can legitimately merge more than one PR.
    #[must_use]
    pub fn is_narrated_at(&self, repo: &str, issue: u32, pr: u32, now: Instant) -> bool {
        self.completions
            .get(&(repo.to_owned(), issue, pr))
            .is_some_and(|received| now.saturating_duration_since(*received) < self.ttl)
    }

    /// Drop every expired entry at local time `now`.
    pub fn prune_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.completions
            .retain(|_, received| now.saturating_duration_since(*received) < ttl);
    }

    /// Number of tracked completions, including not-yet-pruned expired ones.
    #[must_use]
    pub fn len(&self) -> usize {
        self.completions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.completions.is_empty()
    }
}

/// The inbound consumer: parses completion ads out of room events and folds them
/// into a shared [`CompletionView`]. A non-completion event (a human chat
/// message, a narration line, any other ad kind) is silently ignored.
pub struct CompletionSink {
    view: Arc<Mutex<CompletionView>>,
}

impl CompletionSink {
    #[must_use]
    pub fn new(view: Arc<Mutex<CompletionView>>) -> Self {
        Self { view }
    }
}

impl InboundEventSink for CompletionSink {
    fn on_event(&self, event: &Value) {
        // safehoused's live push carries the message text at `envelope.body`
        // (#6249); keep the top-level `body` as a fallback for a flat emitter.
        let Some(body) = event
            .pointer("/envelope/body")
            .and_then(Value::as_str)
            .or_else(|| event.get("body").and_then(Value::as_str))
        else {
            return;
        };
        let Some(ad) = CompletedAd::from_body_str(body) else {
            return;
        };
        match self.view.lock() {
            Ok(mut view) => {
                let now = Instant::now();
                view.observe_at(&ad, now);
                view.prune_expired(now);
            }
            Err(poisoned) => {
                log::error!("safehouse: completion view mutex poisoned ({poisoned:?})");
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ad(host: &str, pr: u32) -> CompletedAd {
        CompletedAd::new(7, "loom".into(), host.into(), 1, "ts".into(), pr)
    }

    fn view() -> CompletionView {
        CompletionView::new("self-host".into(), Duration::from_secs(10))
    }

    /// A completion ad round-trips through the wire body.
    #[test]
    fn a_completion_ad_round_trips() {
        let a = ad("peer", 42);
        assert_eq!(CompletedAd::from_body_str(&a.to_body_json()), Some(a));
    }

    /// The wire shape older peers parse is unchanged: the `loom_claim` marker and
    /// `kind: "completed"`.
    #[test]
    fn the_wire_shape_matches_what_older_peers_parse() {
        let body: Value = serde_json::from_str(&ad("peer", 42).to_body_json()).unwrap();
        assert_eq!(body["loom_claim"], 1);
        assert_eq!(body["kind"], "completed");
        assert_eq!(body["pr"], 42);
    }

    /// Anything but a completion ad parses as `None`: chat, malformed JSON, a
    /// wrong version, and every retired ad kind an older peer may still send.
    #[test]
    fn everything_but_a_completion_ad_is_ignored() {
        assert_eq!(CompletedAd::from_body_str("hello"), None);
        assert_eq!(CompletedAd::from_body_str("{}"), None);
        for kind in [
            "advertise",
            "retract",
            "heartbeat",
            "filing_lock",
            "dispatch_backoff_armed",
        ] {
            let body = json!({
                "loom_claim": 1, "kind": kind, "issue": 7, "repo": "loom", "host": "peer",
            })
            .to_string();
            assert_eq!(CompletedAd::from_body_str(&body), None, "{kind}");
        }
        let v2 = json!({
            "loom_claim": 2, "kind": "completed", "issue": 7, "repo": "loom", "host": "peer",
        })
        .to_string();
        assert_eq!(CompletedAd::from_body_str(&v2), None);
    }

    /// A pre-#6062 peer's ad carries no `pr`; it parses and is keyed as `0`.
    #[test]
    fn a_missing_pr_is_keyed_as_zero() {
        let body = json!({
            "loom_claim": 1, "kind": "completed", "issue": 7, "repo": "loom", "host": "peer",
        })
        .to_string();
        let a = CompletedAd::from_body_str(&body).unwrap();
        assert_eq!(a.pr, None);
        let mut v = view();
        let now = Instant::now();
        assert!(v.observe_at(&a, now));
        assert!(v.is_narrated_at("loom", 7, 0, now));
    }

    #[test]
    fn a_peer_completion_is_narrated_until_its_ttl_lapses() {
        let mut v = view();
        let now = Instant::now();
        assert!(v.observe_at(&ad("peer", 42), now));
        assert!(v.is_narrated_at("loom", 7, 42, now + Duration::from_secs(9)));
        assert!(!v.is_narrated_at("loom", 7, 42, now + Duration::from_secs(10)));
        // Keyed on the PR: a second PR for the same issue is a distinct completion.
        assert!(!v.is_narrated_at("loom", 7, 43, now));
    }

    #[test]
    fn this_hosts_own_ad_never_suppresses_its_own_narration() {
        let mut v = view();
        let now = Instant::now();
        assert!(!v.observe_at(&ad("self-host", 42), now));
        assert!(!v.is_narrated_at("loom", 7, 42, now));
    }

    #[test]
    fn two_unresolved_identity_hosts_still_dedup_each_other() {
        let unknown = crate::sweep_registry::UNKNOWN_HOST;
        let mut v = CompletionView::new(unknown.into(), Duration::from_secs(10));
        let now = Instant::now();
        assert!(v.observe_at(&ad(unknown, 42), now));
        assert!(v.is_narrated_at("loom", 7, 42, now));
    }

    #[test]
    fn pruning_reclaims_only_expired_entries() {
        let mut v = view();
        let base = Instant::now();
        v.observe_at(&ad("peer", 1), base);
        v.observe_at(&ad("peer", 2), base + Duration::from_secs(8));
        v.prune_expired(base + Duration::from_secs(11));
        assert_eq!(v.len(), 1);
        assert!(v.is_narrated_at("loom", 7, 2, base + Duration::from_secs(11)));
    }

    /// The sink reads the envelope `body` only — `task_id`, `type` and the room
    /// never matter — so an ad folds identically whatever thread key it carries.
    #[test]
    fn the_sink_folds_a_completion_whatever_its_task_id() {
        for task_id in [Some(json!("4028")), None, Some(json!("other_99"))] {
            let mut envelope = json!({
                "v": 1, "from": "loom_daemon", "to": "*", "type": "task",
                "body": ad("peer", 42).to_body_json(),
            });
            if let Some(t) = &task_id {
                envelope["task_id"] = t.clone();
            }
            let view = Arc::new(Mutex::new(CompletionView::new(
                "self-host".into(),
                Duration::from_secs(60),
            )));
            CompletionSink::new(view.clone())
                .on_event(&json!({"event": "message", "envelope": envelope}));
            assert!(
                view.lock()
                    .unwrap()
                    .is_narrated_at("loom", 7, 42, Instant::now()),
                "task_id {task_id:?} must not change how the ad folds"
            );
        }
    }

    /// An ad kind this daemon no longer understands leaves the view untouched.
    #[test]
    fn the_sink_ignores_a_retired_ad_kind() {
        let view = Arc::new(Mutex::new(view()));
        let body = json!({
            "loom_claim": 1, "kind": "advertise", "issue": 7, "repo": "loom", "host": "peer",
        })
        .to_string();
        CompletionSink::new(view.clone())
            .on_event(&json!({"event": "message", "envelope": {"body": body}}));
        assert!(view.lock().unwrap().is_empty());
    }

    /// The completion ad never carries a thread key, so safehoused sends it as a
    /// plain top-level message rather than threading machine traffic.
    #[test]
    fn a_completion_ad_sends_without_a_task_id() {
        let a = ad("peer", 42);
        let env = super::super::completion_ad_to_envelope(&a);
        assert_eq!(env.task_id, None);
        let req = super::super::build_send_request(&env, 1, Some("room")).unwrap();
        assert!(req.get("task_id").is_none(), "{req}");
        assert_eq!(CompletedAd::from_body_str(req["body"].as_str().unwrap()), Some(a));
    }
}
