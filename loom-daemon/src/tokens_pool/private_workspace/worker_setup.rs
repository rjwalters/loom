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
    // Loom's own source installs ignored helper symlinks. Consumer repositories
    // carry installed helpers in Git. Never copy host scripts into the worker.
    for (installed, source) in [
        (".loom/scripts", "defaults/scripts"),
        (".loom/hooks", "defaults/hooks"),
        (".loom/roles", "defaults/roles"),
        (".claude/commands/loom", "defaults/.claude/commands/loom"),
    ] {
        let target = repo.join(installed);
        if !target.exists() && repo.join(source).is_dir() {
            std::fs::create_dir_all(target.parent().unwrap())?;
            std::os::unix::fs::symlink(repo.join(source), &target)?;
        }
        if !target.canonicalize()?.starts_with(repo) {
            bail!("installed private helper escaped its clone");
        }
    }
    control::audit()?;
    let helper = repo.join(".loom/scripts/provision-codex-hooks.sh");
    if helper.is_file() {
        let status = Command::new("bash")
            .arg(&helper)
            .args([
                "verify",
                "--codex-home",
                PROFILE,
                "--workspace",
                REPO,
                "--json",
            ])
            .output()?;
        if !status.status.success() {
            let result = Command::new("bash")
                .arg(&helper)
                .args(["install", "--codex-home", PROFILE, "--workspace", REPO])
                .output()?;
            if !result.status.success() {
                bail!("private hook provisioning failed; inspect the account locally");
            }
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
    // The transport binds these from the host-owned lease after re-validating
    // the container by ID; a missing or stale value fails closed (#8787).
    let container_id = std::env::var("LOOM_PRIVATE_CONTAINER_ID").unwrap_or_default();
    let revision = std::env::var("LOOM_PRIVATE_BASE_REVISION").unwrap_or_default();
    containment::evidence(&container_id, &revision).map_err(|_| {
        anyhow::anyhow!("{}", containment::PolicyStatus::ContextInvalid.obligation())
    })?;
    let role = std::env::var("LOOM_ROLE").unwrap_or_default();
    if containment::mutable(&role) {
        // Mutable roles need the verified clone AND the managed policy bridge
        // (trusted, byte-identical to the base revision). Containment satisfies
        // only repository isolation; every other requirement is still checked.
        let account = std::env::var("LOOM_ACCOUNT_NAME").unwrap_or_default();
        let proof = containment::in_container_proof(&account, &container_id, &revision)?;
        let admitted = containment::admit_with(Path::new(REPO), &role, Some("codex"), &proof)
            .map_err(|e| anyhow::anyhow!(e.diagnostic()))?;
        if let Some(execution) = &admitted.execution {
            eprintln!(
                "{}{}",
                crate::runtime_admission::CONTAINMENT_LOG_MARKER,
                execution.summary()
            );
        }
    } else if !role.is_empty() {
        crate::runtime_admission::resolve_and_admit(Path::new(REPO), &role, Some("codex"))
            .map_err(|e| anyhow::anyhow!(e.diagnostic()))?;
    }
    let (bin, args) = command.split_first().context("private command missing")?;
    let error = Command::new(bin).args(args).current_dir(REPO).exec();
    Err(error.into())
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
