//! `loom-daemon worker proxy-exec` — the credential egress proxy for a
//! container that a **shell adapter** builds (issue #8697, follow-up to #8674).
//!
//! Claude's per-sweep container is not built by [`super::super::containment`]:
//! `spawn-claude.sh` re-execs itself inside a `docker run` (#7429/#7430), and a
//! shell script has nowhere to host a listener whose lifetime must be exactly
//! the container's. So the adapter does what it already does — select the
//! account on the host, assemble the `docker run` argv — and then, instead of
//! `exec docker run …`, runs
//!
//! ```text
//! loom-daemon worker proxy-exec --credential-env CLAUDE_CODE_OAUTH_TOKEN \
//!     --upstream https://api.anthropic.com --base-url-env ANTHROPIC_BASE_URL \
//!     -- docker run … -e CLAUDE_CODE_OAUTH_TOKEN -e ANTHROPIC_BASE_URL … <image> …
//! ```
//!
//! This process reads the REAL credential out of its own environment, binds
//! the listener, registers a per-launch placeholder, and spawns the command
//! with the credential variable **overwritten** by the placeholder and each
//! `--base-url-env` variable set to the proxy's URL. The adapter forwards
//! those names by NAME (`-e VAR`), so docker reads the placeholder — never the
//! real value — out of the environment this process handed it.
//!
//! Everything downstream of [`super::arm`] is the native path's code: the same
//! registry, the same listener, the same [`super::run_with_proxy`] lifetime
//! (placeholder closed the moment the child exits).
//!
//! # Fail closed
//!
//! Every problem is an exit-78 refusal, never a launch with the real value:
//! an unset/empty credential variable, a value that is already a placeholder
//! (a nested proxy would forward a placeholder upstream as if it were a key),
//! a malformed upstream, a variable name that is not a variable name, or a
//! listener that cannot bind. The adapter, for its part, refuses to fall back
//! to a plain `docker run` when this subcommand is missing from the resolved
//! binary.

use super::{arm, run_with_proxy, HeaderStyle, Prepared, ProfileProxy, PLACEHOLDER_PREFIX};
use crate::worker_spawn::LaunchError;
use std::ffi::OsString;
use std::process::Command;

/// Arguments for `loom-daemon worker proxy-exec`.
#[derive(clap::Args, Debug)]
pub struct ExecArgs {
    /// Environment variable that holds the REAL credential in this process's
    /// environment. The command sees a per-launch placeholder under the same
    /// name instead.
    #[arg(long, value_name = "VAR")]
    pub credential_env: String,
    /// The one origin the credential may be sent to
    /// (e.g. `https://api.anthropic.com`).
    #[arg(long, value_name = "URL")]
    pub upstream: String,
    /// How the real credential is presented upstream:
    /// `authorization-bearer` (default) or `x-api-key`.
    #[arg(long, value_name = "STYLE", default_value = "authorization-bearer", value_parser = parse_header)]
    pub header: HeaderStyle,
    /// Environment variable set, in the command's environment, to the proxy's
    /// base URL. Repeatable.
    #[arg(long = "base-url-env", value_name = "VAR")]
    pub base_url_env: Vec<String>,
    /// Attribution label for the launch record (never a secret).
    #[arg(long, value_name = "NAME", default_value = "claude")]
    pub provider: String,
    /// The command to run under the proxy — normally a `docker run`. Use `--`
    /// before it.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required = true,
        value_name = "COMMAND"
    )]
    pub command: Vec<OsString>,
}

fn parse_header(raw: &str) -> Result<HeaderStyle, String> {
    match raw.trim() {
        "authorization-bearer" => Ok(HeaderStyle::AuthorizationBearer),
        "x-api-key" => Ok(HeaderStyle::XApiKey),
        _ => Err("expected authorization-bearer or x-api-key".to_string()),
    }
}

fn is_env_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Validate, arm the proxy, and build the command with the placeholder
/// substituted into its environment. Split from [`cli`] so the substitution is
/// testable without spawning anything.
///
/// `env` is this process's environment (passed in, so tests need not mutate
/// process-global state).
pub(crate) fn build(
    args: &ExecArgs,
    env: &[(OsString, OsString)],
) -> Result<(Prepared, Command), LaunchError> {
    if !is_env_name(&args.credential_env) {
        return Err(LaunchError::config(
            "proxy-exec: --credential-env must be an environment variable name",
        ));
    }
    if args.base_url_env.iter().any(|n| n == &args.credential_env) {
        return Err(LaunchError::config(
            "proxy-exec: --base-url-env must not name the credential variable",
        ));
    }
    let declared = ProfileProxy {
        upstream: args.upstream.clone(),
        header: args.header,
        base_url_env: args.base_url_env.clone(),
    };
    let upstream = declared.validate()?;
    let (program, rest) = args
        .command
        .split_first()
        .ok_or_else(|| LaunchError::config("proxy-exec: no command given after --"))?;

    let secret = env
        .iter()
        .find(|(k, _)| k.to_str() == Some(args.credential_env.as_str()))
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            LaunchError::config(format!(
                "proxy-exec: credential substitution is enabled but {} is unset or empty; \
                 refusing to launch (never falls back to forwarding a credential)",
                args.credential_env
            ))
        })?
        .to_str()
        .ok_or_else(|| LaunchError::config("proxy-exec: the credential value must be UTF-8"))?
        .to_string();
    if secret.trim().is_empty() {
        return Err(LaunchError::config(format!(
            "proxy-exec: {} is blank; refusing to launch",
            args.credential_env
        )));
    }
    if secret.starts_with(PLACEHOLDER_PREFIX) {
        // Already inside a proxied launch (or handed a leaked placeholder):
        // forwarding it upstream as though it were a key would be a silent
        // failure at best and a placeholder leak to the provider at worst.
        return Err(LaunchError::config(format!(
            "proxy-exec: {} already holds a Loom placeholder, not a real credential; \
             refusing to nest a second proxy",
            args.credential_env
        )));
    }

    let prepared = arm(
        secret.clone(),
        args.provider.clone(),
        &declared,
        upstream,
        &[args.credential_env.as_str()],
        Vec::new(),
    )?;

    let mut command = Command::new(program);
    command.args(rest);
    // Defense in depth: any OTHER variable carrying the same value (an
    // operator who also exported it as ANTHROPIC_AUTH_TOKEN, say) is removed
    // from the child's environment, so a by-name `-e` of that variable cannot
    // carry the real value across the boundary either.
    for (key, value) in env {
        let assigned = prepared
            .injection
            .assignments
            .iter()
            .any(|(k, _)| key.to_str() == Some(k.as_str()));
        if !assigned && value.to_string_lossy().contains(secret.as_str()) {
            command.env_remove(key);
        }
    }
    for (key, value) in &prepared.injection.assignments {
        command.env(key, value);
    }
    Ok((prepared, command))
}

/// Entry point for `loom-daemon worker proxy-exec`. Never returns on success:
/// [`run_with_proxy`] exits with the child's status once the placeholder has
/// been invalidated.
pub fn cli(args: ExecArgs) -> anyhow::Result<()> {
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let outcome = build(&args, &env).and_then(|(prepared, command)| {
        // Secret-free: launch id, upstream origin, bind address, header style.
        eprintln!("{}", prepared.dispatch_marker());
        run_with_proxy(prepared, command)
    });
    if let Err(error) = outcome {
        eprintln!("{}", error.message);
        std::process::exit(error.code);
    }
    Ok(())
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
