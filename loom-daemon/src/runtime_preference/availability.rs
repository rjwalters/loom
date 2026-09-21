//! "Which credential pool does this tap consume, and does it have anything
//! spawnable right now?" — the shared availability mapping (Issue #8436).
//!
//! This is the same question `role_runner::runtime_preflight` asks before a
//! role tick (#8408), asked **without the skip side effects**: no counter is
//! bumped, no `note_pre_spawn_skip` line is written, no `RoleTickOutcome` is
//! produced. The preflight gate answers "should I refuse to spawn?"; this
//! answers "may I select this tap?", which the ordered resolver needs for
//! every tap in the list, most of which it will not launch.
//!
//! | Tap runtime | Credential source | Read |
//! |---|---|---|
//! | `claude` | [`CredentialSource::ClaudeTokens`] | `.loom/tokens/` (else the shared pool) |
//! | `codex` | [`CredentialSource::CodexAccounts`] | enabled `loom-daemon accounts` codex profiles |
//! | `pi`, `opencode` | [`CredentialSource::ApiKeys`] or [`CredentialSource::Unobservable`] | the profile's credential ladder (#8401/#8428) |
//! | anything else | [`CredentialSource::Unobservable`] | nothing |
//!
//! # The one rule, restated for selection
//!
//! `runtime_preflight`'s rule is *never skip a launch that could have
//! succeeded*. Its mirror image here is **never pass over a tap that could
//! have served the work** — a false "unavailable" on a higher tier silently
//! routes paid-for subscription work onto a metered endpoint, which is the
//! exact failure the preference list exists to prevent. So every case the
//! daemon cannot actually observe resolves to [`Availability::Ungated`]:
//!
//! - A native harness may authenticate through its **own** auth store, which
//!   the daemon cannot read (`runtime-model-trials.md`). An absent
//!   `credentialEnv` is not proof of an empty credential source.
//! - A provider with **no registered API-key accounts** is not pooled on this
//!   host at all; `worker_spawn::credential`'s ladder falls through to the
//!   harness's own store, so there is no wall to gate on.
//! - A Codex tap whose adapter would select from a *different* account
//!   provider, or whose credential is pinned by environment, does not consult
//!   the codex pool — so a dry codex pool is not its wall.
//!
//! The one deliberate fail-closed case is an **unreadable** pool: accounts may
//! be registered and deliberately disabled to stop spend, and
//! `worker_spawn::credential::resolve` refuses (exit 78) rather than launch on
//! the ambient credential. A tap that would refuse at spawn must be passed
//! over here, not selected and then killed.

use super::resolve::{SkipReason, Tap};
use crate::role_runner::{PoolHold, PoolStateFile};
use crate::runtime_admission::ResolvedRuntime;
use std::path::Path;

/// The credential source a tap draws from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// The Claude OAuth token pool (`.loom/tokens/`, else the shared pool).
    ClaudeTokens,
    /// The Codex account pool (`loom-daemon accounts`, provider `codex`).
    CodexAccounts,
    /// The provider-neutral API-key account pool (#8401), namespaced by
    /// provider (`zai`, a metered endpoint's own namespace, …).
    ApiKeys { provider: String },
    /// Nothing the daemon can count. Always treated as available — see the
    /// module doc.
    Unobservable,
}

impl CredentialSource {
    /// Stable wire name, matching `CredentialPool::as_str` where the two
    /// overlap so one grep finds a pool across the role-tick record and the
    /// preference marker.
    #[must_use]
    pub fn wire(&self) -> String {
        match self {
            Self::ClaudeTokens => "claude_tokens".to_string(),
            Self::CodexAccounts => "codex_accounts".to_string(),
            Self::ApiKeys { provider } => format!("api_keys:{provider}"),
            Self::Unobservable => "unobservable".to_string(),
        }
    }
}

/// Whether a tap can be launched right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// At least one credential is selectable now. `spawnable`/`total` are
    /// `None` for an [`CredentialSource::Unobservable`] source.
    Spawnable {
        source: CredentialSource,
        spawnable: Option<usize>,
        total: Option<usize>,
    },
    /// Nothing is selectable. `hold` distinguishes the self-healing state
    /// (every member cooling down) from the two permanent ones (#8444), so a
    /// caller can tell "wait" from "an operator must act".
    Exhausted {
        source: CredentialSource,
        hold: PoolHold,
        detail: String,
    },
}

impl Availability {
    #[must_use]
    pub fn is_spawnable(&self) -> bool {
        matches!(self, Self::Spawnable { .. })
    }

