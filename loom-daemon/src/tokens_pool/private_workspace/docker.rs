use super::*;
use serde_json::{json, Value};
use std::time::Duration;

use super::super::docker_cli;

/// Budget for the long `exec`s (`setup`, `prepare`): a clone or fetch inside
/// the session legitimately outlasts a reconcile-sized call.
pub(super) const EXEC_TIMEOUT: Duration = docker_cli::DOCKER_RUN_TIMEOUT;

/// Capture without forwarding Docker stderr: exec/auth errors may contain
/// credential material. All callers report the operation, never its payload.
///
/// Bounded by [`docker_cli::timeout_for`], so a wedged engine fails the call
/// with the typed [`docker_cli::DockerTimedOut`] instead of holding the lease
/// (and the reconciler thread) forever.
pub(super) fn command(args: &[&str]) -> Result<String> {
    command_with(args, docker_cli::timeout_for(args))
}

/// [`command`] with an explicit budget, for calls longer than their
/// subcommand's default (`exec` of `setup`/`prepare`).
pub(super) fn command_with(args: &[&str], timeout: Duration) -> Result<String> {
    command_env(args, &[], timeout)
}

/// [`command_with`] with variables set on the `docker` client process only
/// (#13916): a forge token resolved for one exec, named in `args` as a bare
/// `--env NAME` so its value never appears in argv.
pub(super) fn command_env(
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<String> {
    // Every Docker call of this module goes through here, so a fake installed
    // on this thread (`fake`, tests only) stands in for the whole engine.
    #[cfg(test)]
    if let Some(result) = fake::dispatch(args, env) {
        return result;
    }
    let (success, stdout, _stderr) = docker_cli::run_bounded_env("docker", args, env, timeout)
        .map_err(|error| match error.downcast::<docker_cli::DockerTimedOut>() {
            Ok(timed_out) => anyhow::Error::new(timed_out),
            Err(error) => error.context("Docker unavailable"),
        })?;
    if !success {
        bail!(
            "Docker {} failed; inspect the session with Docker locally",
            args.first().unwrap_or(&"operation"),
        );
    }
    Ok(stdout)
}

pub(super) fn desktop() -> Result<bool> {
    Ok(cfg!(target_os = "macos")
        && command(&["info", "--format", "{{.OperatingSystem}}"])?.trim() == "Docker Desktop")
}

pub(super) fn engine() -> Result<String> {
    let id = command(&["info", "--format", "{{.ID}}"])?;
    if id.trim().is_empty() {
        bail!("Docker engine identity is unavailable");
    }
    Ok(id.trim().to_owned())
}

pub(super) fn inspect(name: &str) -> Result<Option<Value>> {
    // Listing gives an authoritative missing result; transport errors never
    // masquerade as absence (which would incorrectly reclaim a stale lease).
    let names = command(&["container", "ls", "-a", "--format", "{{.Names}}"])?;
    if !names.lines().any(|line| line == name) {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&command(&["inspect", name])?)?;
    Ok(Some(value[0].clone()))
}

pub(super) fn inspect_id(id: &str) -> Result<Option<Value>> {
    if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("lease has an invalid container identity; preserve it for operator recovery");
    }
    let ids = command(&["container", "ls", "-aq", "--no-trunc"])?;
    if !ids.lines().any(|candidate| candidate == id) {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&command(&["inspect", id])?)?;
    Ok(Some(value[0].clone()))
}

pub(super) fn validate(config: &Config, state: &Value) -> Result<()> {
    validate_settings(config, state)?;
    validate_volume(config)?;
    validate_ownership(config)
}

fn validate_ownership(config: &Config) -> Result<()> {
    let mut sources = vec![
        config.volume.clone(),
        config.profile.to_string_lossy().into_owned(),
    ];
    if config.docker_desktop && cfg!(target_os = "macos") {
        sources.push(format!("/host_mnt{}", config.profile.display()));
    }
    for source in sources {
        let peers = command(&[
            "ps",
            "-a",
            "--filter",
            &format!("volume={source}"),
            "--format",
            "{{.Names}}",
        ])?;
        if peers.lines().any(|peer| peer != config.container) {
            bail!("private workspace or account profile is attached to another container; refuse shared ownership");
        }
    }

    Ok(())
}

