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
use std::sync::{Arc, Mutex, OnceLock};
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

/// Env var overriding how long this host may advertise peer claims with no
/// receive before peer coordination is judged DEGRADED (Issue #6157), in
/// whole seconds. A zero/unparseable value falls through to
/// [`DEFAULT_COORDINATION_DEGRADE_GRACE`].
pub const COORDINATION_DEGRADE_GRACE_ENV: &str = "LOOM_PEER_COORDINATION_DEGRADE_GRACE_SECS";

/// Default degrade grace: 10 minutes — 20× the default reaper
/// re-advertisement cadence
/// ([`crate::sweep_registry::reaper::DEFAULT_REAPER_INTERVAL_SECS`], 30s), so
/// a handful of missed room round-trips (a momentary safehoused hiccup)
/// never trips it, but a genuinely one-way transport — the 2026-08-13
/// incident's `received=0` across 2510 advertisements, sustained for hours —
/// is caught in single-digit minutes, not hours.
pub const DEFAULT_COORDINATION_DEGRADE_GRACE: Duration = Duration::from_secs(600);

/// Env var overriding how many consecutive genuine peer receives must land
/// while coordination is DEGRADED before it is judged recovered (Issue
/// #6157). A zero/unparseable value falls through to
/// [`DEFAULT_COORDINATION_RECOVERY_THRESHOLD`].
pub const COORDINATION_RECOVERY_THRESHOLD_ENV: &str = "LOOM_PEER_COORDINATION_RECOVERY_THRESHOLD";

/// Default recovery threshold: 3 consecutive receives. "Sustained", per the
/// issue's AC4 — a single stray ad (e.g. from an unrelated peer, arriving
/// moments after this host's own transport happens to reconnect) must not by
/// itself clear a DEGRADED verdict whose entire point was "receives were
/// suspiciously absent for a long time".
pub const DEFAULT_COORDINATION_RECOVERY_THRESHOLD: u64 = 3;

/// Resolve the coordination-degrade grace window: env override, else
/// [`DEFAULT_COORDINATION_DEGRADE_GRACE`].
#[must_use]
pub fn resolve_coordination_degrade_grace() -> Duration {
    std::env::var(COORDINATION_DEGRADE_GRACE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_COORDINATION_DEGRADE_GRACE)
}

/// Resolve the coordination-recovery threshold: env override, else
/// [`DEFAULT_COORDINATION_RECOVERY_THRESHOLD`].
#[must_use]
pub fn resolve_coordination_recovery_threshold() -> u64 {
    std::env::var(COORDINATION_RECOVERY_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_COORDINATION_RECOVERY_THRESHOLD)
}

// ============================================================================
// Claim advertisement
// ============================================================================

/// Whether an advertisement asserts a claim, retracts one, or announces a
/// terminal completion narration (Issue #6352).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// "I am dispatching issue #N" — record a peer claim.
    Advertise,
    /// "My sweep for issue #N exited/crashed" — retract my earlier claim early
    /// (before its TTL would lapse).
    Retract,
    /// "I just narrated the public-feed completion for issue #N" (Issue
    /// #6352) — a peer observing this must not narrate its own completion
    /// for the same `(repo, issue)`. Rides the exact same wire shape and
    /// transport as [`ClaimKind::Advertise`]/[`ClaimKind::Retract`] (the
    /// existing peer-claim channel, #4028) but is folded into a **separate**
    /// bookkeeping map ([`PeerClaimView::observe_completion_at`]) — a
    /// completion is a durable historical fact, not a live in-flight claim,
    /// so it deliberately does not participate in
    /// [`ClaimKind::Advertise`]/[`ClaimKind::Retract`]'s TTL-bounded
    /// dispatch-backoff semantics or the `#6157` coordination-health
    /// counters (`advertised`/`received`/`expired`/`dispatch_skipped`),
    /// which stay scoped to dispatch coordination only.
    Completed,
}

