//! Launch-boundary regression coverage for runtime-dependent model defaults.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::sweep_registry::test_support::*;
use crate::sweep_registry::{DispatchModel, SweepRegistry};
use crate::types::SweepKind;
use crate::work_finder::{RegistryDispatcher, WorkDispatcher};
use crate::worker_spawn::{profiles, Options};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn isolated(root: &Path) -> Self {
        let keys = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_RUNTIME_JUDGE",
            "LOOM_RUNTIME_CURATOR",
            "LOOM_CONFIG_DEFAULTS_FILE",
            "LOOM_CODEX_PROFILE_ROOT",
            "LOOM_CODEX_PROFILE",
            "LOOM_CODEX_HOME",
            "CODEX_HOME",
            "LOOM_MODEL_EXPERIMENT",
            "LOOM_MODEL_EXPERIMENT_CANARY",
            "LOOM_MODEL",
            "LOOM_MODEL_PROFILE",
            "LOOM_BACKSTOP_MAX_CONCURRENT",
            "LOOM_BACKSTOP_LEASE_STALE_SECS",
            "LOOM_BACKSTOP_LEASE_DIR",
            "LOOM_SHARED_TOKENS_DIR",
            "LOOM_SHARED_API_KEYS_DIR",
        ];
        let prior = keys
            .into_iter()
            .map(|key| {
                let value = std::env::var_os(key);
                std::env::remove_var(key);
                (key, value)
            })
            .collect();
        for key in [
            "LOOM_CONFIG_DEFAULTS_FILE",
            "LOOM_SHARED_TOKENS_DIR",
            "LOOM_SHARED_API_KEYS_DIR",
        ] {
            std::env::set_var(key, "");
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.join("codex-profiles"));
        std::env::set_var("LOOM_BACKSTOP_LEASE_DIR", root.join("backstop"));
        Self(prior)
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn fallback_config() -> Value {
    json!({"runtimes": {
        "default": "claude", "preference": ["claude", "codex", "opencode"],
        "backstopCeiling": {"maxConcurrent": 1},
        "defaultModelProfile": "fixture-zai", "modelProfiles": {"fixture-zai": {
            "model": "glm-5.3", "providers": {"opencode": "zai-coding-plan"}
        }}
    }})
}

fn fixture(root: &Path, config: &Value) -> (SweepRegistry, std::path::PathBuf, std::path::PathBuf) {
    let (registry, gh_log) = open_pr_guard_registry(root, "", 0, false);
    let defaults = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    for entry in fs::read_dir(defaults.join("roles")).unwrap().flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
            fs::copy(entry.path(), root.join(".loom/roles").join(entry.file_name())).unwrap();
        }
    }
    fs::copy(
        root.join(".loom/scripts/spawn-claude.sh"),
        root.join(".loom/scripts/spawn-codex.sh"),
    )
    .unwrap();
    for runtime in ["claude", "codex", "opencode"] {
        fs::copy(
            defaults.join(format!("runtimes/{runtime}.json")),
            root.join(format!(".loom/runtimes/{runtime}.json")),
        )
        .unwrap();
    }
    fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    fs::write(root.join(".loom/config.json"), config.to_string()).unwrap();
    let record = root.join("launch-args");
    fs::write(
        root.join(".loom/scripts/spawn-claude.sh"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$LOOM_RUNTIME\" \"$@\" > '{}'\nexit 0\n",
            record.display()
        ),
    )
    .unwrap();
    (registry, record, gh_log)
}

fn captured_options(record: &Path, runtime: &str) -> Options {
    let captured = fs::read_to_string(record).unwrap();
    let args: Vec<_> = captured.lines().collect();
    assert_eq!(args[0], runtime, "actual admitted launch runtime");
    assert!(args
        .iter()
        .any(|arg| arg.contains("/loom:sweep 8715 --claim-owned 8715")));
    Options {
        model: args
            .windows(2)
            .find(|pair| pair[0] == "--model")
            .map(|pair| pair[1].to_owned()),
        ..Options::default()
    }
}

fn assert_native_profile(options: &Options, config: &Value) {
    let selected = profiles::select("opencode", options, config)
        .expect("actual child arguments must resolve the admitted native profile");
    assert_eq!(
        (selected.provider.as_str(), selected.model.as_str()),
        ("zai-coding-plan", "glm-5.3")
    );
}

#[test]
#[serial_test::serial]
fn work_finder_fallback_launch_uses_admitted_native_profile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let _env = Environment::isolated(root);
    let config = fallback_config();
    let (registry, record, _) = fixture(root, &config);
    // A one-slot backstop also catches accidentally admitting/reserving twice.
    let registry = Arc::new(Mutex::new(registry));
    assert!(RegistryDispatcher::new(registry)
        .dispatch(8715, Some("complex"))
        .unwrap());
    let options = captured_options(&record, "opencode");
    assert_native_profile(&options, &config);
    assert_eq!(options.model, None, "an implicit native model is left to its profile");
}

