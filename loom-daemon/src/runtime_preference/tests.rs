//! Tests for the ordered runtime preference resolver (Issue #8436).
//!
//! The pure-walk tests use `T = &'static str` as the admission result so the
//! interesting orderings need no token pool, no Codex seat, and no metered
//! endpoint on the test host. The integration-shaped tests below build a real
//! `defaults/` fixture and drive [`resolve_runtime`].

use super::availability::{availability, Availability, CredentialSource};
use super::ceiling;
use super::resolve::{resolve, SkipReason, Tap};
use super::{
    check_runtimes_preference_config, preference_for, resolve_runtime, Decision, PreferenceSource,
    StaticReason,
};
use crate::role_runner::PoolHold;
use crate::runtime_admission::{resolve_and_admit, RuntimeSource};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Environment isolation
// ---------------------------------------------------------------------------

/// Clears every runtime-pinning env var for the scope of a test and restores
/// whatever each previously held, including across a panic. A dispatched agent
/// session exports `LOOM_RUNTIME` (see `runtime_admission`'s own guard, #4739),
/// and here an ambient value does not merely outrank config — it takes the
/// *operator-pin* branch and disables the fall-through under test entirely.
///
/// It also repoints the **Codex profile root** at an owned empty directory,
/// which is what makes "codex has no accounts" a property of the fixture
/// rather than of the host. Without it these tests read the developer's real
/// `~/.loom/codex-profiles`, so every assertion that a codex tap is skipped
/// passes in CI (no profiles) and fails on any fleet host that actually has
/// Codex seats provisioned — the shape
/// `codex_is_never_selected_for_builder_however_high_it_is_listed` hit on
/// robb-studio. `role_runner::runtime_preflight`, `provider_health_feedback`
/// and `work_finder::pool_preflight` all isolate the same var in their own
/// guards; this file was the one that did not.
struct ClearedRuntimeEnv {
    prior: Vec<(&'static str, Option<String>)>,
    /// Held only so the empty profile root outlives the guard. Never read.
    _profile_root: tempfile::TempDir,
}

const PIN_VARS: [&str; 8] = [
    "LOOM_RUNTIME",
    "LOOM_RUNTIME_BUILDER",
    "LOOM_RUNTIME_JUDGE",
    "LOOM_RUNTIME_CURATOR",
    "LOOM_CODEX_PROFILE_ROOT",
    "LOOM_CODEX_PROFILE",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
];

impl ClearedRuntimeEnv {
    fn new() -> Self {
        let prior = PIN_VARS
            .iter()
            .map(|key| {
                let prior = std::env::var(key).ok();
                std::env::remove_var(key);
                (*key, prior)
            })
            .collect();
        let profile_root = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profile_root.path());
        Self {
            prior,
            _profile_root: profile_root,
        }
    }
}

impl Drop for ClearedRuntimeEnv {
    fn drop(&mut self) {
        for (key, value) in self.prior.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The pure walk
// ---------------------------------------------------------------------------

fn taps(names: &[&str]) -> Vec<Tap> {
    names.iter().map(|name| Tap::runtime(name)).collect()
}

fn unavailable(source: &str) -> SkipReason {
    SkipReason::Unavailable {
        source: source.to_string(),
        detail: "0/21 spawnable".to_string(),
    }
}

/// The happy path: the most-preferred tap serves the work and nothing below it
/// is evaluated at all — not admitted, not asked for a credential.
#[test]
fn a_healthy_first_tap_short_circuits_the_whole_list() {
    let mut admitted_for = Vec::new();
    let mut asked_for = Vec::new();
    let resolution = resolve(
        &taps(&["claude", "codex", "opencode"]),
        |tap| {
            admitted_for.push(tap.runtime.clone());
            Ok::<_, SkipReason>("admitted")
        },
        |_, tap, _| {
            asked_for.push(tap.runtime.clone());
            Ok(())
        },
    );
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.tier, 0);
    assert_eq!(chosen.tap.runtime, "claude");
    assert!(resolution.skipped.is_empty());
    assert!(!resolution.fell_through());
    // The short circuit is the contract, not an optimisation: evaluating a
    // lower tier would read a metered endpoint's pool for work that is never
    // going there.
    assert_eq!(admitted_for, vec!["claude"]);
    assert_eq!(asked_for, vec!["claude"]);
}

/// The feature's whole reason to exist: the subscription tier is dry, so the
/// walk falls through to the backstop and records why.
#[test]
fn an_exhausted_first_tap_falls_through_and_records_the_reason() {
    let resolution = resolve(
        &[
            Tap::runtime("claude"),
            Tap::with_profile("opencode", "zai-metered"),
        ],
        |_| Ok::<_, SkipReason>("admitted"),
        |_, tap, _| {
            if tap.runtime == "claude" {
                Err(unavailable("claude_tokens"))
            } else {
                Ok(())
            }
        },
    );
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.tier, 1);
    assert_eq!(chosen.tap.to_string(), "opencode:zai-metered");
    assert!(resolution.fell_through());
    assert_eq!(resolution.skipped.len(), 1);
    assert_eq!(resolution.skipped[0].tap.runtime, "claude");
    assert_eq!(resolution.skipped[0].reason.kind(), "unavailable");
}

