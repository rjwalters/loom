//! A model profile's `excludedRoles` is enforced at admission for every
//! selection source (#11400): GLM-first Doctor ticks, and Judge / Curator are
//! never admitted on `zai-flash` — via a preference entry, a `runtimes.roles`
//! binding, or an env pin.
//!
//! Hermetic: [`ClearedRuntimeEnv`] clears `LOOM_RUNTIME*`,
//! `LOOM_MODEL_PROFILE`, `ZAI_API_KEY` / `ZHIPU_API_KEY` (#10767),
//! `LOOM_NATIVE_AUTH_FILE` and `LOOM_SHARED_API_KEYS_DIR`, so neither an
//! exported key, an auth snapshot nor a host-wide pool can make the per-repo
//! `zai` pool stop being the wall.
use super::tests::{fixture, provision_claude_pool, write_config, ClearedRuntimeEnv};
use super::unprovisioned_tests::provision_zai_pool;
use super::{resolve_runtime, Decision, PreferenceSource, StaticReason};
use crate::runtime_admission::{resolve_and_admit, RuntimeSource};
use crate::worker_spawn::profiles::EXCLUDED_ROLE;
use std::path::Path;

fn zai_tap() -> serde_json::Value {
    serde_json::json!({"runtime": "opencode", "modelProfile": "zai-flash"})
}

fn shipped_config() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../.loom/config.json")).unwrap()
}

fn chosen(decision: &Decision) -> (String, Option<String>) {
    let Decision::Preference { resolution, .. } = decision else {
        panic!("expected the preference path, got {decision:?}");
    };
    let chosen = resolution.chosen.as_ref().expect("a tap must be chosen");
    (chosen.admitted.runtime.clone(), chosen.tap.model_profile.clone())
}

/// Resolve `role` against the fleet's routing, as fleet-gitops
/// `config.defaults.runtimes` renders it into the machine tier (#11450): plain
/// string entries (its schema has no `{runtime, modelProfile}` objects) on the
/// bundled `zai-flash` default profile. This repo's `.loom/config.json` no
/// longer carries it.
fn write_fleet_role_preference(root: &Path, role: &str) {
    let list = match role {
        "builder" => serde_json::json!(["opencode", "claude"]),
        "doctor" => serde_json::json!(["opencode", "codex", "claude"]),
        other => unreachable!("{other}"),
    };
    write_config(
        root,
        &serde_json::json!({"runtimes": {
            "defaultModelProfile": "zai-flash",
            "rolePreference": {role: list}
        }}),
    );
}

/// Claude is chosen; the `opencode` entry was passed over as unavailable (no
/// selectable zai seat), and any other skip is Doctor's Codex step, which the
/// admission gate refuses (`worktreeIsolation` is only `partial`).
fn assert_fell_through_to_claude(decision: &Decision, role: &str) {
    assert_eq!(chosen(decision), ("claude".into(), None), "{role}");
    let Decision::Preference { resolution, .. } = decision else {
        unreachable!()
    };
    let reason = resolution.skipped[0].reason.to_string();
    assert!(reason.starts_with("unavailable("), "{role}: {reason}");
    for skip in &resolution.skipped[1..] {
        let reason = skip.reason.to_string();
        assert!(reason.starts_with("not-admitted("), "{role}: {reason}");
    }
    assert_eq!(resolution.skipped.len(), if role == "doctor" { 2 } else { 1 }, "{role}");
}

