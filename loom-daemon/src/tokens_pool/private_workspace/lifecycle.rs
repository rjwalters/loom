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

/// The profile's private-workspace registration, read tolerantly for
/// `accounts list` (issue #14298): a local `workspace.json` read only — no
/// docker, no lease, no engine or ownership validation — because a list of
/// every account must not fail on state one broken session left behind.
/// `None` when the profile has no private workspace, and also when the
/// state exists but cannot be read; surfacing that failure is the dedicated
/// `session status` / `session recover` path's job, not a list's.
pub fn registrations(profile: &Path) -> Option<Config> {
    let dir = state_dir(profile).ok()?;
    if !dir.join("workspace.json").is_file() {
        return None;
    }
    load(&dir).ok()
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

/// Validate a declared binding without touching Docker or any state (#14111):
/// a credential-free HTTPS forge URL and a safe base. Returns the repository's
/// canonical key, the identity two spellings of one repository share.
pub fn check_binding(repository: &str, base: &str) -> Result<String> {
    let url = repository_url(repository)?;
    valid_base(base)?;
    Ok(identify(&url)?.key)
}

/// The repositories an account's private workspace holds — the primary first,
/// then every registered one — as `(url, canonical key)`. `None`: the account
/// is not in private-clone mode (no `workspace.json`).
pub fn registered_repositories(
    workspace: &Path,
    name: &str,
) -> Result<Option<Vec<(String, String)>>> {
    let account = account(workspace, name)?;
    let dir = state_dir(&account.credential_reference)?;
    if !dir.join("workspace.json").exists() {
        return Ok(None);
    }
    let config = load(&dir)?;
    let key = |url: &str| identify(url).map_or_else(|_| url.to_owned(), |id| id.key);
    let mut out = vec![(config.repository.clone(), key(&config.repository))];
    out.extend(
        config
            .repositories
            .iter()
            .map(|r| (r.repository.clone(), key(&r.repository))),
    );
    Ok(Some(out))
}

/// What [`converge`] found or did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Converged {
    /// The repository was already registered with this base; nothing was touched.
    Present,
    /// The repository is now registered (the first one also prepared the workspace).
    Registered,
}

/// Make a declared binding true without an operator (#14111): register
/// `repository` on the account, by the same path as repeating
/// `session start --private-clone` while it is idle — so a busy account fails
/// with [`AccountBusy`] (retry later, never interrupted) and nothing existing
/// is ever deleted or reset.
///
/// Unlike an operator start it never lifts an operator hold (`check_hold` is
/// consulted under the account lease, before the first mutation), never
/// substitutes an image for a container that went missing (that is
/// [`recover`]'s job), and refuses an account that runs a host-mounted session
/// rather than convert it. A firewalled repository, an unknown account and a
/// base that conflicts with the registered one are refused before any Docker call.
pub fn converge(
    workspace: &Path,
    name: &str,
    repository: &str,
    base: &str,
    check_hold: &dyn Fn() -> Result<()>,
) -> Result<Converged> {
    let repository = repository_url(repository)?;
    valid_base(base)?;
    let id = identify(&repository)?;
    let account = account(workspace, name)?;
    check_firewall(workspace, &id)?;
    let profile = account.credential_reference.canonicalize()?;
    let dir = state_dir(&profile)?;
    if dir.join("workspace.json").exists() {
        if let Ok(known) = load(&dir)?.target(Some(&repository)) {
            if known.base != base {
                bail!("private repository {} is already configured with base {}; retain its clone and resolve unfinished work before changing the base", known.repository, known.base);
            }
            return Ok(Converged::Present);
        }
    } else if super::super::session_lifecycle::is_session_managed(&profile) {
        bail!("account runs a host-mounted session; a binding does not convert it (stop it, recover its work, then run session start --private-clone once)");
    }
    start_with(workspace, name, &repository, base, None, Some(check_hold))?;
    Ok(Converged::Registered)
}

pub fn start(
    workspace: &Path,
    name: &str,
    repository: &str,
    base: &str,
    image: Option<&str>,
) -> Result<Status> {
    start_with(workspace, name, repository, base, image, None)
}

