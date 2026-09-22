//! Probe tests. Two properties carry the whole safety argument and are tested
//! against a real child process rather than by reading the builder code: the
//! child's environment cannot contain anything credential-shaped, and the argv
//! cannot contain anything that reaches a model.
#![cfg(unix)]
use super::*;
use crate::native_readiness::{AttemptReport, CacheOutcome, Mode, Stage, StageObservation};
use std::os::unix::fs::PermissionsExt;

/// Write an executable fake CLI with the given `/bin/sh` body.
fn fake_cli(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn state(parent: &Path) -> IsolatedState {
    IsolatedState::create(parent).unwrap()
}

fn probe(bin: PathBuf, seconds: u64, network: NetworkMode) -> Probe {
    Probe::new(bin, Duration::from_secs(seconds), network, &[]).unwrap()
}

/// Variables `/bin/sh` itself exports into `env`'s view, which are not
/// inherited from this process and so are not leaks.
const SHELL_ADDED: &[&str] = &["SHLVL", "_", "OLDPWD", "PS1", "PS2", "PS4", "IFS", "OPTIND"];

#[test]
fn allowlist_contains_no_token_that_could_reach_a_model() {
    for forbidden in [
        "run",
        "-p",
        "--prompt",
        "--model",
        "--agent",
        "--auto",
        "--variant",
        "--standalone",
        "--dangerously-skip-permissions",
    ] {
        assert!(
            !READINESS_ALLOWLIST.contains(&forbidden),
            "{forbidden} must never be executable by a provider-free probe"
        );
    }
    assert!(READINESS_ALLOWLIST.contains(&"--version"));
}

#[test]
fn readiness_argv_is_allowlisted_and_a_refusal_never_echoes_the_token() {
    assert_eq!(validate_readiness(&[]).unwrap(), vec!["--version".to_owned()]);
    assert_eq!(
        validate_readiness(&["debug".into(), "config".into()]).unwrap(),
        vec!["debug".to_owned(), "config".to_owned()]
    );
    // Every refusal produces a byte-identical message, whatever was rejected:
    // the message carries zero information about the input, which is the only
    // way a pasted secret in a mistyped flag cannot end up in a log.
    let messages: Vec<String> = ["run", "--model", "-p", "sk-live-SECRET-TOKEN-0001"]
        .iter()
        .map(|rejected| {
            validate_readiness(&[(*rejected).to_owned()])
                .expect_err(rejected)
                .to_string()
        })
        .collect();
    assert!(messages[0].contains("provider-free allowlist"), "{}", messages[0]);
    assert!(
        messages.iter().all(|m| *m == messages[0]),
        "a refusal must not vary with untrusted input: {messages:?}"
    );
    assert!(!messages[0].contains("sk-live-SECRET-TOKEN-0001"), "{}", messages[0]);
    // A single bad token poisons an otherwise-valid argv.
    assert!(validate_readiness(&["--version".into(), "run".into()]).is_err());
}

#[test]
fn credential_shaped_names_are_recognized_without_enumerating_providers() {
    for name in [
        "ZAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GITEA_TOKEN",
        "FORGE_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "LOOM_NATIVE_AUTH_FILE",
        "OPENCODE_SESSION",
        "npm_config_password",
        "MY_credential_file",
    ] {
        assert!(credential_shaped(name), "{name}");
    }
    for name in [
        "PATH",
        "HOME",
        "LANG",
        "XDG_CACHE_HOME",
        "CI",
        "NO_COLOR",
        "PWD",
    ] {
        assert!(!credential_shaped(name), "{name}");
    }
}

#[test]
fn the_child_environment_is_built_from_scratch_and_holds_no_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = fake_cli(tmp.path(), "fake-cli", "exec env");
    let state = state(tmp.path());
    let probe = probe(cli, 30, NetworkMode::Allowed);
    let command = probe.command(&state, &["--version".to_owned()]).unwrap();
    let output = crate::proc_exec::run_bounded(command, Duration::from_secs(30))
        .unwrap()
        .output()
        .unwrap();
    assert!(output.status.success());
    let dump = String::from_utf8_lossy(&output.stdout);
    let observed: Vec<(&str, &str)> = dump
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| !SHELL_ADDED.contains(key))
        .collect();
    assert!(!observed.is_empty(), "the fake CLI must have dumped an environment");

    // Nothing credential-shaped, whatever this process happens to hold.
    for (key, _) in &observed {
        assert!(!credential_shaped(key), "{key} reached the child");
    }

    // The key set is exactly what `command` sets. Any inherited variable —
    // CARGO_*, RUSTUP_*, LOOM_FORCE_SCOPE, SSH_AUTH_SOCK — would appear here,
    // so this is the assertion that proves `env_clear()` is in force.
    let mut keys: Vec<&str> = observed.iter().map(|(k, _)| *k).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "CI",
            "HOME",
            "LOOM_NATIVE_READINESS_PROBE",
            "NO_COLOR",
            "OPENCODE_CONFIG_DIR",
            "OPENCODE_DISABLE_AUTOUPDATE",
            "PATH",
            "PWD",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
        ]
    );

    // HOME is the isolated tree, not the operator's.
    let home = observed
        .iter()
        .find(|(k, _)| *k == "HOME")
        .map(|(_, v)| *v)
        .unwrap();
    assert_eq!(Path::new(home), state.home.as_path());
    assert_ne!(
        Some(Path::new(home)),
        dirs::home_dir().as_deref(),
        "the probe must not run against the operator's own home"
    );
}

