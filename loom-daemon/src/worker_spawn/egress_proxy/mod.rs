//! Keep the real credential OUT of the worker container (issue #8674, epic
//! #6896).
//!
//! Without this module a contained native-harness launch forwards the real
//! credential into the container's environment (`-e VAR`, by name — see
//! [`super::containment`]). That keeps it out of argv and off the container's
//! filesystem, but it is still readable by anything running inside: `env`,
//! `/proc/self/environ`, a shell hook. Forge text is untrusted input by this
//! repo's own policy (`defaults/docs/untrusted-external-content.md`), and the
//! credential is the highest-value thing that input can reach.
//!
//! With this module the container gets a **per-launch placeholder** instead,
//! and provider traffic is pointed at a host-side listener that swaps the
//! placeholder for the real credential on the way out. The real value never
//! crosses the container boundary in any form.
//!
//! ```text
//!   worker (in container)                 host                       provider
//!   ─────────────────────                 ────                       ────────
//!   ANTHROPIC_AUTH_TOKEN=loom-ph-…  ──▶  registry lookup   ──▶  Authorization:
//!   ANTHROPIC_BASE_URL=http://…:PORT      pin check              Bearer <real>
//!                                         header swap
//! ```
//!
//! # Fail closed, never fall back
//!
//! Every failure in here is an error, never a silent return to passing the
//! real credential through: a proxy that quietly degrades to env-passthrough
//! is indistinguishable from one that works, which is precisely the mistake
//! this issue's own acceptance criteria call out. The *opt-out* is explicit
//! and lives one level up — a profile that declares no `credentialProxy` block
//! stays env-passthrough exactly as before, and the whole mechanism is off
//! until `runtimes.containment.credentialProxy` says otherwise.
//!
//! # Three entry points, one mechanism
//!
//! - The **native-harness** contained dispatch ([`super::containment`], #8674)
//!   calls [`prepare`], which resolves the real credential through the #8401
//!   ladder and hands the placeholder to `docker run` as an assignment.
//! - **Claude's** per-sweep container is built by the `spawn-claude.sh` shell
//!   adapter, which has nowhere to host a listener. It shells out to
//!   `loom-daemon worker proxy-exec` ([`exec`], #8697), which takes the
//!   credential the adapter already selected, swaps it for a placeholder in the
//!   environment of the `docker run` it launches, and serves the proxy for
//!   exactly that launch's lifetime.
//! - An **uncontained native** launch ([`prepare_native`], #11300 slice 3)
//!   arms the same mechanism bound loopback only — the harness runs on this
//!   host — and stays alive as the harness's parent so the placeholder's
//!   lifetime stays exactly the launch's, like the contained path.
//!
//! All three go through [`arm`] and [`run_with_proxy`], so they share one
//! registry shape, one placeholder format and one set of refusals.
//!
//! # Account rotation (#8818)
//!
//! Only the [`exec`] path can rotate: it knows the launch's pool account and
//! workspace, so it installs a [`rotation::HostPool`] and the container asks
//! for a swap through [`rotation::ROTATE_PATH`] (client:
//! `loom-daemon worker proxy-rotate`, [`rotate_client`]). The native path
//! installs nothing, and every rotation request there is refused.

pub mod exec;
#[cfg(test)]
#[path = "native_tests.rs"]
mod native_tests;
pub mod observe;
pub mod registry;
pub mod rotate_client;
pub mod rotation;
pub mod server;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use super::{containment, credential, profiles::Selection, LaunchError};
use crate::api_keys_pool::bad_marks;
pub use registry::{HeaderStyle, Record, Refusal, Registry, Upstream};

use serde_json::Value;
use std::net::IpAddr;
use std::process::Command;

/// The opaque per-launch value handed to the container in place of the real
/// credential.
///
/// Two independent v4 UUIDs' worth of CSPRNG output (244 bits), rendered with a
/// fixed, greppable prefix so an operator who finds one in a log or a container
/// can tell at a glance that it is a Loom placeholder and not a live key.
#[derive(Clone, PartialEq, Eq)]
pub struct Placeholder(String);