/// Admission is asked first and a refused tap never has its pool read — the
/// ordering the module doc commits to, and the reason a Codex skip reads
/// "not-admitted(worktreeIsolation)" rather than a misleading credential
/// complaint.
#[test]
fn a_tap_refused_by_admission_never_has_its_credential_source_read() {
    let mut asked_for: Vec<String> = Vec::new();
    let resolution = resolve(
        &taps(&["codex", "claude"]),
        |tap| {
            if tap.runtime == "codex" {
                Err(SkipReason::NotAdmitted {
                    unmet: vec!["worktreeIsolation".into()],
                    detail: "unmet capabilities: worktreeIsolation".into(),
                })
            } else {
                Ok("admitted")
            }
        },
        |_, tap, _| {
            asked_for.push(tap.runtime.clone());
            Ok(())
        },
    );
    assert_eq!(resolution.chosen.as_ref().unwrap().tap.runtime, "claude");
    assert_eq!(asked_for, vec!["claude"]);
    assert_eq!(resolution.skipped[0].reason.summary(), "worktreeIsolation");
}

/// Every tap skipped ⇒ no choice. The caller holds/skips exactly as before
/// #8436; the #7708 hold arms only here, not on a dry Claude pool alone.
#[test]
fn an_entirely_unavailable_list_yields_no_choice_and_names_every_reason() {
    let resolution = resolve(
        &taps(&["claude", "codex"]),
        |_| Ok::<_, SkipReason>("admitted"),
        |_, tap, _| Err(unavailable(&format!("{}_pool", tap.runtime))),
    );
    assert!(resolution.chosen.is_none());
    assert!(!resolution.fell_through());
    assert_eq!(resolution.skipped.len(), 2);
    let diagnostic = resolution.exhausted_diagnostic("builder");
    assert!(diagnostic.contains("builder"), "{diagnostic}");
    assert!(diagnostic.contains("claude -> unavailable"), "{diagnostic}");
    assert!(diagnostic.contains("codex -> unavailable"), "{diagnostic}");
}

/// The telemetry criterion: the chosen tier AND every higher tier's skip
/// reason are in the launch record.
#[test]
fn the_marker_line_records_the_chosen_tier_and_every_skip_reason() {
    let resolution = resolve(
        &[
            Tap::runtime("claude"),
            Tap::runtime("codex"),
            Tap::with_profile("opencode", "zai-metered"),
        ],
        |tap| {
            if tap.runtime == "codex" {
                Err(SkipReason::NotAdmitted {
                    unmet: vec!["worktreeIsolation".into()],
                    detail: "unmet capabilities: worktreeIsolation".into(),
                })
            } else {
                Ok("admitted")
            }
        },
        |_, tap, _| {
            if tap.runtime == "claude" {
                Err(unavailable("claude_tokens"))
            } else {
                Ok(())
            }
        },
    );
    let line = resolution.marker_line();
    assert!(line.starts_with(super::PREFERENCE_LOG_MARKER), "{line}");
    assert!(line.contains("order=claude,codex,opencode:zai-metered"), "{line}");
    assert!(line.contains("tier=2"), "{line}");
    assert!(line.contains("tap=opencode:zai-metered"), "{line}");
    assert!(line.contains("claude:unavailable(claude_tokens: 0/21 spawnable)"), "{line}");
    assert!(line.contains("codex:not-admitted(worktreeIsolation)"), "{line}");
}

/// The preference marker must be a SIBLING of `# LOOM_RUNTIME_RESOLVED`, never
/// extra fields on that line: `crash_signals::resolved_runtime_after` reads
/// that marker by taking the entire rest of the line as the runtime name, so
/// appending to it would make every crash-signal read report a runtime called
/// `"opencode tier=2"`.
#[test]
fn the_preference_marker_does_not_collide_with_the_resolved_runtime_marker() {
    let resolution =
        resolve(&taps(&["opencode"]), |_| Ok::<_, SkipReason>("admitted"), |_, _, _| Ok(()));
    let line = resolution.marker_line();
    assert!(!line.contains("LOOM_RUNTIME_RESOLVED"), "{line}");
    assert!(super::PREFERENCE_LOG_MARKER.starts_with("# LOOM_RUNTIME_PREFERENCE"));
}

// ---------------------------------------------------------------------------
// Config parsing
// ---------------------------------------------------------------------------

#[test]
fn a_config_with_no_preference_key_yields_nothing() {
    for config in [
        serde_json::json!({}),
        serde_json::json!({"terminals": []}),
        serde_json::json!({"runtimes": {"default": "claude"}}),
        // An empty list is "unset, fall through", matching `roles.<role>: ""`.
        serde_json::json!({"runtimes": {"preference": []}}),
    ] {
        assert_eq!(preference_for(&config, "builder").unwrap(), None, "{config}");
        assert!(check_runtimes_preference_config(&config).is_empty(), "{config}");
    }
}

