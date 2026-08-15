//! Runtime selection and fail-closed capability admission for daemon launches.
//!
//! Standalone roles resolve their own binding. A full sweep is deliberately
//! modelled as one `sweep-lifecycle` launch and is admitted against Builder's
//! requirements; this module does not imply intra-sweep runtime switching.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const BUILTIN_RUNTIME: &str = "claude";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeSource {
    Explicit,
    RoleEnvironment,
    GlobalEnvironment,
    RoleConfig,
    DefaultConfig,
    BuiltIn,
}

impl fmt::Display for RuntimeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", serde_json::to_value(self).unwrap().as_str().unwrap())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRuntime {
    pub role: String,
    pub runtime: String,
    pub source: RuntimeSource,
    pub adapter: PathBuf,
    pub role_manifest: PathBuf,
    pub runtime_manifest: PathBuf,
    /// The role manifest's own declared `suggestedWorkerType`, if any
    /// (#6201) — carried through admission so a caller can log loudly when
    /// the admitted `runtime` diverges from it (see
    /// [`suggested_worker_type_mismatch_warning`]). `None` when the role
    /// manifest has no such key (or it could not be read/parsed — this is a
    /// best-effort preference hint, not a validated requirement).
    pub suggested_worker_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRejection {
    pub role: String,
    pub runtime: String,
    pub source: RuntimeSource,
    pub unmet_capabilities: Vec<String>,
    pub reason: String,
}

impl fmt::Display for RuntimeRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "runtime admission rejected role={} runtime={} source={}: {}",
            self.role, self.runtime, self.source, self.reason
        )
    }
}

impl std::error::Error for RuntimeRejection {}

/// Exit code a client uses for a capability/config admission refusal:
/// `EX_CONFIG`, the same code `check-runtime-capabilities.sh` uses for a
/// mismatch. Keeping the shell checker's 78-vs-1 distinction in the CLI lets a
/// script tell "this runtime cannot run this role" apart from "the daemon
/// errored" without parsing text.
pub const EX_CONFIG: i32 = 78;

impl RuntimeRejection {
    /// Operator-facing, multi-line diagnostic naming the role/lifecycle, the
    /// runtime, the precedence tier that selected it, and the unmet capability
    /// names. Shared by every real client (`loom-daemon dispatch`, the MCP
    /// bridge's rendering, role-runner logs) so one wording is maintained once
    /// and a typed rejection never degrades into "unexpected response".
    #[must_use]
    pub fn diagnostic(&self) -> String {
        let mut out = format!(
            "Runtime admission refused this work (fail-closed).\n  \
             role/lifecycle:   {}\n  runtime:          {}\n  selected by:      {}",
            self.role, self.runtime, self.source
        );
        if !self.unmet_capabilities.is_empty() {
            out.push_str(&format!("\n  unmet capability: {}", self.unmet_capabilities.join(", ")));
        }
        out.push_str(&format!("\n  reason:           {}", self.reason));
        if self.role == "sweep-lifecycle" {
            out.push_str(
                "\n\nA full sweep runs as ONE runtime and is admitted against Builder's \
                 requirements\n(the strongest in the Curator->Builder->Judge->Doctor->Merge \
                 lifecycle); a per-role\nbinding cannot switch runtimes between phases. See \
                 defaults/docs/runtime-adapters.md.",
            );
        }
        out
    }
}

#[derive(Debug, Deserialize)]
struct RoleManifest {
    #[serde(default, rename = "runtimeRequirements")]
    runtime_requirements: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RuntimeManifest {
    runtime: String,
    capabilities: BTreeMap<String, serde_json::Value>,
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Read a role manifest's `suggestedWorkerType` key, best-effort (#6201).
///
/// **Deliberately observability-only.** `suggestedWorkerType` is documented
/// (`defaults/docs/guardrail-parity-codex.md` § "Promotion gate") as "a
/// dispatch *preference* hint only" — e.g. `builder.json` declares
/// `"codex"` as the eventual-promotion target while Codex's
/// `worktreeIsolation` capability is still `"partial"`, so Builder must keep
/// failing closed onto Codex today and actually run on `claude` by default.
/// Wiring this hint into [`choose_runtime`]'s precedence would silently make
/// EVERY zero-config Builder/sweep dispatch attempt (and fail-closed reject)
/// Codex — so this value never feeds runtime *selection*, only the
/// [`suggested_worker_type_mismatch_warning`] surfaced after selection.
///
/// Any read/parse failure (missing file, malformed JSON, non-string value)
/// yields `None` rather than an error — the full role manifest is still read
/// and STRICTLY validated later in [`resolve_and_admit`]'s normal
/// capability-check path, which is where a genuinely missing/malformed
/// manifest fails closed. Deliberately a second, independent read (rather
/// than threading the already-parsed [`RoleManifest`] here) so this
/// best-effort peek can never change the existing fail-closed error
/// text/paths that path produces.
fn role_suggested_worker_type(role_manifest_path: &Path) -> Option<String> {
    let data = fs::read(role_manifest_path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&data).ok()?;
    nonempty(value.get("suggestedWorkerType")?.as_str())
}

fn choose_runtime(
    explicit: Option<&str>,
    role_env: Option<&str>,
    global_env: Option<&str>,
    role_config: Option<String>,
    default_config: Option<String>,
) -> (String, RuntimeSource) {
    [
        (nonempty(explicit), RuntimeSource::Explicit),
        (nonempty(role_env), RuntimeSource::RoleEnvironment),
        (nonempty(global_env), RuntimeSource::GlobalEnvironment),
        (role_config.and_then(|v| nonempty(Some(&v))), RuntimeSource::RoleConfig),
        (default_config.and_then(|v| nonempty(Some(&v))), RuntimeSource::DefaultConfig),
        (Some(BUILTIN_RUNTIME.into()), RuntimeSource::BuiltIn),
    ]
    .into_iter()
    .find_map(|(v, s)| v.map(|v| (v, s)))
    .unwrap()
}

/// Operator-facing WARN text (#6201) for when the runtime a role was
/// admitted onto diverges from its own role manifest's declared
/// `suggestedWorkerType`. This is the loud, at-selection signal the incident
/// that filed #6201 was missing: `curator.json` declared
/// `suggestedWorkerType: "claude"`, yet the role was admitted onto Codex with
/// no diagnostic naming the divergence anywhere.
///
/// **Only fires when something actually overrode the choice**
/// (`admission.source != RuntimeSource::BuiltIn`). A zero-config repo running
/// a role on the honest built-in default is not a "redirect" — it is the
/// absence of any override — and treating it as one would make every ordinary
/// dispatch of a role like Builder noisy: `builder.json` deliberately
/// declares the aspirational `suggestedWorkerType: "codex"` (the eventual
/// promotion target, see `defaults/docs/guardrail-parity-codex.md` §
/// "Promotion gate") while Codex's `worktreeIsolation` capability is still
/// `"partial"`, so Builder legitimately runs on `claude` via the built-in
/// default today and is *expected* to diverge from its own hint until that
/// capability promotes. Once any real override tier wins (env, `runtimes.*`
/// config, or an explicit per-dispatch runtime) and it disagrees with the
/// declared suggestion, this warns regardless of *which* tier won — an
/// operator's own deliberate override is still worth naming loudly, not just
/// an unintentional fleet-wide `runtimes.default` experiment.
///
/// Pulled out as a pure function (rather than inlined at each `log::warn!`
/// call site) so `role_runner.rs`'s per-role-tick dispatch and
/// `sweep_registry::dispatch`'s per-sweep dispatch — the two production
/// callers of [`resolve_and_admit`] — render the exact same wording and
/// cannot silently drift apart.
#[must_use]
pub fn suggested_worker_type_mismatch_warning(admission: &ResolvedRuntime) -> Option<String> {
    if admission.source == RuntimeSource::BuiltIn {
        return None;
    }
    let suggested = admission.suggested_worker_type.as_deref()?;
    if suggested == admission.runtime {
        return None;
    }
    Some(format!(
        "runtime-selection: {} declares suggestedWorkerType={suggested:?} in its role manifest \
         but was admitted onto runtime={:?} (selected by {}) — a runtimes.default/env override, \
         an explicit runtimes.roles.{} binding, or an explicit per-dispatch runtime redirected \
         this role away from its declared preference; if unintentional (e.g. a fleet-wide \
         runtimes.default experiment), scope the override to the roles that actually opted in \
         (#6201)",
        admission.role, admission.runtime, admission.source, admission.role
    ))
}

pub fn canonical_role(role: &str) -> Option<&'static str> {
    match role.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "curator" | "issue-curator" => Some("curator"),
        "judge" | "code-review-specialist" => Some("judge"),
        "builder" | "development-worker" => Some("builder"),
        "doctor" | "pr-fixer" => Some("doctor"),
        "champion" => Some("champion"),
        "auditor" => Some("auditor"),
        "guide" => Some("guide"),
        "architect" => Some("architect"),
        "hermit" => Some("hermit"),
        "driver" => Some("driver"),
        "loom" | "sweep-lifecycle" | "sweep" => Some("sweep-lifecycle"),
        _ => None,
    }
}

