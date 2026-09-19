//! Provider-scoped account health for runtimes which do not have Claude's
//! quota probe. This state is deliberately separate from the legacy Claude
//! `.ranking`, `.bad_tokens`, and `.failure_counts` files.
//!
//! # Model-class scoping (#8058 Phase 2)
//!
//! Phase 1 taught the *Claude* pool that an exhaustion known to be scoped to a
//! single model class should not bad-mark the account for every other class
//! (`bad_tokens::scoped_reason` / `bad_tokens::is_bad_for_class`). This module
//! is the same fix for every other provider, expressed in this module's own
//! state shape rather than in `.bad_tokens` lines:
//!
//! * An **account-wide** hold is [`AccountHealth::cooldown_until`] (plus
//!   [`HealthReason::ReauthRequired`], which is sticky and has no deadline).
//!   `TOKEN_EXHAUSTED`, `RECOVERABLE`, and `SESSION_LIMIT` all still produce
//!   one, unchanged.
//! * A **class-scoped** hold is one entry in
//!   [`AccountHealth::class_cooldowns`] — its own deadline, ageing out on its
//!   own schedule, exactly like Phase 1's per-line `.bad_tokens` marks.
//!   `MODEL_CREDITS_EXHAUSTED` produces one *when the caller names the model
//!   that was in flight*.
//!
//! The rule, in both directions — this mirrors Phase 1 deliberately, because
//! the widening/narrowing hazards are identical:
//!
//! * Nothing here ever **widens** a mark. A class-scoped hold blocks strictly
//!   less than an account-wide one: it can only ever make a class-scoped query
//!   ([`select_healthy_for_class_at`]) succeed where the account-wide one
//!   fails, never the reverse.
//! * Nothing here ever **narrows** an existing account-wide mark. A record
//!   with no class — every state written before this phase, and every
//!   `MODEL_CREDITS_EXHAUSTED` whose model the caller did not supply — stays
//!   account-wide and keeps blocking all classes. Unknown model ⇒ account-wide
//!   is the fail-safe default, never an error.
//! * The **class-less question is unchanged**: [`select_healthy_at`] and
//!   [`AccountHealth::is_eligible_at`] mean "is this account usable at all",
//!   so a live class-scoped hold still blocks them — precisely as a
//!   class-scoped `.bad_tokens` line still blocks Phase 1's [`is_bad`]. Only a
//!   caller that names a *different* class gets the narrower answer.
//! * `ReauthRequired` stays account-wide and permanent: a broken credential is
//!   broken for every class, so the sticky-reauth arm swallows class-scoped
//!   feedback exactly as it swallows every other terminal signal.
//!
//! [`is_bad`]: super::bad_tokens::is_bad

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::account_registry::{AccountDescriptor, AccountId, AccountProvider};
use super::locking::MkdirLock;

const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_EXHAUSTED_COOLDOWN_SECS: u64 = 5 * 60 * 60;
pub const DEFAULT_RECOVERABLE_BACKOFF_SECS: u64 = 60;
pub const DEFAULT_SESSION_BACKOFF_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalClassification {
    Success,
    TokenExpired,
    TokenExhausted,
    /// Per-model-tier usage credits ran out (issue #5687). Credits are scoped
    /// to one model class, so — unlike
    /// [`TerminalClassification::TokenExhausted`], which is an account-wide
    /// plan/quota hold — this records a **class-scoped** cooldown in
    /// [`AccountHealth::class_cooldowns`] and leaves the account serving every
    /// other class (#8058 Phase 2). The two arms were fused until that phase,
    /// because the pool had no per-model account state to narrow into; it has
    /// one now.
    ///
    /// The narrowing is conditional and fail-safe: it applies only when the
    /// caller names the model that was in flight
    /// ([`record_terminal_for_model_at`]). With no model — the class-less
    /// [`record_terminal_at`] — this still records the account-wide
    /// `PlanExhausted` hold it always did, because "which class ran out" is
    /// exactly what a class-scoped mark has to know and must never guess.
    ModelCreditsExhausted,
    Recoverable,
    Timeout,
    Fatal,
    CwdDeleted,
    ModelRefusal,
    SessionLimit,
}

