//! The selected account and its preparation lease travel together to spawn.
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

const LEASE_FD: i32 = 198;

pub struct Selection {
    pub name: String,
    profile: PathBuf,
    prepared: Option<Prepared>,
}

struct Prepared {
    lease: lease::Lease,
    record: Option<export::PreparedRecord>,
    handed_off: std::cell::Cell<bool>,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // Preparation completed and no child was given the descriptor. On a
        // preparation failure no Prepared is constructed: durable uncertainty
        // remains for the next idle-proof recovery.
        if !self.handed_off.get() {
            let _ = self.lease.finish();
            if let Some(record) = &self.record {
                let _ = record.rollback();
            }
        }
    }
}

/// Cheap inventory-only check, usable under a registry mutex. It never probes
/// Docker or credentials. Old lock-held retry paths must stop for recovery.
pub fn uses_private(root: &Path) -> Result<bool> {
    if super::super::paths::codex_profile_root().is_none() {
        return Ok(false);
    }
    for account in super::super::account_registry::account_inventory_quiet(
        root,
        super::super::account_registry::AccountProvider::Codex,
    )? {
        if state_dir(&account.credential_reference)?
            .join("workspace.json")
            .exists()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Selection {
    pub fn prepare(
        root: &Path,
        runtime: &str,
        model: Option<&str>,
        kind: JobKind,
        issue: Option<u64>,
        owner: &str,
    ) -> Result<Option<Self>> {
        export::check_runtime(root, issue, runtime)?;
        if runtime == "codex" {
            adapter::check_environment()?;
        }
        if runtime == "codex"
            && ["LOOM_CODEX_NO_EXEC", "LOOM_SPAWN_NO_EXPORT"]
                .iter()
                .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
        {
            export::check_failover(root, issue, "skip-resolution")?;
            return Ok(None);
        }
        let pinned = std::env::var_os("LOOM_CODEX_HOME")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()));
        if runtime == "codex" {
            if let Some(profile) = &pinned {
                let profile = PathBuf::from(profile).canonicalize()?;
                if !state_dir(&profile)?.join("workspace.json").exists() {
                    export::check_failover(root, issue, "legacy-explicit-profile")?;
                    return Ok(None);
                }
            }
        }
        if runtime != "codex" || !uses_private(root)? {
            export::check_failover(root, issue, "unavailable")?;
            return Ok(None);
        }
        use super::super::account_registry::{
            account_inventory, select_account, AccountBinding, AccountProvider,
        };
        let inventory = account_inventory(root, AccountProvider::Codex)?;
        let explicit = std::env::var("LOOM_CODEX_PROFILE")
            .ok()
            .filter(|v| !v.is_empty());
        let (name, profile) = if let Some(profile) = pinned {
            let profile = PathBuf::from(profile).canonicalize()?;
            let account = inventory
                .iter()
                .find(|a| a.credential_reference.canonicalize().ok().as_ref() == Some(&profile))
                .context("explicit Codex profile is not an inventoried account")?;
            (account.id.name.clone(), profile)
        } else if let Some(name) = explicit {
            let account = account(root, &name)?;
            (account.id.name, account.credential_reference)
        } else {
            let manifest = [
                root.join(".loom/runtimes/codex.json"),
                root.join("defaults/runtimes/codex.json"),
            ]
            .into_iter()
            .find(|p| p.is_file());
            let provider = manifest
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| value["accountProvider"].as_str().map(str::to_owned));
            if provider.as_deref() != Some("codex") {
                export::check_failover(root, issue, "other-provider")?;
                return Ok(None);
            }
            let account = select_account(root, AccountProvider::Codex, model)?;
            let AccountBinding::CodexHome { directory } = account.binding else {
                unreachable!()
            };
            (account.id.name, directory)
        };
        let mut selection = Self {
            name,
            profile,
            prepared: None,
        };
        let dir = state_dir(&selection.profile)?;
        if !dir.join("workspace.json").exists() {
            export::check_failover(root, issue, &selection.name)?;
            return Ok(Some(selection));
        }
        adapter::check(Some(&selection.profile))?;
        let (config, dir) = lifecycle::resolve(root, &selection.name)?;
        export::check_failover(root, issue, &selection.name)?;
        verify_repository(root, &config.repository)?;
        let lease = lease::Lease::acquire(&dir)?;
        let state = docker::inspect(&config.container)?
            .context("private account session is unavailable (container stopped or missing)")?;
        docker::validate(&config, &state)?;
        lease.recover(&config, Some(&state))?;
        if state["State"]["Running"] != true || !docker::idle(Some(&state), &config.container)? {
            bail!("private account session is busy or not running; workspace was not claimed");
        }
        let id = state["Id"].as_str().context("container ID missing")?;
        let mut job = lease::Job {
            owner: owner.into(),
            kind,
            issue,
            branch: issue.map(|n| format!("feature/issue-{n}")),
            container_id: id.into(),
            base_revision: String::new(),
            host_pid: std::process::id(),
        };
        lease.begin(&job)?;
        // The issue helper owns creation of its worktree/branch in the clone.
        job.base_revision = docker::prepare(&config, id, None)?;
        docker::command(&[
            "exec",
            &config.container,
            "loom-daemon",
            "private-workspace",
            "setup",
        ])?;
        lease.begin(&job)?;
        let record = export::record_prepared(root, &config, &job)?;
        selection.prepared = Some(Prepared {
            lease,
            record: Some(record),
            handed_off: std::cell::Cell::new(false),
        });
        Ok(Some(selection))
    }

    pub fn rebind(&self, _root: &Path, owner: &str) -> Result<()> {
        if let Some(prepared) = &self.prepared {
            let mut job =
                lease::read(&prepared.lease.dir)?.context("prepared lease disappeared")?;
            job.owner = owner.into();
            prepared.lease.begin(&job)?;
            if let Some(record) = &prepared.record {
                record.rebind(owner)?;
            }
        }
        Ok(())
    }
    pub(super) fn lease(&self) -> Option<&lease::Lease> {
        self.prepared.as_ref().map(|p| &p.lease)
    }
    pub fn spawned(&self) {
        if let Some(p) = &self.prepared {
            p.handed_off.set(true);
        }
    }

    pub fn apply(&self, command: &mut Command) {
        command
            .env("LOOM_CODEX_HOME", &self.profile)
            .env("LOOM_ACCOUNT_NAME", &self.name)
            .env("LOOM_ACCOUNT_PROVIDER", "codex");
        if let Some(prepared) = &self.prepared {
            let fd = prepared.lease.fd();
            command.env("LOOM_PRIVATE_LEASE_FD", LEASE_FD.to_string());
            // Runs after fork, before exec, on this Command only. The daemon's
            // descriptor remains CLOEXEC; unrelated launches cannot inherit it.
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd, LEASE_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(LEASE_FD, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
}

pub(super) fn inherited_lease(dir: &Path) -> Result<Option<lease::Lease>> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;
    let Some(value) = std::env::var_os("LOOM_PRIVATE_LEASE_FD") else {
        return Ok(None);
    };
    if value != LEASE_FD.to_string().as_str() {
        bail!("invalid private lease descriptor");
    }
    if unsafe { libc::fcntl(LEASE_FD, libc::F_GETFD) } < 0 {
        bail!("private lease descriptor is closed");
    }
    let file = unsafe { std::fs::File::from_raw_fd(LEASE_FD) };
    let expected = std::fs::symlink_metadata(dir.join("account.lock"))?;
    let actual = file.metadata()?;
    if !expected.is_file() || expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        bail!("private lease descriptor does not own the selected account");
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("private lease descriptor is not exclusively owned");
    }
    // Only the supervisor keeps the descriptor; Docker clients cannot hold it.
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(Some(lease::Lease::from_file(file, dir.to_owned())))
}

