//! #11284 slice 1: Z.ai (GLM) first for the coding roles, never for Judge.
//!
//! Drives the real shipped role/runtime manifests, so the
//! `worktreeIsolation` requirement of Builder/Doctor is checked against
//! `defaults/runtimes/opencode.json` exactly as in production.
use super::resolve::SkipReason;
use super::tests::{fixture, write_config, ClearedRuntimeEnv};
use super::{resolve_runtime, Decision};
use std::path::Path;

fn zai_first_config() -> serde_json::Value {
    let zai = serde_json::json!({"runtime": "opencode", "modelProfile": "zai-flash"});
    serde_json::json!({"runtimes": {
        "roles": {},
        "preference": ["claude", "codex", zai.clone()],
        "rolePreference": {
            "builder": [zai.clone(), "claude"],
            "doctor": [zai, "claude"],
            "judge": ["codex", "claude"],
        }
    }})
}

fn provision_claude(root: &Path) {
    let dir = root.join(".loom/tokens");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.token"), "sk-ant-oat-fake\n").unwrap();
}

fn zai_pool(root: &Path, enabled: bool) {
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(root);
    crate::api_keys_pool::registry::add(&pool, "zai", "alpha", "ZAI_API_KEY", "fake", false)
        .unwrap();
    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", enabled).unwrap();
}

fn chosen_tap(decision: Decision) -> String {
    let Decision::Preference { resolution, .. } = decision else {
        panic!("expected the preference path");
    };
    resolution.chosen.as_ref().unwrap().tap.to_string()
}

/// With a free Z.ai seat, Builder and Doctor take the Z.ai tap even though
/// Claude is perfectly healthy; Opencode satisfies their `worktreeIsolation`
/// requirement (it is `yes`), so the entry is admitted, not silently skipped.
#[test]
#[serial_test::serial]
fn builder_and_doctor_prefer_the_zai_tap_when_a_seat_is_free() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude(dir.path());
    zai_pool(dir.path(), true);
    write_config(dir.path(), &zai_first_config());
    for role in ["builder", "doctor"] {
        let decision = resolve_runtime(dir.path(), role, None, 0).unwrap();
        assert_eq!(chosen_tap(decision), "opencode:zai-flash", "{role}");
    }
}

/// With the Z.ai pool empty (every account disabled) they fall through to
/// Claude rather than hold: the weekly cap is what bounds GLM-first.
#[test]
#[serial_test::serial]
fn an_empty_zai_pool_falls_through_to_claude() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude(dir.path());
    zai_pool(dir.path(), false);
    write_config(dir.path(), &zai_first_config());
    for role in ["builder", "doctor"] {
        let Decision::Preference { resolution, .. } =
            resolve_runtime(dir.path(), role, None, 0).unwrap()
        else {
            panic!("expected the preference path");
        };
        let chosen = resolution.chosen.as_ref().unwrap();
        assert_eq!(chosen.admitted.runtime, "claude", "{role}");
        assert_eq!(resolution.skipped[0].reason.kind(), "unavailable", "{role}");
        assert!(resolution.skipped[0]
            .reason
            .summary()
            .contains("api_keys:zai"));
    }
}

/// Judge and Curator are unchanged: no Z.ai tap appears in Judge's order.
#[test]
#[serial_test::serial]
fn judge_and_curator_resolution_is_unchanged() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude(dir.path());
    zai_pool(dir.path(), true);
    write_config(dir.path(), &zai_first_config());
    // Judge: codex has no seat here, so it falls to claude, never Z.ai.
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "judge", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.as_ref().unwrap().admitted.runtime, "claude");
    assert!(resolution.order.iter().all(|t| t.runtime != "opencode"));
    // Curator follows the global order: claude first.
    let decision = resolve_runtime(dir.path(), "curator", None, 0).unwrap();
    assert_eq!(chosen_tap(decision), "claude");
}

