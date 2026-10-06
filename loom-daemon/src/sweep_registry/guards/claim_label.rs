//! The forge's `labeled loom:building` timeline signal, and the guards built
//! on it.
//!
//! Two consumers live here:
//!
//! 1. **Cross-host claim-ownership verification before release/reclaim**
//!    (Issues #5017/#5282) — [`SweepRegistry::fetch_claim_labeled_at`] and
//!    [`SweepRegistry::claim_superseded_on_forge`], moved verbatim out of
//!    `guards.rs` (which is frozen at its current size by
//!    `scripts/file-size-baseline.txt`) so this module's own additions do not
//!    grow it.
//! 2. **The dispatch-time yield to a young *leaseless* foreign claim**
//!    (Issue #9453 Phase 3.1) — [`SweepRegistry::resolve_leaseless_claim_order`],
//!    the second leg of [`SweepRegistry::resolve_lease_order`]'s
//!    claim-then-verify-order tie-break (#6287). The first leg reads lease
//!    *comments*; this one reads the *label event* the same timeline probe
//!    above already fetches, for the one lane that has no lease comment to
//!    read.
//!
//! Both are FAIL-OPEN on an unverifiable read: an unreachable forge must never
//! be turned into a refusal, and must never manufacture a yield.

use super::*;

// ------------------------------------------------------------------------
// Cross-host claim-ownership verification before release/reclaim
// (Issue #5017 / #5282)
// ------------------------------------------------------------------------
//
// `.loom/locks/issue-<N>` (see `locks.rs`'s `release_lock_owned`) is
// strictly HOST-LOCAL filesystem state: it is written by `acquire_lock`
// when *this* daemon dispatches *its own* sweep, and no other host's
// daemon ever sees it. That makes it structurally blind to a genuine
// cross-host race: when host B cancels its own losing duplicate dispatch
// for an issue host A is actively (and validly) building, host B's local
// lock names host B's own (about-to-be-cancelled) sweep as the owner —
// it matches, so `release_lock_owned` returns `Released`, not
// `Superseded`, and the caller proceeds to call `restore_label_to_ready`,
// destroying the ONLY cross-host mutex (the `loom:building` label)
// out from under host A's still-live sweep. This is exactly what
// happened on loom#5270 (2026-08-04): cancelling loom-worker-1's losing
// duplicate reverted `loom:building` on the issue robb-studio's sweep
// still owned, reopening it to a third dispatch.
//
// The forge's own label-event timeline, by contrast, is observed
// identically by every host — it is the one piece of claim state that is
// NOT host-local. `fetch_claim_labeled_at` / `claim_superseded_on_forge`
// below add that cross-host signal as an ADDITIONAL guard alongside (not
// a replacement for) the cheaper host-local `Superseded`/`HolderAlive`
// checks: every call site short-circuits on the existing local check
// first, so the extra `gh api .../timeline` round trip is only paid when
// the local lock could not already answer the question.