#[test]
fn both_entry_forms_parse_and_the_role_list_wins() {
    let config = serde_json::json!({
        "runtimes": {
            "preference": ["claude", {"runtime": "opencode", "modelProfile": "zai-metered"}],
            "rolePreference": {"judge": ["codex", "claude"]}
        }
    });
    let (source, taps) = preference_for(&config, "builder").unwrap().unwrap();
    assert_eq!(source, PreferenceSource::FleetPreference);
    assert_eq!(
        taps,
        vec![
            Tap::runtime("claude"),
            Tap::with_profile("opencode", "zai-metered")
        ]
    );

    // Judge independence: `rolePreference.judge` keeps Judge off the tap that
    // built the change, and outranks the fleet-wide order.
    let (source, taps) = preference_for(&config, "judge").unwrap().unwrap();
    assert_eq!(source, PreferenceSource::RolePreference);
    assert_eq!(taps, taps_of(&["codex", "claude"]));

    // An empty per-role list falls through to the fleet order rather than
    // meaning "no preference at all".
    let config = serde_json::json!({
        "runtimes": {"preference": ["claude"], "rolePreference": {"judge": []}}
    });
    let (source, _) = preference_for(&config, "judge").unwrap().unwrap();
    assert_eq!(source, PreferenceSource::FleetPreference);
}

fn taps_of(names: &[&str]) -> Vec<Tap> {
    taps(names)
}

/// Fail-closed shape validation, in the spirit of #4494's `runtimes.roles`
/// check: a misconfigured list that degraded silently would strand work on the
/// very tier the operator was routing around.
#[test]
fn malformed_preference_config_fails_closed() {
    let cases: [(serde_json::Value, &str); 7] = [
        (serde_json::json!({"runtimes": {"preference": "claude"}}), "must be an array"),
        (serde_json::json!({"runtimes": {"preference": [""]}}), "empty runtime name"),
        (serde_json::json!({"runtimes": {"preference": [42]}}), "must be a runtime name"),
        (
            serde_json::json!({"runtimes": {"preference": [{"runtme": "claude"}]}}),
            "must name a non-empty \"runtime\"",
        ),
        (
            serde_json::json!({"runtimes": {"preference": [{"runtime": "opencode", "profile": "x"}]}}),
            "unknown key(s)",
        ),
        (
            serde_json::json!({"runtimes": {"preference": ["claude", "claude"]}}),
            "more than once",
        ),
        (
            serde_json::json!({"runtimes": {"rolePreference": {"buidler": ["claude"]}}}),
            "unknown role name(s)",
        ),
    ];
    for (config, expected) in cases {
        let error = preference_for(&config, "builder").unwrap_err();
        assert!(error.contains(expected), "{config} -> {error}");
        assert_eq!(check_runtimes_preference_config(&config).len(), 1, "{config}");
    }
    // A sibling role's broken list is caught on ANY role's resolution, not
    // only when that role ticks — the whole-map discipline #4494 established.
    let sibling = serde_json::json!({
        "runtimes": {"rolePreference": {"judge": ["codex", "codex"], "builder": ["claude"]}}
    });
    assert!(preference_for(&sibling, "builder")
        .unwrap_err()
        .contains("more than once"));
}

// ---------------------------------------------------------------------------
// End-to-end resolution against a real fixture
// ---------------------------------------------------------------------------

/// A workspace with the real shipped role/runtime manifests plus executable
/// adapter stubs, so admission behaves exactly as it does in production —
/// including `codex`'s `worktreeIsolation: "partial"`.
fn fixture() -> tempfile::TempDir {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for sub in ["roles", "runtimes", "scripts"] {
        fs::create_dir_all(dir.path().join(".loom").join(sub)).unwrap();
    }
    for entry in fs::read_dir(repo.join("defaults/roles")).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            fs::copy(
                &path,
                dir.path()
                    .join(".loom/roles")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }
    for entry in fs::read_dir(repo.join("defaults/runtimes"))
        .unwrap()
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            fs::copy(
                &path,
                dir.path()
                    .join(".loom/runtimes")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }
    for runtime in ["claude", "codex", "aider"] {
        let adapter = dir
            .path()
            .join(".loom/scripts")
            .join(format!("spawn-{runtime}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    dir
}

fn write_config(root: &Path, config: &serde_json::Value) {
    fs::create_dir_all(root.join(".loom")).unwrap();
    fs::write(root.join(".loom/config.json"), config.to_string()).unwrap();
}

/// Re-provision the Claude token pool with exactly `count` accounts. Zero is
/// the "no pool at all" state the preflight reports as `NoTokenPool`; the
/// directory is rebuilt each call so a later call genuinely *empties* it.
fn provision_claude_pool(root: &Path, count: usize) {
    let dir = root.join(".loom/tokens");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for n in 0..count {
        fs::write(dir.join(format!("account{n}.token")), "sk-ant-oat-fake\n").unwrap();
    }
}

/// Acceptance criterion 4: with no `preference` key, resolution is the
/// pre-#8436 static path — same runtime, same `RuntimeSource`, and no pool
/// consulted (the fixture has no token pool at all, which would make any
/// availability read report exhaustion).
#[test]
#[serial_test::serial]
fn no_preference_key_means_no_behaviour_change() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({"runtimes": {"default": "claude"}}));
    let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let Decision::Static { reason, result } = decision else {
        panic!("expected static resolution with no preference key");
    };
    assert_eq!(reason, StaticReason::NoPreferenceConfigured);
    let admitted = result.unwrap();
    let baseline = resolve_and_admit(dir.path(), "builder", None).unwrap();
    assert_eq!(admitted, baseline);
    assert_eq!(admitted.source, RuntimeSource::DefaultConfig);
}

/// Acceptance criterion 4, second half: an explicit `LOOM_RUNTIME` pin wins
/// outright and disables fall-through even with a preference list configured
/// and the pinned runtime's pool dry. A pin is a deliberate operator act;
/// silently routing around it would make it useless for debugging.
#[test]
#[serial_test::serial]
fn an_operator_pin_disables_fall_through() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
    );
    // No token pool exists, so the preference walk WOULD pass claude over.
    for (pin, expected) in [
        ("LOOM_RUNTIME", RuntimeSource::GlobalEnvironment),
        ("LOOM_RUNTIME_BUILDER", RuntimeSource::RoleEnvironment),
    ] {
        std::env::set_var(pin, "claude");
        let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
        std::env::remove_var(pin);
        let Decision::Static { reason, result } = decision else {
            panic!("{pin} must take the static operator-pin path");
        };
        assert_eq!(reason, StaticReason::OperatorPin(expected));
        assert_eq!(result.unwrap().runtime, "claude");
    }
    // An explicit per-dispatch runtime is the same kind of pin.
    let decision = resolve_runtime(dir.path(), "builder", Some("claude"), 0).unwrap();
    assert!(matches!(
        decision,
        Decision::Static {
            reason: StaticReason::OperatorPin(RuntimeSource::Explicit),
            ..
        }
    ));
}

