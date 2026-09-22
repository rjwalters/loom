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

/// Kimi's own five reasoning-effort levels for `KIMI_MODEL_THINKING_EFFORT`,
/// read out of `@moonshot-ai/kimi-code` 2.0.2's own bundle
/// (`THINKING_EFFORTS = ["low","medium","high","xhigh","max"]`). No `"off"`:
/// unlike Pi, Kimi's env-family model resolver has no disable value here.
///
/// Validating this **here** is not belt-and-braces. 2.0.2 binds the variable
/// through a `string().optional()` schema field (`thinking.forcedEffort`) and
/// does **not** reject an unrecognised value at startup — an observed
/// `KIMI_MODEL_THINKING_EFFORT=bogus` launch proceeded to contact the provider
/// instead of failing. A typo would therefore burn a whole sweep at whatever
/// the harness silently fell back to, so an unsupported level fails closed
/// (78) at launch construction rather than being passed through.
const KIMI_THINKING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Pi,
    OpenCode,
    Kimi,
}
impl Harness {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "pi" => Some(Self::Pi),
            "opencode" => Some(Self::OpenCode),
            "kimi" => Some(Self::Kimi),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
            Self::Kimi => "kimi",
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
            Self::Kimi => "LOOM_KIMI_BIN",
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
            Self::Kimi => {
                // `providerDefinition` has no Kimi equivalent: there is no
                // "whole provider block" concept, only the two fixed
                // KIMI_MODEL_* fields translated from `providerOptions` below.
                if provider.definition.is_some() {
                    return Err(LaunchError::config(
                        "providerDefinition is not supported by the kimi harness",
                    ));
                }
                // #8562: a guarded launch relocates `KIMI_CODE_HOME` into
                // per-launch private state, so the operator's own
                // `config.toml` — and therefore every `[models.<alias>]`
                // entry and the login store beside it — is not visible to
                // the child. The config-alias route below (`-m <alias>`)
                // cannot resolve under that relocation, so a guarded launch
                // requires the config-free env family instead of silently
                // launching a worker that cannot pick a model.
                if guarded && selection.credential_sources.is_empty() {
                    return Err(LaunchError::config(
                        "a guarded kimi launch requires a model profile with a credentialEnv \
                         mapping (the KIMI_MODEL_* env family): the guarded launch relocates \
                         KIMI_CODE_HOME, so a `providers.kimi` config alias cannot resolve",
                    ));
                }
                // A role-tagged launch must fail closed inside `configure`
                // rather than fall through to Kimi's own unguarded builtin
                // tools. This also catches Curator/Guide/Auditor, none of
                // which require a capability the manifest could gate on,
                // since `guarded` is set from the presence of a role tag,
                // independent of that role's own requirements.
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
                if let Some(effort) = &selection.effort {
                    if !KIMI_THINKING_EFFORTS.contains(&effort.as_str()) {
                        return Err(LaunchError::config(format!(
                            "unsupported Kimi thinking effort {effort:?}; expected one of {}",
                            KIMI_THINKING_EFFORTS.join(", ")
                        )));
                    }
                    command.env("KIMI_MODEL_THINKING_EFFORT", effort);
                }
                // The model profile's own shape decides which of Kimi's two
                // model-selection routes this launch uses (issue #8561):
                // `credential_sources` carries every variable the profile
                // DECLARED in `credentialEnv`, independent of whether it
                // resolved at runtime, so this branches on the profile's
                // shape rather than on today's credential state.
                if selection.credential_sources.is_empty() {
                    // No `credentialEnv` at all: `providers.kimi` names a
                    // `[models.<alias>]` entry in the operator's own
                    // `config.toml`, and Kimi resolves the model/auth from
                    // its own config/login store (the credential ladder's
                    // "no pool" case, `worker_spawn::credential::Source::None`).
                    command.args(["-m", &selection.provider]);
                } else {
                    // A declared `credentialEnv`: the config-free env family.
                    // `credential.apply()` (called above) already injected
                    // the mapped `KIMI_MODEL_API_KEY`; only the two fixed
                    // `providerOptions.kimi` keys need translating here.
                    command.env("KIMI_MODEL_NAME", &selection.model);
                    if let Some(options) = provider.options {
                        for (key, value) in options {
                            let value = value.as_str().ok_or_else(|| {
                                LaunchError::config("providerOptions.kimi values must be strings")
                            })?;
                            match key.as_str() {
                                "providerType" => {
                                    command.env("KIMI_MODEL_PROVIDER_TYPE", value);
                                }
                                "baseUrl" => {
                                    command.env("KIMI_MODEL_BASE_URL", value);
                                }
                                other => {
                                    return Err(LaunchError::config(format!(
                                        "providerOptions.kimi does not support {other:?}; \
                                         supported keys are providerType, baseUrl"
                                    )))
                                }
                            }
                        }
                    }
                }
                // Same reasoning as OPENCODE_DISABLE_AUTOUPDATE=1 in
                // docker/native/Dockerfile: a Loom-managed launch must never
                // silently self-update or phone home mid-run.
                command.env("KIMI_DISABLE_TELEMETRY", "1");
                command.env("KIMI_CODE_NO_AUTO_UPDATE", "1");
                // `-p` implies auto-approval and REJECTS `--yolo`/`--auto`
                // outright, so neither is ever emitted here —
                // `--dangerously-skip-permissions` is therefore a no-op for
                // Kimi, exactly like Pi ignores it (`options.skip_permissions`
                // is never read in this arm). Observed verbatim on 2.0.2:
                //   $ kimi -p hi --yolo
                //   error: Cannot combine --prompt with --yolo.
                //   $ kimi -p hi --auto
                //   error: Cannot combine --prompt with --auto.
                // Forwarding the Loom flag itself is not an option either: it
                // is not a Kimi flag, and commander rejects it with
                // `error: unknown option '--dangerously-skip-permissions'`
                // (exit 1) before the prompt ever runs.
                //
                // #8506: the pinned CLI (2.0.2) has no `--input-format` flag
                // at all — `kimi --help` lists only `--output-format
                // <text|stream-json>` — so there is no stdin-delivery route to
                // prefer over argv, unlike Pi/OpenCode. The prompt travels as
                // `-p`'s own argv value; a large enough expanded role prompt
                // can therefore still hit `E2BIG` here. Finding recorded on
                // #8506; nothing on this CLI's surface can work around it.
                if let Some(prompt) = prompt {
                    command.args(["-p", prompt, "--output-format", "stream-json"]);
                }
                // With no stdin route to use, stdin is pinned to `null` rather
                // than inherited: a daemon-dispatched child must never end up
                // holding the dispatcher's TTY (or an unrelated pipe) on an
                // fd the harness may still decide to read.
                command.stdin(Stdio::null());
            }
        }
        // Pi/OpenCode take the expanded prompt on stdin (#8506); Kimi has no
        // such route and pinned its own stdin to `null` in its arm above.
        if !matches!(self, Self::Kimi) {
            if let Some(prompt) = prompt {
                command.stdin(prompt_stdin(prompt)?);
            }
        }
        command.env("LOOM_MODEL", &selection.model);
        Ok(command)
    }
}
