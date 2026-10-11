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
//! | `pi`, `opencode`, `kimi` | [`CredentialSource::ApiKeys`] or [`CredentialSource::Unobservable`] | the profile's credential ladder (#8401/#8428) |
//! | anything else (tier-3 generic) | [`CredentialSource::Binary`] or [`CredentialSource::Unobservable`] | the CLI binary on the daemon's `PATH`, when its name is known (#9368) |
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
//! - A profile with **no remote credential** (a local, unauthenticated
//!   provider) needs nothing the daemon could count.
//! - A Codex tap whose adapter would select from a *different* account
//!   provider, or whose credential is pinned by environment, does not consult
//!   the codex pool — so a dry codex pool is not its wall.
//!
//! - A tier-3 generic runtime whose **binary name the daemon cannot learn**
//!   (no manifest `launch.cliBin`, no `LOOM_GENERIC_CLI_BIN`), or whose
//!   lookup it cannot reproduce (no `PATH`, a relative `PATH` entry, a
//!   relative binary path), is not provably uninstalled. Only a *known* name
//!   that resolves nowhere is passed over — see [`generic`].
//!
//! Two cases are deliberately fail-closed. An **unreadable** pool: accounts
//! may be registered and deliberately disabled to stop spend, and
//! `worker_spawn::credential::resolve` refuses (exit 78) rather than launch on
//! the ambient credential. And an **unprovisioned** credential (#11400): a
//! native tap whose single remote credential variable is unset, whose provider
//! has no API-key pool on this host, with no `LOOM_NATIVE_AUTH_FILE` and no LLM
//! gateway route. Every preference walk is for a role, and role launches are
//! guarded: they redirect `HOME`/`XDG_*` and never reuse the harness's own
//! login (`guardrail-parity-native.md`; `worker_spawn::harness` only prints a
//! hint), so such a launch is certain to fail at the provider. A tap that
//! would fail at spawn must be passed over here, not selected and then killed.

use super::resolve::{SkipReason, Tap};
use crate::role_runner::{PoolHold, PoolStateFile};
use crate::runtime_admission::ResolvedRuntime;
use std::ffi::{OsStr, OsString};
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
    /// A tier-3 generic runtime's CLI binary (#9368). Not a credential pool:
    /// it names the one thing such a tap can be observed to lack — the
    /// executable `spawn-generic.sh` would exec. Only ever reported on an
    /// [`Availability::Exhausted`]; a binary that resolves (or cannot be
    /// checked) stays [`Self::Unobservable`], as before.
    Binary,
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
            Self::Binary => "binary".to_string(),
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
    availability_in(root, tap, admitted, now, &BinaryEnv::from_process())
}

/// [`availability`] with the tier-3 binary gate's two environment reads
/// injected, so a test decides what is "installed" without touching the
/// process environment (or depending on what this host happens to have).
#[must_use]
pub fn availability_in(
    root: &Path,
    tap: &Tap,
    admitted: &ResolvedRuntime,
    now: u64,
    binary_env: &BinaryEnv,
) -> Availability {
    match admitted.runtime.as_str() {
        "claude" => claude(root),
        "codex" => codex(root, admitted, now),
        runtime if crate::worker_spawn::is_native(runtime) => native(root, tap, runtime),
        runtime => generic(root, runtime, admitted, binary_env),
    }
}

/// The two environment values the tier-3 binary gate reads (#9368).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinaryEnv {
    /// `LOOM_GENERIC_CLI_BIN`: an operator's pin of the CLI binary, which
    /// wins over the manifest's `launch.cliBin` at launch and so here too.
    pub cli_bin: Option<OsString>,
    /// The `PATH` a spawned adapter inherits. `None` when unset.
    pub path: Option<OsString>,
}

impl BinaryEnv {
    /// The daemon's own environment — what a launch from this process would
    /// inherit. (`worker_spawn` only ever *prepends* `gh` front directories
    /// to it, which hold no runtime CLI.)
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            cli_bin: std::env::var_os(crate::runtime_launch::CLI_BIN_ENV),
            path: std::env::var_os("PATH"),
        }
    }
}

