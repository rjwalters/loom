//! Native worker seam x API-key account pool (issue #8401): explicit env >
//! pool > fail-closed 78, driven through the real `spawn-worker` path.
//!
//! A sibling of `worker_spawn.rs` rather than a section of it: that file sits
//! just under the 1000-line file-size ratchet, and these tests would carry it
//! over. The three helpers below are therefore deliberate copies of that file's
//! `fixture` / `worker` / `config`. Keep `worker` in step with it — in
//! particular the `FIXTURE_VERSION` line, without which the OpenCode adapter's
//! pre-exec `--version` probe (#8438) refuses the fixture.
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
        // Never let a test reach the operator's real `~/.loom/api-keys`. Tests
        // that exercise the shared pool override this with a tempdir.
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        .env("FIXTURE_VERSION", "1.18.31")
        .env_remove("FIXTURE_VERSION_EXIT");
    c
}

fn config(root: &std::path::Path, value: serde_json::Value) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), value.to_string()).unwrap();
}

const POOL_PROVIDER: &str = "loomtest";
const ALPHA_KEY: &str = "fake-alpha-key-must-never-be-logged";
const BETA_KEY: &str = "fake-beta-key-must-never-be-logged";

/// A profile whose credential is pool-managed. `credentialEnv` names a
/// variable no real environment sets, so the pool is what decides.
fn pooled_profile(root: &std::path::Path) {
    config(
        root,
        serde_json::json!({"runtimes":{"defaultModelProfile":"pooled","modelProfiles":{"pooled":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":POOL_PROVIDER,
            "credentialTargets":{"pi":"LOOM_TEST_HARNESS_SECRET","opencode":"LOOM_TEST_HARNESS_SECRET"}
        }}}}),
    );
}

fn register(root: &std::path::Path, name: &str, key: &str) {
    let dir = root.join(".loom/api-keys").join(POOL_PROVIDER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.env")), format!("LOOM_TEST_PROVIDER_SECRET={key}\n"))
        .unwrap();
}

/// Pin the round-robin cursor so the alternation assertion is deterministic.
fn seed_cursor(root: &std::path::Path) {
    std::fs::write(
        root.join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".rotation_cursor"),
        "0",
    )
    .unwrap();
}

fn launch_field(stderr: &str, field: &str) -> String {
    let line = stderr
        .lines()
        .find(|l| l.starts_with("# LOOM_LAUNCH "))
        .unwrap_or_else(|| panic!("no LOOM_LAUNCH record in: {stderr}"));
    let record: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("# LOOM_LAUNCH ")).unwrap();
    record[field].as_str().unwrap_or("null").to_string()
}

#[test]
fn pooled_credential_alternates_accounts_and_names_only_the_account() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    register(d.path(), "beta", BETA_KEY);
    seed_cursor(d.path());

    let mut seen = Vec::new();
    for _ in 0..4 {
        let log = d.path().join("worker.log");
        let out = worker(d.path(), "opencode")
            .args(["--log", log.to_str().unwrap(), "-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = std::fs::read_to_string(&log).unwrap();
        seen.push(launch_field(&text, "credentialAccount"));
        assert_eq!(launch_field(&text, "credentialSource"), "pool");
        assert_eq!(launch_field(&text, "credentialProvider"), POOL_PROVIDER);
        // The launch record, the log, and the child's own stdout must all be
        // free of key material — the `strings`-over-every-retained-log criterion.
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        for haystack in [&text, &stdout] {
            for key in [ALPHA_KEY, BETA_KEY] {
                assert!(!haystack.contains(key), "key material leaked: {haystack}");
            }
        }
        std::fs::remove_file(&log).unwrap();
    }
    assert_eq!(
        seen,
        vec!["alpha", "beta", "alpha", "beta"],
        "consecutive spawns must alternate"
    );
}

#[test]
fn pooled_credential_reaches_the_child_under_the_harness_variable() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    let out = worker(d.path(), "pi")
        .env("LOOM_TEST_EXPECTED_SECRET", ALPHA_KEY)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("credential_pool_matches=true"), "{text}");
}