#[test]
fn network_denial_points_every_proxy_at_a_closed_loopback_port() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = fake_cli(tmp.path(), "fake-cli", "exec env");
    let state = state(tmp.path());
    let probe = probe(cli, 30, NetworkMode::DeniedByEnv);
    let command = probe.command(&state, &["--version".to_owned()]).unwrap();
    let output = crate::proc_exec::run_bounded(command, Duration::from_secs(30))
        .unwrap()
        .output()
        .unwrap();
    let dump = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "https_proxy=http://127.0.0.1:1",
        "HTTPS_PROXY=http://127.0.0.1:1",
        "npm_config_registry=http://127.0.0.1:1/",
        "npm_config_offline=true",
        "no_proxy=",
    ] {
        assert!(dump.lines().any(|l| l == expected), "missing {expected}");
    }
}

#[test]
fn a_timeout_reports_the_stage_and_byte_counts_but_no_child_output() {
    let tmp = tempfile::tempdir().unwrap();
    // The child prints a secret-shaped string, then hangs past the deadline.
    let secret = "sk-live-CANARY-3f9a2b";
    let cli = fake_cli(tmp.path(), "hang", &format!("printf '%s\\n' 'token={secret}'\nsleep 30"));
    let state = state(tmp.path());
    let probe = probe(cli, 1, NetworkMode::Allowed);
    let observation = probe.readiness(&state);
    let Observation::Failed {
        classification,
        stdout_bytes,
        ..
    } = &observation
    else {
        panic!("expected a timeout, got {observation:?}");
    };
    assert_eq!(*classification, Classification::Timeout);
    assert!(*stdout_bytes > 0, "the bytes were counted");

    // The observation, and any report built from it, carry no child content.
    let attempt = AttemptReport {
        index: 0,
        mode: Mode::Cold,
        cache: CacheOutcome::Bypassed,
        total_millis: 1000,
        stages: vec![StageObservation {
            stage: Stage::ServerSessionReady,
            observation,
        }],
    };
    let serialized = serde_json::to_string(&attempt).unwrap();
    assert!(!serialized.contains(secret), "{serialized}");
    assert!(!serialized.contains("token="), "{serialized}");
    assert!(serialized.contains("server_session_ready"), "the stage survives");
    assert!(serialized.contains("timeout"));
}

