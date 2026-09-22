//! Pre-spawn credential-pool gate, keyed on the **admitted runtime** (#8408).
//!
//! #4642/#7607 skip a role tick before spawning when the launch is provably
//! doomed at credential selection. Until #8363 that read `.loom/tokens/` — the
//! Claude OAuth pool — unconditionally, so a `runtimes.roles.<role> = "codex"`
//! pin went inert the moment the *Claude* pool ran dry, which is exactly the
//! pressure such a pin exists to relieve. #8363 stopped consulting the Claude
//! pool for other runtimes; this module finishes the job by gating each runtime
//! on the credential source it actually draws from:
//!
//! | Admitted runtime | Gate reads | Skip reports |
//! |---|---|---|
//! | `claude` | `.loom/tokens/` (else the shared pool) — unchanged | [`CredentialPool::ClaudeTokens`] |
//! | `codex` | enabled `loom-daemon accounts` codex profiles | [`CredentialPool::CodexAccounts`] |
//! | `pi`, `opencode`, anything else | nothing | — |
//!
//! # The one rule: never skip a launch that could have succeeded
//!
//! A pre-spawn skip is only honest when the real spawn would have died at the
//! same wall. So the codex gate mirrors `spawn-codex.sh`'s own account
//! resolution and stands down wherever that script would not reach the
//! provider-aware selector, or where the selector could still succeed:
//!
//! - **Explicit credential env** ([`CODEX_EXPLICIT_CREDENTIAL_ENV`]): the
//!   adapter uses the pinned profile and never consults the pool.
//! - **The runtime manifest's `accountProvider`** is not `codex`: the adapter
//!   selects from a different pool (it falls open to `claude` on an install
//!   whose manifest predates the key), so the codex pool is not the wall.
//! - **A class-scoped hold** (#8058) only blocks one model class, and the
//!   role's model is not resolved yet at this point — so only *account-wide*
//!   holds count as blocking here.
//! - **A `reauth_required` hold on a session-managed profile** can be released
//!   by the proactive probe the selector runs (#6927), so it is not counted as
//!   blocking either. The probe shells out to `docker`; it is deliberately NOT
//!   run here, on the role loop's synchronous decision phase.
//!
//! An inventory or health-state read error is the one fail-closed case: the
//! selector reads the same files and fails the same way (exit `78`).
//!
//! # Why native harness runtimes are not gated
//!
//! Issue #8408 proposed gating `pi`/`opencode` on the presence of the model
//! profile's `credentialEnv` variable. That is not a sound emptiness signal:
//! `defaults/docs/runtime-model-trials.md` documents authenticating the harness
//! CLI's **own auth store** as an equally valid setup, the daemon cannot
//! observe that store, and `worker_spawn::harness` treats an absent variable as
//! "nothing to map", not as an error. Gating on it would re-create the very
//! defect this issue fixes — a working runtime refused over a resource it does
//! not need. The API-key account pool (#8401) is the countable credential
//! source a native gate can be built on once it lands.
use super::*;
use crate::role_runner::outcome::{PoolHold, PoolStateFile};
use crate::runtime_admission::{ResolvedRuntime, RuntimeRejection};
use crate::tokens_pool::{account_inventory_quiet, health_snapshot, AccountProvider, HealthReason};

/// Environment variables under which `spawn-codex.sh` never reaches the
/// provider-aware account selector: the three explicit profile pins (auth
/// tiers 1-3), plus its two "skip resolution / do not exec" switches. A role
/// child inherits the daemon's environment, so any of these being non-empty
/// here means the codex account pool is not what decides this launch.
const CODEX_EXPLICIT_CREDENTIAL_ENV: [&str; 5] = [
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_CODEX_PROFILE",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
];

/// Cap on how far ahead the codex gate's `next_clear_at` estimate reports —
/// the same presentation-only 900 s bound `tokens_pool::select`'s
/// `pool_clear_estimate` uses for the Claude pool. Never a gate: every tick
/// re-reads live state.
const CODEX_CLEAR_ESTIMATE_CAP_SECS: i64 = 900;