/// Validate the whole `runtimes.roles` key set, then read the requested role's
/// binding plus `runtimes.default`.
///
/// Fail-closed key validation (#4494): a key that is not a known role name —
/// `runtimes.roles.not-a-role`, a typo like `builderr`, or a key whose value is
/// not a string — is an **error**, not a silently-ignored entry. Before this,
/// `config_runtime` only looked up the *requested* canonical role, so a
/// misconfigured key never surfaced anywhere: an operator who typed
/// `runtimes.roles.buidler = "claude"` got the (possibly Codex) default for
/// every Builder launch with no diagnostic at all. The scoped requirement is to
/// "reject unknown role names in explicit dispatch/config", so the whole map is
/// checked on every admission, not just the requested key.
///
/// Empty-value semantics are preserved exactly: `"curator": ""` is a *valid*
/// key with an unset value (it falls through to the next precedence tier), and
/// an absent `runtimes.roles` block is not an error.
/// Validate a resolved `runtimes.roles` JSON value's shape: must be an
/// object; every key a known role name; every value a string.
///
/// Factored out of [`config_runtime`] (#5006) so the exact same rule set can
/// be run twice: once here at admission time (unchanged behavior, #4494),
/// and once proactively from `loom-daemon validate`
/// ([`check_runtimes_roles_config`]) — before any role actually ticks and
/// fails on it. Keeping one function means the two call sites can never
/// silently drift apart.
pub fn validate_runtimes_roles_shape(roles: &serde_json::Value) -> Result<(), String> {
    let Some(map) = roles.as_object() else {
        return Err(format!(
            "runtimes.roles must be an object mapping role names to runtimes, got {}",
            type_name_of(roles)
        ));
    };
    let mut unknown: Vec<String> = map
        .keys()
        .filter(|key| canonical_role(key).is_none())
        .cloned()
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown role name(s) in runtimes.roles: {} (known roles: {})",
            unknown.join(", "),
            KNOWN_ROLE_KEYS.join(", ")
        ));
    }
    let mut non_string: Vec<String> = map
        .iter()
        .filter(|(_, value)| !value.is_string())
        .map(|(key, _)| key.clone())
        .collect();
    non_string.sort();
    if !non_string.is_empty() {
        return Err(format!("runtimes.roles value(s) must be strings: {}", non_string.join(", ")));
    }
    Ok(())
}

/// Proactively check an already-resolved config's `runtimes.roles` map for
/// the same fail-closed problems [`config_runtime`] rejects at admission
/// time (unknown role keys, non-string values, a non-object shape). Returns
/// one formatted message per problem found — currently at most one, since
/// [`validate_runtimes_roles_shape`] stops at the first violation — so
/// `loom-daemon validate` can surface a `runtimes.roles` misconfiguration
/// any time an operator runs it, rather than only when a specific role's
/// tick fails (#5006). An absent `runtimes.roles` block is not an error and
/// yields an empty vec, matching [`config_runtime`]'s own empty-is-fine
/// semantics.
#[must_use]
pub fn check_runtimes_roles_config(config: &serde_json::Value) -> Vec<String> {
    let Some(roles) = crate::config_resolver::get_path(config, "runtimes.roles") else {
        return Vec::new();
    };
    match validate_runtimes_roles_shape(roles) {
        Ok(()) => Vec::new(),
        Err(message) => vec![format!("runtimes.roles: {message}")],
    }
}

fn config_runtime(root: &Path, role: &str) -> Result<(Option<String>, Option<String>), String> {
    let config = crate::config_resolver::resolve_effective_config(root);
    let roles = crate::config_resolver::get_path(&config, "runtimes.roles");
    if let Some(roles) = roles {
        validate_runtimes_roles_shape(roles)?;
    }
    let per_role = roles
        .and_then(|v| v.get(role))
        .and_then(serde_json::Value::as_str)
        .and_then(|v| nonempty(Some(v)));
    let default = crate::config_resolver::get_path(&config, "runtimes.default")
        .and_then(serde_json::Value::as_str)
        .and_then(|v| nonempty(Some(v)));
    Ok((per_role, default))
}