#[test]
fn a_timed_out_probe_leaves_no_descendant_running() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("ticks");
    // A grandchild that keeps appending: if the deadline only killed the
    // immediate child, this file would keep growing after the probe returns.
    let cli = fake_cli(
        tmp.path(),
        "forker",
        &format!(
            "( while true; do printf 'x' >> '{}'; sleep 0.05; done ) &\nsleep 30",
            marker.display()
        ),
    );
    let state = state(tmp.path());
    let probe = probe(cli, 1, NetworkMode::Allowed);
    let observation = probe.readiness(&state);
    assert!(matches!(
        observation,
        Observation::Failed {
            classification: Classification::Timeout,
            ..
        }
    ));
    let settled = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert!(settled > 0, "the grandchild must have run before the deadline");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0),
        settled,
        "the whole process group must be gone once the deadline fires"
    );
}

#[test]
fn version_outcomes_are_classified_without_guessing() {
    let tmp = tempfile::tempdir().unwrap();
    let state = state(tmp.path());

    let good = probe(fake_cli(tmp.path(), "good", "echo 1.18.31"), 30, NetworkMode::Allowed);
    let (observation, line) = good.version(&state);
    assert!(matches!(observation, Observation::Measured { .. }));
    assert_eq!(line.as_deref(), Some("1.18.31"));

    // Exits 0 but says nothing parseable -> not a measured boundary.
    let mute = probe(fake_cli(tmp.path(), "mute", "exit 0"), 30, NetworkMode::Allowed);
    let (observation, line) = mute.version(&state);
    assert!(matches!(
        observation,
        Observation::Failed {
            classification: Classification::UnparsableVersion,
            ..
        }
    ));
    assert_eq!(line, None);

    let broken = probe(fake_cli(tmp.path(), "broken", "exit 3"), 30, NetworkMode::Allowed);
    assert!(matches!(
        broken.version(&state).0,
        Observation::Failed {
            classification: Classification::NonzeroExit,
            ..
        }
    ));

    let missing = probe(tmp.path().join("nope"), 30, NetworkMode::Allowed);
    assert!(matches!(
        missing.version(&state).0,
        Observation::Failed {
            classification: Classification::SpawnFailed,
            ..
        }
    ));
}

#[test]
fn package_resolution_runs_in_the_package_root_with_no_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let state = state(tmp.path());
    let recorder = fake_cli(tmp.path(), "fake-npm", "pwd\nexec env");
    let probe = probe(fake_cli(tmp.path(), "cli", "echo 1.18.31"), 30, NetworkMode::DeniedByEnv)
        .with_package_manager(recorder.clone());
    assert_eq!(probe.package_manager(), recorder.as_path());
    let observation = probe.resolve_packages(&state);
    assert!(matches!(observation, Observation::Measured { .. }), "{observation:?}");
    // Re-run capturing output to inspect the child's view.
    let mut command = std::process::Command::new(&recorder);
    command.current_dir(state.package_root());
    let output = command.output().unwrap();
    let cwd = String::from_utf8_lossy(&output.stdout);
    assert!(
        cwd.lines()
            .next()
            .is_some_and(|l| Path::new(l.trim()) == state.package_root().canonicalize().unwrap()),
        "{cwd}"
    );
}

#[test]
fn isolated_state_is_private_and_provisions_the_production_bindings() {
    let tmp = tempfile::tempdir().unwrap();
    let state = state(tmp.path());
    for path in [&state.root, &state.home, &state.config_dir] {
        let mode = std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{} must be private", path.display());
    }
    assert!(state.root.starts_with(tmp.path()));
    // Two attempts never share a state tree.
    assert_ne!(state.root, IsolatedState::create(tmp.path()).unwrap().root);

    assert!(matches!(provision_bindings(&state), Observation::Measured { .. }));
    let manifest = manifest_bytes(&state).unwrap();
    assert_eq!(
        String::from_utf8(manifest).unwrap(),
        crate::native_tools::provision::OPENCODE_PLUGIN_MANIFEST,
        "the measured manifest must be byte-identical to what a launch provisions"
    );
    assert!(state.config_dir.join("plugins/loom.ts").is_file());
}