impl SweepRegistry {
    /// Fetch the most recent `labeled loom:building` timeline event timestamp
    /// for `issue` (Issue #5017/#5282) — the forge-side, cross-host claim
    /// signal every host observes identically, unlike the host-local
    /// `.loom/locks/issue-<N>` claim lock.
    ///
    /// Mirrors [`crate::claim_reconciliation::forge`]'s own
    /// `fetch_claim_labeled_at` (used there for PR-claim reconciliation) —
    /// the underlying `issues/{n}/timeline` REST endpoint is identical for
    /// issues and PRs, so the query shape is reused verbatim; this copy lives
    /// in `sweep_registry` so the cancel/reap label-restore path (this
    /// module) can call it without a cross-module `pub(crate)` promotion of a
    /// function whose doc comments are specific to PR-claim reconciliation.
    ///
    /// FAIL-OPEN: returns `None` on any `gh` failure/timeout/non-zero
    /// exit/unparseable output, or when the label was never applied. Callers
    /// MUST treat `None` as "cannot verify, proceed with existing behavior"
    /// — same fail-open contract as every other forge probe in this module
    /// ([`classify_preflip_labels`](Self::classify_preflip_labels),
    /// [`issue_is_closed_or_pr`](Self::issue_is_closed_or_pr)).
    pub(crate) fn fetch_claim_labeled_at(&self, issue: u32) -> Option<DateTime<Utc>> {
        // Counted as `guard.claim_timeline` (#10089); the facade applies the
        // cross-owner GH_CONFIG_DIR (#5401) and `LOOM_REPO` as GH_REPO (#8263 —
        // `gh api` has no `--repo` flag).
        let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}/timeline");
        let jq = r#"[.[] | select(.event == "labeled" and .label.name == "loom:building") | .created_at] | max // empty"#;
        let output = self
            .gh_read("guard.claim_timeline", ["api", &path, "--paginate", "--jq", jq])
            .ok()
            .flatten()?;
        if !output.status.success() {
            return None;
        }
        parse_max_timestamp(&output.stdout)
    }

    /// Whether the forge's `loom:building` claim on `issue` was (re-)applied
    /// STRICTLY AFTER `claimed_at` (Issue #5017/#5282) — i.e. a different
    /// claimant, possibly on another host entirely invisible to this host's
    /// `.loom/locks/issue-<N>`, has (re-)claimed the issue since this sweep's
    /// own claim/dispatch time. When `true`, the caller MUST skip
    /// [`restore_label_to_ready`](Self::restore_label_to_ready) — exactly the
    /// same "leave the live claim alone" contract as a host-local
    /// `Superseded`/`HolderAlive` verdict from `release_lock_owned`.
    ///
    /// FAIL-OPEN: an unverifiable read ([`fetch_claim_labeled_at`] returns
    /// `None`) resolves to `false` (not superseded) — an unreachable forge
    /// must never permanently wedge a claim, matching every other check in
    /// this module's fail-open posture (see `restore_label_to_ready`'s own
    /// doc comment).
    ///
    /// [`fetch_claim_labeled_at`]: Self::fetch_claim_labeled_at
    pub(crate) fn claim_superseded_on_forge(&self, issue: u32, claimed_at: DateTime<Utc>) -> bool {
        match self.fetch_claim_labeled_at(issue) {
            Some(labeled_at) if labeled_at > claimed_at => {
                log::warn!(
                    "sweep_registry: issue #{issue}'s `loom:building` claim was (re-)applied at \
                     {} — AFTER this sweep's own claim/dispatch time {} — leaving the label \
                     alone instead of restoring it (#5017/#5282 cross-host claim-ownership \
                     guard). A different claimant, possibly on another host, now owns this \
                     issue; destroying its claim here would repeat the loom#5270 incident.",
                    labeled_at.to_rfc3339(),
                    claimed_at.to_rfc3339(),
                );
                true
            }
            _ => false,
        }
    }
}

// ------------------------------------------------------------------------
// The leaseless-claim dispatch yield (Issue #9453 Phase 3.1)
// ------------------------------------------------------------------------

/// Grace window within which a `labeled loom:building` event with **no** live
/// lease comment beside it is treated as a live foreign claim (Issue #9453
/// Phase 3.1).
///
/// Deliberately the SAME 10-minute label grace orphan recovery already applies
/// to a fresh `loom:building` it cannot otherwise account for
/// ([`crate::worktree_ops::orphan_recovery::DEFAULT_LABEL_GRACE_PERIOD_SECS`]),
/// reused rather than re-derived: both windows answer the identical question —
/// "is this label young enough that a claimant which has not yet published any
/// other evidence of itself is more likely alive than abandoned?" — and a
/// second, independently-tuned constant for the same judgement would be free to
/// drift out of agreement with the reaper that acts on it.
pub(crate) const LEASELESS_CLAIM_LABEL_GRACE_SECS: i64 =
    crate::worktree_ops::orphan_recovery::DEFAULT_LABEL_GRACE_PERIOD_SECS;

