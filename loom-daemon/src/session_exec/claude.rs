//! Enter an adopted Claude seat with the credential selected on the host.
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

use super::{host, posture, HostArgs};
use crate::tokens_pool::{account_registry, claude_seat as seat, paths, select, session_lifecycle};

#[derive(clap::Args)]
pub struct Args {
    #[arg(long)]
    workspace: PathBuf,
    /// Original spawn-claude arguments, including --use-wrapper when requested.
    #[arg(last = true)]
    args: Vec<String>,
}

pub(super) fn run(request: Args) -> Result<i32> {
    ensure!(
        std::env::var_os("LOOM_PRIVATE_LEASE_FD").is_none(),
        "cannot enter a Claude seat with a private-clone lease"
    );
    let name = std::env::var("LOOM_TOKEN_NAME").context("missing Claude account name")?;
    account_registry::validate_name(&name)?;
    let root = seat::seat_root().context("Claude seats disabled")?;
    ensure!(seat::is_seat(&root, &name), "Claude seat {name} is not registered");
    // Explicit-token callers bypass selection. Never pair their bearer with a
    // stale inherited account name or run a seat after its token was revoked.
    let file = paths::resolve_tokens_dir(&request.workspace).join(format!("{name}.token"));
    let selected =
        select::read_token_file(&file).context("cannot verify the selected seat credential")?;
    ensure!(
        !selected.is_empty()
            && std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok().as_ref() == Some(&selected),
        "process credential does not match the selected Claude seat"
    );

    let mut env = vec![
        "CLAUDE_CODE_OAUTH_TOKEN".into(),
        "LOOM_TOKEN_NAME".into(),
        format!("LOOM_WORKSPACE={}", request.workspace.display()),
        "LOOM_CLAUDE_SEAT_ACTIVE=1".into(),
        "LOOM_CLAUDE_HOST_SELECTS_ACCOUNT=1".into(),
        "LOOM_SPAWN_CONTAINERIZED=1".into(),
        "CARGO_INCREMENTAL=0".into(),
    ];
    // Only carry launch context. HOME, CLAUDE_CONFIG_DIR, PATH, alternate
    // provider keys and host token-pool paths belong to the container.
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        if !value.is_empty()
            && (name.starts_with("OTEL_")
                || matches!(
                    name,
                    "TRACEPARENT"
                        | "GH_TOKEN"
                        | "GITHUB_TOKEN"
                        | "LOOM_ROLE"
                        | "LOOM_RUNTIME"
                        | "LOOM_TERMINAL_ID"
                        | "LOOM_SWEEP_ID"
                        | "LOOM_SWEEP_CLAIM_OWNED"
                        | "LOOM_SWEEP_LEASE_RENEW_DISPATCHED"
                        | "LOOM_LEASE_HOST_ID"
                        | "LOOM_LEASE_PUBLISH_HOSTNAME"
                        | "LOOM_HOST_ID"
                        | "LOOM_ACCOUNT_NAME"
                        | "LOOM_ACCOUNT_PROVIDER"
                        | "LOOM_ACCOUNT_UPSTREAM_ID"
                        | "LOOM_DAEMON_ITEM_ID"
                        | "LOOM_WORKTREE_PATH"
                        | "LOOM_WORKTREE_ROOT"
                        | "LOOM_PROJECT_ROOT"
                        | "LOOM_ROLE_PR_LEASE_FILE"
                        | "LOOM_PICK_JOURNAL"
                        | "LOOM_CLAUDE_SESSION_ID"
                        | "LOOM_RESUME_SESSION_ID"
                        | "LOOM_RESUME_PROMPT"
                        | "LOOM_ROLL_PAUSE_DIR"
                        | "LOOM_MODEL"
                        | "LOOM_EFFORT"
                        | "LOOM_SERVICE_TIER"
                        | "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS"
                        | "CARGO_PROFILE_DEV_DEBUG"
                        | "CARGO_PROFILE_TEST_DEBUG"
                ))
        {
            env.push(name.to_owned());
        }
    }
    let mut child_env = Vec::new();
    for (key, names) in [
        ("user.name", ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"]),
        ("user.email", ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"]),
    ] {
        let configured = std::process::Command::new("git")
            .args(["config", "--get", key])
            .output()?;
        let fallback = String::from_utf8_lossy(&configured.stdout);
        for name in names {
            let value = std::env::var(name).unwrap_or_else(|_| fallback.trim().to_owned());
            if !value.is_empty() {
                env.push(name.into());
                child_env.push((name.into(), value));
            }
        }
    }
    let mut command = vec![request
        .workspace
        .join(".loom/scripts/spawn-claude.sh")
        .display()
        .to_string()];
    command.extend(request.args);
    host::run_with_seat(
        HostArgs {
            owner_pid: None,
            container: seat::container_name(&name),
            workdir: std::env::current_dir()?.display().to_string(),
            env,
            child_env,
            stderr_file: None,
            command,
        },
        Some(&seat::home_dir(&root, &name)),
    )
}

/// Bind both the log record and the exec to the same inspected instance.
/// A container recreated under the same name cannot receive this launch.
pub(super) fn bind_launch(args: &mut HostArgs, state: &Value, home: &Path) -> Result<()> {
    let labels = &state["Config"]["Labels"];
    ensure!(
        labels[seat::RUNTIME_LABEL] == seat::RUNTIME
            && labels[seat::CONTAINMENT_LABEL] == seat::CONTAINMENT
            && labels[session_lifecycle::SESSION_POSTURE_LABEL]
                == session_lifecycle::SESSION_POSTURE,
        "container {} is not a managed Claude seat",
        args.container
    );
    let violations = posture::container_violations(state);
    ensure!(violations.is_empty(), "Claude seat posture mismatch: {}", violations.join(", "));
    let mounts = state["Mounts"]
        .as_array()
        .context("missing Claude seat mounts")?;
    let registered_home = home.canonicalize()?;
    let is_home = |m: &Value| m["Destination"] == seat::CONTAINER_CLAUDE_HOME;
    ensure!(
        mounts.iter().any(|m| is_home(m)
            && m["Type"] == "bind"
            && m["RW"] == true
            && m["Source"].as_str().map(Path::new) == Some(&registered_home)),
        "Claude seat does not mount its registered home"
    );
    seat::check_mount_sources(
        home,
        mounts
            .iter()
            .filter(|m| !is_home(m))
            .filter_map(|m| m["Source"].as_str().map(Path::new)),
    )?;
    if let Ok(dir) = std::env::var("GH_CONFIG_DIR") {
        if !dir.is_empty() && posture::mounted(state, &dir) {
            args.env.push("GH_CONFIG_DIR".into());
        }
    }
    let (Some(id), Some(digest), Some(image)) =
        (state["Id"].as_str(), state["Image"].as_str(), state["Config"]["Image"].as_str())
    else {
        bail!("Claude seat inspect has no container or image identity");
    };
    ensure!(
        !id.is_empty() && digest.starts_with("sha256:") && !image.is_empty(),
        "incomplete Claude seat launch identity"
    );
    eprintln!("# LOOM_DISPATCH_MODE mode=container image={image} cpus=none memory=none containment={} container_id={id} image_digest={digest}", seat::CONTAINMENT);
    args.container = id.to_owned();
    Ok(())
}