pub(super) fn validate_settings(config: &Config, state: &Value) -> Result<()> {
    let hc = &state["HostConfig"];
    let labels = &state["Config"]["Labels"];
    if labels["loom.workspace-mode"] != MODE
        || labels["loom.account"] != config.account
        || labels["loom.repository"] != config.repository
        || labels["loom.workspace"] != REPO
    {
        bail!("session workspace/account/repository identity differs; stop the idle old session before migrating (never reuse or reset it)");
    }
    if hc["Privileged"] != false
        || hc["ReadonlyRootfs"] != true
        || hc["PidMode"] == "host"
        || hc["IpcMode"] == "host"
        || hc["NetworkMode"] == "host"
        || hc["CapAdd"].as_array().is_some_and(|a| !a.is_empty())
        || hc["Devices"].as_array().is_some_and(|a| !a.is_empty())
        || hc["DeviceRequests"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        || !hc["CapDrop"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "ALL"))
        || !hc["SecurityOpt"].as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v == "no-new-privileges" || v == "no-new-privileges:true")
        })
        || state["Config"]["User"] != "1000:1000"
        || state["Config"]["Entrypoint"] != json!(["/usr/bin/tini"])
        || state["Config"]["Cmd"] != json!(["--", "/bin/sleep", "infinity"])
    {
        bail!("private session containment settings differ; refuse reuse and recreate the idle container");
    }
    let mounts = state["Mounts"]
        .as_array()
        .context("missing actual mount inventory")?;
    // Workspace volume + profile bind + one read-only bind per profile control
    // file (+ the optional forge configuration). Because every destination is
    // required to be distinct AND to match one of those exact roles, an exact
    // count is also a proof that none of them is MISSING — which is what makes
    // this the host-side half of the control-file protection (issue #8839).
    let expected = 2
        + bundle::PROFILE_CONTROLS.len()
        + usize::from(config.gh_config.is_some())
        + config.extra_mounts.len();
    if mounts.len() != expected {
        bail!("private session has unexpected mounts; no host workspace, peer volume or runtime socket may be mounted, and every account profile control file must be bound read-only");
    }
    let mut seen = std::collections::HashSet::new();
    for mount in mounts {
        let destination = mount["Destination"]
            .as_str()
            .context("invalid mount destination")?;
        if !seen.insert(destination) {
            bail!("duplicate private session mount");
        }
        let valid = match destination {
            ROOT => {
                mount["Type"] == "volume" && mount["Name"] == config.volume && mount["RW"] == true
            }
            PROFILE => {
                mount["Type"] == "bind"
                    && bind_source_matches(config, mount, &config.profile)
                    && mount["RW"] == true
            }
            GH_CONFIG => config.gh_config.as_ref().is_some_and(|path| {
                mount["Type"] == "bind"
                    && bind_source_matches(config, mount, path)
                    && mount["RW"] == false
            }),
            // The hook registration, Codex's trust state and Loom's readiness
            // receipt, each bound READ-ONLY over its own canonical path. The
            // mount point is what makes them unremovable and unrenameable from
            // inside the session while `auth.json` beside them stays writable.
            other => {
                bundle::PROFILE_CONTROLS.iter().any(|name| {
                    other == bundle::control_destination(name)
                        && mount["Type"] == "bind"
                        && bind_source_matches(config, mount, &config.profile.join(name))
                        && mount["RW"] == false
                })
                // The recorded `accounts.session.extraMounts` (#14451): data
                // only, so each must be a read-only bind of exactly its
                // recorded host path. One that is not recorded, or recorded
                // and missing, breaks the exact count above.
                || config.extra_mounts.iter().any(|extra| {
                    Path::new(other) == extra.container
                        && mount["Type"] == "bind"
                        && bind_source_matches(config, mount, &extra.host)
                        && mount["RW"] == false
                })
            }
        };
        if !valid {
            bail!("private session mount does not match its account-owned configuration");
        }
    }

    Ok(())
}

/// The read-only control-file binds a private session is created with. One
/// source of truth with `validate_settings` above, so a session can never be
/// created in a shape its own validation would accept while unprotected.
fn control_mounts(config: &Config) -> Vec<String> {
    bundle::PROFILE_CONTROLS
        .iter()
        .map(|name| {
            format!(
                "type=bind,src={},dst={},readonly",
                config.profile.join(name).display(),
                bundle::control_destination(name)
            )
        })
        .collect()
}

