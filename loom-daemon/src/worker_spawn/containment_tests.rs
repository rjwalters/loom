//! Unit coverage for the native-ephemeral launch profile (issue #8403).
//!
//! Extracted to a sibling file rather than an inline `#[cfg(test)]` module so
//! `containment.rs` stays small enough to hold in view (file-size policy).
//!
//! These tests assert the SHAPE of the dispatch — the properties a reviewer
//! would otherwise have to run a live container to see: that every isolated
//! directory lands inside the container's ephemeral layer, that credentials
//! are forwarded by name and never by value, and that the telemetry marker
//! names `native-ephemeral`. Live-container assertions (a real canary run,
//! post-run writable-layer scans, `cancel_sweep` teardown timing) are
//! deliberately NOT here: they need docker, a provider key, and a fleet host.

use super::*;
use serde_json::json;

/// Serialize env mutation — `enabled`/`resolve` read process-global env.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clear every env var these tests key on, so one test's leftovers cannot
/// decide another's outcome.
fn clear_env() {
    for key in [
        "LOOM_NATIVE_CONTAINERIZED",
        "LOOM_SPAWN_CONTAINERIZED",
        "LOOM_NATIVE_CONTAINER_IMAGE",
        "LOOM_SWEEP_CONTAINER_CPUS",
        "LOOM_SWEEP_CONTAINER_MEMORY",
        "LOOM_SWEEP_CPU_BUDGET_CORES",
        "LOOM_SWEEP_INFLIGHT_SWEEPS",
        "LOOM_SWEEP_CONTAINER_RESERVED_MEMORY_MB",
        "LOOM_SWEEP_CLAIM_OWNED",
        "CARGO_TARGET_DIR",
        "KIMI_CODE_HOME",
        "KIMI_MODEL_API_KEY",
        "LOOM_NATIVE_CONTAINMENT",
    ] {
        std::env::remove_var(key);
    }
}

fn profile(cpus: Option<&str>, memory: Option<&str>) -> Profile {
    Profile {
        image: "img:test".to_string(),
        cpus: cpus.map(str::to_string),
        memory: memory.map(str::to_string),
        launch_id: "deadbeef".to_string(),
    }
}

fn argv(command: &Command) -> Vec<String> {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

// --- Enablement -------------------------------------------------------

#[test]
fn disabled_by_default() {
    let _g = env_lock();
    clear_env();
    assert!(!enabled(&json!({})));
    assert!(resolve(&json!({})).is_none());
}

#[test]
fn config_ephemeral_enables_it() {
    let _g = env_lock();
    clear_env();
    let config = json!({"runtimes":{"containment":{"native":"ephemeral"}}});
    assert!(enabled(&config));
    assert!(resolve(&config).is_some());
}

#[test]
fn claude_containment_flag_does_not_generalise_to_native() {
    // runtimes.containment.enabled selects the `loom-worker` image, which
    // ships no native CLI — inheriting it would dispatch a native sweep into
    // an image that cannot run it.
    let _g = env_lock();
    clear_env();
    assert!(!enabled(&json!({"runtimes":{"containment":{"enabled":true}}})));
}

#[test]
fn env_overrides_config_both_ways() {
    let _g = env_lock();
    clear_env();
    let on = json!({"runtimes":{"containment":{"native":"ephemeral"}}});
    std::env::set_var("LOOM_NATIVE_CONTAINERIZED", "0");
    assert!(!enabled(&on), "env must be able to turn config OFF");
    std::env::set_var("LOOM_NATIVE_CONTAINERIZED", "1");
    assert!(enabled(&json!({})), "env must be able to turn it ON alone");
    clear_env();
}

#[test]
fn recursion_sentinel_stops_a_second_nesting() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_NATIVE_CONTAINERIZED", "1");
    std::env::set_var("LOOM_SPAWN_CONTAINERIZED", "1");
    assert!(
        resolve(&json!({})).is_none(),
        "the re-exec'd copy inside the container must dispatch bare, not nest"
    );
    clear_env();
}