#[test]
fn an_explicit_credential_env_still_wins_over_the_pool() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    let out = worker(d.path(), "opencode")
        .env("LOOM_TEST_PROVIDER_SECRET", "fake-explicit-secret")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("credential_alias_matches=true"), "{stdout}");
    assert_eq!(launch_field(&stderr, "credentialSource"), "env");
    assert_eq!(launch_field(&stderr, "credentialAccount"), "null");
    assert!(!stderr.contains("fake-explicit-secret") && !stdout.contains("fake-explicit-secret"));
}

#[test]
fn an_all_disabled_pool_fails_closed_at_78_without_key_material() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    register(d.path(), "beta", BETA_KEY);
    std::fs::write(
        d.path()
            .join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".disabled"),
        "alpha\nbeta\n",
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no usable API-key account"), "{stderr}");
    assert!(stderr.contains("disabled by operator"), "{stderr}");
    assert!(stderr.contains("deciding binary:"), "{stderr}");
    for key in [ALPHA_KEY, BETA_KEY] {
        assert!(!stderr.contains(key), "{stderr}");
    }
    assert!(out.stdout.is_empty());
}

#[test]
fn a_host_with_no_pool_keeps_the_pre_pool_behaviour() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    // No accounts registered: the harness is left to its own auth store, and
    // the spawn is NOT failed closed (#8363's trial path).
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(launch_field(&String::from_utf8_lossy(&out.stderr), "credentialSource"), "none");
}

#[test]
fn a_malformed_credential_pool_name_is_rejected_before_launch() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"bad","modelProfiles":{"bad":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":"../escape",
            "credentialTargets":{"pi":"LOOM_TEST_HARNESS_SECRET"}
        }}}}),
    );
    let out = worker(d.path(), "pi")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// Regression tests for the PR #8428 review: the two fail-open paths, on the
// real spawn path, with the controls the Judge used.
// ---------------------------------------------------------------------------

/// The verdict's third table row. Same all-disabled pool as the test above,
/// with the provider directory made unreadable: before the fix this exited 0
/// with `credentialSource:"none"` and the harness launched on its own auth
/// store.
#[cfg(unix)]
#[test]
fn an_unreadable_pool_fails_closed_at_78_instead_of_launching() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    let dir = d.path().join(".loom/api-keys").join(POOL_PROVIDER);
    std::fs::write(dir.join(".disabled"), "alpha\n").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let as_root = std::fs::read_dir(&dir).is_ok();
    let out = worker(d.path(), "opencode")
        .env("LOOM_TEST_EXPECTED_SECRET", ALPHA_KEY)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    // Restore before asserting so the tempdir can always be cleaned up.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if as_root {
        return; // permission bits do not apply
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(out.stdout.is_empty(), "the harness must not have launched");
    assert!(!stderr.contains("# LOOM_LAUNCH"), "{stderr}");
    assert!(stderr.contains(&dir.display().to_string()), "must name the path: {stderr}");
    assert!(stderr.contains("PermissionDenied"), "must name the error: {stderr}");
    assert!(!stderr.contains(ALPHA_KEY), "{stderr}");
}

/// A torn / zero-length `.bad_accounts.json` must not return an exhausted
/// account to selection.
#[test]
fn a_torn_bad_marks_file_fails_closed_at_78() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    std::fs::write(
        d.path()
            .join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".bad_accounts.json"),
        "",
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("not a valid bad-marks file"), "{stderr}");
    assert!(out.stdout.is_empty());
}

