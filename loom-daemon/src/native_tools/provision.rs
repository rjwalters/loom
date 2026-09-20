//! Launch-time bindings are binary-owned; no edit to global harness configuration.
use anyhow::{Context, Result};
use serde_json::json;
use std::{fs, io::Write, path::Path, process::Command};

pub fn configure(command: &mut Command, root: &Path, runtime: &str, model: &str) -> Result<()> {
    super::guard::ready(root)?;
    let binary = std::env::current_exe()?;
    let directory = root.join(".loom/native-tools");
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
        let mut config: serde_json::Value = match std::env::var("OPENCODE_CONFIG_CONTENT") {
            Ok(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw)
                .context("OPENCODE_CONFIG_CONTENT must be a JSON object")?,
            _ => json!({}),
        };
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
