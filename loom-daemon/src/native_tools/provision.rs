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

/// Whether a **live guarded canary receipt** exists for Kimi Code CLI.
///
/// This is evidence tracking, not a setting — the same contract
/// `opencode_version::Major::guard_verified` establishes for OpenCode
/// majors (`.loom/docs/guardrail-parity-native.md` § "OpenCode major
/// versions"). There is deliberately no configuration key and no
/// environment override: the binding below is complete and unit-tested
/// against shapes read out of the pinned CLI's own bundle, but a fixture
/// test can only prove Loom *writes* that configuration, never that a real
/// CLI *honours* it. Only a live canary — including the
/// deliberately-broken-binding case, whose pass condition is "no file
/// written and no unguarded tool used", not the exit code — distinguishes
/// "failed closed" from "fell open".
///
/// Flipped to `true` by the change that also lands the dated receipt in
/// `.loom/docs/guardrail-parity-native.md` and flips
/// `defaults/runtimes/kimi.json`'s `worktreeIsolation`/`loomControl`.
pub const KIMI_GUARD_VERIFIED: bool = false;

pub fn configure(
    command: &mut Command,
    root: &Path,
    runtime: &str,
    model: &str,
    provider: &ProviderConfig,
) -> Result<()> {
    // Issue #8562: the Kimi binding below has no live canary receipt yet.
    // Fail closed here, before ANY provisioning (`super::guard::ready`
    // included) runs, rather than let a role-tagged launch reach a binding
    // no live run has confirmed the CLI honours.
    // `harness::Harness::Kimi::command` is the only caller that reaches this
    // with `runtime == "kimi"`, and only when the launch is role-tagged
    // (`guarded`) — an ordinary free-form trial never calls `configure` at
    // all.
    if runtime == "kimi" && !KIMI_GUARD_VERIFIED {
        anyhow::bail!(
            "Kimi's guarded loom_* tool binding has no live canary receipt yet, so a role-tagged \
             launch is refused rather than run against an unverified boundary. Tracked in issue \
             #8562; see .loom/docs/guardrail-parity-native.md for the receipt this waits on. \
             Unguarded free-form trials (no --role tag and no /loom:<role> prompt) remain \
             supported."
        );
    }
    super::guard::ready(root)?;
    // #8707: the path every guarded `loom_*` tool call in the native session
    // executes. A raw `current_exe()` yields the unlinked inode's ` (deleted)`
    // path once `auto_update` stages a replacement, so a session admitted
    // mid-roll would launch with a tool binary that does not exist — the
    // backstop admitted but unusable. `daemon_bin_resolve` returns the same
    // path in the ordinary case and the on-disk replacement mid-roll.
    let binary = crate::daemon_bin_resolve::resolve_daemon_bin().map_err(anyhow::Error::msg)?;
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
    } else if runtime == "kimi" {
        write_kimi_bindings(command, root, directory, &binary)?;
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

/// Relocate `KIMI_CODE_HOME` into this launch's private state directory and
/// write the three generated bindings into it (#8562).
///
/// Relocating the whole home — rather than dropping a `.kimi-code/` overlay
/// into the worktree — is what makes this per-launch and unreachable from
/// repository content: `[tools]` and `[[hooks]]` have no project-level file
/// at all, so an overlay could not carry the allowlist, and anything written
/// inside the worktree is content the model may edit or commit.
fn write_kimi_bindings(
    command: &mut Command,
    root: &Path,
    directory: &Path,
    binary: &Path,
) -> Result<()> {
    let home = directory.join("kimi");
    state::private_directory(&home)?;
    let cwd = std::env::current_dir().context("cannot resolve the launch working directory")?;
    let server = super::kimi::server_name(directory);
    let hook = super::kimi::hook_command(binary, root, &cwd)?;
    write_binding(&home.join("config.toml"), &super::kimi::config_toml(&server, &hook))?;
    write_binding(
        &home.join("mcp.json"),
        &super::kimi::mcp_json(&server, binary, root, &cwd, &super::guard::directory(root))?,
    )?;
    let agent = home.join("loom-worker.md");
    write_binding(&agent, &super::kimi::agent_file(&server))?;
    command
        .env("KIMI_CODE_HOME", &home)
        .arg("--agent-file")
        .arg(agent);
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