/// Canonical role names accepted as `runtimes.roles` keys, for the fail-closed
/// diagnostic above. Aliases (`issue-curator`, `pr-fixer`, `sweep`, …) are also
/// accepted by [`canonical_role`]; only the canonical spellings are listed so
/// the message stays short and points at the documented shape.
const KNOWN_ROLE_KEYS: &[&str] = &[
    "architect",
    "auditor",
    "builder",
    "champion",
    "curator",
    "doctor",
    "driver",
    "guide",
    "hermit",
    "judge",
    "sweep-lifecycle",
];

fn type_name_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Bundled fallback runtime manifests, compiled directly into the daemon
/// binary via `include_str!` (#5002).
///
/// `roots()` above resolves an unpopulated `.loom/runtimes/` to a
/// `defaults/runtimes/<name>.json` fallback, but `defaults/` is a
/// Loom-*source*-repo-only directory: a consumer install has `.loom/`, never
/// `defaults/`. So on every managed repo that is not the Loom checkout
/// itself, that fallback path is unreachable by construction — a consumer
/// repo whose `.loom/runtimes/` is missing (or missing just one runtime's
/// manifest, e.g. a 0.16.0-era install never resynced per #4688/#4700) was
/// permanently unable to admit a non-builtin runtime the daemon binary
/// itself ships an adapter for.
///
/// This table mirrors the manifests shipped at `defaults/runtimes/*.json` at
/// build time, so the binary carries its own knowledge of the runtimes it
/// ships adapters for and does not depend on any on-disk `defaults/` or
/// `.loom/runtimes/` copy being present or complete. It is consulted only
/// when the on-disk manifest lookup misses (see `resolve_and_admit` below);
/// an on-disk `.loom/runtimes/<name>.json` — however it got there — always
/// wins over the bundled copy, so a repo can still override capabilities
/// on-disk.
const BUNDLED_RUNTIME_MANIFESTS: &[(&str, &str)] = &[
    ("claude", include_str!("../../defaults/runtimes/claude.json")),
    ("codex", include_str!("../../defaults/runtimes/codex.json")),
    ("aider", include_str!("../../defaults/runtimes/aider.json")),
];

/// Look up the bundled fallback manifest contents for `runtime`, if the
/// daemon binary ships one. Returns `None` for any runtime the binary was
/// not built with a manifest for (e.g. an operator-defined custom runtime) —
/// those still fail closed with no fallback, per the unchanged fail-closed
/// contract for non-builtin runtimes with no reachable manifest anywhere.
fn bundled_runtime_manifest(runtime: &str) -> Option<&'static str> {
    BUNDLED_RUNTIME_MANIFESTS
        .iter()
        .find(|(name, _)| *name == runtime)
        .map(|(_, contents)| *contents)
}

fn roots(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let installed = root.join(".loom");
    let defaults = root.join("defaults");
    // Each subdirectory falls back to `defaults/` independently (#4688): a
    // consumer repo can have `.loom/roles/` populated while `.loom/runtimes/`
    // is still missing (a provisioning gap on older installs), and gating
    // BOTH choices on a single combined boolean sent every dispatch on that
    // host to a nonexistent `defaults/roles/...` path, rejecting all of them.
    let roles = if installed.join("roles").is_dir() {
        installed.clone()
    } else {
        defaults.clone()
    };
    let runtimes = if installed.join("runtimes").is_dir() {
        installed.clone()
    } else {
        defaults.clone()
    };
    let scripts = if installed.join("scripts").is_dir() {
        installed
    } else {
        defaults
    };
    (roles.join("roles"), runtimes.join("runtimes"), scripts.join("scripts"))
}

