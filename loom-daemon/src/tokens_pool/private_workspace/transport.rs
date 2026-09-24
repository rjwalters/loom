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
    // An inherited private lease means the daemon admitted this launch for a
    // private clone (possibly on containment evidence, #8787, with the host
    // hook preflight deferred to the worker). It must never degrade into an
    // ordinary shared-session exec.
    let leased = std::env::var_os("LOOM_PRIVATE_LEASE_FD").is_some();
    let host = |args| {
        if leased {
            bail!("private lease was inherited but this session is not a configured private clone; refusing a non-private launch");
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
    let selection = if std::env::var_os("LOOM_PRIVATE_LEASE_FD").is_none() {
        Some(
            dispatch::Selection::prepare(
                &root,
                "codex",
                None,
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
    let job = lease::read(&dir)?.context("private dispatch lost its durable job identity")?;
    let state =
        docker::inspect(&config.container)?.context("private session disappeared before launch")?;
    docker::validate(&config, &state)?;
    if state["Id"] != job.container_id || args.container != config.container {
        bail!("private session identity changed before spawn");
    }
    args.container = job.container_id.clone();
    args.workdir = REPO.into();
    let args_env = args.env.clone();
    args.env = child_env(&args.env, &config, &job)?;
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
    let after = docker::inspect(&config.container)?;
    let same = after.as_ref().is_some_and(|s| s["Id"] == job.container_id);
    // A mutable session could have edited the (writable) clone's guard bundle,
    // hook wiring or trust. Re-verify after it exits so such a change is never
    // silent: the job is marked and its lease retained for operator recovery,
    // and the next admission refuses until the bundle is restored (#8787).
    if same && containment::mutable(&role_of(&args_env)) {
        if let Err(error) = docker::verify_policy(&job.container_id, &job.base_revision) {
            export::update(&root, &config, &job, "policy-violated")?;
            bail!("private session changed its enforcement after launch; lease retained: {error}");
        }
    }
    export::update(&root, &config, &job, outcome)?;
    // Missing transport acknowledgements retain the durable lease even when
    // the host process exits. A later admission must prove the old worker idle.
    if result.is_ok() && same && docker::idle(after.as_ref(), &config.container)? {
        lease.finish()?;
    }
    result
}

/// The role the worker will see: only the forwarded value reaches `execute`,
/// so the post-session check keys on exactly the same one.
fn role_of(env: &[String]) -> String {
    env.iter()
        .find_map(|item| item.strip_prefix("LOOM_ROLE="))
        .map(str::to_owned)
        .unwrap_or_default()
}

fn child_env(input: &[String], config: &Config, job: &lease::Job) -> Result<Vec<String>> {
    let mut env = vec![
        format!("LOOM_WORKSPACE={REPO}"),
        format!("LOOM_PROJECT_ROOT={REPO}"),
        format!("LOOM_WORKTREE_ROOT={REPO}/.loom/worktrees"),
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
            if [
                "LOOM_ROLE",
                "LOOM_TERMINAL_ID",
                "LOOM_SWEEP_ID",
                "LOOM_SWEEP_CLAIM_OWNED",
            ]
            .contains(&key)
            {
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
    // Host-owned identity of the bound job, re-checked by the worker (#8787).
    env.push(format!("LOOM_PRIVATE_CONTAINER_ID={}", job.container_id));
    env.push(format!("LOOM_PRIVATE_BASE_REVISION={}", job.base_revision));
    for name in FORGE_ENV {
        if std::env::var_os(name).is_some() {
            env.push(name.into());
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            volume: "v".into(),
            profile: "/host/profile".into(),
            gh_config: None,
        };
        let job = lease::Job {
            owner: "sweep-7".into(),
            kind: JobKind::Sweep,
            issue: Some(7),
            branch: None,
            container_id: "a".repeat(64),
            base_revision: String::new(),
            host_pid: 1,
        };
        let env = child_env(
            &[
                "LOOM_WORKSPACE=/host/repo".into(),
                "LOOM_WORKTREE_PATH=/host/worktree".into(),
                "LOOM_DAEMON_SOCKET=/host/socket".into(),
                "LOOM_RUN_JOB_HOST=host".into(),
                "LOOM_ROLE=guide".into(),
            ],
            &config,
            &job,
        )
        .unwrap();
        assert!(env.contains(&"LOOM_WORKSPACE=/workspace/repo".into()));
        assert!(env.contains(&"LOOM_ROLE=guide".into()));
        // The bound job identity the worker re-checks comes from the host
        // lease, never from caller-supplied environment (#8787).
        assert!(env.contains(&format!("LOOM_PRIVATE_CONTAINER_ID={}", "a".repeat(64))));
        assert!(env.contains(&"LOOM_PRIVATE_BASE_REVISION=".to_string()));
        assert_eq!(role_of(&["LOOM_ROLE=builder".into()]), "builder");
        assert_eq!(role_of(&["LOOM_ROLE_X=builder".into()]), "");
        assert!(!env
            .iter()
            .any(|v| v.contains("/host/") || v.starts_with("LOOM_RUN_JOB")));
    }
}
