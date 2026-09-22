//! Harness-specific command construction. No provider HTTP client, shell parsing or retries.
use super::{
    credential::{Resolved, Source},
    opencode_version::Major,
    profiles::Selection,
    LaunchError, Options,
};
use std::{
    io::{Seek, SeekFrom, Write as _},
    path::PathBuf,
    process::{Command, Stdio},
};

/// Hand the CLI the expanded prompt through its own stdin rather than as an
/// `argv` element (issue #8506 — Linux's `MAX_ARG_STRLEN` caps a single argv
/// element at 128 KiB, which the judge/curator role expansion routinely
/// exceeds). `tempfile::tempfile()` opens an already-unlinked file (no
/// directory entry ever exists for it on Unix), so the prompt never appears
/// anywhere `ps`/`/proc/<pid>/cmdline` can see and there is nothing left to
/// clean up once the child exits. Writing the whole prompt before `exec`
/// (rather than streaming through a live pipe) is required by `exec()`
/// itself replacing this process image — there is no parent process left to
/// keep writing to a pipe once the harness starts.
fn prompt_stdin(prompt: &str) -> Result<Stdio, LaunchError> {
    let mut file = tempfile::tempfile()
        .map_err(|e| LaunchError::config(format!("cannot create prompt transfer file: {e}")))?;
    file.write_all(prompt.as_bytes())
        .map_err(|e| LaunchError::config(format!("cannot write prompt transfer file: {e}")))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| LaunchError::config(format!("cannot rewind prompt transfer file: {e}")))?;
    Ok(Stdio::from(file))
}

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
        credential: &Resolved,
        prompt: Option<&str>,
        root: &std::path::Path,
        guarded: bool,
    ) -> Result<Command, LaunchError> {
        let bin_key = match self {
            Self::Pi => "LOOM_PI_BIN",
            Self::OpenCode => "LOOM_OPENCODE_BIN",
        };
        let bin = std::env::var_os(bin_key)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.name().into());
        let mut command = Command::new(&bin);
        let cwd = std::env::current_dir().map_err(|e| LaunchError::config(e.to_string()))?;
        command.env("PWD", &cwd);
        command.env("LOOM_NATIVE_WORKER_PID", std::process::id().to_string());
        // Explicit env > API-key pool > nothing; resolved upstream so the
        // secret has exactly one consumer (#8401).
        credential.apply(&mut command);
        if guarded
            && credential.source == Source::None
            && std::env::var_os("LOOM_NATIVE_AUTH_FILE").is_none_or(|value| value.is_empty())
        {
            eprintln!("Loom guarded native launches use isolated auth state; configure profile credentials or an external 0600 LOOM_NATIVE_AUTH_FILE for providers requiring login. Unauthenticated local providers remain supported.");
        }
        let provider = crate::native_tools::provision::ProviderConfig {
            id: &selection.provider,
            options: selection.provider_options.as_ref(),
            definition: selection.provider_definition.as_ref(),
        };
        match self {
            Self::Pi => {
                if !provider.is_empty() {
                    return Err(LaunchError::config(
                        "providerOptions/providerDefinition are only supported by the opencode harness",
                    ));
                }
                if guarded {
                    crate::native_tools::provision::configure(
                        &mut command,
                        root,
                        self.name(),
                        &format!("{}/{}", selection.provider, selection.model),
                        &provider,
                    )
                    .map_err(|e| LaunchError::config(e.to_string()))?;
                }
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
                if prompt.is_some() {
                    // #8506: the prompt is delivered on stdin (below), never as an
                    // argv element — Pi reads the message from stdin when no
                    // positional message/`@file` argument is given, verified
                    // against Pi 0.85.1.
                    command.args(["--print", "--mode", "json"]);
                }
            }
            Self::OpenCode => {
                // Probed before anything is provisioned: the launch `exec`s, so an
                // unsupported CLI (or an unverified guard) can only be refused here.
                let major = super::opencode_version::detect(&bin, guarded)?;
                if prompt.is_some() {
                    command.args(["run", "--format", "json"]);
                    match major {
                        Major::V1 => command.arg("--dir").arg(&cwd),
                        // 2.x has no --dir: the working directory is the one `exec`
                        // inherits (PWD is pinned above). Its `run` attaches to a
                        // shared background service by default, which never sees
                        // this child's credentials, OPENCODE_CONFIG_CONTENT or
                        // isolated config dir, so a private server is unconditional.
                        Major::V2 => command.arg("--standalone").current_dir(&cwd),
                    };
                }
                if guarded {
                    crate::native_tools::provision::configure(
                        &mut command,
                        root,
                        self.name(),
                        &format!("{}/{}", selection.provider, selection.model),
                        &provider,
                    )
                    .map_err(|e| LaunchError::config(e.to_string()))?;
                } else {
                    crate::native_tools::provision::provider_only(&mut command, &provider)
                        .map_err(|e| LaunchError::config(e.to_string()))?;
                }
                let model = format!("{}/{}", selection.provider, selection.model);
                match (major, &selection.effort) {
                    (Major::V1, Some(effort)) => {
                        command.args(["--model", &model, "--variant", effort]);
                    }
                    // 2.x dropped --variant: effort rides on the model as `#<effort>`.
                    (Major::V2, Some(effort)) => {
                        if model.contains('#') || effort.contains('#') {
                            return Err(LaunchError::config(
                                "OpenCode 2.x carries effort as provider/model#effort; a '#' in the model or effort is ambiguous",
                            ));
                        }
                        command.args(["--model", &format!("{model}#{effort}")]);
                    }
                    (_, None) => {
                        command.args(["--model", &model]);
                    }
                }
                if options.skip_permissions {
                    if prompt.is_none() {
                        return Err(LaunchError::config(
                            "OpenCode --auto requires a headless prompt",
                        ));
                    }
                    command.arg("--auto");
                }
                // #8506: no positional message argument — the prompt rides on
                // stdin (below). OpenCode `run` reads the message from stdin
                // when no `message` positional is given, verified against
                // OpenCode 1.18.31.
            }
        }
        if let Some(prompt) = prompt {
            command.stdin(prompt_stdin(prompt)?);
        }
        command.env("LOOM_MODEL", &selection.model);
        Ok(command)
    }
}