/// Decide what — if anything — this role tick launches on, before it spawns.
///
/// **Preference-resolving since #8554.** [`static_check`] alone only ever
/// answers "is the runtime static resolution picked exhausted?" — exactly the
/// pre-#8554 behaviour, and all this function does when no preference list
/// applies. When one DOES apply (`runtimes.rolePreference.<role>`, else
/// `runtimes.preference`), the list decides the tap **for every tick, not
/// only for a tick that would otherwise skip**: `resolve_runtime` walks it
/// against live admission and credential availability, and the first tap that
/// can serve the work is the one this tick launches on. That is what makes
/// `rolePreference.judge` do the job it exists for — keeping Judge off the tap
/// that built the change (a native sweep reviews in the same session that
/// wrote it) — which a gate consulted only on exhaustion could not, since a
/// healthy top-of-chain pool would silently ignore the list.
///
/// The gate is then the **fail-closed reporter**, never bypassed: the chosen
/// tap is still put through [`static_check`], so no launch skips its own
/// pre-spawn gate, and when the whole list is unavailable the tick reports the
/// statically-admitted runtime's own skip — the same self-healing
/// [`RoleTickOutcome::PoolExhausted`] shape #7607 keeps out of the stuck-role
/// streak, not a new failure class.
///
/// - `Ok(admitted: None)`: proceed with the `admission` already passed in,
///   unchanged.
/// - `Ok(admitted: Some(runtime))`: the preference list chose `runtime` — the
///   caller MUST launch with it instead of whatever `admission` named (it may
///   be the same runtime; it is always one whose own gate just passed).
/// - `Err(outcome)`: do not spawn. No list applies and the static gate
///   skipped (byte-identical to before #8554), the chosen tap's own gate
///   skipped, or every listed tap is unavailable (fail closed).
///
/// The returned [`DispatchAdmission`](crate::runtime_preference::DispatchAdmission)
/// also carries the metered backstop slot the choice consumed (#8555). The
/// caller **must** hand it to the child it spawns
/// (`runtime_preference::handoff::attach`); dropping it instead releases the
/// slot, which is exactly right for a tick that ends up not launching.
pub(crate) fn check(
    root: &Path,
    logs: &Path,
    role: &str,
    admission: Option<&Result<ResolvedRuntime, RuntimeRejection>>,
) -> Result<crate::runtime_preference::DispatchAdmission, RoleTickOutcome> {
    // `None` ⇒ the caller opted out of admission (a test `spawn_bin`), and
    // with it out of the pool gate AND out of preference resolution: there is
    // no admitted runtime to pin, so there is nothing to re-point either.
    if admission.is_none() {
        return Ok(crate::runtime_preference::DispatchAdmission::none());
    }
    let gate = |admitted: Option<&Result<ResolvedRuntime, RuntimeRejection>>| match static_check(
        root, logs, role, admitted,
    ) {
        Some(outcome) => Err(outcome),
        None => Ok(()),
    };
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    // Dispatch intent: a tick that settles on a governed backstop tap takes a
    // metered slot (#8555). It is handed back to the caller, which attaches it
    // to the child it spawns; every early return below — including the chosen
    // tap's own pre-spawn gate refusing — drops the reservation, and with it
    // releases the slot, so a tick that does not launch never holds one.
    let context = crate::runtime_preference::DispatchContext {
        complexity: None,
        intent: crate::runtime_preference::Intent::Dispatch,
    };
    let Ok(crate::runtime_preference::Decision::Preference {
        source,
        resolution,
        backstop,
    }) = crate::runtime_preference::resolve_runtime_for(root, role, None, now, context)
    else {
        // No preference list applies to this role, an operator pin is in
        // force (a pin disables fall-through by design), or the preference
        // config itself is malformed: byte-identical to before #8554.
        gate(admission)?;
        return Ok(crate::runtime_preference::DispatchAdmission::none());
    };
    let marker = resolution.marker_line();
    let exhausted = resolution.exhausted_diagnostic(role);
    let Some(chosen) = resolution.chosen else {
        // Every listed tap was skipped. Fail closed, reporting the
        // statically-admitted runtime's own gate outcome when it has one, so
        // a fleet-wide dry list still reads as the self-healing pool skip it
        // is. `RuntimeRejected` is the remainder: a list that excludes the
        // statically-admitted runtime entirely cannot borrow that runtime's
        // verdict, and launching the unlisted runtime anyway would route
        // around the operator's configuration.
        gate(admission)?;
        note_pre_spawn_skip(logs, role, &exhausted);
        return Err(RoleTickOutcome::RuntimeRejected(RuntimeRejection {
            role: role.to_string(),
            runtime: String::new(),
            source: crate::runtime_admission::RuntimeSource::Preference,
            unmet_capabilities: vec![],
            reason: exhausted,
        }));
    };
    log::info!(
        "role_runner: {role} runtime chosen by the ordered preference list — {marker} source={} \
         (#8554)",
        source.as_str()
    );
    // `Ok` by construction, so the chosen tap can be put through its OWN
    // pre-spawn gate in the shape `static_check` reads before being handed
    // back to the caller to launch.
    let chosen_admission = Ok(chosen.admitted);
    gate(Some(&chosen_admission))?;
    // Past every gate: this tick IS launching, so the metered slot (if the
    // chosen tap took one) travels back to the caller, which attaches it to
    // the child it spawns. `?` above returns before this, dropping `backstop`
    // and releasing the slot, which is exactly what a skipped tick should do.
    Ok(crate::runtime_preference::DispatchAdmission {
        admitted: chosen_admission.ok(),
        backstop,
    })
}

