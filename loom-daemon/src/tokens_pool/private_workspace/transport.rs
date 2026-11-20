//! Host session adapter boundary. No worker-controlled filesystem export paths.
use super::*;

pub fn run(mut args: crate::session_exec::HostArgs) -> Result<i32> {
    let root = std::env::var_os("LOOM_WORKSPACE")
        .map(PathBuf::from)
        .or_else(|| {
            args.env
                .iter()
                .find_map(|e| e.strip_prefix("LOOM_WORKSPACE=").map(PathBuf::from))
        });
    let name = std::env::var("LOOM_ACCOUNT_NAME").ok().or_else(|| {
        std::env::var_os("CODEX_HOME").and_then(|p| {
            PathBuf::from(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
    });
    // An inherited private lease means the daemon prepared and admitted this
    // launch AS a private clone — possibly on containment evidence (#8787),
    // with the adapter's own mutable-role hook preflight relocated into the
    // session. It must therefore never degrade into an ordinary host or
    // shared-mount exec, where none of that enforcement exists.
    let leased = std::env::var_os("LOOM_PRIVATE_LEASE_FD").is_some();
    // Where an unleased exec looks for the session its container belongs to:
    // the named workspace, then the daemon's machine checkout (#13937).
    let mut roots: Vec<PathBuf> = root.iter().cloned().collect();
    if let Some(machine) = std::env::var_os("LOOM_MACHINE_CHECKOUT").filter(|v| !v.is_empty()) {
        let machine = PathBuf::from(machine);
        if !roots.contains(&machine) {
            roots.push(machine);
        }
    }
    let host = |mut args: crate::session_exec::HostArgs| {
        if leased {
            bail!("a private account lease was inherited but this session is not a configured private clone; refusing a non-private launch");
        }
        if let Some((root, config)) = session_for_container(&roots, &args.container) {
            authorize_unleased(&mut args, &root, &config, &forge_auth::ExecAuth::resolve);
        }
        crate::session_exec::run_host(args)
    };
    let (Some(root), Some(name)) = (root, name) else {
        return host(args);
    };
    let private_profile = std::env::var_os("CODEX_HOME")
        .or_else(|| std::env::var_os("LOOM_CODEX_HOME"))
        .map(PathBuf::from);
    if private_profile.as_ref().is_some_and(|profile| {
        state_dir(profile).is_ok_and(|dir| !dir.join("workspace.json").exists())
    }) {
        return host(args);
    }
    if !configured(&root, &name)? {
        return host(args);
    }
    let (config, dir) = lifecycle::resolve(&root, &name)?;
    let selection = if !leased {
        // The manual, unleased path launches on the account it resolved and
        // no other: a busy named seat refuses with its own reason, never a
        // different seat a pool walk would hand back (#14627).
        Some(
            dispatch::Selection::prepare_named(
                &root,
                &config,
                JobKind::Role,
                None,
                &format!("manual-{}", uuid::Uuid::new_v4()),
            )?
            .context("private profile was not selected")?,
        )
    } else {
        None
    };
    // Direct adapter invocations retain the preparing object in this process;
    // daemon launches adopt the exact open file description used at preflight.
    let inherited = dispatch::inherited_lease(&dir)?;
    let lease = inherited
        .as_ref()
        .or_else(|| selection.as_ref().and_then(|s| s.lease()))
        .context("private dispatch has no owned account lease")?;
    if lease.dir != dir
        || selection
            .as_ref()
            .is_some_and(|selection| selection.name != config.account)
    {
        bail!("{}", dispatch::WRONG_ACCOUNT);
    }
    let job = lease::read(&lease.dir)?.context("private dispatch lost its durable job identity")?;
    // The clone this job was prepared in, from the durable job the lease owns:
    // an empty repository is the legacy primary, anything else must still be a
    // registered clone of this account or the launch is refused.
    let target = config
        .target((!job.repository.is_empty()).then_some(job.repository.as_str()))
        .context(containment::WRONG_CLONE)?;
    if let Some(selection) = &selection {
        if selection.target() != Some(&target) {
            bail!("{}", containment::WRONG_CLONE);
        }
    }
    let state =
        docker::inspect(&config.container)?.context("private session disappeared before launch")?;
    docker::validate(&config, &state)?;
    if state["Id"] != job.container_id || args.container != config.container {
        bail!("private session identity changed before spawn");
    }
    // Immediately before launch, on the exact container/lease being launched:
    // guard code, forced policy, hook registration and readiness receipt must
    // still hash to the identity bound at admission (issue #8839).
    docker::recheck_control(&config, &job.container_id, &job.control)?;
    args.container = job.container_id.clone();
    args.workdir = target.path.clone();
    // The job exec's forge credential, resolved for THIS exec (#13916).
    let auth = forge_auth::ExecAuth::resolve(&root, &target.repository);
    args.env = child_env(&args.env, &config, &job, &target, &auth)?;
    args.child_env = auth.child_env();
    // Resolve hooks on the worker, using its private installed bridge. Never
    // bless hook trust or enable a capability that the manifest rejects.
    args.command.splice(
        0..0,
        [
            "loom-daemon".into(),
            "private-workspace".into(),
            "execute".into(),
            "--".into(),
        ],
    );
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = finished.clone();
    let monitor_root = root.clone();
    let monitor_config = config.clone();
    let monitor_job = job.clone();
    let monitor = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            let _ = export::update(&monitor_root, &monitor_config, &monitor_job, "running");
            for _ in 0..10 {
                if stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    });
    if let Some(selection) = &selection {
        selection.spawned();
    }
    let result = crate::session_exec::run_host(args);
    finished.store(true, std::sync::atomic::Ordering::Release);
    let _ = monitor.join();
    let outcome = match &result {
        Ok(0) => "completed",
        Ok(143) => "cancelled",
        Ok(_) => "failed",
        Err(_) => "recovery-required",
    };
    export::update(&root, &config, &job, outcome)?;
    let after = docker::inspect(&config.container)?;
    // Missing transport acknowledgements retain the durable lease even when
    // the host process exits. A later admission must prove the old worker idle.
    if result.is_ok()
        && after.as_ref().is_some_and(|s| s["Id"] == job.container_id)
        && docker::idle(after.as_ref(), &config.container)?
    {
        lease.finish()?;
    }
    result
}

/// The private-clone session whose container `container` is, found from the
/// persisted session identity of each Codex account under `roots`. `None` for
/// any other container (a shared-mount session, a non-Loom container), which
/// keeps its plain exec.
fn session_for_container(roots: &[PathBuf], container: &str) -> Option<(PathBuf, Config)> {
    use super::super::account_registry::{account_inventory_quiet, AccountProvider};
    roots.iter().find_map(|root| {
        account_inventory_quiet(root, AccountProvider::Codex)
            .ok()?
            .into_iter()
            .find_map(|account| {
                let config = load(&state_dir(&account.credential_reference).ok()?).ok()?;
                (config.container == container && config.account == account.id.name)
                    .then(|| (root.clone(), config))
            })
    })
}

/// The registered clone an exec at `workdir` runs in: the primary at
/// [`REPO`] or an additional clone at `WORK_ROOT/<slug>`, compared by path
/// component (a worktree under the clone counts). `None` outside every clone,
/// so an exec elsewhere in the container is credentialed for nothing.
fn target_for_workdir(config: &Config, workdir: &str) -> Option<Target> {
    let workdir = Path::new(workdir);
    if workdir.starts_with(REPO) {
        return Some(config.primary());
    }
    config
        .repositories
        .iter()
        .find(|r| workdir.starts_with(format!("{WORK_ROOT}/{}", r.slug)))
        .and_then(|r| config.target(Some(&r.repository)).ok())
}

/// An exec into a private-clone session that is NOT a leased private launch
/// (a hand-run `loom-daemon session-exec host`, or a launcher that names no
/// account) carries the same owner-scoped forge credential a leased job gets
/// (#13937): resolved for the clone `args.workdir` is in, set on the `docker`
/// child's environment and named in argv only as a bare `--env GH_TOKEN`.
/// Before this, such an exec built `HostArgs` with an empty `child_env`, so
/// an App-only worker's in-container `gh`/`git` had no credential at all.
///
/// The owner rule is [`forge_auth::ExecAuth`]'s: no directory proven to hold
/// THIS repository owner's token means nothing is forwarded. A caller that
/// already passed a token value itself is left alone.
fn authorize_unleased(
    args: &mut crate::session_exec::HostArgs,
    root: &Path,
    config: &Config,
    resolve: &dyn Fn(&Path, &str) -> forge_auth::ExecAuth,
) {
    let Some(target) = target_for_workdir(config, &args.workdir) else {
        return;
    };
    let caller_set = args.env.iter().any(|item| {
        item.split_once('=')
            .is_some_and(|(key, _)| key == "GH_TOKEN" || key == "GITHUB_TOKEN")
    });
    if caller_set {
        return;
    }
    let auth = resolve(root, &target.repository);
    let child_env = auth.child_env();
    if child_env.is_empty() {
        return;
    }
    for (name, _) in &child_env {
        if !args.env.iter().any(|item| item == name) {
            args.env.push(name.clone());
        }
    }
    args.child_env = child_env;
}

/// The caller context a private-clone exec carries into the container. Data
/// identities only, never a host path.
///
/// The three lease/claim names travel together: the daemon pre-claims a
/// Builder step's issue and publishes its lease under the daemon's own lease
/// host id before it spawns the session, and the container has a hostname of
/// its own. Without `LOOM_LEASE_HOST_ID` the session resolves a different
/// lease host and reads its dispatcher's lease as a peer's — `lease ensure`
/// is declined (publish exit 4) and `sweep-lease-fence.sh` aborts SUPERSEDED,
/// so the step ends `refused claim_lost` on its own claim. Without
/// `LOOM_SWEEP_LEASE_RENEW_DISPATCHED` it cannot know the dispatch already
/// published and renews that lease, and tries to publish a second one.
/// `LOOM_LEASE_PUBLISH_HOSTNAME` must agree with `LOOM_LEASE_HOST_ID`
/// (#13998): a host that opts into raw publishing has its daemon publish
/// `host=<raw>`, and a session without the opt-in derives the opaque id
/// instead — it would read its own lease as a peer's (fails closed).
///
/// The four `GIT_*` names are the commit identity. The session image has no
/// global git config and the private clone no local one, so without them
/// `git commit` there fails with "Author identity unknown". `spawn-codex.sh`
/// resolves them from the host's own git config, the identity a Claude worker
/// on this host commits under.
const FORWARDED_CONTEXT: &[&str] = &[
    "LOOM_ROLE",
    "LOOM_WORK_ORIGIN",
    "LOOM_TERMINAL_ID",
    "LOOM_SWEEP_ID",
    "LOOM_SWEEP_CLAIM_OWNED",
    "LOOM_SWEEP_LEASE_RENEW_DISPATCHED",
    "LOOM_LEASE_HOST_ID",
    "LOOM_LEASE_PUBLISH_HOSTNAME",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
];

fn child_env(
    input: &[String],
    config: &Config,
    job: &lease::Job,
    target: &Target,
    auth: &forge_auth::ExecAuth,
) -> Result<Vec<String>> {
    let clone = &target.path;
    let mut env = vec![
        format!("LOOM_WORKSPACE={clone}"),
        format!("LOOM_PROJECT_ROOT={clone}"),
        format!("LOOM_WORKTREE_ROOT={clone}/.loom/worktrees"),
        // The selected clone, set on this exec itself: credentials and the
        // worker's containment proof are scoped to exactly this repository.
        format!("LOOM_PRIVATE_REPOSITORY={}", target.repository),
        format!("LOOM_PRIVATE_CLONE={clone}"),
        "LOOM_RUNTIME=codex".into(),
        "LOOM_PRIVATE_WORKSPACE=1".into(),
        "CARGO_INCREMENTAL=0".into(),
        format!("LOOM_ACCOUNT_NAME={}", config.account),
        "LOOM_ACCOUNT_PROVIDER=codex".into(),
    ];
    // Never forward host model/cache/control/executor locations. These are
    // data identities, not paths; fixed container values own every path.
    for item in input {
        if let Some((key, value)) = item.split_once('=') {
            if FORWARDED_CONTEXT.contains(&key) {
                if value.len() > 256 || value.chars().any(char::is_control) {
                    bail!("invalid private child context");
                }
                env.push(item.clone());
            }
        }
    }
    if let Some(issue) = job.issue {
        env.push(format!("LOOM_PRIVATE_ISSUE={issue}"));
    }
    // Forced guard policy and the bound control identity. Both are set by this
    // host on the container process itself, so no worker-writable configuration
    // tier can weaken a guard category and no in-container process can forge
    // the identity its own boundary is rechecked against (issue #8839).
    env.extend(bundle::policy_env());
    env.push(format!("LOOM_PRIVATE_CONTROL={}", job.control));
    // The base revision the host's own `prepare` resolved for this job. The
    // worker's containment proof (#8787) cross-checks it against the clone's
    // identity record, so a stale or replaced workspace cannot answer for the
    // bound job.
    env.push(format!("LOOM_PRIVATE_BASE_REVISION={}", job.base_revision));
    // Bare names only: `docker` copies each value from its own environment,
    // where a host-resolved token was set on that child alone (#13916).
    env.extend(auth.names().iter().map(|name| (*name).to_owned()));
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_auth() -> forge_auth::ExecAuth {
        forge_auth::ExecAuth::resolve_with(
            Path::new("/nonexistent"),
            "https://example.invalid/o/r",
            &|_| None,
            &|_| None,
        )
    }
    #[test]
    fn child_paths_and_control_environment_cannot_escape_private_clone() {
        let config = Config {
            schema_version: 1,
            account: "test".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/repo".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: "/host/profile".into(),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        };
        let job = lease::Job {
            owner: "sweep-7".into(),
            kind: JobKind::Sweep,
            issue: Some(7),
            branch: None,
            container_id: "a".repeat(64),
            base_revision: String::new(),
            host_pid: 1,
            control: "b".repeat(64),
            repository: String::new(),
        };
        let env = child_env(
            &[
                "LOOM_WORKSPACE=/host/repo".into(),
                "LOOM_WORKTREE_PATH=/host/worktree".into(),
                "LOOM_DAEMON_SOCKET=/host/socket".into(),
                "LOOM_RUN_JOB_HOST=host".into(),
                "LOOM_ROLE=guide".into(),
                "LOOM_WORK_ORIGIN=autonomous".into(),
            ],
            &config,
            &job,
            &config.primary(),
            &no_auth(),
        )
        .unwrap();
        assert!(env.contains(&"LOOM_WORKSPACE=/workspace/repo".into()));
        assert!(env.contains(&"LOOM_PRIVATE_CLONE=/workspace/repo".into()));
        assert!(env.contains(&"LOOM_ROLE=guide".into()));
        assert!(env.contains(&"LOOM_WORK_ORIGIN=autonomous".into()));
        // The bound control identity and base revision the worker re-checks
        // come from the host lease, never from caller-supplied environment.
        assert!(env.contains(&format!("LOOM_PRIVATE_CONTROL={}", "b".repeat(64))));
        assert!(env.contains(&"LOOM_PRIVATE_BASE_REVISION=".to_string()));
        assert!(!env
            .iter()
            .any(|v| v.contains("/host/") || v.starts_with("LOOM_RUN_JOB")));
    }

    /// A daemon-dispatched Builder step's lease and claim context, and the
    /// host's commit identity, cross into the private clone; a host path or an
    /// unlisted name beside them still does not.
    #[test]
    fn child_env_carries_the_dispatchers_lease_context_and_commit_identity() {
        let config = Config {
            schema_version: 1,
            account: "test".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/repo".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: "/host/profile".into(),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        };
        let job = lease::Job {
            owner: "step-builder-20-1".into(),
            kind: JobKind::Sweep,
            issue: Some(20),
            branch: None,
            container_id: "a".repeat(64),
            base_revision: String::new(),
            host_pid: 1,
            control: "b".repeat(64),
            repository: String::new(),
        };
        let carried = [
            "LOOM_SWEEP_ID=step-builder-20-1",
            "LOOM_SWEEP_CLAIM_OWNED=20",
            "LOOM_SWEEP_LEASE_RENEW_DISPATCHED=20",
            "LOOM_LEASE_HOST_ID=fleet-host-7",
            "LOOM_LEASE_PUBLISH_HOSTNAME=1",
            "GIT_AUTHOR_NAME=fleet-bot[bot]",
            "GIT_AUTHOR_EMAIL=1+fleet-bot[bot]@users.noreply.example.invalid",
            "GIT_COMMITTER_NAME=fleet-bot[bot]",
            "GIT_COMMITTER_EMAIL=1+fleet-bot[bot]@users.noreply.example.invalid",
        ];
        let dropped = [
            "LOOM_HOST_ID=fleet-host-7",
            "LOOM_ROLE_PR_LEASE_FILE=/host/leases",
            "LOOM_PICK_JOURNAL=/host/journal",
            "GIT_DIR=/host/repo/.git",
            "GIT_CONFIG_GLOBAL=/host/gitconfig",
        ];
        let input: Vec<String> = carried
            .iter()
            .chain(dropped.iter())
            .map(|s| (*s).to_string())
            .collect();
        let env = child_env(&input, &config, &job, &config.primary(), &no_auth()).unwrap();
        for item in carried {
            assert!(env.contains(&item.to_string()), "{item} must be carried: {env:?}");
        }
        for item in dropped {
            assert!(!env.contains(&item.to_string()), "{item} must not be carried");
        }
        // The value rule covers the new names too.
        let bad = ["GIT_AUTHOR_NAME=a\nb".to_string()];
        assert!(child_env(&bad, &config, &job, &config.primary(), &no_auth()).is_err());
    }

    /// The launch environment names the SELECTED clone and its repository, so
    /// a job on an additional clone never runs in, or is credentialed for, the
    /// account's primary (#10796).
    #[test]
    fn child_env_names_the_selected_additional_clone() {
        let mut config = Config {
            schema_version: 1,
            account: "test".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/o/primary".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: "/host/profile".into(),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        };
        let id = identify("https://example.invalid/o/second").unwrap();
        config.repositories.push(Registered {
            repository: id.url.clone(),
            base: "main".into(),
            slug: id.slug.clone(),
        });
        let target = config
            .target(Some("https://example.invalid/o/second"))
            .unwrap();
        let job = lease::Job {
            owner: "o".into(),
            kind: JobKind::Role,
            issue: None,
            branch: None,
            container_id: "a".repeat(64),
            base_revision: "b".repeat(40),
            host_pid: 1,
            control: "c".repeat(64),
            repository: id.url.clone(),
        };
        let env = child_env(&[], &config, &job, &target, &no_auth()).unwrap();
        let clone = format!("{WORK_ROOT}/{}", id.slug);
        assert!(env.contains(&format!("LOOM_WORKSPACE={clone}")));
        assert!(env.contains(&format!("LOOM_PRIVATE_CLONE={clone}")));
        assert!(env.contains(&format!("LOOM_PRIVATE_REPOSITORY={}", id.url)));
        assert!(!env.iter().any(|v| v.contains("/workspace/repo")), "{env:?}");
    }
    fn github_config(repository: &str) -> Config {
        Config {
            schema_version: 2,
            account: "agent-1".into(),
            container: "loom-codex-agent-1".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: repository.into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: "/host/profile".into(),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        }
    }

    fn host_args(workdir: &str) -> crate::session_exec::HostArgs {
        crate::session_exec::HostArgs {
            owner_pid: None,
            container: "loom-codex-agent-1".into(),
            workdir: workdir.into(),
            env: vec!["LOOM_WORKSPACE=/workspace/repo".into()],
            child_env: vec![],
            stderr_file: None,
            command: vec!["codex".into(), "exec".into()],
        }
    }

    /// #13937: a `session-exec host` into a private-clone session, with no
    /// inherited lease, gets the owner-matched token from the daemon's
    /// refreshed primary dir on the `docker` child only, never in argv, and
    /// never the stale copy under the managed repository's own `.loom/`.
    #[test]
    fn unleased_session_exec_host_sets_the_owner_token_on_the_child_only() {
        const SECRET: &str = "ghs_unleasedTESTONLY13937";
        let owner = "acme-host-13937";
        let config = github_config(&format!("https://github.com/{owner}/widget"));
        let primary = forge_auth::published_owner_workspace(owner);
        let stale = forge_auth::published_owner_workspace(owner);
        let fresh_dir =
            crate::credential_preflight::github_app_gh_config_dir_for_owner(primary.path(), owner);
        let checkout = primary.path().as_os_str().to_owned();
        let seen = std::cell::RefCell::new(None);
        let resolve = |root: &Path, repository: &str| {
            forge_auth::ExecAuth::resolve_with(
                root,
                repository,
                &|name| (name == "LOOM_MACHINE_CHECKOUT").then(|| checkout.clone()),
                &|dir| {
                    *seen.borrow_mut() = Some(dir.to_path_buf());
                    (dir == fresh_dir).then(|| SECRET.to_owned())
                },
            )
        };
        let mut args = host_args("/workspace/repo/.loom/worktrees/issue-14");
        authorize_unleased(&mut args, stale.path(), &config, &resolve);
        assert_eq!(seen.borrow().as_deref(), Some(fresh_dir.as_path()));
        let command = crate::session_exec::exec_command(&args, "id-1");
        let argv: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--env" && w[1] == "GH_TOKEN"),
            "{argv:?}"
        );
        assert!(!argv.iter().any(|a| a.contains(SECRET)), "token leaked into argv");
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
            })
            .collect();
        assert_eq!(envs, vec![("GH_TOKEN".into(), Some(SECRET.into()))]);
    }

    /// The session's repository belongs to an owner with no dir of its own:
    /// nothing is read and the exec carries no token, even though the primary
    /// root publishes a neighbour's.
    #[test]
    fn unleased_session_exec_host_forwards_nothing_for_another_owner() {
        let config = github_config("https://github.com/other-host-13937/widget");
        let primary = forge_auth::published_owner_workspace("acme-host-13937");
        let checkout = primary.path().as_os_str().to_owned();
        let resolve = |root: &Path, repository: &str| {
            forge_auth::ExecAuth::resolve_with(
                root,
                repository,
                &|name| (name == "LOOM_MACHINE_CHECKOUT").then(|| checkout.clone()),
                &|_| panic!("read another owner's token"),
            )
        };
        let mut args = host_args("/workspace/repo");
        let before = args.env.clone();
        authorize_unleased(&mut args, primary.path(), &config, &resolve);
        assert!(args.child_env.is_empty());
        assert_eq!(args.env, before);
    }

    /// Only an exec inside a registered clone is credentialed, and for that
    /// clone's own repository.
    #[test]
    fn the_workdir_selects_the_clone_and_anything_else_selects_none() {
        let mut config = github_config("https://github.com/acme/primary");
        let id = identify("https://github.com/acme/second").unwrap();
        config.repositories.push(Registered {
            repository: id.url.clone(),
            base: "main".into(),
            slug: id.slug.clone(),
        });
        let repo = |workdir: &str| target_for_workdir(&config, workdir).map(|t| t.repository);
        assert_eq!(repo("/workspace/repo").as_deref(), Some(config.repository.as_str()));
        assert_eq!(
            repo(&format!("{WORK_ROOT}/{}/.loom/worktrees/x", id.slug)).as_deref(),
            Some(id.url.as_str())
        );
        for elsewhere in [
            "/workspace",
            "/workspace/repository",
            "/tmp",
            "/workspace/work/nope",
        ] {
            assert_eq!(repo(elsewhere), None, "{elsewhere}");
        }
        let resolve =
            |_: &Path, _: &str| -> forge_auth::ExecAuth { panic!("resolved outside a clone") };
        let mut args = host_args("/tmp");
        authorize_unleased(&mut args, Path::new("/nonexistent"), &config, &resolve);
        assert!(args.child_env.is_empty());
    }

    /// #13916: the job exec on an App-only host names `GH_TOKEN` bare and the
    /// value rides only on the `docker` child; the argv never carries it.
    #[test]
    fn job_exec_names_a_resolved_token_bare_and_never_carries_its_value() {
        const SECRET: &str = "ghs_transportTESTONLY13916";
        let config = Config {
            schema_version: 1,
            account: "test".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://github.com/acme/widget".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: "/host/profile".into(),
            gh_config: None,
            session_image: None,
            extra_mounts: Vec::new(),
        };
        let job = lease::Job {
            owner: "o".into(),
            kind: JobKind::Role,
            issue: None,
            branch: None,
            container_id: "a".repeat(64),
            base_revision: "b".repeat(40),
            host_pid: 1,
            control: "c".repeat(64),
            repository: String::new(),
        };
        let ws = forge_auth::published_owner_workspace("acme");
        let auth =
            forge_auth::ExecAuth::resolve_with(ws.path(), &config.repository, &|_| None, &|_| {
                Some(SECRET.into())
            });
        let env = child_env(&[], &config, &job, &config.primary(), &auth).unwrap();
        assert!(env.contains(&"GH_TOKEN".to_string()), "{env:?}");
        assert!(!env.iter().any(|v| v.contains(SECRET)));
        assert_eq!(auth.child_env(), vec![("GH_TOKEN".into(), SECRET.into())]);
    }
}
