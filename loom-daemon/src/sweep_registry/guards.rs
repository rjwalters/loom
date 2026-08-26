//! Pre-dispatch guards: collision detection and the closed-issue /
//! open-PR / park-label label probes.

use super::*;
use crate::claim_reconciliation::forge::parse_max_timestamp;

/// Three-state result of the open-linked-PR probe (Issue #4452).
///
/// Defined in [`crate::worktree_ops::gh`] and re-exported here, where it
/// originated: #5511 moved the enum, the GraphQL document, and the
/// `state == "OPEN"` classification down into `worktree_ops::gh` so orphan
/// recovery could reuse ONE implementation instead of growing a second copy of
/// the same closes-graph query. See that module for the per-variant fail-open
/// contract each consumer relies on.
pub(crate) use crate::worktree_ops::gh::OpenPrProbe;

/// Bounded attempt count for [`SweepRegistry::probe_open_linked_pr`] (Issue
/// #6058). Two total attempts (one retry) absorbs a single transient `gh`
/// transport failure — observed in production as intermittent TLS
/// certificate-verification errors bursting across otherwise-unrelated `gh`
/// invocations for a tick or two — without meaningfully widening the #4123
/// guard's synchronous latency budget (already up to two sequential `gh`
/// calls, GraphQL then REST, per attempt).
pub(crate) const OPEN_PR_PROBE_MAX_ATTEMPTS: u32 = 2;

/// Delay between [`OPEN_PR_PROBE_MAX_ATTEMPTS`] retry attempts. Short by
/// design: the production failure mode this retry targets is a brief
/// per-invocation transport blip (a handful of seconds, bursty, then clear),
/// not a sustained multi-minute outage — a fresh subprocess retried after a
/// short pause is a better bet than a long backoff would be worth waiting
/// for, and the #4123 guard already tolerates the existing GraphQL+REST
/// latency on every call.
pub(crate) const OPEN_PR_PROBE_RETRY_DELAY: Duration = Duration::from_millis(300);

/// Env kill-switch for the verified-open-PR memo (Issue #6788). `0`/`false`/
/// `no`/`off` disables it, restoring the byte-for-byte pre-#6788 probe: every
/// call pays the full GraphQL-then-REST sequence, and a double transport
/// failure falls open immediately. Defaults **on**. Provided because this is
/// the only part of the #4123 guard that can refuse a dispatch on evidence
/// older than the current instant, so an operator needs a way to take it out
/// of the loop without a rebuild.
pub const OPEN_PR_MEMO_ENABLE_ENV: &str = "LOOM_OPEN_PR_MEMO";

/// How long a verified [`OpenPrProbe::Open`] answer is reused *without*
/// re-probing the forge (Issue #6788).
///
/// The #4123 guard is consulted once per work-finder tick per candidate, and a
/// candidate whose PR is parked awaiting a human (`loom:pr` + `loom:operator`,
/// a Champion merge-risk hold) stays a candidate for **days**. Measured on this
/// repo's own daemon log, three such issues re-paid the closes-graph probe
/// 4456 / 5380 / 5489 times over five days — ~15k GraphQL queries spent
/// re-deriving an answer that had not changed once. That spend is a direct
/// contributor to the GraphQL exhaustion that then makes *both* transports fail
/// and drops the guard through its documented fail-open arm (see
/// [`probe_open_linked_pr`](SweepRegistry::probe_open_linked_pr)).
///
/// Fifteen minutes bounds the cost of being wrong in the only direction it can
/// be wrong: an issue whose linked PR just closed/merged waits at most this long
/// before it is dispatchable again. (In the merge case it does not wait at all —
/// the merge closes the issue, which drops it from the candidate query entirely.)
/// The memo is invalidated immediately, ahead of this window, by any verified
/// [`OpenPrProbe::NoneOpen`] answer.
pub(crate) const OPEN_PR_MEMO_FRESH: Duration = Duration::from_secs(900);

/// One entry of the verified-open-PR memo (Issue #6788): the PR number a
/// *verified* [`OpenPrProbe::Open`] answer named, and when that verification
/// happened. In-memory only — a daemon restart clears it, exactly like the
/// dispatch backoff and the quarantine tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenPrMemoEntry {
    /// The open linked PR the probe verified.
    pub(crate) pr: u32,
    /// When that verification was made.
    pub(crate) verified_at: DateTime<Utc>,
}

/// Whether the verified-open-PR memo (Issue #6788) is enabled, per
/// [`OPEN_PR_MEMO_ENABLE_ENV`]. Defaults **on**.
fn open_pr_memo_enabled() -> bool {
    match std::env::var(OPEN_PR_MEMO_ENABLE_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Marker prefix a lease record's forge comment body starts with (Issue
/// #6179, Epic #6165 Phase 1 — "give the forge claim a liveness dimension").
/// [`SweepRegistry::write_lease_comment`] posts a comment whose literal first
/// line is `<prefix><host> sweep=<sweep-id> -->` at the moment a dispatch
/// successfully flips `loom:building` — see `defaults/docs/lease-record.md`
/// for the full format contract and `defaults/docs/lease-renewal.md` for the
/// sibling mechanism (#6180) that keeps the record fresh. Every reader,
/// present or future, must locate the comment via
/// `.starts_with(LEASE_MARKER_PREFIX)`, never by parsing the free-form prose
/// that follows the marker's closing `-->` — and the comment's own
/// forge-assigned `updated_at` is the sole liveness signal, never a
/// timestamp embedded in the text.
///
/// This phase (Phase 1) only *writes* the record: nothing in the
/// reclamation/dispatch decision path reads it back yet. That is Phase 2, a
/// future issue.
///
/// **`<host>` is an opaque id by default (Issue #6322), not this host's raw
/// hostname.** [`SweepRegistry::published_host_id`] is what actually decides
/// the value every writer below uses — see its doc comment.
pub(crate) const LEASE_MARKER_PREFIX: &str = "<!-- loom:lease host=";

/// Lookback window (Issue #6287, Epic #6165 Phase 2) bounding which lease
/// comments [`SweepRegistry::resolve_lease_order`] treats as belonging to
/// *this* claim episode. An issue accumulates one lease comment per
/// dispatch over its whole lifetime (comments are never deleted), so a
/// naive "earliest comment wins" comparison against the issue's full
/// history would always lose to a much older, long-since-completed lease —
/// wedging every normal, uncontested dispatch. Only lease comments whose
/// own forge-assigned `created_at` falls within this many seconds of the
/// current dispatch attempt's pre-flip instant are compared; anything
/// older is historical noise from a prior claim round and is ignored.
/// Generous relative to a genuine "near-simultaneous" race (the scenario
/// this tie-break exists for), which resolves within, at most, a handful
/// of seconds of `gh` round-trip latency.
pub(crate) const LEASE_ORDER_LOOKBACK_SECS: i64 = 90;

/// Bounded attempt count for [`SweepRegistry::resolve_lease_order`]'s
/// "own comment not found" read-back retry (Issue #6816). Mirrors the
/// existing [`OPEN_PR_PROBE_MAX_ATTEMPTS`] pattern in this module: a single
/// immediate read-back racing ahead of the forge's own read-after-write
/// propagation for the comments-list endpoint is the most likely way two
/// near-simultaneous dispatchers can BOTH fail to find their own freshly
/// written lease comment and BOTH fall open to `Proceed`, defeating the
/// #6287 tie-break the two of them were supposed to resolve. A few retries
/// closes most of that window without meaningfully widening dispatch
/// latency in the overwhelmingly common (uncontested, first-read-succeeds)
/// case.
pub(crate) const LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS: u32 = 3;

/// Delay between [`LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS`] retry attempts.
/// Short by design, matching [`OPEN_PR_PROBE_RETRY_DELAY`]'s rationale: the
/// propagation lag this retries against is typically sub-second, not a
/// sustained outage.
pub(crate) const LEASE_ORDER_OWN_COMMENT_RETRY_DELAY: Duration = Duration::from_millis(300);

/// Bounded number of confirmation re-reads performed once this dispatcher's
/// own comment already looks earliest (or sole) in-window, before
/// [`SweepRegistry::resolve_lease_order`] commits to
/// [`LeaseOrderDecision::Proceed`] (Issue #6951).
///
/// The [`LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS`] retry above (#6816) only
/// re-reads when THIS dispatcher's own comment is missing from the
/// read-back — it never re-checks the complementary case, where a PEER's
/// earlier comment simply has not propagated into this GET yet while this
/// dispatcher's own (later-written) comment already has. Two dispatchers on
/// different hosts (and therefore different credentials) racing within a
/// few seconds of each other can each independently take a single read,
/// see no peer, and both conclude "I'm earliest" — exactly the cross-host
/// recurrence #6951 reports (two lease comments 3 seconds apart, each host
/// proceeding). This confirmation phase gives a slower-propagating peer
/// comment additional time to appear before this dispatcher commits to
/// spawning a builder; see [`SweepRegistry::confirm_sole_claim`].
pub(crate) const LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS: u32 = 3;

/// Delay between [`LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS`] confirmation
/// re-reads. `dispatch_inner` already runs off the shared registry mutex
/// specifically to tolerate multi-second dispatch latency (Issue #6592), so
/// this budget favors narrowing the race window over minimizing added
/// latency — unlike [`LEASE_ORDER_OWN_COMMENT_RETRY_DELAY`], which guards a
/// typically sub-second ambiguity.
pub(crate) const LEASE_ORDER_SOLE_CLAIM_CONFIRM_DELAY: Duration = Duration::from_millis(500);

/// Env var toggling cross-host dispatch-collision detection AND enforcement
/// (Issue #4085, Phase 0 of #4028; upgraded from detection-only into
/// enforcement by #5789). Precedence **env > config > default**; default
/// **off** because the probe adds one extra `gh issue view` round-trip per
/// dispatch. `1`/`true`/`yes`/`on` enable; anything else disables. When
/// enabled, the daemon does a pre-flip label read and, on a confirmed
/// collision, [`SweepRegistry::dispatch_inner`](super::SweepRegistry::dispatch_inner)
/// backs off the dispatch instead of proceeding — see
/// [`SweepRegistry::detect_and_record_collision`](super::SweepRegistry::detect_and_record_collision).
pub const COLLISION_DETECT_ENV: &str = "LOOM_DETECT_COLLISIONS";

/// Classification of a pre-flip label read (Issue #4085). The caller
/// ([`SweepRegistry::dispatch_inner`](super::SweepRegistry::dispatch_inner))
/// backs off the dispatch on a confirmed [`CollisionClass::Collision`] (#5789
/// — previously detection-only, recording the outcome without changing
/// dispatch behavior).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CollisionClass {
    /// `loom:issue` was still present and `loom:building` absent — this host is
    /// the first claimant, no collision.
    Clean,
    /// `loom:issue` was already gone, or `loom:building` already present, before
    /// this host flipped the labels — a peer host claimed it first. Carries the
    /// observed pre-flip label set for the diagnostic log record.
    Collision { labels: Vec<String> },
    /// The label state could not be read (gh timeout / non-zero exit /
    /// unparseable JSON). **Fail-closed**: never counted as a collision, so the
    /// baseline is never inflated by an unverifiable flip.
    Unknown,
}

/// A single lease-record comment read back from the forge (Issue #6287),
/// parsed from the marker
/// [`SweepRegistry::write_lease_comment`](super::SweepRegistry::write_lease_comment)
/// posts. `id` is the forge's own server-assigned comment identifier —
/// monotonically increasing with creation order — which is the ONLY signal
/// [`SweepRegistry::resolve_lease_order`](super::SweepRegistry::resolve_lease_order)
/// orders by; `created_at` (also forge-assigned) is used solely to bound
/// comparison to the current claim episode (see [`LEASE_ORDER_LOOKBACK_SECS`]),
/// never to break order ties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseComment {
    pub(crate) id: u64,
    pub(crate) created_at: Option<DateTime<Utc>>,
    pub(crate) host: String,
    pub(crate) sweep_id: String,
}

/// Outcome of
/// [`SweepRegistry::resolve_lease_order`](super::SweepRegistry::resolve_lease_order)
/// — the claim-then-verify-order tie-break (Issue #6287).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseOrderDecision {
    /// This dispatcher's own lease comment is the earliest live one within
    /// the current claim episode, OR the order could not be determined
    /// (fail-open) — proceed to spawn.
    Proceed,
    /// A different lease comment, from a peer dispatch racing for the same
    /// claim, has an earlier forge-assigned order. This dispatcher lost the
    /// tie-break and must yield before spawning a builder or touching a
    /// worktree.
    Yield {
        earliest_host: String,
        earliest_sweep_id: String,
    },
}

