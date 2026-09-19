//! Model/provider choices are data; harness adapters only translate the launch protocol.
use super::{LaunchError, Options};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProfile {
    pub model: String,
    /// Provider IDs are harness vocabulary (Pi `zai`, OpenCode `zai-coding-plan`).
    pub providers: BTreeMap<String, String>,
    pub effort: Option<String>,
    /// Name only. Secrets remain in the inherited environment or CLI auth store.
    pub credential_env: Option<String>,
    #[serde(default)]
    pub credential_targets: BTreeMap<String, String>,
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
    pub credential_env: Option<String>,
    pub credential_target: Option<String>,
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
                credential_env: None,
                credential_target: None,
            });
        }
    }
    let configured: ProfileConfig = serde_json::from_value(
        config
            .get("runtimes")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map_err(|_| LaunchError::config("invalid runtime model profile configuration"))?;
    let name = options
        .profile
        .as_deref()
        .or(configured.default_model_profile.as_deref())
        .unwrap_or("zai-flash");
    let bundled: BTreeMap<String, ModelProfile> =
        serde_json::from_str(include_str!("../../../defaults/model-profiles.json"))
            .expect("bundled profiles");
    let profile = configured
        .model_profiles
        .get(name)
        .or_else(|| bundled.get(name))
        .ok_or_else(|| LaunchError::config("unknown model profile"))?
        .clone();
    if profile.model.trim().is_empty()
        || profile.effort.as_ref().is_some_and(|s| s.trim().is_empty())
    {
        return Err(LaunchError::config("model profile has an empty model or effort"));
    }
    for name in profile
        .credential_env
        .iter()
        .chain(profile.credential_targets.values())
    {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(LaunchError::config(
                "credential mapping must contain environment variable names, not values",
            ));
        }
    }
    let provider = profile
        .providers
        .get(runtime)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            LaunchError::config("model profile has no provider binding for this harness")
        })?
        .clone();
    let model = options.model.clone().unwrap_or(profile.model.clone());
    if model != profile.model {
        return Err(LaunchError::config("bare model differs from the selected profile; use provider/model or define a model profile"));
    }
    let effort = options.effort.clone().or(profile.effort);
    if let Some(e) = &effort {
        if !profile.allowed_efforts.is_empty() && !profile.allowed_efforts.contains(e) {
            return Err(LaunchError::config(
                "reasoning effort is unsupported by the selected model profile",
            ));
        }
    }
    Ok(Selection {
        provider,
        model,
        effort,
        profile: Some(name.into()),
        credential_env: profile.credential_env,
        credential_target: profile.credential_targets.get(runtime).cloned(),
    })
}
