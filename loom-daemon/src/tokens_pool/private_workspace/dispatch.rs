//! The selected account and its preparation lease travel together to spawn.
use super::*;
use std::cell::Cell;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Every matching seat is busy or could not be prepared; self-heals as seats free.
pub(super) const NO_IDLE_SEAT: &str = "no idle seat";
/// The resolved repository is served by no inventoried account: a routing
/// misconfiguration, not contention (#14512).
pub(super) const NO_SEAT_SERVES_REPOSITORY: &str = "no seat serves this repository";

/// The one descriptor a private dispatch hands its account lease down on.
/// Also the only value `roll_pause::resume::release_inherited_lease` accepts.
pub(crate) const LEASE_FD: i32 = 198;

pub struct Selection {
    pub name: String,
    profile: PathBuf,
    prepared: Option<Prepared>,
    /// The containment proof this selection's own admission walk captured
    /// (#14626). The walk proves every winning candidate before it can win;
    /// the admission that consumes this selection runs against exactly that
    /// proof instead of re-proving the same seat a second Docker
    /// inspect-and-exec round-trip later. Never carried across an await, a
    /// lock or a second dispatch: it is spent by the one admission it was
    /// taken for, and anything later proves fresh.
    proven: Option<containment::ContainmentProof>,
}

struct Prepared {
    lease: lease::Lease,
    /// The clone THIS execution was prepared in (#10796). Every later proof,
    /// admission and launch is bound to it, never to the account's primary.
    target: Target,
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

enum SeatPolicy {
    /// Containment admission (#8787): every eligible seat is tried at most
    /// once, and a candidate only wins when a containment proof can be taken
    /// for it — the winning lease, proof and launch stay bound to one seat.
    Contained,
    /// Launch through the admitted runtime's own preparation: a busy seat is
    /// passed over for the next eligible one, with no containment requirement
    /// of its own.
    Existing,
    Idle {
        guarded: bool,
    },
}

/// Why the bounded seat walk passed one candidate over.
enum SeatOutcome {
    /// Another holder owns the seat's exclusive lease, or its session is
    /// already running work.
    Busy,
    /// The seat was idle but could not be prepared — a containment proof that
    /// could not be taken, a stopped session, an expired credential. Carries
    /// that refusal's own text, so an idle seat is never counted as busy.
    Failed(String),
}

/// Every eligible seat was tried once and none could be prepared. Names each
/// seat and why it was passed over, so a refusal can be read (#14578).
#[derive(Debug)]
pub(super) struct NoEligibleSeat {
    summary: String,
    /// Every tried seat was busy: the pool has no idle seat at all. A seat
    /// that failed for any other reason never turns this on.
    pub(super) all_busy: bool,
}

impl std::fmt::Display for NoEligibleSeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

impl std::error::Error for NoEligibleSeat {}

/// One line: refusal texts ride host logs and draw rows, never multi-line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The error for a walk that exhausted every eligible seat.
fn exhausted_error(passed_over: &[(String, SeatOutcome)]) -> NoEligibleSeat {
    let busy = passed_over
        .iter()
        .filter(|(_, outcome)| matches!(outcome, SeatOutcome::Busy))
        .count();
    let detail = passed_over
        .iter()
        .map(|(name, outcome)| match outcome {
            SeatOutcome::Busy => format!("{name} busy"),
            SeatOutcome::Failed(error) => format!("{name} failed: {error}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    NoEligibleSeat {
        summary: format!(
            "no eligible seat could be prepared ({} of {} eligible seats idle): {}",
            passed_over.len() - busy,
            passed_over.len(),
            detail
        ),
        all_busy: busy == passed_over.len(),
    }
}

#[cfg(test)]
impl Selection {
    /// A held reservation (lease taken, job begun) for tests in other modules,
    /// shaped like the one `reserve_seat` hands a read-only launch (#14481).
    pub(crate) fn reserved_for_test(dir: &Path) -> Self {
        let lease = lease::Lease::acquire(dir).unwrap();
        lease
            .begin(&lease::Job {
                owner: "test".into(),
                kind: JobKind::Role,
                issue: None,
                branch: None,
                container_id: "a".repeat(64),
                base_revision: String::new(),
                host_pid: std::process::id(),
                control: "c".repeat(64),
                repository: String::new(),
            })
            .unwrap();
        let config = Config {
            schema_version: 1,
            account: "fixture".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/owner/repo".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: PathBuf::from("/nonexistent"),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        };
        Selection {
            name: "fixture".into(),
            profile: dir.join("profile"),
            prepared: Some(Prepared {
                lease,
                target: Config::primary(&config),
                record: None,
                handed_off: std::cell::Cell::new(false),
            }),
            proven: None,
        }
    }
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
        Self::prepare_with(root, runtime, model, kind, issue, owner, SeatPolicy::Existing)
    }

    /// Reserve a seat for a host-admitted read-only launch. Try each eligible
    /// seat at most once, and keep the successful lease through the spawn.
    pub(super) fn prepare_idle(
        root: &Path,
        model: Option<&str>,
        kind: JobKind,
        issue: Option<u64>,
        owner: &str,
        guarded: bool,
    ) -> Result<Option<Self>> {
        Self::prepare_with(root, "codex", model, kind, issue, owner, SeatPolicy::Idle { guarded })
    }

    /// Prepare a selection for containment admission (#8787, #14578): the
    /// bounded walk tries each eligible seat at most once, and a candidate
    /// only wins when its OWN containment can be proven — so one busy seat
    /// never refuses a draw another idle seat could serve. Explicit profile
    /// pins keep binding: they never walk. The winning proof is carried on
    /// the returned selection (#14626): admission consumes it through
    /// [`Selection::proof_for_admission`] instead of re-proving the seat.
    pub(super) fn prepare_contained(
        root: &Path,
        model: Option<&str>,
        kind: JobKind,
        issue: Option<u64>,
        owner: &str,
    ) -> Result<Option<Self>> {
        Self::prepare_with(root, "codex", model, kind, issue, owner, SeatPolicy::Contained)
    }

    fn prepare_with(
        root: &Path,
        runtime: &str,
        model: Option<&str>,
        kind: JobKind,
        issue: Option<u64>,
        owner: &str,
        policy: SeatPolicy,
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
        let try_idle_seats = matches!(policy, SeatPolicy::Idle { .. });
        let prepare = |name, profile| -> Result<Self> {
            let mut selection = Self::prepare_profile(root, name, profile, kind, issue, owner)?;
            match policy {
                // A guarded read-only reservation must already prove
                // containment for its own seat (#14481).
                SeatPolicy::Idle { guarded: true } => {
                    if selection.lease().is_some() {
                        selection.contain(root)?;
                    }
                }
                // Containment admission: an account with no private clone,
                // and a clone whose boundary cannot be re-proven, cannot win.
                // The walk reports each refusal with its own reason and tries
                // the next eligible seat (#14578). The proof captured here is
                // carried on the selection so the admission that consumes it
                // need not take the same proof twice (#14626).
                SeatPolicy::Contained => {
                    if selection.lease().is_none() {
                        bail!("selected Codex account holds no private-clone workspace");
                    }
                    selection.prove(root)?;
                }
                _ => {}
            }
            Ok(selection)
        };
        if let Some(profile) = pinned {
            let profile = PathBuf::from(profile).canonicalize()?;
            let account = account_inventory(root, AccountProvider::Codex)?
                .into_iter()
                .find(|a| a.credential_reference.canonicalize().ok().as_ref() == Some(&profile))
                .context("explicit Codex profile is not an inventoried account")?;
            return prepare(account.id.name, profile).map(Some);
        }
        if let Some(name) = std::env::var("LOOM_CODEX_PROFILE")
            .ok()
            .filter(|v| !v.is_empty())
        {
            let account = account(root, &name)?;
            return prepare(account.id.name, account.credential_reference).map(Some);
        }
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

        let logical = logical_repository(root);
        let mut tried = std::collections::BTreeSet::new();
        let mut passed_over: Vec<(String, SeatOutcome)> = Vec::new();
        loop {
            // A previous preparation establishes a match even if that
            // account disappears from a later selection snapshot. Unknown
            // routes also prevent a definite routing-miss diagnosis.
            let routing_miss = Cell::new(logical.is_some() && tried.is_empty());
            let account = select_codex_account_where(root, model, |account| {
                if tried.contains(&account.id.name) {
                    return false;
                }
                let route = repository_route(&account.credential_reference, logical.as_deref());
                if route != Some(false) {
                    routing_miss.set(false);
                }
                route == Some(true)
            });
            let account = match account {
                Err(error) if error.is::<super::super::health::NoHealthyAccountError>() => {
                    // A routing miss (no eligible seat serves this repository)
                    // is neither "busy" nor a failed proof, so it keeps its
                    // own diagnosis on every policy. It is judged from the
                    // selector's snapshot, before health/model filtering, and
                    // can only hold while nothing has been tried yet.
                    if routing_miss.get() {
                        return Err(error.context(NO_SEAT_SERVES_REPOSITORY));
                    }
                    if try_idle_seats {
                        return Err(error.context(NO_IDLE_SEAT));
                    }
                    if passed_over.is_empty() {
                        // Nothing eligible was ever offered: keep the pool's
                        // own diagnosis (no healthy account, no model match).
                        return Err(error);
                    }
                    return Err(exhausted_error(&passed_over).into());
                }
                other => other?,
            };
            let AccountBinding::CodexHome { directory } = account.binding else {
                unreachable!()
            };
            match prepare(account.id.name.clone(), directory) {
                Ok(selection) => return Ok(Some(selection)),
                Err(error) => {
                    log::debug!("Codex seat {} could not be prepared: {error}", account.id.name);
                    let outcome = if error.is::<super::AccountBusy>() {
                        SeatOutcome::Busy
                    } else {
                        SeatOutcome::Failed(one_line(&error.to_string()))
                    };
                    passed_over.push((account.id.name.clone(), outcome));
                    tried.insert(account.id.name);
                }
            }
        }
    }

    fn prepare_profile(
        root: &Path,
        name: String,
        profile: PathBuf,
        kind: JobKind,
        issue: Option<u64>,
        owner: &str,
    ) -> Result<Self> {
        let mut selection = Self {
            name,
            profile,
            prepared: None,
            proven: None,
        };
        let dir = state_dir(&selection.profile)?;
        if !dir.join("workspace.json").exists() {
            export::check_failover(root, issue, &selection.name)?;
            return Ok(selection);
        }
        adapter::check(Some(&selection.profile))?;
        let (config, dir) = lifecycle::resolve(root, &selection.name)?;
        export::check_failover(root, issue, &selection.name)?;
        let target = select_target(root, &config)?;
        // Refuse BEFORE the lease, the container exec and the claim when the
        // only forge credential this owner has is provably expired (#14435).
        // Preparing would `git fetch` with a dead token, exit 128 and strand a
        // step the seat was already handed; refusing here lets the draw fall
        // through to a runtime that can serve it.
        if forge_auth::ExecAuth::resolve(root, &target.repository).expired() {
            bail!(
                "private workspace forge credential for {} is expired and no fresher one is \
                 published; the credential refresh has not renewed it",
                target.repository
            );
        }
        let lease = lease::Lease::acquire(&dir)?;
        let state = docker::inspect(&config.container)?
            .context("private account session is unavailable (container stopped or missing)")?;
        docker::validate(&config, &state)?;
        lease.recover(&config, Some(&state))?;
        if state["State"]["Running"] != true {
            bail!("private account session is not running; workspace was not claimed");
        }
        if !docker::idle(Some(&state), &config.container)? {
            return Err(
                AccountBusy("private account session is busy; workspace was not claimed").into()
            );
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
            repository: job_repository(&target),
        };
        lease.begin(&job)?;
        // The issue helper owns creation of its worktree/branch in the clone.
        job.base_revision = docker::prepare_target(&config, root, &target, id, None)?;
        docker::setup(&config)?;
        // Bind the verified control boundary to this account/container/lease
        // BEFORE any mutable work is admitted. Spawn rechecks the same identity.
        job.control = docker::control(&config, id)?;
        lease.begin(&job)?;
        let record = export::record_prepared(root, &config, &job)?;
        selection.prepared = Some(Prepared {
            lease,
            target,
            record: Some(record),
            handed_off: std::cell::Cell::new(false),
        });
        Ok(selection)
    }

    /// Capture this selection's containment proof during the admission walk
    /// (#14626). The proof still comes from this selection's own [`contain`]
    /// — nothing is remembered from another seat or an earlier dispatch — it
    /// is simply kept for the admission that asked the walk to prove this
    /// candidate, instead of being dropped on the floor.
    fn prove(&mut self, root: &Path) -> Result<()> {
        let proof = self.contain(root)?;
        self.proven = Some(proof);
        Ok(())
    }

    /// The containment proof to admit against (#14626): the one this
    /// selection's own walk captured, or a fresh [`contain`] when none was.
    /// Single-use by design — the captured proof is spent here, so a later
    /// admission re-proves rather than reusing spent evidence.
    ///
    /// # Errors
    /// A fixed, secret-free obligation string naming what could not be proven.
    pub(super) fn proof_for_admission(
        &mut self,
        root: &Path,
    ) -> Result<containment::ContainmentProof> {
        match self.proven.take() {
            Some(proof) => Ok(proof),
            None => self.contain(root),
        }
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
        containment::host_proof(&config, &report, &job, &prepared.target)
    }

    /// The clone this selection was prepared in, when it holds a lease.
    pub(super) fn target(&self) -> Option<&Target> {
        self.prepared.as_ref().map(|p| &p.target)
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
pub(super) fn logical_repository(root: &Path) -> Option<String> {
    // Bounded (#9715): a hung `git` must not stall the launch.
    let remote = crate::forge_cmd::get_remote_url(root)?;
    let remote = remote.as_str();
    let normalized = if let Some(ssh) = remote.strip_prefix("git@") {
        let (host, path) = ssh.split_once(':')?;
        format!("https://{host}/{path}")
    } else {
        remote.to_owned()
    };
    Some(normalized.trim_end_matches(".git").to_owned())
}

/// Whether this profile can serve the logical repository, or `None` when
/// private routing evidence cannot be read. Only a confirmed match enters the
/// draw. Accounts without private-clone state remain eligible as shared seats.
fn repository_route(profile: &Path, logical: Option<&str>) -> Option<bool> {
    let Ok(dir) = state_dir(profile) else {
        return Some(true);
    };
    if !dir.join("workspace.json").exists() {
        return Some(true);
    }
    let config = load(&dir).ok()?;
    Some(config.target(Some(logical?)).is_ok())
}

/// What a durable job records for `target`: the canonical repository of an
/// additional clone, empty for the legacy primary (the `run_job` convention).
pub(super) fn job_repository(target: &Target) -> String {
    if target.slug.is_some() {
        target.repository.clone()
    } else {
        String::new()
    }
}

/// The clone of the logical host repository this account holds: its primary or
/// a registered additional one. An account that holds neither is refused, never
/// registered implicitly, and a firewalled repository is refused before any
/// clone is touched.
fn select_target(root: &Path, config: &Config) -> Result<Target> {
    let target = logical_repository(root)
        .and_then(|logical| config.target(Some(&logical)).ok())
        .context("private workspace repository differs from the logical host repository")?;
    if target.slug.is_some() {
        lifecycle::check_firewall(root, &identify(&target.repository)?)?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_git_helpers::run as git;

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
            repositories: vec![],
            volume: format!("loom-codex-workspace-{name}"),
            profile: profile.canonicalize().unwrap(),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
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
        assert_eq!(repository_route(&shared, Some(here)), Some(true));
        assert_eq!(repository_route(&same, Some(here)), Some(true), ".git suffix is not identity");
        assert_eq!(repository_route(&other, Some(here)), Some(false));
        // An unresolvable workspace repository matches no private clone, but
        // shared accounts still serve it.
        assert_eq!(repository_route(&shared, None), Some(true));
        assert_eq!(repository_route(&same, None), None);
        // Unreadable private state is left out, never guessed.
        let corrupt = private_profile(root.path(), "corrupt", here);
        std::fs::write(state_dir(&corrupt).unwrap().join("workspace.json"), "{not json").unwrap();
        assert_eq!(repository_route(&corrupt, Some(here)), None);
    }

    /// A seat holding several private clones serves each of its repositories
    /// (#10796), and the target selected for a workspace is that repository's
    /// own clone, not the account's primary. An unregistered one is refused.
    #[test]
    fn a_multi_repository_seat_serves_and_selects_each_registered_clone() {
        let _fork = super::super::tests::fork_guard();
        let root = tempfile::tempdir().unwrap();
        let primary = "https://github.com/2AMLogic/gf180-parasynth";
        let second = "https://github.com/2AMLogic/vibesql";
        let profile = private_profile(root.path(), "seat", primary);
        let dir = state_dir(&profile).unwrap();
        let mut config = load(&dir).unwrap();
        let id = identify(second).unwrap();
        config.repositories.push(Registered {
            repository: id.url.clone(),
            base: "main".into(),
            slug: id.slug.clone(),
        });
        save(&dir.join("workspace.json"), &config).unwrap();

        assert_eq!(repository_route(&profile, Some(primary)), Some(true));
        assert_eq!(repository_route(&profile, Some(&format!("{second}.git"))), Some(true));
        assert_eq!(
            repository_route(&profile, Some("https://github.com/2AMLogic/other")),
            Some(false)
        );

        let ws = workspace_with_origin(&format!("{second}.git"));
        let target = select_target(ws.path(), &config).unwrap();
        assert_eq!(target.path, format!("{WORK_ROOT}/{}", id.slug));
        assert_eq!(job_repository(&target), id.url);
        let ws = workspace_with_origin(primary);
        let target = select_target(ws.path(), &config).unwrap();
        assert_eq!(target.path, REPO);
        assert_eq!(
            job_repository(&target),
            "",
            "the primary stays the legacy empty job repository"
        );
        let ws = workspace_with_origin("https://github.com/2AMLogic/other");
        assert!(select_target(ws.path(), &config).is_err());
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
                repository_route(&account.credential_reference, logical.as_deref()) == Some(true)
            })
            .unwrap();
            drawn.insert(account.id.name);
        }
        // Nothing but the private clones of another repository: empty pool.
        let none = select_codex_account_where(ws.path(), None, |account| {
            repository_route(&account.credential_reference, logical.as_deref()) == Some(true)
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
    fn fixture_config() -> Config {
        Config {
            schema_version: 1,
            account: "fixture".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/owner/repo".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: PathBuf::from("/nonexistent"),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        }
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
                repository: String::new(),
            })
            .unwrap();
        Selection {
            name: "fixture".into(),
            profile: dir.join("profile"),
            prepared: Some(Prepared {
                lease,
                target: Config::primary(&fixture_config()),
                record: None,
                handed_off: std::cell::Cell::new(false),
            }),
            proven: None,
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