impl ClaimKind {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            ClaimKind::Advertise => "advertise",
            ClaimKind::Retract => "retract",
            ClaimKind::Completed => "completed",
        }
    }

    #[must_use]
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "advertise" => Some(ClaimKind::Advertise),
            "retract" => Some(ClaimKind::Retract),
            "completed" => Some(ClaimKind::Completed),
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
    /// The issue being claimed / retracted / completed.
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
    /// The merged PR number this narration is for (Issue #6062). `Some` only
    /// for [`ClaimKind::Completed`] — [`ClaimKind::Advertise`]/[`ClaimKind::Retract`]
    /// leave it `None` (no PR exists yet at dispatch-claim time). A `Completed`
    /// ad with `None` here is either a malformed/absent field (treated as `0`
    /// by [`PeerClaimView::observe_completion_at`], see that method's doc
    /// comment) or, going forward, should never happen from this binary's own
    /// [`Self::completed`] constructor, which always supplies it.
    pub pr: Option<u32>,
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
            pr: None,
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
            pr: None,
        }
    }

    /// "I just narrated the public-feed completion for issue #N, merged as PR
    /// #`pr`" (Issue #6352, PR-keyed per Issue #6062). Broadcast once, right
    /// after this host successfully builds and sends the `completion`
    /// envelope — see [`crate::safehouse::build_and_narrate_completion`].
    #[must_use]
    pub fn completed(
        issue: u32,
        repo: String,
        host: String,
        pid: u32,
        ts: String,
        pr: u32,
    ) -> Self {
        Self {
            kind: ClaimKind::Completed,
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
            PEER_CLAIM_MARKER: CLAIM_SCHEMA_VERSION,
            "kind": self.kind.as_str(),
            "issue": self.issue,
            "repo": self.repo,
            "host": self.host,
            "pid": self.pid,
            "ts": self.ts,
            "pr": self.pr,
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
        // `pr` is new as of Issue #6062: absent (an older peer's `Completed`
        // ad, or any `Advertise`/`Retract`) degrades to `None` rather than
        // rejecting the whole ad — see the field's own doc comment for how a
        // `None` on a `Completed` ad is then handled downstream.
        let pr = obj
            .get("pr")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok());
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
            pr,
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

/// Env var overriding the peer-**completion** dedup TTL, in seconds (Issue
/// #6352). Precedence **env > config (`safehouse.peerCompletionTtlSecs`) >
/// default**.
pub const PEER_COMPLETION_TTL_ENV: &str = "LOOM_PEER_COMPLETION_TTL_SECS";

/// Default peer-completion dedup TTL: 24 hours. Deliberately far longer than
/// [`DEFAULT_PEER_CLAIM_TTL`] (120s) — that TTL is tuned for a *live,
/// re-advertised* dispatch claim; a completion is a one-shot historical fact
/// with no heartbeat to refresh it, and the race it guards against (two
/// hosts' `SweepExited`/`reconcile_recent_merges` paths both observing the
/// same merge within the default 5-minute reconciliation cadence, or a
/// slower host catching up after a connection hiccup) needs a window well
/// beyond one reconciliation tick, not just beyond one dispatch tick. A rare
/// double-post after the window lapses is an accepted paper-cut (mirrors
/// [`DEFAULT_PEER_CLAIM_TTL`]'s own "soft, not a mutex" contract), not a
/// correctness gate.
pub const DEFAULT_PEER_COMPLETION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Resolve the peer-completion TTL with precedence **env > config
/// (`safehouse.peerCompletionTtlSecs`) > default
/// ([`DEFAULT_PEER_COMPLETION_TTL`])** — mirrors [`resolve_peer_claim_ttl`]'s
/// resolution shape exactly, against the sibling config key.
#[must_use]
pub fn resolve_peer_completion_ttl(repo_root: &Path) -> Duration {
    if let Some(secs) = std::env::var(PEER_COMPLETION_TTL_ENV)
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
    from_config.map_or(DEFAULT_PEER_COMPLETION_TTL, Duration::from_secs)
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

/// One tick's peer-coordination health verdict (Issue #6157) — see
/// [`PeerClaimView::evaluate_coordination`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinationEvaluation {
    /// Whether coordination is judged degraded as of this tick.
    pub degraded: bool,
    /// Human-readable rationale (also used for log lines / alerts).
    pub reason: String,
    /// `true` only on the tick where `degraded` just flipped (either
    /// direction) — the edge a caller should log/alert on, not every tick.
    pub transitioned: bool,
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
    /// Completion-narration dedup, keyed by `(repo_slug, issue, merged-PR
    /// number)` (Issue #6352, PR-keyed per Issue #6062). Deliberately a
    /// **separate** map from `claims` above rather than a third `ClaimKind`
    /// folded into it: a completion is a durable historical fact with its own
    /// (much longer) TTL ([`Self::completion_ttl`]), and — critically —
    /// observing one must **not** perturb the `#6157` coordination-health
    /// bookkeeping (`counters`, `last_received_at`, `coordination_degraded`)
    /// that `claims`-path receives feed; that bookkeeping answers "is dispatch
    /// coordination healthy", a question a narration-layer event has no
    /// bearing on.
    ///
    /// The PR number, not the issue alone, is what makes this durable-forever
    /// (bounded only by TTL) key safe: an issue can legitimately merge more
    /// than one PR over its life (partial increments, `Part of #N`), and each
    /// merge is its own completion. Keying on issue alone would silently
    /// suppress every merge after the first for a still-open issue, fleet-wide,
    /// for the rest of the TTL window — the bug Issue #6062 fixes.
    completions: HashMap<(String, u32, u32), Instant>,
    /// TTL for `completions` entries (Issue #6352) — see
    /// [`resolve_peer_completion_ttl`]/[`DEFAULT_PEER_COMPLETION_TTL`].
    /// Settable independently of the claim `ttl` above via
    /// [`Self::set_completion_ttl`]; defaults to
    /// [`DEFAULT_PEER_COMPLETION_TTL`] so a bare `PeerClaimView::new(..)`
    /// (as every pre-#6352 test call site still constructs) gets a sane,
    /// generous completion-dedup window with no call-site changes required.
    completion_ttl: Duration,
    /// Transport counters (Issue #5921) — see [`PeerClaimCounters`].
    counters: PeerClaimCounters,
    /// Local [`Instant`] of this host's FIRST [`Self::record_advertised`]
    /// call (Issue #6157) — the anchor "sustained advertising" is measured
    /// from when neither this nor [`Self::last_received_at`] has a value
    /// yet. `None` on a daemon that has never dispatched (nothing to judge).
    first_advertised_at: Option<Instant>,
    /// Local [`Instant`] of the most recent GENUINE peer receive (Issue
    /// #6157) — set by [`Self::observe_at`], mirroring what
    /// [`PeerClaimCounters::received`] counts.
    last_received_at: Option<Instant>,
    /// Whether [`Self::evaluate_coordination`] currently judges coordination
    /// DEGRADED (Issue #6157).
    coordination_degraded: bool,
    /// When the current DEGRADED window started; `None` while healthy.
    coordination_degraded_since: Option<Instant>,
    /// How many consecutive genuine peer receives have landed since going
    /// DEGRADED, toward [`resolve_coordination_recovery_threshold`]. Always
    /// `0` while healthy.
    consecutive_receives_while_degraded: u64,
    /// The resolved claims-room identity (Issue #6242) — set once, via
    /// [`Self::set_claims_room`], by
    /// [`crate::workspace_pool::WorkspacePool::start_peer_coordination`]
    /// right after it resolves `SafehouseConfig::claims_room()`. `None`
    /// until set (mirrors a freshly-constructed view never having joined a
    /// room yet). A setter rather than a third `new()` parameter so the
    /// several dozen existing test call sites across this crate stay
    /// untouched.
    claims_room: Option<String>,
}

impl PeerClaimView {
    #[must_use]
    pub fn new(self_host: String, ttl: Duration) -> Self {
        Self {
            self_host,
            ttl,
            claims: HashMap::new(),
            completions: HashMap::new(),
            completion_ttl: DEFAULT_PEER_COMPLETION_TTL,
            counters: PeerClaimCounters::default(),
            first_advertised_at: None,
            last_received_at: None,
            coordination_degraded: false,
            coordination_degraded_since: None,
            consecutive_receives_while_degraded: 0,
            claims_room: None,
        }
    }

    /// Record the resolved claims-room identity (Issue #6242) —
    /// `SafehouseConfig::claims_room()`'s value at coordination-start time,
    /// so `loom-daemon status`/`health` can render it and make a two-host
    /// room mismatch a one-line diff. Pass `None` when peer-claim
    /// coordination is not enabled.
    pub fn set_claims_room(&mut self, claims_room: Option<String>) {
        self.claims_room = claims_room;
    }

    /// Override the completion-dedup TTL (Issue #6352), resolved by
    /// [`crate::workspace_pool::WorkspacePool::start_peer_coordination`] via
    /// [`resolve_peer_completion_ttl`] right after construction — mirrors
    /// [`Self::set_claims_room`]'s post-`new()` setter shape rather than a
    /// third `new()` parameter, so the many existing two-argument
    /// `PeerClaimView::new(host, ttl)` call sites across this crate's tests
    /// stay untouched.
    pub fn set_completion_ttl(&mut self, ttl: Duration) {
        self.completion_ttl = ttl;
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
    /// # `ClaimKind::Completed` is out of scope here (Issue #6352)
    ///
    /// This method only ever folds [`ClaimKind::Advertise`]/
    /// [`ClaimKind::Retract`] into the `claims` map — a `Completed` ad is
    /// routed to the dedicated [`Self::observe_completion_at`] instead (see
    /// [`crate::safehouse::PeerClaimSink::on_event`], the one caller that
    /// dispatches by kind). A `Completed` ad reaching here is treated as a
    /// no-op (`false`, no state change) rather than silently folded into the
    /// dispatch-claims map it does not belong in.
    pub fn observe_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        if ad.kind == ClaimKind::Completed {
            return false;
        }
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
            ClaimKind::Completed => unreachable!("returned above"),
        }
        // #5921: only genuine peer traffic counts as "received" — an ignored
        // self-ad returns `false` above, before this line, so it never
        // inflates the counter.
        self.counters.received += 1;
        // Issue #6157: track receive recency (the anchor
        // `evaluate_coordination` measures "quiet for" against) and, while
        // DEGRADED, progress toward the sustained-receive recovery
        // threshold. Every genuine peer ad counts, advertise or retract —
        // mirroring `received`'s own "did traffic arrive" semantics above; a
        // self-ad never reaches this line (returned `false` above), so it
        // can never manufacture a false recovery.
        self.last_received_at = Some(now);
        if self.coordination_degraded {
            self.consecutive_receives_while_degraded += 1;
        }
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

    /// Observe an inbound [`ClaimKind::Completed`] ad at local time `now`
    /// (Issue #6352): "a peer already narrated the public-feed completion for
    /// this `(repo, issue, merged PR)`". Returns `true` when applied (a
    /// peer's), `false` when ignored as this host's own ad — the identical
    /// self-claim recognition [`Self::observe_at`] applies, including the
    /// `UNKNOWN_HOST` carve-out (see that method's doc comment), so two
    /// unresolved-identity hosts still dedup each other's completions
    /// correctly.
    ///
    /// `ad.pr` missing (a pre-#6062 peer, or a malformed ad) degrades to `0`
    /// rather than rejecting the ad outright — the same "never lose it"
    /// tradeoff every other best-effort field in this module makes. Peers
    /// that never send a real PR number all collide on the same `0` slot per
    /// `(repo, issue)`, so a mixed-version fleet during a rolling upgrade can
    /// under-narrate for the transition window, but the mechanism never
    /// crashes and self-heals once every host is upgraded — a strictly
    /// smaller risk than the permanent over-suppression this issue fixes.
    ///
    /// Deliberately does **not** touch `counters`/`last_received_at`/
    /// `coordination_degraded` — see the `completions` field's doc comment
    /// for why a narration-layer event must not feed the `#6157`
    /// dispatch-coordination-health verdict.
    pub fn observe_completion_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert_eq!(ad.kind, ClaimKind::Completed);
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false; // never suppress our own narration on our own ad
        }
        self.completions
            .insert((ad.repo.clone(), ad.issue, ad.pr.unwrap_or(0)), now);
        true
    }

    /// Whether `(repo, issue, pr)` has a **live** (non-expired) peer-narrated
    /// completion at local time `now` (Issue #6352, PR-keyed per Issue #6062)
    /// — the check [`crate::safehouse::build_and_narrate_completion`] makes
    /// before narrating its own.
    #[must_use]
    pub fn is_narrated_at(&self, repo: &str, issue: u32, pr: u32, now: Instant) -> bool {
        self.completions
            .get(&(repo.to_owned(), issue, pr))
            .is_some_and(|received_at| {
                now.saturating_duration_since(*received_at) < self.completion_ttl
            })
    }

    /// Drop every expired `completions` entry at local time `now` (Issue
    /// #6352) — the `completions`-map sibling of [`Self::prune_expired`],
    /// kept as a separate method (rather than folded into that one) so its
    /// removals never inflate `counters.expired`, which is documented as the
    /// crash-release signal for `claims`, not a general "entries removed"
    /// tally.
    pub fn prune_expired_completions(&mut self, now: Instant) {
        let ttl = self.completion_ttl;
        self.completions
            .retain(|_, received_at| now.saturating_duration_since(*received_at) < ttl);
    }

    /// Number of tracked completions (test/observability aid; includes
    /// not-yet-pruned expired entries).
    #[must_use]
    pub fn completions_len(&self) -> usize {
        self.completions.len()
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
        self.record_advertised_at(Instant::now());
    }

    /// [`Self::record_advertised`] with an injected local time (Issue #6157)
    /// — the real entry point counts against `Instant::now()`; tests inject
    /// a synthetic clock so [`Self::evaluate_coordination`]'s grace-window
    /// math is deterministic (no real sleeps, matching this module's
    /// established testing discipline).
    fn record_advertised_at(&mut self, now: Instant) {
        self.counters.advertised += 1;
        // Issue #6157: anchor "sustained advertising" from the FIRST
        // advertisement this process has ever made — never overwritten by
        // later ones, so the degrade grace is measured against how long
        // this host has been trying, not reset by every heartbeat.
        if self.first_advertised_at.is_none() {
            self.first_advertised_at = Some(now);
        }
    }

    /// Record that a dispatch was backed off because this view showed a live
    /// peer claim on the issue being dispatched (Issue #5921, the #5789
    /// enforcement path) — called from
    /// [`crate::sweep_registry::SweepRegistry::dispatch`]'s peer-claim guard.
    pub fn record_dispatch_skipped(&mut self) {
        self.counters.dispatch_skipped += 1;
    }

    // ------------------------------------------------------------------
    // Peer-coordination health (Issue #6157)
    // ------------------------------------------------------------------

    /// Evaluate (and update) this host's peer-coordination health at local
    /// time `now`. Called on the reaper's cadence
    /// ([`crate::sweep_registry::reaper`], default 30s) — NOT on every
    /// [`Self::to_status`] render, so the verdict changes on a controlled
    /// cadence rather than whenever an IPC caller happens to ask.
    ///
    /// # Decision rule
    ///
    /// - Never advertised ([`Self::first_advertised_at`] is `None`) —
    ///   healthy: nothing to judge yet.
    /// - Not currently degraded: once the time since the more recent of
    ///   `first_advertised_at`/`last_received_at` reaches `grace`, flip
    ///   DEGRADED — the exact signature of the 2026-08-13 incident (sustained
    ///   advertising, zero receives: `received=0` while `advertised>0`, and
    ///   the log-scale equivalent of "no receive within N× the advertisement
    ///   interval").
    /// - Currently degraded: recovers only once `recovery_threshold`
    ///   CONSECUTIVE genuine peer receives have landed since going degraded
    ///   — a single stray ad does not clear it (issue's AC4).
    pub fn evaluate_coordination(
        &mut self,
        now: Instant,
        grace: Duration,
        recovery_threshold: u64,
    ) -> CoordinationEvaluation {
        if self.coordination_degraded {
            if self.consecutive_receives_while_degraded >= recovery_threshold {
                self.coordination_degraded = false;
                self.coordination_degraded_since = None;
                self.consecutive_receives_while_degraded = 0;
                return CoordinationEvaluation {
                    degraded: false,
                    reason: format!(
                        "recovered after {recovery_threshold} consecutive peer receive(s)"
                    ),
                    transitioned: true,
                };
            }
            return CoordinationEvaluation {
                degraded: true,
                reason: format!(
                    "peer coordination degraded ({}/{recovery_threshold} sustained receives \
                     toward recovery)",
                    self.consecutive_receives_while_degraded
                ),
                transitioned: false,
            };
        }

        let Some(first) = self.first_advertised_at else {
            return CoordinationEvaluation {
                degraded: false,
                reason: "no advertisement made yet — nothing to judge".to_string(),
                transitioned: false,
            };
        };
        let anchor = self.last_received_at.unwrap_or(first);
        let quiet_for = now.saturating_duration_since(anchor);
        if quiet_for >= grace {
            self.coordination_degraded = true;
            self.coordination_degraded_since = Some(now);
            self.consecutive_receives_while_degraded = 0;
            return CoordinationEvaluation {
                degraded: true,
                reason: format!(
                    "no peer claim received in {}s (>= {}s grace) despite this host \
                     advertising — the receive path looks one-way or dead",
                    quiet_for.as_secs(),
                    grace.as_secs()
                ),
                transitioned: true,
            };
        }
        CoordinationEvaluation {
            degraded: false,
            reason: "receiving normally".to_string(),
            transitioned: false,
        }
    }

    /// Whether coordination is CURRENTLY judged degraded — a read-only
    /// snapshot; see [`Self::evaluate_coordination`] for the stateful tick
    /// that actually decides this.
    #[must_use]
    pub fn coordination_degraded(&self) -> bool {
        self.coordination_degraded
    }

    /// Seconds since coordination was judged degraded; `None` while healthy.
    #[must_use]
    pub fn coordination_degraded_for_secs(&self, now: Instant) -> Option<u64> {
        self.coordination_degraded_since
            .map(|since| now.saturating_duration_since(since).as_secs())
    }

    /// How many consecutive genuine peer receives have landed since going
    /// degraded, toward [`Self::evaluate_coordination`]'s `recovery_threshold`.
    /// Always `0` while healthy.
    #[must_use]
    pub fn coordination_receives_toward_recovery(&self) -> u64 {
        self.consecutive_receives_while_degraded
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
            coordination: crate::types::PeerCoordinationHealth {
                degraded: self.coordination_degraded,
                degraded_for_secs: self.coordination_degraded_for_secs(now),
                consecutive_receives_toward_recovery: self.consecutive_receives_while_degraded,
                recovery_threshold: resolve_coordination_recovery_threshold(),
            },
            claims_room: self.claims_room.clone(),
        }
    }
}