pub(super) fn idle(state: Option<&Value>, container: &str) -> Result<bool> {
    let Some(state) = state else { return Ok(true) };
    if state["State"]["Running"] == false {
        return Ok(true);
    }
    if state["State"]["Running"] != true
        || state["State"]["Paused"] == true
        || state["ExecIDs"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty())
    {
        return Ok(false);
    }
    let output = command(&["top", container, "-o", "pid,args"])?;
    let processes: Vec<_> = output
        .lines()
        .skip(1)
        .map(str::trim)
        .map(|line| {
            line.split_once(char::is_whitespace)
                .map_or("", |(_, command)| command.trim())
        })
        .collect();
    // Private sessions deliberately have no tmux/baseline interactive shell.
    // Every extra process, including a detached child, is busy/ambiguous.
    Ok(processes.len() == 2
        && processes.contains(&"/usr/bin/tini -- /bin/sleep infinity")
        && processes.contains(&"/bin/sleep infinity"))
}

pub(super) fn volume_exists(volume: &str) -> Result<bool> {
    let volumes = command(&["volume", "ls", "--format", "{{.Name}}"])?;
    Ok(volumes.lines().any(|name| name == volume))
}

/// The recorded image must already be local: recovery never pulls or swaps one.
pub(super) fn image_present(image: &str) -> Result<()> {
    command(&["image", "inspect", "--format", "{{.Id}}", image])
        .map(drop)
        .with_context(|| format!("recorded session image {image} is not available locally"))
}

pub(super) fn create(config: &Config, image: &str) -> Result<()> {
    let volumes = command(&["volume", "ls", "--format", "{{.Name}}"])?;
    if volumes.lines().any(|name| name == config.volume) {
        validate_volume(config)?;
    } else {
        command(&[
            "volume",
            "create",
            "--label",
            &format!("loom.account={}", config.account),
            "--label",
            &format!("loom.repository={}", config.repository),
            "--label",
            &format!("loom.profile={}", config.profile.display()),
            &config.volume,
        ])?;
        // Initialize only a freshly created empty volume. Never recursively
        // chown or erase a reused volume that may contain unfinished work.
        command(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--user",
            "0",
            "--cap-drop",
            "ALL",
            "--cap-add",
            "CHOWN",
            "--security-opt",
            "no-new-privileges",
            "--mount",
            &format!("type=volume,src={},dst={ROOT}", config.volume),
            "--entrypoint",
            "/bin/chown",
            image,
            "1000:1000",
            ROOT,
        ])?;
    }
    validate_ownership(config)?;
    let mut args = vec![
        "run".to_owned(),
        "-d".into(),
        "--name".into(),
        config.container.clone(),
        "--user".into(),
        "1000:1000".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--tmpfs".into(),
        "/tmp:rw,nosuid,nodev,size=512m".into(),
        "--mount".into(),
        format!("type=volume,src={},dst={ROOT}", config.volume),
        "--mount".into(),
        format!("type=bind,src={},dst={PROFILE}", config.profile.display()),
        "--label".into(),
        format!("loom.workspace-mode={MODE}"),
        "--label".into(),
        format!("loom.workspace={REPO}"),
        "--label".into(),
        format!("loom.account={}", config.account),
        "--label".into(),
        format!("loom.repository={}", config.repository),
    ];
    // Read-only binds over the profile's own control files. They must already
    // exist on the host: `provision_controls` below is what guarantees that,
    // and it runs before this in a container that does NOT carry these binds
    // (writing them from inside a session is exactly what they prevent).
    for mount in control_mounts(config) {
        args.extend(["--mount".to_owned(), mount]);
    }
    for value in [
        format!("CODEX_HOME={PROFILE}"),
        "XDG_CACHE_HOME=/workspace/cache/xdg".into(),
        "CARGO_HOME=/workspace/cache/cargo".into(),
        "CARGO_TARGET_DIR=/workspace/cache/target".into(),
        "npm_config_cache=/workspace/cache/npm".into(),
        // `.loom/lease-renew/.lock` has been observed untracked in seat clones
        // (#14160); the writer is not traced (#14226). It is not this
        // container's `$HOME`, which is `/home/loom` here. Point the store at
        // the cache volume so Loom's own lock stays out of the work tree.
        "LOOM_LEASE_RENEW_STATE_DIR=/workspace/cache/lease-renew".into(),
        format!("LOOM_PRIVATE_REPOSITORY={}", config.repository),
    ] {
        args.extend(["--env".into(), value]);
    }
    if let Some(path) = &config.gh_config {
        args.extend([
            "--mount".into(),
            format!("type=bind,src={},dst={GH_CONFIG},readonly", path.display()),
            "--env".into(),
            format!("GH_CONFIG_DIR={GH_CONFIG}"),
        ]);
    }
    // The configured extra mounts, always read-only (#14451). Their JSON is
    // labelled only when there are any, so an unconfigured create is
    // byte-identical to what it has always been.
    for extra in &config.extra_mounts {
        args.extend([
            "--mount".into(),
            format!(
                "type=bind,src={},dst={},readonly",
                extra.host.display(),
                extra.container.display()
            ),
        ]);
    }
    if !config.extra_mounts.is_empty() {
        args.extend([
            "--label".into(),
            format!(
                "{}={}",
                super::super::session_lifecycle::EXTRA_MOUNTS_LABEL,
                super::super::session_image::extra_mounts_label_json(&config.extra_mounts)
            ),
        ]);
    }
    args.extend([
        "--entrypoint".into(),
        "/usr/bin/tini".into(),
        image.into(),
        "--".into(),
        "/bin/sleep".into(),
        "infinity".into(),
    ]);
    command(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(())
}