/// Acceptance criterion 1: a healthy Claude pool keeps sweeps on Claude, with
/// the walk stopping at tier 0.
#[test]
#[serial_test::serial]
fn a_healthy_claude_pool_keeps_the_work_on_claude() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude_pool(dir.path(), 3);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
    );
    let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let Decision::Preference {
        source, resolution, ..
    } = decision
    else {
        panic!("a configured preference list must take the preference path");
    };
    assert_eq!(source, PreferenceSource::FleetPreference);
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.tier, 0);
    assert_eq!(chosen.admitted.runtime, "claude");
    assert_eq!(chosen.admitted.source, RuntimeSource::Preference);
    assert!(resolution.skipped.is_empty());
    assert!(!resolution.fell_through());
}

/// Acceptance criterion 2 (offline half): with the Claude pool unprovisioned,
/// the walk falls through to the backstop tap instead of yielding nothing —
/// i.e. the #7708 host-level hold would NOT arm, which is the entire point of
/// the feature.
///
/// **Live verification NOT performed.** The criterion's live form ("dispatches
/// on `opencode` against a real endpoint") needs a real metered credential and
/// a model call, which this worktree cannot make. What is established here is
/// the *resolution*: `opencode` is admitted for Builder
/// (`defaults/runtimes/opencode.json` declares `worktreeIsolation: "yes"`) and
/// is selected once Claude is dry. Live dispatch is tracked separately, see
/// the follow-up issues on #8436.
#[test]
#[serial_test::serial]
fn an_exhausted_claude_pool_falls_through_instead_of_holding() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
    );
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "builder", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.tier, 1);
    assert_eq!(chosen.admitted.runtime, "opencode");
    assert!(resolution.fell_through());
    assert_eq!(resolution.skipped.len(), 1);
    assert_eq!(resolution.skipped[0].reason.kind(), "unavailable");
    assert!(
        resolution.skipped[0]
            .reason
            .summary()
            .contains("claude_tokens"),
        "{}",
        resolution.skipped[0].reason.summary()
    );
    // And the marker records both halves.
    let marker = Decision::Preference {
        source: PreferenceSource::FleetPreference,
        resolution,
        backstop: None,
    }
    .marker_line()
    .unwrap();
    assert!(marker.contains("tier=1 tap=opencode"), "{marker}");
    assert!(marker.contains("source=preference"), "{marker}");
}

/// The hold still arms when *every* listed runtime is unavailable — the second
/// half of acceptance criterion 2. Here the backstop's own API-key pool is
/// provisioned but every account disabled, so `worker_spawn::credential`'s
/// ladder would refuse at spawn (exit 78) and the tap must be passed over
/// rather than selected and then killed.
#[test]
#[serial_test::serial]
fn a_wholly_unavailable_list_still_yields_no_choice() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::registry::add(&pool, "zai", "alpha", "ZAI_API_KEY", "fake", false)
        .unwrap();
    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", false).unwrap();
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
    );
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "builder", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    assert!(resolution.chosen.is_none(), "{resolution:?}");
    assert_eq!(resolution.skipped.len(), 2);
    assert!(
        resolution.skipped[1]
            .reason
            .summary()
            .contains("api_keys:zai"),
        "{}",
        resolution.skipped[1].reason.summary()
    );
    let diagnostic = resolution.exhausted_diagnostic("builder");
    assert!(diagnostic.contains("preference order: claude,opencode"), "{diagnostic}");
}

