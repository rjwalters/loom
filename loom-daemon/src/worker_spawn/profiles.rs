//! Model/provider choices are data; harness adapters only translate the launch protocol.
use super::{LaunchError, Options};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// One variable (the original form) or the whole set a provider requires.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CredentialEnv {
    One(String),
    Many(Vec<String>),
}
impl CredentialEnv {
    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::One(name) => vec![name.as_str()],
            Self::Many(names) => names.iter().map(String::as_str).collect(),
        }
    }
    /// The array form declares a *required* set and fails closed when one is
    /// unset. The single-string form stays optional, so profiles that rely on a
    /// harness's own login store keep launching exactly as before.
    fn required(&self) -> bool {
        matches!(self, Self::Many(_))
    }
}

/// Alias: the one declared variable under a harness's own name. Map: an explicit
/// source-variable to child-variable mapping, for multi-variable providers
/// (Bedrock, Vertex AI).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CredentialTargets {
    Alias(String),
    Map(BTreeMap<String, String>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProfile {
    pub model: String,
    /// Provider IDs are harness vocabulary (Pi `zai`, OpenCode `zai-coding-plan`).
    pub providers: BTreeMap<String, String>,
    pub effort: Option<String>,
    /// Names only. Secrets remain in the inherited environment, this host's
    /// API-key account pool (`.loom/api-keys/`, #8401), or the CLI auth store.
    pub credential_env: Option<CredentialEnv>,
    /// Pool namespace this profile's credential belongs to. Optional: it
    /// defaults to the slug derived from `credential_env` (`ZAI_API_KEY` ->
    /// `zai`). Set it explicitly when two profiles must share one pool, or
    /// when the derivation would pick a misleading name.
    pub credential_pool: Option<String>,
    #[serde(default)]
    pub credential_targets: BTreeMap<String, CredentialTargets>,
    /// Per-profile opt-in to credential substitution (#8674): the contained
    /// worker receives a per-launch placeholder and its provider traffic is
    /// routed through a host-side proxy that swaps in the real credential.
    /// Absent (the default) means the credential is forwarded into the
    /// container's environment exactly as before.
    pub credential_proxy: Option<super::egress_proxy::ProfileProxy>,
    /// Non-secret provider options (region, project) merged into the harness's
    /// per-launch injected configuration under `provider.<id>.options`.
    #[serde(default)]
    pub provider_options: BTreeMap<String, Value>,
    /// A whole provider block (npm package, baseURL, model map) declared in the
    /// injected configuration under `provider.<id>`, for endpoints the harness
    /// does not know natively.
    #[serde(default)]
    pub provider_definition: BTreeMap<String, Value>,
    #[serde(default)]
    pub allowed_efforts: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileConfig {
    default_model_profile: Option<String>,
    #[serde(default)]
    model_profiles: BTreeMap<String, ModelProfile>,
}

#[derive(Debug)]
pub struct Selection {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub profile: Option<String>,
    /// (source variable in the launching environment, variable set on the child).
    pub credentials: Vec<(String, String)>,
    /// Every variable the profile declares in `credentialEnv`, whether or not a
    /// `credentialTargets` entry renames it. Containment (#8403) forwards these
    /// by NAME into the container, where profile resolution re-runs against the
    /// container's own environment: an unmapped-but-required source dropped
    /// here would fail that inner resolution closed (exit 78).
    pub credential_sources: Vec<String>,
    pub provider_options: Option<Map<String, Value>>,
    pub provider_definition: Option<Map<String, Value>>,
    /// API-key pool namespace override (#8401); see [`ModelProfile::credential_pool`].
    pub credential_pool: Option<String>,
    /// Validated `credentialProxy` block (#8674), when the profile declares one.
    pub credential_proxy: Option<super::egress_proxy::ProfileProxy>,
}

fn check_name(value: &str) -> Result<(), LaunchError> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Ok(());
    }
    // Never echo the offending string: a mistaken value must not reach a log.
    Err(LaunchError::config(
        "credential mapping must contain environment variable names, not values",
    ))
}