/// Prefix every placeholder carries. Public so a test — or an operator
/// grepping a container — can assert on it.
pub const PLACEHOLDER_PREFIX: &str = "loom-placeholder-";

impl Placeholder {
    #[must_use]
    pub fn generate() -> Self {
        Self(format!(
            "{PLACEHOLDER_PREFIX}{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Redacted: a placeholder is a live bearer token for as long as its launch is
/// open, so it is treated exactly like the credential it stands for.
impl std::fmt::Debug for Placeholder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Placeholder(<redacted>)")
    }
}

/// A profile's `credentialProxy` block: which upstream this profile's
/// credential may reach, how it is presented there, and which environment
/// variables inside the container must be re-pointed at the proxy.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileProxy {
    /// The ONE origin this profile's credential may be sent to.
    pub upstream: String,
    /// How the real credential is presented upstream.
    #[serde(default)]
    pub header: HeaderStyle,
    /// Environment variables set inside the container to the proxy's base URL
    /// (`ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`, …). A profile whose harness
    /// reads its base URL from injected *configuration* rather than the
    /// environment cannot be proxied by this slice; see the follow-ups.
    #[serde(default)]
    pub base_url_env: Vec<String>,
    /// Opt this profile in to passive per-request telemetry (#11300). Takes
    /// effect only when [`observe::enabled`] is also true; default off.
    #[serde(default)]
    pub observe: bool,
    /// The provider-config option key that receives the proxy base URL when
    /// the harness reads its provider base URL from injected *configuration*
    /// rather than the environment (issue #11300, slice 3). OpenCode is that
    /// harness: its `provider.<id>.options.baseURL` rides the launch's
    /// provisioned config (`OPENCODE_CONFIG_CONTENT`), so the launcher patches
    /// that one option to the proxy URL and leaves the environment alone.
    /// Inert on any runtime but `opencode` — dropped from the selection at
    /// resolution, so a profile may declare it for its opencode binding while
    /// other bindings keep launching (#11714); arming a runtime left with no
    /// usable redirection fails closed in [`prepare_scoped`].
    #[serde(default)]
    pub base_url_config: Option<String>,
}
/// Longest accepted provider-config option key.
const MAX_BASE_URL_CONFIG_LEN: usize = 64;

impl ProfileProxy {
    /// Validate without binding anything — called from profile resolution so a
    /// malformed block fails at selection time, not at dispatch time.
    pub fn validate(&self) -> Result<Upstream, LaunchError> {
        let upstream = Upstream::parse(&self.upstream)
            .map_err(|why| LaunchError::config(format!("model profile credentialProxy: {why}")))?;
        for name in &self.base_url_env {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(LaunchError::config(
                    "model profile credentialProxy.baseUrlEnv must contain environment \
                     variable names",
                ));
            }
        }
        if let Some(option) = &self.base_url_config {
            let ok = !option.is_empty()
                && option.len() <= MAX_BASE_URL_CONFIG_LEN
                && option
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
            if !ok {
                return Err(LaunchError::config(
                    "model profile credentialProxy.baseUrlConfig must be a provider-config \
                     option key (e.g. \"baseURL\")",
                ));
            }
        }
        Ok(upstream)
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn truthy(value: &str) -> bool {
    matches!(value.trim(), "1" | "true" | "yes")
}

/// Is credential substitution turned on for this workspace?
///
/// | Precedence | Source |
/// |---|---|
/// | 1 | `LOOM_NATIVE_CREDENTIAL_PROXY` (`1`/`true`/`yes` enables; anything else disables) |
/// | 2 | `.loom/config.json` → `runtimes.containment.credentialProxy` |
/// | 3 | off — the credential is forwarded by name, exactly as before |
///
/// Deliberately a separate switch from `runtimes.containment.native`: turning
/// containment on must not silently change how credentials reach the harness,
/// and a provider whose end-to-end path is not yet verified must be able to
/// keep running contained with plain env-passthrough.
#[must_use]
pub fn enabled(config: &Value) -> bool {
    enabled_with(config, &env_nonempty)
}

/// [`enabled`] with the environment injected, so a caller that already reads
/// env through a seam (the LLM gateway's `plan_with`) asks the same question.
#[must_use]
pub fn enabled_with(config: &Value, env: &dyn Fn(&str) -> Option<String>) -> bool {
    if let Some(raw) = env("LOOM_NATIVE_CREDENTIAL_PROXY").filter(|v| !v.trim().is_empty()) {
        return truthy(&raw);
    }
    match config.pointer("/runtimes/containment/credentialProxy") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => truthy(s),
        _ => false,
    }
}

/// Everything the dispatcher needs once substitution has been decided: the
/// container-side env changes, the bound listener, and the live registry.
pub struct Prepared {
    pub injection: containment::Injection,
    pub launch_id: String,
    /// The proxy's own base URL — what `base_url_env` assignments and the
    /// `base_url_config` config patch both point the harness at.
    base_url: String,
    registry: Registry,
    bound: server::Bound,
    marker: String,
}

impl Prepared {
    /// The proxy base URL the harness is pointed at.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Secret-free one-line record for the per-sweep log, mirroring
    /// [`containment::Profile::dispatch_marker`]'s shape.
    #[must_use]
    pub fn dispatch_marker(&self) -> &str {
        &self.marker
    }