/// The pre-#8554 pool gate: is the admitted runtime's OWN credential source
/// exhausted right now? Never consults the preference list — [`check`] is the
/// preference-aware wrapper around it.
///
/// `pub(crate)` for the same single reason [`check`] was before #8554:
/// `runtime_preference`'s tests pin **this** verdict against the
/// side-effect-free availability mapping that shares its reads, which is the
/// anti-drift guarantee behind "one mapping, two renderings". That comparison
/// has to run against the gate itself, not the wrapper that consults the
/// mapping. The role runner remains the only production caller of either.
pub(crate) fn static_check(
    root: &Path,
    logs: &Path,
    role: &str,
    admission: Option<&Result<ResolvedRuntime, RuntimeRejection>>,
) -> Option<RoleTickOutcome> {
    // `None` ⇒ the caller opted out of admission (a test `spawn_bin`), and
    // with it out of every pool preflight — unchanged from before #8408.
    match admission? {
        // Claude keeps its pre-#8363 ordering: the pool gate runs even when
        // admission itself was rejected (e.g. an incomplete installation).
        Ok(admitted) if admitted.runtime == "claude" => claude_gate(root, logs, role),
        Err(rejected) if rejected.runtime == "claude" => claude_gate(root, logs, role),
        // A rejected non-Claude admission is reported as the rejection it is.
        Ok(admitted) if admitted.runtime == "codex" => codex_gate(root, logs, role, admitted),
        _ => None,
    }
}

/// The #4642 / #7607 Claude token-pool gate. Behaviour, skip text, and
/// counters are byte-identical to the pre-#8408 preflight.
fn claude_gate(root: &Path, logs: &Path, role: &str) -> Option<RoleTickOutcome> {
    if crate::tokens::token_pool_size(root) == 0 {
        NO_TOKEN_POOL_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
        note_pre_spawn_skip(
            logs,
            role,
            "no token pool available (neither a per-repo .loom/tokens/ pool nor a provisioned \
         shared pool); run `loom-daemon tokens bootstrap` — #4642",
        );
        return Some(RoleTickOutcome::NoTokenPool);
    }
    // Do not launch a known exhausted Claude pool (#7607).
    let pool = crate::tokens_pool::select::spawnable_pool_state(root);
    if pool.total > 0 && pool.usable == 0 {
        POOL_EXHAUSTED_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
        let next_clear_at = crate::tokens_pool::select::pool_clear_estimate(&pool.dir);
        note_pre_spawn_skip(
            logs,
            role,
            &format!(
                "token pool exhausted: 0/{} spawnable in {} (every account bad-marked or \
                 hard-excluded by .ranking); next check ~{} — run `loom-daemon tokens \
                 check --ranking` or `loom-daemon tokens unblock <name>` — #7607",
                pool.total,
                pool.dir.display(),
                next_clear_at.to_rfc3339()
            ),
        );
        return Some(RoleTickOutcome::PoolExhausted {
            total: pool.total,
            next_clear_at,
            pool: CredentialPool::ClaudeTokens,
            // `pool.total > 0` is the guard above, and an unreadable Claude
            // pool is not a state this gate can observe, so the Claude arm
            // only ever reports the self-healing hold — the "no pool at all"
            // case is `RoleTickOutcome::NoTokenPool`, checked before it.
            hold: PoolHold::SelfHealing,
        });
    }
    None
}

