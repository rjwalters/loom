//! Resolve the credentials a native harness spawn needs (issue #8401).
//!
//! A profile declares a *mapping* — `(source variable, child variable)` pairs
//! (#8421: one for a plain API key, several for Bedrock / Vertex AI). The
//! ladder below runs per pair, with one structural limit: an account file
//! holds exactly one `KEY=value`, so **the pool can supply exactly one
//! variable**. It is consulted only when a single source variable is unset;
//! every other pair is environment-only, exactly as #8421 defined it. The
//! array form of `credentialEnv` is a *required-in-environment* set that
//! `profiles::resolve` has already enforced before this runs, so in practice
//! the pool applies to the single-string form.
//!
//! The ladder, in order:
//!
//! 1. **Explicit environment** — a non-empty `credentialEnv` in the launching
//!    environment always wins. This is the #8363 single-key trial path and it
//!    is deliberately unchanged: an operator exporting `ZAI_API_KEY` for one
//!    run must never be silently overridden by a registered pool.
//! 2. **Pool selection** — otherwise, if this host has registered accounts for
//!    the profile's credential provider, select one
//!    ([`crate::api_keys_pool::select_api_key`]).
//! 3. **Fail closed (78, `EX_CONFIG`)** — when step 2 applied *and* came up
//!    empty, **or when whether it applies could not be determined**. A provider
//!    whose pool directory does not exist is not an error: it means this host
//!    never opted into pool management, and the harness is left to its own
//!    auth store exactly as before. A pool directory that exists but cannot be
//!    read (`EACCES` from a uid-mismatched `0700` dir or bind mount, `EIO`, …)
//!    is the opposite: accounts may be registered — and may all be disabled to
//!    stop spend — so launching on the harness's ambient credential instead
//!    would defeat the operator's intent. That refuses pre-spawn.
//!
//! The chosen account's **name** travels onward (launch record, logs); the
//! value only ever reaches the child process's environment.

use super::{profiles::Selection, LaunchError};
use crate::api_keys_pool;
use std::ffi::OsString;
use std::process::Command;

/// Where the credential for this spawn came from. Serialised into the
/// `LOOM_LAUNCH` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// No `credentialEnv`/`credentialTargets` mapping, or nothing to inject.
    None,
    /// Inherited from the launching environment.
    Env,
    /// Selected from this host's API-key account pool.
    Pool,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Env => "env",
            Self::Pool => "pool",
        }
    }
}

/// The resolved credentials plus their provenance. No `Serialize`; `Debug`
/// redacts.
#[derive(Clone)]
pub struct Resolved {
    /// [`Source::Pool`] when the pool supplied a variable, else [`Source::Env`]
    /// when at least one came from the environment, else [`Source::None`].
    pub source: Source,
    /// Pool provider namespace, when the pool decided.
    pub provider: Option<String>,
    /// Account **name** — never key material. Safe to log.
    pub account: Option<String>,
    /// `(child variable, value)`. `OsString`, as in #8421's pass-through: a
    /// non-UTF-8 value in the launching environment must still reach the child.
    injected: Vec<(String, OsString)>,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let targets: Vec<(&str, &str)> = self
            .injected
            .iter()
            .map(|(target, _)| (target.as_str(), "<redacted>"))
            .collect();
        f.debug_struct("Resolved")
            .field("source", &self.source)
            .field("provider", &self.provider)
            .field("account", &self.account)
            .field("injected", &targets)
            .finish()
    }
}

impl Resolved {
    /// Inject the credentials into the child's environment. This is the only
    /// place a value is used; none ever reaches argv.
    pub fn apply(&self, command: &mut Command) {
        for (target, value) in &self.injected {
            command.env(target, value);
        }
    }
}

/// The pool provider namespace a profile's credential belongs to: an explicit
/// `credentialPool` override when set, else derived from its `credentialEnv`
/// (`ZAI_API_KEY` -> `zai`).
///
/// Deriving from the *source* variable rather than from the harness-facing
/// `providers` entry is what keeps one subscription registered once: the same
/// Z.ai coding plan is `zai` to Pi and `zai-coding-plan` to OpenCode.
///
/// Takes the override as a bare `Option<&str>`, not a whole [`Selection`], so
/// a caller that only has a [`super::profiles::ModelProfile`] in hand — e.g.
/// `spawn-worker profile-check` (issue #8450 item 2), which never builds a
/// full `Selection` — can call it directly.
#[must_use]
pub fn pool_provider(credential_pool: Option<&str>, source_var: &str) -> Option<String> {
    if let Some(explicit) = credential_pool {
        return api_keys_pool::paths::validate_provider(explicit)
            .ok()
            .map(|()| explicit.to_string());
    }
    api_keys_pool::paths::provider_from_credential_env(source_var)
}