    #[must_use]
    pub fn source(&self) -> &CredentialSource {
        match self {
            Self::Spawnable { source, .. } | Self::Exhausted { source, .. } => source,
        }
    }

    /// The [`SkipReason`] a resolver records when this tap is passed over.
    /// `None` when the tap is spawnable and must not be passed over.
    #[must_use]
    pub fn skip_reason(&self) -> Option<SkipReason> {
        match self {
            Self::Spawnable { .. } => None,
            Self::Exhausted { source, detail, .. } => Some(SkipReason::Unavailable {
                source: source.wire(),
                detail: detail.clone(),
            }),
        }
    }
}

/// Answer the availability question for `tap`, whose admission produced
/// `admitted`.
///
/// `now` is epoch seconds, passed in rather than read so the codex pool's
/// cooldown arithmetic is deterministic under test.
#[must_use]
pub fn availability(root: &Path, tap: &Tap, admitted: &ResolvedRuntime, now: u64) -> Availability {
    match admitted.runtime.as_str() {
        "claude" => claude(root),
        "codex" => codex(root, admitted, now),
        runtime if crate::worker_spawn::is_native(runtime) => native(root, tap, runtime),
        _ => ungated(CredentialSource::Unobservable),
    }
}

fn ungated(source: CredentialSource) -> Availability {
    Availability::Spawnable {
        source,
        spawnable: None,
        total: None,
    }
}

/// The Claude token pool, read exactly as `runtime_preflight::claude_gate`
/// reads it: the `token_pool_size == 0` "no pool at all" case first (the
/// permanent [`PoolHold::Unprovisioned`] state its `NoTokenPool` outcome
/// reports), then the #7607 `total > 0 && usable == 0` exhaustion.
///
/// Both reads are `tokens` / `tokens_pool::select` public functions, called in
/// the same order with the same arguments, so there is one implementation of
/// *what spawnable means* even though the two callers render it differently.
/// `runtime_preference::tests::claude_availability_agrees_with_the_preflight_gate`
/// pins the two verdicts together against drift.
fn claude(root: &Path) -> Availability {
    let source = CredentialSource::ClaudeTokens;
    if crate::tokens::token_pool_size(root) == 0 {
        return Availability::Exhausted {
            source,
            hold: PoolHold::Unprovisioned,
            detail: "no token pool available (neither a per-repo .loom/tokens/ pool nor a \
                     provisioned shared pool) — #4642"
                .to_string(),
        };
    }
    let pool = crate::tokens_pool::select::spawnable_pool_state(root);
    if pool.total > 0 && pool.usable == 0 {
        return Availability::Exhausted {
            source,
            hold: PoolHold::SelfHealing,
            detail: format!(
                "0/{} spawnable (every account bad-marked or hard-excluded by .ranking) — #7607",
                pool.total
            ),
        };
    }
    Availability::Spawnable {
        source,
        spawnable: Some(pool.usable),
        total: Some(pool.total),
    }
}

/// The Codex account pool, via the *same* reads `runtime_preflight`'s codex
/// gate performs — `codex_pool_is_the_wall` (the adapter's own account-
/// provider + explicit-pin logic) and `codex_pool_state`. Sharing those two
/// functions rather than re-deriving them is the "reuse, don't duplicate the
/// availability mapping" direction on #8436.
///
/// **Known blind spot (#8443).** Role ticks do not record `TOKEN_EXHAUSTED`
/// health holds — only the sweep reaper calls `record_terminal_for_model` —
/// so an account whose *usage* is capped can still read as spawnable here.
/// That is a defect in the health feed this function consumes, not in the
/// mapping; it makes the Codex tier read as available more often than it is,
/// which is the safe direction for a preference walk (it over-prefers a
/// paid-for seat rather than over-spending on a metered one), but it does mean
/// a Codex tier cannot be relied on to fall through until #8443 lands.
fn codex(root: &Path, admitted: &ResolvedRuntime, now: u64) -> Availability {
    let source = CredentialSource::CodexAccounts;
    if !crate::role_runner::runtime_preflight::codex_pool_is_the_wall(root, admitted) {
        return ungated(CredentialSource::Unobservable);
    }
    let state = crate::role_runner::runtime_preflight::codex_pool_state(root, now);
    if let Some((file, error)) = &state.read_error {
        return Availability::Exhausted {
            source,
            hold: PoolHold::Unreadable(*file),
            detail: format!("{} could not be read: {error}", file.as_str()),
        };
    }
    if state.spawnable > 0 {
        return Availability::Spawnable {
            source,
            spawnable: Some(state.spawnable),
            total: Some(state.enabled),
        };
    }
    let hold = if state.enabled == 0 {
        PoolHold::Unprovisioned
    } else {
        PoolHold::SelfHealing
    };
    Availability::Exhausted {
        source,
        hold,
        detail: format!(
            "0/{} spawnable ({}) — #8408",
            state.enabled,
            if state.enabled == 0 {
                "no enabled codex account is provisioned"
            } else {
                "every enabled account is cooling down or needs re-auth"
            }
        ),
    }
}

