//! The relay's session side (#10964): who gets wired, with what, and for how
//! long. Every test that reads the opt-in is `#[serial]` and clears the env
//! override first — the same discipline `observability`'s own config tests
//! use — and none registers the process-global relay, so "no receiver is
//! running" stays true for every other test in this binary.
use super::*;
use serial_test::serial;

struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn clear() -> Self {
        let keys = [ENABLED_ENV, "LOOM_SWEEP_CONTAINERIZED", RELAY_HEADERS_ENV];
        let previous = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for key in keys {
            std::env::remove_var(key);
        }
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A workspace whose config carries `block` as its whole content.
fn workspace(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(dir.path().join(".loom/config.json"), config).unwrap();
    dir
}

const OPTED_IN: &str = r#"{"observability":{"agentRelay":{"enabled":true}}}"#;

fn relay() -> Arc<Relay> {
    Relay::new(SocketAddr::from(([127, 0, 0, 1], 43180)), "host-fixture", None).unwrap()
}

fn sweep(root: &Path, issue: u32, sweep_id: &str) -> SessionIdentity {
    SessionIdentity {
        harness: Harness::ClaudeCode,
        kind: SessionKind::Sweep,
        role: None,
        issue: Some(issue),
        sweep_id: Some(sweep_id.to_string()),
        workspace_root: root.to_path_buf(),
    }
}

/// The command's explicit environment edits: `Some` set, `None` removed.
fn env_of(command: &Command) -> Vec<(String, Option<String>)> {
    command
        .get_envs()
        .map(|(k, v)| {
            (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
        })
        .collect()
}

fn value<'a>(env: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.as_deref())
}

/// The token a prepared command carries.
fn token_of(command: &Command) -> String {
    let env = env_of(command);
    value(&env, RELAY_HEADERS_ENV)
        .and_then(|header| header.strip_prefix("Authorization=Bearer "))
        .unwrap()
        .to_string()
}

#[test]
#[serial]
fn the_relay_is_off_unless_its_own_switch_is_set() {
    let _env = EnvGuard::clear();
    // An `otlp` exporter configured and enabled is not the relay's opt-in.
    let exporting = workspace(
        r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
    );
    assert!(!enabled(exporting.path()));
    assert!(!enabled(workspace("{}").path()));
    assert!(enabled(workspace(OPTED_IN).path()));
    assert!(!enabled(
        workspace(r#"{"observability":{"agentRelay":{"enabled":false}}}"#).path()
    ));
}

#[test]
#[serial]
fn the_env_override_wins_in_both_directions() {
    let _env = EnvGuard::clear();
    std::env::set_var(ENABLED_ENV, "0");
    assert!(!enabled(workspace(OPTED_IN).path()));
    std::env::set_var(ENABLED_ENV, "true");
    assert!(enabled(workspace("{}").path()));
}

#[test]
#[serial]
fn with_no_receiver_running_a_launch_gets_nothing_from_loom() {
    let _env = EnvGuard::clear();
    // Opted in, admitted for a wired harness — and still nothing, because
    // this process started no receiver. This is the whole of what a session
    // on a machine without a relaying daemon sees.
    let root = workspace(OPTED_IN);
    assert!(global().is_none(), "no test may register the global relay");
    let mut sweep_command = Command::new("/bin/true");
    assert!(!prepare_sweep_child(
        &mut sweep_command,
        root.path(),
        Some("claude"),
        Some(7),
        "sw-1"
    ));
    assert!(env_of(&sweep_command).is_empty());
    let mut role_command = Command::new("/bin/true");
    assert!(
        prepare_role_child(&mut role_command, root.path(), Some("claude"), "judge", None).is_none()
    );
    assert!(env_of(&role_command).is_empty());
    // Ending an execution with no receiver is a no-op, not a panic.
    end_execution("sw-1");
}

#[test]
#[serial]
fn a_session_started_outside_the_daemon_gets_no_exporter_environment() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut launched = Command::new("/bin/true");
    let lease = relay.prepare(&mut launched, root.path(), sweep(root.path(), 7, "sw-1"));
    assert!(lease.is_some());
    // The address and token exist in exactly one place: the launched child's
    // own `Command`. The daemon's process environment — the only thing a
    // session started some other way could inherit from it — is untouched.
    for name in RELAY_SET_ENV {
        assert!(
            std::env::var_os(name).is_none_or(|v| !v.to_string_lossy().contains("127.0.0.1:43180")),
            "{name} leaked into the daemon's own environment"
        );
    }
    assert!(std::env::var_os(RELAY_HEADERS_ENV).is_none());
    // A command the relay was never asked to prepare carries nothing.
    assert!(env_of(&Command::new("/bin/true")).is_empty());
}

#[test]
#[serial]
fn a_workspace_that_did_not_opt_in_is_left_untouched_by_a_running_relay() {
    let _env = EnvGuard::clear();
    let root = workspace("{}");
    let relay = relay();
    let mut command = Command::new("/bin/true");
    command.env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://127.0.0.1:1");
    let before = env_of(&command);
    assert!(relay
        .prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"))
        .is_none());
    assert_eq!(env_of(&command), before);
    assert_eq!(relay.live_sessions(), 0);
}

#[test]
#[serial]
fn a_wired_child_is_pointed_at_the_loopback_receiver_with_its_own_token() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    // Inherited overrides that would redirect a signal or widen capture.
    command.env("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://127.0.0.1:1/v1/logs");
    command.env("OTEL_LOG_USER_PROMPTS", "1");
    let _lease = relay
        .prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"))
        .unwrap();
    let env = env_of(&command);
    assert_eq!(value(&env, "OTEL_EXPORTER_OTLP_ENDPOINT"), Some("http://127.0.0.1:43180"));
    assert_eq!(value(&env, "OTEL_EXPORTER_OTLP_PROTOCOL"), Some("http/protobuf"));
    assert_eq!(value(&env, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some("1"));
    assert_eq!(value(&env, "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA"), Some("1"));
    for signal in [
        "OTEL_METRICS_EXPORTER",
        "OTEL_LOGS_EXPORTER",
        "OTEL_TRACES_EXPORTER",
    ] {
        assert_eq!(value(&env, signal), Some("otlp"), "{signal}");
    }
    for name in RELAY_SET_ENV {
        assert!(value(&env, name).is_some(), "{name} not set");
    }
    for name in RELAY_CLEARED_ENV {
        let entry = env.iter().find(|(key, _)| key == name);
        assert_eq!(entry.map(|(_, v)| v.is_none()), Some(true), "{name} not removed");
    }
    let token = token_of(&command);
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    let bound = relay.authorize(&token).unwrap();
    assert_eq!(bound.identity, sweep(root.path(), 7, "sw-1"));
    assert_eq!(bound.host_id, "host-fixture");
}

#[test]
#[serial]
fn every_launch_gets_a_different_token_bound_to_its_own_session() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut first = Command::new("/bin/true");
    let mut second = Command::new("/bin/true");
    let _a = relay.prepare(&mut first, root.path(), sweep(root.path(), 7, "sw-a"));
    let _b = relay.prepare(&mut second, root.path(), sweep(root.path(), 8, "sw-b"));
    let (a, b) = (token_of(&first), token_of(&second));
    assert_ne!(a, b);
    assert_eq!(relay.authorize(&a).unwrap().identity.issue, Some(7));
    assert_eq!(relay.authorize(&b).unwrap().identity.issue, Some(8));
    // Anything else is nobody: a guess, an empty value, a near miss.
    let near_miss = format!("{}0", &a[..63]);
    for stranger in ["", "not-a-token", near_miss.as_str()] {
        if stranger != a {
            assert!(relay.authorize(stranger).is_none());
        }
    }
}