/// Resolve whether cross-host dispatch-collision detection runs (Issue #4085,
/// Phase 0 of #4028), precedence **env > config > default(false)**. Reads
/// `LOOM_DETECT_COLLISIONS` first, then `autonomous.collisionDetection.enabled`
/// from the effective config, then defaults **off** (the probe adds one extra
/// `gh issue view` round-trip per dispatch, so it is opt-in until a baseline
/// justifies it).
#[must_use]
pub fn resolve_collision_detection(repo_root: &Path) -> bool {
    if let Ok(v) = std::env::var(COLLISION_DETECT_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "autonomous")
        .and_then(|a| a.get("collisionDetection"))
        .and_then(|c| c.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

impl SweepRegistry {
    // ------------------------------------------------------------------------
    // Cross-host collision detection (Issue #4085, Phase 0 of #4028)
    // ------------------------------------------------------------------------

    /// Read the issue's **current** forge labels and classify whether a peer
    /// host already claimed it (Issue #4085). Called by [`dispatch`](Self::dispatch)
    /// immediately *before* the label flip, when detection is enabled — a
    /// post-flip read cannot distinguish a collided issue from a clean one (both
    /// end up `loom:building`), so the observation must happen pre-flip.
    ///
    /// Uses a bounded `gh issue view <N> --json labels`. **Fail-closed**: any
    /// timeout, non-zero exit, or unparseable payload resolves to
    /// [`CollisionClass::Unknown`], never `Collision`, so the baseline is never
    /// inflated by an unverifiable read. The `gh issue edit` flip itself is left
    /// byte-for-byte unchanged.
    pub(crate) fn classify_preflip_labels(&self, issue: u32) -> CollisionClass {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("labels");
        // Same workspace/repo scoping as the flip (#3937): resolve the issue
        // against *this* registry's repo, not the daemon's cwd repo.
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        let timeout = reap_gh_timeout();
        let output = match output_with_timeout(cmd, timeout) {
            Ok(Some(o)) if o.status.success() => o,
            Ok(Some(_)) => return CollisionClass::Unknown, // non-zero exit
            Ok(None) => return CollisionClass::Unknown,    // timed out + killed
            Err(_) => return CollisionClass::Unknown,      // spawn failure
        };
        // `gh issue view --json labels` emits `{"labels":[{"name":"..."},...]}`.
        let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => return CollisionClass::Unknown,
        };
        let Some(arr) = parsed.get("labels").and_then(|l| l.as_array()) else {
            return CollisionClass::Unknown;
        };
        let labels: Vec<String> = arr
            .iter()
            .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        let has_issue = labels.iter().any(|l| l == "loom:issue");
        let has_building = labels.iter().any(|l| l == "loom:building");
        if !has_issue || has_building {
            CollisionClass::Collision { labels }
        } else {
            CollisionClass::Clean
        }
    }

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
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue}/timeline"))
            .arg("--paginate")
            .arg("--jq")
            .arg(
                r#"[.[] | select(.event == "labeled" and .label.name == "loom:building") | .created_at] | max // empty"#,
            );
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        let timeout = reap_gh_timeout();
        let output = output_with_timeout(cmd, timeout).ok().flatten()?;
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

    /// When detection is enabled, probe the pre-flip label state and record a
    /// cross-host collision (Issue #4085). Returns `None` — without invoking
    /// `gh` at all — when detection is disabled, so the disabled dispatch path
    /// is byte-for-byte unchanged; returns `Some(classification)` when the
    /// probe ran, so the caller ([`SweepRegistry::dispatch_inner`]) can act on a
    /// confirmed [`CollisionClass::Collision`] (Issue #5789 upgraded this from
    /// detection-only into a real enforcement gate — the caller now backs off
    /// the dispatch instead of proceeding unchanged). Increments
    /// [`collision_count`](Self::collision_count) and logs a diagnostic record
    /// (issue, repo/workspace, host, timestamp, observed pre-flip labels) on a
    /// confirmed collision — this detection/counting/logging behavior is
    /// reused verbatim from #4085, not replaced, so the existing collision
    /// tests below continue to pass unmodified.
    pub(crate) fn detect_and_record_collision(&mut self, issue: u32) -> Option<CollisionClass> {
        if !self.detect_collisions {
            return None;
        }
        let class = self.classify_preflip_labels(issue);
        match &class {
            CollisionClass::Collision { labels } => {
                self.collision_count += 1;
                log::warn!(
                    "sweep_registry: cross-host dispatch collision (#4085/#5789) — issue #{issue} \
                     in {repo} was already claimed by another host before host {host} attempted \
                     to flip it at {ts}; observed pre-flip labels=[{labels}]; running collision \
                     count={count}",
                    repo = self.config.workspace_root.display(),
                    host = host_identity(),
                    ts = Utc::now().to_rfc3339(),
                    labels = labels.join(", "),
                    count = self.collision_count,
                );
                // Issue #6243: feed the SAME confirmed collision into the
                // shared peer-claim view's windowed same-issue counter,
                // distinct from `self.collision_count`'s monotonic total —
                // see `PeerClaimView::record_same_issue_collision_at`'s doc
                // comment for why a second counter is needed here. A no-op
                // when no view is attached (safehouse disabled) — the same
                // fail-open posture `detect_and_record_collision` already
                // has for every other branch.
                if let Some(view) = &self.peer_claims {
                    let repo = peer_claims::repo_slug(&self.config.workspace_root);
                    view.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .record_same_issue_collision_at(repo, issue, std::time::Instant::now());
                }
            }
            CollisionClass::Unknown => {
                // Fail-closed: an unverifiable read is never a collision.
                log::debug!(
                    "sweep_registry: collision probe for issue #{issue} inconclusive \
                     (fail-closed, not counted) (#4085)"
                );
            }
            CollisionClass::Clean => {}
        }
        Some(class)
    }

    // ------------------------------------------------------------------------
    // Forge label flip
    // ------------------------------------------------------------------------

    /// Best-effort probe of whether a dispatch number is **terminal or not an
    /// issue at all** (Issues #4088, #4504). Returns `Some(true)` when the number
    /// must not be dispatched (a closed issue, or a pull request in ANY state),
    /// `Some(false)` when it is a verifiably open issue, and `None` on any error
    /// (missing/failed/timed-out `gh`, unresolvable repo, unparseable output).
    ///
    /// Callers MUST treat `None` as **fail-open** — a forge outage or a wedged
    /// `gh` must never wedge dispatch. The call is bounded by [`reap_gh_timeout`]
    /// exactly like the label flips so it cannot block the dispatch path.
    ///
    /// **Why REST and not `gh issue view --json state` (#4504).** Issues and PRs
    /// share one number namespace, and `gh issue view` resolves a PR number
    /// happily — as a GraphQL `PullRequest` node, whose `state` is a *three*-value
    /// enum (`OPEN`/`CLOSED`/`MERGED`) rather than an issue's two. The original
    /// #4088 probe matched only `"CLOSED"`, so a merged PR's `"MERGED"` fell into
    /// the `_ => None` fail-open arm — indistinguishable from a `gh` outage — and
    /// dispatch proceeded against already-merged work. Widening the state match to
    /// include `"MERGED"` is *not* sufficient: an **open** PR reports `"OPEN"`,
    /// byte-identical to an open issue, so no state string can separate the two.
    /// The REST payload (`repos/{owner}/{repo}/issues/{N}`) carries a
    /// `pull_request` key present **if and only if** the number is a PR,
    /// regardless of its open/closed/merged state — a structural discriminator
    /// instead of an ever-expanding state-string set. REST is also a separate
    /// rate-limit bucket from GraphQL, matching the #4444 park-label probe.
    ///
    /// The `--jq` collapses both facts into one JSON object
    /// (`{"state":"open","is_pr":false}`); anything that does not parse into that
    /// shape is a genuine lookup failure and returns `None`. `MERGED` is accepted
    /// as terminal alongside `CLOSED` as belt-and-suspenders — REST reports a
    /// merged PR as `state: "closed"`, but an Issue-shaped node reporting `MERGED`
    /// must never fall through to the fail-open arm again.
    pub(crate) fn issue_is_closed_or_pr(&self, issue: u32) -> Option<bool> {
        // `gh api` cannot infer the repo from the working directory; this helper
        // prefers the process-global LOOM_REPO override and falls back to
        // `gh repo view` in the workspace root, so the override keeps working
        // exactly as it did with `gh issue view --repo`. Returns `None` (fail
        // open) when the repo cannot be resolved.
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{issue}"))
            .arg("--jq")
            .arg("{state, is_pr: (.pull_request != null)}");
        // Resolve against this registry's own workspace, matching the label-flip
        // helpers and the other dispatch-path probes (#3937).
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let output = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
        // A PR is terminal for dispatch purposes in EVERY state — an open PR is
        // still not an issue anyone can build.
        if parsed.get("is_pr")?.as_bool()? {
            return Some(true);
        }
        match parsed
            .get("state")?
            .as_str()?
            .trim()
            .to_ascii_uppercase()
            .as_str()
        {
            "CLOSED" | "MERGED" => Some(true),
            "OPEN" => Some(false),
            // Reserved for genuine lookup failures only: an unrecognized state
            // string is an unparseable answer, not a verdict.
            _ => None,
        }
    }

    /// Best-effort probe for an **open** pull request linked to `issue` via
    /// GitHub's authoritative closes-graph (`closedByPullRequestsReferences`),
    /// used by the #4123 open-PR dispatch guard, the #4366 no-progress predicate,
    /// and the #4256 crash-resume path. Returns an [`OpenPrProbe`] that
    /// distinguishes three states (Issue #4452): [`OpenPrProbe::Open`] (verified:
    /// at least one open linked PR), [`OpenPrProbe::NoneOpen`] (verified: the
    /// forge answered, no open linked PR), and [`OpenPrProbe::ProbeFailed`] (the
    /// probe could not produce a verdict — missing/failed/timed-out `gh`,
    /// unresolvable repo, non-zero exit, or unparseable output).
    ///
    /// The per-variant fail-open contract is documented on [`OpenPrProbe`]: the
    /// dispatch guard treats `NoneOpen` **and** `ProbeFailed` as "proceed" (a
    /// forge outage or wedged `gh` must never wedge dispatch), while the
    /// no-progress predicate counts ONLY `NoneOpen` toward a failed attempt so a
    /// probe failure never manufactures quarantine pressure. Bounded by
    /// [`reap_gh_timeout`] exactly like the label flips so it cannot block the
    /// dispatch path.
    ///
    /// The query itself
    /// ([`crate::worktree_ops::gh::open_linked_pr_args`]) and its
    /// classification ([`crate::worktree_ops::gh::parse_open_linked_pr`] — the
    /// load-bearing `state == "OPEN"` filter that keeps merged PRs from reading
    /// as open) are shared with orphan recovery as of #5511; only the transport
    /// differs (this one is timeout-bounded and runs the registry's configured
    /// `gh_bin` in its own workspace).
    ///
    /// **REST fallback (#5911).** The GraphQL closes-graph query above shares a
    /// quota with every other GraphQL caller in the fleet, and quota exhaustion
    /// under concurrent agents is a documented recurring failure mode in this
    /// repo. Pre-#5911 that exhaustion made this probe answer `ProbeFailed`,
    /// which the #4123 dispatch guard (by design) treats as "proceed" — so a
    /// GraphQL-starved tick would silently let the guard fall open and
    /// re-dispatch an issue whose PR was, in fact, still open (observed
    /// repeatedly on #5565/#5569). Only when the GraphQL probe itself fails to
    /// answer, retry over REST (`issues/{n}/timeline`) before falling open —
    /// REST is a *separate* rate-limit bucket from GraphQL, the same rationale
    /// already used by the #4444 park-label probe below. A verified GraphQL
    /// answer (`Open` or `NoneOpen`) is trusted as-is and never pays the extra
    /// REST round trip.
    ///
    /// **Bounded whole-probe retry (#6058).** #5911's REST fallback recovers a
    /// GraphQL-only outage, but production logs from this very flap (issue
    /// #5895 repeatedly bouncing `loom:building` <-> `loom:issue` despite an
    /// open, `loom:operator`-held implementing PR) showed BOTH transports
    /// failing back-to-back — not GraphQL quota exhaustion, but intermittent
    /// `gh` transport failures (`tls: failed to verify certificate: x509:
    /// certificate signed by unknown authority`) bursting across otherwise
    /// unrelated `gh` invocations in the same daemon tick, then clearing a
    /// tick or two later. That shape — a brief, per-invocation transport blip,
    /// not a sustained outage — is exactly what a second attempt after a short
    /// pause is likely to recover from, so retry the *entire* GraphQL-then-REST
    /// sequence once more before conceding `ProbeFailed` (and therefore, at the
    /// #4123 guard, falling open). Still fails open on a genuine sustained
    /// outage after [`OPEN_PR_PROBE_MAX_ATTEMPTS`] attempts — this narrows the
    /// window for the guard's documented fail-open behavior, it does not
    /// remove it (removing it would risk wedging dispatch on a real forge
    /// outage, which is not this issue's failure mode).
    ///
    /// **Verified-open-PR memo (#6788).** #6058 narrowed the double-failure
    /// window; it did not close it, and the window kept firing — four observed
    /// occurrences (#5936/#5914, #6261/#6296, #6389/#6422, #6472/#6484) of the
    /// #4123 guard falling open on an issue whose closing PR was `loom:pr` +
    /// `loom:operator` held. This repo's own daemon log settles the cause:
    /// across those issues, **91% of the fall-through dispatches happened
    /// within 120s of a logged forge rate-limit event, against a ~12% baseline
    /// for the guard-held dispatches** — a ~7.5x relative risk, i.e. the
    /// double-probe-failure fail-open arm is the dominant cause, not some other
    /// gap in the dispatch path. The same log shows why the window kept
    /// re-opening: the guard re-probed those three issues 4456 / 5380 / 5489
    /// times over five days, spending the very GraphQL quota whose exhaustion
    /// opens the window.
    ///
    /// So the memo attacks both ends of that loop, using one piece of state
    /// ([`OpenPrMemoEntry`]) written only from *verified* answers:
    ///
    /// 1. **Fresh-memo short circuit** — a verified `Open(pr)` newer than
    ///    [`OPEN_PR_MEMO_FRESH`] is returned with **zero** `gh` calls, so a
    ///    long-lived open PR is re-derived a few times an hour instead of once
    ///    a minute. This is the half that reduces the quota pressure.
    /// 2. **Known-PR recheck backstop** — when both transports fail on every
    ///    attempt, and a memo exists (however old), re-verify *that one PR*
    ///    with a single non-paginated REST `GET repos/{o}/{r}/pulls/{pr}`
    ///    before conceding. That call needs neither the closes-graph nor a
    ///    timeline walk, so it survives exactly the conditions that killed both
    ///    transports. Only a live `"open"` answer holds the guard.
    ///
    /// The fail-open contract is intact: with no memo, or if the recheck itself
    /// cannot answer, or if it answers anything other than `"open"`, this still
    /// returns [`OpenPrProbe::ProbeFailed`] and the #4123 guard still proceeds.
    /// A genuine forge outage with no prior verified answer cannot wedge
    /// dispatch. The memo is in-memory (a daemon restart clears it), is
    /// invalidated by any verified `NoneOpen`, and can be switched off entirely
    /// via [`OPEN_PR_MEMO_ENABLE_ENV`].
    pub(crate) fn probe_open_linked_pr(&self, issue: u32) -> OpenPrProbe {
        // 1. Fresh-memo short circuit (#6788) — no forge round trip at all.
        if let Some(memo) = self.fresh_open_pr_memo(issue, Utc::now()) {
            log::debug!(
                "sweep_registry: open-PR probe for issue #{issue} served from the verified memo \
                 (PR #{}, verified {}s ago) — skipping the closes-graph round trip (#6788)",
                memo.pr,
                (Utc::now() - memo.verified_at).num_seconds().max(0)
            );
            return OpenPrProbe::Open(memo.pr);
        }

        for attempt in 1..=OPEN_PR_PROBE_MAX_ATTEMPTS {
            let verdict = self.probe_open_linked_pr_transports(issue);
            if verdict != OpenPrProbe::ProbeFailed {
                self.record_open_pr_memo(issue, verdict);
                return verdict;
            }
            if attempt < OPEN_PR_PROBE_MAX_ATTEMPTS {
                log::debug!(
                    "sweep_registry: open-PR probe for issue #{issue} failed on both \
                     transports (attempt {attempt}/{OPEN_PR_PROBE_MAX_ATTEMPTS}) — retrying \
                     once more after a short delay before falling open (#6058)"
                );
                std::thread::sleep(OPEN_PR_PROBE_RETRY_DELAY);
            }
        }

        // 2. Known-PR recheck backstop (#6788). Both transports are down; the
        //    pre-#6788 behavior was to fall open here. If a previous call
        //    verified an open linked PR for this issue, spend one cheap,
        //    targeted REST call re-confirming that specific PR instead.
        self.recheck_memoized_open_pr(issue)
            .unwrap_or(OpenPrProbe::ProbeFailed)
    }

    /// The memoized verified-`Open` answer for `issue` when it is younger than
    /// [`OPEN_PR_MEMO_FRESH`] at `now` (Issue #6788), else `None`. `None`
    /// whenever the memo is disabled via [`OPEN_PR_MEMO_ENABLE_ENV`].
    fn fresh_open_pr_memo(&self, issue: u32, now: DateTime<Utc>) -> Option<OpenPrMemoEntry> {
        if !open_pr_memo_enabled() {
            return None;
        }
        let memo = self.open_pr_memo_entry(issue)?;
        let age = now - memo.verified_at;
        let fresh = chrono::Duration::from_std(OPEN_PR_MEMO_FRESH).ok()?;
        (age >= chrono::Duration::zero() && age < fresh).then_some(memo)
    }

    /// The memoized entry for `issue` regardless of age (Issue #6788), used by
    /// the known-PR recheck backstop — an outage long enough to kill both
    /// transports is exactly when a stale memo is worth re-verifying rather
    /// than discarding.
    fn open_pr_memo_entry(&self, issue: u32) -> Option<OpenPrMemoEntry> {
        let guard = match self.open_pr_memo.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(&issue).copied()
    }

    /// Fold a **verified** probe verdict into the memo (Issue #6788).
    /// [`OpenPrProbe::Open`] records/refreshes it; [`OpenPrProbe::NoneOpen`]
    /// invalidates it immediately (so a closed or merged PR never lingers for
    /// the rest of [`OPEN_PR_MEMO_FRESH`]); [`OpenPrProbe::ProbeFailed`] leaves
    /// it untouched — an unanswered probe is not evidence either way.
    fn record_open_pr_memo(&self, issue: u32, verdict: OpenPrProbe) {
        if !open_pr_memo_enabled() {
            return;
        }
        let mut guard = match self.open_pr_memo.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match verdict {
            OpenPrProbe::Open(pr) => {
                guard.insert(
                    issue,
                    OpenPrMemoEntry {
                        pr,
                        verified_at: Utc::now(),
                    },
                );
            }
            OpenPrProbe::NoneOpen => {
                guard.remove(&issue);
            }
            OpenPrProbe::ProbeFailed => {}
        }
    }

    /// Known-PR recheck backstop (Issue #6788): re-verify the memoized linked
    /// PR for `issue` over a single targeted REST call. Returns
    /// `Some(OpenPrProbe::Open(pr))` only when that PR is confirmed **live and
    /// open**; `None` in every other case (no memo, memo disabled, the recheck
    /// could not answer, or the PR is no longer open) so the caller falls back
    /// to the unchanged [`OpenPrProbe::ProbeFailed`] fail-open contract.
    ///
    /// A confirmed-not-open answer also **invalidates** the memo, so the next
    /// tick does not re-spend this call on a PR that is already gone.
    fn recheck_memoized_open_pr(&self, issue: u32) -> Option<OpenPrProbe> {
        if !open_pr_memo_enabled() {
            return None;
        }
        let memo = self.open_pr_memo_entry(issue)?;
        match self.pull_request_is_open(memo.pr) {
            Some(true) => {
                log::info!(
                    "sweep_registry: open-PR probe for issue #{issue} failed on both transports, \
                     but its last verified linked PR #{} re-confirms as OPEN over a targeted REST \
                     recheck — holding the #4123 guard instead of falling open (#6788)",
                    memo.pr
                );
                self.record_open_pr_memo(issue, OpenPrProbe::Open(memo.pr));
                Some(OpenPrProbe::Open(memo.pr))
            }
            Some(false) => {
                log::debug!(
                    "sweep_registry: open-PR probe for issue #{issue} failed on both transports; \
                     its memoized linked PR #{} is no longer open — dropping the memo and falling \
                     open as before (#6788)",
                    memo.pr
                );
                let mut guard = match self.open_pr_memo.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.remove(&issue);
                None
            }
            None => None,
        }
    }

    /// Whether pull request `pr` is currently in the `open` state (Issue
    /// #6788), over a single non-paginated REST
    /// `GET repos/{owner}/{repo}/pulls/{pr}`. `None` on any failure
    /// (unresolvable repo, spawn error, timeout, non-zero exit, unrecognized
    /// state string) — callers MUST treat `None` as "no answer", never as a
    /// verdict.
    ///
    /// Deliberately the *cheapest* PR question available: one REST GET on a
    /// known number, billed against the REST bucket, with no closes-graph
    /// query and no `--paginate` timeline walk. That is what makes it a usable
    /// backstop for the case where both of
    /// [`probe_open_linked_pr`](Self::probe_open_linked_pr)'s transports have
    /// already failed. A merged PR reports `"closed"`, so a merged PR correctly
    /// reads as not-open here.
    fn pull_request_is_open(&self, pr: u32) -> Option<bool> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/pulls/{pr}"))
            .arg("--jq")
            .arg(".state");
        // Resolve against this registry's own workspace, matching every other
        // dispatch-path probe (#3937).
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let output = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !output.status.success() {
            return None;
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "open" => Some(true),
            "closed" => Some(false),
            // An unrecognized/empty state string is an unparseable answer, not
            // a verdict — same rule as `issue_is_closed_or_pr`.
            _ => None,
        }
    }

    /// Test-only seam (Issue #6788): plant a memo entry with an explicit
    /// verification timestamp so freshness/expiry behavior is testable without
    /// sleeping for [`OPEN_PR_MEMO_FRESH`].
    #[cfg(test)]
    pub(crate) fn seed_open_pr_memo(&self, issue: u32, pr: u32, verified_at: DateTime<Utc>) {
        let mut guard = match self.open_pr_memo.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.insert(issue, OpenPrMemoEntry { pr, verified_at });
    }

    /// One GraphQL-then-REST-fallback round of [`probe_open_linked_pr`],
    /// extracted so the #6058 retry loop above can invoke it more than once
    /// without duplicating the transport-selection logic.
    fn probe_open_linked_pr_transports(&self, issue: u32) -> OpenPrProbe {
        let graphql = self.probe_open_linked_pr_graphql(issue);
        if graphql != OpenPrProbe::ProbeFailed {
            return graphql;
        }
        log::debug!(
            "sweep_registry: GraphQL open-PR probe for issue #{issue} failed to answer \
             (commonly GraphQL quota exhaustion) — retrying over REST (#5911), a separate \
             rate-limit bucket"
        );
        self.probe_open_linked_pr_rest(issue)
    }

    /// The GraphQL closes-graph half of [`probe_open_linked_pr`] — extracted so
    /// #5911's REST fallback can retry independently without duplicating the
    /// GraphQL transport.
    fn probe_open_linked_pr_graphql(&self, issue: u32) -> OpenPrProbe {
        // Repo resolution failure is a PROBE FAILURE, not a verified absence
        // (#4452) — collapsing it into `NoneOpen` would let a partial outage
        // wrongly count a benign self-skip toward quarantine.
        let Some((owner, repo)) = self.resolve_owner_repo() else {
            return OpenPrProbe::ProbeFailed;
        };
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.args(crate::worktree_ops::gh::open_linked_pr_args(&owner, &repo, issue));
        // Resolve against this registry's own workspace, matching the label-flip
        // helpers and `issue_is_closed_or_pr` (#3937).
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        // A spawn error or timeout is a PROBE FAILURE (#4452).
        let Some(output) = output_with_timeout(cmd, reap_gh_timeout()).ok().flatten() else {
            return OpenPrProbe::ProbeFailed;
        };
        // A non-zero `gh` exit (rate limit, auth failure, transient forge error)
        // is a PROBE FAILURE, not a verified "no open PR".
        if !output.status.success() {
            return OpenPrProbe::ProbeFailed;
        }
        crate::worktree_ops::gh::parse_open_linked_pr(&String::from_utf8_lossy(&output.stdout))
    }

    /// REST fallback (#5911) for [`probe_open_linked_pr`], consulted only when
    /// the GraphQL closes-graph probe above returns [`OpenPrProbe::ProbeFailed`]
    /// (most commonly GraphQL quota exhaustion — REST is billed against a
    /// separate limit, mirroring the #4444 park-label probe's own rationale).
    ///
    /// Walks `issues/{n}/timeline` for `cross-referenced` events whose source
    /// is an OPEN pull request in this same repo — the same "source 2" union
    /// signal `/loom:sweep`'s own pre-flight existing-PR probe uses for
    /// non-closing `Part of #N` references (`sweep.md` → "Existing-PR probe").
    /// GitHub emits a `cross-referenced` event for a PR that references the
    /// issue via a **closing** keyword too, so this is a strict superset of the
    /// GraphQL closes-graph for the yes/no question this guard actually asks —
    /// it does not need to distinguish closing from non-closing references,
    /// only "is there an open PR against this issue at all".
    ///
    /// Same fail-open contract as the GraphQL probe: anything short of a
    /// verified answer (spawn/timeout error, non-zero exit, unparseable
    /// output) is [`OpenPrProbe::ProbeFailed`].
    fn probe_open_linked_pr_rest(&self, issue: u32) -> OpenPrProbe {
        let Some((owner, repo)) = self.resolve_owner_repo() else {
            return OpenPrProbe::ProbeFailed;
        };
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        let filter = format!(
            "[.[] | select(.event == \"cross-referenced\" \
             and .source.issue.pull_request != null \
             and .source.issue.state == \"open\" \
             and .source.issue.repository.full_name == \"{owner}/{repo}\") \
             | .source.issue.number] | unique | .[0] // empty"
        );
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{issue}/timeline"))
            .arg("--paginate")
            .arg("--jq")
            .arg(filter);
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let Some(output) = output_with_timeout(cmd, reap_gh_timeout()).ok().flatten() else {
            return OpenPrProbe::ProbeFailed;
        };
        if !output.status.success() {
            return OpenPrProbe::ProbeFailed;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return OpenPrProbe::NoneOpen;
        }
        match trimmed.lines().next().unwrap_or("").trim().parse::<u32>() {
            Ok(pr) => OpenPrProbe::Open(pr),
            Err(_) => OpenPrProbe::ProbeFailed,
        }
    }

    /// Best-effort probe for whether `issue` resolves to a pull request in ANY
    /// state (Issue #4653), used by [`restore_label_to_ready`] to refuse
    /// re-adding `loom:issue` to a PR number during cancel/crash-recovery.
    /// Returns `Some(true)` when the number is a PR, `Some(false)` when it is
    /// a genuine issue, and `None` on any probe failure (missing/failed/
    /// timed-out `gh`, unresolvable repo, unparseable output).
    ///
    /// Mirrors the same REST call as [`issue_is_closed_or_pr`] (the `is_pr`
    /// half of its payload), kept as a separate probe so this fix does not
    /// touch the 2.5 dispatch guard or its tests. Callers MUST treat `None`
    /// as **fail-open** — a forge outage must never block the `loom:building`
    /// cleanup that [`restore_label_to_ready`] performs regardless of this
    /// probe's outcome.
    ///
    /// [`restore_label_to_ready`]: Self::restore_label_to_ready
    /// [`issue_is_closed_or_pr`]: Self::issue_is_closed_or_pr
    pub(crate) fn issue_is_pull_request(&self, issue: u32) -> Option<bool> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{issue}"))
            .arg("--jq")
            .arg(".pull_request != null");
        // Resolve against this registry's own workspace, matching the other
        // dispatch-path probes (#3937).
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let output = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !output.status.success() {
            return None;
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// Best-effort probe for the first [`PARK_LABELS`] entry currently on
    /// `issue`, used by the #4444 park-label dispatch guard (step 2.7). Returns
    /// `Some(label)` when the issue carries a park label and `None` otherwise —
    /// where `None` covers BOTH "not parked" and any failure (missing/failed/
    /// timed-out `gh`, unresolvable repo, unparseable output).
    ///
    /// Callers MUST treat `None` as **fail-open**, matching
    /// [`issue_is_closed_or_pr`](Self::issue_is_closed_or_pr) and
    /// [`probe_open_linked_pr`](Self::probe_open_linked_pr)'s `ProbeFailed`: a
    /// forge outage or a wedged `gh` must never wedge dispatch. Bounded by
    /// [`reap_gh_timeout`] like every other dispatch-path `gh` call (#3973).
    ///
    /// Deliberately probes over **REST** (`gh api repos/{owner}/{repo}/issues/N`)
    /// rather than `gh issue view --json labels` (which rides GraphQL, like
    /// steps 2.5/2.6): REST is a separate rate-limit bucket, so a deliberate park
    /// still holds while the GraphQL quota is exhausted — exactly the condition
    /// under which the #4123 open-PR guard failed open during the 2026-07-29
    /// incident. The `--jq` emits one label name per line.
    ///
    /// The returned label is chosen by [`PARK_LABELS`] order, not forge order, so
    /// the refusal message is deterministic when an issue carries both.
    ///
    /// One narrow exemption applies since #6893 — see
    /// [`mechanical_capability_exempt`](Self::mechanical_capability_exempt).
    /// Keeping it here rather than only in the work-finder's own candidate
    /// filter is what makes the capability lane actually reachable: this guard
    /// covers all six dispatch routes, so an item the finder un-parked would
    /// otherwise be refused two steps later by this very probe.
    ///
    /// [`PARK_LABELS`]: crate::work_finder::PARK_LABELS
    pub(crate) fn first_park_label(&self, issue: u32) -> Option<String> {
        let labels = self.current_labels_via_rest(issue)?;
        // `PARK_LABELS` order, not forge order, so the refusal is deterministic
        // when an issue carries both. `loom:blocked` sorts first, so an item
        // carrying it never reaches the exemption below.
        let park = crate::work_finder::PARK_LABELS
            .iter()
            .find(|park| labels.iter().any(|l| l == *park))?;
        if **park == *crate::capability::OPERATOR_ONLY_LABEL
            && self.mechanical_capability_exempt(issue, &labels)
        {
            return None;
        }
        Some((*park).to_string())
    }

    /// True when `issue` is a `loom:operator-mechanical` item whose declared
    /// `<!-- loom:capability=<name> -->` requirements (#6892) are **fully** held
    /// by this host, and it may therefore be dispatched into the propose-mode
    /// lane instead of parked by `loom:operator-only` (#6893, AC1).
    ///
    /// Fails closed at every step, in this order — each `false` leaves the park
    /// in force:
    ///
    /// 1. **This host declared no capabilities** (the default: no
    ///    `LOOM_WORKER_CAPABILITIES`). Checked first and cheapest, so the common
    ///    path costs one `BTreeSet::is_empty` and **zero extra forge calls**.
    /// 2. **The labels are not the mechanical shape** — the other three
    ///    `loom:operator-only` sub-kinds, `loom:blocked`, `loom:needs-capability`
    ///    and `loom:operator` are all refused here, from the labels this guard
    ///    already fetched. Still no extra forge call.
    /// 3. **The body could not be read** — a `gh` failure/timeout means we could
    ///    not see the declaration, and an unreadable declaration is treated
    ///    exactly like an absent one. Note this is the *opposite* of the
    ///    surrounding guard's fail-**open** convention, and deliberately so: for
    ///    the guard, failing open means "dispatch normally"; here, failing open
    ///    would mean "override a human's park on a hunch."
    /// 4. **The declaration is empty, unrecognized, or not fully held** — see
    ///    [`crate::capability::route_mechanical`].
    ///
    /// The body read (step 3) is a second REST call on the same endpoint as
    /// [`current_labels_via_rest`](Self::current_labels_via_rest), reached only
    /// by items that already passed steps 1 and 2 — i.e. essentially never,
    /// unless a host has opted into the lane *and* is looking at a mechanical
    /// item.
    fn mechanical_capability_exempt(&self, issue: u32, labels: &[String]) -> bool {
        let held = crate::capability::held_capabilities();
        if held.is_empty() || !crate::capability::labels_eligible_for_capability_lane(labels) {
            return false;
        }
        let Some(body) = self.issue_body_via_rest(issue) else {
            log::info!(
                "issue #{issue}: could not read the body to check its capability declaration; \
                 keeping the `loom:operator-only` park (#6893 fails closed)"
            );
            return false;
        };
        let routing = crate::capability::route_mechanical(labels, Some(&body), &held);
        if routing.is_dispatchable() {
            log::info!(
                "issue #{issue}: `loom:operator-mechanical` capability declaration is satisfied by \
                 this host; dispatching into the PROPOSE-ONLY lane (#6893 AC4 — the worker \
                 produces commands/a PR for an operator to approve, never live execution)"
            );
            return true;
        }
        if let crate::capability::MechanicalRouting::MissingCapabilities { missing } = &routing {
            log::info!(
                "issue #{issue}: `loom:operator-mechanical` declares capability/ies this host does \
                 not hold ({}); keeping the `loom:operator-only` park (#6893)",
                missing.join(", ")
            );
        }
        false
    }

    /// Read `issue`'s markdown body over the same REST endpoint
    /// [`current_labels_via_rest`](Self::current_labels_via_rest) uses. `None` on
    /// any failure; callers treat that as "no declaration" (fail closed).
    pub(crate) fn issue_body_via_rest(&self, issue: u32) -> Option<String> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{issue}"))
            .arg("--jq")
            .arg(".body // \"\"");
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let output = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Read `issue`'s current label names over the GitHub REST API. `None` on any
    /// failure (see [`first_park_label`](Self::first_park_label) for the
    /// fail-open contract); `Some(vec![])` for an issue with no labels, which is
    /// a *successful* read and must stay distinguishable from a failed one.
    pub(crate) fn current_labels_via_rest(&self, issue: u32) -> Option<Vec<String>> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{issue}"))
            .arg("--jq")
            .arg(".labels[].name");
        // Resolve against this registry's own workspace, matching the label-flip
        // helpers and the other dispatch-path probes (#3937).
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let output = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !output.status.success() {
            return None;
        }
        Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect(),
        )
    }

    /// Resolve `(owner, repo)` for the registry's workspace, needed because
    /// `gh api` (unlike `gh issue view`) cannot infer the repo from the working
    /// directory — neither the GraphQL open-PR probe nor the REST park-label
    /// probe accepts a `--repo` flag. Prefers the process-global `LOOM_REPO`
    /// override (`owner/repo`) when set, else asks `gh repo view` in the
    /// workspace root. Returns `None` on any failure so both guards fail open.
    pub(crate) fn resolve_owner_repo(&self) -> Option<(String, String)> {
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            if let Some((o, r)) = repo.split_once('/') {
                if !o.is_empty() && !r.is_empty() {
                    return Some((o.to_string(), r.to_string()));
                }
            }
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("repo")
            .arg("view")
            .arg("--json")
            .arg("owner,name")
            .arg("--jq")
            .arg(".owner.login + \"/\" + .name");
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let output = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.trim().split_once('/').and_then(|(o, r)| {
            if o.is_empty() || r.is_empty() {
                None
            } else {
                Some((o.to_string(), r.to_string()))
            }
        })
    }

    pub(crate) fn flip_label_to_building(&self, issue: u32) -> Result<()> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:issue")
            .arg("--add-label")
            .arg("loom:building");
        // Run in the registry's own workspace so the issue number resolves
        // against *this* repo, not the daemon process's cwd repo. Without this
        // a multi-workspace daemon (#3928) flips labels against the wrong repo
        // (`GraphQL: Could not resolve to an issue ...`) — see #3937. The
        // process-global LOOM_REPO override still wins when set.
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        // Bounded so a wedged `gh` can never block the dispatch/read path
        // indefinitely (Issue #3973); stdio piping is forced by the helper.
        let timeout = reap_gh_timeout();
        match output_with_timeout(cmd, timeout)
            .with_context(|| format!("failed to invoke {} for issue #{issue}", gh.display()))?
        {
            Some(output) if output.status.success() => Ok(()),
            Some(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(anyhow!("gh issue edit failed for #{issue}: {}", stderr.trim()))
            }
            None => Err(anyhow!(
                "gh issue edit for #{issue} exceeded {}s and was killed (#3973)",
                timeout.as_secs()
            )),
        }
    }

    /// Whether the lease publishers below should publish this host's RAW
    /// [`host_identity()`] value rather than [`opaque_host_id`] (Issue
    /// #6322). Reads [`LEASE_PUBLISH_HOSTNAME_ENV`] only — there is
    /// deliberately no per-repo config key for this: a shell counterpart
    /// (`defaults/scripts/sweep-lease-fence.sh`) must derive the exact same
    /// answer this process does with no access to `loom-daemon`'s own
    /// `.loom/config.json` resolution, so env is the only source both sides
    /// can agree on without risking silent divergence between the writer and
    /// the sweep-side fencing reader.
    fn lease_publish_raw_hostname(&self) -> bool {
        std::env::var(LEASE_PUBLISH_HOSTNAME_ENV)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    }

    /// The host identity to PUBLISH in a `loom:lease`/`loom:lease-yield`
    /// forge comment (Issue #6322) — [`opaque_host_id`] of
    /// [`host_identity()`] by default, or the raw value when
    /// [`lease_publish_raw_hostname`](Self::lease_publish_raw_hostname) opts
    /// in via [`LEASE_PUBLISH_HOSTNAME_ENV`].
    ///
    /// **Every** publisher of a lease-shaped forge comment
    /// ([`write_lease_comment`](Self::write_lease_comment),
    /// [`post_lease_yield_comment`](Self::post_lease_yield_comment)) AND the
    /// one reconciliation reader that must recognize "this dispatcher's own
    /// comment" ([`resolve_lease_order`](Self::resolve_lease_order)) call
    /// this method rather than `host_identity()` directly — centralizing the
    /// transform here means a future publisher can never forget it, and
    /// `resolve_lease_order`'s own-claim recognition can never drift out of
    /// sync with what was actually published.
    ///
    /// Logs (once per process) when it publishes the opaque form, so an
    /// operator reading this daemon's own log has a local, self-contained
    /// pointer back to the raw hostname — see `defaults/docs/lease-record.md`
    /// for the full resolution recipe.
    pub(crate) fn published_host_id(&self) -> String {
        let host = host_identity();
        if self.lease_publish_raw_hostname() {
            return host;
        }
        let opaque = opaque_host_id(&host);
        static LOGGED: OnceLock<()> = OnceLock::new();
        LOGGED.get_or_init(|| {
            log::info!(
                "sweep_registry: loom:lease forge comments publish the opaque id {opaque} for \
                 this host (raw hostname {host:?} kept out of public issue-tracker comments, \
                 Issue #6322) — recompute `opaque_host_id` locally against a candidate hostname \
                 to confirm a match, or set ${LEASE_PUBLISH_HOSTNAME_ENV}=1 to restore raw \
                 hostnames (see defaults/docs/lease-record.md)"
            );
        });
        opaque
    }

    /// Write a lease record (Issue #6179, Epic #6165 Phase 1) — a best-effort
    /// forge comment posted at the moment a dispatch successfully flips
    /// `loom:building`, so the claim gains a liveness dimension (a lease)
    /// that a *future* phase can read to decide reclamation. This function
    /// only ever writes; nothing in this registry parses the comment back —
    /// see [`LEASE_MARKER_PREFIX`]'s doc comment and
    /// `defaults/docs/lease-record.md` for the full format contract.
    ///
    /// Called from exactly one call site —
    /// [`SweepRegistry::dispatch_inner`](Self::dispatch_inner), immediately
    /// after a successful [`flip_label_to_building`](Self::flip_label_to_building)
    /// — and only on that success: a failed label flip means there is no
    /// claim to advertise a lease for. Skipped when label flips are disabled
    /// (test fixtures / `skip_label_flip`), matching every other best-effort
    /// forge mutation in this registry. Fail-open like `watchdog.rs`'s
    /// `post_watchdog_gaveup_comment`: a `gh` failure here only logs (at
    /// `warn`) and never propagates — posting a lease record must never fail
    /// dispatch or undo the claim it documents.
    pub(crate) fn write_lease_comment(&self, issue: u32, sweep_id: &str) {
        if self.config.skip_label_flip {
            return;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let host = self.published_host_id();
        let body = format!(
            "{prefix}{host} sweep={sweep_id} -->\n\
             This issue's `loom:building` claim was acquired by sweep `{sweep_id}` on host \
             `{host}` at {ts}. This comment is a **lease record** (Issue #6179, Epic #6165) — \
             its liveness signal is this comment's own forge-assigned `updated_at`, never a \
             timestamp embedded in this text. See `defaults/docs/lease-record.md` for the \
             format contract this establishes, and `defaults/docs/lease-renewal.md` for how \
             the owning sweep keeps it fresh for the lifetime of its claim. Nothing reads this \
             record yet (write-only, Phase 1) — a future phase will use it to decide \
             reclamation of an abandoned claim.",
            prefix = LEASE_MARKER_PREFIX,
            host = host,
            sweep_id = sweep_id,
            ts = Utc::now().to_rfc3339(),
        );
        let mut comment = Command::new(&gh);
        comment
            .arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(body);
        // Run in the registry's own workspace so the issue number resolves
        // against *this* repo in a multi-workspace daemon (#3928/#3937),
        // mirroring every other forge mutation in this file.
        comment.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut comment,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            comment.arg("--repo").arg(repo);
        }
        // Bounded so a wedged `gh` can never block the dispatch path (#3973),
        // exactly like the label flip this immediately follows.
        let timeout = reap_gh_timeout();
        match output_with_timeout(comment, timeout) {
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => log::warn!(
                "lease comment for #{issue} exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(None) => log::warn!(
                "lease comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::warn!("lease comment for #{issue} failed: {e}"),
        }
    }

    // ------------------------------------------------------------------------
    // Claim-then-verify-order dedup at dispatch time (Issue #6287, Epic #6165
    // Phase 2)
    // ------------------------------------------------------------------------
    //
    // Phase 1 (#6179) wrote a lease record and left it write-only: nothing
    // read it back. This phase closes that loop for the exact race #4028
    // named as the historical failure mode of label-only forge claiming: two
    // dispatchers flip `loom:issue` -> `loom:building` for the same issue in
    // the same window, both succeed (the flip is unconditionally idempotent
    // — `gh issue edit --add-label` does not fail when the label is already
    // present), and both would otherwise proceed to spawn a duplicate
    // builder. The lease comment each dispatcher writes right after its own
    // flip (4b) gives every host a shared, forge-assigned sequencer: read
    // back every live lease comment on the issue and let the one with the
    // EARLIEST server-assigned comment `id` — never a locally-recorded
    // timestamp — proceed. Every other dispatcher yields before doing any
    // real work (spawning a builder, creating/entering a worktree).

    /// Parse `host=`/`sweep=` out of a lease comment body's literal first
    /// line (Issue #6179's format, reused verbatim by #6287's read-back and
    /// available for #6286's reclamation guard to reuse rather than
    /// re-deriving). Only the first line is inspected — the format contract
    /// forbids depending on anything past the marker's closing `-->`.
    pub(crate) fn parse_lease_marker_line(body: &str) -> Option<(String, String)> {
        let first_line = body.lines().next()?;
        let rest = first_line.strip_prefix(LEASE_MARKER_PREFIX)?;
        let rest = rest.strip_suffix(" -->")?;
        let (host, sweep_id) = rest.split_once(" sweep=")?;
        if host.is_empty() || sweep_id.is_empty() {
            return None;
        }
        Some((host.to_string(), sweep_id.to_string()))
    }

    /// Parse the newline-delimited JSON (NDJSON) [`read_lease_comments`]'s
    /// (`Self::read_lease_comments`) `gh api ... --jq` call emits — one
    /// `{id, created_at, body}` object per line, not a `[...]`-wrapped array.
    ///
    /// This repo already hit and fixed the array-literal version of this bug
    /// once (Issue #4637, see `parse_max_timestamp` in
    /// `claim_reconciliation.rs`): `gh api --paginate --jq` re-invokes the
    /// `--jq` filter once per response page and concatenates the raw
    /// per-page output, rather than applying the filter across the combined
    /// result set. A `[...]`-wrapped filter turns a multi-page result into
    /// two or more concatenated array literals (`[...][...]`), which is not
    /// valid JSON and fails to parse as a whole. NDJSON has no such
    /// wrapper to corrupt — each page's output is still one complete JSON
    /// value per line, so concatenating pages just adds more lines.
    ///
    /// Each line is parsed independently; a line that is not a JSON object,
    /// or whose marker line / `id` fails to parse, is silently dropped
    /// rather than failing the whole batch (defensive — the `--jq` filter
    /// already selects on the marker prefix, so this should rarely trigger
    /// against a real forge response). Always succeeds: a comments read only
    /// reaches this function after `read_lease_comments` has already
    /// confirmed a zero exit, so "no lines parsed" means "verified zero
    /// lease comments", not "read failed" — there is no `None` case left to
    /// return.
    pub(crate) fn parse_lease_comments_json(stdout: &[u8]) -> Vec<LeaseComment> {
        let raw = String::from_utf8_lossy(stdout);
        let mut out = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(item) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            let Some(id) = item.get("id").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let created_at = item
                .get("created_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let Some(body) = item.get("body").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some((host, sweep_id)) = Self::parse_lease_marker_line(body) else {
                continue;
            };
            out.push(LeaseComment {
                id,
                created_at,
                host,
                sweep_id,
            });
        }
        out
    }

    /// Read back every lease-record comment currently on `issue` (Issue
    /// #6287). Uses REST (`gh api repos/{owner}/{repo}/issues/N/comments`),
    /// the same transport [`current_labels_via_rest`](Self::current_labels_via_rest)
    /// and [`fetch_claim_labeled_at`](Self::fetch_claim_labeled_at) use, so
    /// this read rides a separate rate-limit bucket from the GraphQL calls
    /// earlier dispatch guards use. `--jq` pre-filters to comments whose body
    /// starts with [`LEASE_MARKER_PREFIX`] client-side (applied by the `gh`
    /// binary to the already-downloaded response — the full comment payload
    /// still crosses the wire; only the *output* is filtered, sparing the
    /// caller from unrelated comment volume).
    ///
    /// The `--jq` filter emits one JSON object per line (NDJSON), not a
    /// `[...]`-wrapped array — see [`parse_lease_comments_json`]'s doc
    /// comment for why: `--paginate` re-invokes `--jq` once per response page
    /// (Issue #4637), and an array-literal filter turns a multi-page result
    /// into `[...][...]`, which is not valid JSON.
    ///
    /// FAIL-OPEN: returns `None` on any unresolved repo, timeout, or
    /// non-zero exit — callers MUST treat `None` as "unverifiable, do not
    /// block", matching every other forge probe in this module.
    ///
    /// [`parse_lease_comments_json`]: Self::parse_lease_comments_json
    pub(crate) fn read_lease_comments(&self, issue: u32) -> Option<Vec<LeaseComment>> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{issue}/comments"))
            .arg("--paginate")
            .arg("--jq")
            .arg(format!(
                r#".[] | select(.body | startswith("{prefix}")) | {{id: .id, created_at: .created_at, body: .body}}"#,
                prefix = LEASE_MARKER_PREFIX,
            ));
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        let timeout = reap_gh_timeout();
        let output = output_with_timeout(cmd, timeout).ok().flatten()?;
        if !output.status.success() {
            return None;
        }
        Some(Self::parse_lease_comments_json(&output.stdout))
    }

    /// The claim-then-verify-order tie-break itself (Issue #6287, Epic #6165
    /// Phase 2): after this dispatcher has already flipped `loom:building`
    /// and written its own lease comment (4a/4b in
    /// [`dispatch_inner`](Self::dispatch_inner)), re-read every live lease
    /// comment on `issue` and decide whether THIS dispatcher's own comment
    /// (identified by [`published_host_id`](Self::published_host_id) +
    /// `sweep_id`) is the earliest one —
    /// by forge-assigned comment `id`, never a locally-recorded timestamp —
    /// among those written within [`LEASE_ORDER_LOOKBACK_SECS`] of
    /// `episode_start` (the instant this dispatch attempt began its own
    /// flip, passed by the caller so the bound is anchored to THIS
    /// dispatch's local clock rather than re-reading `Utc::now()` here).
    ///
    /// FAIL-OPEN in every ambiguous case — an unreadable forge
    /// ([`read_lease_comments`] returns `None`), or this dispatcher's own
    /// comment still not found after [`LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS`]
    /// read-backs (its write may have genuinely failed, not merely raced
    /// ahead of forge-side propagation) — resolves to
    /// [`LeaseOrderDecision::Proceed`]: this mechanism only ever ADDS a
    /// refusal on POSITIVE evidence of a peer's earlier claim, it never
    /// invents one from an unverifiable read.
    ///
    /// # Issue #6816: bounded retry on "own comment not found"
    ///
    /// A single immediate read-back of "own comment not found" is
    /// ambiguous between two very different situations: the write
    /// genuinely failed (nothing to compare against, correctly fail open),
    /// or the write succeeded but this read raced ahead of the forge's own
    /// read-after-write propagation for the *list comments* endpoint (the
    /// comment exists, but is not yet visible to this GET). Two dispatchers
    /// racing within a few seconds of each other are exactly the scenario
    /// most likely to hit the second case on **both** sides at once: each
    /// posts its own lease comment and immediately reads it back before the
    /// peer's — or even its own — write has settled, and a single-shot
    /// fail-open lets both sides conclude "unverifiable, proceed" instead of
    /// exactly one of them finding the other's earlier comment. Retrying the
    /// read a few times, with a short delay, closes most of that
    /// window — mirroring the existing
    /// [`OPEN_PR_PROBE_MAX_ATTEMPTS`]/[`OPEN_PR_PROBE_RETRY_DELAY`] pattern
    /// this module already uses for the same class of transient-forge-read
    /// ambiguity — while still falling open exactly as before once the
    /// retries are exhausted, so a genuinely failed write (or a real outage)
    /// can never wedge dispatch.
    ///
    /// [`read_lease_comments`]: Self::read_lease_comments
    pub(crate) fn resolve_lease_order(
        &self,
        issue: u32,
        sweep_id: &str,
        episode_start: DateTime<Utc>,
    ) -> LeaseOrderDecision {
        // Must match exactly what `write_lease_comment` PUBLISHED for this
        // host (Issue #6322) — comparing against raw `host_identity()` here
        // while the publisher writes `opaque_host_id(host_identity())` would
        // silently break "recognize my own claim" for every dispatch once
        // publishing goes opaque.
        let host = self.published_host_id();
        let cutoff = episode_start - chrono::Duration::seconds(LEASE_ORDER_LOOKBACK_SECS);

        for attempt in 1..=LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS {
            let Some(comments) = self.read_lease_comments(issue) else {
                return LeaseOrderDecision::Proceed;
            };
            let in_window: Vec<&LeaseComment> = comments
                .iter()
                .filter(|c| c.created_at.is_some_and(|ts| ts >= cutoff))
                .collect();
            let Some(own_id) = in_window
                .iter()
                .filter(|c| c.host == host && c.sweep_id == sweep_id)
                .map(|c| c.id)
                .min()
            else {
                // Ambiguous: this dispatcher's own just-written comment is
                // not visible yet. Retry a few times (read-after-write lag)
                // before giving up and falling open (#6816).
                if attempt < LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS {
                    log::debug!(
                        "sweep_registry: lease-order read-back for issue #{issue} \
                         sweep={sweep_id} did not find this dispatcher's own comment yet \
                         (attempt {attempt}/{LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS}) — retrying \
                         after a short delay before falling open (#6816)"
                    );
                    std::thread::sleep(LEASE_ORDER_OWN_COMMENT_RETRY_DELAY);
                    continue;
                }
                return LeaseOrderDecision::Proceed;
            };
            let Some(earliest) = in_window.iter().min_by_key(|c| c.id) else {
                return LeaseOrderDecision::Proceed;
            };
            return if earliest.id < own_id {
                LeaseOrderDecision::Yield {
                    earliest_host: earliest.host.clone(),
                    earliest_sweep_id: earliest.sweep_id.clone(),
                }
            } else {
                // Own comment currently looks earliest (or sole) in-window
                // — confirm with a few bounded re-reads before committing
                // to Proceed, since a genuine peer's earlier comment may
                // simply not have propagated into THIS read yet (#6951).
                self.confirm_sole_claim(issue, sweep_id, &host, cutoff)
            };
        }
        // Unreachable in practice (the loop above always returns within its
        // body), but keeps the function total without relying on that.
        LeaseOrderDecision::Proceed
    }

    /// Confirmation phase for [`resolve_lease_order`](Self::resolve_lease_order)
    /// (Issue #6951): re-reads live lease comments up to
    /// [`LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS`] times, each after
    /// [`LEASE_ORDER_SOLE_CLAIM_CONFIRM_DELAY`], to give a
    /// slower-propagating peer comment a chance to appear before this
    /// dispatcher commits to [`LeaseOrderDecision::Proceed`]. Yields the
    /// moment an earlier peer comment becomes visible; otherwise falls open
    /// to `Proceed` once the confirmation budget is exhausted — like the
    /// rest of this tie-break, this phase only ever ADDS a refusal on
    /// positive evidence of a peer's earlier claim, it never invents one
    /// from an unverifiable or merely-absent read.
    fn confirm_sole_claim(
        &self,
        issue: u32,
        sweep_id: &str,
        host: &str,
        cutoff: DateTime<Utc>,
    ) -> LeaseOrderDecision {
        for attempt in 1..=LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS {
            std::thread::sleep(LEASE_ORDER_SOLE_CLAIM_CONFIRM_DELAY);
            let Some(comments) = self.read_lease_comments(issue) else {
                return LeaseOrderDecision::Proceed;
            };
            let in_window: Vec<&LeaseComment> = comments
                .iter()
                .filter(|c| c.created_at.is_some_and(|ts| ts >= cutoff))
                .collect();
            let Some(own_id) = in_window
                .iter()
                .filter(|c| c.host == host && c.sweep_id == sweep_id)
                .map(|c| c.id)
                .min()
            else {
                // Own comment is no longer visible in this read — ambiguous
                // (never invent a refusal from an unverifiable read); fall
                // open, matching every other branch of this tie-break.
                return LeaseOrderDecision::Proceed;
            };
            if let Some(earliest) = in_window.iter().min_by_key(|c| c.id) {
                if earliest.id < own_id {
                    log::info!(
                        "sweep_registry: lease-order confirmation re-read for issue #{issue} \
                         sweep={sweep_id} found a peer comment (host={}, sweep={}) that had not \
                         propagated on the initial read — yielding (confirm attempt {attempt}/\
                         {LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS}, #6951)",
                        earliest.host,
                        earliest.sweep_id,
                    );
                    return LeaseOrderDecision::Yield {
                        earliest_host: earliest.host.clone(),
                        earliest_sweep_id: earliest.sweep_id.clone(),
                    };
                }
            }
        }
        LeaseOrderDecision::Proceed
    }

    /// Best-effort annotation posted when this dispatcher yields a
    /// claim-then-verify-order tie-break (Issue #6287) — the "annotate" half
    /// of the epic body's "release/annotate its own claim" contract. Chosen
    /// over restoring `loom:building` -> `loom:issue`: the forge label is
    /// idempotent across both racing flips (there is exactly one
    /// `loom:building` regardless of how many dispatchers flipped it), so it
    /// is *already* correct and protects the winning claimant — reverting it
    /// here would destroy that winner's only cross-host mutex out from under
    /// its still-live sweep (the exact loom#5270 failure mode
    /// [`restore_label_to_ready`](Self::restore_label_to_ready)'s own doc
    /// comment describes, reproduced from a different call site). This
    /// comment is purely an observability/audit trail; nothing reads it
    /// back. Fail-open like [`write_lease_comment`](Self::write_lease_comment):
    /// a `gh` failure here only logs and never propagates.
    pub(crate) fn post_lease_yield_comment(
        &self,
        issue: u32,
        sweep_id: &str,
        earliest_host: &str,
        earliest_sweep_id: &str,
    ) {
        if self.config.skip_label_flip {
            return;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let host = self.published_host_id();
        let body = format!(
            "<!-- loom:lease-yield host={host} sweep={sweep_id} earliest_host={earliest_host} \
             earliest_sweep={earliest_sweep_id} -->\n\
             This dispatcher's own lease record (sweep `{sweep_id}` on host `{host}`) was NOT the \
             earliest live lease comment on this issue — a lease from sweep `{earliest_sweep_id}` \
             on host `{earliest_host}` has an earlier forge-assigned comment order. Standing down \
             before spawning a builder or touching a worktree (Issue #6287, Epic #6165 Phase 2 \
             claim-then-verify-order tie-break). The `loom:building` label is left untouched — it \
             is already correct, protecting the earlier claimant's own winning lease.",
        );
        let mut comment = Command::new(&gh);
        comment
            .arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(body);
        comment.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut comment,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            comment.arg("--repo").arg(repo);
        }
        let timeout = reap_gh_timeout();
        match output_with_timeout(comment, timeout) {
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => log::warn!(
                "lease-yield comment for #{issue} exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(None) => log::warn!(
                "lease-yield comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::warn!("lease-yield comment for #{issue} failed: {e}"),
        }
    }

    /// Restore a crashed/orphaned claim's `loom:building` back to
    /// `loom:issue` — UNLESS the issue currently carries `loom:blocked`
    /// (Issue #4206), `loom:operator-only` (Issue #4887), OR the target
    /// number actually resolves to a pull request (Issue #4653). A
    /// deliberate operator park (applied by hand, possibly while the now-dead
    /// sweep was still `loom:building`) must never be clobbered into an
    /// illegal `loom:blocked`/`loom:operator-only` + `loom:issue` combo by
    /// the crash-recovery path, and a PR number must never be handed a
    /// `loom:issue` label meant for issues — that reproduces the stray-label
    /// symptom from #4653's incident report, reached whenever the 2.5
    /// dispatch guard's fail-open window lets a PR number through and the
    /// sweep is later cancelled or crash-recovered.
    ///
    /// The `loom:operator-only` carve-out closes the #4887 race: a Builder
    /// that aborts mid-build (e.g. missing OAuth scope, or an operator RETIRE
    /// decision) correctly re-routes the issue `loom:building` ->
    /// `loom:operator-only` itself, then exits; when the reaper later notices
    /// the dead child and calls this restore, an unconditional restore would
    /// re-add `loom:issue` on top of that reroute ~25-30s later with no
    /// accompanying comment, leaving the issue in the illegal
    /// `loom:operator-only` + `loom:issue` combo and re-queuing it for the
    /// exact same blocker.
    ///
    /// All checks are best-effort and fail-open: an unverifiable read falls
    /// back to the pre-#4206 unconditional restore (re-adding `loom:issue`),
    /// since a stranded `loom:building` claim is the more common failure mode
    /// this path exists to fix. Every carve-out only skips the `loom:issue`
    /// re-add — the stale `loom:building` claim is always removed.
    pub(crate) fn restore_label_to_ready(&self, issue: u32) -> Result<()> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let blocked = self.issue_has_blocked_label(issue);
        // Only probe `loom:operator-only` when `loom:blocked` doesn't already
        // decide the outcome — avoids a redundant `gh` call in the (more
        // common) blocked path.
        let operator_only = !blocked && self.issue_has_operator_only_label(issue);
        let parked = blocked || operator_only;
        // Only probe PR-ness when a park carve-out doesn't already decide the
        // outcome — avoids a redundant `gh` call on the parked path.
        let is_pr = !parked && self.issue_is_pull_request(issue).unwrap_or(false);
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:building");
        if blocked {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found `loom:blocked` \
                 already present — preserving the operator's park by removing the stale \
                 `loom:building` claim only, NOT re-adding `loom:issue` (#4206)"
            );
        } else if operator_only {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found \
                 `loom:operator-only` already present — preserving the authoritative \
                 reroute by removing the stale `loom:building` claim only, NOT re-adding \
                 `loom:issue` (#4887)"
            );
        } else if is_pr {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} resolved to a pull \
                 request — removing the stale `loom:building` claim only, NOT re-adding \
                 `loom:issue` (#4653)"
            );
        } else {
            cmd.arg("--add-label").arg("loom:issue");
        }
        // Scope the restore to the registry's workspace so the crash-path label
        // recovery resolves against the right repo in a multi-workspace daemon
        // (#3937). LOOM_REPO still overrides when set.
        cmd.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        // Best-effort during reap, but bounded so a wedged `gh` on the
        // `ListSweeps` / `GetSweepStatus` read path cannot block the registry
        // read indefinitely (Issue #3973).
        let timeout = reap_gh_timeout();
        if output_with_timeout(cmd, timeout)?.is_none() {
            log::warn!(
                "sweep_registry: restore_label_to_ready gh for #{issue} exceeded {}s \
                 and was killed (#3973)",
                timeout.as_secs()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// #4452 unit coverage: the three-state probe distinguishes a VERIFIED
    /// "no open PR" (empty `graphql` stdout, exit 0) from a PROBE FAILURE
    /// (`graphql` exits non-zero), where the old `Option<u32>` returned `None`
    /// for both. Also covers the `Open` verdict and the unparseable-stdout leg.
    #[test]
    fn probe_open_linked_pr_distinguishes_none_open_from_probe_failure() {
        // Verified no open PR: gh succeeds with empty stdout.
        let dir = tempdir().unwrap();
        let reg = no_progress_test_registry(dir.path(), "OPEN", "", false);
        assert_eq!(
            reg.probe_open_linked_pr(9001),
            OpenPrProbe::NoneOpen,
            "empty successful graphql output is a VERIFIED absence"
        );

        // Verified open PR: gh succeeds and prints a PR number.
        let dir = tempdir().unwrap();
        let reg = no_progress_test_registry(dir.path(), "OPEN", "9100", false);
        assert_eq!(
            reg.probe_open_linked_pr(9002),
            OpenPrProbe::Open(9100),
            "a parseable PR number is a VERIFIED open PR"
        );

        // Probe failure: gh api graphql exits non-zero.
        let dir = tempdir().unwrap();
        let reg = no_progress_pr_probe_fail_registry(dir.path(), "OPEN");
        assert_eq!(
            reg.probe_open_linked_pr(9003),
            OpenPrProbe::ProbeFailed,
            "a non-zero graphql exit is a PROBE FAILURE, not a verified absence"
        );

        // Probe failure: gh exits 0 but stdout is wholly unparseable (a
        // truncated/garbled response), which must NOT be read as a verified
        // absence.
        let dir = tempdir().unwrap();
        let reg = no_progress_test_registry(dir.path(), "OPEN", "not-a-number", false);
        assert_eq!(
            reg.probe_open_linked_pr(9004),
            OpenPrProbe::ProbeFailed,
            "unparseable non-empty stdout is a PROBE FAILURE (#4452)"
        );
    }

    /// #5911: when the GraphQL closes-graph probe cannot answer (e.g. quota
    /// exhaustion), the REST timeline fallback recovers a VERIFIED `Open`
    /// instead of the pre-#5911 unconditional `ProbeFailed` — this is exactly
    /// the gap that let the #4123 dispatch guard fall open and repeatedly
    /// re-dispatch #5565 despite its PR #5569 being open the whole time.
    #[test]
    fn probe_open_linked_pr_rest_fallback_recovers_open_when_graphql_fails() {
        let dir = tempdir().unwrap();
        let (reg, _log) = open_pr_guard_rest_fallback_registry(dir.path(), "5569", 0, true);
        assert_eq!(
            reg.probe_open_linked_pr(5565),
            OpenPrProbe::Open(5569),
            "GraphQL failure must retry over REST (a separate rate-limit bucket) before \
             falling open (#5911)"
        );
    }

    /// #5911: the REST fallback's own VERIFIED "no open PR" answer is trusted
    /// (still `NoneOpen`, not `ProbeFailed`) when GraphQL could not answer.
    #[test]
    fn probe_open_linked_pr_rest_fallback_recovers_none_open_when_graphql_fails() {
        let dir = tempdir().unwrap();
        let (reg, _log) = open_pr_guard_rest_fallback_registry(dir.path(), "", 0, true);
        assert_eq!(
            reg.probe_open_linked_pr(5565),
            OpenPrProbe::NoneOpen,
            "a verified-empty REST timeline answer is a genuine NoneOpen, not ProbeFailed"
        );
    }

    /// #5911: when BOTH GraphQL and its REST fallback fail to answer, the
    /// overall probe still reports `ProbeFailed` — the fallback recovers a
    /// GraphQL-only outage, it does not mask a genuine full forge outage.
    #[test]
    fn probe_open_linked_pr_rest_fallback_still_fails_when_both_transports_down() {
        let dir = tempdir().unwrap();
        let (reg, _log) = open_pr_guard_rest_fallback_registry(dir.path(), "", 1, true);
        assert_eq!(
            reg.probe_open_linked_pr(5565),
            OpenPrProbe::ProbeFailed,
            "a REST fallback that also fails must not manufacture a verified answer"
        );
    }

    /// #6058: a transient failure on BOTH transports on the first attempt —
    /// the production shape observed behind issue #5895's `loom:building`
    /// <-> `loom:issue` flap (intermittent `gh` TLS certificate-verification
    /// errors, not GraphQL quota exhaustion) — must not immediately fall the
    /// #4123 guard open. A second attempt, after a short delay, recovers the
    /// verified answer instead.
    #[test]
    fn probe_open_linked_pr_retries_and_recovers_from_transient_failure() {
        let dir = tempdir().unwrap();
        let (reg, _log) = open_pr_guard_transient_failure_registry(dir.path(), 9100, 1);
        assert_eq!(
            reg.probe_open_linked_pr(5895),
            OpenPrProbe::Open(9100),
            "a transient failure on the first attempt (both transports) must be retried once \
             before falling open (#6058)"
        );
    }

    /// #6058: the retry is bounded — a SUSTAINED failure across every attempt
    /// still falls open (`ProbeFailed`, the documented #4123 guard contract,
    /// unchanged). The retry narrows the fail-open window, it does not
    /// remove it: a genuine forge outage must never wedge dispatch. Also
    /// asserts the guard actually retried [`OPEN_PR_PROBE_MAX_ATTEMPTS`]
    /// times rather than looping forever or giving up after one attempt.
    #[test]
    fn probe_open_linked_pr_retry_still_fails_open_on_sustained_outage() {
        let dir = tempdir().unwrap();
        let (reg, log) = open_pr_guard_rest_fallback_registry(dir.path(), "", 1, true);
        assert_eq!(
            reg.probe_open_linked_pr(5565),
            OpenPrProbe::ProbeFailed,
            "a sustained outage across every retry attempt must still fall open, never wedge \
             dispatch (#6058)"
        );
        let invocations = std::fs::read_to_string(&log).unwrap_or_default();
        let graphql_calls = invocations.matches("api graphql").count();
        assert_eq!(
            graphql_calls, OPEN_PR_PROBE_MAX_ATTEMPTS as usize,
            "the guard must retry the full GraphQL-then-REST sequence \
             OPEN_PR_PROBE_MAX_ATTEMPTS times, not loop forever or give up after one (#6058)"
        );
    }

    // --- #6788: verified-open-PR memo -------------------------------------

    /// #6788 (part 1, the quota half): a verified `Open` answer is memoized, so
    /// a second probe inside [`OPEN_PR_MEMO_FRESH`] is served with **zero**
    /// `gh` invocations. This is what stops the guard re-deriving the same
    /// unchanged answer once a work-finder tick for days on end (measured:
    /// 4456 / 5380 / 5489 re-probes of three `loom:operator`-held issues over
    /// five days) — the spend that exhausts the GraphQL quota whose exhaustion
    /// then drops the guard through its fail-open arm.
    #[test]
    #[serial]
    fn probe_open_linked_pr_memo_serves_repeat_calls_without_a_forge_round_trip() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        let (reg, log) = open_pr_guard_registry(dir.path(), "6484", 0, true);

        assert_eq!(reg.probe_open_linked_pr(6472), OpenPrProbe::Open(6484));
        let after_first = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();
        assert!(after_first > 0, "the first probe must actually hit `gh`");

        assert_eq!(
            reg.probe_open_linked_pr(6472),
            OpenPrProbe::Open(6484),
            "a fresh memo must still answer Open with the same PR number"
        );
        let after_second = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(
            after_second, after_first,
            "a memo hit must cost NO additional `gh` invocation (#6788); got {after_second} \
             total invocations vs {after_first} after the first probe"
        );
    }

    /// #6788: the memo is a *freshness-bounded* cache, not a permanent verdict
    /// — an entry older than [`OPEN_PR_MEMO_FRESH`] is ignored and the forge is
    /// re-consulted, so an issue whose PR closed is never held out of dispatch
    /// for longer than that window.
    #[test]
    #[serial]
    fn probe_open_linked_pr_memo_expires_after_the_freshness_window() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        // The fake forge now reports NO open linked PR.
        let (reg, log) = open_pr_guard_registry(dir.path(), "", 0, true);
        // A memo older than the freshness window claims PR #6484 is open.
        let stale = Utc::now()
            - chrono::Duration::from_std(OPEN_PR_MEMO_FRESH).unwrap()
            - chrono::Duration::seconds(1);
        reg.seed_open_pr_memo(6472, 6484, stale);

        assert_eq!(
            reg.probe_open_linked_pr(6472),
            OpenPrProbe::NoneOpen,
            "a memo past OPEN_PR_MEMO_FRESH must be ignored and the forge re-consulted (#6788)"
        );
        assert!(
            !std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
            "an expired memo must fall through to a real probe"
        );
        assert!(
            reg.open_pr_memo_entry(6472).is_none(),
            "a VERIFIED NoneOpen must invalidate the memo immediately, not wait out the window"
        );
    }

    /// #6788 (part 2, the fail-open half — the headline fix): when BOTH
    /// transports fail on every attempt, a previously verified linked PR is
    /// re-confirmed over one targeted REST `pulls/<n>` call and the #4123 guard
    /// **holds** instead of falling open. This is the exact window behind all
    /// four observed occurrences (#5936/#5914, #6261/#6296, #6389/#6422,
    /// #6472/#6484): 91% of the fall-through dispatches in this repo's daemon
    /// log landed within 120s of a forge rate-limit event, against a ~12%
    /// baseline for guard-held dispatches.
    #[test]
    #[serial]
    fn probe_open_linked_pr_recheck_holds_guard_when_both_transports_fail() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        let (reg, log) = open_pr_guard_pulls_recheck_registry(dir.path(), "open", 0, true);
        reg.seed_open_pr_memo(6472, 6484, Utc::now() - chrono::Duration::hours(3));

        assert_eq!(
            reg.probe_open_linked_pr(6472),
            OpenPrProbe::Open(6484),
            "with both transports down but a known linked PR that re-confirms as open, the \
             guard must hold rather than fall open (#6788)"
        );
        let invocations = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            invocations.contains("pulls/6484"),
            "the backstop must re-verify the memoized PR directly; invocations: {invocations}"
        );
    }

    /// #6788: the backstop is a *re-verification*, never a replay of stale
    /// state. A memoized PR that is no longer open yields the unchanged
    /// `ProbeFailed` fall-open verdict, and the dead memo is dropped so the
    /// next tick does not re-spend the recheck on it.
    #[test]
    #[serial]
    fn probe_open_linked_pr_recheck_falls_open_when_memoized_pr_is_closed() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        let (reg, _log) = open_pr_guard_pulls_recheck_registry(dir.path(), "closed", 0, true);
        reg.seed_open_pr_memo(6472, 6484, Utc::now() - chrono::Duration::hours(3));

        assert_eq!(
            reg.probe_open_linked_pr(6472),
            OpenPrProbe::ProbeFailed,
            "a memoized PR that is no longer open must NOT hold the guard (#6788)"
        );
        assert!(
            reg.open_pr_memo_entry(6472).is_none(),
            "a confirmed-not-open recheck must invalidate the memo"
        );
    }

    /// #6788: the documented #4123 fail-open contract is preserved end to end.
    /// When the targeted recheck ITSELF cannot answer — a genuine, total forge
    /// outage rather than the GraphQL-exhaustion window this fix targets — the
    /// probe still concedes `ProbeFailed` and dispatch still proceeds. Wedging
    /// dispatch on a real outage would be a worse failure than the one being
    /// fixed.
    #[test]
    #[serial]
    fn probe_open_linked_pr_recheck_still_falls_open_on_a_total_forge_outage() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        let (reg, _log) = open_pr_guard_pulls_recheck_registry(dir.path(), "", 1, true);
        reg.seed_open_pr_memo(6472, 6484, Utc::now() - chrono::Duration::hours(3));

        assert_eq!(
            reg.probe_open_linked_pr(6472),
            OpenPrProbe::ProbeFailed,
            "if even the targeted recheck cannot answer, the guard must still fall open (#6788)"
        );
    }

    /// #6788: with no memo at all — the very first probe of an issue, or any
    /// probe after a daemon restart — a double transport failure behaves
    /// byte-for-byte as it did pre-#6788: `ProbeFailed`, and not one wasted
    /// `pulls/` call (there is no PR number to recheck).
    #[test]
    #[serial]
    fn probe_open_linked_pr_without_a_memo_is_unchanged_pre_6788_behavior() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        let (reg, log) = open_pr_guard_pulls_recheck_registry(dir.path(), "open", 0, true);

        assert_eq!(reg.probe_open_linked_pr(6472), OpenPrProbe::ProbeFailed);
        let invocations = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !invocations.contains("pulls/"),
            "with no memo there is nothing to recheck — the backstop must not invent a PR \
             number or spend a call; invocations: {invocations}"
        );
    }

    /// #6788: [`OPEN_PR_MEMO_ENABLE_ENV`] takes the whole mechanism out of the
    /// loop — no short circuit (every call re-probes) and no backstop (a double
    /// transport failure falls open even with a memo present).
    #[test]
    #[serial]
    fn open_pr_memo_env_kill_switch_restores_pre_6788_behavior() {
        std::env::set_var(OPEN_PR_MEMO_ENABLE_ENV, "0");

        // No short circuit: a second probe still pays a forge round trip.
        let short_circuit_dir = tempdir().unwrap();
        let (reg, log) = open_pr_guard_registry(short_circuit_dir.path(), "6484", 0, true);
        let first = reg.probe_open_linked_pr(6472);
        let after_first = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();
        let second = reg.probe_open_linked_pr(6472);
        let after_second = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();

        // No backstop: a seeded memo cannot hold the guard.
        let backstop_dir = tempdir().unwrap();
        let (reg, _log) =
            open_pr_guard_pulls_recheck_registry(backstop_dir.path(), "open", 0, true);
        reg.seed_open_pr_memo(6472, 6484, Utc::now());
        let backstop = reg.probe_open_linked_pr(6472);

        // Restore the ambient environment BEFORE asserting, so a failing
        // assertion cannot leak the kill switch into sibling tests.
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);

        assert_eq!(first, OpenPrProbe::Open(6484));
        assert_eq!(second, OpenPrProbe::Open(6484));
        assert!(
            after_second > after_first,
            "disabled ⇒ every call re-probes the forge (#6788 kill switch)"
        );
        assert_eq!(
            backstop,
            OpenPrProbe::ProbeFailed,
            "disabled ⇒ a double transport failure falls open exactly as it did pre-#6788"
        );
    }

    /// #6788 dispatch-level integration: the whole point of the backstop is
    /// that `dispatch()` refuses. During a GraphQL-exhaustion window that kills
    /// both transports, an issue whose linked PR is still open must be refused
    /// with the typed [`OpenPrDispatchError`] (which the work-finder attributes
    /// to its `pr-open-skip` counter) rather than dispatched — the recurrence
    /// this issue exists to stop.
    #[test]
    #[serial]
    fn dispatch_refuses_open_pr_via_memo_recheck_when_both_transports_fail() {
        std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
        let dir = tempdir().unwrap();
        let (mut reg, _log) = open_pr_guard_pulls_recheck_registry(dir.path(), "open", 0, false);
        reg.seed_open_pr_memo(6472, 6484, Utc::now() - chrono::Duration::hours(3));

        let err = reg
            .dispatch(&SweepKind::Issue(6472), None, None, None, None)
            .expect_err("dispatch must be refused while the linked PR is open");
        let refusal = err
            .downcast_ref::<OpenPrDispatchError>()
            .unwrap_or_else(|| panic!("expected an OpenPrDispatchError, got: {err:#}"));
        assert_eq!(refusal.issue, 6472);
        assert_eq!(refusal.pr, 6484, "the refusal must name the PR the recheck re-confirmed");
    }

    /// #5911: a VERIFIED GraphQL answer is trusted as-is and never pays the
    /// extra REST round trip — the fallback is consulted ONLY on
    /// `ProbeFailed`, not on every call.
    #[test]
    fn probe_open_linked_pr_skips_rest_fallback_when_graphql_succeeds() {
        let dir = tempdir().unwrap();
        let reg = no_progress_test_registry(dir.path(), "OPEN", "9100", false);
        assert_eq!(reg.probe_open_linked_pr(9002), OpenPrProbe::Open(9100));
        // `no_progress_test_registry`'s fake `gh` has no `timeline` arm at
        // all — if the REST fallback were consulted here it would fall
        // through to the fixture's `exit 0`/empty-stdout catch-all and
        // (incorrectly) still read as `NoneOpen`/`ProbeFailed` depending on
        // parse, so this assertion alone would not distinguish "skipped" from
        // "fell through". The `Open(9100)` verdict — the GraphQL-only
        // fixture's own PR number — is the real proof: a REST detour through
        // the catch-all could never reproduce that specific number.
    }

    /// #5911 dispatch-level integration: a GraphQL outage must not let
    /// `dispatch()` fall open and flip `loom:issue -> loom:building` when the
    /// REST fallback verifies an open PR — the exact #5565/#5569 scenario.
    #[test]
    #[serial]
    fn dispatch_refuses_open_pr_via_rest_fallback_when_graphql_fails() {
        let dir = tempdir().unwrap();
        let (mut reg, gh_log) = open_pr_guard_rest_fallback_registry(dir.path(), "5569", 0, false);
        let err = reg
            .dispatch(&SweepKind::Issue(5565), None, None, None, None)
            .expect_err("GraphQL outage recovered via REST must still refuse dispatch (#5911)");
        assert!(
            err.downcast_ref::<OpenPrDispatchError>().is_some(),
            "refusal must carry the typed OpenPrDispatchError, not some other failure; got: {err}"
        );
        let log = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !log.contains("issue") || !log.contains("edit"),
            "a refused dispatch must never flip labels; gh invocations: {log}"
        );
    }

    /// A pre-flip read showing `loom:building` already present is a collision.
    #[test]
    fn classify_preflip_labels_flags_prior_building_claim() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let registry = collision_registry(
            dir.path(),
            &gh_log,
            r#"{"labels":[{"name":"loom:building"},{"name":"loom:curated"}]}"#,
            0,
        );
        match registry.classify_preflip_labels(42) {
            CollisionClass::Collision { labels } => {
                assert!(labels.iter().any(|l| l == "loom:building"));
            }
            other => panic!("expected Collision, got {other:?}"),
        }
    }

    /// A pre-flip read with `loom:issue` already gone is a collision even if
    /// `loom:building` is not (yet) visible.
    #[test]
    fn classify_preflip_labels_flags_missing_issue_label() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let registry = collision_registry(
            dir.path(),
            &gh_log,
            r#"{"labels":[{"name":"tier:goal-supporting"}]}"#,
            0,
        );
        assert!(matches!(registry.classify_preflip_labels(42), CollisionClass::Collision { .. }));
    }

    /// `loom:issue` still present and `loom:building` absent ⇒ this host is the
    /// first claimant: Clean, not a collision.
    #[test]
    fn classify_preflip_labels_clean_when_issue_label_present() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let registry = collision_registry(
            dir.path(),
            &gh_log,
            r#"{"labels":[{"name":"loom:issue"},{"name":"loom:curated"}]}"#,
            0,
        );
        assert_eq!(registry.classify_preflip_labels(42), CollisionClass::Clean);
    }

    /// Fail-closed: a non-zero `gh` exit is `Unknown`, never a collision — an
    /// unverifiable read must not inflate the baseline.
    #[test]
    fn classify_preflip_labels_fail_closed_on_gh_error() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let registry = collision_registry(dir.path(), &gh_log, "", 1);
        assert_eq!(registry.classify_preflip_labels(42), CollisionClass::Unknown);
    }

    /// Fail-closed: unparseable stdout (exit 0 but not the expected JSON) is
    /// `Unknown`, never a collision.
    #[test]
    fn classify_preflip_labels_fail_closed_on_unparseable() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let registry = collision_registry(dir.path(), &gh_log, "not json at all", 0);
        assert_eq!(registry.classify_preflip_labels(42), CollisionClass::Unknown);
    }

    /// With detection enabled, a collision increments the counter once per call;
    /// an Unknown/Clean read does not.
    #[test]
    fn detect_and_record_collision_increments_counter() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let mut registry =
            collision_registry(dir.path(), &gh_log, r#"{"labels":[{"name":"loom:building"}]}"#, 0);
        registry.set_collision_detection(true);
        assert_eq!(registry.collision_count(), 0);
        registry.detect_and_record_collision(42);
        assert_eq!(registry.collision_count(), 1);
        registry.detect_and_record_collision(43);
        assert_eq!(registry.collision_count(), 2);
    }

    /// Issue #6243: a confirmed collision also feeds the SAME-issue windowed
    /// counter on the attached `PeerClaimView`, distinct from
    /// `registry.collision_count()`'s own monotonic total — both must
    /// advance together from one detection event.
    #[test]
    fn detect_and_record_collision_feeds_the_peer_claims_same_issue_counter() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let mut registry =
            collision_registry(dir.path(), &gh_log, r#"{"labels":[{"name":"loom:building"}]}"#, 0);
        registry.set_collision_detection(true);
        let view = std::sync::Arc::new(std::sync::Mutex::new(peer_claims::PeerClaimView::new(
            "self-host".to_string(),
            peer_claims::DEFAULT_PEER_CLAIM_TTL,
        )));
        registry.set_peer_claims(view.clone());

        registry.detect_and_record_collision(42);
        registry.detect_and_record_collision(43);

        assert_eq!(registry.collision_count(), 2, "the existing monotonic total still advances");
        let now = std::time::Instant::now();
        let locked = view.lock().unwrap();
        assert_eq!(
            locked.same_issue_collision_count(now),
            2,
            "the new windowed same-issue counter must advance identically"
        );
    }

    /// With NO `PeerClaimView` attached (safehouse disabled — the common
    /// case), `detect_and_record_collision` must still work exactly as
    /// before: the monotonic counter advances and nothing panics.
    #[test]
    fn detect_and_record_collision_without_peer_claims_view_still_counts() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let mut registry =
            collision_registry(dir.path(), &gh_log, r#"{"labels":[{"name":"loom:building"}]}"#, 0);
        registry.set_collision_detection(true);
        registry.detect_and_record_collision(42);
        assert_eq!(registry.collision_count(), 1);
    }

    /// With detection DISABLED, `detect_and_record_collision` is a pure no-op:
    /// no `gh` call at all (the disabled dispatch path is byte-for-byte
    /// unchanged) and the counter stays zero.
    #[test]
    fn detect_and_record_collision_disabled_is_noop() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let mut registry =
            collision_registry(dir.path(), &gh_log, r#"{"labels":[{"name":"loom:building"}]}"#, 0);
        // detection left at its default (false)
        assert!(!registry.collision_detection_enabled());
        registry.detect_and_record_collision(42);
        assert_eq!(registry.collision_count(), 0);
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.is_empty(),
            "disabled detection must not invoke gh at all; got: {gh_calls:?}"
        );
    }

    /// A fail-closed Unknown read never increments the counter even with
    /// detection enabled.
    #[test]
    fn detect_and_record_collision_unknown_not_counted() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let mut registry = collision_registry(dir.path(), &gh_log, "", 1);
        registry.set_collision_detection(true);
        registry.detect_and_record_collision(42);
        assert_eq!(registry.collision_count(), 0, "Unknown must not count");
    }

    /// Config resolution honors precedence env > config > default(off) (#4085).
    #[test]
    #[serial]
    fn resolve_collision_detection_env_overrides() {
        std::env::remove_var(COLLISION_DETECT_ENV);
        let dir = tempdir().unwrap();
        // No file, no env → default off.
        assert!(!resolve_collision_detection(dir.path()));

        // Config file enables it.
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            r#"{"autonomous":{"collisionDetection":{"enabled":true}}}"#,
        )
        .unwrap();
        assert!(resolve_collision_detection(dir.path()), "config enables");

        // Env overrides config (off wins over config-on).
        std::env::set_var(COLLISION_DETECT_ENV, "off");
        assert!(!resolve_collision_detection(dir.path()), "env off overrides config on");
        std::env::set_var(COLLISION_DETECT_ENV, "1");
        assert!(resolve_collision_detection(dir.path()), "env on");
        std::env::remove_var(COLLISION_DETECT_ENV);
    }

    /// Issue #4206 (Option 2): the crash-path label restore must NEVER add
    /// `loom:issue` while the issue currently carries `loom:blocked` on the
    /// forge — that would produce the illegal `loom:blocked` + `loom:issue`
    /// combo, silently overriding an operator's deliberate park. The stale
    /// `loom:building` claim from the now-dead sweep is still cleared.
    #[test]
    fn restore_label_to_ready_does_not_add_loom_issue_when_blocked() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        // `gh issue view <n> --json labels --jq '...'` (the pre-check probe)
        // reports the issue as currently `loom:blocked`; any `gh issue edit`
        // call is recorded and always succeeds.
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  echo "true"
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real restore path
        let registry = SweepRegistry::new(config);

        registry.restore_label_to_ready(4206).unwrap();

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 4206 --remove-label loom:building"),
            "expected the stale loom:building claim to still be removed; got: {gh_calls:?}"
        );
        assert!(
            !gh_calls.contains("--add-label loom:issue"),
            "must NOT re-add loom:issue while loom:blocked is present (illegal combo); got: \
             {gh_calls:?}"
        );
    }

    /// Issue #4887 regression: simulates the claim-abort race from #4607/#4608
    /// — a Builder claims `loom:issue` -> `loom:building`, then correctly
    /// aborts mid-build and reroutes `loom:building` -> `loom:operator-only`
    /// itself (missing OAuth scope / an operator RETIRE decision), then its
    /// process exits. When the reaper later notices the dead child and calls
    /// `restore_label_to_ready` as crash-path cleanup, it must NOT re-add
    /// `loom:issue` on top of that reroute — that would leave the issue in
    /// the illegal `loom:operator-only` + `loom:issue` combo and re-queue it
    /// for the exact same blocker (an infinite reclaim loop). The stale
    /// `loom:building` claim (already gone in the live incident, but the
    /// restore always issues the removal defensively) is still requested.
    #[test]
    fn restore_label_to_ready_does_not_add_loom_issue_when_operator_only() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        // The `loom:blocked` pre-check (first `gh issue view --json labels`
        // call) reports false; the `loom:operator-only` follow-up check
        // (second `gh issue view --json labels` call) reports true. Any `gh
        // issue edit` call is recorded and always succeeds.
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  case "$*" in
    *loom:blocked*) echo "false" ;;
    *loom:operator-only*) echo "true" ;;
    *) echo "false" ;;
  esac
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real restore path
        let registry = SweepRegistry::new(config);

        registry.restore_label_to_ready(4607).unwrap();

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 4607 --remove-label loom:building"),
            "expected the stale loom:building claim to still be removed; got: {gh_calls:?}"
        );
        assert!(
            !gh_calls.contains("--add-label loom:issue"),
            "must NOT re-add loom:issue while loom:operator-only is present (illegal combo, \
             #4887 — reproduces the #4607/#4608 claim-abort race); got: {gh_calls:?}"
        );
    }

    /// Issue #4653: the crash-path label restore must NEVER add `loom:issue`
    /// when the target number resolves to a pull request. This covers the
    /// #4123-fail-open window where the 2.5 dispatch guard (`gh` outage) lets
    /// a PR number through to `loom:building` and the sweep is later
    /// cancelled or crash-recovered — the stale `loom:building` claim is
    /// still cleared, but `loom:issue` must not be reapplied to a PR.
    #[test]
    #[serial]
    fn restore_label_to_ready_does_not_add_loom_issue_when_target_is_pr() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        // `gh issue view <n> --json labels --jq '...'` (the #4206 blocked
        // pre-check) reports not-blocked; `gh api repos/.../issues/<n>
        // --jq '.pull_request != null'` (the #4653 is-pr probe) reports
        // `true`. Any `gh issue edit` call is recorded and always succeeds.
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  echo "false"
  exit 0