/// Snapshot of the codex account pool as the gate reads it.
///
/// `pub(crate)` since #8436: `runtime_preference::availability` answers the
/// same question for a tap the ordered resolver is considering, and shares
/// this read rather than forking the mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPoolState {
    /// Enabled codex accounts in the workspace's inventory.
    pub enabled: usize,
    /// Enabled accounts not under an account-wide hold the selector could not
    /// release — see the module doc for why class-scoped holds and
    /// session-managed re-auth holds are not counted as blocking.
    pub spawnable: usize,
    /// Earliest account-wide cooldown deadline (epoch seconds) among the
    /// blocked accounts, when any has one.
    pub earliest_clear: Option<u64>,
    /// Set when the inventory or the health state could not be read; the
    /// selector fails closed on the same read, so the gate does too. Names
    /// WHICH file failed (#8444) — the gate used to blame the inventory even
    /// when it was `account-health.json` that would not parse.
    pub read_error: Option<(PoolStateFile, String)>,
}

/// Read the codex account pool for `root` at `now` (epoch seconds). Pure file
/// reads — no selection cursor is advanced and no probe is run.
///
/// `account-health.json` is read and parsed **once** per call (#8444). The
/// per-account read this replaced re-parsed the file for every enabled
/// account and left a window in which an account observed early and one
/// observed late could disagree about the same tick's state.
pub(crate) fn codex_pool_state(root: &Path, now: u64) -> CodexPoolState {
    let mut state = CodexPoolState {
        enabled: 0,
        spawnable: 0,
        earliest_clear: None,
        read_error: None,
    };
    // The quiet read (#8444): the loud variant's "registered but its profile
    // directory was not found" warning fires per unprovisioned registry entry
    // and this runs on every codex-role tick, so it is the one caller that
    // must not own that stderr line.
    let inventory = match account_inventory_quiet(root, AccountProvider::Codex) {
        Ok(inventory) => inventory,
        Err(e) => {
            state.read_error = Some((PoolStateFile::Inventory, format!("{e:#}")));
            return state;
        }
    };
    let health_state = match health_snapshot(root) {
        Ok(health) => health,
        Err(e) => {
            // Count the pool first: `enabled` is what the skip reports as
            // `0/N`, and the inventory read that produced it did succeed.
            state.enabled = inventory.iter().filter(|account| account.enabled).count();
            state.read_error = Some((PoolStateFile::HealthState, format!("{e:#}")));
            return state;
        }
    };
    for account in inventory.iter().filter(|account| account.enabled) {
        state.enabled += 1;
        let Some(health) = health_state.get(&account.id) else {
            state.spawnable += 1;
            continue;
        };
        if let Some(deadline) = health.cooldown_until.filter(|deadline| *deadline > now) {
            state.earliest_clear = Some(
                state
                    .earliest_clear
                    .map_or(deadline, |best| best.min(deadline)),
            );
            continue;
        }
        let releasable = crate::tokens_pool::session_lifecycle::is_session_managed(
            &account.credential_reference,
        );
        if health.reason == HealthReason::ReauthRequired && !releasable {
            continue;
        }
        state.spawnable += 1;
    }
    state
}