    /// Apply this launch's substitution to an **uncontained native** command
    /// (issue #11300, slice 3): the placeholder replaces the real credential
    /// under every mapped name (overriding the by-name forwarding
    /// [`crate::worker_spawn::harness`] already applied), every
    /// `base_url_env` name is set to the proxy URL, and — when the profile
    /// declared `base_url_config` — the provider-config option named there is
    /// patched to the proxy URL inside the `OPENCODE_CONFIG_CONTENT` the
    /// launch provisions.
    ///
    /// # Errors
    ///
    /// A `OPENCODE_CONFIG_CONTENT` that does not parse as a JSON object fails
    /// closed: that content was written by this launcher's own provisioning,
    /// so a malformed value means a bug or tampering, and silently launching
    /// un-patched would send the placeholder straight at the real provider.
    pub fn apply_native_env(
        &self,
        command: &mut Command,
        provider_id: &str,
        config_option: Option<&str>,
    ) -> Result<(), LaunchError> {
        for (name, value) in &self.injection.assignments {
            command.env(name, value);
        }
        if let Some(option) = config_option {
            patch_opencode_provider_base_url(command, provider_id, option, &self.base_url)?;
        }
        Ok(())
    }
}

/// Patch the OpenCode provider-config option `option` on provider `provider_id`
/// to `base_url`, inside the `OPENCODE_CONFIG_CONTENT` env `command` already
/// carries (`native_tools::provision` wrote it; the guarded path always does,
/// the unguarded path only when the profile declared a provider block).
///
/// A missing or empty value starts a fresh config object — the same shape
/// [`crate::native_tools::provision::merge_provider`] builds — because the
/// only harness that reads this variable is the one being pointed at the
/// proxy. An explicit assignment always wins over an inherited ambient value,
/// so an ambient `OPENCODE_CONFIG_CONTENT` the launcher cannot see through
/// [`std::process::Command::get_env`] is read from the launcher's own
/// environment for the merge and then overridden.
fn patch_opencode_provider_base_url(
    command: &mut Command,
    provider_id: &str,
    option: &str,
    base_url: &str,
) -> Result<(), LaunchError> {
    let explicit = command
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new("OPENCODE_CONFIG_CONTENT"))
        .and_then(|(_, value)| value.map(std::ffi::OsString::from));
    let raw = explicit
        .or_else(|| std::env::var_os("OPENCODE_CONFIG_CONTENT"))
        .filter(|value| !value.is_empty());
    let mut config: Value = match raw {
        Some(raw) => serde_json::from_slice(raw.as_encoded_bytes()).map_err(|error| {
            LaunchError::config(format!(
                "credentialProxy.baseUrlConfig cannot patch OPENCODE_CONFIG_CONTENT: {error}"
            ))
        })?,
        None => serde_json::json!({}),
    };
    let object = config.as_object_mut().ok_or_else(|| {
        LaunchError::config(
            "credentialProxy.baseUrlConfig cannot patch OPENCODE_CONFIG_CONTENT: not a JSON \
             object",
        )
    })?;
    fn at<'a>(
        object: &'a mut serde_json::Map<String, Value>,
        key: &str,
    ) -> &'a mut serde_json::Map<String, Value> {
        let entry = object
            .entry(key.to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !entry.is_object() {
            *entry = serde_json::json!({});
        }
        entry.as_object_mut().expect("object entry")
    }
    let value = at(at(at(object, "provider"), provider_id), "options");
    value.insert(option.to_string(), Value::String(base_url.to_string()));
    command.env("OPENCODE_CONFIG_CONTENT", config.to_string());
    Ok(())
}

