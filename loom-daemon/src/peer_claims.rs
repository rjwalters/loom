//! Pure cross-host peer-claim view + TTL expiry (Issue #4028, Phase 1 — soft
//! claim advertisement over the safehouse room).
//!
//! # What this is
//!
//! On a multi-host deployment (one daemon per machine, shared repo backlog) the
//! only cross-host coordination for claiming an issue is the forge label
//! (`loom:issue → loom:building`), whose flip is **non-atomic** — two hosts can
//! both read `loom:issue` and dispatch before either flip propagates
//! ([`crate::sweep_registry::SweepRegistry::flip_label_to_building`] is an
//! unconditional `--remove-label`/`--add-label`, no CAS). This module implements
//! the **soft claim**: each daemon advertises "claiming issue #N on host H" into
//! the shared safehouse room the moment it decides to dispatch, and every peer
//! keeps a **peer-claim view** so it can back off on an issue a peer is already
//! building — far faster than the label propagates.
//!
//! # Soft claim, not a mutex (read this)
//!
//! A room broadcast is eventually consistent, so this is a **fast backoff**, not
//! a lock: two hosts advertising near-simultaneously still race. Advertisement
//! *shrinks* the collision window; it does not close it. The atomic authority for
//! the final claim (a real cross-host CAS) is **Phase 2**, out of scope here — see
//! #4028's "Design options for the atomic authority" section.
//!
//! **#5789 upgraded the two existing collision signals from detection-only into
//! enforcement**, without landing Phase 2's atomic CAS. `SweepRegistry::dispatch`
//! now (a) checks THIS [`PeerClaimView`] for a live peer claim before even
//! acquiring its local claim lock, and (b) backs off when the opt-in pre-flip
//! forge-label read (`classify_preflip_labels`, #4085) confirms a peer already
//! flipped `loom:building` first — in both cases retracting any advertisement
//! and lock this host had already applied instead of "detecting and continuing
//! unchanged." This tightens the race window as far as the soft signals below
//! allow; it is still not a mutex, and two hosts advertising within the same
//! instant (no peer-claim view attached, or the ad simply hasn't arrived yet)
//! can still both pass every guard and duplicate a dispatch — closing THAT
//! residual gap is exactly what Phase 2's atomic CAS remains for.
//!
//! # Primary in-flight signal on safehouse hosts (#4431)
//!
//! Since #4431 the peer claim is the **primary fast signal** that an issue is
//! in flight on a safehouse-enabled fleet: the forge `loom:building` label
//! remains the durable audit trail, but its reconciliation pass demotes to a
//! slow healing cadence
//! ([`crate::claim_reconciliation::DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS`]).
//! The contract that makes this safe is **re-advertisement**: the dispatch-time
//! ad is no longer one-shot — `sweep_registry::readvertise_peer_claims`
//! re-publishes every live sweep's claim each reaper tick (default 30s, well
//! under the TTL), and [`PeerClaimView::observe_at`] refreshes the local
//! expiry clock on every repeat `Advertise`. A live claim therefore never
//! expires from peers' views mid-run, while a crashed host's claims still free
//! within one TTL of its last heartbeat — the promotion changes how *long* a
//! claim stays visible, not the "soft, eventually-consistent" nature above.
//!
//! # Which room claim ads ride: the signal room (#4225)
//!
//! #4225 splits narration across a low-volume **signal** room and per-repo
//! **firehose** rooms, routing by attention class: `task` envelopes (which a
//! claim ad is — see [`crate::safehouse::claim_ad_to_envelope`]) go to the repo
//! firehose. Claim ads are the **one deliberate exception** and stay on the
//! signal room, because coordination has a membership requirement narration does
//! not: firehose rooms are created *lazily* by whichever host narrates a repo
//! first, and each host runs its own bot account, so a peer that never joined
//! `fleet-<repo>` would silently stop seeing ads there — disabling dedup with no
//! error anywhere. Every host's bot is already in the signal room. The full
//! rationale (and the matching reader behavior) is on
//! `safehouse::run_coordination`; nothing in *this* module is room-aware, which
//! is why the exception costs no code here.
//!
//! # TTL is measured against LOCAL receipt, never the advertiser's clock
//!
//! A crashed peer must not starve an issue forever, so every peer claim expires
//! after a bounded TTL ([`DEFAULT_PEER_CLAIM_TTL`], overridable). The expiry
//! clock is the **local [`Instant`] at which this daemon observed the ad**, not
//! the advertiser's wall-clock timestamp — wall clocks are not comparable across
//! hosts (clock skew), so the advertised `ts` is carried for diagnostics only and
//! is **never** used for TTL math. A peer can also retract its claim early (on
//! its sweep exiting/crashing) so the issue frees before the TTL lapses.
//!
//! # Purity
//!
//! [`PeerClaimView`] and [`ClaimAd`] have **no socket dependency** — the decide
//! logic (observe / retract / expire) is fully unit-testable with an injected
//! [`Instant`], mirroring the `decide`/`plan` split in
//! [`crate::claim_reconciliation`]. The socket transport lives entirely in
//! [`crate::safehouse`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