/// The account provider `spawn-codex.sh` will select from for `admitted`'s
/// runtime — the same manifest lookup, in the same order, as that script's
/// `_loom_account_provider_for_runtime`: the workspace's installed manifest,
/// else the one beside the adapter, else (missing file or key) `claude`.
///
/// **One deliberate divergence** (#8444): the script parses the manifest with
/// `jq` and falls open to `claude` when `jq` is not on `PATH`; this reader
/// parses the JSON natively and so still answers `codex` there. On such a
/// host the script would select from the *Claude* pool, making a dry codex
/// pool the wrong wall to gate on. It is left as a known gap rather than
/// mirrored, because probing `PATH` for `jq` would make this gate — and every
/// test of it — depend on a tool that is a de-facto prerequisite of the whole
/// `.loom/scripts/` surface anyway: a host missing `jq` cannot run a Loom
/// spawn adapter at all, so the divergence is unreachable in practice.
fn adapter_account_provider(root: &Path, admitted: &ResolvedRuntime) -> String {
    let file = format!("{}.json", admitted.runtime);
    let beside_adapter = admitted
        .adapter
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("runtimes").join(&file));
    std::iter::once(root.join(".loom").join("runtimes").join(&file))
        .chain(beside_adapter)
        .find(|path| path.is_file())
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|doc| {
            doc.get("accountProvider")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .filter(|provider| !provider.is_empty())
        .unwrap_or_else(|| "claude".to_string())
}

/// Whether the codex account pool is what actually decides this launch — the
/// two stand-down conditions from the module doc, in one predicate: the
/// adapter would select from a different account provider, or an explicit
/// credential pin means it never reaches the selector at all.
///
/// Extracted for #8436 so `runtime_preference::availability` asks the exact
/// same question (with the exact same environment reads and manifest lookup)
/// that [`codex_gate`] asks, rather than re-deriving a second answer that
/// could drift from this one.
pub(crate) fn codex_pool_is_the_wall(root: &Path, admitted: &ResolvedRuntime) -> bool {
    if adapter_account_provider(root, admitted) != "codex" {
        return false;
    }
    let pinned = CODEX_EXPLICIT_CREDENTIAL_ENV
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    !pinned
}

fn codex_gate(
    root: &Path,
    logs: &Path,
    role: &str,
    admitted: &ResolvedRuntime,
) -> Option<RoleTickOutcome> {
    if !codex_pool_is_the_wall(root, admitted) {
        return None;
    }
    let now = chrono::Utc::now();
    let now_epoch = u64::try_from(now.timestamp()).unwrap_or(0);
    let state = codex_pool_state(root, now_epoch);
    if state.spawnable > 0 {
        return None;
    }
    POOL_EXHAUSTED_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
    let cap = now + chrono::Duration::seconds(CODEX_CLEAR_ESTIMATE_CAP_SECS);
    let next_clear_at = state
        .earliest_clear
        .and_then(|deadline| i64::try_from(deadline).ok())
        .and_then(|deadline| chrono::DateTime::from_timestamp(deadline, 0))
        .map_or(cap, |deadline| deadline.clamp(now, cap));
    // #8444: which of the three states this is decides both the operator
    // text and — through `hold` — whether the skip is reported as the
    // self-healing hold `PoolExhausted` documents or as the permanent
    // misconfiguration the stuck-role path escalates.
    let (hold, why) = match &state.read_error {
        Some((file, error)) => (
            PoolHold::Unreadable(*file),
            format!("{} could not be read: {error}", file.as_str()),
        ),
        None if state.enabled == 0 => {
            (PoolHold::Unprovisioned, "no enabled codex account is provisioned".to_string())
        }
        None => (
            PoolHold::SelfHealing,
            "every enabled account is cooling down or needs re-auth".to_string(),
        ),
    };
    note_pre_spawn_skip(
        logs,
        role,
        &format!(
            "codex account pool exhausted: 0/{} spawnable in `loom-daemon accounts` (provider \
             codex; {why}); the Claude token pool is not consulted for the codex runtime; next \
             check ~{} — run `loom-daemon accounts list` or `loom-daemon accounts status <name>` \
             — #8408",
            state.enabled,
            next_clear_at.to_rfc3339()
        ),
    );
    Some(RoleTickOutcome::PoolExhausted {
        total: state.enabled,
        next_clear_at,
        pool: CredentialPool::CodexAccounts,
        hold,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