pub fn bundled() -> BTreeMap<String, ModelProfile> {
    serde_json::from_str(include_str!("../../../defaults/model-profiles.json"))
        .expect("bundled profiles")
}

/// Resolve a profile by name, falling back to configuration's default and then
/// the bundled set. Configuration profiles shadow bundled ones of the same name.
pub fn lookup(name: Option<&str>, config: &Value) -> Result<(String, ModelProfile), LaunchError> {
    let configured: ProfileConfig = serde_json::from_value(
        config
            .get("runtimes")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map_err(|_| LaunchError::config("invalid runtime model profile configuration"))?;
    let name = name
        .or(configured.default_model_profile.as_deref())
        .unwrap_or("zai-flash")
        .to_string();
    let profile = configured
        .model_profiles
        .get(&name)
        .cloned()
        .or_else(|| bundled().remove(&name))
        .ok_or_else(|| LaunchError::config("unknown model profile"))?;
    if profile.model.trim().is_empty()
        || profile.effort.as_ref().is_some_and(|s| s.trim().is_empty())
    {
        return Err(LaunchError::config("model profile has an empty model or effort"));
    }
    Ok((name, profile))
}

/// One harness's credential mapping: (source variable, child variable) pairs,
/// plus the variables that must be present before launch.
#[derive(Debug, Default)]
pub struct CredentialMapping {
    pub pairs: Vec<(String, String)>,
    pub required: Vec<String>,
}

/// Resolve that mapping. Names are validated; values are never read here.
pub fn credentials(
    profile: &ModelProfile,
    runtime: &str,
) -> Result<CredentialMapping, LaunchError> {
    let Some(declared) = &profile.credential_env else {
        if profile.credential_targets.contains_key(runtime) {
            return Err(LaunchError::config(
                "credentialTargets requires credentialEnv to declare the source variables",
            ));
        }
        return Ok(CredentialMapping::default());
    };
    let sources = declared.names();
    for name in &sources {
        check_name(name)?;
    }
    let pairs = match profile.credential_targets.get(runtime) {
        // No explicit target for this harness. A single-string credentialEnv
        // still has an implicit target: the harness reads the source variable
        // under its own name (Pi reads ZAI_API_KEY as ZAI_API_KEY) — exactly
        // what unset-variable inheritance would have delivered had the value
        // been exported instead of pooled. Keying the pool decision on
        // "does a rename pair exist" silently skipped the pool for every
        // profile that omits an explicit target; treating it as (VAR, VAR)
        // keeps this in the same one-variable ladder as the pool. The array
        // form is a required-in-environment set with no single implicit
        // target, so it stays unmapped here.
        None => match declared {
            CredentialEnv::One(name) => vec![(name.clone(), name.clone())],
            CredentialEnv::Many(_) => Vec::new(),
        },
        Some(CredentialTargets::Alias(target)) => {
            check_name(target)?;
            if sources.len() != 1 {
                return Err(LaunchError::config("a single credentialTargets name needs exactly one credentialEnv variable; use a variable map"));
            }
            vec![(sources[0].to_string(), target.clone())]
        }
        Some(CredentialTargets::Map(map)) => {
            let mut pairs = Vec::new();
            for (source, target) in map {
                check_name(source)?;
                check_name(target)?;
                if !sources.contains(&source.as_str()) {
                    return Err(LaunchError::config(
                        "credentialTargets maps a variable that credentialEnv does not declare",
                    ));
                }
                pairs.push((source.clone(), target.clone()));
            }
            pairs
        }
    };
    let required = if declared.required() {
        sources.iter().map(|s| (*s).to_string()).collect()
    } else {
        Vec::new()
    };
    Ok(CredentialMapping { pairs, required })
}

/// Required variables that are unset or empty in the launching environment.
pub fn missing(required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|name| std::env::var_os(name.as_str()).is_none_or(|value| value.is_empty()))
        .cloned()
        .collect()
}

fn provider_block(
    field: &BTreeMap<String, Value>,
    runtime: &str,
    what: &str,
) -> Result<Option<Map<String, Value>>, LaunchError> {
    match field.get(runtime) {
        None => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map.clone())),
        Some(_) => Err(LaunchError::config(format!("{what} must be a JSON object"))),
    }
}

