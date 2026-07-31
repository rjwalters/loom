//! Insta-crash tallying, the pre-flight tripwire advisory, and the
//! per-issue quarantine lifecycle.

use super::*;

// ============================================================================
// Insta-crash quarantine (Issue #3939)
// ============================================================================
//
// The startup watchdog (#3887) and the mid-build-death watchdog (#3895) rescue a
// *hung* or *mid-build-dead* child. Neither covers the **insta-crash**: a sweep
// whose child dies within seconds of spawn (e.g. a missing token pool / selector
// import failure, #3938) is reaped, its `loom:building` claim restored to
// `loom:issue`, and it simply re-qualifies on the next work-finder poll — so the
// same broken issue is re-dispatched every tick, occupying a global concurrency
// slot forever and starving healthy work in other repos.
//
// The quarantine backstop closes that gap. The reaper counts *consecutive*
// insta-crashes per issue — a terminal transition that (a) wrote no phase
// checkpoint (never reached real work) and (b) happened within
// [`DEFAULT_QUARANTINE_INSTA_CRASH_SECS`] of dispatch. After
// [`DEFAULT_QUARANTINE_THRESHOLD`] consecutive insta-crashes the issue is
// quarantined: the work finder skips it (in-memory, so no forge round-trip is
// required for the load-bearing behavior) until a TTL
// ([`DEFAULT_QUARANTINE_TTL_SECS`]) elapses. A terminal outcome that *did* make
// progress (checkpoint present) or that was a clean/slow exit resets the counter,
// so a genuine one-off failure never accretes toward quarantine.

/// Env var toggling insta-crash quarantine (Issue #3939). `0`/`false`/`no`/`off`
/// disables; `1`/`true`/`yes`/`on` forces on. Overrides config. Defaults ON — it
/// is a safety backstop against a broken workspace starving the shared queue.
pub const QUARANTINE_ENABLE_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE";

/// Env var overriding the consecutive-insta-crash threshold at which an issue is
/// quarantined. A zero/invalid value falls through to config/default.
pub const QUARANTINE_THRESHOLD_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_THRESHOLD";

/// Env var overriding the quarantine TTL, in seconds. A zero/invalid value falls
/// through to config/default.
pub const QUARANTINE_TTL_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_TTL_SECS";

/// Env var overriding the insta-crash window, in seconds: a checkpoint-less
/// terminal transition within this wall-clock window of dispatch counts as an
/// insta-crash. A zero/invalid value falls through to config/default.
pub const QUARANTINE_INSTA_CRASH_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_INSTA_CRASH_SECS";

/// Default consecutive-insta-crash threshold before quarantine (#3939).
pub const DEFAULT_QUARANTINE_THRESHOLD: u32 = 3;

/// Default quarantine TTL: a quarantined issue is auto-released after this window
/// so a transient breakage (e.g. a token pool that was re-provisioned) recovers
/// without operator action (#3939).
pub const DEFAULT_QUARANTINE_TTL_SECS: u64 = 3600;

/// Default insta-crash window (#3939): a checkpoint-less terminal transition
/// within this many seconds of dispatch counts toward the insta-crash tally. A
/// real build that reaches even the Curator checkpoint, or a slow death past this
/// window, is a *different* failure mode (handled by the mid-build watchdog) and
/// never counts here.
pub const DEFAULT_QUARANTINE_INSTA_CRASH_SECS: i64 = 60;

/// Marker substring embedded in every quarantine comment body posted by
/// [`SweepRegistry::apply_quarantine_label`] (Issue #3939). Used by
/// [`crate::quarantine_reconciliation`] (Issue #4110) to distinguish a
/// daemon-applied `loom:blocked` from one a human deliberately applied by
/// hand — only the former is safe to auto-release at startup.
pub const QUARANTINE_COMMENT_MARKER: &str = "Auto-quarantined by loom-daemon (#3939)";

/// Resolved insta-crash-quarantine parameters (Issue #3939), set on the registry
/// at construction so [`SweepRegistry::reap_once`] can enforce them without a
/// per-tick config read. Defaults mirror the shipped constants (enabled — it is a
/// safety backstop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuarantineConfig {
    /// Whether insta-crash quarantine is active. When `false` the reaper neither
    /// counts insta-crashes nor quarantines (byte-for-byte the pre-#3939 path).
    pub enabled: bool,
    /// Consecutive insta-crashes before an issue is quarantined.
    pub threshold: u32,
    /// How long a quarantine entry persists before auto-release.
    pub ttl: Duration,
    /// The insta-crash wall-clock window: a checkpoint-less terminal transition
    /// within this many seconds of dispatch counts as an insta-crash.
    pub insta_crash_secs: i64,
}

impl Default for QuarantineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: DEFAULT_QUARANTINE_THRESHOLD,
            ttl: Duration::from_secs(DEFAULT_QUARANTINE_TTL_SECS),
            insta_crash_secs: DEFAULT_QUARANTINE_INSTA_CRASH_SECS,
        }
    }
}

// ============================================================================
// Claude-wrapper pre-flight-death workspace tripwire (Issue #4386)
// ============================================================================
//
// The per-issue insta-crash quarantine above (#3939) never trips on a
// fleet-wide, environmental spawn failure (e.g. a stale `.mcp.json`): a dozen
// *different* issues each dying once at claude-wrapper pre-flight never
// reaches any single issue's consecutive threshold, so nothing quarantines
// and `loom-daemon dispatch` / `status` report success and an idle-healthy
// daemon while every child dies within ~1s. This tripwire tracks the
// consecutive pre-flight-death streak **across issues**, independent of the
// per-issue tally, and trips a workspace-level advisory once the streak
// reaches [`PreflightTripwireConfig::threshold`].

/// Env var overriding the consecutive-pre-flight-death threshold at which the
/// workspace-level advisory trips (Issue #4386). A zero/invalid value falls
/// through to config/default.
pub const PREFLIGHT_TRIPWIRE_THRESHOLD_ENV: &str = "LOOM_PREFLIGHT_TRIPWIRE_THRESHOLD";

/// Default consecutive-pre-flight-death threshold before the workspace
/// advisory trips (#4386).
pub const DEFAULT_PREFLIGHT_TRIPWIRE_THRESHOLD: u32 = 3;

/// Resolved pre-flight-death tripwire parameters (Issue #4386), set on the
/// registry at construction so [`SweepRegistry::reap_once`] can enforce it
/// without a per-tick config read. Mirrors [`QuarantineConfig`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreflightTripwireConfig {
    /// Consecutive cross-issue pre-flight deaths before the workspace
    /// advisory trips.
    pub threshold: u32,
}

impl Default for PreflightTripwireConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_PREFLIGHT_TRIPWIRE_THRESHOLD,
        }
    }
}

/// Read `.loom/config.json → autonomous.workFinder.preflightTripwire.threshold`
/// (Issue #4386), soft-failing to `None` on a missing file, malformed JSON, or
/// an absent block — mirrors [`read_quarantine_file_config`].
#[must_use]
pub(crate) fn read_preflight_tripwire_file_threshold(repo_root: &Path) -> Option<u32> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "autonomous.workFinder.preflightTripwire")
        .and_then(|c| c.get("threshold"))
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n > 0)
        .and_then(|n| u32::try_from(n).ok())
}