impl std::str::FromStr for TerminalClassification {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(match value {
            "SUCCESS" => Self::Success,
            "TOKEN_EXPIRED" => Self::TokenExpired,
            "TOKEN_EXHAUSTED" => Self::TokenExhausted,
            "MODEL_CREDITS_EXHAUSTED" => Self::ModelCreditsExhausted,
            "RECOVERABLE" => Self::Recoverable,
            "TIMEOUT" => Self::Timeout,
            "FATAL" => Self::Fatal,
            "CWD_DELETED" => Self::CwdDeleted,
            "MODEL_REFUSAL" => Self::ModelRefusal,
            "SESSION_LIMIT" => Self::SessionLimit,
            other => bail!("unknown terminal classification {other:?}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    Healthy,
    ReauthRequired,
    PlanExhausted,
    TransientFailure,
    SessionLimit,
    /// Per-model-class usage credits ran out and the account has **no**
    /// account-wide hold (#8058 Phase 2). The blocking deadlines live in
    /// [`AccountHealth::class_cooldowns`], never in
    /// [`AccountHealth::cooldown_until`] — this reason exists so an operator
    /// reading the state file can tell "one class is out of credits" apart
    /// from the account-wide `PlanExhausted` that `TOKEN_EXHAUSTED` produces.
    ///
    /// Never overwrites a *live* account-wide reason: an account already
    /// holding a `PlanExhausted`/`TransientFailure`/`SessionLimit` cooldown
    /// keeps it, because the wider hold is the one that decides selection.
    ModelCreditsExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountHealth {
    pub provider: AccountProvider,
    pub name: String,
    pub reason: HealthReason,
    pub updated_at: u64,
    pub signal_provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<u64>,
    #[serde(default)]
    pub consecutive_transient_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success: Option<u64>,
    /// When a *proactive* auth-state probe last produced a conclusive result
    /// for this account (issue #6927). Distinct from `updated_at`, which any
    /// reactive terminal signal also moves: this field exists so a caller can
    /// rate-limit re-probing without re-running the probe to find out how
    /// stale it is. `None` on every record written before #6927 and on any
    /// account that has only ever been judged reactively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe: Option<u64>,
    /// Per-model-class exhaustion deadlines (#8058 Phase 2): `class` →
    /// `cooldown_until`. Written only by `MODEL_CREDITS_EXHAUSTED` feedback
    /// that named the model in flight; each entry ages out on its own
    /// schedule, independently of every other class and of the account-wide
    /// [`Self::cooldown_until`].
    ///
    /// Additive and optional, exactly like `last_probe` (#6927) before it:
    /// `#[serde(default)]` means every record written before this phase reads
    /// back as "no class-scoped holds" (i.e. account-wide behaviour,
    /// unchanged), and `skip_serializing_if` keeps the field off disk entirely
    /// until an account actually has one. That is why [`SCHEMA_VERSION`] is
    /// **not** bumped here: `read_state` rejects any version it does not
    /// recognize outright and there is no migration path, so bumping would
    /// hard-fail every live `account-health.json` on upgrade — an outage, in
    /// exchange for a field that already round-trips through both the old and
    /// the new reader.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub class_cooldowns: HashMap<String, u64>,
}

impl AccountHealth {
    fn id(&self) -> AccountId {
        AccountId {
            provider: self.provider,
            name: self.name.clone(),
        }
    }

    /// Whether this account is usable **at all** at `now`.
    ///
    /// The account-wide question, unchanged by #8058 Phase 2: a live
    /// class-scoped hold still counts here, exactly as a class-scoped
    /// `.bad_tokens` line still counts for Phase 1's
    /// [`super::bad_tokens::is_bad`]. Callers that can name the class they
    /// need should ask [`Self::is_eligible_for_class_at`] instead.
    #[must_use]
    pub fn is_eligible_at(&self, now: u64) -> bool {
        self.is_eligible_for_class_at(now, None)
    }

    /// [`Self::is_eligible_at`], narrowed to one model class (#8058 Phase 2).
    ///
    /// `model_class` is an already-normalized class (see [`model_class_of`]);
    /// `None` is exactly [`Self::is_eligible_at`]. A class-scoped query is
    /// strictly narrower — it can only ever return `true` where the
    /// account-wide question returns `false`, because every account-wide hold
    /// (`ReauthRequired`, `cooldown_until`) is checked first and identically.
    #[must_use]
    pub fn is_eligible_for_class_at(&self, now: u64, model_class: Option<&str>) -> bool {
        if self.reason == HealthReason::ReauthRequired {
            return false;
        }
        if self.cooldown_until.is_some_and(|deadline| deadline > now) {
            return false;
        }
        self.blocking_class_cooldown_at(now, model_class).is_none()
    }

    /// The live class-scoped hold that blocks `model_class` at `now`, as
    /// `(class, cooldown_until)`, or `None` when no class hold applies.
    ///
    /// A `None` class asks the account-wide question, so *any* live class hold
    /// answers it; the one reported back is the latest-clearing (ties broken
    /// by class name) so the operator-facing message never promises an earlier
    /// recovery than the state actually allows.
    #[must_use]
    pub fn blocking_class_cooldown_at(
        &self,
        now: u64,
        model_class: Option<&str>,
    ) -> Option<(&str, u64)> {
        match model_class.and_then(normalize_class) {
            Some(queried) => self
                .class_cooldowns
                .get_key_value(&queried)
                .filter(|(_, deadline)| **deadline > now)
                .map(|(class, deadline)| (class.as_str(), *deadline)),
            None => {
                let mut blocking: Option<(&str, u64)> = None;
                for (class, deadline) in &self.class_cooldowns {
                    if *deadline <= now {
                        continue;
                    }
                    let candidate = (class.as_str(), *deadline);
                    if blocking.is_none_or(|current| {
                        (candidate.1, std::cmp::Reverse(candidate.0))
                            > (current.1, std::cmp::Reverse(current.0))
                    }) {
                        blocking = Some(candidate);
                    }
                }
                blocking
            }
        }
    }
}

/// Normalize an already-classified model class for use as a
/// [`AccountHealth::class_cooldowns`] key, or `None` when the value carries no
/// usable class (#8058 Phase 2).
///
/// Deliberately permissive-then-rejecting: trims, lowercases, and requires the
/// result to be a non-empty run of `[a-z0-9._-]`. Anything else — empty,
/// whitespace, a stray quote, an embedded space — reads as **no class**, which
/// is the account-wide (fail-safe) answer on both the write and the read side.
fn normalize_class(class: &str) -> Option<String> {
    let normalized = class.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || !normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(normalized)
}

/// Resolve a raw model alias or pinned ID to the class a class-scoped health
/// hold is keyed by, or `None` when the value carries no usable class (#8058
/// Phase 2).
///
/// A deliberate **superset** of [`super::bad_tokens::model_class_of`], not a
/// second copy of it: a Claude model is routed through that function first, so
/// `claude-opus-5` and `opus` collapse to the one `opus` class the Claude pool
/// already uses and the two phases never disagree about a shared vocabulary.
/// Every other provider — Codex above all, the reason this module exists —
/// has no such classifier, so its model string normalizes to itself
/// ([`normalize_class`]). That is the narrow direction: two Codex model names
/// that happen to share one credit pool become two independent holds, which at
/// worst costs one failed dispatch that re-marks the second class, whereas
/// collapsing them by guesswork would block a class that still had credit.
///
/// **Unrecognized is `None`, never an error** — the same contract Phase 1
/// chose. `None` degrades to today's account-wide behaviour everywhere it is
/// consumed; account health must never fail closed on a model name it has not
/// been taught.
#[must_use]
pub fn model_class_of(model: &str) -> Option<String> {
    super::bad_tokens::model_class_of(model).or_else(|| normalize_class(model))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthFile {
    version: u32,
    #[serde(default)]
    accounts: Vec<AccountHealth>,
    #[serde(default)]
    cursors: HashMap<String, u64>,
}

impl Default for HealthFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            accounts: Vec::new(),
            cursors: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCapacity {
    pub provider: AccountProvider,
    pub raw: usize,
    pub enabled: usize,
    pub healthy: usize,
    pub cooldown: usize,
    pub reauth_required: usize,
    pub observed_at: u64,
}

#[derive(Debug, Clone)]
pub struct NoHealthyAccountError {
    pub provider: AccountProvider,
    pub reasons: Vec<String>,
}

impl std::fmt::Display for NoHealthyAccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no healthy {:?} account is available: {}",
            self.provider,
            self.reasons.join(", ")
        )
    }
}

impl std::error::Error for NoHealthyAccountError {}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("account-health.json")
}

fn lock_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("account-health.lock")
}

