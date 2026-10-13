//! Coverage for the UNCONTAINED native-launch path (issue #11300, slice 3):
//! loopback-only substitution for a native harness on the host, the
//! OpenCode provider-config base-URL override, and the shapes that are
//! refused rather than half-proxied.

use super::*;
use serde_json::json;

use super::tests::{anthropic_proxy, env_lock, selection_with};

// --------------------- uncontained native launches (issue #11300, slice 3)

/// The `zai-flash` shape: a coding-plan endpoint one path below the origin,
/// no env base URL — OpenCode reads its provider base URL from config.
fn zai_flash_proxy() -> ProfileProxy {
    ProfileProxy {
        upstream: "https://api.z.ai/api/coding/paas/v4".into(),
        header: HeaderStyle::AuthorizationBearer,
        base_url_env: Vec::new(),
        observe: false,
        base_url_config: Some("baseURL".into()),
    }
}

#[test]
#[serial_test::serial]
fn prepare_native_binds_loopback_only_and_hands_back_the_resolved_credential() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    std::env::set_var("LOOM_TEST_PROXY_KEY_8674", "sk-fake-real-credential");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let mut selection = selection_with(Some(zai_flash_proxy()));
    selection.provider = "zai-coding-plan".into();
    selection.profile = Some("zai-flash".into());
    let (prepared, resolved) = prepare_native(tmp.path(), &selection, &on, "opencode")
        .unwrap()
        .expect("substitution should apply on the native path");
    std::env::remove_var("LOOM_TEST_PROXY_KEY_8674");

    // The harness runs on this host: loopback, nothing container-side.
    assert!(prepared.base_url().starts_with("http://127.0.0.1:"), "{}", prepared.base_url());
    assert!(prepared.injection.mask_dirs.is_empty());
    assert!(!prepared.injection.add_host_gateway);

    // The placeholder replaces the real credential under BOTH names, and the
    // real value appears nowhere in the assignments.
    let assignments = &prepared.injection.assignments;
    assert!(
        assignments
            .iter()
            .all(|(_, value)| value != "sk-fake-real-credential"),
        "{assignments:?}"
    );
    for name in ["LOOM_TEST_PROXY_KEY_8674", "ANTHROPIC_AUTH_TOKEN"] {
        assert!(
            assignments
                .iter()
                .any(|(k, v)| k == name && v.starts_with(PLACEHOLDER_PREFIX)),
            "{name} must carry the placeholder: {assignments:?}"
        );
    }
    // The launcher hands the harness command exactly the credential the
    // proxy holds — the pool ladder must not run a second time.
    assert_eq!(
        resolved
            .value_for("ANTHROPIC_AUTH_TOKEN")
            .map(|v| v.to_string_lossy().to_string()),
        Some("sk-fake-real-credential".to_string())
    );
}

#[test]
#[serial_test::serial]
fn prepare_native_refuses_legacy_runtimes_and_non_opencode_config_overrides() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    // A legacy adapter that somehow selects a proxy-declaring profile.
    let legacy =
        prepare_native(tmp.path(), &selection_with(Some(anthropic_proxy())), &on, "claude");
    assert!(legacy.is_err(), "credentialProxy is native-only");
    // The config-side base URL override is OpenCode-only, and Pi is not.
    let pi_selection = selection_with(Some(zai_flash_proxy()));
    let pi = prepare_native(tmp.path(), &pi_selection, &on, "pi");
    assert!(pi.is_err());
    assert!(
        pi.unwrap_err().message.contains("opencode"),
        "the refusal must name the harness"
    );
    // OpenCode itself is fine (fails later only if no credential resolves,
    // which is the ordinary fail-closed ladder, not a shape refusal).
    std::env::set_var("LOOM_TEST_PROXY_KEY_8674", "sk-fake-real-credential");
    let opencode = prepare_native(tmp.path(), &pi_selection, &on, "opencode");
    std::env::remove_var("LOOM_TEST_PROXY_KEY_8674");
    assert!(opencode.unwrap().is_some());
}

#[test]
#[serial_test::serial]
fn prepare_refuses_a_config_only_override_on_a_contained_launch() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    std::env::set_var("LOOM_TEST_PROXY_KEY_8674", "sk-fake-real-credential");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let error =
        prepare(tmp.path(), &selection_with(Some(zai_flash_proxy())), &on, "opencode").unwrap_err();
    std::env::remove_var("LOOM_TEST_PROXY_KEY_8674");
    assert!(
        error
            .message
            .contains("baseUrlConfig is only supported on uncontained"),
        "{}",
        error.message
    );
}

/// #11714: a profile may DECLARE config-side redirection for its opencode
/// binding while it also binds `pi`. With the flag OFF, a contained launch of
/// that profile is plain env-passthrough — the declaration alone must not
/// refuse it.
#[test]
#[serial_test::serial]
fn prepare_is_a_no_op_when_the_flag_is_off_even_with_a_config_only_proxy_block() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let prepared =
        prepare(tmp.path(), &selection_with(Some(zai_flash_proxy())), &json!({}), "opencode")
            .unwrap();
    assert!(prepared.is_none());
}