/// Provider configuration is data the harness reads, so a credential VALUE in it
/// would be written into the injected configuration string. Reject that: the
/// harness's own `{env:VAR}` indirection resolves against the mapped child
/// environment instead.
fn reject_literal_secrets(
    blocks: &[&Map<String, Value>],
    names: &[&str],
) -> Result<(), LaunchError> {
    if blocks.is_empty() {
        return Ok(());
    }
    let text = serde_json::to_string(blocks).unwrap_or_default();
    for name in names {
        // Short values collide with ordinary words; a real credential is long.
        if let Some(value) = std::env::var(name).ok().filter(|v| v.len() >= 8) {
            if text.contains(&value) {
                return Err(LaunchError::config(format!(
                    "provider configuration embeds the value of {name}; reference it as {{env:{name}}} instead"
                )));
            }
        }
    }
    Ok(())
}

/// Everything a harness needs from a profile, resolved and fail-closed.
pub fn resolve(
    runtime: &str,
    name: &str,
    profile: &ModelProfile,
) -> Result<Selection, LaunchError> {
    let provider = profile
        .providers
        .get(runtime)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            LaunchError::config("model profile has no provider binding for this harness")
        })?
        .clone();
    let mapping = credentials(profile, runtime)?;
    if let Some(pool) = &profile.credential_pool {
        crate::api_keys_pool::paths::validate_provider(pool).map_err(|_| {
            LaunchError::config("model profile credentialPool is not a valid pool name")
        })?;
        if matches!(profile.credential_env, Some(CredentialEnv::Many(_))) {
            return Err(LaunchError::config(
                "model profile credentialPool applies only to a single-string credentialEnv; \
                 an array-form credentialEnv declares a required set of variables that one \
                 pooled account (a single KEY=value) cannot satisfy",
            ));
        }
    }
    if let Some(proxy) = &profile.credential_proxy {
        // Fail at selection, not at dispatch: a malformed upstream is a
        // misconfiguration that must never reach a launch that then decides
        // what to do about it.
        proxy.validate()?;
        // Both counts must be 1, not just `pairs`: an array-form
        // `credentialEnv` with a `credentialTargets` map that only covers
        // one of its declared names still produces `pairs.len() == 1`, but
        // `credential_sources` (the full declared set, #8437) carries the
        // rest forward for by-name forwarding — and nothing downstream
        // withholds a name outside the mapped pair. Refuse that shape here
        // rather than proxy one variable while forwarding the others in the
        // clear.
        let declared_count = profile
            .credential_env
            .as_ref()
            .map(|env| env.names().len())
            .unwrap_or(0);
        if mapping.pairs.len() != 1 || declared_count != 1 {
            return Err(LaunchError::config(
                "model profile credentialProxy requires exactly one credential variable; a \
                 multi-variable provider has no single value to substitute, and every \
                 credentialEnv variable it declares must be part of that single mapped pair",
            ));
        }
    }
    let unset = missing(&mapping.required);
    if !unset.is_empty() {
        return Err(LaunchError::config(format!(
            "model profile '{name}' requires environment variables that are unset: {}",
            unset.join(", ")
        )));
    }
    let provider_options = provider_block(&profile.provider_options, runtime, "providerOptions")?;
    let provider_definition =
        provider_block(&profile.provider_definition, runtime, "providerDefinition")?;
    let blocks: Vec<&Map<String, Value>> = provider_options
        .iter()
        .chain(provider_definition.iter())
        .collect();
    let declared = profile
        .credential_env
        .as_ref()
        .map(CredentialEnv::names)
        .unwrap_or_default();
    reject_literal_secrets(&blocks, &declared)?;
    Ok(Selection {
        provider,
        model: profile.model.clone(),
        effort: profile.effort.clone(),
        profile: Some(name.to_string()),
        credentials: mapping.pairs,
        credential_sources: declared.iter().map(|s| (*s).to_string()).collect(),
        provider_options,
        provider_definition,
        credential_pool: profile.credential_pool.clone(),
        credential_proxy: profile.credential_proxy.clone(),
    })
}

