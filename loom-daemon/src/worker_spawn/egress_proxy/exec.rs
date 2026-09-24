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
//! loom-daemon worker proxy-exec --docker-workspace <WORKSPACE> -- docker run … <image> …
//! ```
//!
//! (The credential variable, upstream and base-URL variable default to
//! Claude's — `CLAUDE_CODE_OAUTH_TOKEN`, `https://api.anthropic.com`,
//! `ANTHROPIC_BASE_URL` — because Claude is the one shell adapter using this
//! today; each is an explicit flag for any other.)
//!
//! This process reads the REAL credential out of its own environment, binds
//! the listener, registers a per-launch placeholder, and spawns the command
//! with the credential variable **overwritten** by the placeholder and each
//! `--base-url-env` variable set to the proxy's URL. Those names are forwarded
//! by NAME (`-e VAR`), so docker reads the placeholder — never the real value
//! — out of the environment this process handed it.
//!
//! `--docker-workspace` keeps that contract out of the shell adapter (#8697
//! review): it splices the proxy's own `docker run` flags in right after
//! `run` — the `-e VAR` forwards above (plus `--forward-env`, `LOOM_TOKEN_NAME`
//! by default), `--add-host host.docker.internal:host-gateway` so the
//! container can reach the host listener, and an empty read-only tmpfs over
//! every token-pool directory the workspace bind mount would otherwise expose
//! (`.loom/tokens`, `.loom/api-keys`, and the shared pool when it lives inside
//! the workspace), so the container holds no real credential on its
//! filesystem either.
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
use std::path::{Path, PathBuf};
use std::process::Command;

/// Arguments for `loom-daemon worker proxy-exec`.
#[derive(clap::Args, Debug)]
pub struct ExecArgs {
    /// Environment variable that holds the REAL credential in this process's
    /// environment. The command sees a per-launch placeholder under the same
    /// name instead.
    #[arg(long, value_name = "VAR", default_value = "CLAUDE_CODE_OAUTH_TOKEN")]
    pub credential_env: String,
    /// The one origin the credential may be sent to.
    #[arg(long, value_name = "URL", default_value = "https://api.anthropic.com")]
    pub upstream: String,
    /// How the real credential is presented upstream:
    /// `authorization-bearer` (default) or `x-api-key`.
    #[arg(long, value_name = "STYLE", default_value = "authorization-bearer", value_parser = parse_header)]
    pub header: HeaderStyle,
    /// Environment variable set, in the command's environment, to the proxy's
    /// base URL. Repeatable.
    #[arg(
        long = "base-url-env",
        value_name = "VAR",
        default_value = "ANTHROPIC_BASE_URL"
    )]
    pub base_url_env: Vec<String>,
    /// The command is a `docker run` whose container bind-mounts this
    /// workspace: splice in the proxy's own docker flags (by-name `-e`
    /// forwards, `--add-host`, token-pool tmpfs masks) right after `run`.
    #[arg(long, value_name = "DIR")]
    pub docker_workspace: Option<PathBuf>,
    /// With `--docker-workspace`: an extra variable to forward into the
    /// container by name (e.g. the non-secret account name host-side
    /// selection exported). Repeatable.
    #[arg(
        long = "forward-env",
        value_name = "VAR",
        default_value = "LOOM_TOKEN_NAME"
    )]
    pub forward_env: Vec<String>,
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
    command.args(docker_args(args, rest, env)?);
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

/// The command's argv after the program: unchanged, or — with
/// `--docker-workspace` — `run`, then the proxy's own docker flags, then the
/// caller's.
fn docker_args(
    args: &ExecArgs,
    rest: &[OsString],
    env: &[(OsString, OsString)],
) -> Result<Vec<OsString>, LaunchError> {
    let Some(workspace) = &args.docker_workspace else {
        return Ok(rest.to_vec());
    };
    let (run, tail) = rest
        .split_first()
        .filter(|(run, _)| run.as_os_str() == "run")
        .ok_or_else(|| {
            LaunchError::config("proxy-exec: --docker-workspace needs a `docker run …` command")
        })?;
    if !args.forward_env.iter().all(|n| is_env_name(n)) {
        return Err(LaunchError::config(
            "proxy-exec: --forward-env must be an environment variable name",
        ));
    }
    let mut out = vec![run.clone()];
    for dir in masked_pool_dirs(workspace, env) {
        out.push("--mount".into());
        out.push(format!("type=tmpfs,destination={},tmpfs-mode=0555", dir.display()).into());
    }
    let forwarded = |argv: &[OsString], name: &str| {
        argv.windows(2)
            .any(|w| w[0] == "-e" && w[1].as_os_str() == name)
    };
    let names = std::iter::once(&args.credential_env)
        .chain(&args.base_url_env)
        .chain(&args.forward_env);
    for name in names {
        if !forwarded(tail, name) && !forwarded(&out, name) {
            out.push("-e".into());
            out.push(name.into());
        }
    }
    out.push("--add-host".into());
    out.push("host.docker.internal:host-gateway".into());
    out.extend(tail.iter().cloned());
    Ok(out)
}

/// Token-pool directories the workspace bind mount would expose inside the
/// container: the per-repo pools, plus the shared pool
/// (`LOOM_SHARED_TOKENS_DIR`, else `$HOME/.loom/tokens`) when it lives inside
/// the workspace. Existing directories only — masking a missing one would make
/// docker create the mountpoint on the host through the bind mount.
fn masked_pool_dirs(workspace: &Path, env: &[(OsString, OsString)]) -> Vec<PathBuf> {
    let var = |name: &str| {
        env.iter()
            .find(|(k, v)| k.to_str() == Some(name) && !v.is_empty())
            .map(|(_, v)| PathBuf::from(v))
    };
    let shared = var("LOOM_SHARED_TOKENS_DIR")
        .unwrap_or_else(|| var("HOME").unwrap_or_default().join(".loom/tokens"));
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in [
        workspace.join(".loom/tokens"),
        workspace.join(".loom/api-keys"),
        shared,
    ] {
        if dir.starts_with(workspace) && dir != workspace && dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
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