#[test]
#[serial]
fn the_token_is_never_in_the_registry_or_its_debug_output() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    let _lease = relay.prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"));
    let token = token_of(&command);
    assert!(!format!("{relay:?}").contains(&token));
    let state = relay.lock();
    assert!(state.sessions.keys().all(|key| hex::encode(key) != token));
    assert!(state.sessions.contains_key(&digest(&token)));
}

#[test]
#[serial]
fn dropping_a_lease_ends_the_session() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    let lease = relay.prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"));
    let token = token_of(&command);
    assert!(relay.authorize(&token).is_some());
    drop(lease);
    assert!(relay.authorize(&token).is_none());
    assert_eq!(relay.live_sessions(), 0);
}

#[test]
#[serial]
fn a_sweep_session_lives_until_its_execution_ends_and_no_other_does() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut first = Command::new("/bin/true");
    let mut second = Command::new("/bin/true");
    relay
        .prepare(&mut first, root.path(), sweep(root.path(), 7, "sw-a"))
        .unwrap()
        .until_execution_ends();
    relay
        .prepare(&mut second, root.path(), sweep(root.path(), 8, "sw-b"))
        .unwrap()
        .until_execution_ends();
    let (a, b) = (token_of(&first), token_of(&second));
    assert!(relay.authorize(&a).is_some(), "detaching the lease must not end the session");
    assert_eq!(relay.end_execution("sw-a"), 1);
    assert!(relay.authorize(&a).is_none());
    assert!(relay.authorize(&b).is_some());
    assert_eq!(relay.end_execution("sw-a"), 0);
}

