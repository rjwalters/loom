//! The result type for one role-runner invocation (Issue #8056 extracted it
//! from `role_runner.rs`).
//!
//! A pure data type with no behaviour beyond one predicate, split into its own
//! module because `role_runner.rs` is at its
//! `scripts/file-size-baseline.txt` ceiling and every addition there has to be
//! paid for by a removal — exactly the "put new code in a NEW sibling module"
//! rule `scripts/check-file-size-budget.sh` documents. Re-exported from
//! `role_runner`, so `crate::role_runner::RoleTickOutcome` is unchanged for
//! every caller.

use super::ModelRuntimeMismatch;

/// Which credential source a pre-spawn pool gate consulted (Issue #8408).
///
/// The #7607 gate used to read `.loom/tokens/` — the Claude OAuth pool — for
/// every role, whatever runtime the role was admitted onto, so a
/// `runtimes.roles.<role> = "codex"` pin went inert the moment the *Claude*
/// pool ran dry: the one situation the pin exists to relieve. The gate now
/// asks the pool the admitted runtime actually draws from, and this enum is
/// how a [`RoleTickOutcome::PoolExhausted`] skip says which one that was — in
/// the daemon log, the role log, and the `role_tick.outcome` record's
/// `gated_pool` key.
///
/// Native harness runtimes (`pi`, `opencode`) deliberately have no variant:
/// their credential may live in the harness CLI's own auth store, which the
/// daemon cannot observe, so an absent `credentialEnv` variable is not proof
/// of an empty credential source and nothing sound can be gated on it (see
/// `runtime_preflight`'s module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPool {
    /// The Claude OAuth token pool (`.loom/tokens/`, else the shared pool).
    ClaudeTokens,
    /// The Codex account pool (`loom-daemon accounts`, provider `codex`).
    CodexAccounts,
}

/// The file a credential-pool read failed on (Issue #8444) — named in every
/// rendering of an [`PoolHold::Unreadable`] hold so an operator is told which
/// file to repair, instead of the gate always blaming the inventory.
///
/// A `Copy` enum rather than a path/String so it can ride inside the `Copy`
/// [`PoolHold`] and so the derived detail string is byte-stable tick to tick
/// — the property the stuck-role streak is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolStateFile {
    /// The workspace account registry, `.loom/accounts.json` (or the on-disk
    /// profile discovery that stands in for it before one exists).
    Inventory,
    /// The provider health state, `.loom/account-health.json`.
    HealthState,
}

impl PoolStateFile {
    /// Operator-facing name of the file, as it reads inside a sentence.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inventory => "the account inventory (.loom/accounts.json)",
            Self::HealthState => "the account health state (.loom/account-health.json)",
        }
    }
}

/// Why a credential pool read as "nothing spawnable" (Issue #8444).
///
/// #8408's codex gate reported three different situations through one
/// [`RoleTickOutcome::PoolExhausted`] verdict, but only one of them is the
/// self-healing state that variant documents (and that the health machinery
/// assumes): every member under a cooldown / re-auth hold that ages out on
/// its own. A pool with **no account provisioned at all**, or one whose
/// inventory / health state cannot be read, can never clear without an
/// operator — reported as self-healing, it drew one WARN and was then filed
/// under "pool exhausted (N role(s) held)" forever, never reaching the
/// stuck-role escalation path that `NoTokenPool` (the Claude pool's
/// equivalent permanent state) does reach.
///
/// So the hold is carried on the outcome. The two permanent holds get a
/// **stable, timestamp-free** `detail`
/// ([`RoleTickOutcome::pool_exhausted_detail`]) so the byte-identical streak
/// in `record_role_tick_at` / `crate::health::assess_role_liveness` can form,
/// and are kept OUT of the disjoint self-healing `pool_exhausted` bucket
/// (`crate::health::RoleTickSummary::pool_exhausted`) so they surface as the
/// persistent, escalatable misconfiguration they are. `gated_pool` is
/// unchanged by the hold: it still names the pool that was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolHold {
    /// Every member is held by a cooldown / re-auth hold that clears on its
    /// own — the state [`RoleTickOutcome::PoolExhausted`] was written for.
    SelfHealing,
    /// The pool is empty as configured: zero enabled accounts/tokens exist,
    /// so no amount of waiting makes one spawnable.
    Unprovisioned,
    /// The pool's own state file could not be read or parsed, so the gate
    /// fails closed exactly where the spawn-time selector would (exit `78`).
    Unreadable(PoolStateFile),
}

impl PoolHold {
    /// Whether this hold is expected to clear with no operator action — the
    /// property `PoolExhausted`'s self-healing routing depends on.
    #[must_use]
    pub fn is_self_healing(self) -> bool {
        matches!(self, Self::SelfHealing)
    }