/// The `earliest_host` reported when this dispatcher yields to a leaseless
/// claim (Issue #9453 Phase 3.1). A label event names no host and no sweep —
/// that is precisely the gap Phase 2's mandatory `lease ensure` closes for
/// compliant lanes — so the yield reports an explicit "unknown" rather than
/// inventing a plausible-looking identity.
pub(crate) const LEASELESS_CLAIM_HOST: &str = "unknown-host";

/// Prefix of the `earliest_sweep_id` reported when this dispatcher yields to a
/// leaseless claim, completed by the label event's own timestamp (compact
/// `%Y%m%dT%H%M%SZ`, matching this codebase's sweep-id timestamp convention so
/// the value stays a single punctuation-free token in logs, the
/// `LeaseOrderDispatchError`, and the standdown annotation).
pub(crate) const LEASELESS_CLAIM_SWEEP_PREFIX: &str = "leaseless-loom-building-label-";

/// Whether `in_window` — the lease comments
/// [`lease_episode::in_episode`](super::lease_episode::in_episode) admitted
/// into the current claim episode — contains NOTHING but this dispatcher's own
/// records (Issue #9453 Phase 3.1).
///
/// This is the precondition for consulting the label timeline at all, and it is
/// load-bearing in **both** directions:
///
/// - **False** (a foreign lease record is in-window) ⇒ the #6287 comment-order
///   tie-break already has better evidence than a label event and has already
///   decided. Firing the label leg here would be actively harmful: in a genuine
///   two-daemon race BOTH hosts see a young `loom:building` (whichever flipped
///   first created it, and the second flip is a no-op that adds no event), so
///   both would yield and nobody would build.
/// - **True** ⇒ every compliant lane publishes a lease (#8193 + #9453 Phase 2),
///   so a claim with no lease record at all is evidence of exactly the lane
///   that publishes none: a hand-claim.
pub(crate) fn is_sole_claimant(in_window: &[&LeaseComment], host: &str, sweep_id: &str) -> bool {
    in_window
        .iter()
        .all(|c| c.host == host && c.sweep_id == sweep_id)
}

/// Whether a `labeled loom:building` event at `labeled_at` is a **live foreign**
/// claim relative to a dispatch attempt whose episode began at `episode_start`
/// (Issue #9453 Phase 3.1). Pure, so the two properties it encodes can be
/// pinned without a `gh` fixture:
///
/// - **Foreign**: strictly OLDER than `episode_start`. This dispatcher captures
///   `episode_start` immediately *before* its own label flip, so the event its
///   own flip creates is always newer — without this leg every uncontested
///   dispatch would yield to itself. (When the label was already present, the
///   flip is a no-op that creates no event at all, so the newest event remains
///   the foreign claimant's.)
/// - **Live**: no older than [`LEASELESS_CLAIM_LABEL_GRACE_SECS`]. An older
///   label is the shape a finished or abandoned claim leaves behind, and
///   yielding to it forever would wedge redispatch — the same anti-wedge
///   property [`LEASE_ORDER_LOOKBACK_SECS`] gives the comment-order leg.
pub(crate) fn claim_label_is_live_foreign(
    labeled_at: DateTime<Utc>,
    episode_start: DateTime<Utc>,
) -> bool {
    labeled_at < episode_start
        && episode_start - labeled_at <= chrono::Duration::seconds(LEASELESS_CLAIM_LABEL_GRACE_SECS)
}

/// Slack, in seconds, between this dispatcher's own flip window and a
/// `labeled loom:building` event's forge timestamp for the event to still be
/// attributable to that flip (Issue #10345). Covers forge timestamps being
/// truncated to whole seconds plus ordinary clock skew; anything farther away
/// is treated as someone else's claim.
pub(crate) const OWN_FLIP_ATTRIBUTION_SLACK_SECS: i64 = 2;

