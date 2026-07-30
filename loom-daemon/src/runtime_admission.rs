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

fn config_runtime(root: &Path, role: &str) -> (Option<String>, Option<String>) {
    let config = crate::config_resolver::resolve_effective_config(root);
    let roles = crate::config_resolver::get_path(&config, "runtimes.roles");
    let per_role = roles
        .and_then(|v| v.get(role))
        .and_then(serde_json::Value::as_str)
        .and_then(|v| nonempty(Some(v)));
    let default = crate::config_resolver::get_path(&config, "runtimes.default")
        .and_then(serde_json::Value::as_str)
        .and_then(|v| nonempty(Some(v)));
    (per_role, default)
}

fn roots(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let installed = root.join(".loom");
    let defaults = root.join("defaults");
    let manifests = if installed.join("roles").is_dir() && installed.join("runtimes").is_dir() {
        installed.clone()
    } else {
        defaults.clone()
    };
    let scripts = if installed.join("scripts").is_dir() {
        installed
    } else {
        defaults
    };
    (manifests.join("roles"), manifests.join("runtimes"), scripts.join("scripts"))
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
    let (role_config, default_config) = config_runtime(root, canonical);
    let role_env = std::env::var(&env_name).ok();
    let global_env = std::env::var("LOOM_RUNTIME").ok();
    let (runtime, source) = choose_runtime(
        explicit,
        role_env.as_deref(),
        global_env.as_deref(),
        role_config,
        default_config,
    );
    let (roles, runtimes, scripts) = roots(root);
    let role_manifest = roles.join(format!("{lookup_role}.json"));
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
    let runtime_data = fs::read(&runtime_manifest).map_err(|e| {
        reject(format!("runtime manifest {}: {e}", runtime_manifest.display()), vec![])
    })?;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    /// One matrix drives both implementations: every shipped role/runtime
    /// pair is evaluated natively and by the installed shell checker, and the
    /// decisions must be identical. This prevents two independent assertion
    /// lists from silently drifting.
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
            for runtime in &runtimes {
                let native = resolve_and_admit(repo, &role, Some(runtime));
                // Roles outside the daemon's canonical launch set are not
                // schedulable here; the shell checker remains a generic tool.
                if canonical_role(&role).is_none() {
                    continue;
                }
                let shell_role = if canonical_role(&role) == Some("sweep-lifecycle") {
                    "builder"
                } else {
                    &role
                };
                let shell = Command::new("bash")
                    .arg(&checker)
                    .args(["--role", shell_role, "--runtime", runtime, "--dir"])
                    .arg(&defaults)
                    .status()
                    .unwrap();
                assert_eq!(
                    native.is_ok(),
                    shell.success(),
                    "native/shell admission drift for role={role} runtime={runtime}: native={native:?}, shell={shell}"
                );
            }
        }
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