/// Hand-written, NOT derived: this struct reaches both the placeholder (inside
/// `registry`) and, through it, the real credential. Only the fields that are
/// already safe to log — the ones [`Self::dispatch_marker`] is built from — are
/// rendered. A `#[derive(Debug)]` here would be one `dbg!` away from printing a
/// live key, which is exactly the exposure this module exists to remove.
impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("launch_id", &self.launch_id)
            .field("marker", &self.marker)
            .field("registry", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Decide whether this contained launch substitutes its credential, and if so
/// resolve the real one on the HOST and bind the listener.
///
/// `Ok(None)` means "env-passthrough, unchanged" and is reached only when the
/// feature is off or the profile declares no `credentialProxy`. Every other
/// outcome is `Err`.
///
/// `runtime` is the launcher's already-resolved runtime (role binding, env,
/// `runtimes.default` or built-in default — see
/// `worker_spawn::resolve_launch_runtime`); observe-mode attribution uses it
/// verbatim rather than rereading `LOOM_RUNTIME`.
pub fn prepare(
    root: &std::path::Path,
    selection: &Selection,
    config: &Value,
    runtime: &str,
) -> Result<Option<Prepared>, LaunchError> {
    prepare_scoped(root, selection, config, runtime, Scope::Contained)
        .map(|prepared| prepared.map(|(prepared, _)| prepared))
}

/// [`prepare`] for an **uncontained native** launch (issue #11300, item 5):
/// the same substitution, bound **loopback only** — the harness runs on this
/// host, so loopback is both reachable and the tightest scope available, and
/// the container-only machinery (bridge-gateway discovery, the api-keys tmpfs
/// mask, `host.docker.internal`) does not apply.
///
/// Returns the [`credential::Resolved`] alongside the [`Prepared`] so the
/// launcher can hand the harness command exactly the credential the proxy
/// holds: unlike the contained path, there is no container re-exec whose
/// profile resolution would re-read the substitution — a second resolution
/// here would run the pool ladder twice and could land on a different account.
///
/// Refused (fail closed) on a non-native runtime that declares a
/// `credentialProxy` block. Selection has already dropped an inert
/// `base_url_config` off a non-opencode runtime (#11714), so the check below
/// only fires for a hand-built selection — defense in depth alongside
/// `prepare_scoped`'s armed-path redirect check.
pub fn prepare_native(
    root: &std::path::Path,
    selection: &Selection,
    config: &Value,
    runtime: &str,
) -> Result<Option<(Prepared, credential::Resolved)>, LaunchError> {
    if !super::is_native(runtime) {
        if selection.credential_proxy.is_some() {
            return Err(LaunchError::config(
                "model profile credentialProxy is only supported on the native runtimes (pi, \
                 opencode, kimi); this launch resolved a legacy adapter",
            ));
        }
        return Ok(None);
    }
    if selection
        .credential_proxy
        .as_ref()
        .is_some_and(|proxy| proxy.base_url_config.is_some())
        && runtime != "opencode"
    {
        return Err(LaunchError::config(
            "model profile credentialProxy.baseUrlConfig is only supported by the opencode \
             harness, whose provider base URL is provisioned through OPENCODE_CONFIG_CONTENT",
        ));
    }
    prepare_scoped(root, selection, config, runtime, Scope::NativeLoopback)
}

/// Where the listener binds and which container-only machinery applies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The launch runs in a container: docker-aware bind, per-repo api-keys
    /// tmpfs mask, `--add-host …:host-gateway` (#8674).
    Contained,
    /// The launch runs on this host: loopback bind, nothing container-side.
    NativeLoopback,
}

