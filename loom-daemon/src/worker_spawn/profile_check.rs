//! Report a model profile's resolvability without spawning anything: which
//! harness it binds, which variables it maps, and which of them are unset.
//!
//! For an unset variable that a pooled provider could supply, this also
//! reports the pool's namespace and selectable count, and whether a real
//! spawn would refuse at exit 78 — the same ladder
//! [`super::credential::resolve`] runs at spawn time, consulted read-only
//! (secret-free `list`/`health`, never [`crate::api_keys_pool::registry::read_credential`]).
//! Before this, a pooled profile always printed `status: resolvable`
//! regardless of whether the pool had anything selectable (issue #8450 item
//! 2): the overall per-harness `required`-variable check this file already
//! ran never looked at the pool at all.
use super::{credential, profiles, LaunchError};
use std::fmt::Write;
use std::path::Path;

#[derive(clap::Args)]
pub struct WorkerArgs {
    #[command(subcommand)]
    command: WorkerCommand,
}

#[derive(clap::Subcommand)]
enum WorkerCommand {
    /// Report whether a model profile can be resolved here. Reads no secrets,
    /// contacts no provider and never launches a worker. Exits 78 when a bound
    /// harness is unresolvable (unknown profile, unmapped harness, unset vars).
    ProfileCheck {
        /// Profile name. Defaults to the configured/bundled default profile.
        #[arg(value_name = "NAME")]
        name: Option<String>,
        /// Restrict the report to one harness (`pi`, `opencode`).
        #[arg(long, value_name = "HARNESS")]
        runtime: Option<String>,
    },

    /// Measure guarded native startup boundaries with a provider-free probe
    /// (#8581). Makes no model call, reads no credential, contacts no forge,
    /// and never retries anything for money.
    ///
    /// Prints one JSON timing report distinguishing the boundaries it measured
    /// (binary probe, binding provisioning, package resolution, provider-free
    /// readiness) from the ones no provider-free probe can observe (first
    /// provider event, first tool, completion) — which are reported as
    /// unknown-with-a-reason rather than estimated.
    Readiness(crate::native_readiness::measure::ReadinessArgs),

    /// Run a command (normally a `docker run`) behind the credential egress
    /// proxy (#8697): the real credential in `--credential-env` is swapped for
    /// a per-launch placeholder in the command's environment, and a host-side
    /// listener substitutes it back on the way to `--upstream`. Used by
    /// `spawn-claude.sh`'s contained dispatch; fails closed (exit 78).
    ProxyExec(super::egress_proxy::exec::ExecArgs),
}

fn report(name: Option<&str>, runtime: Option<&str>) -> Result<(String, bool), LaunchError> {
    let root = super::workspace(None)?;
    let config = crate::config_resolver::resolve_effective_config(&root);
    let (name, profile) = profiles::lookup(name, &config)?;
    let mut out = String::new();
    let _ = writeln!(out, "profile: {name}");
    let _ = writeln!(out, "model: {}", profile.model);
    if let Some(effort) = &profile.effort {
        let _ = writeln!(out, "effort: {effort}");
    }
    let harnesses: Vec<&String> = profile
        .providers
        .keys()
        .filter(|h| runtime.is_none_or(|r| r == h.as_str()))
        .collect();
    if harnesses.is_empty() {
        return Err(LaunchError::config("model profile has no provider binding for this harness"));
    }
    let mut resolvable = true;
    for harness in harnesses {
        let _ = writeln!(out, "harness {harness}: provider {}", profile.providers[harness]);
        let mapping = profiles::credentials(&profile, harness)?;
        let mut pool_would_refuse = false;
        for (source, target) in &mapping.pairs {
            let is_set = std::env::var_os(source).is_some_and(|v| !v.is_empty());
            let mut state = if is_set {
                "present".to_string()
            } else {
                "unset".to_string()
            };
            if !is_set {
                if let Some(status) = pool_status(&root, &profile, source) {
                    if status.would_refuse() {
                        pool_would_refuse = true;
                    }
                    let _ = write!(state, "; {status}");
                }
            }
            let _ = writeln!(out, "  credential {source} -> {target} ({state})");
        }
        for key in ["providerOptions", "providerDefinition"] {
            let field = if key == "providerOptions" {
                &profile.provider_options
            } else {
                &profile.provider_definition
            };
            if let Some(block) = field.get(harness).and_then(serde_json::Value::as_object) {
                let keys: Vec<&str> = block.keys().map(String::as_str).collect();
                let _ = writeln!(out, "  {key}: {}", keys.join(", "));
            }
        }
        let unset = profiles::missing(&mapping.required);
        if !unset.is_empty() {
            resolvable = false;
            let _ = writeln!(out, "  status: unresolvable, set {}", unset.join(", "));
        } else if pool_would_refuse {
            resolvable = false;
            let _ = writeln!(
                out,
                "  status: unresolvable, spawn would refuse at 78 (no selectable pool account)"
            );
        } else {
            let _ = writeln!(out, "  status: resolvable");
        }
    }
    Ok((out, resolvable))
}