fn read_state(workspace: &Path) -> Result<HealthFile> {
    let path = state_path(workspace);
    if !path.exists() {
        return Ok(HealthFile::default());
    }
    let state: HealthFile = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("malformed provider health state {}", path.display()))?;
    if state.version != SCHEMA_VERSION {
        bail!(
            "unsupported provider health schema version {} in {}",
            state.version,
            path.display()
        );
    }
    let mut ids = HashSet::new();
    for account in &state.accounts {
        if account.name.is_empty() || !ids.insert(account.id()) {
            bail!("invalid or duplicate account in provider health state");
        }
    }
    Ok(state)
}

fn write_state(workspace: &Path, state: &HealthFile) -> Result<()> {
    let path = state_path(workspace);
    let parent = path.parent().expect("health path has parent");
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".account-health.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state)?;
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

fn with_state<T>(
    workspace: &Path,
    operation: impl FnOnce(&mut HealthFile) -> Result<T>,
) -> Result<T> {
    fs::create_dir_all(workspace.join(".loom"))?;
    let _lock = MkdirLock::acquire(&lock_path(workspace)).map_err(anyhow::Error::msg)?;
    let mut state = read_state(workspace)?;
    let result = operation(&mut state)?;
    write_state(workspace, &state)?;
    Ok(result)
}

fn cooldown_from_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub fn record_terminal(
    workspace: &Path,
    id: &AccountId,
    classification: TerminalClassification,
    provenance: &str,
) -> Result<()> {
    record_terminal_at(workspace, id, classification, provenance, now_epoch())
}

/// Record terminal feedback with no model information: every hold it writes is
/// account-wide, exactly as before #8058 Phase 2.
pub fn record_terminal_at(
    workspace: &Path,
    id: &AccountId,
    classification: TerminalClassification,
    provenance: &str,
    now: u64,
) -> Result<()> {
    record_terminal_for_class_at(workspace, id, classification, None, provenance, now)
}

/// [`record_terminal_at`], told which **model** was in flight (#8058 Phase 2).
///
/// `model` is a raw alias or pinned ID — whatever the adapter reported —
/// resolved here through [`model_class_of`], so classification happens in
/// exactly one place. `None`, an empty value, or a model that normalizes to no
/// class all reproduce [`record_terminal_at`] verbatim.
///
/// Only `MODEL_CREDITS_EXHAUSTED` uses the class: every other category is an
/// account-level fact (the credential is dead, the plan is out, the session
/// cap is hit) and stays account-wide whatever model provoked it. The one
/// other class-aware arm is `SUCCESS`, which clears the named class's hold —
/// direct evidence that that class works again.
pub fn record_terminal_for_model_at(
    workspace: &Path,
    id: &AccountId,
    classification: TerminalClassification,
    model: Option<&str>,
    provenance: &str,
    now: u64,
) -> Result<()> {
    let class = model.and_then(model_class_of);
    record_terminal_for_class_at(workspace, id, classification, class.as_deref(), provenance, now)
}

/// [`record_terminal_for_model_at`] with an **already-normalized** class
/// (see [`model_class_of`]). Callers holding a raw model string should use
/// [`record_terminal_for_model_at`] so the classification happens once.
pub fn record_terminal_for_class_at(
    workspace: &Path,
    id: &AccountId,
    classification: TerminalClassification,
    model_class: Option<&str>,
    provenance: &str,
    now: u64,
) -> Result<()> {
    if id.name.is_empty() || provenance.is_empty() {
        bail!("account identity and signal provenance are required");
    }
    let model_class = model_class.and_then(normalize_class);
    with_state(workspace, |state| {
        let existing = state.accounts.iter().position(|entry| entry.id() == *id);
        if matches!(
            classification,
            TerminalClassification::Timeout
                | TerminalClassification::Fatal
                | TerminalClassification::CwdDeleted
                | TerminalClassification::ModelRefusal
        ) {
            return Ok(());
        }
        let entry = existing.map_or_else(
            || AccountHealth {
                provider: id.provider,
                name: id.name.clone(),
                reason: HealthReason::Healthy,
                updated_at: now,
                signal_provenance: provenance.to_string(),
                cooldown_until: None,
                consecutive_transient_failures: 0,
                last_success: None,
                last_probe: None,
                class_cooldowns: HashMap::new(),
            },
            |index| state.accounts.remove(index),
        );
        let mut entry = entry;
        entry.updated_at = now;
        entry.signal_provenance = provenance.to_string();
        match classification {
            // Once authentication has expired, ordinary runtime feedback cannot
            // verify that credentials were repaired. Keep the hold sticky until
            // clear_reauth() performs that explicit transition.
            _ if entry.reason == HealthReason::ReauthRequired => {
                entry.cooldown_until = None;
                if classification == TerminalClassification::Success {
                    entry.last_success = Some(now);
                    entry.consecutive_transient_failures = 0;
                }
            }
            TerminalClassification::Success => {
                entry.last_success = Some(now);
                entry.consecutive_transient_failures = 0;
                entry.reason = HealthReason::Healthy;
                entry.cooldown_until = None;
                // A success on a named class proves that class recovered, and
                // says nothing about any other. A class-less success is the
                // account-wide statement it has always been, so it clears
                // every hold — which is exactly what it did before this phase,
                // when a credit exhaustion WAS the account-wide cooldown it
                // cleared. Never leaves the account more blocked than before.
                match model_class.as_deref() {
                    Some(class) => {
                        entry.class_cooldowns.remove(class);
                    }
                    None => entry.class_cooldowns.clear(),
                }
            }
            TerminalClassification::TokenExpired => {
                entry.reason = HealthReason::ReauthRequired;
                entry.cooldown_until = None;
            }
            TerminalClassification::TokenExhausted => {
                entry.reason = HealthReason::PlanExhausted;
                entry.cooldown_until = Some(now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_EXHAUSTED_COOLDOWN_SECS",
                    DEFAULT_EXHAUSTED_COOLDOWN_SECS,
                )));
            }
            // #8058 Phase 2: credits are scoped to one model class, so this
            // records a hold for that class alone and leaves the account
            // serving every other one. #5687 fused this with the account-wide
            // TOKEN_EXHAUSTED arm above because the pool had no per-model
            // state to narrow into; `class_cooldowns` is that state.
            //
            // With no class the arms stay fused, and deliberately so: "which
            // class ran out" is the whole content of a class-scoped mark, and
            // guessing it would block a class that still had credit. Unknown
            // model ⇒ the account-wide over-approximation, as before.
            TerminalClassification::ModelCreditsExhausted => {
                let deadline = now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_EXHAUSTED_COOLDOWN_SECS",
                    DEFAULT_EXHAUSTED_COOLDOWN_SECS,
                ));
                match model_class.clone() {
                    Some(class) => {
                        entry.class_cooldowns.insert(class, deadline);
                        // The account-wide summary only moves when there is no
                        // wider live hold to preserve: a running
                        // PlanExhausted / TransientFailure / SessionLimit
                        // cooldown is the one that decides selection, and a
                        // narrower fact must never overwrite it.
                        if !entry.cooldown_until.is_some_and(|until| until > now) {
                            entry.reason = HealthReason::ModelCreditsExhausted;
                            entry.cooldown_until = None;
                        }
                    }
                    None => {
                        entry.reason = HealthReason::PlanExhausted;
                        entry.cooldown_until = Some(deadline);
                    }
                }
            }
            TerminalClassification::Recoverable => {
                entry.reason = HealthReason::TransientFailure;
                entry.consecutive_transient_failures =
                    entry.consecutive_transient_failures.saturating_add(1);
                entry.cooldown_until = Some(now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_RECOVERABLE_BACKOFF_SECS",
                    DEFAULT_RECOVERABLE_BACKOFF_SECS,
                )));
            }
            TerminalClassification::SessionLimit => {
                entry.reason = HealthReason::SessionLimit;
                entry.cooldown_until = Some(now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_SESSION_BACKOFF_SECS",
                    DEFAULT_SESSION_BACKOFF_SECS,
                )));
            }
            TerminalClassification::Timeout
            | TerminalClassification::Fatal
            | TerminalClassification::CwdDeleted
            | TerminalClassification::ModelRefusal => unreachable!(),
        }
        state.accounts.push(entry);
        state
            .accounts
            .sort_by(|a, b| (a.provider as u8, &a.name).cmp(&(b.provider as u8, &b.name)));
        Ok(())
    })
}