fi
if [ "$1" = "api" ]; then
  echo "true"
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real restore path
        let registry = SweepRegistry::new(config);

        // Bypass the `resolve_owner_repo` -> `gh repo view` hop (irrelevant to
        // this test) exactly like the dispatch-guard PR tests do.
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let result = registry.restore_label_to_ready(6501);
        std::env::remove_var("LOOM_REPO");
        result.unwrap();

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 6501 --remove-label loom:building"),
            "expected the stale loom:building claim to still be removed; got: {gh_calls:?}"
        );
        assert!(
            !gh_calls.contains("--add-label loom:issue"),
            "must NOT add loom:issue when the target number resolves to a pull request; got: \
             {gh_calls:?}"
        );
    }

    /// Issue #3937: in a multi-workspace daemon (process cwd = the *default*
    /// repo), the registry's forge label flips must run in the registry's own
    /// `workspace_root`. Otherwise a non-default workspace's issue number
    /// resolves against the daemon's cwd repo and the claim silently fails
    /// (`GraphQL: Could not resolve to an issue ...`). Point a fake `gh` at a
    /// recorder that captures its own cwd (`pwd -P`) and assert both the flip
    /// and the restore executed in the registry root — not the process cwd.
    ///
    /// The fake `gh` recorder lives outside `registry_root`, and the process
    /// cwd is left untouched (this crate's tests run in parallel; mutating the
    /// global cwd would race). The `current_dir` fix makes the recorded cwd
    /// equal to `workspace_root` regardless of where the process itself sits,
    /// which is exactly the invariant under test.
    #[test]
    fn label_flips_run_in_registry_workspace_root() {
        // `recorder` stands in for the daemon process's cwd repo; the flips
        // must NOT resolve here. `registry_root` is the non-default workspace.
        let recorder = tempdir().unwrap();
        let registry_root = tempdir().unwrap();

        let gh_log = recorder.path().join("gh-invocations.log");
        let cwd_log = recorder.path().join("gh-cwd.log");
        let fake_gh = recorder.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\npwd -P >> \"{}\"\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            cwd_log.display(),
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(registry_root.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real gh path
        let registry = SweepRegistry::new(config);

        // Issue number that only exists in the non-default repo (mirrors the
        // live #6199 symptom): the flip must still target registry_root.
        registry.flip_label_to_building(6199).unwrap();
        registry.restore_label_to_ready(6199).unwrap();

        let want = std::fs::canonicalize(registry_root.path()).unwrap();
        let recorded = std::fs::read_to_string(&cwd_log).unwrap_or_default();
        let cwds: Vec<_> = recorded.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            cwds.len(),
            5,
            "expected the flip (1 call) + restore (Issue #4206's pre-check `loom:blocked` \
             probe, Issue #4887's follow-up `loom:operator-only` probe — the fake `gh` prints \
             nothing so both park probes read as absent, Issue #4653's `is_pr` probe's \
             `resolve_owner_repo` lookup — which bails before the second `gh api` call since \
             the fake `gh` prints nothing for `repo view` — then the edit — 4 calls) to invoke \
             gh five times total; got cwds: {cwds:?}"
        );
        for cwd in &cwds {
            let got = std::fs::canonicalize(cwd).unwrap();
            assert_eq!(
                got,
                want,
                "gh label flip must run in the registry workspace_root ({}), not the \
                 recorder/process cwd ({}); recorded cwd: {cwd}",
                want.display(),
                recorder.path().display(),
            );
        }

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls
                .contains("issue edit 6199 --remove-label loom:issue --add-label loom:building"),
            "expected the flip-to-building call; got: {gh_calls:?}"
        );
    }

    /// Issue #5431: `classify_preflip_labels` must thread a registered
    /// cross-owner workspace's installation-token `GH_CONFIG_DIR` through to
    /// the real `gh issue view` child, mirroring the coverage
    /// `watch_registry`'s `build_command_applies_gh_config_for_registered_workspace_root`
    /// added for the listing/reconciliation call sites in #5420.
    #[test]
    #[serial]
    fn classify_preflip_labels_applies_registered_gh_config_dir() {
        crate::credential_preflight::clear_owner_root_registry();
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
        crate::credential_preflight::register_root_gh_config_dir(dir.path(), &owner_dir);

        let fake_gh = install_fake_gh_env_logger(
            dir.path(),
            &gh_log,
            r#"{"labels":[{"name":"loom:issue"}]}"#,
            0,
        );
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        let registry = SweepRegistry::new(config);

        registry.classify_preflip_labels(9401);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains(&format!("GH_CONFIG_DIR={}", owner_dir.display())),
            "expected the registered owner's GH_CONFIG_DIR on the gh child; got: {gh_calls:?}"
        );

        crate::credential_preflight::clear_owner_root_registry();
    }

    /// The unregistered-root counterpart to the above: a single-owner
    /// workspace (the common case) must leave `GH_CONFIG_DIR` untouched on
    /// the child, i.e. byte-identical to pre-#5401 behavior.
    #[test]
    #[serial]
    fn classify_preflip_labels_is_a_noop_for_unregistered_workspace_root() {
        // #5651: scrub the test process's own ambient GH_CONFIG_DIR before
        // asserting the child inherits "<unset>" — otherwise this leaks
        // whatever the invoking host's environment happens to contain (e.g.
        // any real Loom fleet worker, which exports GH_CONFIG_DIR
        // process-wide for the daemon, #4458) and the assertion below fails
        // even though the no-op production behavior it exercises is
        // unaffected.
        let _env_guard = ClearedGhConfigDirEnv::new();
        crate::credential_preflight::clear_owner_root_registry();
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let fake_gh = install_fake_gh_env_logger(
            dir.path(),
            &gh_log,
            r#"{"labels":[{"name":"loom:issue"}]}"#,
            0,
        );
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        let registry = SweepRegistry::new(config);

        registry.classify_preflip_labels(9402);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("GH_CONFIG_DIR=<unset>"),
            "an unregistered root must not set GH_CONFIG_DIR on the child; got: {gh_calls:?}"
        );
    }

    /// Issue #6179 (Epic #6165 Phase 1): `write_lease_comment` posts a `gh
    /// issue comment` whose body's literal first line is the lease marker
    /// `<!-- loom:lease host=<host> sweep=<sweep-id> -->`, carrying the exact
    /// sweep id it was called with. Issue #6322: `<host>` is the PUBLISHED
    /// (opaque by default) id, not the raw `host_identity()` value.
    ///
    /// `#[serial]`: this test computes `registry.published_host_id()` a
    /// second time AFTER the write to compare against, and `host_identity()`
    /// reads process-global env (`LOOM_HOST_ID`/`HOSTNAME`) — without
    /// `#[serial]` this races against the OTHER `#[serial]`-marked tests in
    /// this crate that mutate those same env vars (e.g.
    /// `host_identity_env_precedence` in `mod.rs`), which can make the two
    /// calls resolve to different values and spuriously fail this
    /// assertion.
    #[test]
    #[serial]
    fn write_lease_comment_posts_the_marker_with_the_given_sweep_id() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 0);
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let registry = SweepRegistry::new(config);

        registry.write_lease_comment(6179, "sweep-test-6179");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue comment 6179"),
            "expected a gh issue comment call for #6179; got: {gh_calls:?}"
        );
        let published = registry.published_host_id();
        assert!(
            gh_calls.contains(&format!(
                "{prefix}{host} sweep=sweep-test-6179 -->",
                prefix = LEASE_MARKER_PREFIX,
                host = published,
            )),
            "expected the literal lease marker with the given sweep id; got: {gh_calls:?}"
        );
        // Issue #6322: the marker's `host=` must be the opaque published id,
        // never the raw hostname (unless this test process happened to opt
        // into raw publishing, which it did not).
        let raw_host = host_identity();
        if raw_host != published {
            assert!(
                !gh_calls.contains(&format!(
                    "{prefix}{raw_host} sweep=sweep-test-6179 -->",
                    prefix = LEASE_MARKER_PREFIX,
                )),
                "the raw hostname must never appear in the published lease marker by default \
                 (Issue #6322); got: {gh_calls:?}"
            );
        }
    }

    /// The lease write must be skipped entirely (no `gh` invocation at all)
    /// when label flips are disabled — mirroring every other best-effort
    /// forge mutation on the dispatch path, and matching the invariant that
    /// a lease is only ever written alongside a real claim.
    #[test]
    fn write_lease_comment_is_a_noop_when_label_flips_are_skipped() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 0);
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = true;
        let registry = SweepRegistry::new(config);

        registry.write_lease_comment(6179, "sweep-test-6179");

        assert!(
            !gh_log.exists()
                || std::fs::read_to_string(&gh_log)
                    .unwrap_or_default()
                    .is_empty(),
            "skip_label_flip must suppress the lease write entirely — no gh child spawned"
        );
    }

    /// A failed `gh issue comment` (e.g. `gh` missing/exiting non-zero) must
    /// never panic or propagate — this method has no `Result` return
    /// precisely because posting a lease record is best-effort and must
    /// never affect the dispatch it documents.
    #[test]
    fn write_lease_comment_is_fail_open_on_gh_failure() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let fake_gh = install_fake_gh(dir.path(), &gh_log, "boom", 1);
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let registry = SweepRegistry::new(config);

        // Must not panic.
        registry.write_lease_comment(6179, "sweep-test-6179");
    }

    // ------------------------------------------------------------------------
    // Opaque host id publishing (Issue #6322) — the raw hostname must not
    // land in a public forge comment by default, but the pre-#6322 raw
    // behavior must remain available via LEASE_PUBLISH_HOSTNAME_ENV.
    // ------------------------------------------------------------------------

    /// Guards + restores `LOOM_LEASE_PUBLISH_HOSTNAME` for the duration of a
    /// single test, mirroring the `HostIdentityEnvGuard` pattern `mod.rs`'s
    /// own test module uses for `LOOM_HOST_ID`/`HOSTNAME`.
    struct LeasePublishHostnameEnvGuard {
        previous: Option<String>,
    }

    impl LeasePublishHostnameEnvGuard {
        fn set(value: &str) -> Self {
            let previous = std::env::var(LEASE_PUBLISH_HOSTNAME_ENV).ok();
            std::env::set_var(LEASE_PUBLISH_HOSTNAME_ENV, value);
            Self { previous }
        }
    }

    impl Drop for LeasePublishHostnameEnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(LEASE_PUBLISH_HOSTNAME_ENV, v),
                None => std::env::remove_var(LEASE_PUBLISH_HOSTNAME_ENV),
            }
        }
    }

    /// Default (env unset): `published_host_id` returns the opaque form, not
    /// the raw `host_identity()` value.
    #[test]
    #[serial]
    fn published_host_id_defaults_to_opaque() {
        std::env::remove_var(LEASE_PUBLISH_HOSTNAME_ENV);
        let dir = tempdir().unwrap();
        let registry = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()));
        let raw = host_identity();
        assert_eq!(
            registry.published_host_id(),
            opaque_host_id(&raw),
            "with no opt-in, published_host_id must publish the opaque transform of the raw host"
        );
    }

    /// `LOOM_LEASE_PUBLISH_HOSTNAME=1` restores the pre-#6322 raw-hostname
    /// publishing behavior — the escape hatch Issue #6322's acceptance
    /// criteria require.
    #[test]
    #[serial]
    fn published_host_id_raw_opt_in_via_env() {
        let _guard = LeasePublishHostnameEnvGuard::set("1");
        let dir = tempdir().unwrap();
        let registry = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()));
        assert_eq!(
            registry.published_host_id(),
            host_identity(),
            "LOOM_LEASE_PUBLISH_HOSTNAME=1 must restore raw hostname publishing"
        );
    }

    /// A falsy/unrecognized value must NOT be treated as opt-in — only the
    /// recognized truthy tokens flip the default.
    #[test]
    #[serial]
    fn published_host_id_ignores_falsy_env_values() {
        let _guard = LeasePublishHostnameEnvGuard::set("0");
        let dir = tempdir().unwrap();
        let registry = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()));
        assert_eq!(
            registry.published_host_id(),
            opaque_host_id(&host_identity()),
            "an unrecognized/falsy value must not opt into raw publishing"
        );
    }

    /// End-to-end: with the env opt-in set, `write_lease_comment`'s actual
    /// posted marker carries the raw hostname again (regression coverage for
    /// the escape hatch at the real call site, not just the pure helper).
    #[test]
    #[serial]
    fn write_lease_comment_publishes_raw_hostname_when_opted_in() {
        let _guard = LeasePublishHostnameEnvGuard::set("true");
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 0);
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let registry = SweepRegistry::new(config);

        registry.write_lease_comment(6322, "sweep-test-6322");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains(&format!(
                "{prefix}{host} sweep=sweep-test-6322 -->",
                prefix = LEASE_MARKER_PREFIX,
                host = host_identity(),
            )),
            "the opt-in must restore the raw hostname in the actual posted marker; got: \
             {gh_calls:?}"
        );
    }

    // ------------------------------------------------------------------------
    // Claim-then-verify-order dedup at dispatch time (Issue #6287, Epic
    // #6165 Phase 2) — pure-function and single-method unit coverage. The
    // full end-to-end "two near-simultaneous dispatches" scenario is covered
    // at the `dispatch()` level in `dispatch.rs`'s own test module.
    // ------------------------------------------------------------------------

    /// [`SweepRegistry::parse_lease_marker_line`] extracts `host`/`sweep_id`
    /// from a real lease marker's literal first line, ignoring everything
    /// after the closing `-->` (the format contract's free-form prose).
    #[test]
    fn parse_lease_marker_line_extracts_host_and_sweep_id() {
        let body = "<!-- loom:lease host=studio-host sweep=sweep-2026-08-13T23-01-04Z-a1b2c3 -->\n\
                     This issue's `loom:building` claim was acquired...";
        assert_eq!(
            SweepRegistry::parse_lease_marker_line(body),
            Some(("studio-host".to_string(), "sweep-2026-08-13T23-01-04Z-a1b2c3".to_string()))
        );
    }

    /// A comment whose first line does not carry the exact lease marker
    /// prefix (including the visually-similar `loom:lease-yield` standdown
    /// annotation marker, Issue #6287) must never parse as a lease record.
    #[test]
    fn parse_lease_marker_line_rejects_non_matching_bodies() {
        assert_eq!(SweepRegistry::parse_lease_marker_line("just a regular comment"), None);
        assert_eq!(
            SweepRegistry::parse_lease_marker_line(
                "<!-- loom:lease-yield host=h sweep=s earliest_host=h2 earliest_sweep=s2 -->\nprose"
            ),
            None,
            "the standdown annotation marker must never be mistaken for a lease record"
        );
        assert_eq!(
            SweepRegistry::parse_lease_marker_line("<!-- loom:lease host= sweep=s -->"),
            None,
            "an empty host must not parse"
        );
    }

    /// [`SweepRegistry::parse_lease_comments_json`] parses the NDJSON
    /// (`{id, created_at, body}` per line) shape the real `--jq` filter
    /// emits, silently dropping any entry with a missing/non-numeric `id` or
    /// an unparseable marker line rather than failing the whole batch.
    #[test]
    fn parse_lease_comments_json_parses_valid_entries_and_drops_malformed_ones() {
        let stdout = b"{\"id\":101,\"created_at\":\"2026-08-15T09:42:22Z\",\"body\":\"<!-- loom:lease host=loom-worker-1 sweep=sweep-a -->\\nprose\"}\n\
            {\"id\":102,\"created_at\":\"2026-08-15T09:42:25Z\",\"body\":\"<!-- loom:lease host=loom-worker-2 sweep=sweep-b -->\\nprose\"}\n\
            {\"created_at\":\"2026-08-15T09:42:30Z\",\"body\":\"<!-- loom:lease host=no-id sweep=sweep-c -->\"}\n\
            {\"id\":103,\"created_at\":\"2026-08-15T09:42:35Z\",\"body\":\"not a lease comment at all\"}\n";
        let parsed = SweepRegistry::parse_lease_comments_json(stdout);
        assert_eq!(parsed.len(), 2, "the missing-id and non-matching-body entries must be dropped");
        assert_eq!(parsed[0].id, 101);
        assert_eq!(parsed[0].host, "loom-worker-1");
        assert_eq!(parsed[0].sweep_id, "sweep-a");
        assert_eq!(parsed[1].id, 102);
        assert_eq!(parsed[1].host, "loom-worker-2");
        assert_eq!(parsed[1].sweep_id, "sweep-b");
    }

    /// Empty stdout is a successful read with zero lease comments; garbage
    /// input is silently dropped line-by-line rather than failing the whole
    /// read — there is no `None`/failure case left in this function (see its
    /// doc comment), since it only ever runs after `read_lease_comments` has
    /// already confirmed a zero exit.
    #[test]
    fn parse_lease_comments_json_empty_or_garbage_input_is_a_verified_empty_read() {
        assert_eq!(SweepRegistry::parse_lease_comments_json(b""), vec![]);
        assert_eq!(SweepRegistry::parse_lease_comments_json(b"not json\n"), vec![]);
    }

    /// Issue #6293/#4637 regression: `gh api --paginate` re-invokes `--jq`
    /// once per response page and simply concatenates each page's raw
    /// output. For the old `[...]`-wrapped filter this produced invalid JSON
    /// (`[...][...]`) that failed to parse at all. NDJSON has no such
    /// wrapper — concatenating two pages' worth of one-object-per-line
    /// output is still valid, line-parseable NDJSON, so a multi-page result
    /// must parse exactly like a single-page one.
    #[test]
    fn parse_lease_comments_json_handles_multi_page_concatenation() {
        // Simulates `--paginate` concatenating page 1 (one matching lease
        // comment) directly onto page 2 (another), exactly as `gh` would.
        let page_1 = b"{\"id\":201,\"created_at\":\"2026-08-15T09:00:00Z\",\"body\":\"<!-- loom:lease host=host-a sweep=sweep-a -->\"}\n";
        let page_2 = b"{\"id\":202,\"created_at\":\"2026-08-15T09:05:00Z\",\"body\":\"<!-- loom:lease host=host-b sweep=sweep-b -->\"}\n";
        let stdout = [page_1.as_slice(), page_2.as_slice()].concat();
        let parsed = SweepRegistry::parse_lease_comments_json(&stdout);
        assert_eq!(parsed.len(), 2, "both pages' entries must survive concatenation");
        assert_eq!(parsed[0].id, 201);
        assert_eq!(parsed[0].host, "host-a");
        assert_eq!(parsed[1].id, 202);
        assert_eq!(parsed[1].host, "host-b");
    }

    /// Build a registry whose fake `gh` answers `repo view` (so
    /// `resolve_owner_repo` succeeds) and the `.../comments` read with a
    /// fixed stdout/exit code, for [`resolve_lease_order`] unit coverage.
    fn lease_order_unit_registry(
        dir: &Path,
        comments_stdout: &str,
        exit_code: i32,
    ) -> SweepRegistry {
        let fake_gh = dir.join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$*\" == *\"/comments\"* ]]; then\n\
             printf '%s' '{stdout}'\n\
             exit {code}\n\
             fi\n\
             exit 1\n",
            stdout = comments_stdout.replace('\'', "'\\''"),
            code = exit_code,
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }
        let mut config = SweepRegistryConfig::new(dir.to_path_buf());
        config.gh_bin = Some(fake_gh);
        SweepRegistry::new(config)
    }

    /// Issue #6287: an unreadable forge (non-zero `gh api` exit) must
    /// fail-open to [`LeaseOrderDecision::Proceed`] — an unverifiable read
    /// must never manufacture a refusal.
    #[test]
    #[serial]
    fn resolve_lease_order_proceeds_when_the_read_fails() {
        let dir = tempdir().unwrap();
        let registry = lease_order_unit_registry(dir.path(), "boom", 1);
        assert_eq!(
            registry.resolve_lease_order(9822, "sweep-mine", Utc::now()),
            LeaseOrderDecision::Proceed
        );
    }

    /// Issue #6287: when this dispatcher's own lease comment cannot be found
    /// among the read-back set (e.g. its write failed, or the read raced
    /// ahead of forge propagation), there is nothing to compare against —
    /// fail-open to [`LeaseOrderDecision::Proceed`] rather than inventing a
    /// refusal from an absence.
    #[test]
    #[serial]
    fn resolve_lease_order_proceeds_when_its_own_comment_is_not_found() {
        let dir = tempdir().unwrap();
        let stdout = format!(
            r#"{{"id":1,"created_at":"{now}","body":"<!-- loom:lease host=peer-host sweep=sweep-peer -->"}}"#,
            now = Utc::now().to_rfc3339(),
        );
        let registry = lease_order_unit_registry(dir.path(), &stdout, 0);
        assert_eq!(
            registry.resolve_lease_order(9823, "sweep-mine", Utc::now()),
            LeaseOrderDecision::Proceed
        );
    }

    /// Issue #6287: the core positive case — this dispatcher's own comment
    /// (id 2) is NOT the earliest (a peer's id 1 is) within the lookback
    /// window, so the tie-break yields, naming the earlier host/sweep. Uses
    /// `registry.published_host_id()` for the "own" record rather than
    /// overriding it — `resolve_lease_order` identifies "this dispatcher's
    /// own comment" by that exact value (Issue #6322: the opaque published
    /// id, not raw `host_identity()`), so the fixture must match whatever
    /// the function under test actually resolves.
    #[test]
    #[serial]
    fn resolve_lease_order_yields_when_a_peer_comment_is_earlier() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        let stdout = format!(
            "{{\"id\":1,\"created_at\":\"{t1}\",\"body\":\"<!-- loom:lease host=peer-host sweep=sweep-peer -->\"}}\n\
             {{\"id\":2,\"created_at\":\"{t2}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            t1 = now.to_rfc3339(),
            t2 = now.to_rfc3339(),
        );
        let registry = lease_order_unit_registry(dir.path(), &stdout, 0);
        assert_eq!(
            registry.resolve_lease_order(9824, "sweep-mine", now),
            LeaseOrderDecision::Yield {
                earliest_host: "peer-host".to_string(),
                earliest_sweep_id: "sweep-peer".to_string(),
            }
        );
    }

    /// Issue #6287: the complementary case — this dispatcher's own comment
    /// (id 1) IS the earliest, so the tie-break proceeds.
    #[test]
    #[serial]
    fn resolve_lease_order_proceeds_when_its_own_comment_is_earliest() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        let stdout = format!(
            "{{\"id\":1,\"created_at\":\"{t1}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}\n\
             {{\"id\":2,\"created_at\":\"{t2}\",\"body\":\"<!-- loom:lease host=peer-host sweep=sweep-peer -->\"}}",
            t1 = now.to_rfc3339(),
            t2 = now.to_rfc3339(),
        );
        let registry = lease_order_unit_registry(dir.path(), &stdout, 0);
        assert_eq!(
            registry.resolve_lease_order(9825, "sweep-mine", now),
            LeaseOrderDecision::Proceed
        );
    }

    /// Issue #6287: a stale lease comment from a long-completed, unrelated
    /// prior dispatch of the same issue number (an old `id` and a
    /// `created_at` far outside [`LEASE_ORDER_LOOKBACK_SECS`]) must be
    /// excluded from the comparison entirely — otherwise every normal,
    /// uncontested re-dispatch of a previously-built issue would spuriously
    /// yield to its own history.
    #[test]
    #[serial]
    fn resolve_lease_order_ignores_comments_outside_the_lookback_window() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        let stale = now - chrono::Duration::seconds(LEASE_ORDER_LOOKBACK_SECS + 3600);
        let stdout = format!(
            "{{\"id\":1,\"created_at\":\"{stale}\",\"body\":\"<!-- loom:lease host=old-claimant sweep=sweep-ancient -->\"}}\n\
             {{\"id\":2,\"created_at\":\"{fresh}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            stale = stale.to_rfc3339(),
            fresh = now.to_rfc3339(),
        );
        let registry = lease_order_unit_registry(dir.path(), &stdout, 0);
        assert_eq!(
            registry.resolve_lease_order(9826, "sweep-mine", now),
            LeaseOrderDecision::Proceed,
            "the stale, out-of-window lease record must not out-rank this dispatcher's own"
        );
    }

    // --- Issue #6816: own-comment read-back retry -------------------------

    /// Build a registry whose fake `gh` answers `repo view` (so
    /// `resolve_owner_repo` succeeds) and whose `.../comments` read
    /// simulates read-after-write propagation lag (Issue #6816): the first
    /// `missing_for_calls` invocations of the comments read omit this
    /// dispatcher's own lease comment (`stdout_before`), and every
    /// invocation after that includes it (`stdout_after`) — mirroring
    /// [`open_pr_guard_transient_failure_registry`] in `test_support.rs`,
    /// whose counter-file-across-subprocess-invocations technique this
    /// reuses for the same reason: each `gh api` call is a fresh short-lived
    /// process, so an in-memory counter cannot survive between invocations.
    fn lease_order_stateful_registry(
        dir: &Path,
        stdout_before: &str,
        stdout_after: &str,
        missing_for_calls: u32,
    ) -> (SweepRegistry, PathBuf) {
        let counter = dir.join("comments-call-count");
        let fake_gh = dir.join("fake-gh-stateful.sh");
        let script = format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$*\" == *\"/comments\"* ]]; then\n\
             n=$(( $(cat \"{counter}\" 2>/dev/null || echo 0) + 1 ))\n\
             printf '%s' \"$n\" > \"{counter}\"\n\
             if [[ \"$n\" -le {missing_for_calls} ]]; then\n\
             printf '%s' '{before}'\n\
             else\n\
             printf '%s' '{after}'\n\
             fi\n\
             exit 0\n\
             fi\n\
             exit 1\n",
            counter = counter.display(),
            missing_for_calls = missing_for_calls,
            before = stdout_before.replace('\'', "'\\''"),
            after = stdout_after.replace('\'', "'\\''"),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }
        let mut config = SweepRegistryConfig::new(dir.to_path_buf());
        config.gh_bin = Some(fake_gh);
        (SweepRegistry::new(config), counter)
    }

    /// Issue #6816: the core regression case — this dispatcher's own lease
    /// comment is missing on the FIRST read-back (simulating read-after-write
    /// propagation lag) but visible by the second, and a peer's comment
    /// (earlier `id`) is present throughout. Before the #6816 fix, a single
    /// immediate "own comment not found" read fell open to `Proceed`
    /// unconditionally — exactly the bug this issue reports, since BOTH
    /// racing dispatchers could hit that same ambiguous read and both
    /// proceed. After the fix, the retry finds the now-visible peer comment
    /// and correctly yields.
    #[test]
    #[serial]
    fn resolve_lease_order_retries_and_finds_a_peer_once_propagation_catches_up() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        let peer_only = format!(
            r#"{{"id":1,"created_at":"{t1}","body":"<!-- loom:lease host=peer-host sweep=sweep-peer -->"}}"#,
            t1 = now.to_rfc3339(),
        );
        let both = format!(
            "{{\"id\":1,\"created_at\":\"{t1}\",\"body\":\"<!-- loom:lease host=peer-host sweep=sweep-peer -->\"}}\n\
             {{\"id\":2,\"created_at\":\"{t2}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            t1 = now.to_rfc3339(),
            t2 = now.to_rfc3339(),
        );
        // Own comment missing on the first read only; visible from the
        // second read onward — well within LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS.
        let (registry, counter) = lease_order_stateful_registry(dir.path(), &peer_only, &both, 1);
        assert_eq!(
            registry.resolve_lease_order(9827, "sweep-mine", now),
            LeaseOrderDecision::Yield {
                earliest_host: "peer-host".to_string(),
                earliest_sweep_id: "sweep-peer".to_string(),
            },
            "once the retry finds the peer's earlier comment, this dispatcher must yield rather \
             than fail open on a stale first read"
        );
        let calls: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            calls >= 2,
            "expected at least 2 comments-read attempts (the #6816 retry), got {calls}"
        );
    }

    /// Issue #6816: the retry is bounded — if this dispatcher's own comment
    /// never becomes visible within
    /// [`LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS`] attempts (a genuinely failed
    /// write, not just propagation lag), the guard must still fall open to
    /// `Proceed` exactly as before rather than looping forever or wedging
    /// dispatch.
    #[test]
    #[serial]
    fn resolve_lease_order_retry_still_falls_open_when_own_comment_never_appears() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let peer_only = format!(
            r#"{{"id":1,"created_at":"{t1}","body":"<!-- loom:lease host=peer-host sweep=sweep-peer -->"}}"#,
            t1 = now.to_rfc3339(),
        );
        // Never includes this dispatcher's own comment, no matter how many
        // times it is read.
        let (registry, counter) =
            lease_order_stateful_registry(dir.path(), &peer_only, &peer_only, 0);
        assert_eq!(
            registry.resolve_lease_order(9828, "sweep-mine", now),
            LeaseOrderDecision::Proceed,
            "a sustained absence (genuinely failed write) must still fall open, never wedge \
             dispatch (#6816)"
        );
        let calls: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            calls, LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS,
            "expected exactly LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS attempts, not fewer (giving \
             up early) or more (retrying forever)"
        );
    }

    // --- Issue #6951: sole-claim confirmation before Proceed ---------------

    /// Issue #6951: the core regression case — this dispatcher's own
    /// comment IS visible (and looks earliest/sole) on the very first read,
    /// but a peer's genuinely earlier comment has not propagated into that
    /// read yet, becoming visible only once the confirmation phase re-reads.
    /// Before the #6951 fix, `resolve_lease_order` returned `Proceed` the
    /// instant it saw its own comment as earliest, with no further check —
    /// exactly the bug this issue reports (two cross-host dispatchers 3
    /// seconds apart, each seeing only its own comment on a single read).
    /// After the fix, the confirmation re-read finds the now-visible peer
    /// comment and correctly yields.
    #[test]
    #[serial]
    fn resolve_lease_order_confirmation_finds_a_slow_peer_and_yields() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        // First read: only this dispatcher's own comment (id 2) is visible —
        // it looks sole/earliest. From the second read onward, a peer's
        // earlier comment (id 1) has propagated into view.
        let own_only = format!(
            "{{\"id\":2,\"created_at\":\"{t2}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            t2 = now.to_rfc3339(),
        );
        let both = format!(
            "{{\"id\":1,\"created_at\":\"{t1}\",\"body\":\"<!-- loom:lease host=peer-host sweep=sweep-peer -->\"}}\n\
             {{\"id\":2,\"created_at\":\"{t2}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            t1 = now.to_rfc3339(),
            t2 = now.to_rfc3339(),
        );
        let (registry, counter) = lease_order_stateful_registry(dir.path(), &own_only, &both, 1);
        assert_eq!(
            registry.resolve_lease_order(9829, "sweep-mine", now),
            LeaseOrderDecision::Yield {
                earliest_host: "peer-host".to_string(),
                earliest_sweep_id: "sweep-peer".to_string(),
            },
            "a peer comment that only propagates during the confirmation phase must still be \
             found — this dispatcher must yield rather than commit to Proceed on the first, \
             sole-looking read (#6951)"
        );
        let calls: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            calls >= 2,
            "expected the initial read plus at least one confirmation re-read, got {calls}"
        );
    }

    /// Issue #6951: the confirmation phase is bounded — if this dispatcher
    /// genuinely is alone (no peer ever appears), it must still fall open to
    /// `Proceed` after exactly [`LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS`]
    /// confirmation re-reads, not loop forever or wedge dispatch.
    #[test]
    #[serial]
    fn resolve_lease_order_confirmation_bounded_when_no_peer_ever_appears() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        let own_only = format!(
            "{{\"id\":1,\"created_at\":\"{t1}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            t1 = now.to_rfc3339(),
        );
        // Never includes a peer comment, no matter how many times it is read.
        let (registry, counter) =
            lease_order_stateful_registry(dir.path(), &own_only, &own_only, 0);
        assert_eq!(
            registry.resolve_lease_order(9830, "sweep-mine", now),
            LeaseOrderDecision::Proceed,
            "a genuinely sole claimant must still proceed once the confirmation budget is \
             exhausted (#6951)"
        );
        let calls: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // One initial read (finds own comment, sole/earliest) plus the full
        // confirmation budget.
        assert_eq!(
            calls,
            1 + LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS,
            "expected exactly 1 initial read + LEASE_ORDER_SOLE_CLAIM_CONFIRM_ATTEMPTS \
             confirmation re-reads, not fewer (giving up early) or more (retrying forever)"
        );
    }

    /// Issue #6951: a peer whose comment is already visible and genuinely
    /// earlier on the very first read (the ordinary #6287 case, unrelated to
    /// the propagation-lag confirmation phase) must still yield immediately,
    /// without paying for any confirmation re-reads at all.
    #[test]
    #[serial]
    fn resolve_lease_order_yields_immediately_without_confirmation_when_peer_already_visible() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let this_host = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
            .published_host_id();
        let stdout = format!(
            "{{\"id\":1,\"created_at\":\"{t1}\",\"body\":\"<!-- loom:lease host=peer-host sweep=sweep-peer -->\"}}\n\
             {{\"id\":2,\"created_at\":\"{t2}\",\"body\":\"<!-- loom:lease host={this_host} sweep=sweep-mine -->\"}}",
            t1 = now.to_rfc3339(),
            t2 = now.to_rfc3339(),
        );
        let (registry, counter) = lease_order_stateful_registry(dir.path(), &stdout, &stdout, 0);
        assert_eq!(
            registry.resolve_lease_order(9831, "sweep-mine", now),
            LeaseOrderDecision::Yield {
                earliest_host: "peer-host".to_string(),
                earliest_sweep_id: "sweep-peer".to_string(),
            }
        );
        let calls: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            calls, 1,
            "an already-visible earlier peer must yield on the first read, without entering \
             the confirmation phase at all"
        );
    }
}