/// The sealed-bundle probe of [`super::super::session_image::verify_override_bundle`]
/// routed through [`command`], so it shares this module's engine seam (and its
/// test fake) rather than spawning its own `docker`.
struct ImageProbe;

impl super::super::session_image::BundleProbe for ImageProbe {
    fn run_image_script(&self, image: &str, script: &str) -> Result<String> {
        command_with(
            &[
                "run",
                "--rm",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "/bin/sh",
                image,
                "-c",
                script,
            ],
            docker_cli::DOCKER_RUN_TIMEOUT,
        )
    }
}

/// Prove the configured override `image` carries the sealed control bundle
/// this daemon enforces, before any container is created from it (#14451).
pub(super) fn verify_image(image: &str) -> Result<()> {
    super::super::session_image::verify_override_bundle(image, &ImageProbe)
        .context("refusing to create a private session from the configured override image")
}

pub(super) fn setup(config: &Config) -> Result<()> {
    let output = command_with(
        &[
            "exec",
            &config.container,
            "loom-daemon",
            "private-workspace",
            "setup",
        ],
        EXEC_TIMEOUT,
    )?;
    report(&output)
}

/// Materialize the account profile's control files — the managed hook
/// registration, Codex's own trust-state file and Loom's readiness receipt —
/// with the image-owned, sealed provisioner, **before** the session container
/// exists.
///
/// This cannot happen inside the session: the session binds those three files
/// read-only, which is the entire protection, so a write there is `EROFS` by
/// design. It therefore runs in a throwaway container from the same image,
/// carrying the account profile and nothing else — no workspace volume, no
/// forge configuration, no network — so the only thing it can touch is the
/// profile Loom already owns. `auth.json` is never read, written or copied by
/// any of it.
pub(super) fn provision_controls(config: &Config, image: &str) -> Result<()> {
    let output = command(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--user",
        "1000:1000",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=64m",
        "--mount",
        &format!("type=bind,src={},dst={PROFILE}", config.profile.display()),
        "--env",
        &format!("CODEX_HOME={PROFILE}"),
        "--entrypoint",
        "loom-daemon",
        image,
        "private-workspace",
        "provision-controls",
    ]).context("private control provisioning endpoint unavailable; use a session image that ships the versioned control bundle")?;
    report(&output)
}

fn report(output: &str) -> Result<()> {
    let report: worker_setup::SetupReport = serde_json::from_str(output)
        .context("private setup endpoint unavailable; update the session image")?;
    if report.protocol != PROTOCOL {
        bail!("private setup protocol mismatch; update the session image");
    }
    match report.status {
        worker_setup::SetupStatus::Ready => Ok(()),
        worker_setup::SetupStatus::ProfileInaccessible => bail!("private account profile is not readable and writable by session UID 1000; check external profile ownership and permissions (profile was preserved)"),
        worker_setup::SetupStatus::ControlMissing => bail!("private session image ships no {} control bundle, so the account profile's hook registration cannot be established from a sealed provisioner; guard code and effective policy would live in worker-writable paths", bundle::CONTROL_PROTOCOL),
        worker_setup::SetupStatus::Failed => bail!("private helper/configuration setup failed; inspect the owned account locally (configuration was preserved)"),
    }
}

/// Take the worker's own observation of the control boundary. Addressed by the
/// container's immutable ID, never by its mutable Docker name: the boundary is
/// bound to one container identity, and a rename between the identity check and
/// this probe must not let a different worker answer for it.
fn observe_control(id: &str) -> Result<bundle::Report> {
    let output = command(&["exec", id, "loom-daemon", "private-workspace", "control"])
        .context("private control endpoint unavailable; use a session image that ships the versioned control bundle")?;
    serde_json::from_str(&output)
        .context("private control endpoint is not the supported protocol; update the session image")
}