#[test]
#[serial_test::serial]
fn request_fallback_and_native_first_ignore_claude_experiment() {
    for native_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        let mut config = fallback_config();
        if native_first {
            config["runtimes"]["default"] = json!("opencode");
            config["runtimes"]["preference"] = json!(["opencode", "claude"]);
        }
        let (mut registry, record, gh_log) = fixture(root, &config);
        registry
            .dispatch_with_model(
                &SweepKind::Issue(8715),
                None,
                DispatchModel::Request(None),
                None,
                None,
            )
            .unwrap();
        let options = captured_options(&record, "opencode");
        assert_native_profile(&options, &config);
        assert_eq!(options.model, None);
        assert!(
            !fs::read_to_string(gh_log).unwrap().contains("--json body"),
            "native dispatch must not fetch a Claude experiment stratum"
        );
    }
}

#[test]
#[serial_test::serial]
fn claude_launch_retains_cost_safe_defaults_aliases_and_experiment() {
    for (preference, model, experiment, expected) in [
        (false, None, false, "sonnet"),
        (true, None, false, "sonnet"),
        (false, Some("opus@xhigh"), false, "claude-opus-5@xhigh"),
        (true, None, true, ""),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        let mut config = fallback_config();
        if preference {
            // Even a configured native binding must use the admitted Claude default.
            config["runtimes"]["default"] = json!("opencode");
            config["runtimes"]["preference"] = json!(["codex", "claude"]);
        } else {
            config["runtimes"]
                .as_object_mut()
                .unwrap()
                .remove("preference");
        }
        if let Some(model) = model {
            config["autonomous"] = json!({"model": model});
        }
        let (registry, record, _) = fixture(root, &config);
        fs::write(root.join(".loom/tokens/fixture.token"), "fake-not-a-credential").unwrap();
        let expected = if experiment {
            std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
            std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
            let arm = crate::script_helpers::sweep_experiment::assign_arm(8715, Some("complex"));
            crate::script_helpers::sweep_experiment::resolved_arm_model(arm, &config)
        } else {
            expected.into()
        };
        let registry = Arc::new(Mutex::new(registry));
        assert!(RegistryDispatcher::new(registry)
            .dispatch(8715, Some("complex"))
            .unwrap());
        assert_eq!(captured_options(&record, "claude").model.as_deref(), Some(expected.as_str()));
    }
}

#[test]
#[serial_test::serial]
fn native_launch_keeps_config_and_explicit_pins_including_invalid_ones() {
    for (configured, explicit, expected, valid) in [
        ("glm-5.3", None, "glm-5.3", true),
        ("opus", None, "claude-opus-5", false),
        ("glm-5.3", Some("sonnet"), "sonnet", false),
        ("opus", Some("other/pinned-model"), "other/pinned-model", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        let mut config = fallback_config();
        config["autonomous"] = json!({"model": configured});
        let (mut registry, record, _) = fixture(root, &config);
        registry
            .dispatch_with_model(
                &SweepKind::Issue(8715),
                None,
                DispatchModel::Request(explicit),
                None,
                None,
            )
            .unwrap();
        let options = captured_options(&record, "opencode");
        assert_eq!(options.model.as_deref(), Some(expected));
        let selection = profiles::select("opencode", &options, &config);
        if valid {
            assert!(selection.is_ok(), "{selection:?}");
        } else {
            let error = selection.unwrap_err();
            assert_eq!(error.code, 78);
            assert!(error
                .message
                .contains("bare model differs from the selected profile"));
        }
    }
}

#[test]
#[serial_test::serial]
fn refused_preferences_and_explicit_runtime_never_claim_or_spawn() {
    for explicit_runtime in [None, Some("codex"), Some("claude")] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        let mut config = fallback_config();
        if explicit_runtime.is_none() {
            config["runtimes"]["preference"] = json!(["claude", "codex"]);
        }
        let (mut registry, record, gh_log) = fixture(root, &config);
        if let Some(runtime) = explicit_runtime {
            std::env::set_var("LOOM_RUNTIME", runtime);
        }
        let result = registry.dispatch_with_model(
            &SweepKind::Issue(8715),
            None,
            DispatchModel::Request(None),
            None,
            None,
        );
        // A pinned Claude runtime keeps the legacy static-admission behavior,
        // even with no pool; it must not silently fall through to OpenCode.
        if explicit_runtime == Some("claude") {
            result.unwrap();
            assert_eq!(captured_options(&record, "claude").model.as_deref(), Some("sonnet"));
        } else {
            assert!(result.is_err());
            assert!(!record.exists(), "rejected dispatch must not spawn");
            assert!(!gh_log.exists(), "rejected dispatch must not probe or flip labels");
            assert!(!registry.config().locks_dir().join("issue-8715").exists());
        }
    }
}