fn prepare_scoped(
    root: &std::path::Path,
    selection: &Selection,
    config: &Value,
    runtime: &str,
    scope: Scope,
) -> Result<Option<(Prepared, credential::Resolved)>, LaunchError> {
    if !enabled(config) {
        return Ok(None);
    }
    let Some(declared) = selection.credential_proxy.clone() else {
        return Ok(None);
    };
    let upstream = declared.validate()?;
    // Where the armed proxy may re-point the harness. The container provisions
    // its config INSIDE, where this launcher never runs: a config-side base
    // URL override is patched on the host, into a command the launcher builds
    // itself, so it is uncontained-native only (#11300 slice 3). Gated on
    // `enabled` here so a flag-off launch stays plain env-passthrough — a
    // profile may DECLARE config-side redirection for its opencode binding
    // while other bindings launch unproxied (#11714).
    if scope == Scope::Contained && declared.base_url_config.is_some() {
        return Err(LaunchError::config(
            "model profile credentialProxy.baseUrlConfig is only supported on uncontained \
             native launches; a contained launch is re-pointed with baseUrlEnv instead",
        ));
    }
    // A proxy the armed harness would not follow substitutes the placeholder
    // while the traffic still goes to the real upstream: fail closed rather
    // than half-proxy. OpenCode is re-pointed through its provisioned config;
    // every other harness only through baseUrlEnv (#11714).
    let redirected = !declared.base_url_env.is_empty()
        || (scope == Scope::NativeLoopback && declared.base_url_config.is_some());
    if !redirected {
        return Err(LaunchError::config(format!(
            "credentialProxy is enabled for this profile but nothing re-points the {runtime} \
             harness at the proxy, so the credential placeholder would still be sent to the \
             real upstream; declare credentialProxy.baseUrlEnv for this runtime or leave the \
             credentialProxy flag off"
        )));
    }
    // One account file is one assignment, and one placeholder stands for one
    // credential. A multi-variable provider (Bedrock, Vertex) has no single
    // value to substitute, so refuse rather than proxy one variable and leak
    // the rest.
    let [(source, target)] = &selection.credentials[..] else {
        return Err(LaunchError::config(
            "credentialProxy applies to a profile with exactly one credential variable; \
             this profile declares a different number",
        ));
    };
    // `credentials` is only the MAPPED pairs; `credential_sources` is every
    // declared name (#8437). A declared-but-unmapped variable (array-form
    // `credentialEnv` with a partial `credentialTargets` map) still passes
    // the check above with exactly one pair, but would be forwarded into the
    // container by name unproxied and unwithheld — this refuses that shape
    // too, as defense in depth alongside the `profiles::resolve` guard.
    if selection.credential_sources.len() != 1 {
        return Err(LaunchError::config(
            "credentialProxy applies to a profile with exactly one credential variable; \
             this profile declares additional credentialEnv variables beyond the mapped pair",
        ));
    }
    // Resolve the REAL credential here, on the host, through the unchanged
    // #8401 ladder — pool selection, fail-closed states and account
    // attribution all behave exactly as they do for an uncontained launch.
    let resolved = credential::resolve(root, selection)?;
    let secret = resolved
        .value_for(target)
        .ok_or_else(|| {
            LaunchError::config(format!(
                "credentialProxy is enabled for this profile but no credential resolved for \
                 {target}; export {source} or register an API-key account for it"
            ))
        })?
        .to_str()
        .ok_or_else(|| LaunchError::config("credentialProxy requires a UTF-8 credential value"))?
        .to_string();
    let provider = resolved
        .provider
        .clone()
        .or_else(|| selection.profile.clone())
        .unwrap_or_else(|| selection.provider.clone());
    // #8699: only a POOL-selected credential may ever be bad-marked at the
    // proxy — an env export (#8363's one-off trial path) is never touched,
    // mirroring `ingest::LaunchRecord::is_pool_selected`'s guard 1. When it is
    // pool-selected, `resolved.provider` is already the pool's own provider
    // namespace (the first arm of the `provider` fallback chain above), so no
    // separate field is needed to carry it into the bad-mark call.
    let pool_attribution = (resolved.source == credential::Source::Pool)
        .then(|| resolved.account.clone())
        .flatten()
        .map(|account| PoolAttribution {
            workspace_root: root.to_path_buf(),
            account,
            model_class: bad_marks::normalize_model_class(&selection.model),
        });
    let observe = (declared.observe && observe::enabled(config)).then(|| {
        let seat = pool_attribution
            .as_ref()
            .map_or("-", |attribution| attribution.account.as_str());
        // The proxy runs in this launcher process, which has no ops sink:
        // journal for the daemon's backfill instead (#11300).
        observe_context(runtime, selection, seat).with_journal(root)
    });
    let mask_dirs = match scope {
        Scope::Contained => vec![crate::api_keys_pool::paths::per_repo_api_keys_dir(root)],
        // No container filesystem to mask: the harness reads the workspace
        // with host permissions by design, and the placeholder is the only
        // credential it is ever handed.
        Scope::NativeLoopback => Vec::new(),
    };
    let prepared = arm(
        secret,
        provider,
        &declared,
        upstream,
        &[source.as_str(), target.as_str()],
        mask_dirs,
        pool_attribution,
        observe,
        scope,
    )?;
    Ok(Some((prepared, resolved)))
}