/// A tier-3 generic-passthrough tap (`gemini`, `aider`, operator-defined),
/// gated on the one thing the daemon can observe about it: whether the CLI
/// binary its adapter would exec exists (#9368).
///
/// Such a runtime has no credential pool, so before this gate nothing could
/// ever make it skip: with `["claude", "gemini"]` and a dry Claude pool the
/// walk chose an uninstalled `gemini`, and `spawn-generic.sh` exited 127 on
/// every tick instead of the walk falling through or holding cleanly.
///
/// Fail-open on everything unobservable, per the module's one rule — the tap
/// is passed over **only** when a known binary name provably does not
/// resolve. The hold is [`PoolHold::Unprovisioned`]: no amount of waiting
/// installs a CLI, an operator must.
///
/// Deliberately the catch-all arm only. `claude`, `codex` and the native
/// harnesses launch through their own adapters and install locations (some
/// under `~/.loom/opt/`), so a `PATH` miss proves nothing about them.
fn generic(
    root: &Path,
    runtime: &str,
    admitted: &ResolvedRuntime,
    env: &BinaryEnv,
) -> Availability {
    // A contained launch (#8787) execs inside its own environment; the
    // host's PATH says nothing about what is installed there.
    if admitted.execution.is_some() {
        return ungated(CredentialSource::Unobservable);
    }
    let Some(bin) = crate::runtime_launch::cli_bin_with(root, runtime, env.cli_bin.as_deref())
    else {
        return ungated(CredentialSource::Unobservable);
    };
    if binary_resolves(&bin, env.path.as_deref()) != Some(false) {
        return ungated(CredentialSource::Unobservable);
    }
    let detail = if Path::new(&bin).is_absolute() {
        format!("'{bin}' is not an executable file")
    } else {
        format!("'{bin}' not found in PATH")
    };
    Availability::Exhausted {
        source: CredentialSource::Binary,
        hold: PoolHold::Unprovisioned,
        detail: format!(
            "{detail} — install the CLI or drop tier-3 runtime {runtime} from the preference \
             list; #9368"
        ),
    }
}

/// Whether `bin` resolves the way `spawn-generic.sh`'s `command -v` will:
/// a bare name is an executable file in some `PATH` directory, an absolute
/// path is checked as-is.
///
/// `None` is "cannot tell": no `PATH`, or a lookup that depends on the
/// spawned process's working directory (a relative `bin` with a directory
/// component, or a relative/empty `PATH` entry that did not settle it
/// earlier). The daemon's cwd is not the worker's, so those are never
/// reported as a miss.
fn binary_resolves(bin: &str, path: Option<&OsStr>) -> Option<bool> {
    let candidate = Path::new(bin);
    if candidate.is_absolute() {
        return Some(is_executable_file(candidate));
    }
    if candidate.components().count() != 1 {
        return None;
    }
    let mut cwd_dependent = false;
    for dir in std::env::split_paths(path?) {
        if !dir.is_absolute() {
            cwd_dependent = true;
        } else if is_executable_file(&dir.join(candidate)) {
            return Some(true);
        }
    }
    (!cwd_dependent).then_some(false)
}

