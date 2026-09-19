//! Harness-specific command construction. No provider HTTP client, shell parsing or retries.
use super::{profiles::Selection, LaunchError, Options};
use std::{path::PathBuf, process::Command};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Pi,
    OpenCode,
}
impl Harness {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "pi" => Some(Self::Pi),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
        }
    }
    pub fn command(
        self,
        options: &Options,
        selection: &Selection,
        prompt: Option<&str>,
    ) -> Result<Command, LaunchError> {
        let bin_key = match self {
            Self::Pi => "LOOM_PI_BIN",
            Self::OpenCode => "LOOM_OPENCODE_BIN",
        };
        let bin = std::env::var_os(bin_key)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.name().into());
        let mut command = Command::new(bin);
        let cwd = std::env::current_dir().map_err(|e| LaunchError::config(e.to_string()))?;
        command.env("PWD", &cwd);
        if let (Some(source), Some(target)) =
            (&selection.credential_env, &selection.credential_target)
        {
            if let Some(value) = std::env::var_os(source).filter(|v| !v.is_empty()) {
                command.env(target, value);
            }
        }
        match self {
            Self::Pi => {
                command.args([
                    "--provider",
                    &selection.provider,
                    "--model",
                    &selection.model,
                ]);
                if let Some(effort) = &selection.effort {
                    if !["off", "minimal", "low", "medium", "high", "xhigh", "max"]
                        .contains(&effort.as_str())
                    {
                        return Err(LaunchError::config("unsupported Pi thinking level"));
                    }
                    command.args(["--thinking", effort]);
                }
                if let Some(prompt) = prompt {
                    command.args(["--print", "--mode", "json", "--", prompt]);
                }
            }
            Self::OpenCode => {
                if prompt.is_some() {
                    command
                        .args(["run", "--format", "json"])
                        .arg("--dir")
                        .arg(&cwd);
                }
                command.args([
                    "--model",
                    &format!("{}/{}", selection.provider, selection.model),
                ]);
                if let Some(effort) = &selection.effort {
                    command.args(["--variant", effort]);
                }
                if options.skip_permissions {
                    if prompt.is_none() {
                        return Err(LaunchError::config(
                            "OpenCode --auto requires a headless prompt",
                        ));
                    }
                    command.arg("--auto");
                }
                if let Some(prompt) = prompt {
                    command.args(["--", prompt]);
                }
            }
        }
        if prompt.is_some() {
            command.stdin(std::process::Stdio::null());
        }
        command.env("LOOM_MODEL", &selection.model);
        Ok(command)
    }
}