/// Whether a `labeled loom:building` event (`actor`, `created_at`) is
/// attributable to this dispatcher's OWN label flip, performed inside
/// `[flip_start, flip_end]` on the local clock (Issue #10345). Pure.
///
/// Both legs must hold: the actor is the fleet's own forge identity (a human
/// or any other login is a hand-claim), AND the event falls within
/// [`OWN_FLIP_ATTRIBUTION_SLACK_SECS`] of the flip window (the shared bot
/// identity at an unrelated time is some other fleet lane's claim).
pub(crate) fn claim_event_is_own_flip(
    actor: &str,
    created_at: DateTime<Utc>,
    flip_start: DateTime<Utc>,
    flip_end: DateTime<Utc>,
    fleet: &crate::forge_identity::FleetLogins,
) -> bool {
    if actor.trim().is_empty() || !fleet.contains(actor) {
        return false;
    }
    let slack = chrono::Duration::seconds(OWN_FLIP_ATTRIBUTION_SLACK_SECS);
    created_at >= flip_start - slack && created_at <= flip_end + slack
}

/// Parse `actor<TAB>timestamp` lines (one per `--paginate` page) into the
/// newest event's `(actor, created_at)`. `None` when no line parses.
pub(crate) fn parse_claim_event(stdout: &[u8]) -> Option<(String, DateTime<Utc>)> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let (actor, ts) = line.trim().split_once('\t')?;
            let ts = DateTime::parse_from_rfc3339(ts.trim().trim_matches('"')).ok()?;
            Some((actor.trim().to_string(), ts.with_timezone(&Utc)))
        })
        .max_by_key(|(_, ts)| *ts)
}

impl SweepRegistry {
    /// The newest `labeled loom:building` event's `(actor login, created_at)`.
    /// `None` on any read failure or unparseable output (callers fail closed).
    pub(crate) fn fetch_claim_event(&self, issue: u32) -> Option<(String, DateTime<Utc>)> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}/timeline");
        let jq = r#"[.[] | select(.event == "labeled" and .label.name == "loom:building")] | max_by(.created_at) | select(. != null) | "\(.actor.login // "")\t\(.created_at)""#;
        let output = self
            .gh_read("guard.claim_timeline", ["api", &path, "--paginate", "--jq", jq])
            .ok()
            .flatten()?;
        if !output.status.success() {
            return None;
        }
        parse_claim_event(&output.stdout)
    }

    /// Whether a leaseless yield's `loom:building` label is *provably* this
    /// dispatcher's own phantom (Issue #10345), so it may be reverted.
    ///
    /// Requires: the newest label event is attributable to this dispatcher's
    /// flip ([`claim_event_is_own_flip`]), AND a fresh lease-comment read shows
    /// no lease record other than this sweep's own. FAIL-CLOSED: every
    /// unverifiable read returns `false` (keep the label, as before #10345) so
    /// a real hand-claim's mutex (#5270/#9453) is never destroyed on a guess.
    pub(crate) fn leaseless_yield_is_own_phantom(
        &self,
        issue: u32,
        sweep_id: &str,
        flip_start: DateTime<Utc>,
        flip_end: DateTime<Utc>,
    ) -> bool {
        let Some((actor, created_at)) = self.fetch_claim_event(issue) else {
            return false;
        };
        let fleet = crate::forge_identity::FleetLogins::for_root(&self.config.workspace_root);
        if !claim_event_is_own_flip(&actor, created_at, flip_start, flip_end, &fleet) {
            return false;
        }
        let host = self.published_host_id();
        let Some(comments) = self.read_lease_comments(issue) else {
            return false;
        };
        comments
            .iter()
            .all(|c| c.host == host && c.sweep_id == sweep_id)
    }
}

impl LeaseOrderDecision {
    /// Chain [`SweepRegistry::resolve_leaseless_claim_order`] onto a
    /// comment-order verdict (Issue #9453 Phase 3.1): the label leg runs ONLY
    /// when the comment-order leg found no reason to yield *and* this
    /// dispatcher is the sole in-window claimant (see [`is_sole_claimant`]).
    /// Every other verdict — including every fail-open `Proceed` reached with a
    /// foreign lease record in the window — is returned untouched.
    pub(crate) fn or_leaseless_claim(
        self,
        registry: &SweepRegistry,
        issue: u32,
        episode_start: DateTime<Utc>,
        sole_in_window: bool,
    ) -> Self {
        if self != Self::Proceed || !sole_in_window {
            return self;
        }
        registry.resolve_leaseless_claim_order(issue, episode_start)
    }

