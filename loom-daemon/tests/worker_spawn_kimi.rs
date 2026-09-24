//! Native worker seam x Kimi Code CLI adapter (issue #8561): headless launch,
//! model-profile binding (config-alias and Moonshot-API-key routes) and the
//! honest fail-closed manifest, driven through the real `spawn-worker` path.
//!
//! A sibling of `worker_spawn.rs` rather than a section of it: that file sits
//! at the 1000-line file-size ratchet, and these tests would carry it over.
//! `fixture` / `worker` / `config` / `profile_check` below are therefore
//! deliberate copies of that file's helpers, the same pattern
//! `worker_spawn_api_keys.rs` already uses.
use std::{path::PathBuf, process::Command, sync::OnceLock};

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
        .env("LOOM_KIMI_BIN", fixture())
        // Deliberately a PIPE, not `Stdio::null()`. Kimi has no stdin
        // prompt-delivery route on the pinned CLI (#8506 finding), so the
        // adapter pins the child's stdin to `/dev/null` itself rather than
        // letting it inherit the dispatcher's. Handing `spawn-worker` a pipe
        // here makes that override observable: if the adapter ever stopped
        // setting it, the exec'd fixture would report `stdin_kind=pipe` and
        // `stdin_is_dev_null=false`, and
        // `kimi_pins_child_stdin_to_dev_null_rather_than_inheriting` fails.
        // `Command::output()` never writes to the pipe and drops the handle,
        // so nothing can block on it either way.
        .stdin(std::process::Stdio::piped());
    c
}

fn config(root: &std::path::Path, value: serde_json::Value) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), value.to_string()).unwrap();
}

fn profile_check(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["worker", "profile-check"])
        .args(args)
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "");
    c.output().unwrap()
}

fn kimi_alias_profile(d: &std::path::Path) {
    config(
        d,
        serde_json::json!({"runtimes":{"defaultModelProfile":"kimi-alias","modelProfiles":{"kimi-alias":{"model":"kimi-k2-thinking-alias","providers":{"kimi":"kimi-k2-thinking-alias"},"effort":"high","allowedEfforts":["low","medium","high","xhigh","max"]}}}}),
    );
}

/// No `allowedEfforts` restriction, unlike [`kimi_alias_profile`] — isolates
/// the harness-level [`KIMI_THINKING_EFFORTS`] check from `profiles::select`'s
/// own, separate `allowedEfforts` gate, which would otherwise reject an
/// unsupported effort first and never exercise the harness's own validation.
fn kimi_alias_profile_no_effort_gate(d: &std::path::Path) {
    config(
        d,
        serde_json::json!({"runtimes":{"defaultModelProfile":"kimi-alias-open","modelProfiles":{"kimi-alias-open":{"model":"kimi-k2-thinking-alias","providers":{"kimi":"kimi-k2-thinking-alias"}}}}}),
    );
}

fn kimi_api_key_profile(d: &std::path::Path) {
    config(
        d,
        serde_json::json!({"runtimes":{"defaultModelProfile":"kimi-moonshot","modelProfiles":{"kimi-moonshot":{"model":"kimi-k2-thinking","providers":{"kimi":"moonshot"},"credentialEnv":"LOOM_TEST_KIMI_KEY","credentialTargets":{"kimi":"KIMI_MODEL_API_KEY"},"providerOptions":{"kimi":{"providerType":"kimi","baseUrl":"https://api.moonshot.ai/v1"}},"effort":"high","allowedEfforts":["low","medium","high","xhigh","max"]}}}}),
    );
}