/// Acceptance 1 (Doctor, #11400) and the operator's Builder addition: with a
/// selectable `zai` account the role resolves `opencode:zai-flash`; with no
/// zai pool on the host at all, or with every account disabled, it falls
/// through to `claude`. "No pool at all" is the common state of an
/// unprovisioned fleet host: a guarded launch never reuses OpenCode's own
/// login, so selecting the tap there would fail every tick at the provider.
#[test]
#[serial_test::serial]
fn doctor_and_builder_run_glm_first_and_fall_through_to_claude_without_a_zai_seat() {
    let _env = ClearedRuntimeEnv::new();
    for role in ["doctor", "builder"] {
        let dir = fixture();
        provision_claude_pool(dir.path(), 2);
        write_fleet_role_preference(dir.path(), role);

        // No zai pool on this host, no exported key, no auth snapshot.
        let decision = resolve_runtime(dir.path(), role, None, 0).unwrap();
        assert_fell_through_to_claude(&decision, role);

        provision_zai_pool(dir.path(), true);
        let decision = resolve_runtime(dir.path(), role, None, 0).unwrap();
        // A bare entry carries no per-tap profile; it was judged against the
        // `zai-flash` default, which is why the zai pool decides it.
        assert_eq!(chosen(&decision), ("opencode".into(), None), "{role}");
        let Decision::Preference { source, .. } = &decision else {
            unreachable!()
        };
        assert_eq!(*source, PreferenceSource::RolePreference, "{role}");

        // Same account, disabled: nothing selectable, so the walk falls through.
        provision_zai_pool(dir.path(), false);
        let decision = resolve_runtime(dir.path(), role, None, 0).unwrap();
        assert_fell_through_to_claude(&decision, role);
    }
}

/// Acceptance 2, preference half: a zai-flash entry is skipped for Judge and
/// Curator as `not-admitted(excludedRole)` even with a healthy zai pool, and
/// listing it first does not change that.
#[test]
#[serial_test::serial]
fn a_zai_flash_preference_entry_is_never_admitted_for_judge_or_curator() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    provision_zai_pool(dir.path(), true);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": [zai_tap(), "claude"]}}),
    );
    for role in ["judge", "curator"] {
        let decision = resolve_runtime(dir.path(), role, None, 0).unwrap();
        assert_eq!(chosen(&decision), ("claude".into(), None), "{role}");
        let Decision::Preference { resolution, .. } = &decision else {
            unreachable!()
        };
        assert_eq!(resolution.skipped.len(), 1, "{role}");
        assert_eq!(
            resolution.skipped[0].reason.to_string(),
            format!("not-admitted({EXCLUDED_ROLE})"),
            "{role}"
        );
    }
    // The same entry still serves a role the profile does not exclude.
    let decision = resolve_runtime(dir.path(), "doctor", None, 0).unwrap();
    assert_eq!(chosen(&decision), ("opencode".into(), Some("zai-flash".into())));
}

/// A bare `opencode` entry falls back to the bundled default profile
/// (`zai-flash`), so it is judged against that profile, not waved through.
#[test]
#[serial_test::serial]
fn a_bare_opencode_entry_is_judged_against_the_default_profile() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["opencode", "claude"]}}),
    );
    let decision = resolve_runtime(dir.path(), "judge", None, 0).unwrap();
    assert_eq!(chosen(&decision).0, "claude");

    // A default profile that excludes nothing admits it (its credential
    // exported, so availability does not pass it over as unprovisioned).
    std::env::set_var("GEMINI_API_KEY", "exported");
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {
            "preference": ["opencode", "claude"],
            "defaultModelProfile": "quick-flash"
        }}),
    );
    let decision = resolve_runtime(dir.path(), "judge", None, 0).unwrap();
    std::env::remove_var("GEMINI_API_KEY");
    assert_eq!(chosen(&decision).0, "opencode");
}

