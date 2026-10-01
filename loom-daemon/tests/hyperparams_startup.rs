//! Startup-capture integration coverage for the unified hyperparameters
//! (Issue #9683): the success path of [`loom_daemon::hyperparams::startup_init`]
//! populates **process globals** (the resolved vector + its provenance
//! digest), and the lease-TTL resolver and the span provenance stamper read
//! them. Those globals are `OnceLock`s that live for the whole test binary —
//! so this coverage runs as its OWN integration-test process, isolated from
//! the lib unit tests whose suites assert default TTLs and exact span
//! attribute counts. Keep it here; do not fold it back into
//! `hyperparams_tests.rs`.

use loom_daemon::hyperparams::{
    digest_global, env_vector, lease_ttl_minutes_from_layer, resolved_global, startup_init, Source,
    HYPERPARAMS_ENV,
};
use loom_daemon::telemetry::trace::{provenance, TraceAttributes};
use serial_test::serial;

fn write_config(dir: &std::path::Path, body: &str) {
    let loom_dir = dir.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    std::fs::write(loom_dir.join("config.json"), body).unwrap();
}

#[test]
#[serial]
fn startup_init_captures_vector_digest_and_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"hyperparameters": {"lifecycle": {"leaseTtlMinutes": 42}}}"#);
    startup_init(tmp.path()).unwrap();
    let resolved = resolved_global().expect("startup_init captured the vector");
    assert_eq!(resolved.params.lifecycle.lease_ttl_minutes, 42.0);
    assert_eq!(resolved.sources["lifecycle.leaseTtlMinutes"], Source::Config);
    // And the provenance stamper records the captured digest.
    let mut attributes = TraceAttributes::new();
    provenance::stamp(&mut attributes);
    let digest = resolved.digest();
    assert_eq!(
        attributes
            .get(provenance::HYPERPARAMS_DIGEST)
            .map(String::as_str),
        Some(digest.as_str())
    );
    assert_eq!(digest_global(), Some(digest.as_str()));
}

// NOTE: this binary has exactly ONE startup_init success-path test. The
// startup globals (RESOLVED / DIGEST / ROOT) are OnceLocks — first call
// wins, later calls silently keep them — so every assertion that depends on
// them must live in that one test, in a deliberate order. A second
// "capture" test would race this one alphabetically and assert against the
// first test's root and vector.
//
// Hot-apply (#9768) is asserted here for the same reason: the lease TTL
// re-resolves from the layer anchored at the captured root, so a
// committed-block edit lands WITHOUT a daemon restart — unlike the digest,
// which intentionally stays pinned to the startup vector (a run's
// provenance is the vector it STARTED under).
#[test]
#[serial]
fn lease_ttl_hot_applies_from_a_config_edit_without_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"hyperparameters": {"lifecycle": {"leaseTtlMinutes": 42}}}"#);
    startup_init(tmp.path()).unwrap();
    assert_eq!(lease_ttl_minutes_from_layer(), Some(42.0));
    assert_eq!(loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(), 42.0);

    // Edit the committed block in place — no restart.
    write_config(tmp.path(), r#"{"hyperparameters": {"lifecycle": {"leaseTtlMinutes": 60}}}"#);
    assert_eq!(lease_ttl_minutes_from_layer(), Some(60.0), "config edit hot-applies");
    assert_eq!(loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(), 60.0);
    // The single-knob env var still sits above the hot-applied layer.
    std::env::set_var(loom_daemon::claim_reconciliation::LEASE_TTL_MINUTES_ENV, "90");
    assert_eq!(loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(), 90.0);
    std::env::remove_var(loom_daemon::claim_reconciliation::LEASE_TTL_MINUTES_ENV);

    // Removing the block falls back to the default tier, not a stale value.
    write_config(tmp.path(), r#"{}"#);
    assert_eq!(lease_ttl_minutes_from_layer(), None);
    assert_eq!(loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(), 15.0);
}

/// The `--validate` gate (#9768) runs startup's strict checks against a
/// workspace without booting one: valid layer → Ok; invalid → Err naming
/// the offending path.
#[test]
#[serial]
fn hyperparams_validate_gate_mirrors_startup() {
    let good = tempfile::tempdir().unwrap();
    write_config(good.path(), r#"{"hyperparameters": {"dispatch": {"maxConcurrent": 6}}}"#);
    let args = loom_daemon::hyperparams::HyperparamsArgs {
        json: false,
        validate: true,
        workspace: good.path().to_path_buf(),
    };
    args.run().expect("valid layer passes the gate");

    let bad = tempfile::tempdir().unwrap();
    write_config(
        bad.path(),
        r#"{"hyperparameters": {"dispatch": {"maxConcurrent": 0, "bogus": 1}}}"#,
    );
    let args = loom_daemon::hyperparams::HyperparamsArgs {
        json: false,
        validate: true,
        workspace: bad.path().to_path_buf(),
    };
    let problem = args.run().unwrap_err().to_string();
    assert!(problem.contains("dispatch.maxConcurrent"), "{problem}");
    assert!(problem.contains("dispatch.bogus"), "{problem}");
}

#[test]
#[serial]
fn startup_init_fails_loudly_on_an_unparseable_env_vector() {
    std::env::set_var(HYPERPARAMS_ENV, "not json");
    let tmp = tempfile::tempdir().unwrap();
    let result = startup_init(tmp.path());
    let problem = env_vector().err().unwrap();
    std::env::remove_var(HYPERPARAMS_ENV);
    let error = result.unwrap_err().to_string();
    assert!(error.contains(&problem), "{error}");
}