/// A Z.ai entry on Judge's preference list is skipped with the rule named,
/// and the walk never lands on it even when it is the only healthy tap.
#[test]
#[serial_test::serial]
fn a_zai_entry_in_judges_preference_list_is_skipped_with_the_rule() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    zai_pool(dir.path(), true);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": [
            {"runtime": "opencode", "modelProfile": "zai-flash"}
        ]}}}),
    );
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "judge", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    assert!(resolution.chosen.is_none(), "{resolution:?}");
    let SkipReason::NotAdmitted { detail, .. } = &resolution.skipped[0].reason else {
        panic!("{:?}", resolution.skipped[0].reason);
    };
    assert!(detail.contains(crate::runtime_admission::JUDGE_ZAI_RULE), "{detail}");

    // A non-Z.ai profile on the same runtime is still admitted for Judge.
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": [
            {"runtime": "opencode", "modelProfile": "kimi-k2"}
        ]}}}),
    );
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "judge", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.as_ref().unwrap().admitted.runtime, "opencode");
}

/// Config pin (`runtimes.roles.judge`), env pin (`LOOM_RUNTIME_JUDGE`,
/// `LOOM_RUNTIME`) and an explicit dispatch runtime each name a bare native
/// runtime, which launches on the default profile (`zai-flash`): all are
/// refused, with no fall-through to another runtime and no Z.ai launch.
#[test]
#[serial_test::serial]
fn judge_is_refused_on_zai_for_config_pin_env_pin_and_explicit() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude(dir.path());
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"roles": {"judge": "opencode"},
            "preference": ["claude"]}}),
    );
    let refused = |decision: Decision| {
        let Decision::Static { result, .. } = decision else {
            panic!("a pin must take the static path");
        };
        let rejection = result.unwrap_err();
        assert_eq!(rejection.runtime, "opencode");
        assert!(rejection
            .reason
            .contains(crate::runtime_admission::JUDGE_ZAI_RULE));
    };
    // Config pin.
    // (`runtimes.roles` is read by the static binding, not the pin tiers, so
    // an empty-pin walk resolves through the preference list; the pin is
    // exercised through the binding directly.)
    let rejection =
        crate::runtime_admission::resolve_and_admit(dir.path(), "judge", None).unwrap_err();
    assert!(
        rejection
            .reason
            .contains(crate::runtime_admission::JUDGE_ZAI_RULE),
        "{rejection:?}"
    );
    // Env pins.
    for pin in ["LOOM_RUNTIME_JUDGE", "LOOM_RUNTIME"] {
        std::env::set_var(pin, "opencode");
        let decision = resolve_runtime(dir.path(), "judge", None, 0).unwrap();
        std::env::remove_var(pin);
        refused(decision);
    }
    // Explicit per-dispatch runtime.
    refused(resolve_runtime(dir.path(), "judge", Some("opencode"), 0).unwrap());
    // An env-selected zai profile on a native pin is refused too; a non-zai
    // one is admitted, and the rule is Judge-only (Builder keeps opencode).
    std::env::set_var("LOOM_MODEL_PROFILE", "zai-pro");
    refused(resolve_runtime(dir.path(), "judge", Some("opencode"), 0).unwrap());
    std::env::set_var("LOOM_MODEL_PROFILE", "kimi-k2");
    let decision = resolve_runtime(dir.path(), "judge", Some("opencode"), 0).unwrap();
    let Decision::Static { result, .. } = decision else {
        panic!()
    };
    assert_eq!(result.unwrap().runtime, "opencode");
    std::env::set_var("LOOM_MODEL_PROFILE", "zai-flash");
    let decision = resolve_runtime(dir.path(), "builder", Some("opencode"), 0).unwrap();
    let Decision::Static { result, .. } = decision else {
        panic!()
    };
    assert_eq!(result.unwrap().runtime, "opencode");
    std::env::remove_var("LOOM_MODEL_PROFILE");
}
