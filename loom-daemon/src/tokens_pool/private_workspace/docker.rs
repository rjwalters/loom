use super::*;
use serde_json::{json, Value};
use std::process::{Command, Stdio};

/// Capture without forwarding Docker stderr: exec/auth errors may contain
/// credential material. All callers report the operation, never its payload.
pub(super) fn command(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("Docker unavailable")?;
    if !output.status.success() {
        bail!(
            "Docker {} failed ({}); inspect the session with Docker locally",
            args.first().unwrap_or(&"operation"),
            output.status
        );
    }
    String::from_utf8(output.stdout).context("Docker returned invalid text")
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
    let expected = 2 + usize::from(config.gh_config.is_some());
    if mounts.len() != expected {
        bail!("private session has unexpected mounts; no host workspace, peer volume or runtime socket may be mounted");
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
            _ => false,
        };
        if !valid {
            bail!("private session mount does not match its account-owned configuration");
        }
    }

    Ok(())
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
    for value in [
        format!("CODEX_HOME={PROFILE}"),
        "XDG_CACHE_HOME=/workspace/cache/xdg".into(),
        "CARGO_HOME=/workspace/cache/cargo".into(),
        "CARGO_TARGET_DIR=/workspace/cache/target".into(),
        "npm_config_cache=/workspace/cache/npm".into(),
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

pub(super) fn setup(config: &Config) -> Result<()> {
    let output = command(&[
        "exec",
        &config.container,
        "loom-daemon",
        "private-workspace",
        "setup",
    ])?;
    let report: worker_setup::SetupReport = serde_json::from_str(&output)
        .context("private setup endpoint unavailable; update the session image")?;
    if report.protocol != PROTOCOL {
        bail!("private setup protocol mismatch; update the session image");
    }
    match report.status {
        worker_setup::SetupStatus::Ready => Ok(()),
        worker_setup::SetupStatus::ProfileInaccessible => bail!("private account profile is not readable and writable by session UID 1000; check external profile ownership and permissions (profile was preserved)"),
        worker_setup::SetupStatus::Failed => bail!("private helper/configuration setup failed; inspect the owned account locally (configuration was preserved)"),
    }
}

pub(super) fn prepare(config: &Config, id: &str, branch: Option<&str>) -> Result<String> {
    let mut args = vec!["exec", "--workdir", ROOT];
    // `--env NAME` copies the host process's value directly; secrets never
    // appear in argv, config files, remote URLs or our captured output.
    for name in FORGE_ENV {
        if std::env::var_os(name).is_some() {
            args.extend(["--env", name]);
        }
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "prepare",
        "--account",
        &config.account,
        "--repository",
        &config.repository,
        "--base",
        &config.base,
        "--container-id",
        id,
    ]);
    if let Some(branch) = branch {
        args.extend(["--branch", branch]);
    }
    let output = command(&args).context("private workspace endpoint failed; use a current session image supporting private-workspace protocol")?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private workspace preparation refused: {error}");
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
