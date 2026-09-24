//! Verified private-clone containment admits a mutable role only through the
//! real launch path, and only while every remaining obligation holds (#8787).
//! Git, Docker, the adapters, transport, worker and managed hook bridge are
//! real; the Codex CLI and the forge are disposable fixtures.
use super::dispatch::Environment;
use super::*;
use loom_daemon::runtime_admission::resolve_and_admit;
use loom_daemon::tokens_pool::private_workspace::{containment, dispatch::Selection, JobKind};

fn registry(f: &Fixture) -> PathBuf {
    let root = f.root.path().join("registry");
    for args in [
        vec!["init", "-b", "main"],
        vec!["remote", "add", "origin", f.repository.as_str()],
    ] {
        checked(
            Command::new("git")
                .args(&args)
                .current_dir(&root)
                .output()
                .unwrap(),
        );
    }
    let defaults = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    for (src, dest) in [
        ("scripts", ".loom/scripts"),
        ("runtimes", ".loom/runtimes"),
        ("roles", ".loom/roles"),
        (".claude/commands/loom", ".claude/commands/loom"),
    ] {
        dispatch::copy_tree(&defaults.join(src), &root.join(dest));
    }
    root
}

fn lease_job(f: &Fixture, name: &str) -> PathBuf {
    f.root
        .path()
        .join("profiles/.private-sessions")
        .join(name)
        .join("job.json")
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn verified_clone_admits_builder_only_with_every_remaining_obligation() {
    let mut f = Fixture::with_adapters(true);
    f.repository = f.repository.replace("/gitea/", "/github/");
    let name = f.names[0].clone();
    let gh_host = reqwest::Url::parse(&f.repository)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();
    checked(
        f.command(&[
            "session",
            "start",
            &name,
            "--private-clone",
            &f.repository,
            "--image",
            &f.image,
        ])
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .output()
        .unwrap(),
    );
    let root = registry(&f);
    let _environment = Environment::set(&[
        ("LOOM_WORKSPACE", Some(root.clone().into_os_string())),
        ("LOOM_CODEX_PROFILE_ROOT", Some(f.root.path().join("profiles").into_os_string())),
        ("LOOM_CODEX_PROFILE", Some(name.clone().into())),
        ("LOOM_RUNTIME", Some("codex".into())),
        ("LOOM_DAEMON_SELF_BIN", Some(f.host.clone().into_os_string())),
        ("GH_CONFIG_DIR", Some(f.root.path().join("forge").into_os_string())),
        ("GH_HOST", Some(gh_host.clone().into())),
        ("GH_TOKEN", Some(auth::GH_TOKEN.into())),
        ("LOOM_CODEX_AUTH_MODE_CHECK", Some("0".into())),
        ("CODEX_HOME", None),
        ("LOOM_CODEX_HOME", None),
        ("LOOM_PRIVATE_LEASE_FD", None),
        ("LOOM_ROLE", None),
        ("LOOM_CODEX_NO_EXEC", None),
        ("LOOM_SPAWN_NO_EXPORT", None),
    ]);
    let rejection = || resolve_and_admit(&root, "builder", Some("codex")).unwrap_err();
    assert!(rejection().containment_eligible());
    let admit = |owner: &str| {
        containment::admit_new(
            &root,
            "builder",
            Some("codex"),
            rejection(),
            None,
            JobKind::Sweep,
            Some(8787),
            owner,
        )
    };
    // The disposable forge's own bare repository is the ground truth for refs.
    let remote_ref = |reference: &str| {
        docker(&[
            "exec",
            &f.server,
            "git",
            "-C",
            "/srv/repo.git",
            "rev-parse",
            "--verify",
            reference,
        ])
    };
    let remote_main = || remote_ref("refs/heads/main");
    let main_before = remote_main();

    // 1. The hook bridge is installed in the clone but not trusted: refused
    //    with the precise obligation, and the account lease is released.
    let Err(untrusted) = admit("fixture-untrusted") else {
        panic!("untrusted hooks were admitted")
    };
    assert_eq!(untrusted.unmet_capabilities, vec!["worktreeIsolation"]);
    assert!(untrusted.reason.contains("pre_tool_use hook"), "{}", untrusted.reason);
    assert!(!lease_job(&f, &name).exists());

    // Simulate the operator's documented one-time interactive trust step
    // (what Codex itself persists after the prompt). No Loom code path writes
    // this; production refuses until an operator does.
    let config = f
        .root
        .path()
        .join("profiles")
        .join(&name)
        .join("config.toml");
    let mut trust = std::fs::read_to_string(&config).unwrap_or_default();
    trust.push_str(
        "\n[hooks.state.\"loom-fixture-operator\"]\ntrusted_hash = \"operator-accepted\"\n",
    );
    std::fs::write(&config, trust).unwrap();

    // 2. A peer container attached to this account's volume: refused.
    let peer = format!("loom-codex-session-{}", f.names[2]);
    docker(&[
        "run",
        "-d",
        "--name",
        &peer,
        "--mount",
        &format!("type=volume,src=loom-codex-workspace-{name},dst=/peer"),
        "--entrypoint",
        "/bin/sleep",
        &f.image,
        "infinity",
    ]);
    let Err(shared) = admit("fixture-peer") else {
        panic!("a shared volume was admitted")
    };
    assert!(shared.reason.contains("another container"), "{}", shared.reason);
    docker(&["rm", "-f", &peer]);

    // 3. A busy account lease: refused, the holder is untouched.
    let holder = Selection::prepare(&root, "codex", None, JobKind::Role, None, "fixture-holder")
        .unwrap()
        .unwrap();
    let Err(busy) = admit("fixture-busy") else {
        panic!("a busy lease was admitted")
    };
    assert!(busy.reason.contains("busy"), "{}", busy.reason);
    drop(holder);

    // 4. Mutation between preparation and launch: a guard edit after the
    //    host proof is refused by the worker before Codex starts.
    let (admitted, selection, _) = admit("fixture-tamper").unwrap();
    assert!(admitted.execution.is_some());
    f.exec(&name, "printf 'exit 0\\n' >> /workspace/repo/.loom/hooks/guard-codex-bridge.sh");
    let mut command = f.adapter(&name, "guarded");
    command
        .env("LOOM_ROLE", "builder")
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN);
    selection.apply(&mut command);
    let output = command.output().unwrap();
    selection.spawned();
    drop(selection);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("control/guard integrity"), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-guarded-complete"));
    f.exec(&name, "git -C /workspace/repo checkout -- .loom/hooks/guard-codex-bridge.sh");

    // 5. A stale clone identity between preparation and launch: refused.
    let (_, selection, _) = admit("fixture-stale").unwrap();
    f.exec(&name, "cp /workspace/identity.json /tmp/identity.json && sed -i 's/\"revision\": \"[0-9a-f]*\"/\"revision\": \"0000000000000000000000000000000000000000\"/' /workspace/identity.json");
    let mut command = f.adapter(&name, "guarded");
    command
        .env("LOOM_ROLE", "builder")
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN);
    selection.apply(&mut command);
    let output = command.output().unwrap();
    selection.spawned();
    drop(selection);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("container identity"));
    f.exec(&name, "cp /tmp/identity.json /workspace/identity.json");

    // 6. The allowed path: private issue-branch edit, commit and push through
    //    spawn-worker -> spawn-codex -> session-exec -> worker, with the hook
    //    bridge denying force-push, merge, write_stdin and base-checkout
    //    patches while allowing the issue worktree.
    let (admitted, selection, _) = admit("fixture-builder").unwrap();
    let execution = admitted.execution.clone().unwrap();
    assert_eq!(execution.native["worktreeIsolation"], "partial");
    let status: Value =
        serde_json::from_str(&f.cli(&["session", "status", &name, "--json"])).unwrap();
    assert_eq!(status["admission"]["execution"]["mode"], "private-clone");
    let mut command = f.adapter(&name, "guarded");
    command
        .env("LOOM_ROLE", "builder")
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN);
    selection.apply(&mut command);
    let output = command.output().unwrap();
    selection.spawned();
    drop(selection);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = checked(output);
    assert!(stdout.contains("fixture-guarded-complete"), "{stdout}\n{stderr}");
    assert!(stderr.contains("# LOOM_RUNTIME_CONTAINMENT "), "{stderr}");
    assert!(stderr.contains("hooks=deferred-to-private-clone"), "{stderr}");
    assert!(!stderr.contains(auth::GH_TOKEN));
    let pushed = remote_ref("refs/heads/feature/issue-8787");
    assert_ne!(pushed.trim(), main_before.trim());
    // Protected remote refs are unchanged, and no host path was written.
    assert_eq!(remote_main(), main_before);
    assert!(!root.join("change.txt").exists());
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("host-sibling/keep")).unwrap(),
        "host fixture"
    );
    let status: Value =
        serde_json::from_str(&f.cli(&["session", "status", &name, "--json"])).unwrap();
    assert!(status["lease"].is_null(), "{status}");
    assert!(status.get("admission").is_none(), "{status}");
}
