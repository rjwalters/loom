use super::super::session_hold::{self, OperatorHeld};
use super::super::session_lifecycle::{
    container_name, mark_session_managed, DEFAULT_SESSION_IMAGE,
};
use super::*;

pub fn configured(workspace: &Path, name: &str) -> Result<bool> {
    let account = account(workspace, name)?;
    Ok(state_dir(&account.credential_reference)?
        .join("workspace.json")
        .exists())
}

pub(super) fn resolve(workspace: &Path, name: &str) -> Result<(Config, PathBuf)> {
    let account = account(workspace, name)?;
    let dir = state_dir(&account.credential_reference)?;
    let config =
        load(&dir).context("no private workspace; use session start --private-clone URL")?;
    if config.account != account.id.name
        || config.profile != account.credential_reference.canonicalize()?
    {
        bail!("private session profile/account changed; preserve existing workspace for recovery");
    }
    if config.engine != docker::engine()? {
        bail!("Docker engine changed; old worker liveness is unknown, restore the original Docker context");
    }
    Ok((config, dir))
}

/// Make `repository` one of the account's repositories. The primary and an
/// already-registered repository must keep their base; a new one is added
/// (lazily cloned by its first job). Runs under the idle account lease, so a
/// pre-#10795 single-repository account is migrated only while no worker can
/// be using it, and its volume, primary clone and unfinished work are untouched.
pub(super) fn register(
    workspace: &Path,
    config: &mut Config,
    repository: &str,
    base: &str,
) -> Result<()> {
    if let Ok(known) = config.target(Some(repository)) {
        if known.base != base {
            bail!("private repository {} is already configured with base {}; retain its clone and resolve unfinished work before changing the base", known.repository, known.base);
        }
        return Ok(());
    }
    let id = identify(repository)?;
    check_firewall(workspace, &id)?;
    if config.repositories.iter().any(|r| r.slug == id.slug) {
        bail!("private repository directory name collides with another configured repository");
    }
    config.schema_version = config.schema_version.max(2);
    config.repositories.push(Registered {
        repository: id.url,
        base: base.to_owned(),
        slug: id.slug,
    });
    Ok(())
}

/// Refuse a repository the fleet roster marks `firewall: true` (notebook,
/// corporate) before any clone: a cached roster that cannot be read fails closed.
pub(super) fn check_firewall(workspace: &Path, id: &RepoId) -> Result<()> {
    let records: Vec<(String, Option<String>)> =
        super::super::session_lifecycle::firewalled_records(workspace)
            .context("cannot read the firewall roster; refusing to clone")?
            .into_iter()
            .map(|(_, record)| (record.name, record.remote))
            .collect();
    if repository::firewalled(id, &records) {
        bail!("refusing to clone {}: the fleet roster marks it firewall: true", id.key);
    }
    Ok(())
}

