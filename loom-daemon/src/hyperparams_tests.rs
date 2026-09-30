//! Coverage for the unified hyperparameters module (Issue #9683): schema
//! defaults, tier precedence (`env-vector > config > legacy > default`),
//! strict layer validation, digest stability, and the startup capture that
//! feeds the provenance stamper and the lease-TTL resolver.
//!
//! Env-var-touching tests are `#[serial]` (`serial_test`) so concurrent
//! tests cannot observe each other's `LOOM_HYPERPARAMS`. The startup-success
//! test deliberately populates the process-global vector (`OnceLock`): no
//! other test here asserts the globals are unset, so capture order across
//! the suite is harmless.

use super::*;
use serial_test::serial;
use std::path::Path;

fn write_config(dir: &Path, body: &str) {
    let loom_dir = dir.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    std::fs::write(loom_dir.join("config.json"), body).unwrap();
}

fn empty_workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// ---------------------------------------------------------------------------
// Defaults and tier precedence
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn empty_workspace_resolves_all_defaults() {
    let tmp = empty_workspace();
    let resolved = resolve_effective(tmp.path());
    assert_eq!(resolved.params, Hyperparameters::default());
    assert!(resolved
        .sources
        .values()
        .all(|source| *source == Source::Default));
}

#[test]
#[serial]
fn config_block_overrides_defaults_field_wise() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"dispatch": {"tickIntervalSecs": 30, "maxConcurrent": 6},
            "lifecycle": {"leaseTtlMinutes": 30}}}"#,
    );
    let resolved = resolve_effective(tmp.path());
    assert_eq!(resolved.params.dispatch.tick_interval_secs, 30);
    assert_eq!(resolved.params.dispatch.max_concurrent, 6);
    assert_eq!(resolved.params.lifecycle.lease_ttl_minutes, 30.0);
    assert_eq!(resolved.sources["dispatch.tickIntervalSecs"], Source::Config);
    assert_eq!(resolved.sources["dispatch.maxConcurrent"], Source::Config);
    assert_eq!(resolved.sources["lifecycle.leaseTtlMinutes"], Source::Config);
    // Untouched fields fall through to their defaults, and say so.
    assert_eq!(
        resolved.params.dispatch.max_admissions_per_tick,
        DEFAULT_MAX_ADMISSIONS_PER_TICK
    );
    assert_eq!(resolved.sources["dispatch.maxAdmissionsPerTick"], Source::Default);
}

#[test]
#[serial]
fn legacy_config_fills_absent_fields() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"intervalSecs": 120, "buildBackoff": {"high": 60, "low": 10}},
            "idleExit": {"idleMinutes": 15}}}"#,
    );
    let resolved = resolve_effective(tmp.path());
    assert_eq!(resolved.params.dispatch.tick_interval_secs, 120);
    assert_eq!(resolved.sources["dispatch.tickIntervalSecs"], Source::Legacy);
    assert_eq!(resolved.params.rework.build_backoff_high, 60);
    assert_eq!(resolved.params.rework.build_backoff_low, 10);
    assert_eq!(resolved.params.lifecycle.idle_exit_minutes, 15);
    assert_eq!(resolved.sources["lifecycle.idleExitMinutes"], Source::Legacy);
}

#[test]
#[serial]
fn config_block_beats_legacy_keys() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"dispatch": {"tickIntervalSecs": 30}},
            "autonomous": {"workFinder": {"intervalSecs": 120}}}"#,
    );
    let resolved = resolve_effective(tmp.path());
    assert_eq!(resolved.params.dispatch.tick_interval_secs, 30);
    assert_eq!(resolved.sources["dispatch.tickIntervalSecs"], Source::Config);
}