#[test]
fn kimi_config_alias_profile_passes_m_and_never_touches_env_family_vars() {
    let d = tempfile::tempdir().unwrap();
    kimi_alias_profile(d.path());
    let out = worker(d.path(), "kimi")
        .env("FIXTURE_PRINT_ENV", "KIMI_MODEL_NAME,KIMI_MODEL_API_KEY")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    for arg in [
        "-m",
        "kimi-k2-thinking-alias",
        "-p",
        "hello",
        "--output-format",
        "stream-json",
    ] {
        assert!(text.contains(&format!("arg={arg:?}")), "{text}");
    }
    // The env-family variables are not merely "not set to the alias" — they
    // are absent entirely, which the fixture renders as an empty value. The
    // trailing newline is what makes this an exact-emptiness assertion rather
    // than a prefix match that any value would also satisfy.
    assert!(text.contains("child_env KIMI_MODEL_NAME=\n"), "{text}");
    assert!(text.contains("child_env KIMI_MODEL_API_KEY=\n"), "{text}");
    // Kimi never gets the prompt on stdin: no stdin-delivery route exists on
    // the pinned CLI (#8506 finding), so the fixture's own always-drain read
    // sees nothing at all — unlike Pi/OpenCode, which deliver the whole
    // prompt there.
    assert!(text.contains("stdin_len=0"), "{text}");
    assert!(!text.contains("hello\nSTDIN"), "prompt must not reach stdin: {text}");
}

/// The prompt has no stdin route on Kimi, so the adapter pins the child's
/// stdin to `/dev/null` rather than leaving it inherited: a daemon-dispatched
/// launch must never hand the harness the dispatcher's own TTY or an unrelated
/// pipe on an fd it may still read. [`worker`] deliberately gives
/// `spawn-worker` a **pipe**, so the `/dev/null` the fixture reports can only
/// have come from the adapter.
#[test]
fn kimi_pins_child_stdin_to_dev_null_rather_than_inheriting() {
    let d = tempfile::tempdir().unwrap();
    kimi_alias_profile(d.path());
    let out = worker(d.path(), "kimi")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("stdin_kind=null"), "{text}");
    assert!(text.contains("stdin_is_dev_null=true"), "{text}");
}

#[test]
fn kimi_api_key_profile_uses_env_family_and_translates_provider_options() {
    let d = tempfile::tempdir().unwrap();
    kimi_api_key_profile(d.path());
    let log = d.path().join("worker.log");
    let out = worker(d.path(), "kimi")
        .env("LOOM_TEST_KIMI_KEY", "fake-moonshot-secret-key")
        .env(
            "FIXTURE_PRINT_ENV",
            "KIMI_MODEL_NAME,KIMI_MODEL_API_KEY,KIMI_MODEL_PROVIDER_TYPE,KIMI_MODEL_BASE_URL",
        )
        .args(["--log", log.to_str().unwrap(), "-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains("arg=\"-m\""), "{text} (env-family route never passes -m)");
    assert!(text.contains("child_env KIMI_MODEL_NAME=kimi-k2-thinking"), "{text}");
    assert!(text.contains("child_env KIMI_MODEL_API_KEY=fake-moonshot-secret-key"), "{text}");
    assert!(text.contains("child_env KIMI_MODEL_PROVIDER_TYPE=kimi"), "{text}");
    assert!(
        text.contains("child_env KIMI_MODEL_BASE_URL=https://api.moonshot.ai/v1"),
        "{text}"
    );
    // The launch record and the child's own stdout never leak the value.
    let record = text.lines().find(|l| l.contains("LOOM_LAUNCH")).unwrap();
    assert!(!record.contains("fake-moonshot-secret-key"), "{record}");
    assert!(record.contains("\"runtime\":\"kimi\""), "{record}");
    assert!(record.contains("\"provider\":\"moonshot\""), "{record}");
    assert!(record.contains("\"model\":\"kimi-k2-thinking\""), "{record}");
    assert!(record.contains("\"profile\":\"kimi-moonshot\""), "{record}");
    assert!(record.contains("\"effort\":\"high\""), "{record}");
}