/// Resolve the full [`PreflightTripwireConfig`] for `repo_root` with
/// precedence **env > config > default** (Issue #4386), mirroring
/// [`resolve_quarantine_config`].
#[must_use]
pub fn resolve_preflight_tripwire_config(repo_root: &Path) -> PreflightTripwireConfig {
    let threshold = std::env::var(PREFLIGHT_TRIPWIRE_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .or_else(|| read_preflight_tripwire_file_threshold(repo_root))
        .unwrap_or(DEFAULT_PREFLIGHT_TRIPWIRE_THRESHOLD);
    PreflightTripwireConfig { threshold }
}

// ============================================================================
// Insta-crash quarantine config resolution (Issue #3939)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.workFinder.quarantine` this
/// module consumes (Issue #3939). Each field is `Option` so an absent key falls
/// through to the env-var / built-in-default resolution — precedence
/// **env > config > default** for every knob, matching the rest of the module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuarantineFileConfig {
    /// `autonomous.workFinder.quarantine.enabled` — whether quarantine runs.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.quarantine.threshold` — consecutive insta-crashes
    /// before quarantine (zero/invalid dropped to `None`).
    pub threshold: Option<u32>,
    /// `autonomous.workFinder.quarantine.ttlSecs` — quarantine TTL, in seconds
    /// (zero/invalid dropped to `None`).
    pub ttl_secs: Option<u64>,
    /// `autonomous.workFinder.quarantine.instaCrashSecs` — insta-crash window, in
    /// seconds (zero/invalid dropped to `None`).
    pub insta_crash_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.workFinder.quarantine` (Issue #3939),
/// soft-failing every field to `None` on a missing file, malformed JSON, or an
/// absent `autonomous` / `workFinder` / `quarantine` block. Mirrors
/// [`read_startup_race_config`].
#[must_use]
pub fn read_quarantine_file_config(repo_root: &Path) -> QuarantineFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(q) = crate::config_resolver::get_path(&effective, "autonomous.workFinder.quarantine")
    else {
        return QuarantineFileConfig::default();
    };
    QuarantineFileConfig {
        enabled: q.get("enabled").and_then(serde_json::Value::as_bool),
        threshold: q
            .get("threshold")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| u32::try_from(n).ok()),
        ttl_secs: q
            .get("ttlSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        insta_crash_secs: q
            .get("instaCrashSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve the full [`QuarantineConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #3939). Reads the file
/// config internally, then layers env overrides on top, then the shipped
/// defaults. Enabled defaults **on** — it is a safety backstop.
#[must_use]
pub fn resolve_quarantine_config(repo_root: &Path) -> QuarantineConfig {
    let file = read_quarantine_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(QUARANTINE_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let threshold = std::env::var(QUARANTINE_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .or(file.threshold)
        .unwrap_or(DEFAULT_QUARANTINE_THRESHOLD);

    let ttl_secs = std::env::var(QUARANTINE_TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.ttl_secs)
        .unwrap_or(DEFAULT_QUARANTINE_TTL_SECS);

    let insta_crash_secs = std::env::var(QUARANTINE_INSTA_CRASH_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.insta_crash_secs)
        .and_then(|s| i64::try_from(s).ok())
        .unwrap_or(DEFAULT_QUARANTINE_INSTA_CRASH_SECS);

    QuarantineConfig {
        enabled,
        threshold,
        ttl: Duration::from_secs(ttl_secs),
        insta_crash_secs,
    }
}

impl SweepRegistry {
    /// The set of issue numbers currently quarantined for insta-crashing (Issue
    /// #3939). Consumed by the work finder to skip re-dispatch. TTL expiry is
    /// applied by [`reap_once`](Self::reap_once) (which the work-finder's
    /// `in_flight()` read path already calls via `reap_liveness`), so this is a
    /// plain read of the pruned map.
    #[must_use]
    pub fn quarantined_issues(&self) -> HashSet<u32> {
        self.quarantined.keys().copied().collect()
    }

    /// Sorted view of the currently-quarantined issues (Issue #3939), for the
    /// `loom-daemon status` per-repo breakdown.
    #[must_use]
    pub fn quarantined_issues_sorted(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.quarantined.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Test/inspection helper: the current consecutive-insta-crash count for an
    /// issue (Issue #3939). `0` when the issue has no recorded insta-crashes.
    #[must_use]
    pub fn insta_crash_count(&self, issue: u32) -> u32 {
        self.insta_crash_counts.get(&issue).copied().unwrap_or(0)
    }

    /// Whether `issue` is currently quarantined (Issue #3939).
    #[must_use]
    pub fn is_quarantined(&self, issue: u32) -> bool {
        self.quarantined.contains_key(&issue)
    }

    /// Manually clear an issue's quarantine + insta-crash tally (Issue #3939),
    /// the operator-action release path (reachable via the `ClearQuarantine` IPC
    /// request / `loom-daemon quarantine clear <issue>` CLI). Returns `true` when
    /// an entry existed. When an entry is actually cleared, a best-effort forge
    /// label restore re-arms `loom:issue` (mirroring [`expire_quarantine`]), so
    /// the operator command both releases the in-memory pause and re-enters the
    /// issue into the work-finder queue — the label flip alone never sufficed.
    ///
    /// A failed label restore (Issue #4110) does NOT make this return `false` —
    /// the in-memory quarantine genuinely was cleared — but the issue is added
    /// to [`pending_quarantine_release`](Self::pending_quarantine_release_issues)
    /// so the background reaper keeps retrying the flip until it succeeds,
    /// instead of leaving the issue permanently stranded at `loom:blocked`.
    pub fn clear_quarantine(&mut self, issue: u32) -> bool {
        self.insta_crash_counts.remove(&issue);
        // #4485: the operator's "let this run now" action also clears any
        // dispatch-backoff window — otherwise a cleared quarantine could still
        // be held back for up to `DispatchBackoffConfig::max`, making the
        // operator command look like it did nothing.
        self.clear_dispatch_backoff(issue);
        let was_quarantined = self.quarantined.remove(&issue).is_some();
        if was_quarantined {
            self.attempt_quarantine_release(issue);
        }
        was_quarantined
    }

    /// Read-only view of every currently-quarantined issue in this registry
    /// (Issue #4215), joining `quarantined` (applied-at), `insta_crash_counts`
    /// (tally), and `quarantine_config.ttl` into one row per issue — the data
    /// backing `loom-daemon quarantine list` / [`crate::types::Request::ListQuarantines`].
    /// Sorted by issue number, like [`Self::quarantined_issues_sorted`].
    ///
    /// `ttl_remaining_secs` is computed against `now` (passed in rather than
    /// read internally so callers — and tests — can pin the clock) and clamped
    /// to `0` for an entry already past its TTL; the actual expiry sweep only
    /// runs from [`Self::reap_once`], so a stale-but-not-yet-reaped entry is
    /// expected, not a bug.
    #[must_use]
    pub fn quarantine_entries(&self, now: DateTime<Utc>) -> Vec<crate::types::QuarantineEntry> {
        let ttl = self.quarantine_config.ttl;
        let mut entries: Vec<crate::types::QuarantineEntry> = self
            .quarantined
            .iter()
            .map(|(&issue, &quarantined_at)| {
                let elapsed = (now - quarantined_at).to_std().unwrap_or_default();
                let ttl_remaining_secs = ttl.saturating_sub(elapsed).as_secs();
                crate::types::QuarantineEntry {
                    issue,
                    workspace_root: self.config.workspace_root.clone(),
                    quarantined_at,
                    insta_crash_count: self.insta_crash_count(issue),
                    insta_crash_threshold: self.quarantine_config.threshold,
                    ttl_remaining_secs,
                }
            })
            .collect();
        entries.sort_unstable_by_key(|e| e.issue);
        entries
    }

    /// Test-only helper: seed an in-memory quarantine entry for `issue` so
    /// cross-module tests (e.g. the IPC dispatcher in `ipc.rs`) can exercise the
    /// `ClearQuarantine` path without driving the full insta-crash accrual.
    #[cfg(test)]
    pub fn seed_quarantine_for_test(&mut self, issue: u32) {
        self.quarantined.insert(issue, Utc::now());
        self.insta_crash_counts
            .insert(issue, self.quarantine_config.threshold);
    }

    /// Test-only helper (Issue #4215): like [`Self::seed_quarantine_for_test`]
    /// but with an explicit `quarantined_at` and `insta_crash_count`, so tests
    /// of `quarantine_entries` can pin a past-TTL `quarantined_at` (to exercise
    /// the `ttl_remaining_secs` clamp) or a distinct tally.
    #[cfg(test)]
    pub fn seed_quarantine_with_details_for_test(
        &mut self,
        issue: u32,
        quarantined_at: DateTime<Utc>,
        insta_crash_count: u32,
    ) {
        self.quarantined.insert(issue, quarantined_at);
        self.insta_crash_counts.insert(issue, insta_crash_count);
    }

    /// Issues currently awaiting a retried `loom:blocked` -> `loom:issue` label
    /// restore after at least one failed attempt (Issue #4110). Exposed for
    /// tests and `loom-daemon status`-style diagnostics.
    #[must_use]
    pub fn pending_quarantine_release_issues(&self) -> HashSet<u32> {
        self.pending_quarantine_release.clone()
    }

    // ------------------------------------------------------------------------
    // Insta-crash quarantine (Issue #3939)
    // ------------------------------------------------------------------------

    /// Record an insta-crash-classified death, first giving the
    /// account-exhaustion classifier a chance to re-attribute it to the spawn
    /// account (#4122).
    ///
    /// When `insta_crash` is `true` and the dead sweep's log tail matches an
    /// account-exhaustion signature, the death is charged to the ACCOUNT (the
    /// spawn token is marked bad) and the issue's insta-crash quarantine tally
    /// is left **untouched** — neither incremented (it was not the issue's
    /// fault) nor reset (exhaustion is neutral for the issue's consecutive
    /// streak). Otherwise the normal [`record_terminal_outcome`] accounting
    /// applies unchanged.
    ///
    /// [`record_terminal_outcome`]: Self::record_terminal_outcome
    pub(crate) fn record_insta_crash_outcome(
        &mut self,
        sweep_id: &SweepId,
        issue: u32,
        insta_crash: bool,
    ) {
        if insta_crash && self.insta_crash_is_account_exhaustion(sweep_id, issue) {
            return;
        }
        self.record_terminal_outcome(issue, insta_crash);
    }

    /// Persist provider health before any reaper retry/failover decision.
    pub(crate) fn apply_provider_health_feedback(
        &self,
        sweep_id: &SweepId,
        exit_code: Option<i32>,
    ) {
        let Some(info) = self.entries.get(sweep_id) else {
            return;
        };
        if info.runtime != "codex" || info.token_name == UNKNOWN_TOKEN_NAME {
            return;
        }
        let Ok(contents) = std::fs::read_to_string(&info.log_path) else {
            return;
        };
        let anchor = format!("sweep_id={sweep_id}");
        let Some(result) = parse_terminal_result_after(&contents, &anchor) else {
            return;
        };
        if result.provider != AccountProvider::Codex
            || result.account != info.token_name
            || exit_code.is_some_and(|code| code != result.exit_code)
        {
            log::warn!("sweep_registry: ignored mismatched Codex terminal feedback for {sweep_id}");
            return;
        }
        let id = AccountId {
            provider: result.provider,
            name: result.account,
        };
        if let Err(error) = tokens_pool::record_terminal(
            &self.config.workspace_root,
            &id,
            result.category,
            "spawn-codex:v1",
        ) {
            log::warn!(
                "sweep_registry: failed to persist Codex terminal feedback for {sweep_id}: {error}"
            );
        }
    }

    /// Classify whether an insta-crash death was caused by account exhaustion
    /// (#4122).
    ///
    /// Reads the tail of the dead sweep's log and matches it against the
    /// [`exhaustion_signatures`] table. On a match the spawn account is marked
    /// bad (with the Rust-side exhaustion cooldown TTL — see
    /// [`bad_tokens::is_bad`]) and `true` is returned so the caller charges the
    /// account, not the issue. Returns `false` when the log shows no exhaustion
    /// signature — the caller then applies the normal insta-crash accounting.
    ///
    /// Marking the account bad is best-effort and independent of the quarantine
    /// config: even when quarantine is disabled, an exhaustion death should
    /// still rotate the bad account out of the pool. When the spawn account was
    /// never captured (`token=unknown`) the death is still not charged to the
    /// issue, but no account can be marked bad.
    pub(crate) fn insta_crash_is_account_exhaustion(&self, sweep_id: &SweepId, issue: u32) -> bool {
        let Some(info) = self.entries.get(sweep_id) else {
            return false;
        };
        let log_path = info.log_path.clone();
        let token_name = info.token_name.clone();

        let tail = match tail_lines(&log_path, EXHAUSTION_LOG_TAIL_LINES) {
            Ok(lines) => lines.join("\n"),
            Err(_) => return false,
        };
        let Some(signature) = classify_account_exhaustion(&tail) else {
            return false;
        };

        if token_name == UNKNOWN_TOKEN_NAME {
            log::warn!(
                "sweep_registry: issue #{issue} sweep {sweep_id} insta-crashed on \
                 account-exhaustion signature '{signature}' but the spawn account was never \
                 captured (token=unknown) — NOT charging the issue's quarantine tally, but cannot \
                 mark an account bad (#4122)"
            );
            return true;
        }

        let reason = format!("exhausted: {signature} (daemon insta-crash, issue #{issue})");
        match bad_tokens::mark_bad(&self.config.workspace_root, &token_name, &reason) {
            Ok(()) => log::warn!(
                "sweep_registry: issue #{issue} sweep {sweep_id} insta-crashed on \
                 account-exhaustion signature '{signature}' — marked account '{token_name}' bad \
                 (cooldown TTL) and charged the ACCOUNT, not the issue's quarantine tally (#4122)"
            ),
            Err(e) => log::warn!(
                "sweep_registry: issue #{issue} sweep {sweep_id} insta-crashed on \
                 account-exhaustion signature '{signature}' (account '{token_name}') but mark_bad \
                 failed: {e} — still NOT charging the issue's quarantine tally (#4122)"
            ),
        }
        true
    }

    // ------------------------------------------------------------------------
    // Claude-wrapper pre-flight-death workspace tripwire (Issue #4386)
    // ------------------------------------------------------------------------

    /// Consult the workspace-level pre-flight-death streak for one terminal
    /// Issue sweep, updating [`preflight_death_streak`](Self::preflight_death_streak)
    /// and the tripwire advisory, and returning the death-class label to carry
    /// on the terminal event's `death_class` payload field (`None` when this
    /// death is not classified as pre-flight).
    ///
    /// `insta_crash` is the SAME checkpoint-less-fast-death window bool the
    /// caller already computes for [`record_insta_crash_outcome`] — a
    /// pre-flight death is, by construction, always inside that window
    /// (claude-wrapper bails within ~1s), so this only needs to inspect the
    /// log tail when `insta_crash` is `true`:
    ///
    /// - `insta_crash == false` (a clean exit, a slow death, or a call site
    ///   that already proved genuine checkpoint progress this run): this
    ///   death definitely is not a pre-flight death — reset the streak
    ///   unconditionally and return `None`.
    /// - `insta_crash == true`: classify the tail via
    ///   [`classify_preflight_outcome`]. A `Preflight` verdict increments the
    ///   streak and returns the matched label; `NonPreflight` (the log reached
    ///   `# CLAUDE_CLI_START`) resets the streak; `Unknown` (unreadable log,
    ///   or an account-exhaustion signature — exhaustion wins, #4122) leaves
    ///   the streak untouched. Either way `update_preflight_advisory` runs so
    ///   a threshold crossing is caught immediately.
    pub(crate) fn record_preflight_streak(
        &mut self,
        sweep_id: &SweepId,
        insta_crash: bool,
    ) -> Option<String> {
        if !insta_crash {
            self.reset_preflight_streak();
            return None;
        }
        let tail = self
            .entries
            .get(sweep_id)
            .map(|info| info.log_path.clone())
            .and_then(|p| tail_lines(&p, EXHAUSTION_LOG_TAIL_LINES).ok())
            .map(|lines| lines.join("\n"));

        match classify_preflight_outcome(tail.as_deref()) {
            PreflightOutcome::Preflight(label) => {
                self.preflight_death_streak += 1;
                self.preflight_death_last_marker = Some(label.to_string());
                self.update_preflight_advisory();
                Some(label.to_string())
            }
            PreflightOutcome::NonPreflight => {
                self.reset_preflight_streak();
                None
            }
            PreflightOutcome::Unknown => None,
        }
    }

    /// Reset the cross-issue pre-flight-death streak to `0` (Issue #4386):
    /// called for any terminal outcome that is definitively NOT a pre-flight
    /// death (reached `# CLAUDE_CLI_START`, or genuine checkpoint-proven
    /// progress this run). Always re-evaluates the advisory so a tripped
    /// state clears the instant a healthy dispatch is observed.
    pub(crate) fn reset_preflight_streak(&mut self) {
        self.preflight_death_streak = 0;
        self.preflight_death_last_marker = None;
        self.update_preflight_advisory();
    }

    /// Re-evaluate the workspace-level pre-flight advisory against
    /// [`preflight_death_streak`](Self::preflight_death_streak) and
    /// [`PreflightTripwireConfig::threshold`], emitting
    /// [`Event::PreflightAdvisory`] on the `daemon.preflight.advisory` topic
    /// **only on a state-change transition** (into or out of tripped) — the
    /// same dedup discipline `daemon.capacity.advisory` /
    /// `daemon.dispatch.headroom_advisory` use, so this never fires every
    /// tick.
    ///
    /// Issue #4644: waiting for `threshold` *consecutive* pre-flight deaths
    /// (the ordinary #4386 streak gate) is the wrong cadence for the "whole
    /// account pool is dead" case — when `healthy_accounts == 0`, EVERY
    /// future dispatch will die identically at token selection, so the very
    /// FIRST such death already proves it. `pool_exhausted_now` force-trips
    /// immediately (bypassing the streak/threshold check) whenever the live
    /// `.ranking` snapshot confirms zero healthy accounts, so the operator
    /// hears one aggregate signal on the first death rather than after N
    /// per-issue error blocks accumulate silently.
    pub(crate) fn update_preflight_advisory(&mut self) {
        let threshold = self.preflight_tripwire_config.threshold.max(1);
        let pool_exhausted_now = self.preflight_pool_exhausted_now();
        let should_trip = pool_exhausted_now || self.preflight_death_streak >= threshold;
        if should_trip == self.preflight_advisory_tripped {
            return;
        }
        self.preflight_advisory_tripped = should_trip;
        let marker = self.preflight_death_last_marker.clone().unwrap_or_default();
        let message = if should_trip {
            self.preflight_advisory_message(pool_exhausted_now, &marker)
        } else {
            "pre-flight advisory cleared — a recent dispatch reached claude-wrapper CLI start \
             (#4386)"
                .to_string()
        };
        if pool_exhausted_now {
            log::error!(
                "sweep_registry: workspace {} pre-flight advisory TRIPPED — token pool exhausted \
                 (0 healthy accounts), streak={} (#4644)",
                self.config.workspace_root.display(),
                self.preflight_death_streak
            );
        } else {
            log::warn!(
                "sweep_registry: workspace {} pre-flight advisory {} (streak={}, threshold={}) \
                 (#4386)",
                self.config.workspace_root.display(),
                if should_trip { "TRIPPED" } else { "cleared" },
                self.preflight_death_streak,
                threshold
            );
        }
        self.emit_event(Event::PreflightAdvisory {
            workspace_root: self.config.workspace_root.display().to_string(),
            consecutive_deaths: self.preflight_death_streak,
            marker,
            message,
        });
    }

    /// Whether the live `.ranking` snapshot confirms zero healthy accounts
    /// RIGHT NOW, given at least one pre-flight death has already been
    /// observed this streak (Issue #4644). Shared by
    /// [`update_preflight_advisory`](Self::update_preflight_advisory) (to
    /// decide whether to force-trip) and
    /// [`preflight_advisory`](Self::preflight_advisory) (so the status-surface
    /// message text never drifts from the trip decision).
    #[must_use]
    pub(crate) fn preflight_pool_exhausted_now(&self) -> bool {
        self.preflight_death_streak > 0
            && capacity::read_ranking(&self.config.workspace_root)
                .is_some_and(|snap| snap.total > 0 && snap.available == 0)
    }

    /// Render the operator-facing advisory message for the tripped state,
    /// naming the specific whole-pool-dead cause (Issue #4644) when
    /// `pool_exhausted_now` holds, else the ordinary #4386 streak wording.
    #[must_use]
    pub(crate) fn preflight_advisory_message(
        &self,
        pool_exhausted_now: bool,
        marker: &str,
    ) -> String {
        if pool_exhausted_now {
            format!(
                "WARNING: token pool exhausted (0 healthy accounts) — every dispatch will die at \
                 token selection ({marker}); add accounts (`loom-daemon tokens bootstrap`) or wait \
                 for the pool to recover before re-dispatching (#4644)"
            )
        } else {
            format!(
                "WARNING: last {} dispatches died at claude-wrapper pre-flight ({marker}) — check \
                 .mcp.json",
                self.preflight_death_streak
            )
        }
    }

    /// Record a terminal sweep outcome against the insta-crash tally (#3939).
    ///
    /// `insta_crash` is `true` only when the reaper classified the death as a
    /// true insta-crash: a checkpoint-less, non-clean exit inside the insta-crash
    /// window. In that case the per-issue consecutive counter is incremented and,
    /// on reaching [`QuarantineConfig::threshold`], the issue is quarantined
    /// (skipped by the work finder until the TTL). Any other outcome — real
    /// progress (checkpoint present) or a clean/slow exit — resets the counter, so
    /// only *consecutive* insta-crashes accrue toward quarantine.
    ///
    /// A no-op when quarantine is disabled ([`QuarantineConfig::enabled`] is
    /// `false`), preserving the pre-#3939 behavior byte-for-byte.
    pub(crate) fn record_terminal_outcome(&mut self, issue: u32, insta_crash: bool) {
        if !self.quarantine_config.enabled {
            return;
        }
        if !insta_crash {
            // Progress or a clean/slow exit breaks the consecutive run. Leave any
            // existing quarantine entry untouched — a quarantined issue is never
            // dispatched, so it cannot reach here; this only clears the runway for
            // an issue that has NOT yet been quarantined.
            self.insta_crash_counts.remove(&issue);
            return;
        }
        let count = self
            .insta_crash_counts
            .entry(issue)
            .and_modify(|c| *c += 1)
            .or_insert(1);
        let count = *count;
        if count >= self.quarantine_config.threshold && !self.quarantined.contains_key(&issue) {
            self.quarantined.insert(issue, Utc::now());
            log::warn!(
                "sweep_registry: issue #{issue} QUARANTINED after {count} consecutive \
                 insta-crashes (each <{}s with no checkpoint) — pausing re-dispatch for {}s so it \
                 stops starving the shared queue (#3939). Clear via operator action or wait for \
                 the TTL.",
                self.quarantine_config.insta_crash_secs,
                self.quarantine_config.ttl.as_secs()
            );
            self.apply_quarantine_label(issue, count);
        } else {
            log::info!(
                "sweep_registry: issue #{issue} insta-crashed ({count}/{} consecutive, <{}s, no \
                 checkpoint) (#3939)",
                self.quarantine_config.threshold,
                self.quarantine_config.insta_crash_secs
            );
        }
    }

    /// Release any quarantine entries older than the configured TTL (#3939).
    /// Called at the top of each [`reap_once`](Self::reap_once). Cheap
    /// early-return when nothing is quarantined. On release the insta-crash tally
    /// is cleared too, so the issue re-qualifies with a fresh runway, and a
    /// best-effort forge label restore re-arms `loom:issue` — retried on
    /// subsequent ticks via [`pending_quarantine_release`](Self::pending_quarantine_release_issues)
    /// if the first attempt fails (Issue #4110).
    pub(crate) fn expire_quarantine(&mut self) {
        if self.quarantined.is_empty() {
            return;
        }
        let ttl_secs = i64::try_from(self.quarantine_config.ttl.as_secs()).unwrap_or(i64::MAX);
        let now = Utc::now();
        let expired: Vec<u32> = self
            .quarantined
            .iter()
            .filter(|(_, at)| (now - **at).num_seconds() >= ttl_secs)
            .map(|(issue, _)| *issue)
            .collect();
        for issue in expired {
            self.quarantined.remove(&issue);
            self.insta_crash_counts.remove(&issue);

            // Issue #4206: before restoring the forge label, check whether a
            // human re-applied `loom:blocked` well AFTER the daemon's own
            // quarantine comment — a deliberate later park, distinguishable
            // from the daemon's own (older) quarantine by comparing the most
            // recent `labeled loom:blocked` timeline event against the most
            // recent quarantine-marker comment. If so, the in-memory entry
            // above is still purged (this issue stops occupying quarantine
            // bookkeeping/TTL churn), but the forge label is left completely
            // untouched — TTL expiry becomes a no-op for it.
            if !self.config.skip_label_flip && self.is_manually_reparked(issue) {
                log::info!(
                    "sweep_registry: issue #{issue} quarantine TTL expired, but the forge shows \
                     `loom:blocked` was re-applied by a human after the daemon's quarantine \
                     comment — purging the in-memory record WITHOUT touching the forge label \
                     (#4206)"
                );
                continue;
            }

            log::info!(
                "sweep_registry: issue #{issue} quarantine expired after {ttl_secs}s — eligible \
                 for re-dispatch again (#3939)"
            );
            self.attempt_quarantine_release(issue);
        }
    }

    /// Best-effort probe (Issue #4206) for whether `issue`'s current
    /// `loom:blocked` label was applied by a human well after the daemon's
    /// last quarantine-marker comment — i.e. a deliberate LATER park that
    /// happens to sit on an issue the daemon also quarantined at some point.
    /// Bounded by [`reap_gh_timeout`] like every other reaper-path `gh` call
    /// (Issue #3973). Fail-open (`false`) on any unresolvable signal so a
    /// forge hiccup never permanently strands a genuine daemon quarantine —
    /// see [`quarantine_reconciliation::forge::probe_manual_repark`].
    pub(crate) fn is_manually_reparked(&self, issue: u32) -> bool {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        quarantine_reconciliation::forge::probe_manual_repark(
            &gh,
            &self.config.workspace_root,
            issue,
        )
    }

    /// Retry every issue in [`pending_quarantine_release`](Self::pending_quarantine_release_issues)
    /// (Issue #4110). Called every [`reap_once`](Self::reap_once) tick, right
    /// after [`expire_quarantine`](Self::expire_quarantine): a previously
    /// failed `loom:blocked` -> `loom:issue` restore (transient `gh` failure or
    /// timeout, #3973) is retried here until it succeeds, instead of leaving
    /// the issue permanently stranded. Cheap early-return when nothing is
    /// pending. Idempotent — re-running the flip on an issue that a human
    /// already restored by hand is a harmless no-op `gh` call.
    pub(crate) fn retry_pending_quarantine_releases(&mut self) {
        if self.pending_quarantine_release.is_empty() {
            return;
        }
        let pending: Vec<u32> = self.pending_quarantine_release.iter().copied().collect();
        for issue in pending {
            self.attempt_quarantine_release(issue);
        }
    }

    /// Attempt the `loom:blocked` -> `loom:issue` label restore for `issue`
    /// (Issue #4110). On success, clears any pending-retry record. On
    /// failure, records `issue` in [`pending_quarantine_release`](Self::pending_quarantine_release_issues)
    /// (if not already there) and logs at `warn` — a silent strand is the
    /// defect this exists to prevent, so the failure must be visible above
    /// the default log level.
    pub(crate) fn attempt_quarantine_release(&mut self, issue: u32) {
        if self.release_quarantine_label(issue) {
            self.pending_quarantine_release.remove(&issue);
        } else {
            let first_attempt = self.pending_quarantine_release.insert(issue);
            log::warn!(
                "sweep_registry: quarantine release for #{issue} failed — `loom:blocked` may \
                 remain stranded on the forge; retrying on the next reaper tick (#4110){}",
                if first_attempt {
                    ""
                } else {
                    " (repeated failure)"
                }
            );
        }
    }

    /// Best-effort release of every currently-quarantined issue's
    /// `loom:blocked` label before this registry is dropped (Issue #4110).
    /// Workspace eviction ([`crate::workspace_pool::WorkspacePool::evict`])
    /// discards this registry's in-memory state, including any live
    /// quarantine and its reaper — so without this, an evicted workspace's
    /// quarantined issues would sit at `loom:blocked` until the *next* full
    /// daemon restart triggers the startup reconciliation pass
    /// ([`crate::quarantine_reconciliation`]). Each release here is a single
    /// best-effort attempt (no in-registry retry survives past eviction,
    /// since the reaper that would drive it is gone); a failure is logged at
    /// `warn` and left for the reconciliation pass to pick up at the next
    /// restart. Returns the number of quarantines flushed (attempted).
    pub fn flush_quarantines_for_eviction(&mut self) -> usize {
        let issues: Vec<u32> = self.quarantined.keys().copied().collect();
        for issue in &issues {
            self.quarantined.remove(issue);
            self.insta_crash_counts.remove(issue);
            if !self.release_quarantine_label(*issue) {
                log::warn!(
                    "sweep_registry: eviction release for #{issue} failed — `loom:blocked` may \
                     remain stranded until the next daemon-restart reconciliation pass (#4110)"
                );
            }
        }
        issues.len()
    }

    /// Best-effort forge mutation on quarantine (#3939): add `loom:blocked`,
    /// remove `loom:issue`, and post an explanatory comment so the pause is
    /// visible to a human on the forge — not just in the daemon log. Skipped
    /// entirely when label flips are disabled (test fixtures / `skip_label_flip`).
    /// Every step is best-effort: a `gh` failure is logged at debug and never
    /// affects the load-bearing in-memory quarantine.
    pub(crate) fn apply_quarantine_label(&self, issue: u32, count: u32) {
        if self.config.skip_label_flip {
            return;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));

        let mut edit = Command::new(&gh);
        edit.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--add-label")
            .arg("loom:blocked")
            .arg("--remove-label")
            .arg("loom:issue");
        edit.current_dir(&self.config.workspace_root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            edit.arg("--repo").arg(repo);
        }
        // Bounded (Issue #3973): quarantine runs from `reap_once`, which is on
        // the `ListSweeps` / `GetSweepStatus` read path.
        let timeout = reap_gh_timeout();
        match output_with_timeout(edit, timeout) {
            Ok(Some(_)) => {}
            Ok(None) => log::debug!(
                "sweep_registry: quarantine label edit for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::debug!("sweep_registry: quarantine label edit for #{issue} failed: {e}"),
        }

        let body = format!(
            "{marker}: this issue's sweep insta-crashed {count} \
             times in a row — each child died within {secs}s of dispatch without writing a phase \
             checkpoint, which almost always means an environment/configuration failure (e.g. a \
             missing token pool, #3938) rather than a problem with the issue itself. Re-dispatch \
             is paused so it stops occupying a global concurrency slot and starving other work. \
             Once the underlying cause is fixed, clear the quarantine with `loom-daemon \
             quarantine clear {issue}`, or simply wait for the {ttl}s TTL — both paths release the \
             in-memory pause AND restore `loom:issue` (#4110). Note: manually flipping \
             `loom:blocked` -> `loom:issue` on the forge does NOT release the daemon's in-memory \
             quarantine on its own — the work finder skips it until the CLI clear or the TTL \
             fires.",
            marker = QUARANTINE_COMMENT_MARKER,
            secs = self.quarantine_config.insta_crash_secs,
            ttl = self.quarantine_config.ttl.as_secs(),
        );
        let mut comment = Command::new(&gh);
        comment
            .arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(body);
        comment.current_dir(&self.config.workspace_root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            comment.arg("--repo").arg(repo);
        }
        match output_with_timeout(comment, timeout) {
            Ok(Some(_)) => {}
            Ok(None) => log::debug!(
                "sweep_registry: quarantine comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::debug!("sweep_registry: quarantine comment for #{issue} failed: {e}"),
        }
    }

    /// Best-effort forge mutation on quarantine expiry/clear (#3939): remove
    /// `loom:blocked` and re-add `loom:issue` so a released issue re-enters the
    /// work-finder queue. The mirror of [`apply_quarantine_label`]. Skipped
    /// (returns `true` — a no-op success) when label flips are disabled.
    ///
    /// Returns `true` on a confirmed successful flip, `false` otherwise
    /// (Issue #4110: previously this returned `()` and treated a completed-but-
    /// failed `gh` invocation — non-zero exit — identically to success, which is
    /// the root cause of the observed strand: the in-memory quarantine was
    /// already dropped by the caller before this ran, so a swallowed failure
    /// left `loom:blocked` on the forge with nothing left to retry it). Callers
    /// use the return value to decide whether to retry
    /// ([`attempt_quarantine_release`](Self::attempt_quarantine_release)).
    /// Every failure is logged at `warn` (was `debug`) — a silent strand is
    /// exactly the defect this exists to prevent.
    pub(crate) fn release_quarantine_label(&self, issue: u32) -> bool {
        if self.config.skip_label_flip {
            return true;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut edit = Command::new(&gh);
        edit.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:blocked")
            .arg("--add-label")
            .arg("loom:issue");
        edit.current_dir(&self.config.workspace_root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            edit.arg("--repo").arg(repo);
        }
        // Bounded (Issue #3973): expire_quarantine runs from `reap_once`, on the
        // `ListSweeps` / `GetSweepStatus` read path.
        let timeout = reap_gh_timeout();
        match output_with_timeout(edit, timeout) {
            Ok(Some(output)) if output.status.success() => true,
            Ok(Some(output)) => {
                log::warn!(
                    "sweep_registry: quarantine-release label edit for #{issue} exited {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                false
            }
            Ok(None) => {
                log::warn!(
                    "sweep_registry: quarantine-release label edit for #{issue} exceeded {}s, \
                     killed (#3973)",
                    timeout.as_secs()
                );
                false
            }
            Err(e) => {
                log::warn!(
                    "sweep_registry: quarantine-release label edit for #{issue} failed: {e}"
                );
                false
            }
        }
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

    /// AC #1: the consecutive-insta-crash counter increments and, at the
    /// threshold, quarantines. Exercises `record_terminal_outcome` directly so
    /// the classification (which depends on a real child's exit code) is not in
    /// the way of the counter/threshold logic.
    #[test]
    fn record_terminal_outcome_counts_then_quarantines_at_threshold() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        // Default config: threshold 3.
        assert_eq!(registry.quarantine_config().threshold, 3);

        registry.record_terminal_outcome(7, true);
        assert_eq!(registry.insta_crash_count(7), 1);
        assert!(!registry.is_quarantined(7));

        registry.record_terminal_outcome(7, true);
        assert_eq!(registry.insta_crash_count(7), 2);
        assert!(!registry.is_quarantined(7));

        registry.record_terminal_outcome(7, true);
        assert_eq!(registry.insta_crash_count(7), 3);
        assert!(registry.is_quarantined(7), "3rd consecutive insta-crash quarantines");
    }

    /// AC #1: a non-insta terminal outcome (real progress / clean exit) resets
    /// the consecutive tally, so only *consecutive* insta-crashes accrue.
    #[test]
    fn record_terminal_outcome_resets_run_on_progress() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        registry.record_terminal_outcome(8, true);
        registry.record_terminal_outcome(8, true);
        assert_eq!(registry.insta_crash_count(8), 2);

        // Real progress / clean exit breaks the streak.
        registry.record_terminal_outcome(8, false);
        assert_eq!(registry.insta_crash_count(8), 0);
        assert!(!registry.is_quarantined(8));

        // A subsequent insta-crash starts the count over — one is not enough.
        registry.record_terminal_outcome(8, true);
        assert_eq!(registry.insta_crash_count(8), 1);
        assert!(!registry.is_quarantined(8));
    }

    /// Disabled quarantine (config `enabled=false`) is a total no-op: no counting,
    /// no quarantine — byte-for-byte the pre-#3939 path.
    #[test]
    fn record_terminal_outcome_noop_when_disabled() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        registry.set_quarantine_config(QuarantineConfig {
            enabled: false,
            ..QuarantineConfig::default()
        });
        for _ in 0..5 {
            registry.record_terminal_outcome(9, true);
        }
        assert_eq!(registry.insta_crash_count(9), 0);
        assert!(!registry.is_quarantined(9));
    }

    /// AC #1 (end-to-end via the reaper): three consecutive checkpoint-less quick
    /// deaths — re-dispatched each time — drive the issue into quarantine. The
    /// dead-PID kill-probe yields no exit code, which the reaper treats as a
    /// non-clean death inside the insta-crash window (the #3938 case).
    #[test]
    fn reaper_quarantines_after_consecutive_insta_crashes() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        for seq in 0..3 {
            insert_dead_running(&mut registry, 42, seq);
            registry.reap_once();
        }
        assert!(registry.is_quarantined(42), "issue #42 quarantined after 3 insta-crashes");
        assert!(
            registry.quarantined_issues().contains(&42),
            "quarantined set exposes #42 to the work finder"
        );
        assert_eq!(registry.quarantined_issues_sorted(), vec![42]);
    }

    /// AC: when the live `.ranking` snapshot shows zero healthy accounts, the
    /// pre-flight advisory trips on the FIRST pre-flight death observed —
    /// not after `threshold` (default 3) consecutive deaths accumulate — so
    /// the operator gets one aggregate signal instead of N per-issue error
    /// blocks while the pool is fully dead.
    #[test]
    fn preflight_advisory_trips_immediately_when_pool_is_fully_exhausted() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        seed_token_pool(dir.path(), "agent-9"); // ensures the per-repo pool
                                                // dir wins over any shared
                                                // fallback
        std::fs::write(
            dir.path().join(".loom").join("tokens").join(".ranking"),
            "agent-9|exhausted|0.99\nagent-10|blocked|0.00\n",
        )
        .unwrap();

        assert!(!registry.preflight_advisory().0);

        insert_dead_running_with_log(
            &mut registry,
            4647,
            0,
            "agent-9",
            "==== loom-daemon dispatch: sweep-issue-4647-0 ====\nToken selection failed:\n",
        );
        registry.reap_once();

        assert_eq!(registry.preflight_death_streak(), 1, "only ONE death observed so far");
        assert!(
            registry.preflight_advisory().0,
            "healthy=0 must force an immediate trip, not wait for the streak threshold"
        );
    }

    /// The symmetric negative: an ordinary pre-flight death against a pool
    /// that still has a healthy account must NOT force-trip — it follows the
    /// normal #4386 streak/threshold cadence.
    #[test]
    fn preflight_advisory_does_not_force_trip_when_pool_has_a_healthy_account() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        seed_token_pool(dir.path(), "agent-9");
        std::fs::write(
            dir.path().join(".loom").join("tokens").join(".ranking"),
            "agent-9|available|0.10\nagent-10|exhausted|0.99\n",
        )
        .unwrap();

        insert_dead_running_with_log(
            &mut registry,
            4648,
            0,
            "agent-9",
            "==== loom-daemon dispatch: sweep-issue-4648-0 ====\nspawn-claude: preflight failed\n",
        );
        registry.reap_once();

        assert_eq!(registry.preflight_death_streak(), 1);
        assert!(
            !registry.preflight_advisory().0,
            "a single death with a healthy account still in the pool must not force-trip"
        );
    }

    /// The operator's `loom-daemon quarantine clear <issue>` releases the
    /// dispatch-backoff window too — otherwise the command would look like it did
    /// nothing for up to `max`.
    #[test]
    fn clear_quarantine_also_clears_dispatch_backoff() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        registry.seed_quarantine_for_test(34);
        registry.record_dispatch_failure(34);

        assert!(registry.clear_quarantine(34));
        assert_eq!(registry.dispatch_failure_count(34), 0);
        assert!(registry
            .dispatch_backoff_remaining(34, Utc::now())
            .is_none());
    }

    /// AC #1: an exhaustion insta-crash marks the account bad and does NOT
    /// charge the issue's quarantine tally — three in a row never quarantine.
    #[test]
    fn reaper_exhaustion_insta_crash_marks_account_not_issue() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        seed_token_pool(dir.path(), "agent-3");

        for seq in 0..3 {
            insert_dead_running_with_log(
                &mut registry,
                55,
                seq,
                "agent-3",
                "loom-daemon dispatch: start\nClaude: hit your weekly limit\n",
            );
            registry.reap_once();
        }

        // AC: not quarantined (the death was the account's fault), tally
        // untouched.
        assert!(
            !registry.is_quarantined(55),
            "exhaustion insta-crashes must not move the issue to loom:blocked (#4122)"
        );
        assert_eq!(registry.insta_crash_count(55), 0);
        // AC: the spawn account is marked bad so selection rotates past it.
        assert!(bad_tokens::is_bad(dir.path(), "agent-3"), "account marked bad on exhaustion");
    }

    /// AC #2: an insta-crash whose log does NOT match the exhaustion signature
    /// retains today's behavior — charged to the issue, account untouched.
    #[test]
    fn reaper_non_exhaustion_insta_crash_charges_issue() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        seed_token_pool(dir.path(), "agent-3");

        for seq in 0..3 {
            insert_dead_running_with_log(
                &mut registry,
                56,
                seq,
                "agent-3",
                // Includes `# CLAUDE_CLI_START` (#4386): a REAL wrapper log for
                // "the CLI actually started, then something unrelated crashed
                // it" always carries this marker — the wrapper logs it
                // unconditionally right before exec'ing the CLI. Without it,
                // this log would misclassify as a pre-flight death (#4386's
                // own carve-out), which is not what this fixture is testing.
                "loom-daemon dispatch: start\n# CLAUDE_CLI_START\nsome unrelated crash: boom\n",
            );
            registry.reap_once();
        }

        // AC: charged to the issue exactly as before #4122.
        assert!(
            registry.is_quarantined(56),
            "non-exhaustion insta-crashes still quarantine the issue"
        );
        // AC: the account is NOT marked bad for a non-exhaustion crash.
        assert!(!bad_tokens::is_bad(dir.path(), "agent-3"));
    }

    /// A single exhaustion insta-crash leaves an existing (real) tally
    /// untouched — exhaustion is neutral for the issue's consecutive streak,
    /// neither incrementing nor resetting it (#4122).
    #[test]
    fn exhaustion_insta_crash_is_neutral_for_existing_tally() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        seed_token_pool(dir.path(), "agent-3");

        // Two genuine insta-crashes accrue a tally of 2.
        registry.record_terminal_outcome(57, true);
        registry.record_terminal_outcome(57, true);
        assert_eq!(registry.insta_crash_count(57), 2);

        // An exhaustion insta-crash must not touch the tally.
        insert_dead_running_with_log(
            &mut registry,
            57,
            9,
            "agent-3",
            "Claude: hit your weekly limit\n",
        );
        registry.reap_once();

        assert_eq!(
            registry.insta_crash_count(57),
            2,
            "exhaustion neither increments nor resets the issue's tally"
        );
        assert!(!registry.is_quarantined(57));
        assert!(bad_tokens::is_bad(dir.path(), "agent-3"));
    }

    // ------------------------------------------------------------------------
    // Claude-wrapper pre-flight-death workspace tripwire (Issue #4386)
    // ------------------------------------------------------------------------

    /// AC: a reaped sweep whose log shows the claude-wrapper pre-flight
    /// marker is classified `death_class: Some("preflight-mcp-failed")` on
    /// its terminal event, and does NOT charge the issue's insta-crash
    /// quarantine tally (same carve-out reasoning as #4122's account
    /// exhaustion).
    #[tokio::test]
    async fn reaper_preflight_death_does_not_charge_insta_crash_tally() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        insert_dead_running_with_log(
            &mut registry,
            4386,
            0,
            "unknown",
            "spawn-claude: dispatching\n# MCP_PREFLIGHT_FAILED\n",
        );
        let changed = registry.reap_once();
        assert!(changed >= 1);

        let mut saw_exited = false;
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            if let Event::SweepExited {
                issue, death_class, ..
            } = ev
            {
                assert_eq!(issue, 4386);
                assert_eq!(death_class.as_deref(), Some("preflight-mcp-failed"));
                saw_exited = true;
            }
        }
        assert!(saw_exited, "expected a classified sweep.issue.4386.exited event");

        assert_eq!(
            registry.insta_crash_count(4386),
            0,
            "pre-flight deaths must not charge the issue's quarantine tally (#4386)"
        );
        assert!(!registry.is_quarantined(4386));
        assert_eq!(registry.preflight_death_streak(), 1);
    }

    /// AC: 3 consecutive pre-flight deaths across DIFFERENT issues trip the
    /// workspace-level advisory (default threshold 3); a sweep whose log
    /// reaches `# CLAUDE_CLI_START` resets the streak and clears the
    /// advisory; the advisory event fires only on the state-change
    /// transition (dedup), never every tick.
    #[tokio::test]
    async fn workspace_tripwire_trips_across_issues_resets_on_progress_and_dedups() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe(["daemon.preflight.advisory"]);

        assert!(!registry.preflight_advisory().0);

        // Two pre-flight deaths (different issues) do NOT yet trip (default
        // threshold 3).
        for (issue, seq) in [(9101u32, 0u32), (9102, 0)] {
            insert_dead_running_with_log(
                &mut registry,
                issue,
                seq,
                "unknown",
                "# MCP_PREFLIGHT_FAILED\n",
            );
            registry.reap_once();
        }
        assert!(!registry.preflight_advisory().0);
        assert_eq!(registry.preflight_death_streak(), 2);
        assert!(
            matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
            "no advisory before the streak reaches the threshold"
        );

        // A third consecutive pre-flight death, on a THIRD different issue,
        // trips it.
        insert_dead_running_with_log(&mut registry, 9103, 0, "unknown", "# MCP_PREFLIGHT_FAILED\n");
        registry.reap_once();
        let (tripped, message) = registry.preflight_advisory();
        assert!(tripped, "3 consecutive cross-issue pre-flight deaths must trip the advisory");
        let message = message.expect("advisory message present when tripped");
        assert!(message.contains("pre-flight"));
        assert!(message.contains("mcp.json"));

        match sub.recv().await.unwrap() {
            Event::PreflightAdvisory {
                consecutive_deaths, ..
            } => assert_eq!(consecutive_deaths, 3),
            other => panic!("unexpected event: {other:?}"),
        }

        // A fourth consecutive pre-flight death (still tripped) must NOT
        // re-fire the advisory event — dedup on state change only.
        insert_dead_running_with_log(&mut registry, 9104, 0, "unknown", "# MCP_PREFLIGHT_FAILED\n");
        registry.reap_once();
        assert!(registry.preflight_advisory().0);
        assert!(
            matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
            "advisory must not re-fire while already tripped (dedup)"
        );

        // A sweep whose log reaches `# CLAUDE_CLI_START` resets the streak
        // and clears the advisory, firing the clearing transition event.
        insert_dead_running_with_log(
            &mut registry,
            9105,
            0,
            "unknown",
            "# CLAUDE_CLI_START\nsome later crash\n",
        );
        registry.reap_once();
        assert_eq!(registry.preflight_death_streak(), 0);
        assert!(!registry.preflight_advisory().0);
        match sub.recv().await.unwrap() {
            Event::PreflightAdvisory {
                consecutive_deaths, ..
            } => assert_eq!(consecutive_deaths, 0),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Edge case (#4386): a dead sweep whose log file is missing/unreadable
    /// at reap time is classified "unknown", NOT pre-flight — it must
    /// neither increment nor reset the workspace streak.
    #[test]
    fn reaper_preflight_classification_unreadable_log_is_unknown_not_reset() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Prime a genuine streak of 2 via two real pre-flight deaths.
        insert_dead_running_with_log(&mut registry, 9201, 0, "unknown", "# MCP_PREFLIGHT_FAILED\n");
        registry.reap_once();
        insert_dead_running_with_log(&mut registry, 9202, 0, "unknown", "# MCP_PREFLIGHT_FAILED\n");
        registry.reap_once();
        assert_eq!(registry.preflight_death_streak(), 2);

        // A dead sweep with NO log file at all (missing/unreadable).
        insert_dead_running(&mut registry, 9203, 0);
        registry.reap_once();
        assert_eq!(
            registry.preflight_death_streak(),
            2,
            "an unreadable log must classify as unknown — neither incrementing nor resetting \
             the pre-flight streak"
        );
    }

    /// Edge case (#4386): when BOTH an account-exhaustion signature and a
    /// pre-flight marker are present in the same log tail, exhaustion wins —
    /// it is already attributed to the account, so it must not also be
    /// charged toward (or reset) the pre-flight streak.
    #[test]
    fn reaper_preflight_marker_with_exhaustion_signature_exhaustion_wins() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        seed_token_pool(dir.path(), "agent-9");

        insert_dead_running_with_log(
            &mut registry,
            9301,
            0,
            "agent-9",
            "Claude: hit your weekly limit\n# MCP_PREFLIGHT_FAILED\n",
        );
        registry.reap_once();

        assert!(
            bad_tokens::is_bad(dir.path(), "agent-9"),
            "exhaustion still marks the account bad"
        );
        assert_eq!(registry.insta_crash_count(9301), 0);
        assert!(!registry.is_quarantined(9301));
        assert_eq!(
            registry.preflight_death_streak(),
            0,
            "an exhaustion-classified death must not count toward the pre-flight streak (#4386)"
        );
    }

    /// AC #1 + #3 (#4009): a death whose checkpoint was written BY THIS run
    /// (mtime at/after `started_at`) is real mid-build progress — the
    /// mid-build-death watchdog's remit (#3895) — so it never counts toward
    /// quarantine. Each run starts in the past and (re)writes its checkpoint
    /// during the run, so the checkpoint post-dates `started_at`.
    #[test]
    fn reaper_checkpoint_death_does_not_count_as_insta_crash() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();

        for seq in 0..5 {
            // Run started 5s ago; it reaches real work and writes its checkpoint
            // now, so the checkpoint mtime post-dates `started_at`.
            insert_dead_running_at(
                &mut registry,
                43,
                seq,
                Utc::now() - chrono::Duration::seconds(5),
            );
            std::fs::write(cp_dir.join("issue-43.json"), r#"{"phase":"builder","issue":43}"#)
                .unwrap();
            registry.reap_once();
        }
        assert_eq!(registry.insta_crash_count(43), 0, "mid-build (this-run) deaths never count");
        assert!(!registry.is_quarantined(43), "a mid-build death is not quarantined");
    }

    /// AC #4 (#4009): a STALE checkpoint — one left on disk by an earlier
    /// dispatch, its mtime predating this run's `started_at` — must NOT exempt an
    /// issue from the insta-crash quarantine. Three consecutive pre-work deaths
    /// inside the insta-crash window drive the issue into quarantine even though
    /// `issue-<N>.json` exists on disk the whole time. This is the exact
    /// infinite-re-dispatch-loop regression #4009 reported (issue #4009 itself
    /// crash-looped 3x while a stale checkpoint from an earlier run persisted).
    #[test]
    fn reaper_stale_checkpoint_death_counts_as_insta_crash() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        assert_eq!(registry.quarantine_config().threshold, 3);

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        // A checkpoint written by a PRIOR dispatch, before any run below starts.
        std::fs::write(cp_dir.join("issue-45.json"), r#"{"phase":"builder","issue":45}"#).unwrap();
        let stale_written_at = std::fs::metadata(cp_dir.join("issue-45.json"))
            .and_then(|m| m.modified())
            .map(DateTime::<Utc>::from)
            .unwrap();

        // Each run starts AFTER the stale checkpoint was written (so the file is
        // stale relative to it) and dies pre-work inside the insta-crash window.
        for seq in 0..3 {
            let started_at = stale_written_at + chrono::Duration::seconds(5);
            insert_dead_running_at(&mut registry, 45, seq, started_at);
            registry.reap_once();
        }
        assert_eq!(
            registry.insta_crash_count(45),
            3,
            "stale-checkpoint pre-work deaths count toward quarantine"
        );
        assert!(
            registry.is_quarantined(45),
            "issue quarantines at threshold despite a stale checkpoint on disk"
        );
        assert!(registry.quarantined_issues().contains(&45));
    }

    /// AC #4: a quarantine entry auto-releases once it ages past the TTL, and the
    /// insta-crash tally is cleared so the issue re-qualifies with a fresh runway.
    #[test]
    fn quarantine_expires_after_ttl() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        registry.set_quarantine_config(QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ..QuarantineConfig::default()
        });

        // Quarantine #44 with a timestamp two hours in the past (past the 1h TTL).
        registry
            .quarantined
            .insert(44, Utc::now() - chrono::Duration::seconds(7200));
        registry.insta_crash_counts.insert(44, 3);
        assert!(registry.is_quarantined(44));

        // reap_once runs expire_quarantine at the top of the tick.
        registry.reap_once();
        assert!(!registry.is_quarantined(44), "aged-out quarantine is released");
        assert_eq!(registry.insta_crash_count(44), 0, "tally cleared on release");
    }

    /// AC #4: a quarantine entry within its TTL is NOT released early.
    #[test]
    fn quarantine_within_ttl_is_retained() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        registry.set_quarantine_config(QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ..QuarantineConfig::default()
        });

        // Quarantined 10s ago — well inside the 1h TTL.
        registry
            .quarantined
            .insert(45, Utc::now() - chrono::Duration::seconds(10));
        registry.reap_once();
        assert!(registry.is_quarantined(45), "a fresh quarantine survives the TTL check");
    }

    /// AC #2/#4: the operator-action release path clears both the quarantine and
    /// the insta-crash tally.
    #[test]
    fn clear_quarantine_releases_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        registry.quarantined.insert(46, Utc::now());
        registry.insta_crash_counts.insert(46, 3);

        assert!(registry.clear_quarantine(46), "returns true when an entry existed");
        assert!(!registry.is_quarantined(46));
        assert_eq!(registry.insta_crash_count(46), 0);
        assert!(!registry.clear_quarantine(46), "idempotent: false when nothing to clear");
    }

    /// `quarantine_entries` (Issue #4215) joins `quarantined`, `insta_crash_counts`,
    /// and `quarantine_config` into one row per issue, sorted by issue number
    /// like `quarantined_issues_sorted`, and reflects each issue's own tally.
    #[test]
    fn quarantine_entries_sorted_and_reflects_insta_crash_count() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let now = Utc::now();
        // Insert out of order to verify the accessor sorts, not just echoes
        // insertion order.
        registry.quarantined.insert(200, now);
        registry.insta_crash_counts.insert(200, 3);
        registry.quarantined.insert(100, now);
        registry.insta_crash_counts.insert(100, 7);

        let entries = registry.quarantine_entries(now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].issue, 100, "sorted ascending by issue number");
        assert_eq!(entries[0].insta_crash_count, 7);
        assert_eq!(entries[0].workspace_root, dir.path());
        assert_eq!(entries[0].insta_crash_threshold, registry.quarantine_config().threshold);
        assert_eq!(entries[1].issue, 200);
        assert_eq!(entries[1].insta_crash_count, 3);
    }

    /// `ttl_remaining_secs` is computed against the `now` passed in, not a
    /// fresh `Utc::now()` read internally — an issue quarantined `ttl / 2`
    /// seconds ago should show roughly half its TTL remaining, and an issue
    /// quarantined `2 * ttl` seconds ago (past-TTL, awaiting the next reaper
    /// tick) must clamp to 0 rather than go negative.
    #[test]
    fn quarantine_entries_ttl_remaining_reflects_elapsed_time() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let ttl_secs = registry.quarantine_config().ttl.as_secs();

        let now = Utc::now();
        let half_ttl_ago = now - chrono::Duration::seconds((ttl_secs / 2) as i64);
        let past_ttl = now - chrono::Duration::seconds((ttl_secs * 2) as i64);
        registry.quarantined.insert(1, half_ttl_ago);
        registry.insta_crash_counts.insert(1, 3);
        registry.quarantined.insert(2, past_ttl);
        registry.insta_crash_counts.insert(2, 3);

        let entries = registry.quarantine_entries(now);
        let e1 = entries.iter().find(|e| e.issue == 1).unwrap();
        let e2 = entries.iter().find(|e| e.issue == 2).unwrap();
        assert!(
            e1.ttl_remaining_secs > 0 && e1.ttl_remaining_secs <= ttl_secs,
            "half-elapsed entry should have a positive, bounded remainder; got {}",
            e1.ttl_remaining_secs
        );
        assert_eq!(e2.ttl_remaining_secs, 0, "past-TTL entry must clamp to 0");
    }

    /// The operator-action release path (`clear_quarantine`) must restore
    /// `loom:issue` on the forge — the whole point of the #3960 fix is that
    /// clearing the in-memory quarantine ALSO re-arms the label, so the manual
    /// path is not a dead-end that leaves the issue stuck in `loom:blocked`.
    #[test]
    fn clear_quarantine_restores_forge_label() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            gh_log.display()
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
        let mut registry = SweepRegistry::new(config);
        registry.quarantined.insert(52, Utc::now());
        registry.insta_crash_counts.insert(52, 3);

        assert!(registry.clear_quarantine(52));
        assert!(!registry.is_quarantined(52));

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 52 --remove-label loom:blocked --add-label loom:issue"),
            "expected clear_quarantine to restore loom:blocked -> loom:issue; got: {gh_calls:?}"
        );
    }

    /// A no-op clear (issue not quarantined) must NOT touch the forge — no
    /// stray label edit for an issue that was never blocked.
    #[test]
    fn clear_quarantine_noop_skips_forge_label() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let mut registry = SweepRegistry::new(config);

        assert!(!registry.clear_quarantine(999), "no-op when not quarantined");
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.is_empty(),
            "expected no forge call for a no-op clear; got: {gh_calls:?}"
        );
    }

    // ------------------------------------------------------------------------
    // Durable quarantine release (Issue #4110)
    // ------------------------------------------------------------------------

    /// TTL-driven release now performs the *real* `gh` label-flip argv.
    /// Previously the quarantine test suite only ever exercised
    /// `expire_quarantine` via `fixture_registry`, which sets
    /// `skip_label_flip = true` — so no test asserted what argv the release
    /// path actually sends to `gh`. Also confirms a subsequent tick with
    /// nothing pending makes no further `gh` calls at all (idempotence: a
    /// released issue is not repeatedly re-flipped).
    #[test]
    fn quarantine_expiry_flips_real_forge_label_via_argv() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 0);

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real release path
        let mut registry = SweepRegistry::new(config);
        registry.set_quarantine_config(QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ..QuarantineConfig::default()
        });
        registry
            .quarantined
            .insert(60, Utc::now() - chrono::Duration::seconds(7200));
        registry.insta_crash_counts.insert(60, 3);

        registry.reap_once();

        assert!(!registry.is_quarantined(60));
        assert!(
            registry.pending_quarantine_release_issues().is_empty(),
            "a successful flip leaves nothing pending"
        );
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 60 --remove-label loom:blocked --add-label loom:issue"),
            "expected TTL expiry to send the real label-flip argv; got: {gh_calls:?}"
        );

        // A later tick with nothing quarantined/pending must not re-invoke gh.
        registry.reap_once();
        let gh_calls_after = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert_eq!(gh_calls, gh_calls_after, "no further gh calls once nothing is pending");
    }

    /// Issue #4206 (Option 1): TTL expiry must never release a `loom:blocked`
    /// that the forge shows was RE-applied by a human well after the
    /// daemon's own quarantine comment — a deliberate later park. The
    /// in-memory quarantine entry is still purged (so it stops occupying TTL
    /// bookkeeping and re-firing every tick), but the forge label must be
    /// left completely untouched — this is exactly the reported crash-loop:
    /// the daemon repeatedly overriding a human's park on a
    /// previously-quarantined issue.
    #[test]
    fn quarantine_ttl_expiry_never_releases_a_later_manual_repark() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        // `gh issue view <n> --json comments --jq ...` (the marker-comment
        // recency probe) reports an old daemon quarantine comment;
        // `gh api repos/{owner}/{repo}/issues/<n>/timeline ...` (the
        // labeled-event recency probe) reports a `loom:blocked` labeling
        // event long AFTER that comment — the signature of a later manual
        // re-park.
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  echo '"2020-01-01T00:00:00Z"'
  exit 0