// ============================================================================
// Process-global handle (Issue #6157)
// ============================================================================

/// Process-global peer-claim view handle, mirroring
/// [`crate::rate_limit_breaker::register_global`]'s shape: the daemon's ONE
/// shared [`PeerClaimView`]
/// ([`crate::workspace_pool::WorkspacePool::start_peer_coordination`] owns
/// the canonical `Arc`) is registered here once so
/// [`crate::claim_reconciliation`] — which runs its stale-claim reclamation
/// pass on its own `spawn_blocking` task with no
/// [`crate::sweep_registry::SweepRegistry`] reference of its own — can
/// consult the SAME live coordination-degraded verdict the reaper maintains,
/// without threading an `Arc` through the reconciliation call chain. Unset
/// (byte-for-byte no-op) reads as "not degraded" — the pre-#6157 behavior,
/// and the correct default for a host with `safehouse.enabled` false.
///
/// Deliberately NOT unit-tested directly (same posture as
/// [`crate::rate_limit_breaker`]'s own `GLOBAL`): a `OnceLock` is
/// process-wide and every `#[test]` in this crate's test binary shares one
/// process, so setting it from a test would leak into every other test that
/// happens to run afterward. [`PeerClaimView::evaluate_coordination`] (the
/// actual decision logic) is fully covered on a plain instance instead; see
/// `claim_reconciliation`'s `reconcile_workspace_with_coordination` for how
/// production callers inject this function while tests inject a synthetic
/// closure.
static GLOBAL_VIEW: OnceLock<Arc<Mutex<PeerClaimView>>> = OnceLock::new();