/// Conclusive result of a *proactive* auth-state probe (issue #6927). Only
/// the two states a probe can actually establish are representable: an
/// inconclusive probe (container down, CLI missing, timeout, unparseable
/// output) must never reach this API, because "we could not tell" is not
/// evidence of either health or expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    LoggedIn,
    NotLoggedIn,
}

/// What [`record_probe_at`] actually changed, so a caller can report it
/// without re-reading the state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeEffect {
    /// The account was newly excluded from selection ([`HealthReason::ReauthRequired`]).
    MarkedReauthRequired,
    /// A pre-existing reauth hold was released — the healthy probe is the
    /// independent verification [`clear_reauth`] demands of its callers.
    ClearedReauthHold,
    /// Health already agreed with the probe; only the probe stamp moved.
    Unchanged,
}

/// Apply a proactive auth-state probe result to an account's health record.
///
/// This is the pre-dispatch sibling of [`record_terminal_at`]: it drives the
/// *same* [`HealthReason::ReauthRequired`] exclusion [`select_healthy_at`]
/// already honours, but from evidence gathered before any work is dispatched
/// rather than from a dispatch that already failed. It is deliberately a
/// separate entry point rather than another [`TerminalClassification`] arm —
/// a probe result is not a terminal runtime outcome, must not touch
/// `last_success`, transient-failure counters, or any cooldown, and (unlike
/// ordinary runtime feedback, which cannot verify repaired credentials) a
/// healthy probe IS the independent verification a hold release requires.
pub fn record_probe_at(
    workspace: &Path,
    id: &AccountId,
    outcome: ProbeOutcome,
    provenance: &str,
    now: u64,
) -> Result<ProbeEffect> {
    if id.name.is_empty() || provenance.is_empty() {
        bail!("account identity and signal provenance are required");
    }
    with_state(workspace, |state| {
        let index = state.accounts.iter().position(|entry| entry.id() == *id);
        let mut entry = index.map_or_else(
            || AccountHealth {
                provider: id.provider,
                name: id.name.clone(),
                reason: HealthReason::Healthy,
                updated_at: now,
                signal_provenance: provenance.to_string(),
                cooldown_until: None,
                consecutive_transient_failures: 0,
                last_success: None,
                last_probe: None,
                class_cooldowns: HashMap::new(),
            },
            |index| state.accounts.remove(index),
        );
        entry.last_probe = Some(now);
        let effect = match outcome {
            ProbeOutcome::NotLoggedIn if entry.reason != HealthReason::ReauthRequired => {
                entry.reason = HealthReason::ReauthRequired;
                // Same shape as the reactive TOKEN_EXPIRED arm: a reauth hold
                // is sticky, never a timed cooldown, so it cannot lapse back
                // into selection just because time passed.
                entry.cooldown_until = None;
                entry.updated_at = now;
                entry.signal_provenance = provenance.to_string();
                ProbeEffect::MarkedReauthRequired
            }
            ProbeOutcome::LoggedIn if entry.reason == HealthReason::ReauthRequired => {
                entry.reason = HealthReason::Healthy;
                entry.cooldown_until = None;
                entry.consecutive_transient_failures = 0;
                entry.updated_at = now;
                entry.signal_provenance = provenance.to_string();
                ProbeEffect::ClearedReauthHold
            }
            // A healthy probe says nothing about an exhaustion cooldown, a
            // per-class credit hold, or a transient-failure backoff, so it
            // deliberately leaves all three alone — it only ever releases an
            // auth hold.
            ProbeOutcome::LoggedIn | ProbeOutcome::NotLoggedIn => ProbeEffect::Unchanged,
        };
        state.accounts.push(entry);
        state
            .accounts
            .sort_by(|a, b| (a.provider as u8, &a.name).cmp(&(b.provider as u8, &b.name)));
        Ok(effect)
    })
}

