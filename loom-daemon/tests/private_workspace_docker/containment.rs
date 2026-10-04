//! Verified private-clone containment as runtime admission (#8787), against a
//! real Docker session, the real image-owned control bundle, real Git, the real
//! production adapters and the real `loom-daemon` launch path.
//!
//! Credential-free throughout: the only "credential" anywhere is the synthetic
//! disposable string in `synthetic-auth.json`, the forge is a local container,
//! and no model is ever called (the fixture `codex` is a shell script).
//!
//! **What the hook-trust fixture stands in for.** Codex persists hook trust
//! only through an interactive TUI decision. `docker/session/test-image.sh` §12
//! drives that real TUI against the real CLI and measures that a trusted hook
//! blocks a force-push while an untrusted one silently does not (#8839). Here,
//! the operator's one-time decision is stood in for by writing a synthetic
//! `hooks.state` entry into the disposable profile BEFORE the session binds it
//! read-only — the same observable signal `provision-codex-hooks.sh verify`
//! reads in production. What this module proves is the admission consequence:
//! with that signal absent, a mutable role is refused; with it present, the
//! mutable role runs — and every other obligation is still enforced.
use super::*;
use crate::dispatch::Environment;
use loom_daemon::runtime_preference::resolve_for_dispatch_with;
use loom_daemon::tokens_pool::private_workspace::{containment, JobKind};

fn preparer<'a>(root: &std::path::Path, issue: Option<u64>) -> containment::Preparer<'a> {
    containment::Preparer::new(
        root,
        JobKind::Sweep,
        issue,
        "containment-proof".into(),
        Box::new(|_| None),
    )
}

/// The operator's one-time Codex hook-trust decision, as a disposable fixture
/// (see the module header). Written while the session is stopped, because the
/// live session binds `config.toml` read-only over its own path.
///
/// Keyed exactly as Codex keys it: the hooks.json path under the session's
/// `CODEX_HOME` mount point, plus the position of Loom's managed entry. Trust
/// under any other key is not trust for Loom's hook, and admission ignores it.
fn establish_hook_trust(profile: &std::path::Path) {
    let hooks: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.join("hooks.json")).unwrap()).unwrap();
    let key = loom_daemon::tokens_pool::codex_hooks::loom_trust_keys(
        &hooks,
        std::path::Path::new(loom_daemon::tokens_pool::codex_hooks::SESSION_CODEX_HOME),
    )
    .into_iter()
    .next()
    .expect("the managed registration is provisioned before trust is established");
    let config = profile.join("config.toml");
    let existing = std::fs::read_to_string(&config).unwrap_or_default();
    std::fs::write(
        &config,
        format!("{existing}\n[hooks.state.\"{key}\"]\ntrusted_hash = \"fixture-trust-decision\"\n"),
    )
    .unwrap();
}

fn environment(f: &Fixture, root: &std::path::Path, name: &str, gh_host: &str) -> Environment {
    let [gh_bin, write_scope_cache] = f.write_scope_env();
    Environment::set(&[
        gh_bin,
        write_scope_cache,
        ("LOOM_WORKSPACE", Some(root.to_owned().into_os_string())),
        ("LOOM_CODEX_PROFILE_ROOT", Some(f.root.path().join("profiles").into_os_string())),
        ("LOOM_CODEX_PROFILE", Some(name.into())),
        ("LOOM_RUNTIME", Some("codex".into())),
        ("LOOM_DAEMON_SELF_BIN", Some(f.host.clone().into_os_string())),
        ("GH_CONFIG_DIR", Some(f.root.path().join("forge").into_os_string())),
        ("GH_HOST", Some(gh_host.into())),
        ("GH_TOKEN", Some(auth::GH_TOKEN.into())),
        ("LOOM_CODEX_AUTH_MODE_CHECK", Some("0".into())),
        ("CODEX_HOME", None),
        ("LOOM_CODEX_HOME", None),
        ("LOOM_PRIVATE_LEASE_FD", None),
        ("LOOM_ROLE", None),
        ("LOOM_CODEX_NO_EXEC", None),
        ("LOOM_SPAWN_NO_EXPORT", None),
    ])
}