pub fn start(
    workspace: &Path,
    name: &str,
    repository: &str,
    base: &str,
    image: Option<&str>,
) -> Result<Status> {
    let repository = repository_url(repository)?;
    valid_base(base)?;
    let account = account(workspace, name)?;
    let profile = account.credential_reference.canonicalize()?;
    let dir = state_dir(&profile)?;
    let lease = lease::Lease::acquire(&dir)?;
    let engine = docker::engine()?;
    let config = if dir.join("workspace.json").exists() {
        let mut existing = load(&dir)?;
        if existing.account != account.id.name
            || existing.profile != profile
            || existing.engine != engine
        {
            bail!("private workspace identity differs (account/profile/engine); retain its volume and resolve unfinished work before configuring a new session");
        }
        register(workspace, &mut existing, &repository, base)?;
        existing
    } else {
        let gh_config = std::env::var_os("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".config/gh")))
            .filter(|path| path.is_dir())
            .map(|path| path.canonicalize())
            .transpose()?;
        if let Some(path) = &gh_config {
            for parent in path.ancestors() {
                if parent.join(".git").exists() {
                    bail!("forge credentials must be outside every repository/worktree");
                }
            }
        }
        Config {
            schema_version: 1,
            account: account.id.name.clone(),
            container: container_name(&account.id.name),
            engine,
            docker_desktop: docker::desktop()?,
            repository,
            base: base.to_owned(),
            repositories: Vec::new(),
            volume: format!("loom-codex-workspace-{}", account.id.name),
            profile,
            gh_config,
        }
    };
    let state = docker::inspect(&config.container)?;
    if let Some(state) = &state {
        if image.is_some_and(|requested| state["Config"]["Image"] != requested) {
            bail!("session image differs from --image; stop the idle container before recreating it with the requested image");
        }
        docker::validate(&config, state)?;
    }
    lease.recover(&config, state.as_ref())?;
    if !docker::idle(state.as_ref(), &config.container)? {
        bail!("session has an unleased worker; stop it before preparing the workspace");
    }
    save(&dir.join("workspace.json"), &config)?;
    // An operator start lifts the hold an operator `stop` left, before the
    // first mutation, so a failure past this point leaves the session down
    // and unheld — which the reconciler then recovers on its backoff, the
    // operator's stated intent — never running and held (#10453, #12772).
    session_hold::lift_holds(std::slice::from_ref(&config.profile))?;
    match state {
        None => {
            let image = image.unwrap_or(DEFAULT_SESSION_IMAGE);
            // The account profile's hook registration, Codex trust state and
            // readiness receipt are established HERE, in a throwaway container
            // carrying nothing but the profile — because the session created on
            // the next line binds all three read-only, and a session cannot
            // provision what it is forbidden to write (issue #8839). An
            // already-provisioned profile is left byte-for-byte alone.
            docker::provision_controls(&config, image)?;
            docker::create(&config, image)?;
        }
        Some(state) if state["State"]["Running"] != true => {
            docker::command(&["start", &config.container])?;
        }
        Some(_) => {}
    }
    let state = docker::inspect(&config.container)?.context("created session disappeared")?;
    docker::validate(&config, &state)?;
    mark_session_managed(&config.profile, &config.container)?;
    let id = state["Id"].as_str().context("container id unavailable")?;
    // Adopt before any probe: even an image/protocol failure must not permit
    // a host-direct process to refresh this profile behind its new owner.
    docker::prepare(&config, id, None)?;
    // A session may not come up at all unless its image ships a control
    // boundary this host supports: provisioning the managed hook and then
    // discovering the bundle is missing/stale would leave an apparently ready
    // session whose guard code lives in worker-writable paths (issue #8839).
    docker::setup(&config)?;
    docker::control(&config, id)?;
    snapshot(config, &dir)
}

/// Unattended recovery of an already-configured private session whose
/// container is stopped or gone (#12772). Unlike [`start`] it never provisions:
/// it resolves the persisted identity, resumes a stopped container or recreates
/// a missing one against the surviving volume and the recorded `image`, then
/// re-verifies ownership, mounts and controls. It does not run `prepare`, so no
/// clone, fetch or reset touches unfinished work, and a restart is not an
/// admission: each job still proves its own containment.
///
/// `check_hold` runs under the exclusive account lease, immediately before the
/// first mutation, and fails the recovery when an operator hold has appeared.
/// The hold [`stop`] writes in the account profile is checked there too, so a
/// caller with a narrower view of the account cannot restart a stopped one.
/// Because [`stop`] writes its hold and [`recover`] reads it under the same
/// exclusive lease, the two cannot interleave: a stop either lands before the
/// check (the recovery refuses) or after the whole recovery (it stops the
/// recovered container); there is no running-and-held outcome to undo.
pub fn recover(
    workspace: &Path,
    name: &str,
    image: Option<&str>,
    check_hold: &dyn Fn() -> Result<()>,
) -> Result<Status> {
    let (config, dir) = resolve(workspace, name)?;
    let lease = lease::Lease::acquire(&dir)?;
    let held = || -> Result<()> {
        if session_hold::held_across(std::slice::from_ref(&config.profile)) {
            return Err(OperatorHeld.into());
        }
        check_hold()
    };
    let state = docker::inspect(&config.container)?;
    if let Some(state) = &state {
        docker::validate(&config, state)?;
    }
    lease.recover(&config, state.as_ref())?;
    if !docker::idle(state.as_ref(), &config.container)? {
        bail!("private session has an unleased or unverified worker; not recovering it");
    }
    match &state {
        Some(state) if state["State"]["Running"] == true => {}
        Some(_) => {
            held()?;
            docker::command(&["start", &config.container])?;
        }
        None => {
            // `create` would silently make an empty volume; recovery must not.
            if !docker::volume_exists(&config.volume)? {
                bail!(
                    "private workspace volume {} is missing; refusing to create an empty \
                     replacement (restore it or reconfigure with session start --private-clone)",
                    config.volume
                );
            }
            let image = image.context(
                "recorded session image is unknown; refusing to substitute one (start the \
                 session by hand with --image)",
            )?;
            docker::image_present(image)?;
            held()?;
            docker::provision_controls(&config, image)?;
            docker::create(&config, image)?;
        }
    }
    let state = docker::inspect(&config.container)?.context("recovered session disappeared")?;
    docker::validate(&config, &state)?;
    if state["State"]["Running"] != true {
        bail!("recovered private session is not running");
    }
    mark_session_managed(&config.profile, &config.container)?;
    let id = state["Id"].as_str().context("container id unavailable")?;
    docker::setup(&config)?;
    docker::control(&config, id)?;
    snapshot(config, &dir)
}

fn snapshot(config: Config, dir: &Path) -> Result<Status> {
    let state = docker::inspect(&config.container)?;
    if let Some(state) = &state {
        docker::validate(&config, state)?;
    }
    Ok(Status {
        workspace_mode: MODE,
        private_root: REPO,
        container_id: state
            .as_ref()
            .and_then(|s| s["Id"].as_str().map(str::to_owned)),
        running: state
            .as_ref()
            .is_some_and(|s| s["State"]["Running"] == true),
        lease: lease::read(dir)?,
        admission: lease::admission(dir)?,
        config,
    })
}

pub fn status(workspace: &Path, name: &str) -> Result<Status> {
    let (config, dir) = resolve(workspace, name)?;
    snapshot(config, &dir)
}

pub fn stop(workspace: &Path, name: &str, force: bool) -> Result<Status> {
    let (config, dir) = resolve(workspace, name)?;
    let lease = lease::Lease::acquire(&dir)?;
    let state = docker::inspect(&config.container)?;
    if state.is_none() {
        lease.recover(&config, None)?;
    }
    if let Some(state) = &state {
        docker::validate(&config, state)?;
    }
    if !force && !docker::idle(state.as_ref(), &config.container)? {
        bail!("private session has a surviving worker; wait for cleanup or explicitly use --force");
    }
    // A stop is deliberate, so it stays down: the operator hold is written
    // under the exclusive lease, before `docker stop`, and the reconciler's
    // unattended `recover` refuses a held account until an operator `start`
    // lifts it (#10453, #12772). Written even when no container exists, so a
    // session already gone is not recreated behind the operator either.
    session_hold::write_hold(&config.profile, session_hold::now_unix_ms())?;
    if state.is_some() {
        docker::command(&["stop", "--time", "15", &config.container])?;
        let stopped =
            docker::inspect(&config.container)?.context("container disappeared during stop")?;
        if stopped["State"]["Running"] != false {
            bail!("container stop not confirmed");
        }
        docker::command(&["rm", &config.container])?;
    }
    // Deliberately retain the volume, account profile and clone identity.
    lease.finish()?;
    snapshot(config, &dir)
}

/// CLI-process entry point: session-exec installs cancellation handlers.
/// Daemon integration must launch this command as a child, not call it in-process.
pub fn run_job(workspace: &Path, args: JobArgs) -> Result<i32> {
    if args.owner.is_empty() || args.owner.chars().any(char::is_control) {
        bail!("job owner must be a nonempty, printable identity");
    }
    let (config, dir) = resolve(workspace, &args.name)?;
    let target = config.target(args.repository.as_deref())?;
    if target.slug.is_some() {
        // Before the lease and any clone: an excluded forge never reaches git.
        check_firewall(workspace, &identify(&target.repository)?)?;
    }
    let lease = lease::Lease::acquire(&dir)?;
    let state = docker::inspect(&config.container)?
        .context("private session is stopped; start it first")?;
    docker::validate(&config, &state)?;
    lease.recover(&config, Some(&state))?;
    if state["State"]["Running"] != true || !docker::idle(Some(&state), &config.container)? {
        bail!("private session is not running and idle; refuse job admission");
    }
    let id = state["Id"].as_str().context("container id unavailable")?;
    let mut job = lease::Job {
        owner: args.owner,
        kind: args.kind,
        issue: args.issue,
        branch: args.branch.clone(),
        container_id: id.to_owned(),
        base_revision: String::new(),
        host_pid: std::process::id(),
        control: String::new(),
        repository: if target.slug.is_some() {
            target.repository.clone()
        } else {
            String::new()
        },
    };
    // Publish before clone/fetch/branch operations; a host crash cannot turn a
    // still-running preparation exec into an apparently free account.
    lease.begin(&job)?;
    job.base_revision = docker::prepare_target(&config, &target, id, args.branch.as_deref())?;
    job.control = docker::control(&config, id)?;
    lease.begin(&job)?;
    let mut env: Vec<String> = FORGE_ENV
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .map(|name| (*name).to_owned())
        .collect();
    // Supervised jobs are mutable work too: carry the same forced guard policy
    // and the bound identity the adapter path carries.
    env.extend(bundle::policy_env());
    env.push(format!("LOOM_PRIVATE_CONTROL={}", job.control));
    env.push(format!("LOOM_PRIVATE_REPOSITORY={}", target.repository));
    docker::recheck_control(&config, id, &job.control)?;
    let code = crate::session_exec::run_host(crate::session_exec::HostArgs {
        owner_pid: None,
        container: config.container.clone(),
        workdir: target.path.clone(),
        env,
        stderr_file: None,
        command: args.command,
    })?;
    let after =
        docker::inspect(&config.container)?.context("session disappeared before lease release")?;
    if after["Id"] != id || !docker::idle(Some(&after), &config.container)? {
        bail!("worker returned but account idle state is unproven; retain lease for recovery");
    }
    lease.finish()?;
    Ok(code)
}