/// Verify the session's versioned control boundary and return the identity to
/// bind to the account lease. Every claim in the report is re-derived inside
/// the container from the image-owned bundle, and its view of the account
/// profile is cross-checked here against the host's own read of the canonical
/// profile — a container label or environment variable would prove nothing
/// about either.
pub(super) fn control(config: &Config, id: &str) -> Result<String> {
    bundle::accept(&observe_control(id)?, &config.profile)
}

/// Recheck a bound control identity immediately before mutable work, and hand
/// back the observation it was proven against. The report carries the one
/// boundary fact identity alone cannot be read back out of — whether this
/// clone has a managed registration at all (`Report::managed`) — which
/// containment admission (#8787) requires; it is part of the hashed identity,
/// so it cannot drift from what was bound.
pub(super) fn recheck_control(config: &Config, id: &str, bound: &str) -> Result<bundle::Report> {
    let report = observe_control(id)?;
    bundle::rebind(&report, &config.profile, bound)?;
    Ok(report)
}

pub(super) fn prepare(
    config: &Config,
    workspace: &Path,
    id: &str,
    branch: Option<&str>,
) -> Result<String> {
    prepare_target(config, workspace, &config.primary(), id, branch)
}

/// Prepare one repository of the account: the primary, or a lazily cloned
/// additional one. Credentials stay scoped to THAT repository's forge via the
/// exec's own `LOOM_PRIVATE_REPOSITORY`, never widened on the container.
pub(super) fn prepare_target(
    config: &Config,
    workspace: &Path,
    target: &Target,
    id: &str,
    branch: Option<&str>,
) -> Result<String> {
    // Resolved per exec (#13916): an App installation token outlives no job.
    let auth = forge_auth::ExecAuth::resolve(workspace, &target.repository);
    prepare_with(config, target, id, branch, &auth)
}

/// Drive one clone's `private-workspace recover` verb under the job's auth
/// context (#14030): the exec carries the same repository-scoped credential
/// env as preparation, so a publish pushes only through that clone's helper.
pub(super) fn recover_target(
    config: &Config,
    target: &Target,
    action: RecoverAction,
    issue: Option<u64>,
    branches: &[String],
    worktrees: &[PathBuf],
    auth: &forge_auth::ExecAuth,
) -> Result<RecoverOutcome> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", target.repository);
    let issue_flag = issue.map(|n| n.to_string());
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "recover",
        "--account",
        &config.account,
        "--repository",
        &target.repository,
        "--base",
        &target.base,
    ]);
    match action {
        RecoverAction::Publish => args.extend(["--publish"]),
        RecoverAction::Discard => args.push("--discard"),
        RecoverAction::ValidateWorktrees => args.extend(["--discard", "--validate-worktrees"]),
        RecoverAction::List => {}
    }
    if let Some(issue) = &issue_flag {
        args.extend(["--issue", issue]);
    }
    for branch in branches {
        args.extend(["--branch", branch]);
    }
    let worktree_flags: Vec<String> = worktrees.iter().map(|w| w.display().to_string()).collect();
    for worktree in &worktree_flags {
        args.extend(["--worktree", worktree]);
    }
    if let Some(slug) = &target.slug {
        args.extend(["--slug", slug]);
    }
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context(
        "private workspace endpoint failed; use a current session image supporting private-workspace protocol",
    )?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private clone recovery refused: {error}");
    }
    serde_json::from_value(result["outcome"].clone())
        .context("private workspace recover returned an invalid report")
}

/// Drive the in-volume `private-workspace rebind` verb (#14357) under the
/// credential scope of the CANONICAL repository.
pub(super) fn rebind_target(
    config: &Config,
    from: &Registered,
    to: &RepoId,
    dry_run: bool,
    auth: &forge_auth::ExecAuth,
) -> Result<RebindOutcome> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", to.url);
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "rebind",
        "--account",
        &config.account,
        "--from",
        &from.repository,
        "--from-slug",
        &from.slug,
        "--to",
        &to.url,
        "--to-slug",
        &to.slug,
        "--base",
        &from.base,
    ]);
    if dry_run {
        args.push("--dry-run");
    }
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context(
        "private workspace endpoint failed; use a current session image supporting private-workspace protocol",
    )?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private clone rebind refused: {error}");
    }
    serde_json::from_value(result["outcome"].clone())
        .context("private workspace rebind returned an invalid report")
}