    /// Stable, timestamp-free tail of the ring/telemetry `detail` for a
    /// permanent hold; empty for [`Self::SelfHealing`], whose detail keeps
    /// its (deliberately volatile) next-check estimate.
    #[must_use]
    pub fn detail_suffix(self) -> &'static str {
        match self {
            Self::SelfHealing => "",
            Self::Unprovisioned => "no account provisioned (needs an operator)",
            Self::Unreadable(PoolStateFile::Inventory) => {
                "the account inventory could not be read (needs an operator)"
            }
            Self::Unreadable(PoolStateFile::HealthState) => {
                "the account health state could not be read (needs an operator)"
            }
        }
    }
}

impl CredentialPool {
    /// Stable wire value for `role_tick.outcome`'s `gated_pool` key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeTokens => "claude_tokens",
            Self::CodexAccounts => "codex_accounts",
        }
    }

    /// Operator-facing name of this pool, as it reads inside a sentence.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeTokens => "token pool",
            Self::CodexAccounts => "codex account pool",
        }
    }

    /// Leading tag of the in-memory ring / telemetry `detail` string. The
    /// Claude value is the pre-#8408 literal, byte for byte.
    #[must_use]
    pub fn detail_tag(self) -> &'static str {
        match self {
            Self::ClaudeTokens => "pool-exhausted",
            Self::CodexAccounts => "codex-account-pool-exhausted",
        }
    }

    /// The `<pool> exhausted: 0/N spawnable (<why>)` clause shared by every
    /// daemon-log WARN for this skip. The Claude rendering is the pre-#8408
    /// literal, byte for byte; only a non-Claude pool reads differently.
    #[must_use]
    pub fn exhausted_phrase(self, total: usize) -> String {
        match self {
            Self::ClaudeTokens => format!(
                "token pool exhausted: 0/{total} spawnable (every account bad-marked or \
                 hard-excluded by .ranking)"
            ),
            Self::CodexAccounts => format!(
                "codex account pool exhausted: 0/{total} spawnable (no enabled `loom-daemon \
                 accounts` codex profile is free of a cooldown or re-auth hold; the Claude \
                 token pool is not consulted for this runtime)"
            ),
        }
    }

    /// [`Self::exhausted_phrase`], widened to the three holds #8444 separates.
    /// The [`PoolHold::SelfHealing`] rendering *is* `exhausted_phrase`, byte
    /// for byte, so nothing about the pre-#8444 log lines moves; the two
    /// permanent holds read as the operator-actionable states they are rather
    /// than as "exhausted".
    #[must_use]
    pub fn hold_phrase(self, total: usize, hold: PoolHold) -> String {
        let label = self.label();
        match hold {
            PoolHold::SelfHealing => self.exhausted_phrase(total),
            PoolHold::Unprovisioned => format!(
                "{label} empty: no enabled account is provisioned at all, so this can never \
                 clear on its own — provision one or drop the runtime pin"
            ),
            PoolHold::Unreadable(file) => format!(
                "{label} unusable: {} — this can never clear on its own; repair the file",
                file.as_str()
            ),
        }
    }

    /// The `<what> still <state>` clause of the repeat-skip DEBUG line
    /// (#8444). Pool-aware — a codex-pool skip must not report "token pool",
    /// the pre-#8444 hardcoded wording — and hold-aware, so a pool that was
    /// never provisioned is not described as exhausted.
    #[must_use]
    pub fn repeat_phrase(self, hold: PoolHold) -> String {
        let label = self.label();
        match hold {
            PoolHold::SelfHealing => format!("{label} still exhausted"),
            PoolHold::Unprovisioned => format!("{label} still has no account provisioned"),
            PoolHold::Unreadable(file) => format!("{label} still unusable: {}", file.as_str()),
        }
    }
}