/// A native harness tap (`pi`, `opencode`), gated on the API-key account pool
/// **only where `worker_spawn::credential::resolve`'s ladder would actually
/// consult it** — see that module for the ladder this mirrors step for step:
///
/// 1. Every source variable already exported ⇒ the pool is never consulted.
/// 2. Exactly one unset variable with a derivable pool provider that is
///    *pooled on this host* ⇒ the pool is the wall; count it.
/// 3. Anything else (unresolvable profile, no pool provider, provider not
///    pooled) ⇒ the harness's own auth store decides, which is unobservable.
///
/// An unreadable pool directory is the fail-closed case, matching the ladder's
/// step 3 refusal.
fn native(root: &Path, tap: &Tap, runtime: &str) -> Availability {
    let config = crate::config_resolver::resolve_effective_config(root);
    // An unresolvable profile is not this function's error to report: the
    // launch path raises it with full context. Passing the tap over on a
    // config error we cannot explain here would be the "false unavailable"
    // the module doc forbids, so it stays ungated and fails loudly at launch.
    let Ok((name, profile)) =
        crate::worker_spawn::profiles::lookup(tap.model_profile.as_deref(), &config)
    else {
        return ungated(CredentialSource::Unobservable);
    };
    let Ok(selection) = crate::worker_spawn::profiles::resolve(runtime, &name, &profile) else {
        return ungated(CredentialSource::Unobservable);
    };
    // Step 1/2 of the ladder: which single source variable is unset?
    let unset: Vec<&str> = selection
        .credentials
        .iter()
        .filter(|(source, _)| std::env::var_os(source).is_none_or(|value| value.is_empty()))
        .map(|(source, _)| source.as_str())
        .collect();
    let [source_var] = unset[..] else {
        // Zero unset ⇒ fully exported, the pool is never consulted. More than
        // one ⇒ one account file cannot fill them, so the ladder is
        // environment-only (or refuses at launch with its own diagnostic).
        return ungated(CredentialSource::Unobservable);
    };
    let Some(provider) = crate::worker_spawn::credential::pool_provider(&selection, source_var)
    else {
        return ungated(CredentialSource::Unobservable);
    };
    let source = CredentialSource::ApiKeys {
        provider: provider.clone(),
    };
    match crate::api_keys_pool::is_pooled(root, &provider) {
        // Not pooled on this host: the harness's own auth store decides.
        Ok(false) => return ungated(CredentialSource::Unobservable),
        Ok(true) => {}
        Err(error) => {
            return Availability::Exhausted {
                source,
                hold: PoolHold::Unreadable(PoolStateFile::Inventory),
                detail: format!(
                    "the API-key pool for provider {provider:?} could not be read: {error}"
                ),
            }
        }
    }
    let health = match crate::api_keys_pool::select::health(root, Some(&provider)) {
        Ok(health) => health,
        Err(error) => {
            return Availability::Exhausted {
                source,
                hold: PoolHold::Unreadable(PoolStateFile::Inventory),
                detail: format!(
                    "the API-key pool for provider {provider:?} could not be read: {error}"
                ),
            }
        }
    };
    let Some(entry) = health.into_iter().find(|h| h.provider == provider) else {
        return ungated(CredentialSource::Unobservable);
    };
    if let Some(error) = entry.unreadable {
        return Availability::Exhausted {
            source,
            hold: PoolHold::Unreadable(PoolStateFile::Inventory),
            detail: format!(
                "the API-key pool for provider {provider:?} could not be read: {error}"
            ),
        };
    }
    if entry.selectable > 0 {
        return Availability::Spawnable {
            source,
            spawnable: Some(entry.selectable),
            total: Some(entry.total),
        };
    }
    Availability::Exhausted {
        source,
        hold: if entry.total == 0 {
            PoolHold::Unprovisioned
        } else {
            PoolHold::SelfHealing
        },
        detail: format!(
            "0/{} selectable for provider {provider:?} (exhausted {}, disabled {}, at capacity \
             {}) — #8401",
            entry.total, entry.exhausted, entry.disabled, entry.at_capacity
        ),
    }
}