/// Observe-mode tags for a launch on `runtime` (the launcher's resolved
/// runtime, never reread from the environment) with `selection`'s profile.
fn observe_context(runtime: &str, selection: &Selection, seat: &str) -> observe::ObserveContext {
    let profile = selection
        .profile
        .clone()
        .unwrap_or_else(|| selection.provider.clone());
    let tap = match selection.profile.as_deref() {
        Some(p) => crate::runtime_preference::Tap::with_profile(runtime, p),
        None => crate::runtime_preference::Tap::runtime(runtime),
    };
    observe::ObserveContext::from_env(seat, &tap.to_string(), &profile, runtime)
}

/// The pool attribution [`arm`] needs so the proxy can bad-mark this launch's
/// account on a 429/quota-exhausted response (#8699). `None` for a
/// non-pool-selected credential (an env export, #8363) or a non-pool-capable
/// caller (`exec::build`'s Claude path, #8697) — the proxy bad-marks nothing
/// in either case.
struct PoolAttribution {
    workspace_root: std::path::PathBuf,
    account: String,
    model_class: Option<String>,
}

/// Bind the listener, mint the placeholder and register the launch record —
/// the half of substitution that does not care where the real credential came
/// from. Shared by the native-harness dispatch ([`prepare`], which resolves
/// the credential through the #8401 ladder) and the shell-adapter entry point
/// ([`exec`], issue #8697, which is handed a credential its adapter already
/// selected), so both paths get one registry, one placeholder shape and one
/// set of refusal semantics.
///
/// `credential_names` are every environment variable the container might read
/// the credential from: each is assigned the placeholder AND withheld from
/// by-name forwarding.
#[allow(clippy::too_many_arguments)] // one record shape; each argument is a distinct launch fact
fn arm(
    secret: String,
    provider: String,
    declared: &ProfileProxy,
    upstream: Upstream,
    credential_names: &[&str],
    mask_dirs: Vec<std::path::PathBuf>,
    pool_attribution: Option<PoolAttribution>,
    observe: Option<observe::ObserveContext>,
    scope: Scope,
) -> Result<Prepared, LaunchError> {
    let (bind_ip, host) = match scope {
        Scope::Contained => resolve_bind(),
        // The harness runs on this host; loopback is reachable and no other
        // address is ever needed, so no override is offered.
        Scope::NativeLoopback => (
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::Ipv4Addr::LOCALHOST.to_string(),
        ),
    };
    let bound = server::Bound::bind(bind_ip).map_err(|e| {
        LaunchError::config(format!("credentialProxy cannot bind a listener on {bind_ip}: {e}"))
    })?;
    let base_url = format!("http://{host}:{}", bound.addr().port());

    let launch_id = uuid::Uuid::new_v4().simple().to_string();
    let placeholder = Placeholder::generate();
    let registry = Registry::new();
    let mut record =
        Record::new(launch_id.clone(), provider, upstream.clone(), declared.header, secret);
    if let Some(attribution) = pool_attribution {
        record = record.with_pool_account(
            attribution.workspace_root,
            attribution.account,
            attribution.model_class,
        );
    }
    if let Some(context) = observe {
        record = record.with_observe(context);
    }
    registry.insert(&placeholder, record);

    // The placeholder is assigned under EVERY credential name (for a native
    // profile: its source variable and its harness-facing target), so the
    // profile resolution that re-runs inside the container finds it whichever
    // name it reads — and, being an explicit assignment, it wins over any
    // by-name forwarding of the same variable.
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut withheld: Vec<String> = Vec::new();
    for name in credential_names {
        if !assignments.iter().any(|(k, _)| k == name) {
            assignments.push(((*name).to_string(), placeholder.as_str().to_string()));
            withheld.push((*name).to_string());
        }
    }
    for name in &declared.base_url_env {
        assignments.push((name.clone(), base_url.clone()));
    }
    let marker = format!(
        "# LOOM_EGRESS_PROXY launch={launch_id} upstream={}://{} bind={} base_url={base_url} header={}",
        if declared.upstream.starts_with("http://") { "http" } else { "https" },
        upstream.authority(),
        bound.addr(),
        match declared.header {
            HeaderStyle::AuthorizationBearer => "authorization-bearer",
            HeaderStyle::XApiKey => "x-api-key",
        },
    );
    Ok(Prepared {
        injection: containment::Injection {
            assignments,
            withheld,
            mask_dirs,
            add_host_gateway: scope == Scope::Contained,
        },
        launch_id,
        base_url,
        registry,
        bound,
        marker,
    })
}