/// Prepare the registry-shaped workspace the daemon dispatches from: a real Git
/// checkout pointed at the disposable forge, carrying the shipped role/runtime
/// manifests and adapters.
fn registry_workspace(f: &Fixture) -> std::path::PathBuf {
    let root = f.root.path().join("registry");
    checked(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&root)
            .output()
            .unwrap(),
    );
    checked(
        Command::new("git")
            .args(["remote", "add", "origin", &f.repository])
            .current_dir(&root)
            .output()
            .unwrap(),
    );
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

/// Bare-host, unmanaged-clone, untrusted-profile and stale-context sessions
/// must all stay refused for the containment claim, and the refusal must name
/// the precise obligation rather than a generic capability message.
#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn every_unverified_private_shape_is_refused_for_a_mutable_role() {
    let mut f = Fixture::with_adapters(true);
    f.repository = f.repository.replace("/gitea/", "/github/");
    let name = &f.names[0];
    let gh_host = reqwest::Url::parse(&f.repository)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();
    checked(
        f.command(&[
            "session",
            "start",
            name,
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
    let root = registry_workspace(&f);
    let _environment = environment(&f, &root, name, &gh_host);

    // ---- 1. bare host: the same workspace, no private session at all ------
    // Identical manifests and adapters; the ONLY difference is that no Codex
    // account here owns a private clone. Ordinary host admission must be
    // completely unchanged: the same capability refusal, no containment claim.
    {
        let empty = tempfile::tempdir().unwrap();
        let _no_profiles =
            Environment::set(&[("LOOM_CODEX_PROFILE_ROOT", Some(empty.path().into()))]);
        let refused =
            resolve_for_dispatch_with(&root, "builder", Some("codex"), &mut preparer(&root, None))
                .unwrap_err();
        assert_eq!(refused.unmet_capabilities, vec!["worktreeIsolation"]);
        assert!(refused.reason.contains("bare-host"), "{}", refused.reason);
        // And with no preparer at all — every probe and lock-held path — the
        // refusal is byte-for-byte the pre-#8787 one.
        let static_refusal =
            loom_daemon::runtime_admission::resolve_and_admit(&root, "builder", Some("codex"))
                .unwrap_err();
        assert_eq!(static_refusal.reason, "unmet capabilities: worktreeIsolation");
    }

    // ---- 2. a managed session with NO established hook trust --------------
    // The boundary itself is Ready and the registration names the image-owned
    // bridge; the ONLY thing missing is the operator's trust decision, without
    // which the pinned CLI reads the registration and then silently ignores it.
    let report: Value =
        serde_json::from_str(&f.exec(name, "loom-daemon private-workspace control")).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["managed"], true, "{report}");
    let untrusted =
        resolve_for_dispatch_with(&root, "builder", Some("codex"), &mut preparer(&root, None))
            .unwrap_err();
    assert_eq!(untrusted.unmet_capabilities, vec!["worktreeIsolation"]);
    assert!(
        untrusted
            .reason
            .contains("has not established Codex hook trust"),
        "{}",
        untrusted.reason
    );
    assert!(untrusted.reason.contains("fails OPEN"), "{}", untrusted.reason);
    // The same refusal reaches production dispatch, and nothing was claimed.
    let mut config = loom_daemon::sweep_registry::SweepRegistryConfig::new(root.clone());
    config.journal_path = Some(root.join("sweeps.json"));
    let registry = std::sync::Arc::new(std::sync::Mutex::new(
        loom_daemon::sweep_registry::SweepRegistry::new(config),
    ));
    let rejected = loom_daemon::sweep_registry::SweepRegistry::dispatch_unlocked(
        &registry,
        &loom_daemon::types::SweepKind::Issue(8787),
        None,
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(rejected.to_string().contains("hook trust"), "{rejected}");
    assert!(!root.join(".loom/locks/issue-8787").exists());
    // A refused admission holds no account: the next preparation succeeds.
    let free = loom_daemon::tokens_pool::private_workspace::dispatch::Selection::prepare(
        &root,
        "codex",
        None,
        JobKind::Role,
        None,
        "containment-not-leaked",
    )
    .unwrap()
    .unwrap();
    drop(free);

    // ---- 3. an UNMANAGED clone (no Loom surface) --------------------------
    // A clone that ships no Loom hook provisioner has no managed bridge to
    // enforce with, so it can never carry a mutable role however well contained
    // it is. The boundary reports that as `managed: false`, and
    // `containment::enforcing` refuses on it — covered at the unit layer against
    // a real `bundle::Report` by
    // `tokens_pool::private_workspace::containment_tests::an_unmanaged_clone_is_refused_even_when_the_boundary_is_ready`,
    // rather than by building a second disposable forge + image here purely to
    // observe a boolean this fixture's own clone cannot produce (it ships the
    // installed Loom surface by construction, and removing it would leave the
    // clone dirty, which `prepare` correctly refuses before admission is reached).

    // ---- 4. stale context: the bound container is replaced ---------------
    let selection = loom_daemon::tokens_pool::private_workspace::dispatch::Selection::prepare(
        &root,
        "codex",
        None,
        JobKind::Sweep,
        Some(8787),
        "containment-stale",
    )
    .unwrap()
    .unwrap();
    docker(&["rm", "-f", &format!("loom-codex-session-{name}")]);
    let stale = selection.contain(&root).unwrap_err().to_string();
    assert!(
        stale.contains("replaced") || stale.contains("disappeared") || stale.contains("stale"),
        "{stale}"
    );
    drop(selection);
}

/// The positive path, end to end through production code: admission on a
/// verified boundary, the real adapter chain, an allowed private issue-branch
/// commit and push, and every obligation containment does NOT cover still
/// enforced afterwards.
#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn verified_containment_admits_a_mutable_role_through_the_real_adapters() {
    let mut f = Fixture::with_adapters(true);
    f.repository = f.repository.replace("/gitea/", "/github/");
    let name = &f.names[0];
    let peer = &f.names[1];
    let gh_host = reqwest::Url::parse(&f.repository)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();
    let start = |account: &str| {
        checked(
            f.command(&[
                "session",
                "start",
                account,
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
    };
    start(name);
    // The operator's one-time trust decision (module header), then a restart so
    // the session binds the trusted `config.toml` read-only.
    let profile = f.root.path().join("profiles").join(name);
    f.cli(&["session", "stop", name, "--json"]);
    establish_hook_trust(&profile);
    start(name);
    // A peer account with its own session: nothing below may reach its profile
    // or its volume.
    start(peer);

    let protected = docker(&[
        "exec",
        &f.server,
        "git",
        "-C",
        "/srv/repo.git",
        "rev-parse",
        "main",
    ]);
    let root = registry_workspace(&f);
    let _environment = environment(&f, &root, name, &gh_host);

    // ---- admission --------------------------------------------------------
    let mut prep = preparer(&root, Some(8787));
    let admission =
        resolve_for_dispatch_with(&root, "sweep-lifecycle", Some("codex"), &mut prep).unwrap();
    let admitted = admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "codex");
    let execution = admitted.execution.clone().expect("containment provenance");
    assert_eq!(execution.mode, "private-clone");
    assert_eq!(execution.satisfied, vec!["worktreeIsolation"]);
    // The static manifest is NOT promoted by this admission; the record says so.
    assert_eq!(execution.native["worktreeIsolation"], "partial");
    assert_eq!(execution.native["hooks"], "partial");
    assert_eq!(execution.control.len(), 12);
    assert_eq!(execution.account, name.as_str());
    let shipped: Value =
        serde_json::from_slice(&std::fs::read(root.join(".loom/runtimes/codex.json")).unwrap())
            .unwrap();
    assert_eq!(shipped["capabilities"]["worktreeIsolation"], "partial");
    assert_eq!(shipped["capabilities"]["hooks"], "partial");
    // The provenance is durable and secret-free, and `session status` shows it.
    let status: Value =
        serde_json::from_str(&f.cli(&["session", "status", name, "--json"])).unwrap();
    assert_eq!(status["admission"]["execution"]["mode"], "private-clone");
    assert_eq!(status["admission"]["role"], "sweep-lifecycle");
    let rendered = serde_json::to_string(&status).unwrap();
    assert!(!rendered.contains("synthetic-credential-free-fixture"), "{rendered}");
    assert!(!rendered.contains(auth::GH_TOKEN), "{rendered}");

    // ---- the launch: the real adapters, on the selection that proved it ----
    let (selection, _model) = prep
        .take()
        .expect("the contained selection travels to the launch");
    let mut command = f.adapter(name, "mutate");
    command
        .env("LOOM_ROLE", "sweep-lifecycle")
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    selection.apply(&mut command);
    let child = command.spawn().unwrap();
    selection.spawned();
    drop(selection);
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = checked(output);
    assert!(stdout.contains("fixture-private-complete"), "{stdout}\n{stderr}");
    // The worker re-admitted itself in-container and said so, on the marker the
    // daemon and the worker share.
    assert!(
        stderr.contains("# LOOM_RUNTIME_CONTAINMENT ") && stderr.contains("mode=private-clone"),
        "{stderr}"
    );
    // The adapter relocated (did not skip) its mutable-role hook preflight.
    assert!(stderr.contains("verified-in-private-session"), "{stderr}");
    assert!(stderr.contains("trust-bypass=never"), "{stderr}");
    assert!(!stderr.contains("dangerously-bypass-hook-trust"), "{stderr}");

    // ---- allowed: an issue-branch commit and push inside the clone --------
    assert!(f
        .exec(name, "git -C /workspace/repo log -1 --format=%s origin/feature/issue-8786")
        .contains("fixture issue mutation"));
    assert!(stdout.contains("/pull/17"), "{stdout}");

    // ---- denied: protected remote operations ------------------------------
    // The forced guard policy reached the model's own process (the fixture
    // `codex` asserts it), and the protected ref never moved.
    assert_eq!(
        protected,
        docker(&[
            "exec",
            &f.server,
            "git",
            "-C",
            "/srv/repo.git",
            "rev-parse",
            "main",
        ]),
        "a contained mutable role must not move a protected remote ref"
    );

    // ---- denied: host, sibling and peer repositories ----------------------
    assert_eq!(std::fs::read_to_string(f.root.path().join("source/file")).unwrap(), "base");
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("host-sibling/keep")).unwrap(),
        "host fixture"
    );
    assert!(!root.join("mutation.txt").exists());
    f.exec(name, "test ! -e /peer-profile && test ! -e /peer-workspace");
    f.exec(
        name,
        "test ! -e /var/run/docker.sock && test ! -e /run/containerd/containerd.sock",
    );
    // The peer account's profile and volume are simply not reachable from here.
    f.exec(peer, "echo peer-only > /workspace/cache/peer-marker");
    f.exec(name, "test ! -e /workspace/cache/peer-marker");

    // ---- denied: host control / secondary executors -----------------------
    // (`run-job` is attempted by the fixture worker itself and must have been
    //  refused, or the script would have exited 91 above.)
    let run_job = Command::new("docker")
        .args([
            "exec",
            &format!("loom-codex-session-{name}"),
            "loom-daemon",
            "private-workspace",
            "execute",
            "--",
            "sh",
            "-c",
            "true",
        ])
        .output()
        .unwrap();
    assert!(!run_job.status.success(), "an unaudited argv shape was accepted");

    // ---- the credential boundary ------------------------------------------
    assert_eq!(
        std::fs::read_to_string(profile.join("auth.json")).unwrap(),
        "synthetic-credential-free-fixture"
    );
    assert!(!stderr.contains(auth::GH_TOKEN));
    assert!(f
        .exec(
            name,
            "grep -rl synthetic-credential-free-fixture /opt/loom/private-control /workspace/repo || true"
        )
        .trim()
        .is_empty());
    let record: Value = serde_json::from_slice(
        &std::fs::read(root.join(".loom/private-jobs/issue-8787.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["outcome"], "completed");
    let rendered = serde_json::to_string(&record).unwrap();
    assert!(!rendered.contains(auth::GH_TOKEN), "{rendered}");
}