/// Whether an unset `source` credential variable would be supplied by this
/// host's API-key pool — namespace and selectable count, secret-free.
/// `None` only when nothing derives a namespace at all (an invalid explicit
/// `credentialPool`, or a variable name with no recognizable provider stem),
/// in which case the caller's existing unset/required-variable handling is
/// unchanged. A namespace that derives but holds no accounts is reported as
/// [`PoolStatus::Unregistered`] rather than `None` (#8711): a bundled
/// quick-tap preset on a host with nothing registered yet must still NAME the
/// namespace the operator has to run `api-keys add` against, which is the one
/// thing a bare `unset` never told them.
fn pool_status(root: &Path, profile: &profiles::ModelProfile, source: &str) -> Option<PoolStatus> {
    let provider = credential::pool_provider(profile.credential_pool.as_deref(), source)?;
    // Secret-free by construction ([`crate::api_keys_pool::ProviderHealth`]
    // cannot carry key material); mirrors exactly what `api-keys health` would
    // report for this provider.
    let snapshot = crate::api_keys_pool::health(root, Some(&provider)).ok()?;
    let entry = snapshot.into_iter().next()?;
    if let Some(problem) = entry.unreadable {
        return Some(PoolStatus::Unreadable { provider, problem });
    }
    if entry.total == 0 {
        // Not opted into pool management on this host — the harness's own
        // credential store applies instead, exactly as `credential::resolve`
        // treats a missing pool directory. Reported, not refused: the launch
        // this describes still proceeds.
        return Some(PoolStatus::Unregistered { provider });
    }
    Some(PoolStatus::Pooled {
        provider,
        selectable: entry.selectable,
        total: entry.total,
    })
}

/// What [`pool_status`] found. [`std::fmt::Display`] is the exact text
/// appended to a credential line's state.
enum PoolStatus {
    Pooled {
        provider: String,
        selectable: usize,
        total: usize,
    },
    /// The namespace derives but this host has registered nothing in it. Not
    /// a refusal — `credential::resolve` falls through to the harness's own
    /// credential store exactly as if no pool existed.
    Unregistered {
        provider: String,
    },
    Unreadable {
        provider: String,
        problem: String,
    },
}

impl PoolStatus {
    /// `true` when a real spawn would find a usable account here — i.e. would
    /// NOT hit `credential::resolve`'s fail-closed exit 78.
    fn would_supply(&self) -> bool {
        matches!(self, Self::Pooled { selectable, .. } if *selectable > 0)
    }

    /// `true` when a real spawn would fail closed at 78 here. Deliberately not
    /// `!would_supply()`: an [`Self::Unregistered`] namespace supplies nothing
    /// AND refuses nothing — the launch proceeds on the harness's own auth
    /// store, so reporting it must not flip the profile to unresolvable.
    fn would_refuse(&self) -> bool {
        match self {
            Self::Unregistered { .. } => false,
            other => !other.would_supply(),
        }
    }
}

impl std::fmt::Display for PoolStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pooled {
                provider,
                selectable,
                total,
            } => write!(
                f,
                "pool {provider}: {selectable}/{total} selectable{}",
                if *selectable == 0 {
                    " — spawn would refuse at 78"
                } else {
                    ""
                }
            ),
            Self::Unregistered { provider } => write!(
                f,
                "pool {provider}: no accounts registered here — harness auth store \
                 applies; add one with `loom-daemon api-keys add {provider} <account>`"
            ),
            Self::Unreadable { provider, problem } => {
                write!(f, "pool {provider}: UNREADABLE — {problem} — spawn would refuse at 78")
            }
        }
    }
}