/// Serve the proxy for exactly as long as the contained launch runs, then
/// invalidate the placeholder.
///
/// This is the one dispatch path that does **not** `exec` (see
/// [`super::run`]'s module docs): the listener has to outlive the `docker run`
/// invocation, and the cheapest way to make the placeholder's lifetime exactly
/// the container's lifetime is to stay alive as its parent. The child keeps
/// this process's process group, so the daemon's existing process-group
/// teardown reaps both; `--rm` still removes the container.
///
/// Once the child has exited and the placeholder is dead, the launch's
/// per-request usage tally (#8699) is written to `log` — the same retained
/// launch log that carries the dispatch markers — as one
/// [`Record::usage_marker`] line per launch, keyed by `launch_id`.
pub fn run_with_proxy(
    prepared: Prepared,
    mut command: Command,
    log: &mut dyn std::io::Write,
) -> Result<(), LaunchError> {
    let Prepared {
        registry, bound, ..
    } = prepared;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| LaunchError::config(format!("credentialProxy runtime: {e}")))?;
    let guard = runtime.enter();
    let listener = bound
        .into_tokio()
        .map_err(|e| LaunchError::config(format!("credentialProxy listener: {e}")))?;
    runtime.spawn(server::serve(listener, registry.clone()));
    drop(guard);

    let mut child = command.spawn().map_err(|error| LaunchError {
        code: if error.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        },
        message: format!("cannot execute worker harness under the credential proxy: {error}"),
    })?;
    let status = child.wait().map_err(|error| LaunchError {
        code: 126,
        message: format!("worker harness could not be waited on: {error}"),
    });
    // Invalidate FIRST, whatever happened: a placeholder that escaped the
    // container must be dead the moment the launch is over.
    registry.close_all();
    write_usage_markers(&registry, log);
    let status = status?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Write every launch's usage line to `log`. Best-effort: a failed write is