// --- Limits -----------------------------------------------------------

#[test]
fn cpus_falls_back_to_the_bare_metal_budget_then_unbounded() {
    let _g = env_lock();
    clear_env();
    assert_eq!(resolve_cpus(&json!({})), None, "no budget => no --cpus flag");
    std::env::set_var("LOOM_SWEEP_CPU_BUDGET_CORES", "3");
    assert_eq!(resolve_cpus(&json!({})).as_deref(), Some("3"));
    assert_eq!(
        resolve_cpus(&json!({"runtimes":{"containment":{"cpus":"6"}}})).as_deref(),
        Some("6"),
        "config outranks the host-wide bare-metal budget"
    );
    std::env::set_var("LOOM_SWEEP_CONTAINER_CPUS", "9");
    assert_eq!(
        resolve_cpus(&json!({"runtimes":{"containment":{"cpus":"6"}}})).as_deref(),
        Some("9"),
        "env outranks config"
    );
    clear_env();
}

#[test]
fn memory_is_always_capped() {
    let _g = env_lock();
    clear_env();
    let resolved = resolve_memory(&json!({})).expect("memory is always applied");
    assert!(
        resolved.ends_with('m'),
        "computed share carries docker's unit suffix, got {resolved}"
    );
    std::env::set_var("LOOM_SWEEP_CONTAINER_MEMORY", "2g");
    assert_eq!(resolve_memory(&json!({})).as_deref(), Some("2g"));
    clear_env();
}

#[test]
fn memory_budget_divides_across_in_flight_sweeps() {
    // Mirrors lib/memory-budget.sh's loom_mem_budget_mb, including its floor.
    assert_eq!(budget_mb(16384, 2048, 1), 14336);
    assert_eq!(budget_mb(16384, 2048, 4), 3584);
    assert_eq!(budget_mb(1024, 2048, 1), 512, "floor, never zero or negative");
    assert_eq!(budget_mb(16384, 2048, 0), 14336, "a 0 divisor is treated as 1");
}

#[test]
fn meminfo_total_is_parsed_and_junk_is_rejected() {
    assert_eq!(
        parse_meminfo_total_kb("MemFree: 12 kB\nMemTotal:   16384000 kB\n"),
        Some(16_384_000)
    );
    assert_eq!(parse_meminfo_total_kb("MemTotal:  0 kB\n"), None);
    assert_eq!(parse_meminfo_total_kb("nothing here\n"), None);
}

// --- Telemetry --------------------------------------------------------

#[test]
fn dispatch_marker_names_the_native_containment_kind() {
    let marker = profile(Some("4"), Some("2048m")).dispatch_marker();
    assert_eq!(
        marker,
        "# LOOM_DISPATCH_MODE mode=container image=img:test cpus=4 memory=2048m containment=native-ephemeral"
    );
}

#[test]
fn dispatch_marker_uses_none_not_an_empty_field() {
    let marker = profile(None, None).dispatch_marker();
    assert!(marker.contains("cpus=none"), "{marker}");
    assert!(marker.contains("memory=none"), "{marker}");
}

// --- The docker invocation -------------------------------------------

fn build(profile: &Profile, credentials: &[&str]) -> Vec<String> {
    let workspace = Path::new("/srv/repo");
    let command = docker_command(
        profile,
        workspace,
        Path::new("/srv/repo/.loom/worktrees/issue-1"),
        None,
        &[OsString::from("-p"), OsString::from("/loom:sweep 1")],
        credentials,
    )
    .expect("docker command");
    argv(&command)
}

