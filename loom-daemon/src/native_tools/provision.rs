//! Launch-time bindings are binary-owned; no edit to global harness configuration.
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::{fs, io::Write, path::Path, process::Command};

mod state;

pub use state::{outside_every_repository, private_directory};

/// The exact pinned plugin manifest a guarded OpenCode launch provisions.
///
/// Exposed as a constant so the readiness measurement in
/// [`crate::native_readiness`] keys its package cache on the same bytes
/// production writes (#8581). A pin that drifts between the two would produce a
/// cache entry that is valid for a package set no launch ever uses.
pub const OPENCODE_PLUGIN_MANIFEST: &str =
    r#"{"private":true,"dependencies":{"@opencode-ai/plugin":"1.18.31"}}"#;

/// Write the guarded OpenCode bindings — the plugin source and the pinned
/// package manifest — into `config_dir`.
///
/// Extracted from [`configure`] so a measurement can provision exactly what a
/// launch provisions, rather than a hand-copied approximation of it.
///
/// # Errors
///
/// Propagates any filesystem failure, including a plugins directory that
/// cannot be made 0700-private.
pub fn write_opencode_bindings(config_dir: &Path) -> Result<()> {
    state::private_directory(&config_dir.join("plugins"))?;
    write_binding(&config_dir.join("plugins/loom.ts"), include_str!("opencode.mjs"))?;
    write_binding(&config_dir.join("package.json"), OPENCODE_PLUGIN_MANIFEST)
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
    // Issue #8561/#8562: Kimi has no guarded `loom_*` tool binding yet — no
    // extension, no plugin, nothing to write. Fail closed here, before ANY
    // provisioning (`super::guard::ready` included) runs, rather than let a
    // role-tagged launch fall through to Kimi's own unguarded builtin tools.
    // `harness::Harness::Kimi::command` is the only caller that reaches this
    // with `runtime == "kimi"`, and only when the launch is role-tagged
    // (`guarded`) — an ordinary free-form trial never calls `configure` at
    // all.
    if runtime == "kimi" {
        anyhow::bail!(
            "Kimi has no guarded loom_* tool binding yet; a role-tagged launch would otherwise \
             run with Kimi's own unguarded builtin tools instead of Loom's worktree/workflow/ \
             destructive policies. Tracked in issue #8562. Unguarded free-form trials (no \
             --role tag and no /loom:<role> prompt) remain supported."
        );
    }
    super::guard::ready(root)?;
    let binary = std::env::current_exe()?;
    let state = state::prepare(root)?;
    state.configure(command, runtime)?;
    let directory = &state.directory;
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
        write_opencode_bindings(&config_dir)?;
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