#[test]
#[serial]
fn env_vector_beats_config_block_and_normalizes_dotted_keys() {
    let tmp = empty_workspace();
    write_config(tmp.path(), r#"{"hyperparameters": {"dispatch": {"tickIntervalSecs": 30}}}"#);
    std::env::set_var(HYPERPARAMS_ENV, r#"{"dispatch.tickIntervalSecs": 45}"#);
    let resolved = resolve_effective(tmp.path());
    std::env::remove_var(HYPERPARAMS_ENV);
    assert_eq!(resolved.params.dispatch.tick_interval_secs, 45);
    assert_eq!(resolved.sources["dispatch.tickIntervalSecs"], Source::EnvVector);
}

#[test]
fn dotted_keys_normalize_to_nested_groups() {
    let normalized = normalize_dotted(&serde_json::json!({
        "dispatch.maxConcurrent": 4,
        "lifecycle.leaseTtlMinutes": 20,
    }));
    assert_eq!(normalized["dispatch"]["maxConcurrent"], 4);
    assert_eq!(normalized["lifecycle"]["leaseTtlMinutes"], 20);
}

#[test]
fn env_vector_must_be_a_json_object() {
    assert!(parse_env_vector("[1,2]").is_err());
    assert!(parse_env_vector("42").is_err());
    assert!(parse_env_vector("{}").is_ok());
}

#[test]
#[serial]
fn invalid_env_vector_is_soft_ignored_by_per_tick_readers_but_fatal_at_startup() {
    let tmp = empty_workspace();
    std::env::set_var(HYPERPARAMS_ENV, "not json");
    // Per-tick readers soft-ignore: the committed block still applies.
    let layer = overlay_from_effective(&serde_json::json!({
        "hyperparameters": {"dispatch": {"maxConcurrent": 2}}
    }));
    assert_eq!(layer["dispatch"]["maxConcurrent"], 2);
    // ...but the startup gate names the garbage and refuses to boot.
    let result = startup_init(tmp.path());
    std::env::remove_var(HYPERPARAMS_ENV);
    let problem = result.unwrap_err().to_string();
    assert!(problem.contains(HYPERPARAMS_ENV), "{problem}");
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn validate_accepts_a_well_formed_layer() {
    let violations = validate_layer(&serde_json::json!({
        "dispatch": {"tickIntervalSecs": 30, "maxConcurrent": 6, "maxAdmissionsPerTick": 2},
        "lifecycle": {"leaseTtlMinutes": 20.5, "idleExitMinutes": 90},
        "rework": {"buildBackoffHigh": 40, "buildBackoffLow": 25},
    }));
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn validate_names_unknown_groups_and_keys() {
    let violations = validate_layer(&serde_json::json!({
        "nope": {},
        "dispatch": {"bogus": 1},
    }));
    let paths: Vec<_> = violations.iter().map(|v| v.path.as_str()).collect();
    assert!(paths.contains(&"hyperparameters.nope"), "{violations:?}");
    assert!(paths.contains(&"dispatch.bogus"), "{violations:?}");
}

#[test]
fn validate_rejects_out_of_range_and_mistyped_values() {
    let violations = validate_layer(&serde_json::json!({
        "dispatch": {"tickIntervalSecs": 1, "maxConcurrent": 0},
        "lifecycle": {"leaseTtlMinutes": -5, "idleExitMinutes": 0},
        "rework": {"buildBackoffHigh": "lots"},
    }));
    let paths: Vec<_> = violations.iter().map(|v| v.path.as_str()).collect();
    for path in [
        "dispatch.tickIntervalSecs",
        "dispatch.maxConcurrent",
        "lifecycle.leaseTtlMinutes",
        "lifecycle.idleExitMinutes",
        "rework.buildBackoffHigh",
    ] {
        assert!(paths.contains(&path), "{violations:?}");
    }
}

#[test]
fn validate_rejects_a_crossed_backoff_pair() {
    let violations = validate_layer(&serde_json::json!({
        "rework": {"buildBackoffHigh": 10, "buildBackoffLow": 10},
    }));
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert_eq!(violations[0].path, "rework.buildBackoffLow");
}

#[test]
fn validate_treats_nulls_as_absent() {
    assert!(validate_layer(&Value::Null).is_empty());
    assert!(validate_layer(&serde_json::json!({"dispatch": null, "lifecycle": null})).is_empty());
}

// ---------------------------------------------------------------------------
// Digest and startup capture
// ---------------------------------------------------------------------------

#[test]
fn digest_is_stable_and_distinct() {
    let a = digest_of(&Hyperparameters::default());
    let b = digest_of(&Hyperparameters::default());
    assert_eq!(a, b);
    assert!(a.starts_with("sha256:"), "{a}");
    let mut tweaked = Hyperparameters::default();
    tweaked.dispatch.max_concurrent += 1;
    assert_ne!(a, digest_of(&tweaked));
}

#[test]
#[serial]
fn startup_init_fails_fast_naming_the_offending_path() {
    let tmp = empty_workspace();
    write_config(tmp.path(), r#"{"hyperparameters": {"dispatch": {"maxConcurrent": 0}}}"#);
    let problem = startup_init(tmp.path()).unwrap_err().to_string();
    assert!(problem.contains("dispatch.maxConcurrent"), "{problem}");
}

// The startup SUCCESS path populates process globals (resolved vector +
// digest) that outlive a test: its coverage lives in
// `tests/hyperparams_startup.rs` as its own integration-test process so the
// lib suite's default-TTL and exact-attribute-count assertions stay
// unpoisoned.

// ---------------------------------------------------------------------------
// Consumer overlays (read through each module's own reader)
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn work_finder_reader_applies_the_dispatch_overlay() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"dispatch": {"tickIntervalSecs": 45, "maxAdmissionsPerTick": 2}},
            "autonomous": {"workFinder": {"intervalSecs": 120}}}"#,
    );
    let config = crate::work_finder::read_work_finder_config(tmp.path());
    assert_eq!(config.interval_secs, Some(45));
    assert_eq!(config.max_admissions_per_tick, Some(2));
}

#[test]
#[serial]
fn work_finder_reader_soft_ignores_an_invalid_overlay_value() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"dispatch": {"tickIntervalSecs": 0}},
            "autonomous": {"workFinder": {"intervalSecs": 120}}}"#,
    );
    let config = crate::work_finder::read_work_finder_config(tmp.path());
    assert_eq!(config.interval_secs, Some(120));
}

