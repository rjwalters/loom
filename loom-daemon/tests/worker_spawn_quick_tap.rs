//! Bundled quick-tap model profiles (`quick-cerebras`, `quick-flash`, issue
//! #8711) — the zero-config half of the #8436 metered-backstop pattern,
//! driven through the real `spawn-worker` / `worker profile-check` paths.
//!
//! A sibling of `worker_spawn.rs` rather than a section of it, for the same
//! reason `worker_spawn_api_keys.rs` and `worker_spawn_preference_marker.rs`
//! are: that file sits at the 1000-line file-size ratchet and these tests
//! would carry it over. `worker` and `profile_check` below are deliberate
//! copies of its own.
use std::process::Command;

#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;

fn worker(root: &std::path::Path, runtime: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["spawn-worker", "--"])
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_RUNTIME", runtime)
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        // Never let a test reach the operator's real `~/.loom/api-keys` (#8401).
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_NATIVE_TOOLS_DIR", fixture().parent().unwrap().join("state"))
        .env_remove("LOOM_NATIVE_AUTH_FILE")
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        // The OpenCode adapter probes `--version` before exec (#8438).
        .env("FIXTURE_VERSION", "1.18.31")
        .env_remove("FIXTURE_VERSION_EXIT");
    c
}

fn profile_check(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["worker", "profile-check"])
        .args(args)
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        // Never let a pool report depend on the operator's real
        // `~/.loom/api-keys` (#8401): this binary is not `cfg(test)`, so the
        // in-crate refusal does not apply to it.
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env_remove("AWS_PROFILE")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .env_remove("GOOGLE_CLOUD_PROJECT")
        .env_remove("VERTEX_LOCATION")
        .env_remove("CEREBRAS_API_KEY")
        .env_remove("GEMINI_API_KEY");
    c.output().unwrap()
}

/// Issue #8711: the bundled quick-tap presets are the zero-config half of the
/// #8436 metered-backstop pattern, so on a host that has registered NOTHING
/// they must (a) resolve — the string `credentialEnv` form stays lenient, the
/// harness's own auth store may supply the key — and (b) name the pool
/// namespace `api-keys add` takes, which is the one fact an operator needs
/// next and which a bare `unset` never carried.
#[test]
fn bundled_quick_tap_presets_resolve_and_name_their_pool_with_nothing_registered() {
    let d = tempfile::tempdir().unwrap();
    for (name, provider, model, namespace) in [
        ("quick-cerebras", "cerebras", "gpt-oss-120b", "cerebras"),
        ("quick-flash", "google", "gemini-3.5-flash", "gemini"),
    ] {
        let out = profile_check(d.path(), &[name, "--runtime", "pi"]);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{name}: {text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(text.contains(&format!("profile: {name}")), "{text}");
        assert!(text.contains(&format!("model: {model}")), "{text}");
        assert!(text.contains(&format!("harness pi: provider {provider}")), "{text}");
        assert!(text.contains(&format!("pool {namespace}: no accounts registered")), "{text}");
        assert!(text.contains("status: resolvable"), "{text}");
        // Data only: a preset can never carry key material into the repo.
        assert!(!text.contains("credentialProxy"), "{text}");
    }
    // A registered-but-unusable pool still refuses, exactly as before — the
    // unregistered report above is the only new non-refusing state.
    let mut add = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    let key = d.path().join("fake-key");
    std::fs::write(&key, "fake-cerebras-secret\n").unwrap();
    let out = add
        .args(["api-keys", "add", "cerebras", "alpha", "--key-file"])
        .arg(&key)
        .current_dir(d.path())
        .env("LOOM_WORKSPACE", d.path())
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text =
        String::from_utf8_lossy(&profile_check(d.path(), &["quick-cerebras"]).stdout).into_owned();
    assert!(text.contains("pool cerebras: 1/1 selectable"), "{text}");
    assert!(!text.contains("fake-cerebras-secret"), "{text}");
}

/// Issue #8711 AC4: adding bundled presets is a no-op for a repo that names
/// none of them. The unconfigured default is still `zai-flash`, on both the
/// report surface and a real launch.
#[test]
fn bundled_presets_do_not_change_the_unconfigured_default() {
    let d = tempfile::tempdir().unwrap();
    let text = String::from_utf8_lossy(&profile_check(d.path(), &[]).stdout).into_owned();
    assert!(text.contains("profile: zai-flash"), "{text}");
    let out = worker(d.path(), "pi")
        .env_remove("ZAI_API_KEY")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let launched = String::from_utf8_lossy(&out.stdout);
    assert!(launched.contains("glm-5.3-flash"), "{launched}");
    assert!(!launched.contains("gpt-oss-120b"), "{launched}");
    assert!(!launched.contains("gemini"), "{launched}");
}