pub fn select(runtime: &str, options: &Options, config: &Value) -> Result<Selection, LaunchError> {
    // A qualified explicit model is deliberately independent of the default profile.
    if options.profile.is_none() {
        if let Some((provider, model)) = options.model.as_deref().and_then(|m| m.split_once('/')) {
            if provider.is_empty() || model.is_empty() {
                return Err(LaunchError::config("model must be provider/model"));
            }
            return Ok(Selection {
                provider: provider.into(),
                model: model.into(),
                effort: options.effort.clone(),
                profile: None,
                credentials: Vec::new(),
                credential_sources: Vec::new(),
                provider_options: None,
                provider_definition: None,
                credential_pool: None,
                credential_proxy: None,
            });
        }
    }
    let (name, profile) = lookup(options.profile.as_deref(), config)?;
    let mut selection = resolve(runtime, &name, &profile)?;
    if let Some(model) = options.model.clone() {
        if model != profile.model {
            return Err(LaunchError::config("bare model differs from the selected profile; use provider/model or define a model profile"));
        }
    }
    if let Some(effort) = options.effort.clone() {
        selection.effort = Some(effort);
    }
    if let Some(e) = &selection.effort {
        if !profile.allowed_efforts.is_empty() && !profile.allowed_efforts.contains(e) {
            return Err(LaunchError::config(
                "reasoning effort is unsupported by the selected model profile",
            ));
        }
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env mutation across these two tests — `resolve` reads
    /// `missing()` against the process environment for a `Many` (required)
    /// `credentialEnv`.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn profile_with_credentials(
        credential_env: CredentialEnv,
        credential_targets: BTreeMap<String, CredentialTargets>,
    ) -> ModelProfile {
        ModelProfile {
            model: "test-model".to_string(),
            providers: BTreeMap::from([("test-runtime".to_string(), "test-provider".to_string())]),
            effort: None,
            credential_env: Some(credential_env),
            credential_targets,
            credential_pool: None,
            credential_proxy: None,
            provider_options: BTreeMap::new(),
            provider_definition: BTreeMap::new(),
            allowed_efforts: Vec::new(),
        }
    }

    /// Regression test for #8454: #8421 replaced the old
    /// `credential_env`/`credential_target` pair with `credentials` (the
    /// mapped pairs only), which silently drops every declared source that
    /// has no `credentialTargets` entry — including the DEFAULT bundled
    /// `zai-flash` profile, whose `ZAI_API_KEY` has no target at all. #8437's
    /// `credential_sources` fixes that by carrying every declared name
    /// forward regardless of mapping, but nothing pinned it: a future
    /// `Selection` refactor that keeps `credentials` and drops
    /// `credential_sources` would compile clean and silently reintroduce this
    /// bug. This asserts both fields directly off `resolve()`, not off a
    /// hand-built fixture, so it fails the moment that wiring breaks.
    #[test]
    fn credential_sources_includes_every_declared_variable_even_when_unmapped() {
        let _guard = env_lock();
        // A `Many` (array-form) `credentialEnv` is a required set (see
        // `CredentialEnv::required`) — `resolve()` fails closed if either is
        // unset, so both must be present for this to reach the assertions.
        std::env::set_var("LOOM_TEST_8454_CRED_A", "value-a");
        std::env::set_var("LOOM_TEST_8454_CRED_B", "value-b");

        let credential_targets = BTreeMap::from([(
            "test-runtime".to_string(),
            CredentialTargets::Map(BTreeMap::from([(
                "LOOM_TEST_8454_CRED_A".to_string(),
                "MAPPED_TARGET".to_string(),
            )])),
        )]);
        let profile = profile_with_credentials(
            CredentialEnv::Many(vec![
                "LOOM_TEST_8454_CRED_A".to_string(),
                "LOOM_TEST_8454_CRED_B".to_string(),
            ]),
            credential_targets,
        );

        let selection = resolve("test-runtime", "test-profile", &profile);

        std::env::remove_var("LOOM_TEST_8454_CRED_A");
        std::env::remove_var("LOOM_TEST_8454_CRED_B");

        let selection = selection.expect("both required sources are set");
        // Every declared source is carried forward, mapped or not — this is
        // the field containment (#8437) chains ahead of `credentials` so an
        // unmapped-but-required source still reaches the container.
        assert_eq!(
            selection.credential_sources,
            vec!["LOOM_TEST_8454_CRED_A", "LOOM_TEST_8454_CRED_B"]
        );
        // Only the mapped source produces a (source, target) pair.
        assert_eq!(
            selection.credentials,
            vec![("LOOM_TEST_8454_CRED_A".to_string(), "MAPPED_TARGET".to_string())]
        );
    }

    /// Same regression, for the string form with no `credentialTargets` at
    /// all — the exact shape of the bundled `zai-flash` profile the issue
    /// names (`"credentialEnv": "ZAI_API_KEY"`, no `credentialTargets` key).
    #[test]
    fn credential_sources_string_form_with_no_targets_forwards_by_name_only() {
        // The single-string form is optional (`CredentialEnv::required` is
        // false for `One`), so `resolve()` does not fail closed on an unset
        // variable here and no env mutation is needed.
        let profile = profile_with_credentials(
            CredentialEnv::One("ZAI_API_KEY".to_string()),
            BTreeMap::new(),
        );

        let selection = resolve("test-runtime", "zai-flash", &profile)
            .expect("optional credentialEnv resolves");

        assert_eq!(selection.credential_sources, vec!["ZAI_API_KEY"]);
        // PR #8428's pool-bypass fix: no explicit target still means an
        // implicit (VAR, VAR) pair, the same name inheritance would have
        // used — otherwise `credential::resolve` never consults the pool for
        // an untargeted profile (it sees zero pairs and returns early).
        assert_eq!(
            selection.credentials,
            vec![("ZAI_API_KEY".to_string(), "ZAI_API_KEY".to_string())]
        );
    }

    /// Regression for the Judge's blocking finding on #8701: an array-form
    /// `credentialEnv` with a `credentialTargets` map that only covers ONE
    /// of its declared names produces exactly one `credentials` pair — the
    /// old `mapping.pairs.len() != 1` guard alone let this through — while
    /// `credential_sources` still carries the unmapped variable forward for
    /// by-name forwarding, which `credentialProxy`'s "exactly one variable"
    /// promise does not withhold. `resolve()` must refuse this shape, not
    /// just the two-pairs-mapped shape the pre-existing test covered.
    #[test]
    fn credential_proxy_refuses_a_declared_but_unmapped_variable() {
        let _guard = env_lock();
        std::env::set_var("LOOM_TEST_8701_CRED_A", "value-a");
        std::env::set_var("LOOM_TEST_8701_CRED_B", "value-b");

        let credential_targets = BTreeMap::from([(
            "test-runtime".to_string(),
            CredentialTargets::Map(BTreeMap::from([(
                "LOOM_TEST_8701_CRED_A".to_string(),
                "MAPPED_TARGET".to_string(),
            )])),
        )]);
        let mut profile = profile_with_credentials(
            CredentialEnv::Many(vec![
                "LOOM_TEST_8701_CRED_A".to_string(),
                "LOOM_TEST_8701_CRED_B".to_string(),
            ]),
            credential_targets,
        );
        profile.credential_proxy = Some(super::super::egress_proxy::ProfileProxy {
            upstream: "https://api.anthropic.com".to_string(),
            header: super::super::egress_proxy::HeaderStyle::AuthorizationBearer,
            base_url_env: vec!["ANTHROPIC_BASE_URL".to_string()],
        });

        let error = resolve("test-runtime", "test-profile", &profile);

        std::env::remove_var("LOOM_TEST_8701_CRED_A");
        std::env::remove_var("LOOM_TEST_8701_CRED_B");

        let error = error.expect_err("a declared-but-unmapped variable must be refused");
        assert!(error.message.contains("every credentialEnv variable"), "{}", error.message);
    }
}