/// An existing regular file (symlinks followed) with any execute bit set.
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
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
///
/// **Sandbox hold (#10003).** A healthy pool is not enough: a Codex tick
/// whose sandbox refused every tool call arms a short host-wide hold
/// ([`super::sandbox_hold`]), and while it is live this tap is unavailable
/// whatever the pool says. That is what lets `rolePreference:
/// ["codex","claude"]` fall through to Claude when Codex can run nothing.
/// The hold is self-healing (`PoolHold::SelfHealing`): it ages out and the next
/// tick re-tests the sandbox.
///
/// **Session down (#10454).** A session-managed account whose container is not
/// running is not spawnable (spawn-codex.sh refuses it), so a pool of only
/// such accounts is Exhausted with a detail naming `SessionDown`. The read is
/// the session watch's published snapshot (#10660), never a `docker` process;
/// with no fresh snapshot, or Docker unqueryable, the count stays at zero —
/// cannot observe, not down.
fn codex(root: &Path, admitted: &ResolvedRuntime, now: u64) -> Availability {
    let source = CredentialSource::CodexAccounts;
    if let Some(hold) = super::sandbox_hold::active("codex", now) {
        return Availability::Exhausted {
            source,
            hold: PoolHold::SelfHealing,
            detail: hold.describe("codex", now),
        };
    }
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
    // A merging role may draw a Codex seat only when every seat it could draw
    // runs Loom's guard hook (#9390 follow-up; see `codex_guard`). Otherwise
    // the walk falls through to the next tap instead of selecting Codex and
    // failing the tick in spawn-codex.sh's own fail-closed check.
    if state.spawnable > 0 && super::codex_guard::guarded(&admitted.role) {
        let unready = super::codex_guard::unready_profiles(root);
        if unready.as_ref().map_or(true, |names| !names.is_empty()) {
            return Availability::Exhausted {
                source,
                hold: PoolHold::Unprovisioned,
                detail: match unready {
                    Ok(names) => format!(
                        "Loom's guard hook is not ready on Codex profile(s) {} for this \
                         merging role ({}) — provision-codex-hooks.sh verify --all-profiles \
                         --workspace <repo>; #9390",
                        names.join(", "),
                        admitted.role
                    ),
                    Err(reason) => format!("{reason} ({}) — #9390", admitted.role),
                },
            };
        }
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
                "no enabled codex account is provisioned".to_string()
            } else {
                crate::role_runner::runtime_preflight::codex_exhausted_reason(&state)
            }
        ),
    }
}

/// A native harness tap (`pi`, `opencode`, `kimi`), gated on the API-key account pool
/// **only where `worker_spawn::credential::resolve`'s ladder would actually
/// consult it** — see that module for the ladder this mirrors step for step:
///
/// 1. Every source variable already exported ⇒ the pool is never consulted.
/// 2. Exactly one unset variable with a derivable pool provider that is
///    *pooled on this host* ⇒ the pool is the wall; count it.
/// 3. A single unset variable whose provider is *not* pooled here ⇒ the guarded
///    launch has no credential unless `LOOM_NATIVE_AUTH_FILE` supplies one, so
///    it is [`PoolHold::Unprovisioned`] (#11400); with the auth file set it is
///    unobservable (the snapshot's account health is not countable).
/// 4. Anything else (unresolvable profile, no pool provider, gateway route) ⇒
///    unobservable, and a real failure surfaces loudly at launch.
///
/// An unreadable pool directory is fail-closed too, matching the ladder's
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
    // #9473: a profile routed through the LLM gateway never consults the pool
    // (the gateway's virtual key replaces it), so the pool is not its wall. A
    // routed profile that cannot launch fails loudly at launch, as above.
    if !matches!(crate::worker_spawn::llm_gateway::plan(runtime, &selection, &config), Ok(None)) {
        return ungated(CredentialSource::Unobservable);
    }
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
    let Some(provider) = crate::worker_spawn::credential::pool_provider(
        selection.credential_pool.as_deref(),
        source_var,
    ) else {
        return ungated(CredentialSource::Unobservable);
    };
    let source = CredentialSource::ApiKeys {
        provider: provider.clone(),
    };
    match crate::api_keys_pool::is_pooled(root, &provider) {
        // Not pooled on this host. A guarded role launch never reuses the
        // harness's own login, so only an external auth snapshot can supply
        // the credential (#11400). Kimi refuses that snapshot at provision
        // time, so it never counts there.
        Ok(false) if runtime != "kimi" && native_auth_file_set() => {
            return ungated(CredentialSource::Unobservable)
        }
        Ok(false) => {
            return Availability::Exhausted {
                source,
                hold: PoolHold::Unprovisioned,
                detail: format!(
                    "{source_var} is unset and provider {provider:?} has no API-key pool on \
                     this host (no LOOM_NATIVE_AUTH_FILE, no LLM gateway route); a guarded \
                     launch would have no credential — loom-daemon api-keys add {provider} <account>; #11400"
                ),
            }
        }
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

/// Whether an external native auth snapshot is configured — the one way a
/// guarded launch can authenticate without an env or pooled credential
/// (`native_tools::provision::state::prepare` reads the same variable).
fn native_auth_file_set() -> bool {
    std::env::var_os("LOOM_NATIVE_AUTH_FILE").is_some_and(|value| !value.is_empty())
}