/// Acceptance 2, pin half: a `runtimes.roles` binding, a per-role env pin and
/// an explicit per-dispatch runtime onto zai-flash are refused with the reason
/// — a pin is never silently re-routed.
#[test]
#[serial_test::serial]
fn pins_onto_zai_flash_are_refused_for_judge_and_curator_with_the_reason() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    let assert_refused = |role: &str, rejection: crate::runtime_admission::RuntimeRejection| {
        assert_eq!(rejection.unmet_capabilities, vec![EXCLUDED_ROLE.to_string()], "{role}");
        assert!(rejection.reason.contains("\"zai-flash\""), "{}", rejection.reason);
        assert!(rejection.reason.contains("excludedRoles"), "{}", rejection.reason);
        assert_eq!(rejection.runtime, "opencode");
    };

    // runtimes.roles binding (profile from runtimes.defaultModelProfile).
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {
            "roles": {"judge": "opencode", "curator": "opencode"},
            "defaultModelProfile": "zai-flash"
        }}),
    );
    for role in ["judge", "curator"] {
        let rejection = resolve_and_admit(dir.path(), role, None).unwrap_err();
        assert_eq!(rejection.source, RuntimeSource::RoleConfig);
        assert_refused(role, rejection);
    }

    // LOOM_RUNTIME_<ROLE> + LOOM_MODEL_PROFILE env pin.
    write_config(dir.path(), &serde_json::json!({}));
    std::env::set_var("LOOM_MODEL_PROFILE", "zai-flash");
    for (role, pin) in [
        ("judge", "LOOM_RUNTIME_JUDGE"),
        ("curator", "LOOM_RUNTIME_CURATOR"),
    ] {
        std::env::set_var(pin, "opencode");
        let decision = resolve_runtime(dir.path(), role, None, 0).unwrap();
        std::env::remove_var(pin);
        let Decision::Static { reason, result } = decision else {
            panic!("{pin} must take the static operator-pin path");
        };
        assert_eq!(reason, StaticReason::OperatorPin(RuntimeSource::RoleEnvironment));
        assert_refused(role, result.unwrap_err());
    }

    // Explicit per-dispatch runtime.
    let rejection = resolve_and_admit(dir.path(), "judge", Some("opencode")).unwrap_err();
    assert_eq!(rejection.source, RuntimeSource::Explicit);
    assert_refused("judge", rejection);

    // Doctor on the same pin is admitted, and so is Judge on another profile.
    assert_eq!(
        resolve_and_admit(dir.path(), "doctor", Some("opencode"))
            .unwrap()
            .runtime,
        "opencode"
    );
    std::env::set_var("LOOM_MODEL_PROFILE", "quick-flash");
    assert_eq!(
        resolve_and_admit(dir.path(), "judge", Some("opencode"))
            .unwrap()
            .runtime,
        "opencode"
    );
    std::env::remove_var("LOOM_MODEL_PROFILE");
}

/// A native sweep runs Judge in the same session, so a sweep on zai-flash is
/// refused through its Judge/Curator phase rather than reviewing on GLM.
#[test]
#[serial_test::serial]
fn a_native_sweep_on_zai_flash_is_refused_through_its_review_phases() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({}));
    let rejection = super::super::runtime_admission::resolve_and_admit_profiled(
        dir.path(),
        "sweep-lifecycle",
        Some("opencode"),
        Some("zai-flash"),
        crate::runtime_admission::AdmissionContext::Host,
    )
    .unwrap_err();
    assert_eq!(rejection.unmet_capabilities, vec![EXCLUDED_ROLE.to_string()]);
    assert!(rejection.reason.contains("native sweep phase"), "{}", rejection.reason);
}

/// `loom-daemon validate`: the shipped profiles pass; an unknown role name in
/// a configured profile's `excludedRoles` (or a non-array) is a finding.
#[test]
fn validate_rejects_an_unknown_excluded_role() {
    use crate::worker_spawn::profiles::check_excluded_roles_config;
    assert!(check_excluded_roles_config(&serde_json::json!({})).is_empty());
    let shipped = shipped_config();
    assert_eq!(check_excluded_roles_config(&shipped), Vec::<String>::new());
    assert_eq!(super::check_runtimes_preference_config(&shipped), Vec::<String>::new());
    let bad = serde_json::json!({"runtimes": {"modelProfiles": {
        "glm": {"model": "m", "providers": {"opencode": "p"}, "excludedRoles": ["judge", "reviewer"]},
        "worse": {"model": "m", "providers": {}, "excludedRoles": "judge"}
    }}});
    let findings = check_excluded_roles_config(&bad);
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings[0].contains("glm.excludedRoles") && findings[0].contains("reviewer"));
    assert!(findings[1].contains("worse.excludedRoles must be an array"));
    let result = crate::role_validation::validate_from_config(
        &bad,
        crate::role_validation::ValidationMode::Strict,
    );
    assert!(!result.valid);
}