/// The result of one role invocation.
// Deliberately `PartialEq` only (not `Eq`): `LoadSkipped` carries an `f64`
// load-per-core reading, and `f64` has no total ordering (`NaN`), so it
// cannot implement `Eq`. Nothing in this module keys off `RoleTickOutcome`
// as a hash/ordered-set element — every comparison is `==`/`matches!`.
#[derive(Debug, Clone, PartialEq)]
pub enum RoleTickOutcome {
    /// The invocation ran to completion with a zero exit code.
    Success,
    /// The invocation could not be run, or ran and reported failure. Never
    /// fatal to the daemon — logged and skipped.
    Failure(String),
    /// Fail-closed scheduling rejection with machine-readable provenance.
    RuntimeRejected(crate::runtime_admission::RuntimeRejection),
    /// No available token pool for this workspace (issue #4642): neither a
    /// per-repo `.loom/tokens/` pool nor a provisioned shared pool
    /// (`LOOM_SHARED_TOKENS_DIR` / `~/.loom/tokens`) exists, so
    /// `spawn-claude.sh`'s own token-selection preflight is guaranteed to
    /// exit `78` (`EX_CONFIG`). A distinct variant — never folded into the
    /// generic [`RoleTickOutcome::Failure`] tally a real invocation failure
    /// increments — because this is a permanent config state until an
    /// operator provisions a pool, not a transient failure worth retrying
    /// identically forever.
    NoTokenPool,
    /// Token pool present (unlike [`Self::NoTokenPool`]) but every account in
    /// it is currently unusable — bad-marked (`.bad_tokens`) or hard-excluded
    /// by `.ranking` (`exhausted`/`blocked`) — so `spawn-claude.sh`'s own
    /// token-selection preflight is, just like `NoTokenPool`, guaranteed to
    /// exit `78` (`EX_CONFIG`) (issue #7607: ~600 wasted spawns/host/day
    /// observed while a shared pool read 0 spawnable). Kept distinct from
    /// `NoTokenPool` because the operator remedy differs (wait for a
    /// TTL/rate-limit-window clear or `tokens unblock`, vs. `tokens
    /// bootstrap`) and because this state — unlike `NoTokenPool` — is
    /// expected to clear itself the moment any account's cooldown or
    /// `.ranking` window rolls over: it is re-checked fresh on every
    /// subsequent tick with no operator action, so it must never be
    /// mistaken for a config-shaped, non-self-healing defect. Never folded
    /// into the generic [`Self::Failure`] tally, and — unlike `NoTokenPool`
    /// — also kept out of [`crate::health::RoleTickSummary::persistent`]
    /// (see [`crate::health::RoleTickSummary::pool_exhausted`]): hundreds of
    /// identical exit-78s from a shared, fleet-wide exhausted pool must not
    /// read as hundreds of broken roles.
    ///
    /// All of the above describes [`PoolHold::SelfHealing`], the hold this
    /// variant was written for. #8408's codex gate also reaches here for two
    /// states that can NEVER self-heal (`hold` is how they are told apart,
    /// #8444): a pool with zero provisioned accounts, and a pool whose state
    /// file cannot be read. Those keep this variant — and with it the
    /// `gated_pool` attribution and the operator remedy that names the right
    /// pool — but are routed to the persistent/escalatable path instead, on
    /// a stable `detail`. See [`PoolHold`].
    PoolExhausted {
        /// Total `*.token` files in the resolved pool (repo-local shadow
        /// pool when present, else shared — #3938/#7527).
        total: usize,
        /// Best-effort estimate of when at least one account might become
        /// spawnable again — see
        /// [`crate::tokens_pool::select::pool_clear_estimate`]. Never a hard
        /// gate: the very next tick re-checks the live pool state regardless
        /// of this estimate.
        next_clear_at: chrono::DateTime<chrono::Utc>,
        /// Which credential pool was read as exhausted (Issue #8408) — the
        /// one the admitted runtime actually draws from. `total` counts that
        /// pool's members: `*.token` files for [`CredentialPool::ClaudeTokens`],
        /// enabled codex profiles for [`CredentialPool::CodexAccounts`].
        pool: CredentialPool,
        /// Why the pool read as empty (Issue #8444) — and so whether this
        /// skip is the self-healing hold this variant documents, or a
        /// permanent misconfiguration that must escalate. See [`PoolHold`].
        hold: PoolHold,
    },
    /// A provable model/runtime mismatch (#5028, follow-up to #5001 AC2/AC3):
    /// the admitted runtime and the resolved model are confidently-known,
    /// differing provider families (e.g. a Claude-shaped model resolved for a
    /// role admitted onto the Codex runtime) — see
    /// [`crate::sweep_registry::model_runtime_mismatch`]. A distinct variant,
    /// deliberately never folded into the generic [`Self::Failure`] tally: it
    /// is detected BEFORE any spawn, is a permanent config conflict rather
    /// than a transient invocation failure, and self-heals the moment the
    /// conflicting config is corrected (no restart, no one-shot disable).
    ModelRuntimeMismatch(ModelRuntimeMismatch),
    /// The invocation was still running when [`DEFAULT_ROLE_TIMEOUT`] (or a
    /// test override) was reached, AND the host was measured as saturated
    /// (`load_per_core >= `[`ROLE_TIMEOUT_LOAD_SATURATION_THRESHOLD`]`) at
    /// that moment (issue #6637). A distinct variant, deliberately never
    /// folded into the generic [`Self::Failure`] tally a real invocation
    /// failure increments: a fixed 1800s wall-clock ceiling reads as a
    /// role/machinery failure to a log consumer (e.g. `fleet-check`) even
    /// when it only fired because concurrent sweeps (or other host load)
    /// starved this tick of wall-clock progress — not because the invocation
    /// itself was broken. `detail` carries the tail of the role's own log
    /// file at the moment of termination (mirrors the exit-status failure
    /// path's `tail_of_file` use) so an operator can still see which phase
    /// the invocation was in, even though this isn't counted as a failure.
    LoadSkipped {
        /// The measured load-per-core ratio at the moment the ceiling fired.
        load_per_core: f64,
        /// Tail of the role's log file at termination — the same
        /// `clean_and_cap_detail`-cleaned text a genuine timeout `Failure`
        /// would carry, retained here purely for diagnostic value.
        detail: String,
    },
}