fn verify_repository(root: &Path, expected: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()?;
    let remote = String::from_utf8(output.stdout)?;
    let remote = remote.trim();
    let normalized = if let Some(ssh) = remote.strip_prefix("git@") {
        ssh.split_once(':')
            .map(|(host, path)| format!("https://{host}/{path}"))
    } else {
        Some(remote.to_owned())
    };
    if !output.status.success()
        || normalized.as_deref().map(|s| s.trim_end_matches(".git"))
            != Some(expected.trim_end_matches(".git"))
    {
        bail!("private workspace repository differs from the logical host repository");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reservation(dir: &Path) -> Selection {
        let lease = lease::Lease::acquire(dir).unwrap();
        lease
            .begin(&lease::Job {
                owner: "test".into(),
                kind: JobKind::Sweep,
                issue: Some(7),
                branch: None,
                container_id: "a".repeat(64),
                base_revision: String::new(),
                host_pid: std::process::id(),
            })
            .unwrap();
        Selection {
            name: "fixture".into(),
            profile: dir.join("profile"),
            prepared: Some(Prepared {
                lease,
                record: None,
                handed_off: std::cell::Cell::new(false),
            }),
        }
    }
    #[test]
    #[serial_test::serial(private_workspace_fork)]
    fn failed_spawn_releases_prepared_reservation_without_a_child() {
        let dir = tempfile::tempdir().unwrap();
        let selection = reservation(dir.path());
        let mut command = Command::new("/nonexistent/loom-private-test");
        selection.apply(&mut command);
        assert!(command.spawn().is_err());
        drop(selection);
        assert!(lease::Lease::acquire(dir.path()).is_ok());
        assert!(!dir.path().join("job.json").exists());
    }
    #[test]
    #[serial_test::serial(private_workspace_fork)]
    fn child_inherits_same_account_lock_after_dispatcher_drops_its_copy() {
        let dir = tempfile::tempdir().unwrap();
        let selection = reservation(dir.path());
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2"]);
        selection.apply(&mut command);
        let mut child = command.spawn().unwrap();
        selection.spawned();
        drop(selection);
        assert!(lease::Lease::acquire(dir.path()).is_err());
        assert!(dir.path().join("job.json").exists());
        assert!(child.wait().unwrap().success());
        assert!(lease::Lease::acquire(dir.path()).is_ok());
        // Process exit releases the flock, not the durable uncertainty fence.
        assert!(dir.path().join("job.json").exists());
    }
}
