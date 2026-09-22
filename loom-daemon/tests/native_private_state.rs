//! Guarded fake-harness launches exercise real provisioning without provider calls.
#![allow(clippy::unwrap_used)]
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};
fn fixture() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let bin = dir.join("harness");
        assert!(Command::new("rustc")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/worker_cli.rs"))
            .arg("-o")
            .arg(&bin)
            .status()
            .unwrap()
            .success());
        bin
    })
}
fn worker(root: &Path, base: &Path, runtime: &str) -> Command {
    let roles = root.join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(root.join(".git"), "fixture checkout").unwrap();
    std::fs::write(roles.join("builder.json"), "{}").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    command.args(["spawn-worker", "--", "-p", "inspect fixture"])
        .current_dir(root).env("LOOM_WORKSPACE", root).env("LOOM_ROLE", "builder")
        .env("LOOM_RUNTIME", runtime).env("LOOM_NATIVE_TOOLS_DIR", base)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "").env("LOOM_SHARED_API_KEYS_DIR", "")
        .env("LOOM_PI_BIN", fixture()).env("LOOM_OPENCODE_BIN", fixture())
        .env("FIXTURE_VERSION", "1.18.31")
        .env("LOOM_NATIVE_GUARD_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"))
        .env("FIXTURE_WRITE_NATIVE_STATE", "1")
        .env("FIXTURE_PRINT_ENV", "PI_CODING_AGENT_DIR,PI_CODING_AGENT_SESSION_DIR,OPENCODE_CONFIG_DIR,XDG_DATA_HOME,XDG_STATE_HOME,XDG_CACHE_HOME");
    for key in [
        "LOOM_MODEL",
        "LOOM_MODEL_PROFILE",
        "LOOM_NATIVE_AUTH_FILE",
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "OPENCODE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        command.env_remove(key);
    }
    command
}
fn env_path(output: &str, key: &str) -> PathBuf {
    PathBuf::from(
        output
            .lines()
            .find_map(|line| line.strip_prefix(&format!("child_env {key}=")))
            .unwrap(),
    )
}
#[test]
fn guarded_harnesses_write_auth_and_sessions_only_to_isolated_external_directories() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let base = temporary.path().join("external");
    for runtime in ["pi", "opencode"] {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let output = worker(&root, &base, runtime).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let text = String::from_utf8_lossy(&output.stdout);
            let directory = env_path(
                &text,
                if runtime == "pi" {
                    "PI_CODING_AGENT_DIR"
                } else {
                    "XDG_DATA_HOME"
                },
            );
            assert!(directory.starts_with(base.canonicalize().unwrap()));
            assert!(!directory.starts_with(root.canonicalize().unwrap()));
            assert!(directory.join("fixture-auth.json").is_file());
            assert!(directory.join("fixture-session.json").is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            seen.push(directory);
        }
        assert_ne!(seen[0], seen[1]);
    }
    assert!(!root.join(".loom/native-tools").exists());
}
#[test]
fn unsafe_overrides_refuse_before_harness_start_or_secret_writes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let safe = temporary.path().join("safe");
    for runtime in ["pi", "opencode"] {
        for key in [
            "LOOM_NATIVE_TOOLS_DIR",
            "PI_CODING_AGENT_DIR",
            "XDG_DATA_HOME",
        ] {
            let bad = root.join("ignored-secret-state");
            let output = worker(&root, &safe, runtime)
                .env(key, &bad)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(78),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stdout.is_empty(), "the harness must never start");
            assert!(!bad.exists());
            assert!(!safe.exists());
        }
    }
}

#[test]
fn default_state_base_follows_external_home_for_both_harnesses() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let home = temporary.path().join("home");
    std::fs::create_dir(&home).unwrap();
    for runtime in ["pi", "opencode"] {
        let output = worker(&root, &temporary.path().join("unused"), runtime)
            .env_remove("LOOM_NATIVE_TOOLS_DIR")
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8_lossy(&output.stdout);
        let path = env_path(
            &text,
            if runtime == "pi" {
                "PI_CODING_AGENT_DIR"
            } else {
                "XDG_DATA_HOME"
            },
        );
        assert!(path.starts_with(
            home.canonicalize()
                .unwrap()
                .join(".local/state/loom/native-tools")
        ));
        assert!(path.join("fixture-auth.json").is_file());
    }
    assert!(!root.join(".loom/native-tools").exists());
}