/// logged and never changes the launch's exit status.
pub(crate) fn write_usage_markers(registry: &Registry, log: &mut dyn std::io::Write) {
    for line in registry.usage_markers() {
        if let Err(error) = writeln!(log, "{line}") {
            log::warn!("egress-proxy: could not write usage marker: {error}");
            return;
        }
    }
    if let Err(error) = log.flush() {
        log::warn!("egress-proxy: could not flush usage marker: {error}");
    }
}

/// Where the listener binds, and the hostname the container reaches it at.
///
/// - `LOOM_EGRESS_PROXY_BIND` wins, for a host whose docker networking is
///   neither of the two shapes below.
/// - On Docker Desktop (macOS/Windows) a loopback bind is reachable from the
///   container as `host.docker.internal`, so loopback is both the default and
///   the tightest option available.
/// - On Linux `host.docker.internal` resolves to the bridge gateway, and a
///   loopback-bound listener is *not* reachable there, so the listener binds
///   the bridge gateway address instead. That address is reachable by other
///   containers on the same bridge — which is why the placeholder is a
///   per-launch bearer token rather than an ambient allowance: a neighbour
///   without it gets a logged 401.
fn resolve_bind() -> (IpAddr, String) {
    if let Some(raw) = env_nonempty("LOOM_EGRESS_PROXY_BIND") {
        if let Ok(ip) = raw.parse::<IpAddr>() {
            let host = if ip.is_loopback() {
                "host.docker.internal".to_string()
            } else {
                ip.to_string()
            };
            return (ip, host);
        }
    }
    if cfg!(target_os = "linux") {
        if let Some(gateway) = docker_bridge_gateway() {
            return (gateway, gateway.to_string());
        }
        log::warn!(
            "egress-proxy: could not determine the docker bridge gateway; binding loopback. \
             Set LOOM_EGRESS_PROXY_BIND if the container cannot reach the proxy."
        );
    }
    (IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), "host.docker.internal".to_string())
}

/// Probe the docker bridge gateway. The program resolves through the shared
/// [`docker_program`](crate::sweep_registry::reaper::container_stop::docker_program)
/// so a `cfg(test)` build never runs the host's real `docker` (#11449,
/// #11523): there the probe fails at once and the caller takes the loopback
/// fallback.
fn docker_bridge_gateway() -> Option<IpAddr> {
    let mut command = Command::new(crate::sweep_registry::reaper::container_stop::docker_program());
    command.args([
        "network",
        "inspect",
        "bridge",
        "--format",
        "{{range .IPAM.Config}}{{.Gateway}}{{end}}",
    ]);
    let crate::proc_exec::Completion::Exited(output) =
        crate::proc_exec::run_bounded(command, std::time::Duration::from_secs(5)).ok()?
    else {
        return None;
    };
    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<IpAddr>()
        .ok()
}