/// Acceptance criterion 3, against the REAL shipped manifests: Codex is never
/// selected for Builder however high it sits in the list, because
/// `defaults/runtimes/codex.json` declares `worktreeIsolation: "partial"` and
/// `defaults/roles/builder.json` requires it. The list is a preference, never
/// an admission override — so the effective build chain is `claude ->
/// <native>`, with Codex skipped as `not-admitted`, not as "no credential".
#[test]
#[serial_test::serial]
fn codex_is_never_selected_for_builder_however_high_it_is_listed() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["codex", "claude"]}}),
    );
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "builder", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.admitted.runtime, "claude");
    assert_eq!(resolution.skipped.len(), 1);
    assert_eq!(resolution.skipped[0].tap.runtime, "codex");
    assert_eq!(resolution.skipped[0].reason.kind(), "not-admitted");
    assert_eq!(resolution.skipped[0].reason.summary(), "worktreeIsolation");

    // On a role Codex IS admitted for, the same list skips it for a
    // completely different reason — an empty codex account pool, which is a
    // *credential* verdict that can clear on its own. That the two roles
    // produce different skip kinds for the same entry is the proof that
    // Builder's skip is admission, not a blanket refusal of Codex.
    let Decision::Preference { resolution, .. } =
        resolve_runtime(dir.path(), "curator", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.skipped[0].tap.runtime, "codex");
    assert_eq!(resolution.skipped[0].reason.kind(), "unavailable");
    assert!(
        resolution.skipped[0]
            .reason
            .summary()
            .contains("codex_accounts"),
        "{}",
        resolution.skipped[0].reason.summary()
    );
}

/// Malformed preference config fails closed at the resolution seam too, not
/// only in `loom-daemon validate`.
#[test]
#[serial_test::serial]
fn a_malformed_preference_list_rejects_the_launch() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({"runtimes": {"preference": ["claude", ""]}}));
    let rejection = resolve_runtime(dir.path(), "builder", None, 0).unwrap_err();
    assert_eq!(rejection.source, RuntimeSource::Preference);
    assert!(rejection.reason.contains("empty runtime name"), "{}", rejection.reason);
}

// ---------------------------------------------------------------------------
// `Decision::into_admission` — the #8554 dispatch-seam collapse
// ---------------------------------------------------------------------------

/// The static path's `result` passes straight through `into_admission`,
/// unchanged in either arm — this is what keeps a dispatch call site that
/// swaps `runtime_admission::resolve_and_admit` for
/// `resolve_runtime(..).and_then(|d| d.into_admission(role))` byte-identical
/// when no preference is configured (#8554).
#[test]
#[serial_test::serial]
fn into_admission_passes_the_static_result_through_unchanged() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({"runtimes": {"default": "claude"}}));
    let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let baseline = resolve_and_admit(dir.path(), "builder", None).unwrap();
    assert_eq!(decision.into_admission("builder").unwrap(), baseline);

    // The static Err arm too — an admission genuinely rejected outright
    // (not merely unavailable), never a preference-shaped diagnostic.
    let unknown_role_decision = resolve_runtime(dir.path(), "not-a-role", None, 0).unwrap();
    let unknown_role_baseline = resolve_and_admit(dir.path(), "not-a-role", None).unwrap_err();
    let error = unknown_role_decision
        .into_admission("not-a-role")
        .unwrap_err();
    assert_eq!(error, unknown_role_baseline);
}

/// The preference path's happy case: the chosen tap's admission comes
/// through as `Ok`.
#[test]
#[serial_test::serial]
fn into_admission_unwraps_the_chosen_tap_on_the_preference_path() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
    );
    let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let admitted = decision.into_admission("builder").unwrap();
    assert_eq!(admitted.runtime, "claude");
    assert_eq!(admitted.source, RuntimeSource::Preference);
}

/// The preference path's fail-closed case: every tap skipped becomes a
/// `RuntimeRejection` naming the role and carrying the same
/// `exhausted_diagnostic` text an operator would read off `Resolution`
/// directly — never a silent `Ok` and never the empty-string/`Unobservable`
/// shape a caller might mistake for a real (if unhelpful) admission.
#[test]
#[serial_test::serial]
fn into_admission_reports_a_wholly_unavailable_list_as_a_rejection() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({"runtimes": {"preference": ["claude"]}}));
    // No Claude pool provisioned at all: the sole listed tap is unavailable.
    let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let rejection = decision.into_admission("builder").unwrap_err();
    assert_eq!(rejection.role, "builder");
    assert_eq!(rejection.source, RuntimeSource::Preference);
    assert!(rejection.unmet_capabilities.is_empty());
    assert!(rejection.reason.contains("preference order: claude"), "{}", rejection.reason);
    assert!(rejection.reason.contains("claude_tokens"), "{}", rejection.reason);
}