#[test]
fn workspace_is_parity_mounted_and_the_cwd_is_preserved() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    let args = build(&profile(Some("2"), Some("1g")), &[]);
    assert_eq!(args[0], "docker");
    assert!(args.contains(&"/srv/repo:/srv/repo".to_string()), "{args:?}");
    assert!(
        args.contains(&"/srv/repo/.loom/worktrees/issue-1".to_string()),
        "-w must preserve the worktree cwd: {args:?}"
    );
    assert!(
        args.last().unwrap() == "/loom:sweep 1",
        "worker args are forwarded unchanged: {args:?}"
    );
    assert!(
        args.contains(&"/srv/repo/.loom/scripts/spawn-worker.sh".to_string()),
        "the container re-execs the SAME dispatcher, not a new wrapper: {args:?}"
    );
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
fn every_isolated_directory_is_inside_the_ephemeral_layer() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    let p = profile(None, Some("1g"));
    let args = build(&p, &[]);
    for (key, _) in ISOLATED_DIRS {
        let assignment = args
            .iter()
            .find(|a| a.starts_with(&format!("{key}=")))
            .unwrap_or_else(|| panic!("{key} must be set: {args:?}"));
        let value = assignment.split_once('=').unwrap().1;
        assert!(
            value.starts_with(&p.ephemeral_root()),
            "{key} must live in the per-launch ephemeral layer, got {value}"
        );
        assert!(
            !value.starts_with("/srv/repo"),
            "{key} must NOT land in the parity-mounted workspace (shared across workers), got {value}"
        );
    }
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

/// #8565: Kimi keeps config, sessions, logs, credentials, plugins AND its
/// downloaded `rg`/`fd` binaries under one `KIMI_CODE_HOME`, defaulting to
/// `$HOME/.kimi-code`. Left unset, every contained Kimi worker would land on
/// the container home's single `.kimi-code` — disjoint between containers, but
/// not inspectably per-launch, and identical in shape to the uncontained
/// shared-home bug this module exists to close.
#[test]
fn kimi_code_home_is_relocated_per_launch_and_is_not_the_container_home() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    let p = profile(None, Some("1g"));
    let args = build(&p, &[]);
    let home = args
        .iter()
        .find(|a| a.starts_with("KIMI_CODE_HOME="))
        .unwrap_or_else(|| panic!("KIMI_CODE_HOME must be set: {args:?}"));
    let value = home.split_once('=').unwrap().1;
    assert_eq!(value, format!("{}/kimi", p.ephemeral_root()), "{args:?}");
    assert!(
        !value.starts_with(&format!("{CONTAINER_HOME}/.kimi-code")),
        "the default shared home is exactly what the relocation avoids: {value}"
    );

    // Two launches differ by PATH, not only by container.
    let other = Profile {
        launch_id: "feedface".to_string(),
        ..p.clone()
    };
    let other_args = build(&other, &[]);
    let other_home = other_args
        .iter()
        .find(|a| a.starts_with("KIMI_CODE_HOME="))
        .expect("KIMI_CODE_HOME");
    assert_ne!(
        home, other_home,
        "two concurrent Kimi workers must not share a session store, log or credential file"
    );
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

/// The runtime half of the image's version pin, for Kimi as for OpenCode: a
/// pinned install is worth nothing if the CLI can update itself, and a
/// dispatched worker must not phone home.
#[test]
fn autoupdate_and_telemetry_are_disabled_for_every_pinned_cli() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    let args = build(&profile(None, Some("1g")), &[]);
    for expected in [
        "OPENCODE_DISABLE_AUTOUPDATE=1",
        "KIMI_CODE_NO_AUTO_UPDATE=1",
        "KIMI_DISABLE_TELEMETRY=1",
    ] {
        assert!(args.iter().any(|a| a == expected), "missing {expected}: {args:?}");
    }
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

