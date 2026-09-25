use super::*;
use std::process::Command;

/// Only fixed status codes cross the Docker boundary. TOML/hook diagnostics
/// can contain operator configuration and must never be copied to host logs.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SetupStatus {
    Ready,
    ProfileInaccessible,
    Failed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SetupReport {
    pub protocol: String,
    pub status: SetupStatus,
}

pub(super) fn report() -> SetupReport {
    let accessible = || -> std::io::Result<()> {
        std::fs::read_dir(PROFILE)?;
        std::fs::File::open(Path::new(PROFILE).join("auth.json"))?;
        // This process owns the account lease. Prove effective access without
        // changing ownership, modes, or any operator configuration.
        let _probe = tempfile::NamedTempFile::new_in(PROFILE)?;
        Ok(())
    };
    let status = if accessible().is_err() {
        SetupStatus::ProfileInaccessible
    } else if setup().is_ok() {
        SetupStatus::Ready
    } else {
        SetupStatus::Failed
    };
    SetupReport {
        protocol: PROTOCOL.into(),
        status,
    }
}

fn setup() -> Result<()> {
    let repo = Path::new(REPO);
    if repo.canonicalize()? != repo || !repo.join(".git").is_dir() {
        bail!("private setup requires the owned clone");
    }
    let control = Path::new(bundle::CONTROL_ROOT);
    // Loom's own source installs ignored helper symlinks. Consumer repositories
    // carry installed helpers in Git. Never copy host scripts into the worker.
    // `.loom/hooks` is the one entry that may point OUT of the clone: when the
    // image ships a control bundle, the worker's own view of the guards is that
    // read-only bundle rather than the writable `defaults/hooks` it could edit.
    for (installed, source) in [
        (".loom/scripts", "defaults/scripts"),
        (".loom/hooks", "defaults/hooks"),
        (".loom/roles", "defaults/roles"),
        (".claude/commands/loom", "defaults/.claude/commands/loom"),
    ] {
        let target = repo.join(installed);
        // Redirect only a link this setup would have created anyway. A clone
        // whose repository ships no `defaults/hooks` gets no new path here:
        // materializing one would leave the clone untracked-dirty, which the
        // next admission's `prepare` correctly refuses.
        let bundled = installed == ".loom/hooks" && control.join("hooks").is_dir();
        if !target.exists() && repo.join(source).is_dir() {
            std::fs::create_dir_all(target.parent().unwrap())?;
            let link = if bundled {
                control.join("hooks")
            } else {
                repo.join(source)
            };
            std::os::unix::fs::symlink(link, &target)?;
        }
        // A helper that does not exist cannot escape anything: a repository
        // carrying no installed Loom surface at all is a supported private
        // clone, it simply has no managed guard registration to establish.
        if target.exists() {
            let resolved = target.canonicalize()?;
            if !resolved.starts_with(repo) && !resolved.starts_with(control) {
                bail!("installed private helper escaped its clone");
            }
        }
    }
    control::audit()?;
    // Only a repository that ships Loom's installed surface has a managed hook
    // to establish. The provisioner that is RUN, though, is the image-owned
    // copy whenever the session image carries a control bundle: it both writes
    // and checks the readiness evidence, so executing the clone's copy would
    // let a worker certify its own readiness by replacing one tracked file.
    if !repo
        .join(".loom/scripts/provision-codex-hooks.sh")
        .is_file()
    {
        return Ok(());
    }
    let bundled = control.join(bundle::PROVISIONER);
    let helper = if bundled.is_file() {
        bundled
    } else {
        repo.join(".loom/scripts/provision-codex-hooks.sh")
    };
    // Register the image-owned bridge explicitly. Without it the managed entry
    // would name a path inside the clone, where deleting one file removes
    // enforcement for the whole session (issue #8839).
    let bridge = control.join("hooks/guard-codex-bridge.sh");
    let mut common = vec!["--codex-home", PROFILE, "--workspace", REPO];
    if bridge.is_file() {
        common.extend(["--bridge", bridge.to_str().context("bridge path")?]);
    }
    let status = Command::new("bash")
        .arg(&helper)
        .arg("verify")
        .args(&common)
        .arg("--json")
        .output()?;
    if !status.status.success() {
        let result = Command::new("bash")
            .arg(&helper)
            .arg("install")
            .args(&common)
            .output()?;
        if !result.status.success() {
            bail!("private hook provisioning failed; inspect the account locally");
        }
    }
    Ok(())
}

pub(super) fn execute(command: Vec<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    if std::env::var("LOOM_PRIVATE_WORKSPACE").as_deref() != Ok("1") {
        bail!("private execution context missing");
    }
    control::audit()?;
    control::audit_args(&command)?;
    let role = std::env::var("LOOM_ROLE").unwrap_or_default();
    if !role.is_empty() {
        crate::runtime_admission::resolve_and_admit(Path::new(REPO), &role, Some("codex"))
            .map_err(|e| anyhow::anyhow!(e.diagnostic()))?;
    }
    let (bin, args) = command.split_first().context("private command missing")?;
    let mut child = Command::new(bin);
    child.args(args).current_dir(REPO);
    // The last check before the model runs, on the boundary the host bound at
    // admission. Nothing has started yet — the session was proven idle — so a
    // boundary that is Ready here is the boundary Codex will read its hook
    // registration from, and the forced policy is applied on this exact exec.
    control_boundary(&mut child)?;
    let error = child.exec();
    Err(error.into())
}

/// Recheck the bound control identity in-container and force the pinned policy.
/// The bound identity arrives in this process's environment from the host's
/// `docker exec`, which the worker cannot write.
fn control_boundary(child: &mut Command) -> Result<()> {
    let bound = std::env::var("LOOM_PRIVATE_CONTROL").unwrap_or_default();
    let root = Path::new(bundle::CONTROL_ROOT);
    if bound.is_empty() && bundle::manifest(root)?.is_none() {
        // A session image without the control boundary keeps the pre-#8839
        // behavior of host and shared-mount sessions: unchanged, and still
        // gated by the unchanged `partial` hook/worktree-isolation capability.
        return Ok(());
    }
    bundle::rebind(&bundle::observe(root, Path::new(PROFILE)), Path::new(PROFILE), &bound)?;
    bundle::apply_policy(child);
    Ok(())
}

pub(super) fn check_host_job() -> Result<()> {
    // Presence in the private mount is stronger than a caller-controlled env
    // flag. A worker cannot unset a variable to turn SSH execution back on.
    if Path::new("/workspace/identity.json").exists()
        || std::env::var_os("LOOM_PRIVATE_WORKSPACE").is_some()
    {
        bail!("run-job host/SSH/Docker executors are unsupported in private-clone v1; run supported build commands directly in the owned clone");
    }
    Ok(())
}
