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
    /// The ordered runtime preference list picked this runtime (Issue #8436):
    /// `runtimes.rolePreference.<role>` or `runtimes.preference`, resolved at
    /// dispatch time against live credential availability. Distinct from
    /// [`Self::Explicit`] on purpose — an explicit per-dispatch runtime is an
    /// operator pin that disables fall-through, whereas this value means the
    /// walk chose among several candidates and recorded why it passed over
    /// the ones above.
    Preference,
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
    ("pi", include_str!("../../defaults/runtimes/pi.json")),
    ("opencode", include_str!("../../defaults/runtimes/opencode.json")),
    ("kimi", include_str!("../../defaults/runtimes/kimi.json")),
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

/// Shared runtime selection, without capability or executable admission.
/// The worker seam and scheduler must resolve the same per-role binding.
pub fn resolve_binding(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
) -> Result<(String, RuntimeSource), RuntimeRejection> {
    let canonical = canonical_role(role).ok_or_else(|| RuntimeRejection {
        role: role.into(),
        runtime: nonempty(explicit).unwrap_or_else(|| BUILTIN_RUNTIME.into()),
        source: RuntimeSource::Explicit,
        unmet_capabilities: vec![],
        reason: "unknown role".into(),
    })?;
    let (role_config, default_config) =
        config_runtime(root, canonical).map_err(|reason| RuntimeRejection {
            role: canonical.into(),
            runtime: nonempty(explicit).unwrap_or_else(|| BUILTIN_RUNTIME.into()),
            source: RuntimeSource::RoleConfig,
            unmet_capabilities: vec![],
            reason,
        })?;
    let env_name = format!("LOOM_RUNTIME_{}", canonical.replace('-', "_").to_ascii_uppercase());
    let role_env = std::env::var(env_name).ok();
    let global_env = std::env::var("LOOM_RUNTIME").ok();
    Ok(choose_runtime(
        explicit,
        role_env.as_deref(),
        global_env.as_deref(),
        role_config,
        default_config,
    ))
}

pub fn resolve_and_admit(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
) -> Result<ResolvedRuntime, RuntimeRejection> {
    resolve_and_admit_with(root, role, explicit, crate::daemon_bin_resolve::resolve_daemon_bin)
}

/// Testable core of [`resolve_and_admit`]: identical except the native
/// adapter's resolution is injected, so tests can simulate a mid-`auto_update`
/// roll (#8707 — `current_exe()` reporting the unlinked inode's
/// `... (deleted)` path) without replacing the running test binary.
fn resolve_and_admit_with(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
    resolve_native_adapter: impl FnOnce() -> Result<PathBuf, String>,
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
    let (roles, runtimes, scripts) = roots(root);
    let role_manifest = roles.join(format!("{lookup_role}.json"));
    let role_suggested = role_suggested_worker_type(&role_manifest);
    let (runtime, source) = resolve_binding(root, canonical, explicit)?;
    let runtime_manifest = runtimes.join(format!("{runtime}.json"));
    let reject = |reason: String, unmet_capabilities: Vec<String>| RuntimeRejection {
        role: canonical.to_string(),
        runtime: runtime.clone(),
        source: source.clone(),
        unmet_capabilities,
        reason,
    };
    let adapter = if crate::worker_spawn::is_native(&runtime) {
        // Native adapters are compiled into the deciding executable, not shell files.
        // #8707: resolve through `daemon_bin_resolve` — the deleted-inode-surviving
        // resolver #6471 added for self-spawned helper probes — so a mid-`auto_update`
        // roll (new binary staged, drain-restart not yet landed) admits against the
        // on-disk replacement instead of rejecting every native dispatch on the raw
        // ` (deleted)`-suffixed `current_exe()` for the whole roll window.
        match resolve_native_adapter() {
            Ok(path) => path,
            Err(reason) => return Err(reject(reason, vec![])),
        }
    } else {
        scripts.join(format!("spawn-{runtime}.sh"))
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
    if canonical == "sweep-lifecycle" && crate::worker_spawn::is_native(&runtime) {
        for phase in ["curator", "judge", "doctor"] {
            resolve_and_admit(root, phase, Some(&runtime)).map_err(|error| {
                reject(
                    format!("native sweep phase {phase}: {}", error.reason),
                    error.unmet_capabilities,
                )
            })?;
        }
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
mod tests;