/// `resolve_for_dispatch` is the function `sweep_registry::dispatch` actually
/// calls, so the "absent config is byte-identical" invariant is pinned at
/// *that* seam too, not only at `resolve_runtime`/`into_admission`: with no
/// preference key it must return exactly what `resolve_and_admit` returns, in
/// both the `Ok` and the `Err` arm (#8554).
#[test]
#[serial_test::serial]
fn resolve_for_dispatch_is_a_one_for_one_substitution_when_unconfigured() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({"runtimes": {"default": "claude"}}));
    assert_eq!(
        super::resolve_for_dispatch(dir.path(), "sweep-lifecycle", None).unwrap(),
        resolve_and_admit(dir.path(), "sweep-lifecycle", None).unwrap()
    );
    assert_eq!(
        super::resolve_for_dispatch(dir.path(), "not-a-role", None).unwrap_err(),
        resolve_and_admit(dir.path(), "not-a-role", None).unwrap_err()
    );
}

/// The dispatch seam's fail-closed half: a configured list whose every tap is
/// unavailable refuses the launch (which `dispatch` turns into a
/// `SweepGlobalRuntimeRejected` event), never an `Ok` onto a dry tap.
#[test]
#[serial_test::serial]
fn resolve_for_dispatch_fails_closed_on_a_wholly_unavailable_list() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    write_config(dir.path(), &serde_json::json!({"runtimes": {"preference": ["claude"]}}));
    // No Claude pool provisioned at all: the sole listed tap is unavailable.
    let rejection = super::resolve_for_dispatch(dir.path(), "sweep-lifecycle", None).unwrap_err();
    assert_eq!(rejection.source, RuntimeSource::Preference);
    assert!(rejection.reason.contains("preference order: claude"), "{}", rejection.reason);
}

// ---------------------------------------------------------------------------
// The shared availability mapping
// ---------------------------------------------------------------------------

/// The availability mapping must agree with the role runner's own pre-spawn
/// gate about whether the Claude pool is a wall — that is the anti-drift
/// guarantee behind "one mapping, two renderings" (#8408 + #8436). The gate
/// produces a skip outcome exactly when the mapping reports exhaustion.
#[test]
#[serial_test::serial]
fn claude_availability_agrees_with_the_preflight_gate() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    let logs = dir.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let admitted = resolve_and_admit(dir.path(), "builder", Some("claude")).unwrap();
    let tap = Tap::runtime("claude");

    for accounts in [0_usize, 2] {
        provision_claude_pool(dir.path(), accounts);
        // `static_check`, not the preference-aware `check` wrapper (#8554):
        // the guarantee under test is that the MAPPING agrees with the GATE,
        // and the wrapper's job is to consult the mapping.
        let gate_skips = crate::role_runner::runtime_preflight::static_check(
            dir.path(),
            &logs,
            "builder",
            Some(&Ok(admitted.clone())),
        )
        .is_some();
        let state = availability(dir.path(), &tap, &admitted, 0);
        assert_eq!(
            gate_skips,
            !state.is_spawnable(),
            "accounts={accounts}: gate_skips={gate_skips} availability={state:?}"
        );
        assert_eq!(state.source(), &CredentialSource::ClaudeTokens);
    }

    // The unprovisioned pool is the permanent hold, not the self-healing one:
    // no amount of waiting makes a token appear.
    provision_claude_pool(dir.path(), 0);
    let state = availability(dir.path(), &tap, &admitted, 0);
    assert!(
        matches!(
            state,
            Availability::Exhausted {
                hold: PoolHold::Unprovisioned,
                ..
            }
        ),
        "{state:?}"
    );
}

/// A native tap whose provider is not pooled on this host is reported
/// available, never passed over: `worker_spawn::credential`'s ladder falls
/// through to the harness's own auth store there, so there is no wall to gate
/// on. This is the mirror of `runtime_preflight`'s "never skip a launch that
/// could have succeeded" — a false "unavailable" on a subscription tier
/// silently routes paid-for work onto a metered endpoint.
#[test]
#[serial_test::serial]
fn an_unpooled_native_provider_is_never_passed_over() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    let admitted = resolve_and_admit(dir.path(), "builder", Some("opencode")).unwrap();
    // No `zai` API-key accounts are registered in this workspace.
    let state = availability(dir.path(), &Tap::runtime("opencode"), &admitted, 0);
    assert!(state.is_spawnable(), "{state:?}");
    assert_eq!(state.source(), &CredentialSource::Unobservable);
    assert!(state.skip_reason().is_none());
}

/// …and once the provider IS pooled, the pool becomes the wall: an all-
/// disabled pool is exhausted, and registering a usable account clears it.
/// Same provider, same profile — only the pool's contents differ.
#[test]
#[serial_test::serial]
fn a_pooled_native_provider_is_gated_on_its_api_key_pool() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    let admitted = resolve_and_admit(dir.path(), "builder", Some("opencode")).unwrap();
    let tap = Tap::runtime("opencode");
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::registry::add(&pool, "zai", "alpha", "ZAI_API_KEY", "fake", false)
        .unwrap();
    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", false).unwrap();

    let state = availability(dir.path(), &tap, &admitted, 0);
    assert!(!state.is_spawnable(), "{state:?}");
    assert_eq!(
        state.source(),
        &CredentialSource::ApiKeys {
            provider: "zai".into()
        }
    );
    assert!(state
        .skip_reason()
        .unwrap()
        .summary()
        .contains("api_keys:zai"));

    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", true).unwrap();
    assert!(availability(dir.path(), &tap, &admitted, 0).is_spawnable());
}