/// Per-provider root resolution: a per-repo pool for some *other* provider
/// must not hide the shared machine-level pool for this profile's provider.
/// Before the fix this spawn ran with `credentialSource:"none"`.
#[test]
fn a_per_repo_pool_for_another_provider_does_not_hide_the_shared_pool() {
    let d = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    let other = d.path().join(".loom/api-keys/othertest");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("local.env"), "OTHERTEST_API_KEY=fake-other-key\n").unwrap();
    let pooled = shared.path().join(POOL_PROVIDER);
    std::fs::create_dir_all(&pooled).unwrap();
    std::fs::write(
        pooled.join("shared-one.env"),
        format!("LOOM_TEST_PROVIDER_SECRET={BETA_KEY}\n"),
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .env("LOOM_SHARED_API_KEYS_DIR", shared.path())
        .env("LOOM_TEST_EXPECTED_SECRET", BETA_KEY)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(launch_field(&stderr, "credentialSource"), "pool");
    assert_eq!(launch_field(&stderr, "credentialAccount"), "shared-one");
    assert!(String::from_utf8_lossy(&out.stdout).contains("credential_pool_matches=true"));
}

/// An account file that assigns a different variable than the profile's
/// `credentialEnv` is never injected under the profile's harness variable.
#[test]
fn an_account_for_the_wrong_variable_is_not_injected() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    let dir = d.path().join(".loom/api-keys").join(POOL_PROVIDER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stray.env"), format!("SOME_OTHER_API_KEY={ALPHA_KEY}\n")).unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("assigns SOME_OTHER_API_KEY"), "{stderr}");
    assert!(!stderr.contains(ALPHA_KEY), "{stderr}");
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// Regression tests for the second PR #8428 review round: a profile with no
// `credentialTargets` entry for the launching harness previously never
// consulted the pool at all (`profiles::credentials` returned an empty pair
// list, so `credential::resolve` took the zero-pairs branch and returned
// `Source::None` before `pool_provider` / `is_pooled` ever ran).
// ---------------------------------------------------------------------------

/// A profile with `credentialPool` set but no `credentialTargets` entry for
/// this harness at all. Before the fix this exited 0 with
/// `credentialSource:"none"` even though every account was disabled -- the
/// exact "operator disabled every account to stop spend" failure the
/// all-disabled-pool test above already covers for a *targeted* profile.
#[test]
fn a_profile_with_no_credential_target_for_the_harness_still_consults_the_pool() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"untargeted","modelProfiles":{"untargeted":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":POOL_PROVIDER
        }}}}),
    );
    register(d.path(), "alpha", ALPHA_KEY);
    std::fs::write(
        d.path()
            .join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".disabled"),
        "alpha\n",
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("no usable API-key account"), "{stderr}");
    assert!(out.stdout.is_empty(), "the harness must not have launched");
}

/// Same untargeted profile, but with a usable account: the pooled value must
/// reach the child under the source variable's own name -- the implicit
/// target inheritance would have used had the value been exported instead.
#[test]
fn a_profile_with_no_credential_target_injects_the_pooled_value_under_the_source_name() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"untargeted","modelProfiles":{"untargeted":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":POOL_PROVIDER
        }}}}),
    );
    register(d.path(), "alpha", ALPHA_KEY);
    let out = worker(d.path(), "opencode")
        .env("FIXTURE_PRINT_ENV", "LOOM_TEST_PROVIDER_SECRET")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(launch_field(&stderr, "credentialSource"), "pool");
    assert_eq!(launch_field(&stderr, "credentialAccount"), "alpha");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("child_env LOOM_TEST_PROVIDER_SECRET={ALPHA_KEY}")),
        "{stdout}"
    );
}

/// `credentialPool` on an array-form `credentialEnv` was silently accepted
/// and ignored -- the array form declares a required set, and one pooled
/// account (a single `KEY=value`) cannot satisfy more than one member of it.
#[test]
fn a_credential_pool_on_an_array_form_credential_env_is_rejected_before_launch() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"arr","modelProfiles":{"arr":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":["LOOM_TEST_PROVIDER_SECRET"],
            "credentialPool":POOL_PROVIDER
        }}}}),
    );
    let out = worker(d.path(), "pi")
        .env("LOOM_TEST_PROVIDER_SECRET", "fake-explicit-secret")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("single-string credentialEnv"), "{stderr}");
    assert!(out.stdout.is_empty());
}
