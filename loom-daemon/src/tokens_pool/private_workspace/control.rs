//! Supported v1 control surface: local tools in the owned clone only.
//! Config sources: https://learn.chatgpt.com/docs/config-file/config-basic
//! and https://learn.chatgpt.com/docs/config-file/config-advanced (2026-09-24).
use super::*;

fn audit_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 65536 {
        bail!("private Codex configuration must be a regular file below 64 KiB");
    }
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
    audit_value(&value)
}

fn audit_value(value: &toml::Value) -> Result<()> {
    if let Some(table) = value.as_table() {
        for (key, child) in table {
            if [
                "mcp_servers",
                "plugins",
                "marketplaces",
                "config_file",
                "profile",
                "profiles",
            ]
            .contains(&key.as_str())
                && child.as_table().is_none_or(|t| !t.is_empty())
            {
                bail!("private-clone v1 refuses MCP/plugin or additional profile/agent configuration; host control endpoints cannot be forwarded; configuration was preserved");
            }
            audit_value(child)?;
        }
    }
    Ok(())
}

pub(super) fn audit() -> Result<()> {
    for path in [
        Path::new(PROFILE).join("config.toml"),
        Path::new(REPO).join(".codex/config.toml"),
        PathBuf::from("/workspace/.codex/config.toml"),
        PathBuf::from("/.codex/config.toml"),
        PathBuf::from("/etc/codex/config.toml"),
    ] {
        audit_file(&path)?;
    }
    // Separate named profiles are documented config layers. Audit every such
    // file, and refuse profile selectors on argv, rather than guessing which
    // additional layer a newer CLI would activate.
    for entry in std::fs::read_dir(PROFILE)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(".config.toml")
        {
            audit_file(&entry.path())?;
        }
    }
    Ok(())
}

pub(super) fn audit_args(command: &[String]) -> Result<()> {
    let mut args = command.iter().skip(1);
    while let Some(arg) = args.next() {
        if matches!(
            arg.as_str(),
            "--profile" | "-p" | "--cd" | "-C" | "--add-dir" | "--remote" | "--remote-executor"
        ) || [
            "--profile=",
            "--cd=",
            "--add-dir=",
            "--remote=",
            "--remote-executor=",
        ]
        .iter()
        .any(|p| arg.starts_with(p))
            || (arg.starts_with("-C") && arg.len() > 2)
            || (arg.starts_with("-p") && !arg.starts_with("--"))
        {
            bail!("private-clone v1 refuses host path, remote executor and alternate profile arguments");
        }
        let config = if matches!(arg.as_str(), "-c" | "--config") {
            Some(args.next().context("missing config override")?.as_str())
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c="))
                .or_else(|| arg.strip_prefix("-c").filter(|s| !s.is_empty()))
        };
        if let Some(config) = config {
            let key = config.split('=').next().unwrap_or("");
            if !["model", "model_reasoning_effort"].contains(&key) {
                bail!("private-clone v1 only permits model/effort CLI configuration overrides");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_project_profile_agent_and_cli_control_routes_without_rewriting_files() {
        for text in [
            "[mcp_servers.host]\nurl='http://host/control'",
            "[agents.worker]\nconfig_file='/host/agent.toml'",
            "[profiles.other.mcp_servers.host]\ncommand='ssh'",
            "[plugins.host]\nenabled=true",
        ] {
            assert!(audit_value(&toml::from_str::<toml::Value>(text).unwrap()).is_err());
        }
        assert!(audit_value(
            &toml::from_str::<toml::Value>(
                "model='fixture'\n[hooks.state.test]\ntrusted_hash='synthetic'"
            )
            .unwrap()
        )
        .is_ok());
        for args in [
            vec!["codex", "exec", "--profile", "host"],
            vec!["codex", "exec", "-c", "mcp_servers.host.url='http://host'"],
        ] {
            assert!(audit_args(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }
}