/// Wire names are stable and pool-qualified, so "how much work went to the
/// metered backstop" is one grep across the role-tick record and the
/// preference marker.
#[test]
fn credential_source_wire_names_are_stable_and_pool_qualified() {
    assert_eq!(CredentialSource::ClaudeTokens.wire(), "claude_tokens");
    assert_eq!(CredentialSource::CodexAccounts.wire(), "codex_accounts");
    assert_eq!(
        CredentialSource::ApiKeys {
            provider: "zai".into()
        }
        .wire(),
        "api_keys:zai"
    );
    // The two overlapping names match `CredentialPool::as_str`'s wire values,
    // so the role-tick `gated_pool` key and this marker agree.
    assert_eq!(
        CredentialSource::ClaudeTokens.wire(),
        crate::role_runner::CredentialPool::ClaudeTokens.as_str()
    );
    assert_eq!(
        CredentialSource::CodexAccounts.wire(),
        crate::role_runner::CredentialPool::CodexAccounts.as_str()
    );
}

// ---------------------------------------------------------------------------
// The per-host backstop ceiling, end to end through the resolver (#8555)
// ---------------------------------------------------------------------------

/// Points the machine-wide backstop lease dir at a tempdir for the scope of a
/// test and restores whatever was there, including across a panic. Mandatory
/// for any ceiling test: the real store is machine-wide by design, so without
/// it a test would count (and reap) the host's real leases.
struct ScopedLeaseDir {
    _dir: tempfile::TempDir,
    prior: Option<std::ffi::OsString>,
}

impl ScopedLeaseDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let prior = std::env::var_os(ceiling::LEASE_DIR_ENV);
        std::env::set_var(ceiling::LEASE_DIR_ENV, dir.path().join("backstop"));
        Self { _dir: dir, prior }
    }
}

impl Drop for ScopedLeaseDir {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(ceiling::LEASE_DIR_ENV, value),
            None => std::env::remove_var(ceiling::LEASE_DIR_ENV),
        }
    }
}

/// A fixture whose Claude pool is unprovisioned (so the walk falls through to
/// the backstop tap) with a ceiling of `max` concurrent backstop dispatches.
fn ceiling_fixture(max: u32) -> tempfile::TempDir {
    let dir = fixture();
    write_config(
        dir.path(),
        &serde_json::json!({
            "runtimes": {
                "preference": ["claude", "opencode"],
                "backstopCeiling": {"maxConcurrent": max}
            }
        }),
    );
    dir
}

/// **The acceptance criterion of #8555**: with a ceiling of N, the N+1th
/// *concurrent* backstop dispatch is passed over — recorded as a skip, exactly
/// like an unavailable tap — and, with nothing below it to take, the walk fails
/// closed. No approval step, no wait: just a dispatch that does not go to the
/// metered endpoint.
#[test]
#[serial_test::serial]
fn the_n_plus_first_concurrent_backstop_dispatch_is_passed_over() {
    let _env = ClearedRuntimeEnv::new();
    let _leases = ScopedLeaseDir::new();
    let dir = ceiling_fixture(1);

    // N = 1: the first dispatch falls through to the backstop and takes the
    // host's only metered slot.
    let first = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let Decision::Preference {
        ref resolution,
        ref backstop,
        ..
    } = first
    else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.tier, 1);
    assert_eq!(chosen.admitted.runtime, "opencode");
    assert!(resolution.fell_through());
    assert_eq!(backstop.as_ref().unwrap().summary(), "1/1");
    // The marker records the slot alongside the fall-through it paid for.
    let marker = first.marker_line().unwrap();
    assert!(marker.contains("tier=1 tap=opencode"), "{marker}");
    assert!(marker.contains("backstop=1/1"), "{marker}");

    // N+1: while that slot is held, the next dispatch is passed over.
    let second = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let Decision::Preference {
        resolution: second_resolution,
        backstop: second_backstop,
        ..
    } = second
    else {
        panic!("expected the preference path");
    };
    assert!(second_backstop.is_none(), "a refused dispatch must hold no slot");
    // Fail closed: nothing below the backstop qualifies, so there is no
    // choice — precisely the pre-#8436 hold, reached for a new reason.
    assert!(second_resolution.chosen.is_none(), "{second_resolution:?}");
    assert_eq!(second_resolution.skipped.len(), 2);
    let refusal = &second_resolution.skipped[1];
    assert_eq!(refusal.tap.runtime, "opencode");
    assert_eq!(refusal.reason.kind(), "ceiling-at-capacity");
    assert!(refusal.reason.summary().contains("1/1"), "{}", refusal.reason.summary());
    let diagnostic = second_resolution.exhausted_diagnostic("builder");
    assert!(diagnostic.contains("ceiling-at-capacity"), "{diagnostic}");

    // Releasing the first dispatch's slot frees the host immediately — the
    // bound is concurrency, not a cooldown, so nothing waits on a clock.
    drop(first);
    let third = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    let Decision::Preference { resolution, .. } = third else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.as_ref().unwrap().tier, 1);
}