#[test]
#[serial]
fn idle_exit_reader_applies_the_lifecycle_overlay() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"lifecycle": {"idleExitMinutes": 20}},
            "autonomous": {"idleExit": {"idleMinutes": 15}}}"#,
    );
    let config = crate::idle_exit::read_config(tmp.path());
    assert_eq!(config.idle_minutes, Some(20));
}

#[test]
#[serial]
fn build_backoff_reader_applies_the_rework_overlay() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"rework": {"buildBackoffHigh": 50}},
            "autonomous": {"workFinder": {"buildBackoff": {"high": 60, "low": 10}}}}"#,
    );
    let parsed = crate::work_finder::build_backoff::BuildBackoffConfig::read(tmp.path());
    assert_eq!(parsed.config.high, 50);
    assert_eq!(parsed.config.low, 10);
    assert_eq!(parsed.rejected_pair, None);
}

#[test]
#[serial]
fn build_backoff_reader_falls_back_on_a_crossed_final_pair() {
    let tmp = empty_workspace();
    write_config(
        tmp.path(),
        r#"{"hyperparameters": {"rework": {"buildBackoffLow": 100}},
            "autonomous": {"workFinder": {"buildBackoff": {"high": 60, "low": 10}}}}"#,
    );
    let parsed = crate::work_finder::build_backoff::BuildBackoffConfig::read(tmp.path());
    assert_eq!(parsed.config.high, DEFAULT_HIGH);
    assert_eq!(parsed.config.low, DEFAULT_LOW);
    assert_eq!(parsed.rejected_pair, Some((60, 100)));
}
