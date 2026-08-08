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

/// Env var toggling cross-host dispatch-collision detection (Issue #4085,
/// Phase 0 of #4028). Precedence **env > config > default**; default **off**
/// because the probe adds one extra `gh issue view` round-trip per dispatch.
/// `1`/`true`/`yes`/`on` enable; anything else disables. When enabled, the
/// daemon does a pre-flip label read and records — but never acts on — the
/// case where a peer host already claimed the issue. Detection only.
pub const COLLISION_DETECT_ENV: &str = "LOOM_DETECT_COLLISIONS";

/// Classification of a pre-flip label read (Issue #4085). Detection only — the
/// caller records the outcome but never changes dispatch behavior on it.
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

    /// When detection is enabled, probe the pre-flip label state and record —
    /// but never act on — a cross-host collision (Issue #4085). A no-op (no `gh`
    /// call at all) when detection is disabled, so the disabled dispatch path is
    /// byte-for-byte unchanged. Increments [`collision_count`](Self::collision_count)
    /// and logs a diagnostic record (issue, repo/workspace, host, timestamp,
    /// observed pre-flip labels) on a confirmed collision.
    pub(crate) fn detect_and_record_collision(&mut self, issue: u32) {
        if !self.detect_collisions {
            return;
        }
        match self.classify_preflip_labels(issue) {
            CollisionClass::Collision { labels } => {
                self.collision_count += 1;
                log::warn!(
                    "sweep_registry: cross-host dispatch collision (#4085) — issue #{issue} in \
                     {repo} was already claimed by another host before host {host} flipped it at \
                     {ts}; observed pre-flip labels=[{labels}]; running collision count={count} \
                     (detection only — dispatch continues unchanged)",
                    repo = self.config.workspace_root.display(),
                    host = host_identity(),
                    ts = Utc::now().to_rfc3339(),
                    labels = labels.join(", "),
                    count = self.collision_count,
                );
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
    pub(crate) fn probe_open_linked_pr(&self, issue: u32) -> OpenPrProbe {
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
    /// [`PARK_LABELS`]: crate::work_finder::PARK_LABELS
    pub(crate) fn first_park_label(&self, issue: u32) -> Option<String> {
        let labels = self.current_labels_via_rest(issue)?;
        crate::work_finder::PARK_LABELS
            .iter()
            .find(|park| labels.iter().any(|l| l == *park))
            .map(|park| (*park).to_string())
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
}