/// The other half of the acceptance criterion: a higher tap that recovers is
/// still preferred, ceiling or no ceiling. The bound applies to the metered
/// tier only — it must never throttle the subscription capacity the fleet
/// already pays for, and while tier 0 serves the work the ceiling is not even
/// consulted (no slot is taken).
#[test]
#[serial_test::serial]
fn a_recovered_higher_tap_is_still_preferred_while_the_backstop_is_at_its_ceiling() {
    let _env = ClearedRuntimeEnv::new();
    let _leases = ScopedLeaseDir::new();
    let dir = ceiling_fixture(1);

    // Hold the host's only backstop slot.
    let held = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
    assert!(matches!(
        &held,
        Decision::Preference {
            backstop: Some(_),
            ..
        }
    ));

    // Claude comes back. Every subsequent dispatch returns to tier 0 even
    // though the backstop is pinned at its ceiling.
    provision_claude_pool(dir.path(), 3);
    for _ in 0..3 {
        let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
        let Decision::Preference {
            resolution,
            backstop,
            ..
        } = decision
        else {
            panic!("expected the preference path");
        };
        let chosen = resolution.chosen.as_ref().unwrap();
        assert_eq!(chosen.tier, 0);
        assert_eq!(chosen.admitted.runtime, "claude");
        assert!(!resolution.fell_through());
        assert!(resolution.skipped.is_empty());
        assert!(backstop.is_none(), "tier 0 must not consume a metered slot");
    }
    drop(held);
}

/// The optional eligibility filter, through the resolver: low-value work never
/// reaches the metered tap, while the work the operator considers worth paying
/// for still does. Unmarked work is `routine`, the daemon's existing default
/// for a missing `<!-- loom:complexity= -->` marker.
#[test]
#[serial_test::serial]
fn the_eligibility_filter_keeps_low_value_work_off_the_metered_tap() {
    let _env = ClearedRuntimeEnv::new();
    let _leases = ScopedLeaseDir::new();
    let dir = fixture();
    write_config(
        dir.path(),
        &serde_json::json!({
            "runtimes": {
                "preference": ["claude", "opencode"],
                "backstopCeiling": {"maxConcurrent": 4, "minComplexity": "complex"}
            }
        }),
    );

    let context = super::DispatchContext {
        complexity: Some("mechanical"),
    };
    let decision = super::resolve_runtime_for(dir.path(), "builder", None, 0, context).unwrap();
    let Decision::Preference { resolution, .. } = decision else {
        panic!("expected the preference path");
    };
    assert!(resolution.chosen.is_none());
    assert_eq!(resolution.skipped[1].reason.kind(), "ceiling-ineligible");

    let context = super::DispatchContext {
        complexity: Some("complex"),
    };
    let decision = super::resolve_runtime_for(dir.path(), "builder", None, 0, context).unwrap();
    let Decision::Preference {
        resolution,
        backstop,
        ..
    } = decision
    else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.as_ref().unwrap().tier, 1);
    assert!(backstop.is_some());
}

/// The no-ceiling edge case, end to end: with no `backstopCeiling` key the
/// resolution is byte-identical to a build without the bound, and the lease
/// store is never even created.
#[test]
#[serial_test::serial]
fn an_unconfigured_ceiling_touches_no_state_and_bounds_nothing() {
    let _env = ClearedRuntimeEnv::new();
    let _leases = ScopedLeaseDir::new();
    let dir = fixture();
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
    );
    let store = ceiling::lease_dir().unwrap();
    let mut held = Vec::new();
    for _ in 0..5 {
        let decision = resolve_runtime(dir.path(), "builder", None, 0).unwrap();
        let Decision::Preference {
            resolution,
            backstop,
            ..
        } = decision
        else {
            panic!("expected the preference path");
        };
        assert_eq!(resolution.chosen.as_ref().unwrap().tier, 1);
        assert!(backstop.is_none(), "an unconfigured ceiling must take no slot");
        held.push(resolution);
    }
    assert!(!store.exists(), "no ceiling ⇒ no lease store at {}", store.display());
}

/// A malformed ceiling fails the launch closed at the resolution seam, not
/// only in `loom-daemon validate` — a typo must never silently mean
/// "unbounded metered spend".
#[test]
#[serial_test::serial]
fn a_malformed_ceiling_rejects_the_launch() {
    let _env = ClearedRuntimeEnv::new();
    let _leases = ScopedLeaseDir::new();
    let dir = fixture();
    write_config(
        dir.path(),
        &serde_json::json!({
            "runtimes": {
                "preference": ["claude", "opencode"],
                "backstopCeiling": {"maxConcurrent": 1, "appliesFrom": 0}
            }
        }),
    );
    let rejection = resolve_runtime(dir.path(), "builder", None, 0).unwrap_err();
    assert_eq!(rejection.source, RuntimeSource::Preference);
    assert!(rejection.reason.contains("appliesFrom"), "{}", rejection.reason);
}
