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
}