/// #11714: with the proxy flag ON, a `pi` selection of the bundled
/// config-only profile has no usable redirection left — `baseUrlConfig` is
/// inert off opencode and the block declares no `baseUrlEnv` — so arming it
/// would swap in the placeholder while the traffic still went to the real
/// upstream. `pi` is intentionally unproxied in this slice: refuse instead.
#[test]
#[serial_test::serial]
fn prepare_native_fails_closed_for_a_pi_selection_with_no_usable_redirection() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let profile = crate::worker_spawn::profiles::bundled()
        .get("zai-flash")
        .cloned()
        .expect("bundled zai-flash");
    // Selection, not `resolve`: the flag-on refusal must not need a
    // credential env var to be set before it can name the real problem.
    let (selection, _) =
        crate::worker_spawn::profiles::resolve_shape("pi", "zai-flash", &profile).unwrap();
    let error = prepare_native(tmp.path(), &selection, &on, "pi").unwrap_err();
    assert!(
        error.message.contains("placeholder"),
        "the refusal must name the leak it prevents: {}",
        error.message
    );
}

fn config_content(command: &std::process::Command) -> Value {
    let raw = command
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new("OPENCODE_CONFIG_CONTENT"))
        .and_then(|(_, value)| value)
        .expect("OPENCODE_CONFIG_CONTENT must be set");
    serde_json::from_str(raw.to_str().expect("utf-8")).unwrap()
}

#[test]
#[serial_test::serial]
fn apply_native_env_points_opencode_at_the_proxy_through_config() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
    std::env::set_var("LOOM_TEST_PROXY_KEY_8674", "sk-fake-real-credential");
    let on = json!({"runtimes":{"containment":{"credentialProxy":true}}});
    let mut selection = selection_with(Some(zai_flash_proxy()));
    selection.provider = "zai-coding-plan".into();
    let (prepared, _) = prepare_native(tmp.path(), &selection, &on, "opencode")
        .unwrap()
        .expect("substitution should apply");
    std::env::remove_var("LOOM_TEST_PROXY_KEY_8674");

    // The config `provision` would have written for a real provider block.
    let mut command = std::process::Command::new("opencode");
    command.env(
        "OPENCODE_CONFIG_CONTENT",
        r#"{"model":"zai-coding-plan/glm-5.3-flash","provider":{"zai-coding-plan":{"options":{"apiKey":"{env:ZHIPU_API_KEY}","baseURL":"https://api.z.ai/api/coding/paas/v4"}}}}"#,
    );
    prepared
        .apply_native_env(&mut command, "zai-coding-plan", Some("baseURL"))
        .unwrap();
    let patched = config_content(&command);
    assert_eq!(
        patched
            .pointer("/provider/zai-coding-plan/options/baseURL")
            .and_then(Value::as_str),
        Some(prepared.base_url()),
        "the provider's base URL must be re-pointed at the proxy"
    );
    // Everything else provisioned survives the merge.
    assert_eq!(
        patched.pointer("/model").and_then(Value::as_str),
        Some("zai-coding-plan/glm-5.3-flash")
    );
    assert!(patched
        .pointer("/provider/zai-coding-plan/options/apiKey")
        .is_some());

    // No previously provisioned config: a fresh object carrying the patch.
    let mut fresh = std::process::Command::new("opencode");
    prepared
        .apply_native_env(&mut fresh, "zai-coding-plan", Some("baseURL"))
        .unwrap();
    assert_eq!(
        config_content(&fresh)
            .pointer("/provider/zai-coding-plan/options/baseURL")
            .and_then(Value::as_str),
        Some(prepared.base_url())
    );

    // Malformed provisioned content fails closed — the launcher wrote that
    // value itself, so it is a bug or tampering, never a silent launch.
    let mut broken = std::process::Command::new("opencode");
    broken.env("OPENCODE_CONFIG_CONTENT", "{not json");
    let error = prepared
        .apply_native_env(&mut broken, "zai-coding-plan", Some("baseURL"))
        .unwrap_err();
    assert!(error.message.contains("OPENCODE_CONFIG_CONTENT"), "{}", error.message);
}

#[test]
fn the_bundled_zai_flash_profile_declares_a_valid_proxied_block() {
    let bundled = crate::worker_spawn::profiles::bundled();
    let profile = bundled.get("zai-flash").expect("bundled zai-flash profile");
    let proxy = profile
        .credential_proxy
        .as_ref()
        .expect("zai-flash must declare credentialProxy");
    let upstream = proxy.validate().unwrap();
    assert_eq!(upstream.host(), "api.z.ai");
    assert_eq!(
        upstream.url_for("/chat/completions"),
        "https://api.z.ai/api/coding/paas/v4/chat/completions"
    );
    assert_eq!(proxy.base_url_config.as_deref(), Some("baseURL"));
    assert!(proxy.base_url_env.is_empty());
    assert_eq!(proxy.header, HeaderStyle::AuthorizationBearer);
}
