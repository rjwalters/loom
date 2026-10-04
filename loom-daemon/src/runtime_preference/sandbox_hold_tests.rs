//! The sandbox hold drives real fall-through (#10003): with
//! `rolePreference: ["codex","claude"]`, a healthy Codex seat AND a healthy
//! Claude pool, the walk chooses Codex until a no-op arms the hold, then
//! Claude until the hold ages out or is cleared.
use super::*;
use std::fs;

/// The second field only keeps the profile root alive for the guard's scope;
/// the third holds the crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (#9964).
struct Env(
    Vec<(&'static str, Option<String>)>,
    #[allow(dead_code)] tempfile::TempDir,
    #[allow(dead_code)] crate::tokens_pool::profile_root_env::ProfileRootLock,
);
impl Env {
    fn new() -> Self {
        let lock = crate::tokens_pool::profile_root_env::lock();
        let keys = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_CURATOR",
            "LOOM_CODEX_PROFILE_ROOT",
            "LOOM_CODEX_PROFILE",
            "LOOM_CODEX_HOME",
            "CODEX_HOME",
            "LOOM_SPAWN_NO_EXPORT",
            "LOOM_CODEX_NO_EXEC",
            sandbox_hold::HOLD_SECS_ENV,
            ceiling::MAX_CONCURRENT_ENV,
        ];
        let prior = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for key in keys {
            std::env::remove_var(key);
        }
        let profiles = tempfile::tempdir().unwrap();
        fs::create_dir_all(profiles.path().join("seat")).unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        sandbox_hold::clear("codex");
        Self(prior, profiles, lock)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        sandbox_hold::clear("codex");
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn workspace() -> tempfile::TempDir {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (from, to) in [
        ("defaults/roles", ".loom/roles"),
        ("defaults/runtimes", ".loom/runtimes"),
    ] {
        fs::create_dir_all(dir.path().join(to)).unwrap();
        for entry in fs::read_dir(repo.join(from)).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                fs::copy(&path, dir.path().join(to).join(path.file_name().unwrap())).unwrap();
            }
        }
    }
    fs::create_dir_all(dir.path().join(".loom/scripts")).unwrap();
    for runtime in ["claude", "codex"] {
        let adapter = dir.path().join(format!(".loom/scripts/spawn-{runtime}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(adapter, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let tokens = dir.path().join(".loom/tokens");
    fs::create_dir_all(&tokens).unwrap();
    fs::write(tokens.join("account0.token"), "sk-ant-oat-fake\n").unwrap();
    let config =
        serde_json::json!({"runtimes": {"rolePreference": {"curator": ["codex", "claude"]}}});
    fs::write(dir.path().join(".loom/config.json"), config.to_string()).unwrap();
    dir
}

fn chosen(root: &Path, now: u64) -> (String, Vec<String>) {
    let Ok(Decision::Preference { resolution, .. }) = resolve_runtime(root, "curator", None, now)
    else {
        panic!("expected the preference path");
    };
    let skipped = resolution
        .skipped
        .iter()
        .map(|s| format!("{}: {}", s.tap.runtime, s.reason.summary()))
        .collect();
    (resolution.chosen.unwrap().admitted.runtime, skipped)
}

#[test]
#[serial_test::serial]
fn a_sandbox_no_op_routes_the_next_tick_to_the_next_tap_until_the_hold_ends() {
    let _env = Env::new();
    let dir = workspace();

    let (runtime, skipped) = chosen(dir.path(), 1_000);
    assert_eq!(runtime, "codex", "a healthy codex seat is preferred: {skipped:?}");

    std::env::set_var(sandbox_hold::HOLD_SECS_ENV, "600");
    sandbox_hold::arm("codex", "shape=exec-denied execs=1 denied=1 succeeded=0", 1_000);

    let (runtime, skipped) = chosen(dir.path(), 1_001);
    assert_eq!(runtime, "claude", "a live sandbox hold falls through");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(skipped[0].starts_with("codex: "), "{skipped:?}");
    assert!(skipped[0].contains("sandbox unavailable"), "{skipped:?}");
    assert!(skipped[0].contains("shape=exec-denied"), "{skipped:?}");

    // Ages out on its own: the next tick re-tests codex.
    let (runtime, _) = chosen(dir.path(), 1_600);
    assert_eq!(runtime, "codex", "the hold self-heals at its deadline");

    // A tick that ran a command clears a live hold at once.
    sandbox_hold::arm("codex", "x", 2_000);
    assert_eq!(chosen(dir.path(), 2_001).0, "claude");
    sandbox_hold::clear("codex");
    assert_eq!(chosen(dir.path(), 2_002).0, "codex");
}

#[test]
#[serial_test::serial]
fn an_operator_pin_is_never_routed_around_by_the_hold() {
    let _env = Env::new();
    let dir = workspace();
    std::env::set_var(sandbox_hold::HOLD_SECS_ENV, "600");
    sandbox_hold::arm("codex", "x", 1_000);
    std::env::set_var("LOOM_RUNTIME_CURATOR", "codex");
    let decision = resolve_runtime(dir.path(), "curator", None, 1_001).unwrap();
    std::env::remove_var("LOOM_RUNTIME_CURATOR");
    let Decision::Static { result, .. } = decision else {
        panic!("a pin takes the static path, which the hold never reads");
    };
    assert_eq!(result.unwrap().runtime, "codex");
}