/// Run the ladder. See the module docs for the ordering and its rationale.
pub fn resolve(root: &std::path::Path, selection: &Selection) -> Result<Resolved, LaunchError> {
    // Step 1, per pair — byte-for-byte what #8421's harness loop did.
    let mut injected: Vec<(String, OsString)> = Vec::new();
    let mut unset: Vec<(&str, &str)> = Vec::new();
    for (source, target) in &selection.credentials {
        match std::env::var_os(source).filter(|v| !v.is_empty()) {
            Some(value) => injected.push((target.clone(), value)),
            None => unset.push((source, target)),
        }
    }
    let environment_only = |injected: Vec<(String, OsString)>| Resolved {
        source: if injected.is_empty() {
            Source::None
        } else {
            Source::Env
        },
        provider: None,
        account: None,
        injected,
    };

    // Step 2 applies to exactly one unset variable (one account file is one
    // assignment). More than one cannot be pooled; refuse rather than guess
    // which of them an explicit `credentialPool` was meant for.
    let (source_var, target) = match unset[..] {
        [] => return Ok(environment_only(injected)),
        [pair] => pair,
        _ if selection.credential_pool.is_some() => {
            return Err(LaunchError::config(
                "model profile sets credentialPool but leaves more than one credential \
                 variable unset; an API-key account supplies exactly one variable",
            ));
        }
        _ => return Ok(environment_only(injected)),
    };

    let Some(provider) = pool_provider(selection.credential_pool.as_deref(), source_var) else {
        return Ok(environment_only(injected));
    };
    let pooled = api_keys_pool::is_pooled(root, &provider).map_err(|error| {
        LaunchError::config(format!(
            "cannot determine whether provider {provider:?} is pooled on this host (profile \
             credential {source_var}); refusing to launch on the harness's own credential \
             store. Export {source_var} for a one-off run, or fix the pool.\n{error}"
        ))
    })?;
    if !pooled {
        return Ok(environment_only(injected));
    }
    // `source_var` is passed down so an account file assigning a different
    // variable is withheld rather than injected under this profile's target.
    // `selection.model` is passed down so an exhausted allowance for ONE model
    // class does not withhold an account whose other allowance is fine (#8424
    // item 3), and so the per-account concurrency cap (#8424 item 4) is
    // evaluated for the account this spawn would actually take.
    let selected = api_keys_pool::select_api_key_for(
        root,
        &provider,
        Some(source_var),
        Some(&selection.model),
        None,
    )
    .map_err(|error| {
        LaunchError::config(format!(
            "no usable API-key account for provider {provider:?} (profile credential \
             {source_var}). Export {source_var} for a one-off run, or fix the pool.\n{error}"
        ))
    })?;
    // The in-flight lease is deliberately dropped, not released: `worker_spawn::run`
    // `exec`s the harness immediately after this, so this PID *becomes* the run
    // the lease describes and the lease must outlive this handle. It is reaped
    // when that PID dies — see `api_keys_pool::inflight`.
    drop(selected.lease);
    injected.push((target.to_string(), OsString::from(selected.credential.value)));
    Ok(Resolved {
        source: Source::Pool,
        provider: Some(selected.provider),
        account: Some(selected.name),
        injected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-variable profile: `credential_env` mapped to `ZHIPU_API_KEY`.
    fn selection(credential_env: Option<&str>, pool: Option<&str>) -> Selection {
        let pairs: Vec<(&str, &str)> = credential_env
            .iter()
            .map(|source| (*source, "ZHIPU_API_KEY"))
            .collect();
        mapped(&pairs, pool)
    }

    fn mapped(pairs: &[(&str, &str)], pool: Option<&str>) -> Selection {
        Selection {
            provider: "zai-coding-plan".into(),
            model: "glm-5.3-flash".into(),
            effort: None,
            profile: Some("zai-flash".into()),
            credentials: pairs
                .iter()
                .map(|(source, target)| ((*source).to_string(), (*target).to_string()))
                .collect(),
            credential_sources: pairs
                .iter()
                .map(|(source, _)| (*source).to_string())
                .collect(),
            provider_options: None,
            provider_definition: None,
            credential_pool: pool.map(str::to_string),
        }
    }

    fn child_env(resolved: &Resolved) -> Vec<(String, String)> {
        let mut command = Command::new("true");
        resolved.apply(&mut command);
        command
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect()
    }

    #[test]
    fn pool_provider_prefers_the_explicit_override() {
        let var = "ZAI_API_KEY";
        assert_eq!(pool_provider(Some("shared-glm"), var).as_deref(), Some("shared-glm"));
        assert_eq!(pool_provider(None, var).as_deref(), Some("zai"));
        // A malformed override does not silently fall back to the derivation:
        // it disables pooling for the profile rather than pointing somewhere
        // the operator did not name.
        assert_eq!(pool_provider(Some("../etc"), var), None);
        assert_eq!(pool_provider(None, "_API_KEY"), None);
    }

    #[test]
    fn a_profile_without_a_credential_mapping_resolves_to_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve(tmp.path(), &selection(None, None)).unwrap();
        assert_eq!(resolved.source, Source::None);
        assert!(resolved.account.is_none());
    }

    #[test]
    fn an_unpooled_provider_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve(tmp.path(), &selection(Some("LOOM_TEST_UNSET_KEY_8401"), None));
        assert_eq!(resolved.unwrap().source, Source::None);
    }

    #[test]
    fn pool_selection_redacts_the_value_in_debug_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = api_keys_pool::paths::per_repo_api_keys_dir(tmp.path());
        api_keys_pool::registry::add(
            &root,
            "loomtest",
            "alpha",
            "LOOM_TEST_UNSET_KEY_8401",
            "fake-pool-secret",
            false,
        )
        .unwrap();
        let resolved =
            resolve(tmp.path(), &selection(Some("LOOM_TEST_UNSET_KEY_8401"), Some("loomtest")))
                .unwrap();
        assert_eq!(resolved.source, Source::Pool);
        assert_eq!(resolved.account.as_deref(), Some("alpha"));
        let rendered = format!("{resolved:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("fake-pool-secret"), "{rendered}");
    }

    #[test]
    fn an_all_disabled_pool_fails_closed_at_78_without_key_material() {
        let tmp = tempfile::tempdir().unwrap();
        let root = api_keys_pool::paths::per_repo_api_keys_dir(tmp.path());
        api_keys_pool::registry::add(
            &root,
            "loomtest",
            "alpha",
            "LOOM_TEST_UNSET_KEY_8401",
            "fake-pool-secret",
            false,
        )
        .unwrap();
        api_keys_pool::registry::set_enabled(&root, "loomtest", "alpha", false).unwrap();
        let error =
            resolve(tmp.path(), &selection(Some("LOOM_TEST_UNSET_KEY_8401"), Some("loomtest")))
                .unwrap_err();
        assert_eq!(error.code, api_keys_pool::EX_CONFIG);
        assert!(error.message.contains("no usable API-key account"), "{}", error.message);
        assert!(!error.message.contains("fake-pool-secret"), "{}", error.message);
    }

    /// Judge finding 1 (#8428): before the fix this returned `Source::None`
    /// — i.e. "launch, and let the harness use its own auth store".
    #[cfg(unix)]
    #[test]
    fn an_unreadable_pool_fails_closed_at_78_naming_the_path_and_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = api_keys_pool::paths::per_repo_api_keys_dir(tmp.path());
        api_keys_pool::registry::add(
            &root,
            "loomtest",
            "alpha",
            "LOOM_TEST_UNSET_KEY_8401",
            "fake-pool-secret",
            false,
        )
        .unwrap();
        api_keys_pool::registry::set_enabled(&root, "loomtest", "alpha", false).unwrap();
        let dir = api_keys_pool::paths::provider_dir(&root, "loomtest");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let as_root = std::fs::read_dir(&dir).is_ok();
        let resolved =
            resolve(tmp.path(), &selection(Some("LOOM_TEST_UNSET_KEY_8401"), Some("loomtest")));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if as_root {
            return; // permission bits do not apply
        }
        let error = resolved.unwrap_err();
        assert_eq!(error.code, api_keys_pool::EX_CONFIG);
        assert!(error.message.contains(&dir.display().to_string()), "{}", error.message);
        assert!(error.message.contains("PermissionDenied"), "{}", error.message);
        assert!(!error.message.contains("fake-pool-secret"), "{}", error.message);
    }

    // ---- Multi-variable credential mappings (#8421) meeting the pool ----

    /// Pairs present in the environment pass through untouched, and the pool
    /// fills the single variable that is not — injected under *its* target.
    #[test]
    #[serial_test::serial]
    fn environment_pairs_pass_through_and_the_pool_fills_the_single_unset_variable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = api_keys_pool::paths::per_repo_api_keys_dir(tmp.path());
        api_keys_pool::registry::add(
            &root,
            "loomtest",
            "alpha",
            "LOOM_TEST_UNSET_KEY_8401",
            "fake-pool-secret",
            false,
        )
        .unwrap();
        std::env::set_var("LOOM_TEST_REGION_SRC_8401", "fake-region");
        let resolved = resolve(
            tmp.path(),
            &mapped(
                &[
                    ("LOOM_TEST_REGION_SRC_8401", "CHILD_REGION"),
                    ("LOOM_TEST_UNSET_KEY_8401", "CHILD_KEY"),
                ],
                Some("loomtest"),
            ),
        );
        std::env::remove_var("LOOM_TEST_REGION_SRC_8401");
        let resolved = resolved.unwrap();
        assert_eq!(resolved.source, Source::Pool);
        assert_eq!(resolved.account.as_deref(), Some("alpha"));
        let mut env = child_env(&resolved);
        env.sort();
        assert_eq!(
            env,
            vec![
                ("CHILD_KEY".to_string(), "fake-pool-secret".to_string()),
                ("CHILD_REGION".to_string(), "fake-region".to_string()),
            ]
        );
        assert!(!format!("{resolved:?}").contains("fake-"), "{resolved:?}");
    }

    /// With every pair present, the pool is never consulted — even a pool that
    /// would fail closed. Explicit environment wins, per pair.
    #[test]
    #[serial_test::serial]
    fn a_fully_exported_mapping_never_consults_the_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let root = api_keys_pool::paths::per_repo_api_keys_dir(tmp.path());
        api_keys_pool::registry::add(
            &root,
            "loomtest",
            "alpha",
            "LOOM_TEST_SET_KEY_8401",
            "x",
            false,
        )
        .unwrap();
        api_keys_pool::registry::set_enabled(&root, "loomtest", "alpha", false).unwrap();
        std::env::set_var("LOOM_TEST_SET_KEY_8401", "fake-explicit");
        let resolved = resolve(
            tmp.path(),
            &mapped(&[("LOOM_TEST_SET_KEY_8401", "CHILD_KEY")], Some("loomtest")),
        );
        std::env::remove_var("LOOM_TEST_SET_KEY_8401");
        let resolved = resolved.unwrap();
        assert_eq!(resolved.source, Source::Env);
        assert_eq!(child_env(&resolved), vec![("CHILD_KEY".into(), "fake-explicit".into())]);
    }

    /// One account file is one assignment: a pool cannot fill two variables.
    #[test]
    fn a_pool_cannot_supply_more_than_one_variable() {
        let tmp = tempfile::tempdir().unwrap();
        let pairs = [
            ("LOOM_TEST_UNSET_KEY_8401", "CHILD_KEY"),
            ("LOOM_TEST_UNSET_OTHER_8401", "CHILD_OTHER"),
        ];
        let error = resolve(tmp.path(), &mapped(&pairs, Some("loomtest"))).unwrap_err();
        assert_eq!(error.code, api_keys_pool::EX_CONFIG);
        assert!(error.message.contains("exactly one variable"), "{}", error.message);
        // Without an explicit pool it stays what #8421 made it: environment-only.
        let resolved = resolve(tmp.path(), &mapped(&pairs, None)).unwrap();
        assert_eq!(resolved.source, Source::None);
        assert!(child_env(&resolved).is_empty());
    }
}