pub(super) fn prepare_with(
    config: &Config,
    target: &Target,
    id: &str,
    branch: Option<&str>,
    auth: &forge_auth::ExecAuth,
) -> Result<String> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", target.repository);
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    // `--env NAME` copies the `docker` client's value directly: the host
    // process's own, or one this host resolved and set on that child alone
    // (#13916). Secrets never appear in argv, config files, remote URLs or
    // our captured output.
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "prepare",
        "--account",
        &config.account,
        "--repository",
        &target.repository,
        "--base",
        &target.base,
        "--container-id",
        id,
    ]);
    if let Some(branch) = branch {
        args.extend(["--branch", branch]);
    }
    if let Some(slug) = &target.slug {
        args.extend(["--slug", slug]);
    }
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context("private workspace endpoint failed; use a current session image supporting private-workspace protocol")?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private workspace preparation refused: {error}");
    }
    // Optional since the reap-reporting field: an older session image omits
    // it, and a non-array or non-string entry is skipped, never fatal.
    for path in result["reaped_worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        log::info!(
            "private session {}: reaped finished-PR worktree {path} in {}",
            config.account,
            target.repository
        );
    }
    result["revision"]
        .as_str()
        .map(str::to_owned)
        .context("private clone did not resolve a base revision")
}

fn validate_volume(config: &Config) -> Result<()> {
    let existing: Value = serde_json::from_str(&command(&["volume", "inspect", &config.volume])?)?;
    validate_volume_settings(config, &existing[0])
}

pub(super) fn validate_volume_settings(config: &Config, volume: &Value) -> Result<()> {
    if volume["Driver"] != "local"
        || volume["Options"]
            .as_object()
            .is_some_and(|options| !options.is_empty())
        || (!volume["Options"].is_null() && !volume["Options"].is_object())
        || volume["Labels"]["loom.account"] != config.account
        || volume["Labels"]["loom.repository"] != config.repository
        || volume["Labels"]["loom.profile"] != config.profile.to_string_lossy().as_ref()
    {
        bail!("workspace volume must be an ordinary local Docker-managed volume owned by this account; driver/bind options and unknown ownership are refused");
    }
    Ok(())
}

fn bind_source_matches(config: &Config, mount: &Value, expected: &Path) -> bool {
    let expected = expected.to_string_lossy();
    // Desktop rewrites both effective and requested mount sources into its VM
    // namespace. The persisted engine identity was verified at resolution;
    // only that detected engine/platform gets this exact prefix equivalence.
    mount["Source"] == expected.as_ref()
        || (cfg!(target_os = "macos")
            && config.docker_desktop
            && mount["Source"] == format!("/host_mnt{expected}"))
}

/// Test seam: every Docker invocation of this module funnels through
/// [`command`], so a fake installed on the current thread stands in for the
/// whole engine (containers, volumes, images, execs) and no test touches a
/// real Docker (#12772).
#[cfg(test)]
pub(super) mod fake {
    use std::cell::RefCell;

    pub(in super::super) type Handler = Box<dyn FnMut(&[&str]) -> anyhow::Result<String>>;

    thread_local! {
        static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
        /// The child environment each faked call would have carried (#13916).
        static ENVS: RefCell<Vec<Vec<(String, String)>>> = const { RefCell::new(Vec::new()) };
    }

    /// Every faked call's `docker` child environment, in call order.
    pub(in super::super) fn seen_env() -> Vec<Vec<(String, String)>> {
        ENVS.with(|e| e.borrow().clone())
    }

    /// Route this thread's Docker calls to `handler` until the guard drops.
    pub(in super::super) fn install(handler: Handler) -> Guard {
        HANDLER.with(|h| *h.borrow_mut() = Some(handler));
        ENVS.with(|e| e.borrow_mut().clear());
        Guard
    }

    pub(in super::super) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HANDLER.with(|h| *h.borrow_mut() = None);
            ENVS.with(|e| e.borrow_mut().clear());
        }
    }

    pub(super) fn dispatch(
        args: &[&str],
        env: &[(String, String)],
    ) -> Option<anyhow::Result<String>> {
        HANDLER.with(|h| {
            h.borrow_mut().as_mut().map(|handler| {
                ENVS.with(|e| e.borrow_mut().push(env.to_vec()));
                handler(args)
            })
        })
    }
}