pub fn cli(args: WorkerArgs) -> anyhow::Result<()> {
    let (name, runtime) = match args.command {
        WorkerCommand::ProfileCheck { name, runtime } => (name, runtime),
        WorkerCommand::Readiness(args) => return args.run(),
        WorkerCommand::ProxyExec(args) => return super::egress_proxy::exec::cli(args),
    };
    match report(name.as_deref(), runtime.as_deref()) {
        Ok((text, resolvable)) => {
            print!("{text}");
            if !resolvable {
                std::process::exit(78);
            }
        }
        Err(error) => {
            eprintln!("{}", error.message);
            std::process::exit(error.code);
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_keys_pool::{paths, registry};

    fn profile(credential_pool: Option<&str>) -> profiles::ModelProfile {
        profiles::ModelProfile {
            model: "glm-5.3-flash".into(),
            providers: std::collections::BTreeMap::new(),
            effort: None,
            credential_env: None,
            credential_pool: credential_pool.map(str::to_string),
            credential_proxy: None,
            credential_targets: std::collections::BTreeMap::new(),
            provider_options: std::collections::BTreeMap::new(),
            provider_definition: std::collections::BTreeMap::new(),
            allowed_efforts: Vec::new(),
        }
    }

    /// Issue #8450 item 2, positive case: an unset pooled variable reports the
    /// pool's namespace and a true selectable count.
    #[test]
    fn pool_status_reports_the_namespace_and_selectable_count() {
        let workspace = tempfile::tempdir().unwrap();
        let root = paths::per_repo_api_keys_dir(workspace.path());
        registry::add(&root, "zai", "alpha", "ZAI_API_KEY", "fake-secret", false).unwrap();
        registry::add(&root, "zai", "beta", "ZAI_API_KEY", "fake-secret-2", false).unwrap();
        registry::set_enabled(&root, "zai", "beta", false).unwrap();
        let status = pool_status(workspace.path(), &profile(None), "ZAI_API_KEY").unwrap();
        assert!(status.would_supply());
        let text = status.to_string();
        assert!(text.contains("pool zai: 1/2 selectable"), "{text}");
        assert!(!text.contains("would refuse"), "{text}");
    }

    /// Issue #8450 item 2: when every account is disabled the report must say
    /// a spawn would refuse at 78, not just "unset" the way it did before.
    #[test]
    fn pool_status_flags_a_spawn_that_would_refuse_at_78() {
        let workspace = tempfile::tempdir().unwrap();
        let root = paths::per_repo_api_keys_dir(workspace.path());
        registry::add(&root, "zai", "alpha", "ZAI_API_KEY", "fake-secret", false).unwrap();
        registry::set_enabled(&root, "zai", "alpha", false).unwrap();
        let status = pool_status(workspace.path(), &profile(None), "ZAI_API_KEY").unwrap();
        assert!(!status.would_supply());
        assert!(status.to_string().contains("spawn would refuse at 78"));
    }

    /// A provider with no registered accounts is not pooled at all on this
    /// host — `credential::resolve` leaves it to the harness's own auth store,
    /// so this must NOT read as a refusal. It still names the namespace
    /// (#8711), which is what makes a bundled quick-tap preset self-describing
    /// on a host that has registered nothing yet.
    #[test]
    fn pool_status_names_an_unregistered_namespace_without_refusing() {
        let workspace = tempfile::tempdir().unwrap();
        let status = pool_status(workspace.path(), &profile(None), "ZAI_API_KEY").unwrap();
        assert!(!status.would_supply());
        assert!(!status.would_refuse(), "an unregistered pool must not refuse a launch");
        let text = status.to_string();
        assert!(text.contains("pool zai: no accounts registered here"), "{text}");
        assert!(text.contains("api-keys add zai"), "{text}");
    }

    /// The namespace an unregistered quick-tap preset names is the one
    /// `api-keys add` takes, derived from the profile's own `credentialEnv`.
    #[test]
    fn pool_status_names_the_quick_tap_presets_own_namespaces() {
        let workspace = tempfile::tempdir().unwrap();
        for (variable, namespace) in [
            ("CEREBRAS_API_KEY", "cerebras"),
            ("GEMINI_API_KEY", "gemini"),
        ] {
            let status = pool_status(workspace.path(), &profile(None), variable).unwrap();
            assert!(!status.would_refuse());
            assert!(status.to_string().contains(&format!("pool {namespace}:")), "{status}");
        }
    }

    /// Nothing derives a namespace from this variable, so the caller's
    /// ordinary unset-variable handling applies unchanged.
    #[test]
    fn pool_status_is_none_when_no_namespace_derives() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(pool_status(workspace.path(), &profile(None), "_API_KEY").is_none());
    }

    /// An explicit `credentialPool` override is honoured over the derivation
    /// from `credentialEnv`, exactly as `credential::pool_provider` does.
    #[test]
    fn pool_status_honours_an_explicit_credential_pool_override() {
        let workspace = tempfile::tempdir().unwrap();
        let root = paths::per_repo_api_keys_dir(workspace.path());
        registry::add(&root, "zai-metered", "alpha", "ZAI_API_KEY", "fake-secret", false).unwrap();
        let status =
            pool_status(workspace.path(), &profile(Some("zai-metered")), "ZAI_API_KEY").unwrap();
        assert!(
            matches!(status, PoolStatus::Pooled { ref provider, .. } if provider == "zai-metered")
        );
    }

    /// Issue #8450 item 2, unreadable case: a pool root that cannot be read
    /// must not be reported as "not pooled" — same fail-closed distinction
    /// the spawn path (`credential::resolve`) already makes.
    #[cfg(unix)]
    #[test]
    fn pool_status_reports_an_unreadable_provider_directory() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let root = paths::per_repo_api_keys_dir(workspace.path());
        registry::add(&root, "zai", "alpha", "ZAI_API_KEY", "fake-secret", false).unwrap();
        let dir = paths::provider_dir(&root, "zai");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_as_root = std::fs::read_dir(&dir).is_ok();
        let status = pool_status(workspace.path(), &profile(None), "ZAI_API_KEY");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_as_root {
            return; // permission bits do not apply to this uid
        }
        let status = status.unwrap();
        assert!(!status.would_supply());
        assert!(matches!(status, PoolStatus::Unreadable { .. }));
        assert!(status.to_string().contains("UNREADABLE"));
    }
}