#[test]
#[serial]
fn a_token_past_its_maximum_lifetime_is_refused() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    relay
        .prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"))
        .unwrap()
        .until_execution_ends();
    let token = token_of(&command);
    let Some(long_ago) = Instant::now().checked_sub(MAX_SESSION_LIFETIME) else {
        return; // A host up for less than the lifetime cannot represent it.
    };
    relay
        .lock()
        .sessions
        .get_mut(&digest(&token))
        .unwrap()
        .registered_at = long_ago;
    assert!(relay.authorize(&token).is_none());
    assert_eq!(relay.live_sessions(), 0);
}

#[test]
#[serial]
fn a_containerized_launch_is_not_wired() {
    let _env = EnvGuard::clear();
    let relay = relay();
    // By the child's own environment…
    let root = workspace(OPTED_IN);
    let mut command = Command::new("/bin/true");
    command.env("LOOM_SWEEP_CONTAINERIZED", "1");
    let before = env_of(&command);
    assert!(relay
        .prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"))
        .is_none());
    assert_eq!(env_of(&command), before);
    // …or by the workspace's containment config.
    let contained = workspace(
        r#"{"observability":{"agentRelay":{"enabled":true}},"runtimes":{"containment":{"enabled":true}}}"#,
    );
    let mut command = Command::new("/bin/true");
    assert!(relay
        .prepare(&mut command, contained.path(), sweep(contained.path(), 7, "sw-1"))
        .is_none());
    assert!(env_of(&command).is_empty());
    // An explicit `0` on the child overrides a containing config.
    let mut command = Command::new("/bin/true");
    command.env("LOOM_SWEEP_CONTAINERIZED", "0");
    assert!(relay
        .prepare(&mut command, contained.path(), sweep(contained.path(), 7, "sw-1"))
        .is_some());
}

#[test]
fn only_an_admitted_wired_runtime_names_a_harness() {
    assert_eq!(Harness::from_admitted_runtime(Some("claude")), Some(Harness::ClaudeCode));
    // No admission: the spawn script picks the runtime, so the daemon does
    // not know which CLI will run and binds nothing.
    assert_eq!(Harness::from_admitted_runtime(None), None);
    for unwired in ["codex", "opencode", "", "Claude Code"] {
        assert_eq!(Harness::from_admitted_runtime(Some(unwired)), None, "{unwired}");
    }
    assert_eq!(Harness::ClaudeCode.service_name(), "claude-code");
}

#[test]
fn a_receiver_address_off_loopback_is_refused() {
    assert!(Relay::new(SocketAddr::from(([0, 0, 0, 0], 4318)), "h", None).is_none());
    assert!(Relay::new(SocketAddr::from(([192, 0, 2, 10], 4318)), "h", None).is_none());
    assert!(Relay::new(SocketAddr::from(([127, 0, 0, 1], 4318)), "h", None).is_some());
}

#[test]
#[serial]
fn registrations_stop_at_the_session_ceiling() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    for index in 0..MAX_SESSIONS {
        let mut command = Command::new("/bin/true");
        relay
            .prepare(&mut command, root.path(), sweep(root.path(), 1, &format!("sw-{index}")))
            .unwrap()
            .until_execution_ends();
    }
    let mut command = Command::new("/bin/true");
    assert!(relay
        .prepare(&mut command, root.path(), sweep(root.path(), 1, "one-too-many"))
        .is_none());
    assert!(env_of(&command).is_empty(), "an unregistered launch must not be half-wired");
    assert_eq!(relay.live_sessions(), MAX_SESSIONS);
}

#[test]
#[serial]
fn the_forge_slug_is_bound_once_known_and_never_guessed_from_the_path() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    let _lease = relay.prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"));
    let token = token_of(&command);
    assert_eq!(relay.authorize(&token).unwrap().repo, None);
    relay.note_repo(root.path(), "example-owner/example-repo");
    assert_eq!(
        relay.authorize(&token).unwrap().repo.as_deref(),
        Some("example-owner/example-repo")
    );
}