// ============================================================================
// Constants
// ============================================================================

/// Marker key present in every claim-advertisement body payload, so an inbound
/// room event that is a human chat message (or any non-claim `task` envelope) is
/// cheaply distinguished from a claim and ignored.
pub const PEER_CLAIM_MARKER: &str = "loom_claim";

/// The single claim-payload schema version this daemon speaks. A body carrying a
/// different version is rejected (treated as not-a-claim) rather than
/// mis-parsed.
pub const CLAIM_SCHEMA_VERSION: u64 = 1;

/// Env var overriding the peer-claim TTL, in seconds. Precedence
/// **env > config (`safehouse.peerClaimTtlSecs`) > default**.
pub const PEER_CLAIM_TTL_ENV: &str = "LOOM_PEER_CLAIM_TTL_SECS";

/// Default peer-claim TTL. Chosen at **2×** the default work-finder tick
/// interval ([`crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS`] = 60s) so
/// a peer's soft claim survives at least one full tick — long enough for the
/// peer's forge label flip to propagate and take over as the durable signal —
/// while still freeing a crashed peer's issue promptly.
pub const DEFAULT_PEER_CLAIM_TTL: Duration = Duration::from_secs(120);

// ============================================================================
// Claim advertisement
// ============================================================================

/// Whether an advertisement asserts a claim or retracts one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// "I am dispatching issue #N" — record a peer claim.
    Advertise,
    /// "My sweep for issue #N exited/crashed" — retract my earlier claim early
    /// (before its TTL would lapse).
    Retract,
}

impl ClaimKind {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            ClaimKind::Advertise => "advertise",
            ClaimKind::Retract => "retract",
        }
    }

    #[must_use]
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "advertise" => Some(ClaimKind::Advertise),
            "retract" => Some(ClaimKind::Retract),
            _ => None,
        }
    }
}

/// One claim advertisement, carried in the `body` of a `task`-typed safehouse
/// envelope (Gap 2 of #4028: the envelope `type` enum is closed and owned by the
/// safehouse repo, so a claim rides a `task` envelope with the issue number as
/// `task_id` rather than inventing a fifth type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimAd {
    pub kind: ClaimKind,
    /// The issue being claimed / retracted.
    pub issue: u32,
    /// A **cross-host-stable** repo identity (see [`repo_slug`]) — NOT a local
    /// absolute path, which differs per host for the same logical repo.
    pub repo: String,
    /// The advertising host's identity (see
    /// [`crate::sweep_registry::host_identity`]). Used for self-claim
    /// recognition (a daemon ignores its own ads) and to scope a retraction (a
    /// host may only retract its own claim).
    pub host: String,
    /// The advertising daemon's PID (diagnostic).
    pub pid: u32,
    /// The advertiser's RFC3339 wall-clock timestamp. **Diagnostic only** — TTL
    /// is measured against local receipt time, never this (clock skew).
    pub ts: String,
}

impl ClaimAd {
    #[must_use]
    pub fn advertise(issue: u32, repo: String, host: String, pid: u32, ts: String) -> Self {
        Self {
            kind: ClaimKind::Advertise,
            issue,
            repo,
            host,
            pid,
            ts,
        }
    }

    #[must_use]
    pub fn retract(issue: u32, repo: String, host: String, pid: u32, ts: String) -> Self {
        Self {
            kind: ClaimKind::Retract,
            issue,
            repo,
            host,
            pid,
            ts,
        }
    }

    /// Serialize to the JSON string carried in the envelope `body`.
    #[must_use]
    pub fn to_body_json(&self) -> String {
        json!({
            PEER_CLAIM_MARKER: CLAIM_SCHEMA_VERSION,
            "kind": self.kind.as_str(),
            "issue": self.issue,
            "repo": self.repo,
            "host": self.host,
            "pid": self.pid,
            "ts": self.ts,
        })
        .to_string()
    }

    /// Parse a claim from an already-decoded `body` JSON value. Returns `None`
    /// for any non-claim or malformed payload (missing marker, wrong version,
    /// missing/ill-typed fields) — this is the **malformed-envelope rejection**
    /// AC: a bad payload is dropped, never panics, never mis-claims.
    #[must_use]
    pub fn from_body_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        // Marker + version gate: anything without the exact marker/version is
        // "not a claim" (e.g. a human chat message on the same room).
        if obj.get(PEER_CLAIM_MARKER).and_then(Value::as_u64)? != CLAIM_SCHEMA_VERSION {
            return None;
        }
        let kind = ClaimKind::from_str(obj.get("kind").and_then(Value::as_str)?)?;
        let issue = u32::try_from(obj.get("issue").and_then(Value::as_u64)?).ok()?;
        let repo = obj.get("repo").and_then(Value::as_str)?.to_owned();
        let host = obj.get("host").and_then(Value::as_str)?.to_owned();
        // `pid` and `ts` are diagnostic; tolerate their absence rather than
        // rejecting an otherwise-valid claim.
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
        if repo.is_empty() || host.is_empty() {
            return None;
        }
        Some(Self {
            kind,
            issue,
            repo,
            host,
            pid,
            ts,
        })
    }

    /// Parse a claim from the raw `body` string of an inbound room event.
    #[must_use]
    pub fn from_body_str(body: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(body).ok()?;
        Self::from_body_value(&v)
    }
}

