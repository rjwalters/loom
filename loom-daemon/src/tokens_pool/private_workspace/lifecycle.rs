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
        let existing = load(&dir)?;
        if existing.repository != repository
            || existing.base != base
            || existing.account != account.id.name
            || existing.profile != profile
            || existing.engine != engine
        {
            bail!("private workspace identity differs (repository/base/account/profile/engine); retain its volume and resolve unfinished work before configuring a new session");
        }
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
    match state {
        None => docker::create(&config, image.unwrap_or(DEFAULT_SESSION_IMAGE))?,
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
    };
    // Publish before clone/fetch/branch operations; a host crash cannot turn a
    // still-running preparation exec into an apparently free account.
    lease.begin(&job)?;
    job.base_revision = docker::prepare(&config, id, args.branch.as_deref())?;
    lease.begin(&job)?;
    let env = FORGE_ENV
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .map(|name| (*name).to_owned())
        .collect();
    let code = crate::session_exec::run_host(crate::session_exec::HostArgs {
        owner_pid: None,
        container: config.container.clone(),
        workdir: REPO.to_owned(),
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
