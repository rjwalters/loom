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
//! # Two entry points, one mechanism
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
//!
//! Both go through [`arm`] and [`run_with_proxy`], so they share one registry
//! shape, one placeholder format and one set of refusals.

pub mod exec;
pub mod registry;
pub mod server;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use super::{containment, credential, profiles::Selection, LaunchError};
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
}

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
    if let Some(raw) = env_nonempty("LOOM_NATIVE_CREDENTIAL_PROXY") {
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
    registry: Registry,
    bound: server::Bound,
    marker: String,
}

impl Prepared {
    /// Secret-free one-line record for the per-sweep log, mirroring
    /// [`containment::Profile::dispatch_marker`]'s shape.
    #[must_use]
    pub fn dispatch_marker(&self) -> &str {
        &self.marker
    }
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
pub fn prepare(
    root: &std::path::Path,
    selection: &Selection,
    config: &Value,
) -> Result<Option<Prepared>, LaunchError> {
    if !enabled(config) {
        return Ok(None);
    }
    let Some(declared) = selection.credential_proxy.clone() else {
        return Ok(None);
    };
    let upstream = declared.validate()?;
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
    arm(
        secret,
        provider,
        &declared,
        upstream,
        &[source.as_str(), target.as_str()],
        vec![crate::api_keys_pool::paths::per_repo_api_keys_dir(root)],
    )
    .map(Some)
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
fn arm(
    secret: String,
    provider: String,
    declared: &ProfileProxy,
    upstream: Upstream,
    credential_names: &[&str],
    mask_dirs: Vec<std::path::PathBuf>,
) -> Result<Prepared, LaunchError> {
    let (bind_ip, container_host) = resolve_bind();
    let bound = server::Bound::bind(bind_ip).map_err(|e| {
        LaunchError::config(format!("credentialProxy cannot bind a listener on {bind_ip}: {e}"))
    })?;
    let base_url = format!("http://{container_host}:{}", bound.addr().port());

    let launch_id = uuid::Uuid::new_v4().simple().to_string();
    let placeholder = Placeholder::generate();
    let registry = Registry::new();
    registry.insert(
        &placeholder,
        Record::new(launch_id.clone(), provider, upstream.clone(), declared.header, secret),
    );

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
            add_host_gateway: true,
        },
        launch_id,
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
pub fn run_with_proxy(prepared: Prepared, mut command: Command) -> Result<(), LaunchError> {
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
        message: format!("cannot execute contained worker: {error}"),
    })?;
    let status = child.wait().map_err(|error| LaunchError {
        code: 126,
        message: format!("contained worker could not be waited on: {error}"),
    });
    // Invalidate FIRST, whatever happened: a placeholder that escaped the
    // container must be dead the moment the launch is over.
    registry.close_all();
    let status = status?;
    std::process::exit(status.code().unwrap_or(1));
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

fn docker_bridge_gateway() -> Option<IpAddr> {
    let mut command = Command::new("docker");
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