// ============================================================================
// Repo identity + TTL resolution
// ============================================================================

/// A **cross-host-stable** repo identity for keying peer claims.
///
/// A local absolute `workspace_root` differs per host for the same logical repo
/// (`/Users/a/loom` vs `/home/b/loom`), so it cannot key claims across hosts.
/// This resolves, in order: `$LOOM_REPO` (the forge `owner/repo` slug the daemon
/// already threads through `gh --repo`), else the final path component
/// (directory basename — two clones of `loom` share `"loom"`), else the full
/// path as a last resort.
///
/// # Known limitation (documented, matches the frozen-taxonomy phase-b tradeoff)
///
/// Two *different* repos that share a directory basename **and** are backed by
/// the same shared room could cross-suppress issue #N. This is the same
/// issue-number-keying limitation already accepted for the `sweep.issue.{N}.*`
/// narration taxonomy (#3929); set `$LOOM_REPO` to the `owner/repo` slug to make
/// the key precise. Because the claim is a soft, TTL-bounded, fail-open backoff
/// (never a hard lock), a false suppression self-heals within one TTL.
#[must_use]
pub fn repo_slug(workspace_root: &Path) -> String {
    if let Ok(v) = std::env::var("LOOM_REPO") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_owned();
        }
    }
    workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| workspace_root.display().to_string())
}

/// Resolve the peer-claim TTL with precedence **env > config
/// (`safehouse.peerClaimTtlSecs`) > default([`DEFAULT_PEER_CLAIM_TTL`])**. A
/// zero/unparseable value at any layer falls through (a zero TTL would make every
/// claim expire instantly, defeating the backoff).
#[must_use]
pub fn resolve_peer_claim_ttl(repo_root: &Path) -> Duration {
    if let Some(secs) = std::env::var(PEER_CLAIM_TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
    {
        return Duration::from_secs(secs);
    }
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let from_config = crate::config_resolver::get_path(&effective, "safehouse")
        .and_then(|s| s.get("peerClaimTtlSecs"))
        .and_then(Value::as_u64)
        .filter(|&s| s > 0);
    from_config.map_or(DEFAULT_PEER_CLAIM_TTL, Duration::from_secs)
}

// ============================================================================
// Peer-claim view
// ============================================================================

/// One live peer claim on an issue.
#[derive(Debug, Clone)]
struct ClaimEntry {
    /// Which host holds the claim (for retraction scoping + diagnostics).
    host: String,
    /// Advertising daemon PID (diagnostic).
    #[allow(dead_code)]
    pid: u32,
    /// **Local** [`Instant`] at which this daemon observed the ad — the TTL
    /// clock. Never the advertiser's wall clock.
    received_at: Instant,
}

/// One live peer claim, rendered for external observability (Issue #5921):
/// `loom-daemon status`'s peer-claim section and the `loom-daemon peer-claims`
/// subcommand both consume this rather than reaching into [`PeerClaimView`]'s
/// private map.
#[derive(Debug, Clone)]
pub struct PeerClaimEntrySnapshot {
    /// The cross-host-stable repo identity (see [`repo_slug`]) this claim was
    /// advertised under.
    pub repo: String,
    /// The claimed issue number.
    pub issue: u32,
    /// The host holding the claim.
    pub host: String,
    /// How long this entry has left before it expires from THIS daemon's
    /// view, measured against the same local receipt clock
    /// [`PeerClaimView::is_claimed_at`] uses. Zero when already expired (a
    /// snapshot can observe an entry the instant before
    /// [`PeerClaimView::prune_expired`] next removes it).
    pub remaining_ttl_secs: u64,
}

/// The four transport counters (Issue #5921): so a silent peer-claim view is
/// distinguishable from a genuinely quiet fleet. Each counter is monotonic
/// for the lifetime of the [`PeerClaimView`] (i.e. the daemon process) —
/// `status`/`peer-claims` report the running total, not a per-tick delta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerClaimCounters {
    /// How many times THIS host published a [`ClaimKind::Advertise`] — one
    /// per dispatch-time advertisement ([`crate::sweep_registry::SweepRegistry::publish_peer_claim`])
    /// plus one per live sweep on every reaper re-advertisement tick
    /// ([`crate::sweep_registry::SweepRegistry::readvertise_peer_claims`],
    /// #4431). Counted regardless of whether the outbound channel actually
    /// had a live consumer — this is "did we attempt to advertise", not "did
    /// the room confirm delivery" (the transport is fire-and-forget by
    /// design).
    pub advertised: u64,
    /// How many **peer** (non-self) advertisements/retractions this daemon
    /// has accepted via [`PeerClaimView::observe_at`]. A daemon stuck at
    /// zero while peers are known to be dispatching is exactly the silent
    /// re-advertisement-path failure #5921 exists to make diagnosable.
    pub received: u64,
    /// How many entries [`PeerClaimView::prune_expired`] has removed for
    /// having aged past the TTL with no re-advertisement — the crash-release
    /// path firing.
    pub expired: u64,
    /// How many times `SweepRegistry::dispatch` backed off because THIS
    /// view showed a live peer claim on the issue being dispatched (the
    /// #5789 enforcement path) — the proof the mechanism actually prevented
    /// a duplicate, not just observed one.
    pub dispatch_skipped: u64,
}