pub fn resolve_and_admit(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
) -> Result<ResolvedRuntime, RuntimeRejection> {
    let Some(canonical) = canonical_role(role) else {
        return Err(RuntimeRejection {
            role: role.to_string(),
            runtime: nonempty(explicit).unwrap_or_else(|| BUILTIN_RUNTIME.into()),
            source: RuntimeSource::Explicit,
            unmet_capabilities: vec![],
            reason: format!("unknown role {role:?}"),
        });
    };
    let lookup_role = if canonical == "sweep-lifecycle" {
        "builder"
    } else {
        canonical
    };
    let env_name = format!("LOOM_RUNTIME_{}", canonical.replace('-', "_").to_ascii_uppercase());
    // `roles`/`role_manifest` computed before runtime selection (#6201): the
    // role manifest path depends only on `root`/`lookup_role`, never on the
    // resolved runtime, so the best-effort `suggestedWorkerType` peek below
    // (observability only — see `role_suggested_worker_type`'s doc comment
    // for why this deliberately does NOT feed `choose_runtime`) can run
    // before selection. `runtimes`/`scripts` are still resolved here too
    // (unchanged from before) since they are equally runtime-independent;
    // only `runtime_manifest`/`adapter` (which DO depend on the chosen
    // runtime) are computed after `choose_runtime`.
    let (roles, runtimes, scripts) = roots(root);
    let role_manifest = roles.join(format!("{lookup_role}.json"));
    let role_suggested = role_suggested_worker_type(&role_manifest);
    // A malformed `runtimes.roles` map fails closed for EVERY role, even one
    // whose own key is spelled correctly and even under an explicit override:
    // the config tier is the thing that is broken, and silently honouring the
    // rest of a map with an unknown role key is exactly the "silently ignored"
    // behaviour this rejects.
    let (role_config, default_config) = match config_runtime(root, canonical) {
        Ok(pair) => pair,
        Err(reason) => {
            return Err(RuntimeRejection {
                role: canonical.to_string(),
                runtime: nonempty(explicit).unwrap_or_else(|| BUILTIN_RUNTIME.into()),
                source: RuntimeSource::RoleConfig,
                unmet_capabilities: vec![],
                reason,
            });
        }
    };
    let role_env = std::env::var(&env_name).ok();
    let global_env = std::env::var("LOOM_RUNTIME").ok();
    let (runtime, source) = choose_runtime(
        explicit,
        role_env.as_deref(),
        global_env.as_deref(),
        role_config,
        default_config,
    );
    let runtime_manifest = runtimes.join(format!("{runtime}.json"));
    let adapter = scripts.join(format!("spawn-{runtime}.sh"));

    let reject = |reason: String, unmet_capabilities: Vec<String>| RuntimeRejection {
        role: canonical.to_string(),
        runtime: runtime.clone(),
        source: source.clone(),
        unmet_capabilities,
        reason,
    };
    if !adapter.is_file() {
        return Err(reject(format!("adapter {} is missing", adapter.display()), vec![]));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&adapter)
            .map(|m| m.permissions().mode() & 0o111 == 0)
            .unwrap_or(true)
        {
            return Err(reject(format!("adapter {} is not executable", adapter.display()), vec![]));
        }
    }
    let role_data = fs::read(&role_manifest)
        .map_err(|e| reject(format!("role manifest {}: {e}", role_manifest.display()), vec![]))?;
    let role_doc: RoleManifest = serde_json::from_slice(&role_data).map_err(|e| {
        reject(format!("malformed role manifest {}: {e}", role_manifest.display()), vec![])
    })?;
    let runtime_data = match fs::read(&runtime_manifest) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && runtime == BUILTIN_RUNTIME => {
            // #4688: the builtin `claude` runtime is the zero-config default;
            // its capability manifest is additive, not a hard dependency. A
            // provisioning gap that leaves `.loom/runtimes/` (and any
            // `defaults/runtimes/` fallback) without a `claude.json` must not
            // fail closed for every dispatch on that host — admit with no
            // capability constraints instead. Non-builtin runtimes keep
            // failing closed below: their manifest is the only source of
            // truth for what they can do.
            return Ok(ResolvedRuntime {
                role: canonical.to_string(),
                runtime,
                source,
                adapter,
                role_manifest,
                runtime_manifest,
                suggested_worker_type: role_suggested,
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Non-builtin runtime, no on-disk manifest at the resolved path
            // (which may itself be the unreachable `defaults/runtimes/...`
            // fallback on a consumer install — see `roots()` and
            // `BUNDLED_RUNTIME_MANIFESTS` above). Fall back to the manifest
            // the daemon binary was built with, if any (#5002).
            match bundled_runtime_manifest(&runtime) {
                Some(bundled) => bundled.as_bytes().to_vec(),
                None => {
                    // No bundled fallback for this runtime either: fail
                    // closed, but name a path that could actually exist in
                    // THIS repo -- `.loom/runtimes/<name>.json` -- rather
                    // than `runtime_manifest`, which on a consumer install is
                    // the unreachable `<repo>/defaults/runtimes/<name>.json`
                    // fallback path `roots()` computed (`defaults/` only
                    // exists in the Loom source checkout itself).
                    let reachable = root.join(".loom/runtimes").join(format!("{runtime}.json"));
                    return Err(reject(
                        format!(
                            "runtime manifest not found at {} (no bundled fallback shipped for runtime {runtime:?})",
                            reachable.display()
                        ),
                        vec![],
                    ));
                }
            }
        }
        Err(e) => {
            return Err(reject(
                format!("runtime manifest {}: {e}", runtime_manifest.display()),
                vec![],
            ));
        }
    };
    let runtime_doc: RuntimeManifest = serde_json::from_slice(&runtime_data).map_err(|e| {
        reject(
            format!("malformed runtime manifest {}: {e}", runtime_manifest.display()),
            vec![],
        )
    })?;
    if runtime_doc.runtime != runtime {
        return Err(reject(
            format!("runtime manifest declares {:?}, expected {runtime:?}", runtime_doc.runtime),
            vec![],
        ));
    }
    let mut unmet = Vec::new();
    for requirement in &role_doc.runtime_requirements {
        match runtime_doc
            .capabilities
            .get(requirement)
            .and_then(serde_json::Value::as_str)
        {
            Some("yes") => {}
            _ => unmet.push(requirement.clone()),
        }
    }
    if !unmet.is_empty() {
        return Err(reject(format!("unmet capabilities: {}", unmet.join(", ")), unmet));
    }
    Ok(ResolvedRuntime {
        role: canonical.to_string(),
        runtime,
        source,
        adapter,
        role_manifest,
        runtime_manifest,
        suggested_worker_type: role_suggested,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
    /// scope of a test and restores whatever value (if any) it previously
    /// had — including across a mid-test assertion panic, since Rust
    /// unwinds through `Drop`. Some host/dev-container shells export
    /// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
    /// without this guard that ambient value silently outranks the
    /// `runtimes.default` / `runtimes.roles` config precedence this test
    /// exercises (#4739).
    struct ClearedLoomRuntimeEnv(Option<String>);

    impl ClearedLoomRuntimeEnv {
        fn new() -> Self {
            let prior = std::env::var("LOOM_RUNTIME").ok();
            std::env::remove_var("LOOM_RUNTIME");
            Self(prior)
        }
    }

    impl Drop for ClearedLoomRuntimeEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_RUNTIME", v),
                None => std::env::remove_var("LOOM_RUNTIME"),
            }
        }
    }

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for sub in ["defaults/roles", "defaults/runtimes", "defaults/scripts"] {
            fs::create_dir_all(d.path().join(sub)).unwrap();
        }
        fs::write(d.path().join("defaults/roles/curator.json"), "{}").unwrap();
        fs::write(d.path().join("defaults/roles/judge.json"), r#"{"runtimeRequirements":["mcp"]}"#)
            .unwrap();
        fs::write(
            d.path().join("defaults/roles/builder.json"),
            r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
        )
        .unwrap();
        for (name, isolation) in [("claude", "yes"), ("codex", "partial")] {
            fs::write(
                d.path().join(format!("defaults/runtimes/{name}.json")),
                format!(r#"{{"runtime":"{name}","capabilities":{{"mcp":"yes","worktreeIsolation":"{isolation}"}}}}"#),
            ).unwrap();
            let adapter = d.path().join(format!("defaults/scripts/spawn-{name}.sh"));
            fs::write(&adapter, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(adapter, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        d
    }

    #[test]
    fn precedence_and_empty_values() {
        assert_eq!(
            choose_runtime(Some("codex"), Some("claude"), Some("claude"), None, None),
            ("codex".into(), RuntimeSource::Explicit)
        );
        assert_eq!(
            choose_runtime(Some(" "), Some("codex"), Some("claude"), None, None),
            ("codex".into(), RuntimeSource::RoleEnvironment)
        );
        assert_eq!(
            choose_runtime(None, Some(""), Some("codex"), None, None),
            ("codex".into(), RuntimeSource::GlobalEnvironment)
        );
        assert_eq!(
            choose_runtime(None, None, None, Some("codex".into()), Some("claude".into())),
            ("codex".into(), RuntimeSource::RoleConfig)
        );
        assert_eq!(
            choose_runtime(None, None, None, Some("".into()), Some("codex".into())),
            ("codex".into(), RuntimeSource::DefaultConfig)
        );
        assert_eq!(
            choose_runtime(None, None, None, None, None),
            ("claude".into(), RuntimeSource::BuiltIn)
        );
    }

    /// #6201: [`suggested_worker_type_mismatch_warning`] fires only when the
    /// role manifest declares a preference, the admitted runtime diverges
    /// from it, AND something actually overrode the built-in default — never
    /// when there is nothing declared, never when they already agree, and
    /// never for the honest zero-config default (the `RuntimeSource::BuiltIn`
    /// carve-out that keeps Builder's aspirational `"codex"` hint silent on
    /// every ordinary `claude`-runtime dispatch — see the doc comment on
    /// [`suggested_worker_type_mismatch_warning`] for why).
    #[test]
    fn suggested_worker_type_mismatch_warning_fires_only_on_real_divergence() {
        let admitted =
            |suggested: Option<&str>, runtime: &str, source: RuntimeSource| ResolvedRuntime {
                role: "curator".into(),
                runtime: runtime.into(),
                source,
                adapter: PathBuf::from("/adapter"),
                role_manifest: PathBuf::from("/role.json"),
                runtime_manifest: PathBuf::from("/runtime.json"),
                suggested_worker_type: suggested.map(str::to_string),
            };
        // No declared suggestion at all -> nothing to warn about.
        assert!(suggested_worker_type_mismatch_warning(&admitted(
            None,
            "codex",
            RuntimeSource::DefaultConfig
        ))
        .is_none());
        // Declared and matches admitted -> silent.
        assert!(suggested_worker_type_mismatch_warning(&admitted(
            Some("claude"),
            "claude",
            RuntimeSource::BuiltIn
        ))
        .is_none());
        // The zero-config built-in default diverging from an aspirational
        // suggestion (Builder's real-world "codex" hint while it actually
        // runs on "claude") is NOT a redirect -> silent, regardless of the
        // mismatch, because nothing overrode anything.
        assert!(suggested_worker_type_mismatch_warning(&admitted(
            Some("codex"),
            "claude",
            RuntimeSource::BuiltIn
        ))
        .is_none());
        // Declared "claude" but admitted onto "codex" via a broad default ->
        // loud, names the role, both runtimes, and the winning source.
        let msg = suggested_worker_type_mismatch_warning(&admitted(
            Some("claude"),
            "codex",
            RuntimeSource::DefaultConfig,
        ))
        .expect("mismatch must warn");
        assert!(msg.contains("curator"), "{msg}");
        assert!(msg.contains("suggestedWorkerType=\"claude\""), "{msg}");
        assert!(msg.contains("runtime=\"codex\""), "{msg}");
        assert!(msg.contains("default-config"), "{msg}");
        // Even an EXPLICIT override away from the declared suggestion still
        // warns — an operator deliberately overriding should still see the
        // divergence named, not just an unintentional broad-default redirect.
        assert!(suggested_worker_type_mismatch_warning(&admitted(
            Some("claude"),
            "codex",
            RuntimeSource::Explicit
        ))
        .is_some());
    }

    #[test]
    fn sweep_is_one_runtime_gated_by_builder() {
        let d = fixture();
        let e = resolve_and_admit(d.path(), "sweep-lifecycle", Some("codex")).unwrap_err();
        assert_eq!(e.role, "sweep-lifecycle");
        assert_eq!(e.unmet_capabilities, vec!["worktreeIsolation"]);
    }

    #[test]
    fn shipped_safe_codex_roles_are_admitted() {
        let d = fixture();
        assert_eq!(
            resolve_and_admit(d.path(), "issue-curator", Some("codex"))
                .unwrap()
                .role,
            "curator"
        );
        assert_eq!(
            resolve_and_admit(d.path(), "judge", Some("codex"))
                .unwrap()
                .runtime,
            "codex"
        );
    }

    #[test]
    fn malformed_missing_and_unknown_values_fail_closed() {
        let d = fixture();
        fs::write(
            d.path().join("defaults/runtimes/bad.json"),
            r#"{"runtime":"bad","capabilities":{"mcp":"maybe"}}"#,
        )
        .unwrap();
        let adapter = d.path().join("defaults/scripts/spawn-bad.sh");
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(resolve_and_admit(d.path(), "judge", Some("bad"))
            .unwrap_err()
            .unmet_capabilities
            .contains(&"mcp".to_string()));
        assert!(resolve_and_admit(d.path(), "judge", Some("missing"))
            .unwrap_err()
            .reason
            .contains("adapter"));
        assert!(resolve_and_admit(d.path(), "not-a-role", Some("claude"))
            .unwrap_err()
            .reason
            .contains("unknown role"));
    }

    /// The shell checker's `--json` decision object (#4494), parsed so the
    /// conformance matrix can compare the exact unmet-capability SET rather
    /// than just pass/fail.
    #[derive(Debug, Deserialize)]
    struct ShellDecision {
        decision: String,
        unmet: Vec<String>,
    }

    /// Run `check-runtime-capabilities.sh --json` and return its parsed
    /// decision plus its exit code. Panics (rather than degrading) if the
    /// script does not produce parseable JSON — a silent parse failure is
    /// exactly the drift blindness this checker exists to prevent.
    fn shell_decision(
        checker: &Path,
        dir: &Path,
        role: &str,
        runtime: &str,
    ) -> (ShellDecision, i32) {
        let out = Command::new("bash")
            .arg(checker)
            .args(["--role", role, "--runtime", runtime, "--json", "--dir"])
            .arg(dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let json = stdout.lines().last().unwrap_or_default();
        let parsed: ShellDecision = serde_json::from_str(json).unwrap_or_else(|e| {
            panic!("checker --json output for role={role} runtime={runtime} unparseable ({e}): {stdout:?}")
        });
        (parsed, out.status.code().unwrap_or(-1))
    }

    fn sorted(mut values: Vec<String>) -> Vec<String> {
        values.sort();
        values
    }

    /// One matrix drives both implementations: every shipped role/runtime pair
    /// is evaluated natively and by the installed shell checker, and BOTH the
    /// decision and the exact named unmet-capability set must be identical.
    /// This prevents two independent assertion lists from silently drifting —
    /// comparing only pass/fail would let the two paths agree on "reject" while
    /// naming different capabilities.
    #[test]
    fn native_and_shell_conformance_for_every_shipped_pair() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let defaults = repo.join("defaults");
        let checker = defaults.join("scripts/check-runtime-capabilities.sh");
        let roles = fs::read_dir(defaults.join("roles")).unwrap();
        let runtimes: Vec<String> = fs::read_dir(defaults.join("runtimes"))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|e| e.path().file_stem()?.to_str().map(str::to_owned))
            .collect();
        assert!(!runtimes.is_empty(), "no shipped runtime manifests found");
        let mut compared = 0_usize;
        let mut rejections = 0_usize;

        for role_entry in roles.filter_map(Result::ok) {
            if role_entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(role) = role_entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            // Roles outside the daemon's canonical launch set are not
            // schedulable here; the shell checker remains a generic tool.
            let Some(canonical) = canonical_role(&role) else {
                continue;
            };
            for runtime in &runtimes {
                let native = resolve_and_admit(repo, &role, Some(runtime));
                // A full sweep is one runtime admitted against Builder's
                // (strongest lifecycle) requirements — that is the role the
                // shell checker must be asked about for the `loom`/sweep entry.
                let shell_role = if canonical == "sweep-lifecycle" {
                    "builder"
                } else {
                    canonical
                };
                let (shell, code) = shell_decision(&checker, &defaults, shell_role, runtime);

                // 1. Identical decisions.
                assert_eq!(
                    native.is_ok(),
                    shell.decision == "admit",
                    "native/shell admission drift for role={role} runtime={runtime}: \
                     native={native:?}, shell={shell:?} (exit {code})"
                );
                // 2. Identical unmet-capability SETS (the named capabilities,
                //    not merely the pass/fail verdict).
                let native_unmet = native
                    .as_ref()
                    .err()
                    .map(|r| r.unmet_capabilities.clone())
                    .unwrap_or_default();
                assert_eq!(
                    sorted(native_unmet),
                    sorted(shell.unmet.clone()),
                    "native/shell unmet-capability drift for role={role} runtime={runtime}: \
                     native={native:?}, shell={shell:?}"
                );
                // 3. The checker's EX_CONFIG(78)-vs-error(1) distinction is
                //    preserved and matches the decision it reported.
                match shell.decision.as_str() {
                    "admit" => assert_eq!(code, 0, "role={role} runtime={runtime}"),
                    "reject" => {
                        assert_eq!(code, 78, "role={role} runtime={runtime}");
                        rejections += 1;
                    }
                    other => {
                        panic!("unexpected decision {other:?} for role={role} runtime={runtime}")
                    }
                }
                compared += 1;
            }
        }

        // The matrix must be non-degenerate: it has to actually contain the
        // shipped `builder`/sweep + `codex` refusals whose named unmet set is
        // the contract under test (`worktreeIsolation`).
        assert!(compared >= 2, "conformance matrix compared only {compared} pair(s)");
        assert!(rejections > 0, "conformance matrix contained no rejection to compare");
        let (builder_codex, code) = shell_decision(&checker, &defaults, "builder", "codex");
        assert_eq!(code, 78);
        assert_eq!(builder_codex.unmet, vec!["worktreeIsolation"]);
        assert_eq!(
            resolve_and_admit(repo, "sweep-lifecycle", Some("codex"))
                .unwrap_err()
                .unmet_capabilities,
            builder_codex.unmet,
            "the full-sweep gate must name the same unmet set as the shell checker"
        );
    }

    /// Fail-closed `runtimes.roles` key validation (#4494): an unknown role key
    /// is rejected rather than silently ignored, while an empty *value* keeps
    /// its established "unset, fall through" meaning.
    #[test]
    #[serial_test::serial]
    fn unknown_configured_role_keys_fail_closed() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let d = fixture();
        let write_config = |contents: &str| {
            fs::create_dir_all(d.path().join(".loom")).unwrap();
            fs::write(d.path().join(".loom/config.json"), contents).unwrap();
        };

        // Unknown key -> fail closed, for the requested role AND for any other.
        write_config(r#"{"runtimes":{"roles":{"not-a-role":"codex"}}}"#);
        let rejection = resolve_and_admit(d.path(), "judge", None).unwrap_err();
        assert_eq!(rejection.source, RuntimeSource::RoleConfig);
        assert!(rejection.reason.contains("not-a-role"), "{}", rejection.reason);
        assert!(rejection.unmet_capabilities.is_empty());
        // Even an explicit per-dispatch runtime does not rescue a broken map.
        assert!(resolve_and_admit(d.path(), "curator", Some("claude"))
            .unwrap_err()
            .reason
            .contains("unknown role name"));

        // A misspelled real role is caught by the same check.
        write_config(r#"{"runtimes":{"roles":{"buidler":"claude"}}}"#);
        assert!(resolve_and_admit(d.path(), "judge", None)
            .unwrap_err()
            .reason
            .contains("buidler"));

        // Non-string values fail closed too.
        write_config(r#"{"runtimes":{"roles":{"curator":true}}}"#);
        assert!(resolve_and_admit(d.path(), "curator", None)
            .unwrap_err()
            .reason
            .contains("must be strings"));

        // A non-object `runtimes.roles` fails closed.
        write_config(r#"{"runtimes":{"roles":"codex"}}"#);
        assert!(resolve_and_admit(d.path(), "curator", None)
            .unwrap_err()
            .reason
            .contains("must be an object"));

        // Known keys (canonical + alias) with an EMPTY value stay "unset":
        // resolution falls through to `runtimes.default`, unchanged semantics.
        write_config(
            r#"{"runtimes":{"default":"codex","roles":{"curator":"","issue-curator":"","sweep-lifecycle":""}}}"#,
        );
        let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
        assert_eq!(admitted.runtime, "codex");
        assert_eq!(admitted.source, RuntimeSource::DefaultConfig);

        // And a valid per-role binding still wins over the default.
        write_config(r#"{"runtimes":{"default":"claude","roles":{"curator":"codex"}}}"#);
        let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
        assert_eq!(admitted.runtime, "codex");
        assert_eq!(admitted.source, RuntimeSource::RoleConfig);

        // No `runtimes` block at all remains the zero-config Claude path.
        write_config("{}");
        let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
        assert_eq!(admitted.runtime, "claude");
        assert_eq!(admitted.source, RuntimeSource::BuiltIn);
    }

    /// End-to-end #6201 regression, the exact incident shape: a role
    /// manifest declares `"suggestedWorkerType": "claude"`, but a fleet-wide
    /// `runtimes.default` runtime-selection experiment (e.g. testing Codex
    /// broadly) is in effect with no role-targeted override at all.
    /// `resolve_and_admit` still resolves the runtime exactly as before
    /// (selection is unchanged — see `role_suggested_worker_type`'s doc
    /// comment for why a role's own hint must not become a binding
    /// override), but [`ResolvedRuntime`] now carries the declared
    /// suggestion, and [`suggested_worker_type_mismatch_warning`] is the
    /// loud, at-selection diagnostic the original incident lacked entirely.
    #[test]
    #[serial_test::serial]
    fn suggested_worker_type_is_observable_but_never_overrides_selection() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let d = fixture();
        // curator declares a preference the zero-config fixture didn't have.
        fs::write(
            d.path().join("defaults/roles/curator.json"),
            r#"{"suggestedWorkerType":"claude"}"#,
        )
        .unwrap();
        let write_config = |contents: &str| {
            fs::create_dir_all(d.path().join(".loom")).unwrap();
            fs::write(d.path().join(".loom/config.json"), contents).unwrap();
        };

        // Zero config: curator resolves onto its own declared "claude" via
        // the honest built-in default -> no divergence, no warning.
        let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
        assert_eq!(admitted.runtime, "claude");
        assert_eq!(admitted.source, RuntimeSource::BuiltIn);
        assert_eq!(admitted.suggested_worker_type.as_deref(), Some("claude"));
        assert!(suggested_worker_type_mismatch_warning(&admitted).is_none());

        // A blanket experiment redirects every role to codex, with no
        // curator-specific override — the exact incident shape (#6201):
        // selection is UNCHANGED (curator still ends up on codex, matching
        // pre-#6201 behavior — this fix does not silently rescue the role),
        // but the mismatch is now observable and loudly warned about.
        write_config(r#"{"runtimes":{"default":"codex"}}"#);
        let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
        assert_eq!(admitted.runtime, "codex");
        assert_eq!(admitted.source, RuntimeSource::DefaultConfig);
        assert_eq!(admitted.suggested_worker_type.as_deref(), Some("claude"));
        let msg =
            suggested_worker_type_mismatch_warning(&admitted).expect("must warn on divergence");
        assert!(msg.contains("curator"), "{msg}");
        assert!(msg.contains("default-config"), "{msg}");

        // A role-targeted override (`runtimes.roles.curator`) is a
        // deliberate, per-role decision — selection still lands on codex,
        // and the warning still fires (an operator's own explicit override
        // is still worth naming loudly), but its wording differs by source.
        write_config(r#"{"runtimes":{"default":"codex","roles":{"curator":"codex"}}}"#);
        let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
        assert_eq!(admitted.runtime, "codex");
        assert_eq!(admitted.source, RuntimeSource::RoleConfig);
        assert_eq!(admitted.suggested_worker_type.as_deref(), Some("claude"));
        assert!(suggested_worker_type_mismatch_warning(&admitted).is_some());

        // judge has no declared suggestion in this fixture (still `"{}"`):
        // no divergence is even representable, so no warning either way.
        let judge_admitted = resolve_and_admit(d.path(), "judge", None).unwrap();
        assert_eq!(judge_admitted.runtime, "codex");
        assert!(suggested_worker_type_mismatch_warning(&judge_admitted).is_none());
    }

    /// `check_runtimes_roles_config` (#5006) must report exactly the same
    /// fail-closed problems `config_runtime` rejects at admission time —
    /// unknown role key, non-string value, non-object shape — so
    /// `loom-daemon validate` can catch a bad `runtimes.roles` before any
    /// role's tick actually runs into it.
    #[test]
    fn check_runtimes_roles_config_reports_unknown_key() {
        let config = serde_json::json!({"runtimes": {"roles": {"buidler": "claude"}}});
        let errors = check_runtimes_roles_config(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("buidler"), "{}", errors[0]);
        assert!(errors[0].contains("unknown role name"), "{}", errors[0]);
    }

    #[test]
    fn check_runtimes_roles_config_reports_non_string_value() {
        let config = serde_json::json!({"runtimes": {"roles": {"curator": true}}});
        let errors = check_runtimes_roles_config(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must be strings"), "{}", errors[0]);
        assert!(errors[0].contains("curator"), "{}", errors[0]);
    }

    #[test]
    fn check_runtimes_roles_config_reports_non_object_shape() {
        let config = serde_json::json!({"runtimes": {"roles": "codex"}});
        let errors = check_runtimes_roles_config(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must be an object"), "{}", errors[0]);
    }

    /// Well-formed or absent `runtimes.roles` produces no findings — no
    /// regression for the common (zero-config, or valid config) case.
    #[test]
    fn check_runtimes_roles_config_is_silent_on_valid_or_absent_config() {
        assert!(check_runtimes_roles_config(&serde_json::json!({})).is_empty());
        assert!(check_runtimes_roles_config(&serde_json::json!({"terminals": []})).is_empty());
        assert!(check_runtimes_roles_config(
            &serde_json::json!({"runtimes": {"default": "codex", "roles": {"curator": "claude"}}})
        )
        .is_empty());
        // Empty-value semantics (an unset per-role binding) are preserved.
        assert!(check_runtimes_roles_config(
            &serde_json::json!({"runtimes": {"roles": {"curator": ""}}})
        )
        .is_empty());
    }

    #[test]
    fn installed_layout_has_identical_resolution_and_rejection() {
        let source = fixture();
        let installed = tempfile::tempdir().unwrap();
        fs::create_dir_all(installed.path().join(".loom")).unwrap();
        for sub in ["roles", "runtimes", "scripts"] {
            fs::rename(
                source.path().join("defaults").join(sub),
                installed.path().join(".loom").join(sub),
            )
            .unwrap();
        }
        let admitted = resolve_and_admit(installed.path(), "judge", Some("codex")).unwrap();
        assert!(admitted
            .role_manifest
            .starts_with(installed.path().join(".loom")));
        let rejected =
            resolve_and_admit(installed.path(), "sweep-lifecycle", Some("codex")).unwrap_err();
        assert_eq!(rejected.unmet_capabilities, vec!["worktreeIsolation"]);
    }

    /// `roots()` must resolve each subdirectory's source independently
    /// (#4688): a workspace with `.loom/roles/` present but `.loom/runtimes/`
    /// absent must still resolve `roles` from `.loom/roles`, not fall through
    /// to `defaults/roles` just because `runtimes` is missing.
    #[test]
    fn roots_resolves_roles_and_runtimes_independently() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".loom/roles")).unwrap();
        fs::create_dir_all(root.path().join("defaults/runtimes")).unwrap();
        fs::create_dir_all(root.path().join("defaults/scripts")).unwrap();
        // `.loom/roles` exists but `.loom/runtimes` and `.loom/scripts` do not.
        let (roles, runtimes, scripts) = roots(root.path());
        assert_eq!(roles, root.path().join(".loom/roles"));
        assert_eq!(runtimes, root.path().join("defaults/runtimes"));
        assert_eq!(scripts, root.path().join("defaults/scripts"));
    }

    /// #4688 regression: a consumer repo can have `.loom/roles/` fully
    /// provisioned while `.loom/runtimes/` is still missing (the exact live
    /// incident layout — a provisioning gap on installs that predate
    /// `defaults/runtimes/`). The per-directory `roots()` fallback resolves
    /// roles from `.loom/roles` even though runtimes falls through, and the
    /// builtin `claude` runtime — whose manifest is absent from BOTH
    /// `.loom/runtimes/` and any `defaults/runtimes/` fallback, since this
    /// fixture has no `defaults/` at all — degrades open rather than
    /// rejecting the dispatch.
    #[test]
    fn consumer_layout_missing_runtimes_dir_still_admits_claude() {
        let root = tempfile::tempdir().unwrap();
        let loom_roles = root.path().join(".loom/roles");
        fs::create_dir_all(&loom_roles).unwrap();
        fs::write(
            loom_roles.join("builder.json"),
            r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
        )
        .unwrap();
        fs::write(loom_roles.join("judge.json"), "{}").unwrap();
        let loom_scripts = root.path().join(".loom/scripts");
        fs::create_dir_all(&loom_scripts).unwrap();
        for name in ["claude", "codex"] {
            let adapter = loom_scripts.join(format!("spawn-{name}.sh"));
            fs::write(&adapter, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        // No `.loom/runtimes/` and no `defaults/` at all — the exact live
        // incident layout.
        assert!(!root.path().join(".loom/runtimes").exists());
        assert!(!root.path().join("defaults").exists());

        let admitted = resolve_and_admit(root.path(), "sweep-lifecycle", None).unwrap();
        assert_eq!(admitted.runtime, "claude");
        // Roles resolved from `.loom/roles` — the per-directory fallback
        // means the absence of `.loom/runtimes/` does NOT drag roles down
        // to `defaults/roles` too.
        assert!(admitted
            .role_manifest
            .starts_with(root.path().join(".loom")));
        // Runtimes independently fall through to `defaults/runtimes` (which
        // does not exist in this fixture either) since `.loom/runtimes/` is
        // absent — and admission still succeeds because the missing
        // manifest degrades open for the builtin runtime instead of
        // rejecting the dispatch.
        assert!(admitted
            .runtime_manifest
            .starts_with(root.path().join("defaults/runtimes")));

        // #5002: a non-builtin runtime the daemon ships an adapter for
        // (codex) ALSO admits in this exact layout — via the bundled
        // fallback manifest compiled into the binary, not the degrade-open
        // path exercised above (which is scoped to the builtin runtime
        // only). `judge` declares no `runtimeRequirements`, so it is
        // satisfied by codex's real bundled capabilities.
        let admitted_codex = resolve_and_admit(root.path(), "judge", Some("codex")).unwrap();
        assert_eq!(admitted_codex.runtime, "codex");
        assert_eq!(admitted_codex.source, RuntimeSource::Explicit);
    }

    /// The degrade-open behaviour is scoped to the builtin `claude` runtime
    /// only, and the #5002 bundled-fallback addition is scoped to the
    /// runtimes the daemon binary actually ships a manifest for. A runtime
    /// with NO on-disk manifest anywhere AND no bundled fallback (simulated
    /// here with an operator-defined custom runtime name the binary was
    /// never built with) must still fail closed — a regression guard
    /// against over-widening either fix beyond its intended scope. It also
    /// pins the #5002 corrected error text: the message must name a path
    /// reachable from THIS repo (`.loom/runtimes/<name>.json`), not the
    /// unreachable `defaults/runtimes/<name>.json` fallback path `roots()`
    /// computed (`defaults/` only exists in the Loom source checkout).
    #[test]
    fn consumer_layout_missing_runtimes_dir_non_builtin_still_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let loom_roles = root.path().join(".loom/roles");
        fs::create_dir_all(&loom_roles).unwrap();
        fs::write(loom_roles.join("judge.json"), "{}").unwrap();
        let loom_scripts = root.path().join(".loom/scripts");
        fs::create_dir_all(&loom_scripts).unwrap();
        for name in ["codex", "acme-runtime"] {
            let adapter = loom_scripts.join(format!("spawn-{name}.sh"));
            fs::write(&adapter, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        // No on-disk manifest anywhere (no `.loom/runtimes/`, no
        // `defaults/`) AND no bundled fallback for this made-up runtime
        // name -- still fails closed exactly as before.
        let rejected = resolve_and_admit(root.path(), "judge", Some("acme-runtime")).unwrap_err();
        assert!(rejected.reason.contains("runtime manifest"), "{}", rejected.reason);
        // #5002: the corrected message names a path reachable from a
        // consumer repo, not the unreachable `defaults/runtimes/...` path.
        assert!(
            rejected.reason.contains(".loom/runtimes/acme-runtime.json"),
            "{}",
            rejected.reason
        );
        assert!(!rejected.reason.contains("defaults/runtimes"), "{}", rejected.reason);

        // #5002: a runtime the daemon DOES ship a bundled manifest for
        // (codex) is no longer stuck here -- it now admits via the bundled
        // fallback instead of failing closed, since the compiled-in
        // manifest is a reachable source of truth even with no on-disk
        // `.loom/runtimes/` at all. This is the fix under test, not a
        // weakening of the fail-closed guarantee asserted above.
        let admitted = resolve_and_admit(root.path(), "judge", Some("codex")).unwrap();
        assert_eq!(admitted.runtime, "codex");
    }

    #[test]
    fn rejection_serialization_is_structured_backward_compatible_and_secret_free() {
        let rejection = RuntimeRejection {
            role: "builder".into(),
            runtime: "codex".into(),
            source: RuntimeSource::RoleConfig,
            unmet_capabilities: vec!["worktreeIsolation".into()],
            reason: "unmet capabilities: worktreeIsolation".into(),
        };
        let json = serde_json::to_string(&rejection).unwrap();
        assert!(json.contains("\"role\":\"builder\""));
        assert!(json.contains("\"source\":\"role-config\""));
        assert!(!json.contains("TOKEN"));
        assert_eq!(serde_json::from_str::<RuntimeRejection>(&json).unwrap(), rejection);
    }
}