/// A credential NAME that collides with a relocated directory must not be
/// forwarded: `-e NAME` (no `=`) re-reads the host's value and, coming later
/// on the command line, would beat the assignment that established the
/// per-launch path.
#[test]
fn a_credential_name_cannot_override_a_per_launch_directory() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("KIMI_CODE_HOME", "/host/shared/.kimi-code");
    let p = profile(None, Some("1g"));
    let args = build(&p, &["KIMI_CODE_HOME", "XDG_DATA_HOME"]);
    assert!(
        !args
            .iter()
            .any(|a| a == "KIMI_CODE_HOME" || a == "XDG_DATA_HOME"),
        "a bare -e for a relocated directory would reinstate the host value: {args:?}"
    );
    assert!(
        args.iter()
            .any(|a| a == &format!("KIMI_CODE_HOME={}/kimi", p.ephemeral_root())),
        "the per-launch assignment must survive: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains("/host/shared")),
        "no host state path may reach the container: {args:?}"
    );
    std::env::remove_var("KIMI_CODE_HOME");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

/// The credential-by-name contract is runtime-agnostic, but Kimi's own key
/// name is the one an API-key Kimi profile forwards (`credentialTargets.kimi`
/// → `KIMI_MODEL_API_KEY`), so pin it explicitly alongside the generic case.
#[test]
fn a_kimi_api_key_is_forwarded_by_name_and_never_by_value() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("KIMI_MODEL_API_KEY", "sk-kimi-super-secret-value");
    let args = build(&profile(None, Some("1g")), &["KIMI_MODEL_API_KEY"]);
    assert!(
        args.iter().any(|a| a == "KIMI_MODEL_API_KEY"),
        "the key must be forwarded by bare name: {args:?}"
    );
    assert!(
        !args
            .iter()
            .any(|a| a.contains("sk-kimi-super-secret-value")),
        "no argv element may carry the key's VALUE (it would land in `ps`): {args:?}"
    );
    std::env::remove_var("KIMI_MODEL_API_KEY");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

/// The container-side half: the outer dispatch can only NAME the per-launch
/// directories, and the image's `.loom-native` tree is deliberately empty, so
/// the re-exec'd copy has to create them. Kimi's home is a download target
/// (`$KIMI_CODE_HOME/bin/rg`), not merely a config root, so "the CLI will
/// mkdir it" is not a safe assumption to carry.
#[test]
fn the_reexeced_copy_materializes_the_named_directories_and_nothing_else() {
    let _g = env_lock();
    clear_env();
    let root = std::env::temp_dir().join(format!("loom-8565-{}", uuid::Uuid::new_v4().simple()));
    let home = root.join("kimi");
    std::env::set_var("KIMI_CODE_HOME", &home);

    // Host dispatch (no containment marker) must not touch the filesystem.
    materialize_launch_dirs();
    assert!(!home.exists(), "a host launch must be byte-for-byte unchanged");

    std::env::set_var("LOOM_NATIVE_CONTAINMENT", KIND);
    materialize_launch_dirs();
    assert!(home.is_dir(), "the relocated home must exist before the CLI runs");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&home)
            .expect("home metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "per-launch state is private, got {mode:o}");
    }
    // Idempotent: a second pass over an existing tree is not an error.
    materialize_launch_dirs();
    assert!(home.is_dir());

    let _ = std::fs::remove_dir_all(&root);
    std::env::remove_var("LOOM_NATIVE_CONTAINMENT");
    clear_env();
}

#[test]
fn two_launches_get_disjoint_ephemeral_roots() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_NATIVE_CONTAINERIZED", "1");
    let a = resolve(&json!({})).expect("profile");
    let b = resolve(&json!({})).expect("profile");
    assert_ne!(
        a.ephemeral_root(),
        b.ephemeral_root(),
        "concurrent native workers must not share a session store or auth.json"
    );
    clear_env();
}

#[test]
fn credentials_are_forwarded_by_name_and_never_by_value() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("LOOM_TEST_FAKE_API_KEY", "sk-super-secret-value");
    let args = build(&profile(None, Some("1g")), &["LOOM_TEST_FAKE_API_KEY"]);
    assert!(
        args.iter().any(|a| a == "LOOM_TEST_FAKE_API_KEY"),
        "the credential must be forwarded by bare name: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains("sk-super-secret-value")),
        "no argv element may carry the key's VALUE (it would land in `ps`): {args:?}"
    );
    std::env::remove_var("LOOM_TEST_FAKE_API_KEY");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