/// The set of issues peer hosts have advertised as in-flight, with per-entry TTL
/// expiry. Pure and socket-free: [`crate::safehouse`]'s inbound read task feeds
/// it via [`observe_at`](Self::observe_at); the work-finder queries it via
/// [`claimed_issues_at`](Self::claimed_issues_at).
#[derive(Debug)]
pub struct PeerClaimView {
    /// This daemon's own host identity — ads from this host are ignored
    /// (self-claim recognition, mirroring the `LOOM_SWEEP_CLAIM_OWNED` self-skip
    /// so a daemon never backs off on its own advertisement).
    self_host: String,
    ttl: Duration,
    /// Keyed by `(repo_slug, issue)` so two managed repos' issue #N never
    /// collide.
    claims: HashMap<(String, u32), ClaimEntry>,
    /// Transport counters (Issue #5921) — see [`PeerClaimCounters`].
    counters: PeerClaimCounters,
}

impl PeerClaimView {
    #[must_use]
    pub fn new(self_host: String, ttl: Duration) -> Self {
        Self {
            self_host,
            ttl,
            claims: HashMap::new(),
            counters: PeerClaimCounters::default(),
        }
    }

    /// This daemon's own host identity (self-claim recognition key).
    #[must_use]
    pub fn self_host(&self) -> &str {
        &self.self_host
    }