#[test]
#[serial]
fn a_dispatched_sweep_is_bound_to_its_issue_and_sweep_until_that_sweep_ends() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    assert!(prepare_sweep_child_on(
        Some(&relay),
        &mut command,
        root.path(),
        Some("claude"),
        Some(7),
        "sw-1"
    ));
    let bound = relay.authorize(&token_of(&command)).unwrap();
    assert_eq!(bound.identity, sweep(root.path(), 7, "sw-1"));
    assert_eq!(bound.label(), "sw-1");
    // The call returned, and the session is still live: only the end of the
    // sweep's execution ends it.
    assert_eq!(relay.end_execution("sw-1"), 1);
    assert!(relay.authorize(&token_of(&command)).is_none());

    // A PR-set sweep has no issue; a runtime that is not wired gets nothing.
    let mut prs = Command::new("/bin/true");
    assert!(prepare_sweep_child_on(
        Some(&relay),
        &mut prs,
        root.path(),
        Some("claude"),
        None,
        "sw-2"
    ));
    assert_eq!(relay.authorize(&token_of(&prs)).unwrap().identity.issue, None);
    for runtime in [None, Some("codex"), Some("opencode")] {
        let mut command = Command::new("/bin/true");
        assert!(!prepare_sweep_child_on(
            Some(&relay),
            &mut command,
            root.path(),
            runtime,
            Some(7),
            "sw-3"
        ));
        assert!(env_of(&command).is_empty(), "{runtime:?}");
    }
}

#[test]
#[serial]
fn a_role_tick_is_bound_to_its_role_for_exactly_as_long_as_its_lease() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let relay = relay();
    let mut command = Command::new("/bin/true");
    let lease = prepare_role_child_on(
        Some(&relay),
        &mut command,
        root.path(),
        Some("claude"),
        "judge",
        Some("role-judge-fixture"),
    )
    .unwrap();
    let token = token_of(&command);
    let bound = relay.authorize(&token).unwrap();
    assert_eq!(bound.identity.kind, SessionKind::Role);
    assert_eq!(bound.identity.role.as_deref(), Some("judge"));
    assert_eq!(bound.identity.sweep_id.as_deref(), Some("role-judge-fixture"));
    assert_eq!(bound.identity.issue, None);
    // A sweep ending under the same id does not end a role tick.
    assert_eq!(relay.end_execution("role-judge-fixture"), 0);
    assert!(relay.authorize(&token).is_some());
    drop(lease);
    assert!(relay.authorize(&token).is_none());

    let mut unwired = Command::new("/bin/true");
    assert!(
        prepare_role_child_on(Some(&relay), &mut unwired, root.path(), None, "judge", None)
            .is_none()
    );
    assert!(env_of(&unwired).is_empty());
}

#[tokio::test]
#[serial]
async fn the_forge_slug_is_looked_up_off_the_launch_path_once_per_workspace() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let lookup: RepoLookup = {
        let calls = calls.clone();
        Arc::new(move |_root: String| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some("example-owner/example-repo".to_string())
            })
        })
    };
    let relay = Relay::with_lookup(
        SocketAddr::from(([127, 0, 0, 1], 43180)),
        "host-fixture",
        Some(tokio::runtime::Handle::current()),
        lookup,
    )
    .unwrap();
    let mut command = Command::new("/bin/true");
    // `prepare` is synchronous and returns before any lookup has run.
    let _lease = relay.prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let token = token_of(&command);
    for _ in 0..200 {
        if relay.authorize(&token).unwrap().repo.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        relay.authorize(&token).unwrap().repo.as_deref(),
        Some("example-owner/example-repo")
    );
    // A second session in the same workspace reuses the answer.
    let mut second = Command::new("/bin/true");
    let _other = relay.prepare(&mut second, root.path(), sweep(root.path(), 8, "sw-2"));
    assert!(relay.authorize(&token_of(&second)).unwrap().repo.is_some());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial]
async fn a_workspace_whose_slug_cannot_be_resolved_is_not_looked_up_on_every_request() {
    let _env = EnvGuard::clear();
    let root = workspace(OPTED_IN);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let lookup: RepoLookup = {
        let calls = calls.clone();
        Arc::new(move |_root: String| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            })
        })
    };
    let relay = Relay::with_lookup(
        SocketAddr::from(([127, 0, 0, 1], 43180)),
        "host-fixture",
        Some(tokio::runtime::Handle::current()),
        lookup,
    )
    .unwrap();
    let mut command = Command::new("/bin/true");
    let _lease = relay.prepare(&mut command, root.path(), sweep(root.path(), 7, "sw-1"));
    let token = token_of(&command);
    for _ in 0..20 {
        assert_eq!(relay.authorize(&token).unwrap().repo, None);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
