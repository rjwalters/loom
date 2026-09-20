//! Launch-time bindings are binary-owned; no edit to global harness configuration.
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::{fs, io::Write, path::Path, path::PathBuf, process::Command};

/// Where the launch-time bindings (and OpenCode's config dir, which carries
/// its `auth.json` and plugin install) are written.
///
/// Default: `<workspace>/.loom/native-tools`, machine-local ignored state.
/// `LOOM_NATIVE_TOOLS_DIR` relocates it — set by the native-ephemeral
/// containment profile (issue #8403) to a per-launch path inside the
/// container's own ephemeral writable layer, so N concurrent native workers
/// on one host cannot share one session store or one `auth.json`. The
/// workspace default is deliberately NOT usable for that: it lives under the
/// parity-mounted repo root, which every worker on the host shares.
fn bindings_dir(root: &Path) -> PathBuf {
    std::env::var_os("LOOM_NATIVE_TOOLS_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".loom/native-tools"))
}

/// Profile-declared, non-secret provider data merged into the per-launch config.
/// Secrets never appear here: OpenCode resolves its own `{env:VAR}` indirection
/// against the child environment the profile's credential mapping populates.
#[derive(Default)]
pub struct ProviderConfig<'a> {
    pub id: &'a str,
    pub options: Option<&'a Map<String, Value>>,
    pub definition: Option<&'a Map<String, Value>>,
}
impl ProviderConfig<'_> {
    pub fn is_empty(&self) -> bool {
        self.options.is_none() && self.definition.is_none()
    }
}

fn inherited_config() -> Result<Value> {
    match std::env::var("OPENCODE_CONFIG_CONTENT") {
        Ok(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).context("OPENCODE_CONFIG_CONTENT must be a JSON object")
        }
        _ => Ok(json!({})),
    }
}

fn object_at<'a>(object: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let entry = object.entry(key).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().expect("object entry")
}

fn merge_provider(object: &mut Map<String, Value>, provider: &ProviderConfig) {
    if provider.is_empty() {
        return;
    }
    let providers = object_at(object, "provider");
    let entry = object_at(providers, provider.id);
    for (key, value) in provider.definition.into_iter().flatten() {
        entry.insert(key.clone(), value.clone());
    }
    if let Some(options) = provider.options {
        let target = object_at(entry, "options");
        for (key, value) in options {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// An unguarded trial still needs the profile's provider block: a cloud or
/// custom endpoint is not present in the operator's own OpenCode configuration.
pub fn provider_only(command: &mut Command, provider: &ProviderConfig) -> Result<()> {
    if provider.is_empty() {
        return Ok(());
    }
    let mut config = inherited_config()?;
    let object = config
        .as_object_mut()
        .context("OPENCODE_CONFIG_CONTENT must be a JSON object")?;
    merge_provider(object, provider);
    command.env("OPENCODE_CONFIG_CONTENT", config.to_string());
    Ok(())
}

pub fn configure(
    command: &mut Command,
    root: &Path,
    runtime: &str,
    model: &str,
    provider: &ProviderConfig,
) -> Result<()> {
    super::guard::ready(root)?;
    let binary = std::env::current_exe()?;
    let directory = bindings_dir(root);
    fs::create_dir_all(&directory).context("cannot provision native tool bindings")?;
    command
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_NATIVE_TOOL_BIN", &binary);
    command.env("LOOM_DAEMON_SELF_BIN", &binary);
    command.env("LOOM_NATIVE_GUARD_DIR", super::guard::directory(root));
    // Both startup defaults and unknown tool names fail closed if a binding
    // fails to load. Extensions do not merely intercept enabled native tools.
    if runtime == "pi" {
        let extension = directory.join("pi.ts");
        write_binding(&extension, include_str!("pi.ts"))?;
        command
            .args(["--no-builtin-tools", "--no-extensions", "--extension"])
            .arg(extension);
    } else {
        let config_dir = directory.join("opencode");
        fs::create_dir_all(config_dir.join("plugins"))?;
        write_binding(&config_dir.join("plugins/loom.ts"), include_str!("opencode.mjs"))?;
        write_binding(
            &config_dir.join("package.json"),
            r#"{"private":true,"dependencies":{"@opencode-ai/plugin":"1.18.31"}}"#,
        )?;
        command.env("OPENCODE_CONFIG_DIR", &config_dir);
        let mut config = inherited_config()?;
        let object = config
            .as_object_mut()
            .context("OPENCODE_CONFIG_CONTENT must be a JSON object")?;
        let guarded = json!({
            "model":model,"small_model":model,
            "default_agent":"loom-worker",
            "agent":{"loom-worker":{"model":model,"mode":"primary","description":"Loom guarded issue worker","permission":{"*":"deny","loom_read":"allow","loom_edit":"allow","loom_write":"allow","loom_bash":"allow"}}},
            "permission":{"*":"deny","loom_read":"allow","loom_edit":"allow","loom_write":"allow","loom_bash":"allow"},
            "tools":{"*":false,"loom_read":true,"loom_edit":true,"loom_write":true,"loom_bash":true}
        });
        object.extend(guarded.as_object().expect("guarded config object").clone());
        merge_provider(object, provider);
        command.env("OPENCODE_CONFIG_CONTENT", config.to_string());
        command.args(["--agent", "loom-worker"]);
    }
    Ok(())
}

fn write_binding(path: &Path, content: &str) -> Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().context("binding has no parent")?)?;
    temp.write_all(content.as_bytes())?;
    temp.persist(path)?;
    Ok(())
}