    /// The configured TTL.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Observe an inbound advertisement at local time `now`.
    ///
    /// Returns `true` when the ad was applied (a peer's), `false` when it was
    /// ignored because it originated from **this** host (self-claim
    /// recognition). A [`ClaimKind::Retract`] removes the peer's claim only when
    /// the recorded holder matches the retracting host (a host may not retract
    /// another host's claim).
    ///
    /// # `UNKNOWN_HOST` never self-matches (Issue #5063)
    ///
    /// [`crate::sweep_registry::host_identity`] falls back to
    /// [`crate::sweep_registry::UNKNOWN_HOST`] when it cannot resolve a real
    /// identity. If that sentinel were allowed to self-match here, two hosts
    /// that both failed identity resolution would advertise the identical
    /// string and each would silently ignore the other's claim as its own —
    /// exactly the failure mode this module exists to prevent, and directly
    /// relevant to the cross-host duplicate dispatch investigated in #5017.
    /// So an ad naming [`crate::sweep_registry::UNKNOWN_HOST`] is **always**
    /// treated as a genuine peer claim, even when this daemon's own identity
    /// is also unresolved — recognizing a possibly-own ad as a peer's is the
    /// safe direction (worst case: a harmless extra backoff on an issue this
    /// daemon already holds via its own registry entry); silently ignoring a
    /// genuine peer's claim is not.
    pub fn observe_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false; // never back off on our own advertisement
        }
        let key = (ad.repo.clone(), ad.issue);
        match ad.kind {
            ClaimKind::Advertise => {
                self.claims.insert(
                    key,
                    ClaimEntry {
                        host: ad.host.clone(),
                        pid: ad.pid,
                        received_at: now,
                    },
                );
            }
            ClaimKind::Retract => {
                if self.claims.get(&key).is_some_and(|e| e.host == ad.host) {
                    self.claims.remove(&key);
                }
            }
        }
        // #5921: only genuine peer traffic counts as "received" — an ignored
        // self-ad returns `false` above, before this line, so it never
        // inflates the counter.
        self.counters.received += 1;
        true
    }

    /// Whether `issue` in `repo` currently has a **live** (non-expired) peer
    /// claim at local time `now`.
    #[must_use]
    pub fn is_claimed_at(&self, repo: &str, issue: u32, now: Instant) -> bool {
        self.claims
            .get(&(repo.to_owned(), issue))
            .is_some_and(|e| now.saturating_duration_since(e.received_at) < self.ttl)
    }

    /// The set of issues in `repo` with a live peer claim at local time `now` —
    /// the work-finder's per-tick skip set.
    #[must_use]
    pub fn claimed_issues_at(&self, repo: &str, now: Instant) -> HashSet<u32> {
        self.claims
            .iter()
            .filter(|((r, _), e)| {
                r == repo && now.saturating_duration_since(e.received_at) < self.ttl
            })
            .map(|((_, issue), _)| *issue)
            .collect()
    }

    /// Drop every expired entry at local time `now`. Called opportunistically so
    /// the map does not grow without bound from crashed peers.
    pub fn prune_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        let before = self.claims.len();
        self.claims
            .retain(|_, e| now.saturating_duration_since(e.received_at) < ttl);
        // #5921: count every entry this pass actually removed for having aged
        // out — the crash-release path firing, distinguishable from a view
        // that is merely empty because nothing was ever advertised.
        self.counters.expired += (before - self.claims.len()) as u64;
    }

    /// Number of tracked claims (test/observability aid; includes not-yet-pruned
    /// expired entries).
    #[must_use]
    pub fn len(&self) -> usize {
        self.claims.len()
    }

    /// Whether the view tracks no claims.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }

    /// Record that THIS host published a [`ClaimKind::Advertise`] (Issue
    /// #5921) — called by [`crate::sweep_registry::SweepRegistry::publish_peer_claim`]
    /// for every outbound advertise, including each reaper re-advertisement
    /// heartbeat (#4431). Fire-and-forget by design: counted regardless of
    /// whether the outbound channel had a live consumer.
    pub fn record_advertised(&mut self) {
        self.counters.advertised += 1;
    }

    /// Record that a dispatch was backed off because this view showed a live
    /// peer claim on the issue being dispatched (Issue #5921, the #5789
    /// enforcement path) — called from
    /// [`crate::sweep_registry::SweepRegistry::dispatch`]'s peer-claim guard.
    pub fn record_dispatch_skipped(&mut self) {
        self.counters.dispatch_skipped += 1;
    }

    /// The running transport counters (Issue #5921) — see [`PeerClaimCounters`].
    #[must_use]
    pub fn counters(&self) -> PeerClaimCounters {
        self.counters
    }

    /// Every tracked claim, live or not-yet-pruned-expired, rendered with its
    /// remaining TTL at local time `now` (Issue #5921) — the data
    /// `loom-daemon status`'s peer-claim section and `loom-daemon
    /// peer-claims` both render. Unlike [`Self::claimed_issues_at`] this is
    /// NOT scoped to one repo and does not filter out expired entries (a
    /// `remaining_ttl_secs` of `0` reads as "expired, not yet pruned" rather
    /// than silently vanishing from the view an operator is inspecting).
    /// Order is unspecified (backed by a `HashMap`); callers that need a
    /// stable order should sort.
    #[must_use]
    pub fn entries_at(&self, now: Instant) -> Vec<PeerClaimEntrySnapshot> {
        self.claims
            .iter()
            .map(|((repo, issue), entry)| {
                let age = now.saturating_duration_since(entry.received_at);
                let remaining_ttl_secs = self.ttl.saturating_sub(age).as_secs();
                PeerClaimEntrySnapshot {
                    repo: repo.clone(),
                    issue: *issue,
                    host: entry.host.clone(),
                    remaining_ttl_secs,
                }
            })
            .collect()
    }

    /// Render into the wire [`crate::types::PeerClaimStatus`] shape consumed
    /// by `DaemonStatusReport` (Issue #5921) — mirrors
    /// [`crate::safehouse::SafehouseState::to_status`]'s pattern of a
    /// snapshot-owning type converting itself into the wire DTO.
    #[must_use]
    pub fn to_status(&self, now: Instant) -> crate::types::PeerClaimStatus {
        let mut entries: Vec<crate::types::PeerClaimEntryStatus> = self
            .entries_at(now)
            .into_iter()
            .map(|e| crate::types::PeerClaimEntryStatus {
                repo: e.repo,
                issue: e.issue,
                host: e.host,
                remaining_ttl_secs: e.remaining_ttl_secs,
            })
            .collect();
        // Deterministic rendering (the backing map has no inherent order):
        // by repo, then issue, so a repeated `status`/`peer-claims` call
        // against an unchanged view prints identically.
        entries.sort_by(|a, b| a.repo.cmp(&b.repo).then_with(|| a.issue.cmp(&b.issue)));
        let c = self.counters();
        crate::types::PeerClaimStatus {
            self_host: self.self_host.clone(),
            ttl_secs: self.ttl.as_secs(),
            entries,
            advertised: c.advertised,
            received: c.received,
            expired: c.expired,
            dispatch_skipped: c.dispatch_skipped,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ad(kind: ClaimKind, issue: u32, repo: &str, host: &str) -> ClaimAd {
        ClaimAd {
            kind,
            issue,
            repo: repo.to_owned(),
            host: host.to_owned(),
            pid: 42,
            ts: "2026-07-28T00:00:00Z".to_owned(),
        }
    }

    // ---- ClaimAd JSON round-trip + malformed rejection ----

    #[test]
    fn body_json_round_trips() {
        let a = ClaimAd::advertise(4028, "loom".into(), "maple".into(), 123, "ts".into());
        let parsed = ClaimAd::from_body_str(&a.to_body_json()).unwrap();
        assert_eq!(parsed, a);

        let r = ClaimAd::retract(7, "loom".into(), "birch".into(), 9, "ts".into());
        let parsed = ClaimAd::from_body_str(&r.to_body_json()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn malformed_bodies_are_rejected_not_panicked() {
        // Not JSON.
        assert!(ClaimAd::from_body_str("not json").is_none());
        // JSON but no marker (e.g. a human chat message sharing the room).
        assert!(ClaimAd::from_body_str(r#"{"body":"hello human"}"#).is_none());
        // Marker present but wrong version.
        assert!(ClaimAd::from_body_str(
            r#"{"loom_claim":999,"kind":"advertise","issue":1,"repo":"r","host":"h"}"#
        )
        .is_none());
        // Marker + version but missing required field (repo).
        assert!(ClaimAd::from_body_str(
            r#"{"loom_claim":1,"kind":"advertise","issue":1,"host":"h"}"#
        )
        .is_none());
        // Empty host is rejected.
        assert!(ClaimAd::from_body_str(
            r#"{"loom_claim":1,"kind":"advertise","issue":1,"repo":"r","host":""}"#
        )
        .is_none());
        // Unknown kind.
        assert!(ClaimAd::from_body_str(
            r#"{"loom_claim":1,"kind":"steal","issue":1,"repo":"r","host":"h"}"#
        )
        .is_none());
    }

    #[test]
    fn pid_and_ts_are_optional_diagnostics() {
        let a = ClaimAd::from_body_str(
            r#"{"loom_claim":1,"kind":"advertise","issue":5,"repo":"r","host":"h"}"#,
        )
        .unwrap();
        assert_eq!(a.pid, 0);
        assert_eq!(a.ts, "");
        assert_eq!(a.issue, 5);
    }

    // ---- TTL expiry ----

    #[test]
    fn claim_expires_after_ttl_measured_from_local_receipt() {
        let ttl = Duration::from_secs(100);
        let mut view = PeerClaimView::new("me".into(), ttl);
        let base = Instant::now();
        assert!(view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base));

        // Within the window: still claimed.
        assert!(view.is_claimed_at("loom", 1, base + Duration::from_secs(99)));
        // At/after the TTL: expired.
        assert!(!view.is_claimed_at("loom", 1, base + Duration::from_secs(100)));
        assert!(!view.is_claimed_at("loom", 1, base + Duration::from_secs(101)));
    }

    /// #4431 crux: a repeat `Advertise` for the same (repo, issue) must
    /// refresh the local expiry clock, so a claim re-advertised by the
    /// holder's reaper heartbeat survives arbitrarily far past the original
    /// TTL — while still expiring one TTL after the LAST heartbeat (the
    /// crash-release property).
    #[test]
    fn readvertisement_refreshes_ttl_so_a_live_claim_never_expires_mid_run() {
        let ttl = Duration::from_secs(120);
        let mut view = PeerClaimView::new("me".into(), ttl);
        let base = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 7, "loom", "peer"), base);

        // Re-advertised at t+90 (inside the window): the clock restarts.
        view.observe_at(
            &ad(ClaimKind::Advertise, 7, "loom", "peer"),
            base + Duration::from_secs(90),
        );
        // t+150 is past the ORIGINAL expiry (t+120) but inside the refreshed
        // window (t+90 + 120 = t+210): still claimed.
        assert!(view.is_claimed_at("loom", 7, base + Duration::from_secs(150)));
        assert!(view.is_claimed_at("loom", 7, base + Duration::from_secs(209)));
        // One TTL after the LAST heartbeat: expired — a crashed holder still
        // frees the issue promptly.
        assert!(!view.is_claimed_at("loom", 7, base + Duration::from_secs(210)));
    }

    #[test]
    fn claimed_issues_set_drops_expired_and_scopes_by_repo() {
        let ttl = Duration::from_secs(100);
        let mut view = PeerClaimView::new("me".into(), ttl);
        let base = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base);
        view.observe_at(
            &ad(ClaimKind::Advertise, 2, "loom", "peer"),
            base + Duration::from_secs(50),
        );
        view.observe_at(&ad(ClaimKind::Advertise, 9, "other", "peer"), base);

        // At base+60: #1 (age 60) live, #2 (age 10) live, but scoped to "loom".
        let at = base + Duration::from_secs(60);
        let set = view.claimed_issues_at("loom", at);
        assert_eq!(set, HashSet::from([1, 2]));
        // The "other" repo's claim never appears under "loom".
        assert_eq!(view.claimed_issues_at("other", at), HashSet::from([9]));

        // At base+120: #1 (age 120) expired, #2 (age 70) live.
        let later = base + Duration::from_secs(120);
        assert_eq!(view.claimed_issues_at("loom", later), HashSet::from([2]));
    }

    #[test]
    fn prune_expired_reclaims_map_space() {
        let ttl = Duration::from_secs(10);
        let mut view = PeerClaimView::new("me".into(), ttl);
        let base = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base);
        assert_eq!(view.len(), 1);
        view.prune_expired(base + Duration::from_secs(11));
        assert_eq!(view.len(), 0);
        assert!(view.is_empty());
    }

    // ---- self-claim recognition ----

    #[test]
    fn own_advertisement_is_never_backed_off_on() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(100));
        let now = Instant::now();
        // An ad from our own host is ignored.
        assert!(!view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "me"), now));
        assert!(!view.is_claimed_at("loom", 1, now));
        assert!(view.is_empty());

        // A peer's ad for the same issue IS recorded.
        assert!(view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), now));
        assert!(view.is_claimed_at("loom", 1, now));
    }

    /// Issue #5063: two hosts that both fail identity resolution
    /// (`host_identity()`'s `"unknown-host"` fallback) must NOT recognize
    /// each other's claims as their own. If they did, self-recognition would
    /// silently defeat the entire peer-claim mechanism between exactly the
    /// two hosts most in need of it (the ones with no real identity).
    #[test]
    fn unknown_host_never_self_matches_even_when_local_identity_is_also_unresolved() {
        let mut view = PeerClaimView::new(
            crate::sweep_registry::UNKNOWN_HOST.into(),
            Duration::from_secs(100),
        );
        let now = Instant::now();

        // A peer that ALSO failed identity resolution advertises the same
        // "unknown-host" string this daemon uses for itself. It must still be
        // recorded as a genuine peer claim, not ignored as "our own ad".
        assert!(view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", crate::sweep_registry::UNKNOWN_HOST),
            now
        ));
        assert!(view.is_claimed_at("loom", 1, now));

        // Retraction from the same unresolved-identity peer still works
        // (scoped correctly, not somehow conflated with "self").
        assert!(view.observe_at(
            &ad(ClaimKind::Retract, 1, "loom", crate::sweep_registry::UNKNOWN_HOST),
            now
        ));
        assert!(!view.is_claimed_at("loom", 1, now));
    }

    /// A named self-host is unaffected by the `"unknown-host"` carve-out: an
    /// ad from a peer that happens to be *named* `"unknown-host"` is still
    /// just a normal peer when this daemon's own identity resolved to
    /// something else entirely.
    #[test]
    fn unknown_host_peer_ad_is_recorded_when_self_identity_resolved_normally() {
        let mut view = PeerClaimView::new("robb-studio".into(), Duration::from_secs(100));
        let now = Instant::now();
        assert!(view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", crate::sweep_registry::UNKNOWN_HOST),
            now
        ));
        assert!(view.is_claimed_at("loom", 1, now));
    }

    // ---- retraction ----

    #[test]
    fn peer_retract_frees_the_issue_early() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
        let now = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), now);
        assert!(view.is_claimed_at("loom", 1, now));

        // The same peer retracting (its sweep exited/crashed) frees it well
        // before the long TTL would.
        view.observe_at(&ad(ClaimKind::Retract, 1, "loom", "peer"), now);
        assert!(!view.is_claimed_at("loom", 1, now));
    }

    #[test]
    fn a_host_cannot_retract_another_hosts_claim() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
        let now = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peerA"), now);
        // A different host's retract must not remove peerA's claim.
        view.observe_at(&ad(ClaimKind::Retract, 1, "loom", "peerB"), now);
        assert!(view.is_claimed_at("loom", 1, now));
        // peerA's own retract does.
        view.observe_at(&ad(ClaimKind::Retract, 1, "loom", "peerA"), now);
        assert!(!view.is_claimed_at("loom", 1, now));
    }

    // ---- counters + entries_at / to_status (Issue #5921) ----

    #[test]
    fn observe_at_counts_received_only_for_genuine_peer_ads() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(100));
        let now = Instant::now();

        // Our own ad is ignored — never counted as "received".
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "me"), now);
        assert_eq!(view.counters().received, 0);

        // A peer's advertise and a peer's retract both count as received —
        // the counter answers "did traffic arrive", regardless of kind.
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), now);
        assert_eq!(view.counters().received, 1);
        view.observe_at(&ad(ClaimKind::Retract, 1, "loom", "peer"), now);
        assert_eq!(view.counters().received, 2);
    }

    #[test]
    fn prune_expired_counts_only_entries_actually_removed() {
        let ttl = Duration::from_secs(10);
        let mut view = PeerClaimView::new("me".into(), ttl);
        let base = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base);
        view.observe_at(&ad(ClaimKind::Advertise, 2, "loom", "peer"), base);

        // Nothing expired yet: prune is a no-op, counter unchanged.
        view.prune_expired(base + Duration::from_secs(5));
        assert_eq!(view.counters().expired, 0);

        // Both entries have now aged out.
        view.prune_expired(base + Duration::from_secs(11));
        assert_eq!(view.counters().expired, 2);

        // A second prune with nothing left to remove must not double-count.
        view.prune_expired(base + Duration::from_secs(12));
        assert_eq!(view.counters().expired, 2);
    }

    #[test]
    fn record_advertised_and_dispatch_skipped_are_independent_counters() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(100));
        assert_eq!(view.counters(), PeerClaimCounters::default());

        view.record_advertised();
        view.record_advertised();
        view.record_dispatch_skipped();

        let c = view.counters();
        assert_eq!(c.advertised, 2);
        assert_eq!(c.dispatch_skipped, 1);
        assert_eq!(c.received, 0);
        assert_eq!(c.expired, 0);
    }

    #[test]
    fn entries_at_reports_remaining_ttl_and_zero_floors_past_expiry() {
        let ttl = Duration::from_secs(100);
        let mut view = PeerClaimView::new("me".into(), ttl);
        let base = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 42, "loom", "peer"), base);

        let entries = view.entries_at(base + Duration::from_secs(30));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].issue, 42);
        assert_eq!(entries[0].repo, "loom");
        assert_eq!(entries[0].host, "peer");
        assert_eq!(entries[0].remaining_ttl_secs, 70);

        // Past the TTL (but before any `prune_expired` call removes it): the
        // remaining TTL floors at zero rather than underflowing/panicking,
        // and the entry is still visible — `entries_at` deliberately does not
        // filter out expired-but-not-yet-pruned entries (see its doc comment).
        let entries = view.entries_at(base + Duration::from_secs(500));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].remaining_ttl_secs, 0);
    }

    #[test]
    fn to_status_renders_sorted_entries_and_the_full_counter_set() {
        let ttl = Duration::from_secs(120);
        let mut view = PeerClaimView::new("self-host".into(), ttl);
        let base = Instant::now();
        // Insert out of (repo, issue) order to exercise the sort.
        view.observe_at(&ad(ClaimKind::Advertise, 9, "loom", "peer-b"), base);
        view.observe_at(&ad(ClaimKind::Advertise, 3, "loom", "peer-a"), base);
        view.observe_at(&ad(ClaimKind::Advertise, 1, "another-repo", "peer-c"), base);
        view.record_advertised();
        view.record_dispatch_skipped();

        let status = view.to_status(base + Duration::from_secs(10));
        assert_eq!(status.self_host, "self-host");
        assert_eq!(status.ttl_secs, 120);
        assert_eq!(status.entries.len(), 3);
        // Sorted by (repo, issue): "another-repo" < "loom", then issue 3 < 9.
        assert_eq!(status.entries[0].repo, "another-repo");
        assert_eq!(status.entries[1].repo, "loom");
        assert_eq!(status.entries[1].issue, 3);
        assert_eq!(status.entries[2].issue, 9);
        assert_eq!(status.received, 3);
        assert_eq!(status.advertised, 1);
        assert_eq!(status.dispatch_skipped, 1);
        assert_eq!(status.expired, 0);
    }

    // ---- repo_slug ----

    #[test]
    fn repo_slug_prefers_env_then_basename() {
        // Env override wins.
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        assert_eq!(repo_slug(Path::new("/anything/here")), "rjwalters/loom");
        std::env::remove_var("LOOM_REPO");
        // Basename fallback is cross-host-stable for the same repo.
        assert_eq!(repo_slug(Path::new("/Users/a/loom")), "loom");
        assert_eq!(repo_slug(Path::new("/home/b/loom")), "loom");
    }

    // ---- integration-shaped: two hosts, one issue, exactly one proceeds ----

    #[test]
    fn two_hosts_one_issue_second_host_backs_off_deterministically() {
        // Host A dispatches issue 100 and advertises. Host B, before dispatching
        // its own, observes A's ad and must treat 100 as in-flight — so only A
        // proceeds. Deterministic injected ordering, NO sleeps (#4044).
        let mut host_b = PeerClaimView::new("B".into(), Duration::from_secs(120));
        let t = Instant::now();

        // A's advertisement arrives on B's inbound read task.
        let a_ad = ClaimAd::advertise(100, "loom".into(), "A".into(), 111, "ts".into());
        host_b.observe_at(&a_ad, t);

        // B's work-finder tick: 100 is peer-claimed, so B skips it.
        assert!(host_b.is_claimed_at("loom", 100, t));
        assert!(host_b.claimed_issues_at("loom", t).contains(&100));

        // Once A's sweep exits and A retracts (or the TTL lapses), B is free to
        // pick it up if A never landed a durable label.
        host_b.observe_at(&ClaimAd::retract(100, "loom".into(), "A".into(), 111, "ts".into()), t);
        assert!(!host_b.is_claimed_at("loom", 100, t));
    }
}