/// `start`, optionally unattended: `unattended` is the caller's hold probe. An
/// operator start (`None`) lifts the hold an operator `stop` left; an
/// unattended one honours it, checked under the lease before the first
/// mutation, and recreates nothing the recorded identity says is gone.
fn start_with(
    workspace: &Path,
    name: &str,
    repository: &str,
    base: &str,
    image: Option<&str>,
    unattended: Option<&dyn Fn() -> Result<()>>,
) -> Result<Status> {
    let repository = repository_url(repository)?;
    valid_base(base)?;
    let account = account(workspace, name)?;
    let profile = account.credential_reference.canonicalize()?;
    let dir = state_dir(&profile)?;
    let lease = lease::Lease::acquire(&dir)?;
    let engine = docker::engine()?;
    let existed = dir.join("workspace.json").exists();
    let mut config = if existed {
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
            session_image: None,
            extra_mounts: Vec::new(),
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
        return Err(lease::AccountBusy(
            "session has an unleased worker; stop it before preparing the workspace",
        )
        .into());
    }
    if let Some(check_hold) = unattended {
        if session_hold::held_across(std::slice::from_ref(&config.profile)) {
            return Err(OperatorHeld.into());
        }
        check_hold()?;
        if existed && state.is_none() {
            bail!("private session container is missing; refusing to recreate it with a default image (the reconcile recovery restores it with the recorded one)");
        }
    }
    // The image and read-only mounts a NEW container gets are decided here,
    // once, and recorded in `workspace.json` (#14451): `validate_settings`,
    // `recover` and `status` all read the record, never the live config, so a
    // config edit cannot reshape a seat behind an unattended restart. A
    // container that already exists keeps what it was created with; the
    // configured values apply from the next stop + start. The host tier decides
    // for a seat serving several repositories — `workspace` is the same root
    // the reconciler reads the keys from.
    let mut start_image = image.map(str::to_owned);
    if state.is_none() {
        let configured = super::super::session_image::read_session_image_config(workspace)?;
        let configured_image = configured.image.clone();
        if start_image.is_none() {
            start_image.clone_from(&configured_image);
        }
        config.session_image = if start_image == configured_image {
            configured_image
        } else {
            None
        };
        config.extra_mounts = configured.extra_mounts;
        check_extra_mounts(workspace, &config)?;
        if let Some(configured) = &config.session_image {
            docker::verify_image(configured)?;
        }
    } else if image.is_none() {
        warn_if_config_drifted(workspace, &config);
    }
    save(&dir.join("workspace.json"), &config)?;
    if unattended.is_none() {
        // An operator start lifts the hold an operator `stop` left, before the
        // first mutation, so a failure past this point leaves the session down
        // and unheld — which the reconciler then recovers on its backoff, the
        // operator's stated intent — never running and held (#10453, #12772).
        session_hold::lift_holds(std::slice::from_ref(&config.profile))?;
    }
    match state {
        None => {
            let image = start_image.as_deref().unwrap_or(DEFAULT_SESSION_IMAGE);
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
    docker::prepare(&config, workspace, id, None)?;
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
            // The image an operator `start` recorded from the configuration wins
            // over the last one the reconciler saw, and is re-verified: a
            // daemon roll that changed the sealed bundle refuses a
            // not-yet-rebuilt derived image rather than swapping in another
            // (#14451), exactly as host mode does.
            let image = match config.session_image.as_deref() {
                Some(recorded) => recorded,
                None => image.context(
                    "recorded session image is unknown; refusing to substitute one (start the \
                     session by hand with --image)",
                )?,
            };
            docker::image_present(image)?;
            if config.session_image.is_some() {
                docker::verify_image(image)?;
            }
            check_extra_mounts(workspace, &config)?;
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

/// Refuse the recorded extra read-only mounts a private session must never
/// have (#14451): the host-mode denylist for the host source, and a container
/// target that shadows or nests inside `/workspace`, the `CODEX_HOME` profile,
/// the sealed bundle root or the forge-configuration mount. Run before a
/// container is created, so a refused mount never reaches Docker.
fn check_extra_mounts(workspace: &Path, config: &Config) -> Result<()> {
    if config.extra_mounts.is_empty() {
        return Ok(());
    }
    let reserved = [
        PathBuf::from(ROOT),
        PathBuf::from(PROFILE),
        PathBuf::from(GH_CONFIG),
    ];
    super::super::session_mount_gate::check_private_extra_mounts(
        &config.extra_mounts,
        workspace,
        &reserved,
    )
}

/// An operator `start` over a container that already exists keeps the
/// image and mounts it was created with; say so when the configuration now
/// asks for something else, so the edit is not mistaken for applied.
fn warn_if_config_drifted(workspace: &Path, config: &Config) {
    let Ok(configured) = super::super::session_image::read_session_image_config(workspace) else {
        return;
    };
    if configured.extra_mounts != config.extra_mounts
        || (configured.image.is_some() && configured.image != config.session_image)
    {
        log::warn!(
            "private session {}: accounts.session image/extraMounts differ from the running \
             container's; they apply after `accounts session stop` and a new `start`",
            config.account
        );
    }
}

/// Publish, discard or just list the unpublished branches of an account's
/// private clones (`accounts session recover`, #14030). Same guard policy as
/// a job: resolved workspace identity, a validated running session, and the
/// exclusive account lease held for the whole call — so it can never race a
/// running step, and nothing touches the volume or a branch other than the
/// ones named by an explicit discard scope.
pub fn session_recover(
    workspace: &Path,
    name: &str,
    action: RecoverAction,
    issue: Option<u64>,
    branches: &[String],
) -> Result<RecoverReport> {
    let (config, dir) = resolve(workspace, name)?;
    let targets = {
        let mut targets = vec![config.primary()];
        for registered in &config.repositories {
            targets.push(config.target(Some(&registered.repository))?);
        }
        targets
    };
    // Parity with a job on an additional clone: an excluded forge never
    // reaches git, not even for a publish.
    for target in targets.iter().filter(|t| t.slug.is_some()) {
        check_firewall(workspace, &identify(&target.repository)?)?;
    }
    let _lease = lease::Lease::acquire(&dir)?;
    let state = docker::inspect(&config.container)?
        .context("private session is stopped; start it first")?;
    docker::validate(&config, &state)?;
    if state["State"]["Running"] != true || !docker::idle(Some(&state), &config.container)? {
        bail!("private session is not running and idle; refuse recover admission");
    }
    // Resolved per exec (an installation token outlives no job), but checked
    // for every clone up front: a publish pushes, and with no write credential
    // for a GitHub clone the in-container helper can only fail, so refuse
    // before ANY exec and name what to provide (#14433). List and discard
    // never push.
    let auths: Vec<_> = targets
        .iter()
        .map(|target| forge_auth::ExecAuth::resolve(workspace, &target.repository))
        .collect();
    if action == RecoverAction::Publish {
        if let Some((target, auth)) = targets
            .iter()
            .zip(&auths)
            .find(|(_, auth)| auth.github_token_missing())
        {
            // An owner whose only published credential is provably expired
            // reaches the same bail (`config_dir_for` skips a matched-but-
            // expired directory), but the message must not tell the operator
            // to publish an App config that is already published (#14435).
            if auth.expired() {
                bail!(
                    "private workspace forge credential for {} is expired and no fresher \
                     one is published; the credential refresh has not renewed it; the \
                     unpublished work is retained",
                    target.repository
                );
            }
            bail!(
                "no GitHub write credential for {}: no published App config for the \
                 repository owner was found (.loom/gh-config-by-owner/<owner>/hosts.yml \
                 under the running daemon's workspace root, which this command reads from \
                 the daemon itself; if no daemon answered, start it or export \
                 LOOM_MACHINE_CHECKOUT=<that root> as an override) and no GH_TOKEN is in \
                 the environment; the unpublished work is retained",
                target.repository
            );
        }
    }
    let mut clones = Vec::new();
    for (target, auth) in targets.iter().zip(&auths) {
        clones.push(docker::recover_target(&config, target, action, issue, branches, auth)?);
    }
    let report = RecoverReport {
        account: config.account.clone(),
        clones,
    };
    if action != RecoverAction::List
        && (issue.is_some() || !branches.is_empty())
        && report
            .clones
            .iter()
            .all(|clone| clone.published.is_empty() && clone.discarded.is_empty())
    {
        bail!(
            "no clone of this account has unpublished commits on the named branches; \
             run the listing form (`accounts session recover {name}`) to see what is unpublished"
        );
    }
    Ok(report)
}

/// Rebind an additional private-clone registration to the repository's
/// canonical URL after a forge transfer (`accounts session rebind`, #14357),
/// keeping its clone, local branches, dirty files and unpublished commits.
///
/// Guards are the job policy's plus the hold check: resolved workspace
/// identity, a firewall check on the NEW url, the exclusive account lease, a
/// running idle session, and no operator hold, all before the first mutation
/// (a busy account is [`AccountBusy`], a held one [`OperatorHeld`]; neither is
/// ever interrupted or lifted). The in-volume step verifies the canonical
/// repository shares history with the clone before anything moves, and
/// reports (changing nothing) when both locations already hold a clone.
/// `workspace.json` is rewritten only after the volume is done, so a crash in
/// between is finished by repeating the call. Re-running a completed rebind
/// verifies and reports it. The primary repository is never rebound here.
pub fn rebind(
    workspace: &Path,
    name: &str,
    from: &str,
    to: &str,
    dry_run: bool,
) -> Result<RebindReport> {
    let (from_id, to_id) = (identify(&repository_url(from)?)?, identify(&repository_url(to)?)?);
    if from_id.key == to_id.key {
        bail!("old and canonical URLs name the same repository; a spelling change needs no rebind");
    }
    let (mut config, dir) = resolve(workspace, name)?;
    let is_primary = |key: &str| identify(&config.repository).is_ok_and(|id| id.key == key);
    if is_primary(&from_id.key) || is_primary(&to_id.key) {
        bail!(
            "the primary repository cannot be rebound; only additional registrations ({})",
            registered_urls(&config)
        );
    }
    let find = |key: &str| {
        config
            .repositories
            .iter()
            .position(|r| identify(&r.repository).is_ok_and(|id| id.key == key))
    };
    let (old_at, new_at) = (find(&from_id.key), find(&to_id.key));
    let from_reg = match (old_at, new_at) {
        (Some(at), _) => config.repositories[at].clone(),
        // Old registration already retired: verify the canonical one.
        (None, Some(at)) => Registered {
            repository: from_id.url.clone(),
            base: config.repositories[at].base.clone(),
            slug: from_id.slug.clone(),
        },
        (None, None) => bail!(
            "neither {} nor {} is registered on this account (registered: {})",
            from_id.url,
            to_id.url,
            registered_urls(&config)
        ),
    };
    if let Some(at) = new_at {
        if config.repositories[at].base != from_reg.base {
            bail!("the old and canonical registrations use different bases; resolve that before rebinding");
        }
    }
    check_firewall(workspace, &to_id)?;
    let _lease = lease::Lease::acquire(&dir)?;
    let state = docker::inspect(&config.container)?
        .context("private session is stopped; start it first")?;
    docker::validate(&config, &state)?;
    if state["State"]["Running"] != true || !docker::idle(Some(&state), &config.container)? {
        return Err(
            lease::AccountBusy("private session is not running and idle; try again later").into()
        );
    }
    if session_hold::held_across(std::slice::from_ref(&config.profile)) {
        return Err(OperatorHeld.into());
    }
    let auth = forge_auth::ExecAuth::resolve(workspace, &to_id.url);
    let outcome = docker::rebind_target(&config, &from_reg, &to_id, dry_run, &auth)?;
    let settled = matches!(
        outcome.status,
        RebindStatus::Moved | RebindStatus::AlreadyBound | RebindStatus::NotCloned
    );
    let canonical = Registered {
        repository: to_id.url.clone(),
        base: from_reg.base.clone(),
        slug: to_id.slug.clone(),
    };
    let wanted: Vec<Registered> = {
        let mut list = Vec::new();
        for registered in &config.repositories {
            let key = identify(&registered.repository).map(|id| id.key).ok();
            if key.as_deref() == Some(from_id.key.as_str())
                || key.as_deref() == Some(to_id.key.as_str())
            {
                if !list.contains(&canonical) {
                    list.push(canonical.clone());
                }
            } else {
                list.push(registered.clone());
            }
        }
        list
    };
    let registration_changed = wanted != config.repositories;
    if settled && registration_changed && !dry_run {
        config.repositories = wanted;
        save(&dir.join("workspace.json"), &config)?;
    }
    Ok(RebindReport {
        account: config.account.clone(),
        from: from_id.url,
        to: to_id.url,
        base: from_reg.base,
        status: outcome.status,
        registration_changed: registration_changed && (settled || dry_run),
        clone_path: format!("{WORK_ROOT}/{}", to_id.slug),
        before: if outcome.from.populated {
            outcome.from
        } else {
            outcome.to.clone()
        },
        after: outcome.to,
    })
}

fn registered_urls(config: &Config) -> String {
    std::iter::once(config.repository.as_str())
        .chain(config.repositories.iter().map(|r| r.repository.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
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
        image: state
            .as_ref()
            .and_then(|s| s["Config"]["Image"].as_str().map(str::to_owned)),
        extra_mounts: config.extra_mounts.clone(),
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
    job.base_revision =
        docker::prepare_target(&config, workspace, &target, id, args.branch.as_deref())?;
    job.control = docker::control(&config, id)?;
    lease.begin(&job)?;
    // A fresh credential for the job's own exec (#13916), never the one the
    // prepare exec carried: a long preparation must not hand the job a token
    // that is already close to expiry.
    let auth = forge_auth::ExecAuth::resolve(workspace, &target.repository);
    let mut env: Vec<String> = auth.names().iter().map(|name| (*name).to_owned()).collect();
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
        child_env: auth.child_env(),
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