/// Clear an auth hold only after the caller has independently verified reauth.
pub fn clear_reauth(workspace: &Path, id: &AccountId, provenance: &str) -> Result<()> {
    with_state(workspace, |state| {
        let entry = state
            .accounts
            .iter_mut()
            .find(|entry| entry.id() == *id)
            .ok_or_else(|| anyhow!("no health record for {:?}/{}", id.provider, id.name))?;
        if entry.reason != HealthReason::ReauthRequired {
            bail!("account is not awaiting reauthentication");
        }
        entry.reason = HealthReason::Healthy;
        entry.cooldown_until = None;
        entry.consecutive_transient_failures = 0;
        entry.updated_at = now_epoch();
        entry.signal_provenance = provenance.to_string();
        Ok(())
    })
}

pub fn account_health(workspace: &Path, id: &AccountId) -> Result<Option<AccountHealth>> {
    Ok(read_state(workspace)?
        .accounts
        .into_iter()
        .find(|entry| entry.id() == *id))
}

/// Select a healthy account for `provider` — the account-wide question, whose
/// meaning is unchanged by #8058 Phase 2 (a live class-scoped hold still
/// excludes the account). Callers that know which model they are about to run
/// should use [`select_healthy_for_model_at`] instead.
pub fn select_healthy_at(
    workspace: &Path,
    provider: AccountProvider,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Result<AccountDescriptor> {
    select_healthy_for_class_at(workspace, provider, inventory, None, now)
}

/// [`select_healthy_at`], narrowed to the class the **model** belongs to
/// (#8058 Phase 2).
///
/// `model` is a raw alias or pinned ID, resolved through [`model_class_of`].
/// `None`, an empty value, or a model that normalizes to no class all
/// reproduce [`select_healthy_at`] exactly — selection must never fail closed
/// on a model name it does not recognize.
pub fn select_healthy_for_model_at(
    workspace: &Path,
    provider: AccountProvider,
    inventory: &[AccountDescriptor],
    model: Option<&str>,
    now: u64,
) -> Result<AccountDescriptor> {
    let class = model.and_then(model_class_of);
    select_healthy_for_class_at(workspace, provider, inventory, class.as_deref(), now)
}

/// [`select_healthy_for_model_at`] with an **already-normalized** class (see
/// [`model_class_of`]).
///
/// This is the single scan both the class-aware and the account-wide readers
/// share, so the two can never drift — the same shape Phase 1 chose for
/// `bad_tokens::blocking_entry_in_dir_for_class`.
pub fn select_healthy_for_class_at(
    workspace: &Path,
    provider: AccountProvider,
    inventory: &[AccountDescriptor],
    model_class: Option<&str>,
    now: u64,
) -> Result<AccountDescriptor> {
    let model_class = model_class.and_then(normalize_class);
    let model_class = model_class.as_deref();
    with_state(workspace, |state| {
        for entry in &mut state.accounts {
            if entry.reason == HealthReason::TransientFailure
                && entry.cooldown_until.is_some_and(|deadline| deadline <= now)
            {
                entry.reason = HealthReason::Healthy;
                entry.cooldown_until = None;
                entry.consecutive_transient_failures = 0;
                entry.updated_at = now;
            }
            // Expired class holds are dropped on sight so `class_cooldowns`
            // cannot grow without bound, and the account-wide summary follows
            // them out: once the last class hold has aged away there is
            // nothing left for `ModelCreditsExhausted` to describe.
            let expired = entry.class_cooldowns.len();
            entry.class_cooldowns.retain(|_, deadline| *deadline > now);
            if entry.class_cooldowns.len() != expired
                && entry.class_cooldowns.is_empty()
                && entry.reason == HealthReason::ModelCreditsExhausted
            {
                entry.reason = HealthReason::Healthy;
                entry.updated_at = now;
            }
        }
        let health: HashMap<AccountId, AccountHealth> = state
            .accounts
            .iter()
            .cloned()
            .map(|entry| (entry.id(), entry))
            .collect();
        let mut candidates: Vec<_> = inventory
            .iter()
            .filter(|account| account.id.provider == provider && account.enabled)
            .filter(|account| {
                health
                    .get(&account.id)
                    .is_none_or(|entry| entry.is_eligible_for_class_at(now, model_class))
            })
            .cloned()
            .collect();
        candidates.sort_by_key(|account| {
            (
                health
                    .get(&account.id)
                    .map_or(0, |entry| entry.consecutive_transient_failures),
                account.id.name.clone(),
            )
        });
        if candidates.is_empty() {
            let reasons = inventory
                .iter()
                .filter(|account| account.id.provider == provider)
                .map(|account| {
                    let reason = if !account.enabled {
                        "disabled".to_string()
                    } else if let Some(entry) = health.get(&account.id) {
                        match entry.reason {
                            HealthReason::ReauthRequired => "reauth_required".to_string(),
                            // An account-wide cooldown is reported exactly as
                            // before; only when there is none does a live
                            // class hold get named, so the operator can see
                            // *which* class is out rather than a bare
                            // "unavailable" (#8058 Phase 2).
                            _ => match entry.cooldown_until {
                                Some(until) => format!("cooldown_until={until}"),
                                None => entry
                                    .blocking_class_cooldown_at(now, model_class)
                                    .map_or_else(
                                        || "unavailable".to_string(),
                                        |(class, until)| {
                                            format!("model_class={class} cooldown_until={until}")
                                        },
                                    ),
                            },
                        }
                    } else {
                        "unavailable".to_string()
                    };
                    format!("{}={reason}", account.id.name)
                })
                .collect();
            return Err(NoHealthyAccountError { provider, reasons }.into());
        }
        let fewest = health
            .get(&candidates[0].id)
            .map_or(0, |entry| entry.consecutive_transient_failures);
        candidates.retain(|account| {
            health
                .get(&account.id)
                .map_or(0, |entry| entry.consecutive_transient_failures)
                == fewest
        });
        let cursor_key = format!("{provider:?}").to_ascii_lowercase();
        let cursor = state.cursors.entry(cursor_key).or_default();
        let chosen = candidates[*cursor as usize % candidates.len()].clone();
        *cursor = cursor.saturating_add(1);
        Ok(chosen)
    })
}

pub fn provider_capacity_at(
    workspace: &Path,
    provider: AccountProvider,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Result<ProviderCapacity> {
    let state = read_state(workspace)?;
    let health: HashMap<_, _> = state
        .accounts
        .into_iter()
        .map(|entry| (entry.id(), entry))
        .collect();
    let accounts: Vec<_> = inventory
        .iter()
        .filter(|account| account.id.provider == provider)
        .collect();
    let enabled: Vec<_> = accounts.iter().filter(|account| account.enabled).collect();
    let reauth_required = enabled
        .iter()
        .filter(|account| {
            health
                .get(&account.id)
                .is_some_and(|entry| entry.reason == HealthReason::ReauthRequired)
        })
        .count();
    // Capacity answers the account-wide question, so a live class-scoped hold
    // counts here too (#8058 Phase 2) — `select_healthy_at` excludes the
    // account for exactly that reason, and a capacity report that disagreed
    // with selection would be the dishonest kind.
    let cooldown = enabled
        .iter()
        .filter(|account| {
            health.get(&account.id).is_some_and(|entry| {
                entry.reason != HealthReason::ReauthRequired
                    && (entry.cooldown_until.is_some_and(|until| until > now)
                        || entry.blocking_class_cooldown_at(now, None).is_some())
            })
        })
        .count();
    Ok(ProviderCapacity {
        provider,
        raw: accounts.len(),
        enabled: enabled.len(),
        healthy: enabled.len().saturating_sub(reauth_required + cooldown),
        cooldown,
        reauth_required,
        observed_at: now,
    })
}

#[cfg(test)]
mod tests;