    /// The yielded-to claimant's (host, sweep-id) identity, or `None` for
    /// [`LeaseOrderDecision::Proceed`] — the one shape
    /// [`dispatch_inner`](SweepRegistry::dispatch_inner) consumes, so a new
    /// yield variant never has to be re-plumbed through the dispatch path.
    pub(crate) fn yield_identity(self) -> Option<(String, String)> {
        match self {
            Self::Proceed => None,
            Self::Yield {
                earliest_host,
                earliest_sweep_id,
            } => Some((earliest_host, earliest_sweep_id)),
            Self::YieldToLeaselessClaim { labeled_at } => Some((
                LEASELESS_CLAIM_HOST.to_string(),
                format!("{LEASELESS_CLAIM_SWEEP_PREFIX}{}", labeled_at.format("%Y%m%dT%H%M%SZ")),
            )),
        }
    }
}

impl SweepRegistry {
    /// The label leg of the claim-then-verify-order tie-break (Issue #9453
    /// Phase 3.1): with no foreign lease comment in the current claim episode,
    /// read the issue's newest `labeled loom:building` timeline event and yield
    /// when it is a live foreign claim ([`claim_label_is_live_foreign`]).
    ///
    /// **Why a bare label is now evidence.** Pre-#9453 a leaseless
    /// `loom:building` carried no weight: several lanes claimed without
    /// publishing a lease, so refusing on the label alone would have refused
    /// mostly-phantom claims. Phase 2 makes every compliant lane leased
    /// (`loom-daemon lease ensure`, #8193, called by `worktree.sh` and required
    /// of hand-claim lanes), which inverts the inference — a young
    /// `loom:building` with no lease beside it is now the signature of the one
    /// lane that races the fleet invisibly, the operator-directed hand-claim
    /// behind #9432 (label applied 17:57:07Z; the fleet opened a duplicate PR
    /// 76 minutes later).
    ///
    /// **Cost**: one bounded REST timeline read per dispatch that reaches the
    /// tie-break as sole in-window claimant — the same read
    /// [`claim_superseded_on_forge`](Self::claim_superseded_on_forge) already
    /// makes on the reap path, on REST's own rate-limit bucket rather than the
    /// GraphQL one the earlier dispatch guards use.
    ///
    /// FAIL-OPEN, exactly like every other leg of this tie-break: an
    /// unverifiable read ([`fetch_claim_labeled_at`](Self::fetch_claim_labeled_at)
    /// returns `None` — a `gh` failure, a timeout, or a genuinely absent label
    /// event) resolves to [`LeaseOrderDecision::Proceed`]. This leg only ever
    /// ADDS a refusal on positive, forge-assigned evidence; it never invents one
    /// from an absence.
    pub(crate) fn resolve_leaseless_claim_order(
        &self,
        issue: u32,
        episode_start: DateTime<Utc>,
    ) -> LeaseOrderDecision {
        let Some(labeled_at) = self.fetch_claim_labeled_at(issue) else {
            return LeaseOrderDecision::Proceed;
        };
        if !claim_label_is_live_foreign(labeled_at, episode_start) {
            return LeaseOrderDecision::Proceed;
        }
        log::warn!(
            "sweep_registry: issue #{issue} carries a `loom:building` label applied at {} — \
             before this dispatch attempt's own episode start {}, and inside the {}s label \
             grace — with no lease comment from any peer in the claim episode. Since every \
             compliant lane publishes a lease (#8193, #9453 Phase 2), that is a live foreign \
             claim from a lane this dispatcher cannot see (a hand-claim). Yielding before \
             spawning a builder (#9453 Phase 3.1, Class A of #9447).",
            labeled_at.to_rfc3339(),
            episode_start.to_rfc3339(),
            LEASELESS_CLAIM_LABEL_GRACE_SECS,
        );
        LeaseOrderDecision::YieldToLeaselessClaim { labeled_at }
    }
}

#[cfg(test)]
#[path = "claim_label_tests.rs"]
mod claim_label_tests;