#[test]
fn kimi_thinking_effort_is_validated_and_hygiene_env_is_always_set() {
    let d = tempfile::tempdir().unwrap();
    kimi_alias_profile(d.path());
    // Valid effort rides on KIMI_MODEL_THINKING_EFFORT, plus the two
    // unconditional hygiene variables (same reasoning as
    // OPENCODE_DISABLE_AUTOUPDATE in docker/native/Dockerfile).
    let out = worker(d.path(), "kimi")
        .env(
            "FIXTURE_PRINT_ENV",
            "KIMI_MODEL_THINKING_EFFORT,KIMI_DISABLE_TELEMETRY,KIMI_CODE_NO_AUTO_UPDATE",
        )
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("child_env KIMI_MODEL_THINKING_EFFORT=high"), "{text}");
    assert!(text.contains("child_env KIMI_DISABLE_TELEMETRY=1"), "{text}");
    assert!(text.contains("child_env KIMI_CODE_NO_AUTO_UPDATE=1"), "{text}");
    // Kimi's five documented levels: "off" is not one of them (unlike Pi).
    // A profile with no `allowedEfforts` restriction isolates the harness's
    // own check from `profiles::select`'s separate, more generic gate.
    kimi_alias_profile_no_effort_gate(d.path());
    let out = worker(d.path(), "kimi")
        .args(["--effort", "off", "-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("thinking effort"), "{stderr}");
}

#[test]
fn kimi_skip_permissions_is_a_silent_no_op_and_never_emits_yolo_or_auto() {
    let d = tempfile::tempdir().unwrap();
    kimi_alias_profile(d.path());
    let out = worker(d.path(), "kimi")
        .args(["--dangerously-skip-permissions", "-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("--yolo"), "{text}");
    assert!(!text.contains("--auto"), "{text}");
}

#[test]
fn kimi_missing_binary_is_127_and_unknown_worker_option_is_78() {
    let d = tempfile::tempdir().unwrap();
    kimi_alias_profile(d.path());
    let out = worker(d.path(), "kimi")
        .env("LOOM_KIMI_BIN", d.path().join("missing"))
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(out.stdout.is_empty());
    let out = worker(d.path(), "kimi")
        .args(["--api-key", "never-print-this"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("never-print-this"));
    assert!(out.stdout.is_empty());
}

#[test]
fn kimi_role_tagged_launch_fails_closed_naming_the_guarded_tools_follow_up() {
    // #8562: the guarded loom_* tool binding has no live canary receipt yet
    // (`KIMI_GUARD_VERIFIED = false`), so a role-tagged launch must be
    // refused rather than silently run with Kimi's own unguarded builtin
    // tools — even for Curator, which requires no capability the runtime
    // manifest could otherwise gate on. Uses the credentialEnv profile
    // (rather than [`kimi_alias_profile`]) so this reaches that refusal
    // instead of the earlier, separate "guarded launch needs credentialEnv"
    // check exercised by
    // `kimi_guarded_launch_without_credential_env_fails_closed_first`.
    let d = tempfile::tempdir().unwrap();
    kimi_api_key_profile(d.path());
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("curator.json"), "{}").unwrap();
    let out = worker(d.path(), "kimi")
        .env("LOOM_ROLE", "curator")
        .env("LOOM_TEST_KIMI_KEY", "fake-moonshot-secret-key")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "the harness must never have started");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("8562"), "{stderr}");
    assert!(stderr.contains("guarded"), "{stderr}");
    assert!(!stderr.contains("fake-moonshot-secret-key"), "{stderr}");
    // The unguarded free-form trial (no role tag) is unaffected.
    let out = worker(d.path(), "kimi")
        .env("LOOM_TEST_KIMI_KEY", "fake-moonshot-secret-key")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn kimi_guarded_launch_without_credential_env_fails_closed_first() {
    // A guarded launch relocates KIMI_CODE_HOME, so the config-alias route
    // (`-m <alias>`, resolved from the operator's own config.toml) cannot
    // resolve under it. That check runs — and refuses — before the launch
    // ever reaches the separate #8562 "no live canary receipt" refusal
    // inside `configure()`, so [`kimi_alias_profile`] (no `credentialEnv`)
    // must fail with THIS diagnostic, not the #8562 one.
    let d = tempfile::tempdir().unwrap();
    kimi_alias_profile(d.path());
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("curator.json"), "{}").unwrap();
    let out = worker(d.path(), "kimi")
        .env("LOOM_ROLE", "curator")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "the harness must never have started");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("credentialEnv"), "{stderr}");
    assert!(!stderr.contains("8562"), "{stderr} (must not be the configure()-level refusal)");
    // The unguarded free-form trial (no role tag) is unaffected: it never
    // calls `configure` and never reaches this check either.
    let out = worker(d.path(), "kimi")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn bundled_kimi_example_profiles_load() {
    let d = tempfile::tempdir().unwrap();
    for name in ["example-kimi-subscription", "example-kimi-moonshot-api"] {
        let out = profile_check(d.path(), &[name, "--runtime", "kimi"]);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(&format!("profile: {name}")),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
