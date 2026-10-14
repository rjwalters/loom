//! The selected account and its preparation lease travel together to spawn.
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The one descriptor a private dispatch hands its account lease down on.
/// Also the only value `roll_pause::resume::release_inherited_lease` accepts.
pub(crate) const LEASE_FD: i32 = 198;

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
            account_inventory, select_codex_account_where, AccountBinding, AccountProvider,
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
            // Only accounts that can serve THIS repository: a shared
            // (host-mounted) account, or a private-clone account whose clone
            // is of this repository. A private account bound to another
            // repository would otherwise be picked at random and the launch
            // refused below by `verify_repository` — on robb-studio after
            // agent-1/2 moved to a gf180-parasynth canary (2026-10-02), every
            // Codex role tick in every other workspace that drew one of them
            // failed with "private workspace repository differs".
            let logical = logical_repository(root);
            let account = select_codex_account_where(root, model, |account| {
                serves_repository(&account.credential_reference, logical.as_deref())
            })?;
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
            control: String::new(),
        };
        lease.begin(&job)?;
        // The issue helper owns creation of its worktree/branch in the clone.
        job.base_revision = docker::prepare(&config, id, None)?;
        docker::setup(&config)?;
        // Bind the verified control boundary to this account/container/lease
        // BEFORE any mutable work is admitted. Spawn rechecks the same identity.
        job.control = docker::control(&config, id)?;
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
    /// Re-verify this prepared selection as containment evidence (#8787).
    ///
    /// Everything is re-measured, nothing is remembered: the owned lease's
    /// durable job, the bound container re-inspected **by immutable ID** (not
    /// by its mutable Docker name) with its settings, mounts and exclusive
    /// volume/profile ownership re-validated, and the in-container
    /// `loom-private-control-v1` boundary re-observed and matched against the
    /// identity bound at preparation. Any drift since preparation — a
    /// replaced, renamed, stopped or peer-attached container, a changed mount
    /// topology, an altered guard bundle, registration or profile control
    /// file — fails closed here, before admission.
    ///
    /// # Errors
    /// A fixed, secret-free obligation string naming what could not be proven.
    pub fn contain(&self, root: &Path) -> Result<containment::ContainmentProof> {
        let prepared = self.prepared.as_ref().context(containment::UNPREPARED)?;
        let job = lease::read(&prepared.lease.dir)?.context(containment::UNPREPARED)?;
        let (config, dir) = lifecycle::resolve(root, &self.name)?;
        if dir != prepared.lease.dir || job.base_revision.is_empty() {
            bail!("{}", containment::UNPREPARED);
        }
        let state = docker::inspect_id(&job.container_id)?.context(containment::DRIFTED)?;
        docker::validate(&config, &state)?;
        if state["Id"] != job.container_id.as_str() || state["State"]["Running"] != true {
            bail!("{}", containment::DRIFTED);
        }
        let report = docker::recheck_control(&config, &job.container_id, &job.control)?;
        containment::host_proof(&config, &report, &job)
    }

    /// Persist the secret-free containment provenance beside the durable job,
    /// so `session status` and post-session recovery can both see exactly what
    /// this admission relied on.
    ///
    /// # Errors
    /// When the selection holds no prepared lease, or the record cannot be
    /// written durably.
    pub fn record_admission(
        &self,
        admitted: &crate::runtime_admission::ResolvedRuntime,
    ) -> Result<()> {
        let prepared = self.prepared.as_ref().context(containment::UNPREPARED)?;
        save(
            &prepared.lease.dir.join(lease::ADMISSION),
            &serde_json::json!({
                "role": admitted.role,
                "runtime": admitted.runtime,
                "source": admitted.source,
                "execution": admitted.execution,
            }),
        )
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

/// The workspace's logical repository: its `origin` URL, with an SSH
/// `git@host:path` remote written as `https://host/path` and any `.git`
/// suffix dropped. `None` when it cannot be read.
fn logical_repository(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let remote = String::from_utf8(output.stdout).ok()?;
    let remote = remote.trim();
    let normalized = if let Some(ssh) = remote.strip_prefix("git@") {
        let (host, path) = ssh.split_once(':')?;
        format!("https://{host}/{path}")
    } else {
        remote.to_owned()
    };
    Some(normalized.trim_end_matches(".git").to_owned())
}

/// Whether the account whose profile is `profile` can serve a launch for the
/// `logical` repository: any account without private-clone state can (it is a
/// shared, host-mounted account), and a private-clone account can only when
/// its clone is of that repository. Unreadable private state, or an
/// unresolvable workspace repository, never matches a private account: it is
/// left out of the draw rather than picked and refused.
fn serves_repository(profile: &Path, logical: Option<&str>) -> bool {
    let Ok(dir) = state_dir(profile) else {
        return true;
    };
    if !dir.join("workspace.json").exists() {
        return true;
    }
    match (load(&dir), logical) {
        (Ok(config), Some(logical)) => config.repository.trim_end_matches(".git") == logical,
        _ => false,
    }
}

fn verify_repository(root: &Path, expected: &str) -> Result<()> {
    if logical_repository(root).as_deref() != Some(expected.trim_end_matches(".git")) {
        bail!("private workspace repository differs from the logical host repository");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A workspace whose `origin` is `remote`.
    fn workspace_with_origin(remote: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["remote", "add", "origin", remote]);
        dir
    }

    /// Make `<root>/<name>` a private-clone account profile whose clone is of
    /// `repository` (the state `lifecycle::start` writes).
    fn private_profile(root: &Path, name: &str, repository: &str) -> PathBuf {
        let profile = root.join(name);
        std::fs::create_dir_all(&profile).unwrap();
        let dir = state_dir(&profile).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            schema_version: 1,
            account: name.into(),
            container: format!("loom-codex-session-{name}"),
            engine: "engine".into(),
            docker_desktop: false,
            repository: repository.into(),
            base: "main".into(),
            volume: format!("loom-codex-workspace-{name}"),
            profile: profile.canonicalize().unwrap(),
            gh_config: None,
        };
        save(&dir.join("workspace.json"), &config).unwrap();
        profile
    }

    #[test]
    fn logical_repository_normalizes_ssh_https_and_dot_git() {
        let _fork = super::super::tests::fork_guard();
        for remote in [
            "git@github.com:2AMLogic/gf180-parasynth.git",
            "https://github.com/2AMLogic/gf180-parasynth.git",
            "https://github.com/2AMLogic/gf180-parasynth",
        ] {
            let ws = workspace_with_origin(remote);
            assert_eq!(
                logical_repository(ws.path()).as_deref(),
                Some("https://github.com/2AMLogic/gf180-parasynth"),
                "{remote}"
            );
        }
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(logical_repository(bare.path()), None);
    }

    #[test]
    fn only_shared_accounts_and_private_clones_of_this_repository_serve_it() {
        let _fork = super::super::tests::fork_guard();
        let root = tempfile::tempdir().unwrap();
        let here = "https://github.com/2AMLogic/gf180-parasynth";
        let shared = root.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let same = private_profile(root.path(), "same", &format!("{here}.git"));
        let other = private_profile(root.path(), "other", "https://github.com/2AMLogic/vibesql");
        assert!(serves_repository(&shared, Some(here)));
        assert!(serves_repository(&same, Some(here)), ".git suffix is not identity");
        assert!(!serves_repository(&other, Some(here)));
        // An unresolvable workspace repository matches no private clone, but
        // shared accounts still serve it.
        assert!(serves_repository(&shared, None));
        assert!(!serves_repository(&same, None));
        // Unreadable private state is left out, never guessed.
        let corrupt = private_profile(root.path(), "corrupt", here);
        std::fs::write(state_dir(&corrupt).unwrap().join("workspace.json"), "{not json").unwrap();
        assert!(!serves_repository(&corrupt, Some(here)));
    }

    /// The robb-studio failure (2026-10-02): with one account moved to a
    /// private clone of another repository, pool selection for this workspace
    /// drew it at random and the launch died on `verify_repository`. Selection
    /// now never offers it, and a pool with nothing that serves this
    /// repository fails as an empty pool rather than widening.
    #[test]
    #[serial_test::serial]
    fn pool_selection_never_draws_a_private_clone_of_another_repository() {
        use super::super::super::account_registry::select_codex_account_where;
        use super::super::super::paths::SHARED_ACCOUNTS_ROOT_ENV;
        let _fork = super::super::tests::fork_guard();
        let here = "https://github.com/2AMLogic/gf180-parasynth";
        let ws = workspace_with_origin(&format!("{here}.git"));
        let shared_root = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        for name in ["agent-3", "agent-4"] {
            std::fs::create_dir_all(profiles.path().join(name)).unwrap();
        }
        private_profile(profiles.path(), "agent-1", "https://github.com/2AMLogic/vibesql");
        private_profile(profiles.path(), "agent-2", "https://github.com/2AMLogic/vibesql");
        let accounts = ["agent-1", "agent-2", "agent-3", "agent-4"]
            .iter()
            .map(|name| {
                format!(
                    r#"{{"provider":"codex","name":"{name}","credential_kind":"codex_home","credential_reference":"{name}","enabled":true}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        std::fs::create_dir_all(ws.path().join(".loom")).unwrap();
        std::fs::write(
            ws.path().join(".loom/accounts.json"),
            format!(r#"{{"version":1,"accounts":[{accounts}]}}"#),
        )
        .unwrap();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared_root.path());
        let _profile_root =
            crate::tokens_pool::profile_root_env::ProfileRootEnv::set(profiles.path());
        let logical = logical_repository(ws.path());
        let mut drawn = std::collections::BTreeSet::new();
        for _ in 0..40 {
            let account = select_codex_account_where(ws.path(), None, |account| {
                serves_repository(&account.credential_reference, logical.as_deref())
            })
            .unwrap();
            drawn.insert(account.id.name);
        }
        // Nothing but the private clones of another repository: empty pool.
        let none = select_codex_account_where(ws.path(), None, |account| {
            serves_repository(&account.credential_reference, logical.as_deref())
                && account.id.name.starts_with("agent-1")
        });
        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
        assert!(
            drawn
                .iter()
                .all(|name| name == "agent-3" || name == "agent-4"),
            "{drawn:?}"
        );
        assert!(none.is_err());
    }
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
                control: "c".repeat(64),
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
        let _fork = super::super::tests::fork_guard();
        let dir = tempfile::tempdir().unwrap();
        let selection = reservation(dir.path());
        let mut command = Command::new("/nonexistent/loom-private-test");
        selection.apply(&mut command);
        assert!(command.spawn().is_err());
        drop(selection);
        // #10955: the child that failed to exec held fd 198 without
        // close-on-exec. On macOS such a child's `flock` can outlive its reap
        // by up to ~1 s under load (measured; a failed exec holding only
        // close-on-exec descriptors never does), and any other test's child
        // forked while the lease was held keeps a copy until it execs (#9409),
        // so the release is waited for, within a bound, rather than read once.
        drop(super::super::tests::reacquire(dir.path()));
        assert!(!dir.path().join("job.json").exists());
    }
    #[test]
    #[serial_test::serial(private_workspace_fork)]
    fn child_inherits_same_account_lock_after_dispatcher_drops_its_copy() {
        let _fork = super::super::tests::fork_guard();
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
        // Free once the child exits — modulo another test's fork still holding
        // a copy (#9409), which `reacquire` waits out within a bound.
        drop(super::super::tests::reacquire(dir.path()));
        // Process exit releases the flock, not the durable uncertainty fence.
        assert!(dir.path().join("job.json").exists());
    }
}