impl RoleTickOutcome {
    /// True for a completed, successful invocation.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// True only for a [`Self::PoolExhausted`] skip of the **Claude** token
    /// pool — the one state that may feed the #6614/#7607 cross-source brake,
    /// which holds *sweep* dispatch on a token-selection wall. A dry codex
    /// account pool (#8408) says nothing about the pool sweeps draw from.
    #[must_use]
    pub fn exhausted_claude_token_pool(&self) -> bool {
        matches!(
            self,
            Self::PoolExhausted {
                pool: CredentialPool::ClaudeTokens,
                ..
            }
        )
    }

    /// Test fixture: a Claude-pool [`Self::PoolExhausted`]. Lives here rather
    /// than in `role_runner/tests.rs`, which is at its file-size ceiling.
    #[cfg(test)]
    pub(crate) fn claude_pool_exhausted(
        total: usize,
        next_clear_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self::PoolExhausted {
            total,
            next_clear_at,
            pool: CredentialPool::ClaudeTokens,
            hold: PoolHold::SelfHealing,
        }
    }

    /// Whether this tick is a [`Self::PoolExhausted`] skip of the
    /// **self-healing** kind (Issue #8444) — the one that belongs in
    /// `crate::health::RoleTickSummary::pool_exhausted`'s disjoint bucket.
    /// `false` for every other outcome, and for the two permanent holds,
    /// which are routed to the persistent/escalatable path instead.
    #[must_use]
    pub fn self_healing_pool_hold(&self) -> bool {
        matches!(self, Self::PoolExhausted { hold, .. } if hold.is_self_healing())
    }

    /// The `<pool> <state>: …` clause every daemon-log WARN for a
    /// [`Self::PoolExhausted`] skip is built from — pool- and hold-aware
    /// (#8408/#8444). Empty for every other outcome (callers reach it only
    /// from inside a `PoolExhausted` arm).
    #[must_use]
    pub fn pool_hold_phrase(&self) -> String {
        match self {
            Self::PoolExhausted {
                total, pool, hold, ..
            } => pool.hold_phrase(*total, *hold),
            _ => String::new(),
        }
    }

    /// The repeat-skip DEBUG line's `<pool> still <state>` clause (#8444) —
    /// see [`CredentialPool::repeat_phrase`]. Empty for every other outcome.
    #[must_use]
    pub fn pool_repeat_phrase(&self) -> String {
        match self {
            Self::PoolExhausted { pool, hold, .. } => pool.repeat_phrase(*hold),
            _ => String::new(),
        }
    }

    /// The in-memory ring / telemetry `detail` for a [`Self::PoolExhausted`]
    /// tick. The self-healing hold keeps its pre-#8444 rendering, byte for
    /// byte — deliberately volatile, so it can never build an escalation
    /// streak. A permanent hold is **stable tick to tick** (no counts that
    /// move, no next-check estimate) precisely so it can: that streak is what
    /// `crate::health::assess_role_liveness` reports as a stuck role. Empty
    /// for every other outcome.
    #[must_use]
    pub fn pool_exhausted_detail(&self) -> String {
        match self {
            Self::PoolExhausted {
                total,
                next_clear_at,
                pool,
                hold,
            } if hold.is_self_healing() => format!(
                "{}: 0/{total} spawnable, next check ~{}",
                pool.detail_tag(),
                next_clear_at.to_rfc3339()
            ),
            Self::PoolExhausted { pool, hold, .. } => {
                format!("{}: {}", pool.detail_tag(), hold.detail_suffix())
            }
            _ => String::new(),
        }
    }

    /// Which credential pool gated this tick pre-spawn, as the stable
    /// `role_tick.outcome` `gated_pool` wire value (Issue #8408). `None` for
    /// every outcome that was not a credential-pool skip.
    /// [`Self::NoTokenPool`] is by construction a Claude-pool verdict: it is
    /// only ever produced for a role admitted onto the `claude` runtime.
    #[must_use]
    pub fn gated_pool(&self) -> Option<&'static str> {
        match self {
            Self::NoTokenPool => Some(CredentialPool::ClaudeTokens.as_str()),
            Self::PoolExhausted { pool, .. } => Some(pool.as_str()),
            _ => None,
        }
    }
}