fn an_absent_credential_var_is_not_forwarded() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::remove_var("LOOM_TEST_MISSING_KEY");
    let args = build(&profile(None, Some("1g")), &["LOOM_TEST_MISSING_KEY", ""]);
    assert!(
        !args.iter().any(|a| a == "LOOM_TEST_MISSING_KEY"),
        "forwarding `-e NAME` for an unset NAME is noise at best: {args:?}"
    );
    assert!(args.iter().any(|a| a == "-e"), "sanity: -e flags are present");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
fn a_redirected_cargo_cache_is_parity_mounted_and_forwarded() {
    // MOUNT-CONTRACT.md §4: without this the contained sweep recompiles the
    // world into a writable layer `--rm` then discards.
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    let cache = std::env::temp_dir().join(format!("loom-8403-cargo-{}", std::process::id()));
    std::fs::create_dir_all(&cache).expect("cache dir");
    std::env::set_var("CARGO_TARGET_DIR", &cache);
    let args = build(&profile(None, Some("1g")), &[]);
    let spec = format!("{0}:{0}", cache.display());
    assert!(
        args.contains(&spec),
        "an out-of-workspace CARGO_TARGET_DIR must be parity-mounted: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "CARGO_TARGET_DIR"),
        "…and forwarded by name so the container's cargo uses it: {args:?}"
    );
    let _ = std::fs::remove_dir_all(&cache);
    clear_env();
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
fn a_cargo_cache_inside_the_workspace_is_not_mounted_twice() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("CARGO_TARGET_DIR", "/srv/repo/target");
    let args = build(&profile(None, Some("1g")), &[]);
    assert!(
        !args.iter().any(|a| a.starts_with("/srv/repo/target:")),
        "the workspace parity mount already covers it: {args:?}"
    );
    clear_env();
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
fn host_only_paths_are_not_forwarded_into_the_container() {
    assert!(!forwarded_by_name("LOOM_OPENCODE_BIN"));
    assert!(!forwarded_by_name("LOOM_PI_BIN"));
    assert!(!forwarded_by_name("LOOM_KIMI_BIN"));
    assert!(!forwarded_by_name("LOOM_DAEMON_BIN"));
    assert!(!forwarded_by_name("LOOM_SPAWN_CONTAINERIZED"));
    assert!(!forwarded_by_name("PATH"));
    assert!(
        !forwarded_by_name("CLAUDE_CODE_OAUTH_TOKEN"),
        "the native container has no business holding a Claude token"
    );
    assert!(forwarded_by_name("LOOM_ROLE"));
    assert!(forwarded_by_name("LOOM_SWEEP_CLAIM_OWNED"));
    assert!(forwarded_by_name("GH_TOKEN"));
}

#[test]
fn limits_and_labels_are_applied_when_resolved() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    std::env::set_var("LOOM_SWEEP_CLAIM_OWNED", "8403");
    let args = build(&profile(Some("2"), Some("1500m")), &[]);
    for expected in [
        "--cpus",
        "2",
        "--memory",
        "1500m",
        "loom.containment=native-ephemeral",
        "loom.dispatch.cpus=2",
        "loom.dispatch.memory=1500m",
        "loom.sweep.issue=8403",
    ] {
        assert!(args.iter().any(|a| a == expected), "missing {expected} in {args:?}");
    }
    clear_env();
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
fn an_unbounded_cpu_axis_emits_no_flag_at_all() {
    let _g = env_lock();
    clear_env();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    let args = build(&profile(None, Some("1g")), &[]);
    assert!(!args.iter().any(|a| a == "--cpus"), "{args:?}");
    assert!(!args.iter().any(|a| a.starts_with("loom.dispatch.cpus")), "{args:?}");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}
