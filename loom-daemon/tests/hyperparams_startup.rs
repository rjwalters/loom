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
fn startup_init_captures_vector_digest_provenance_and_lease_ttl() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"hyperparameters": {"lifecycle": {"leaseTtlMinutes": 42}}}"#);
    startup_init(tmp.path()).unwrap();
    let resolved = resolved_global().expect("startup_init captured the vector");
    assert_eq!(resolved.params.lifecycle.lease_ttl_minutes, 42.0);
    assert_eq!(resolved.sources["lifecycle.leaseTtlMinutes"], Source::Config);
    // The lease-TTL resolver picks the layer value up (no env var set here).
    assert_eq!(lease_ttl_minutes_from_layer(), Some(42.0));
    assert_eq!(loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(), 42.0);
    // The single-knob env var still sits above the layer.
    std::env::set_var(loom_daemon::claim_reconciliation::LEASE_TTL_MINUTES_ENV, "90");
    assert_eq!(loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes(), 90.0);
    std::env::remove_var(loom_daemon::claim_reconciliation::LEASE_TTL_MINUTES_ENV);
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