/// Register the process-global peer-claim view. Idempotent: first
/// registration wins.
pub fn register_global_view(view: Arc<Mutex<PeerClaimView>>) {
    let _ = GLOBAL_VIEW.set(view);
}

/// Is the process-global peer-claim view currently judged
/// coordination-DEGRADED (Issue #6157)? Returns the reason string when it
/// is; `None` when healthy OR when no view has been registered (no
/// safehouse coordination established — zero behavior change from before
/// this module existed).
#[must_use]
pub fn global_coordination_degraded_reason() -> Option<String> {
    let view = GLOBAL_VIEW.get()?;
    let v = view
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if v.coordination_degraded {
        Some(format!(
            "peer coordination degraded ({}/{} sustained receives toward recovery)",
            v.consecutive_receives_while_degraded,
            resolve_coordination_recovery_threshold()
        ))
    } else {
        None
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
            pr: None,
        }
    }

    /// A [`ClaimKind::Completed`] ad carrying a real merged-PR number (Issue
    /// #6062) — use this rather than `ad(ClaimKind::Completed, ..)` whenever a
    /// test cares about the PR-keyed dedup, since `ad()` leaves `pr: None`.
    fn completed_ad(issue: u32, repo: &str, host: &str, pr: u32) -> ClaimAd {
        ClaimAd::completed(
            issue,
            repo.to_owned(),
            host.to_owned(),
            42,
            "2026-07-28T00:00:00Z".to_owned(),
            pr,
        )
    }

    // ---- ClaimAd JSON round-trip + malformed rejection ----

    #[test]
    fn body_json_round_trips() {
        let a = ClaimAd::advertise(4028, "loom".into(), "maple".into(), 123, "ts".into());
        let parsed = ClaimAd::from_body_str(&a.to_body_json()).unwrap();
        assert_eq!(parsed, a);
        assert_eq!(parsed.pr, None, "Advertise never carries a PR number");

        let r = ClaimAd::retract(7, "loom".into(), "birch".into(), 9, "ts".into());
        let parsed = ClaimAd::from_body_str(&r.to_body_json()).unwrap();
        assert_eq!(parsed, r);
        assert_eq!(parsed.pr, None, "Retract never carries a PR number");

        // Issue #6062: a `Completed` ad's merged-PR number must survive the
        // JSON round trip too — it is the dedup key's third component.
        let c = ClaimAd::completed(4426, "loom".into(), "host-a".into(), 1, "ts".into(), 4400);
        let parsed = ClaimAd::from_body_str(&c.to_body_json()).unwrap();
        assert_eq!(parsed, c);
        assert_eq!(parsed.pr, Some(4400));
    }

    /// Issue #6062: a `Completed` ad from a peer running a pre-#6062 binary
    /// never sent a `pr` field at all — the parse must still succeed (not
    /// reject the whole ad) and degrade `pr` to `None`, exactly like the
    /// pre-existing `pid`/`ts` diagnostic-absence tolerance.
    #[test]
    fn a_completed_ad_missing_the_pr_field_parses_with_pr_none() {
        let parsed = ClaimAd::from_body_str(
            r#"{"loom_claim":1,"kind":"completed","issue":4426,"repo":"loom","host":"host-a"}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, ClaimKind::Completed);
        assert_eq!(parsed.pr, None);
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

    // ---- completion dedup (Issue #6352) ----

    #[test]
    fn peer_completion_is_observed_and_expires_after_its_own_ttl() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(120));
        view.set_completion_ttl(Duration::from_secs(100));
        let base = Instant::now();

        assert!(!view.is_narrated_at("loom", 4426, 815, base));
        assert!(view.observe_completion_at(&completed_ad(4426, "loom", "peer", 815), base));
        assert!(view.is_narrated_at("loom", 4426, 815, base + Duration::from_secs(99)));
        assert!(!view.is_narrated_at("loom", 4426, 815, base + Duration::from_secs(100)));
    }

    /// The crux of #6352: two hosts race to narrate the same merge — one
    /// wins, the other must observe the winner's `Completed` ad and back
    /// off, mirroring the two-host scenario from the issue's own AC.
    #[test]
    fn peer_completion_from_a_different_host_suppresses_a_would_be_duplicate() {
        let mut view = PeerClaimView::new("host-b".into(), Duration::from_secs(3600));
        let now = Instant::now();
        assert!(!view.is_narrated_at("loom", 1124, 5772, now));

        // host-a narrated first and broadcast a Completed ad.
        view.observe_completion_at(&completed_ad(1124, "loom", "host-a", 5772), now);

        // host-b's own build_and_narrate_completion check now sees it.
        assert!(view.is_narrated_at("loom", 1124, 5772, now));
        // A different issue on the same repo is unaffected.
        assert!(!view.is_narrated_at("loom", 1125, 5772, now));
        // The same issue on a different repo is unaffected (per-repo keying).
        assert!(!view.is_narrated_at("other-repo", 1124, 5772, now));
        // A *different* merged PR against the SAME still-open issue is a
        // distinct completion (Issue #6062: partial increments) — it must not
        // be suppressed just because a different PR for this issue was
        // already narrated.
        assert!(!view.is_narrated_at("loom", 1124, 5773, now));
    }

    /// A `Completed` ad with no `pr` (a pre-#6062 peer, or a malformed ad)
    /// degrades to key `0` rather than being dropped — see
    /// [`PeerClaimView::observe_completion_at`]'s doc comment.
    #[test]
    fn a_completion_missing_pr_degrades_to_key_zero_rather_than_being_dropped() {
        let mut view = PeerClaimView::new("host-b".into(), Duration::from_secs(3600));
        let now = Instant::now();
        assert!(view.observe_completion_at(&ad(ClaimKind::Completed, 1124, "loom", "host-a"), now));
        assert!(view.is_narrated_at("loom", 1124, 0, now));
        // It does NOT stand in for a real PR number — a genuinely known PR is
        // still a distinct, independently-narratable key.
        assert!(!view.is_narrated_at("loom", 1124, 5772, now));
    }

    #[test]
    fn own_completion_ad_never_suppresses_our_own_future_narration() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(3600));
        let now = Instant::now();
        // An echo of our own outbound Completed ad (e.g. relayed back by the
        // room) must be ignored exactly like Advertise/Retract self-echoes.
        assert!(!view.observe_completion_at(&completed_ad(1124, "loom", "me", 5772), now));
        assert!(!view.is_narrated_at("loom", 1124, 5772, now));
    }

    #[test]
    fn prune_expired_completions_reclaims_map_space_independent_of_claims() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(10));
        view.set_completion_ttl(Duration::from_secs(10));
        let base = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base);
        view.observe_completion_at(&completed_ad(2, "loom", "peer", 202), base);
        assert_eq!(view.len(), 1);
        assert_eq!(view.completions_len(), 1);

        // Pruning completions never touches the claims map or its counters.
        view.prune_expired_completions(base + Duration::from_secs(11));
        assert_eq!(view.completions_len(), 0);
        assert_eq!(view.len(), 1, "prune_expired_completions must not touch claims");
        assert_eq!(view.counters().expired, 0, "completions never feed the claims-expiry counter");
    }

    /// A `Completed` ad must never perturb the #6157 coordination-health
    /// bookkeeping that `Advertise`/`Retract` receives feed — a narration
    /// event answers a different question than "is dispatch coordination
    /// healthy".
    #[test]
    fn observing_a_completion_never_touches_coordination_health_counters() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(120));
        let now = Instant::now();
        view.observe_completion_at(&completed_ad(1, "loom", "peer", 101), now);
        let c = view.counters();
        assert_eq!(c.received, 0);
        assert_eq!(c.advertised, 0);
        assert_eq!(c.expired, 0);
        let eval = view.evaluate_coordination(
            now,
            Duration::from_secs(600),
            DEFAULT_COORDINATION_RECOVERY_THRESHOLD,
        );
        // Never advertised ⇒ "nothing to judge yet", unaffected by the
        // completion observed above.
        assert!(!eval.degraded);
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
        // Issue #6157: a freshly-advertising, actively-receiving view is
        // never degraded — `to_status` renders the CURRENT state, it never
        // evaluates (evaluation only happens on the reaper's cadence).
        assert!(!status.coordination.degraded);
        assert_eq!(status.coordination.degraded_for_secs, None);
        assert_eq!(status.coordination.consecutive_receives_toward_recovery, 0);
        // Issue #6242: a freshly-constructed view has never had its
        // claims-room identity set — `to_status` must render `None`, not an
        // empty string, matching `PeerClaimStatus::claims_room`'s
        // `None`-vs-empty-view contract.
        assert_eq!(status.claims_room, None);
    }

    /// Issue #6242: the resolved claims-room identity round-trips through
    /// `to_status()` once `set_claims_room` has been called — the exact
    /// sequence `WorkspacePool::start_peer_coordination` performs after
    /// resolving `SafehouseConfig::claims_room()`.
    #[test]
    fn to_status_carries_the_resolved_claims_room() {
        let ttl = Duration::from_secs(120);
        let mut view = PeerClaimView::new("self-host".into(), ttl);
        view.set_claims_room(Some("!claims:example.org".to_string()));

        let status = view.to_status(Instant::now());
        assert_eq!(status.claims_room, Some("!claims:example.org".to_string()));
    }

    /// A room can be explicitly cleared back to `None` (e.g. a test
    /// resetting state) — `to_status` reflects whatever was last set, never
    /// stale data from a prior call.
    #[test]
    fn to_status_clears_claims_room_back_to_none() {
        let ttl = Duration::from_secs(120);
        let mut view = PeerClaimView::new("self-host".into(), ttl);
        view.set_claims_room(Some("!claims:example.org".to_string()));
        view.set_claims_room(None);

        let status = view.to_status(Instant::now());
        assert_eq!(status.claims_room, None);
    }

    // ---- peer-coordination health (Issue #6157) ----

    #[test]
    fn coordination_stays_healthy_when_never_advertised() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(100));
        let now = Instant::now();
        let eval = view.evaluate_coordination(now, Duration::from_secs(600), 3);
        assert!(!eval.degraded);
        assert!(!eval.transitioned);
        assert!(!view.coordination_degraded());
    }

    #[test]
    fn coordination_stays_healthy_within_grace_with_no_receive_yet() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
        let base = Instant::now();
        view.record_advertised_at(base);

        let grace = Duration::from_secs(600);
        // Just under the grace window: still healthy.
        let eval = view.evaluate_coordination(base + Duration::from_secs(599), grace, 3);
        assert!(!eval.degraded);
        assert!(!eval.transitioned);
    }

    /// The 2026-08-13 incident's exact signature: sustained advertising
    /// (2510 advertised, one per dispatch/reaper heartbeat), zero receives,
    /// for hours. Once the grace window elapses with no receive at all,
    /// coordination must flip DEGRADED.
    #[test]
    fn coordination_degrades_after_grace_with_zero_receives() {
        let mut view = PeerClaimView::new("robb-studio".into(), Duration::from_secs(1000));
        let base = Instant::now();
        view.record_advertised_at(base);

        let grace = Duration::from_secs(600);
        let eval = view.evaluate_coordination(base + Duration::from_secs(600), grace, 3);
        assert!(eval.degraded);
        assert!(eval.transitioned, "the tick that crosses the grace window must transition");
        assert!(view.coordination_degraded());
        assert_eq!(view.coordination_degraded_for_secs(base + Duration::from_secs(600)), Some(0));

        // A later tick, still no receive: still degraded, but no LONGER a
        // transition (already-degraded ticks should not re-fire an alert).
        let eval2 = view.evaluate_coordination(base + Duration::from_secs(700), grace, 3);
        assert!(eval2.degraded);
        assert!(!eval2.transitioned);
        assert_eq!(view.coordination_degraded_for_secs(base + Duration::from_secs(700)), Some(100));
    }

    /// A receive that arrives just before the grace window elapses resets
    /// the "quiet for" anchor — the receive path is not actually dead, it
    /// was just slow once.
    #[test]
    fn coordination_receive_before_grace_elapses_prevents_degrade() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
        let base = Instant::now();
        view.record_advertised_at(base);

        let grace = Duration::from_secs(600);
        // A genuine peer receive lands at t+590, just inside the window.
        view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", "peer"),
            base + Duration::from_secs(590),
        );

        // At t+600 (which would have tripped the grace measured from
        // first-advertised) coordination is still healthy: the anchor moved
        // to the receive at t+590.
        let eval = view.evaluate_coordination(base + Duration::from_secs(600), grace, 3);
        assert!(!eval.degraded);
        assert!(!eval.transitioned);
    }

    /// Issue #6157 AC4: recovery requires SUSTAINED receives, not a single
    /// one — a lone stray ad must not immediately clear a DEGRADED verdict.
    #[test]
    fn coordination_recovery_requires_sustained_not_single_receive() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
        let base = Instant::now();
        view.record_advertised_at(base);
        let grace = Duration::from_secs(600);
        let recovery_threshold = 3;

        // Trip DEGRADED.
        let eval =
            view.evaluate_coordination(base + Duration::from_secs(600), grace, recovery_threshold);
        assert!(eval.degraded && eval.transitioned);

        // A single receive lands while degraded: not enough to recover.
        view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", "peer"),
            base + Duration::from_secs(610),
        );
        let eval2 =
            view.evaluate_coordination(base + Duration::from_secs(620), grace, recovery_threshold);
        assert!(eval2.degraded, "a single receive must not clear a DEGRADED verdict");
        assert!(!eval2.transitioned);
        assert_eq!(view.coordination_receives_toward_recovery(), 1);

        // Two more receives land — three consecutive total, meeting the
        // threshold.
        view.observe_at(
            &ad(ClaimKind::Retract, 1, "loom", "peer"),
            base + Duration::from_secs(630),
        );
        view.observe_at(
            &ad(ClaimKind::Advertise, 2, "loom", "peer"),
            base + Duration::from_secs(640),
        );
        let eval3 =
            view.evaluate_coordination(base + Duration::from_secs(650), grace, recovery_threshold);
        assert!(!eval3.degraded, "3 consecutive receives must clear the DEGRADED verdict");
        assert!(eval3.transitioned);
        assert!(!view.coordination_degraded());
        assert_eq!(view.coordination_receives_toward_recovery(), 0);
    }

    /// A self-advertisement is never counted as a receive (mirrors
    /// `own_advertisement_is_never_backed_off_on`), so it can never
    /// manufacture a false recovery signal for THIS host's own DEGRADED
    /// coordination.
    #[test]
    fn coordination_recovery_ignores_self_advertisements() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
        let base = Instant::now();
        view.record_advertised_at(base);
        let grace = Duration::from_secs(600);
        let eval = view.evaluate_coordination(base + Duration::from_secs(600), grace, 3);
        assert!(eval.degraded && eval.transitioned);

        // Re-advertising (a reaper heartbeat) and our own ad arriving back
        // somehow must not count toward recovery.
        view.record_advertised_at(base + Duration::from_secs(610));
        assert!(!view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", "me"),
            base + Duration::from_secs(611)
        ));
        assert_eq!(view.coordination_receives_toward_recovery(), 0);

        let eval2 = view.evaluate_coordination(base + Duration::from_secs(620), grace, 3);
        assert!(eval2.degraded, "self-ads must never clear a DEGRADED verdict");
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