fi
if [ "$1" = "api" ]; then
  echo '"2030-01-01T00:00:00Z"'
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
        config.skip_label_flip = false;
        let mut registry = SweepRegistry::new(config);
        registry.set_quarantine_config(QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ..QuarantineConfig::default()
        });
        registry
            .quarantined
            .insert(4206, Utc::now() - chrono::Duration::seconds(7200));
        registry.insta_crash_counts.insert(4206, 3);

        registry.reap_once();

        assert!(
            !registry.is_quarantined(4206),
            "the in-memory quarantine entry must still be purged so TTL bookkeeping doesn't \
             keep re-firing on this issue every tick"
        );
        assert_eq!(registry.insta_crash_count(4206), 0);
        assert!(
            registry.pending_quarantine_release_issues().is_empty(),
            "a detected manual repark is not a failed release — nothing should be pending retry"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--remove-label loom:blocked"),
            "a later manual park must never have its loom:blocked label touched by TTL \
             expiry; got gh invocations: {gh_calls:?}"
        );
    }

    /// A `gh` failure during release must NOT permanently strand the issue:
    /// the entry is retained in the pending-release set and retried until it
    /// succeeds (Issue #4110). Previously `expire_quarantine` dropped the
    /// in-memory entry unconditionally and `release_quarantine_label`
    /// swallowed any failure at `debug`, so a single transient `gh` hiccup
    /// permanently stranded `loom:blocked` with nothing left in memory to
    /// retry it — reproducing the exact reported end state (`quarantine
    /// clear` -> "was not quarantined", forge still `loom:blocked`).
    ///
    /// The fake `gh` fails its first three invocations (a counter file
    /// tracks remaining failures) — enough to survive the Issue #4206
    /// manual-repark probe `expire_quarantine` now runs first, the
    /// `expire_quarantine` release attempt itself, AND the same-tick
    /// `retry_pending_quarantine_releases` pass — so the first `reap_once`
    /// tick genuinely ends with the issue still pending, and only the second
    /// tick's retry succeeds. The probe call fails open (a failed/unparsable
    /// read is never treated as a manual repark), so it does not change the
    /// release-retry semantics under test here — it just adds one more `gh`
    /// invocation ahead of the real release attempts.
    #[test]
    fn quarantine_release_retries_after_gh_failure_then_succeeds() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fail_counter = dir.path().join("fails-remaining");
        std::fs::write(&fail_counter, "3").unwrap();
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{log}\"\ncount=$(cat \"{counter}\" \
             2>/dev/null || echo 0)\nif [ \"$count\" -gt 0 ]; then\n  echo $((count - 1)) > \
             \"{counter}\"\n  exit 1\nfi\nexit 0\n",
            log = gh_log.display(),
            counter = fail_counter.display(),
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
        config.skip_label_flip = false;
        let mut registry = SweepRegistry::new(config);
        registry.set_quarantine_config(QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ..QuarantineConfig::default()
        });
        registry
            .quarantined
            .insert(61, Utc::now() - chrono::Duration::seconds(7200));
        registry.insta_crash_counts.insert(61, 3);

        // First tick: both the `expire_quarantine` attempt and the same-tick
        // `retry_pending_quarantine_releases` pass hit the fake gh's two
        // scripted failures. The in-memory quarantine is still gone
        // (`expire_quarantine` drops it before attempting the release), but
        // the issue must NOT be silently stranded — it lands in the
        // pending-retry set instead.
        registry.reap_once();
        assert!(
            !registry.is_quarantined(61),
            "quarantine memory clears regardless of flip outcome"
        );
        assert!(
            registry.pending_quarantine_release_issues().contains(&61),
            "a failed release must be retried, not dropped"
        );

        // Second tick: the fake gh's failure budget is exhausted, so the
        // retry succeeds and clears the pending entry.
        registry.reap_once();
        assert!(
            registry.pending_quarantine_release_issues().is_empty(),
            "a successful retry clears the pending-release record"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert_eq!(
            gh_calls
                .matches("issue edit 61 --remove-label loom:blocked --add-label loom:issue")
                .count(),
            3,
            "expected three attempts: two scripted failures then the successful retry; got: \
             {gh_calls:?}"
        );
    }

    /// Eviction (Issue #4110): a workspace's live quarantines are released
    /// before the pool drops the registry, instead of silently vanishing with
    /// the reaper that would otherwise have retried them.
    #[test]
    fn flush_quarantines_for_eviction_releases_and_clears_state() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 0);

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let mut registry = SweepRegistry::new(config);
        registry.quarantined.insert(62, Utc::now());
        registry.insta_crash_counts.insert(62, 3);

        let flushed = registry.flush_quarantines_for_eviction();
        assert_eq!(flushed, 1);
        assert!(!registry.is_quarantined(62));
        assert_eq!(registry.insta_crash_count(62), 0);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 62 --remove-label loom:blocked --add-label loom:issue"),
            "expected eviction to flush the real label-flip argv; got: {gh_calls:?}"
        );
    }

    /// Config resolution honors precedence env > config > default (#3939).
    #[test]
    #[serial]
    fn resolve_quarantine_config_env_overrides() {
        let dir = tempdir().unwrap();
        // No file, no env → shipped defaults (enabled, threshold 3).
        let base = resolve_quarantine_config(dir.path());
        assert!(base.enabled);
        assert_eq!(base.threshold, DEFAULT_QUARANTINE_THRESHOLD);
        assert_eq!(base.insta_crash_secs, DEFAULT_QUARANTINE_INSTA_CRASH_SECS);

        std::env::set_var(QUARANTINE_THRESHOLD_ENV, "5");
        std::env::set_var(QUARANTINE_TTL_ENV, "120");
        std::env::set_var(QUARANTINE_INSTA_CRASH_ENV, "30");
        std::env::set_var(QUARANTINE_ENABLE_ENV, "off");
        let resolved = resolve_quarantine_config(dir.path());
        std::env::remove_var(QUARANTINE_THRESHOLD_ENV);
        std::env::remove_var(QUARANTINE_TTL_ENV);
        std::env::remove_var(QUARANTINE_INSTA_CRASH_ENV);
        std::env::remove_var(QUARANTINE_ENABLE_ENV);

        assert!(!resolved.enabled, "LOOM_WORK_FINDER_QUARANTINE=off disables");
        assert_eq!(resolved.threshold, 5);
        assert_eq!(resolved.ttl, Duration::from_secs(120));
        assert_eq!(resolved.insta_crash_secs, 30);
    }

    /// Config-file parsing of `autonomous.workFinder.quarantine` (#3939).
    #[test]
    #[serial]
    fn read_quarantine_file_config_parses_block() {
        // Ensure env does not shadow the file values for the resolve assertion.
        std::env::remove_var(QUARANTINE_THRESHOLD_ENV);
        std::env::remove_var(QUARANTINE_TTL_ENV);
        std::env::remove_var(QUARANTINE_INSTA_CRASH_ENV);
        std::env::remove_var(QUARANTINE_ENABLE_ENV);

        let dir = tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            r#"{"autonomous":{"workFinder":{"quarantine":{"enabled":true,"threshold":4,"ttlSecs":900,"instaCrashSecs":45}}}}"#,
        )
        .unwrap();

        let file = read_quarantine_file_config(dir.path());
        assert_eq!(file.enabled, Some(true));
        assert_eq!(file.threshold, Some(4));
        assert_eq!(file.ttl_secs, Some(900));
        assert_eq!(file.insta_crash_secs, Some(45));

        // And the full resolver folds them in (no env set).
        let resolved = resolve_quarantine_config(dir.path());
        assert_eq!(resolved.threshold, 4);
        assert_eq!(resolved.ttl, Duration::from_secs(900));
        assert_eq!(resolved.insta_crash_secs, 45);
    }

    /// A missing `quarantine` block yields all-`None` (env/default resolution) —
    /// zero behavior change for repos that never configure it.
    #[test]
    fn read_quarantine_file_config_absent_block_is_none() {
        let dir = tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("config.json"), r#"{"autonomous":{"workFinder":{}}}"#).unwrap();
        let file = read_quarantine_file_config(dir.path());
        assert_eq!(file, QuarantineFileConfig::default());
    }

    /// config_resolver migration (#4058): a value set only at the project
    /// tier is honored identically to the legacy file.
    #[test]
    #[serial(loom_config_env)]
    fn read_quarantine_file_config_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        let project = dir.path().join(crate::config_resolver::PROJECT_CONFIG_REL);
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(
            &project,
            r#"{"autonomous":{"workFinder":{"quarantine":{"enabled":true,"threshold":4,"ttlSecs":900,"instaCrashSecs":45}}}}"#,
        )
        .unwrap();

        let file = read_quarantine_file_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            file,
            QuarantineFileConfig {
                enabled: Some(true),
                threshold: Some(4),
                ttl_secs: Some(900),
                insta_crash_secs: Some(45),
            }
        );
    }
}
