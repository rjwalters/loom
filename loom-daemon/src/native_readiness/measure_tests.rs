//! Measurement-loop tests. The cold/warm contrast and the network-denied
//! fixture are exercised end to end against fake binaries, so the ordering,
//! short-circuiting and cache wiring are tested rather than asserted.
#![cfg(unix)]
use super::*;
use crate::native_readiness::package_cache::PackageCache;
use std::os::unix::fs::PermissionsExt;

fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake package manager that materializes a plausible `node_modules` tree,
/// and refuses to work when the network has been denied — the behaviour a real
/// resolver has on a cold cache with no registry reachable.
fn fake_npm(dir: &Path) -> PathBuf {
    executable(
        dir,
        "fake-npm",
        r#"if [ -n "${npm_config_offline}" ]; then
  echo 'registry unreachable' >&2
  exit 1
fi
mkdir -p node_modules/@opencode-ai/plugin
printf 'export const plugin = 1;\n' > node_modules/@opencode-ai/plugin/index.js
printf '{"lockfileVersion":3}' > node_modules/.package-lock.json"#,
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    scratch: PathBuf,
    home: PathBuf,
    cli: PathBuf,
    npm: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let scratch = tmp.path().join("scratch");
    let home = tmp.path().join("home");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::create_dir(&home).unwrap();
    let cli = executable(tmp.path(), "fake-opencode", "echo 1.18.31");
    let npm = fake_npm(tmp.path());
    Fixture {
        _tmp: tmp,
        scratch,
        home,
        cli,
        npm,
    }
}

fn args(fixture: &Fixture, deny_network: bool) -> ReadinessArgs {
    ReadinessArgs {
        bin: Some(fixture.cli.clone()),
        attempts: 1,
        phases: Phases::Both,
        deadline_seconds: 30,
        deny_network,
        readiness: Vec::new(),
        cache_dir: None,
        package_manager: fixture.npm.clone(),
        preflight: false,
    }
}

fn probe(fixture: &Fixture, deny_network: bool) -> Probe {
    Probe::new(
        fixture.cli.clone(),
        Duration::from_secs(30),
        if deny_network {
            NetworkMode::DeniedByEnv
        } else {
            NetworkMode::Allowed
        },
        &[],
    )
    .unwrap()
    .with_package_manager(fixture.npm.clone())
}

fn cache(fixture: &Fixture) -> PackageCache {
    PackageCache::open_under(&fixture.home.join("cache"), &fixture.home).unwrap()
}

fn measured(attempt: &AttemptReport, stage: Stage) -> bool {
    matches!(attempt.stage(stage), Some(Observation::Measured { .. }))
}

#[test]
fn the_pinned_dependency_is_read_from_the_provisioned_manifest() {
    assert_eq!(
        plugin_pin(crate::native_tools::provision::OPENCODE_PLUGIN_MANIFEST.as_bytes()),
        "@opencode-ai/plugin@1.18.31"
    );
    assert_eq!(plugin_pin(br#"{"dependencies":{"b":"2","a":"1"}}"#), "a@1,b@2");
    for unusable in [
        &b"not json"[..],
        b"{}",
        br#"{"dependencies":{}}"#,
        br#"{"dependencies":[]}"#,
    ] {
        assert_eq!(plugin_pin(unusable), "unknown");
    }
}

#[test]
fn a_cold_attempt_measures_every_observable_boundary_and_no_provider_boundary() {
    let fixture = fixture();
    let args = args(&fixture, false);
    let (attempt, version) =
        args.attempt(0, Mode::Cold, &probe(&fixture, false), &fixture.scratch, None);
    assert_eq!(version.as_deref(), Some("1.18.31"));
    assert_eq!(attempt.cache, CacheOutcome::Bypassed);
    for stage in [
        Stage::BinaryProbe,
        Stage::BindingProvision,
        Stage::PackageResolution,
        Stage::ServerSessionReady,
    ] {
        assert!(measured(&attempt, stage), "{} {attempt:?}", stage.as_str());
    }
    for stage in [
        Stage::FirstProviderEvent,
        Stage::FirstTool,
        Stage::Completion,
    ] {
        assert!(
            matches!(attempt.stage(stage), Some(Observation::Unknown { .. })),
            "{} must stay unknown",
            stage.as_str()
        );
    }
    // Every boundary appears exactly once, in traversal order.
    let order: Vec<Stage> = attempt.stages.iter().map(|s| s.stage).collect();
    assert_eq!(order, Stage::ALL.to_vec());
}

#[test]
fn a_failed_boundary_stops_the_traversal_rather_than_recording_zeroes() {
    let fixture = fixture();
    let args = args(&fixture, false);
    // A binary that is not there at all.
    let missing = Probe::new(
        fixture.scratch.join("absent"),
        Duration::from_secs(5),
        NetworkMode::Allowed,
        &[],
    )
    .unwrap();
    let (attempt, version) = args.attempt(0, Mode::Cold, &missing, &fixture.scratch, None);
    assert_eq!(version, None);
    assert!(matches!(
        attempt.stage(Stage::BinaryProbe),
        Some(Observation::Failed {
            classification: Classification::SpawnFailed,
            ..
        })
    ));
    for stage in [
        Stage::BindingProvision,
        Stage::PackageResolution,
        Stage::ServerSessionReady,
    ] {
        assert_eq!(attempt.stage(stage), Some(&Observation::NotReached), "{}", stage.as_str());
    }

    let host = HostConditions::begin();
    let phase = PhaseReport::new(Mode::Cold, host, vec![attempt]);
    let report = ReadinessReport::new(NetworkMode::Allowed, vec![], None, None, vec![phase]);
    assert!(unspawnable(&report), "an absent binary is a configuration error");
    assert!(
        report.phases[0].per_stage.is_empty(),
        "nothing was measured, so nothing is aggregated"
    );
}

#[test]
fn a_warm_attempt_publishes_then_reuses_the_keyed_entry() {
    let fixture = fixture();
    let args = args(&fixture, false);
    let cache = cache(&fixture);
    let probe = probe(&fixture, false);

    let (first, version) = args.attempt(0, Mode::Warm, &probe, &fixture.scratch, Some(&cache));
    assert_eq!(first.cache, CacheOutcome::Miss, "{first:?}");
    assert!(measured(&first, Stage::PackageResolution));

    let (second, _) = args.attempt(1, Mode::Warm, &probe, &fixture.scratch, Some(&cache));
    assert_eq!(second.cache, CacheOutcome::Hit, "{second:?}");
    assert!(measured(&second, Stage::PackageResolution));

    // The identity really is keyed on the CLI version the probe read.
    let identity = CacheIdentity::new(
        version.as_deref().unwrap(),
        &plugin_pin(crate::native_tools::provision::OPENCODE_PLUGIN_MANIFEST.as_bytes()),
        crate::native_tools::provision::OPENCODE_PLUGIN_MANIFEST.as_bytes(),
    );
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Hit);

    // A warm restore is timed as work, never as a free boundary.
    assert!(second.total_millis > 0);
}

#[test]
fn only_package_artifacts_reach_the_shared_entry_and_worker_state_never_does() {
    let fixture = fixture();
    let args = args(&fixture, false);
    let cache = cache(&fixture);
    let (attempt, version) =
        args.attempt(0, Mode::Warm, &probe(&fixture, false), &fixture.scratch, Some(&cache));
    assert_eq!(attempt.cache, CacheOutcome::Miss);
    let identity = CacheIdentity::new(
        version.as_deref().unwrap(),
        &plugin_pin(crate::native_tools::provision::OPENCODE_PLUGIN_MANIFEST.as_bytes()),
        crate::native_tools::provision::OPENCODE_PLUGIN_MANIFEST.as_bytes(),
    );
    let entry = cache.lookup(&identity).unwrap().0.expect("published");

    let mut shared: Vec<String> = std::fs::read_dir(&entry)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    shared.sort();
    assert_eq!(
        shared,
        ["loom-package-manifest.json", "node_modules"],
        "only the named package artifacts (plus the integrity record) are shared"
    );
    // The guarded bindings — plugin source and the per-launch manifest — are
    // provisioned per attempt and stay out of the shared entry.
    assert!(!entry.join("plugins").exists());
    assert!(!entry.join("package.json").exists());
    assert_eq!(SHARED_ARTIFACTS, ["node_modules"]);
}

#[test]
fn a_network_denied_fixture_shows_which_boundary_needs_the_network() {
    let fixture = fixture();
    let args = args(&fixture, true);
    let denied = probe(&fixture, true);

    // Cold + denied: the binary probe and binding provisioning still succeed;
    // package resolution is the boundary that fails.
    let (cold, _) = args.attempt(0, Mode::Cold, &denied, &fixture.scratch, None);
    assert!(measured(&cold, Stage::BinaryProbe));
    assert!(measured(&cold, Stage::BindingProvision));
    assert!(matches!(
        cold.stage(Stage::PackageResolution),
        Some(Observation::Failed {
            classification: Classification::NonzeroExit,
            ..
        })
    ));
    assert_eq!(cold.stage(Stage::ServerSessionReady), Some(&Observation::NotReached));

    // Populate the cache with the network allowed, then re-run denied: the same
    // boundary now succeeds from the cache, with no registry reachable.
    let cache = cache(&fixture);
    let (warmed, _) =
        args.attempt(0, Mode::Warm, &probe(&fixture, false), &fixture.scratch, Some(&cache));
    assert_eq!(warmed.cache, CacheOutcome::Miss);

    let (warm, _) = args.attempt(1, Mode::Warm, &denied, &fixture.scratch, Some(&cache));
    assert_eq!(warm.cache, CacheOutcome::Hit);
    assert!(
        measured(&warm, Stage::PackageResolution),
        "a cache hit must not need the network: {warm:?}"
    );
    assert!(measured(&warm, Stage::ServerSessionReady));
}

#[test]
fn an_unreadable_version_is_never_keyed_against_a_cached_entry() {
    let fixture = fixture();
    let mut args = args(&fixture, false);
    let cache = cache(&fixture);
    // Populate the cache from a probe that CAN read a version.
    let _ = args.attempt(0, Mode::Warm, &probe(&fixture, false), &fixture.scratch, Some(&cache));

    // A CLI whose version cannot be parsed stops at the binary probe rather
    // than adopting an entry keyed on a version it never confirmed.
    let mute = executable(&fixture.scratch, "mute", "exit 0");
    args.bin = Some(mute.clone());
    let silent = Probe::new(mute, Duration::from_secs(30), NetworkMode::Allowed, &[])
        .unwrap()
        .with_package_manager(fixture.npm.clone());
    let (attempt, version) = args.attempt(0, Mode::Warm, &silent, &fixture.scratch, Some(&cache));
    assert_eq!(version, None);
    assert!(matches!(
        attempt.stage(Stage::BinaryProbe),
        Some(Observation::Failed {
            classification: Classification::UnparsableVersion,
            ..
        })
    ));
    assert_eq!(attempt.stage(Stage::PackageResolution), Some(&Observation::NotReached));
}

#[test]
fn a_two_phase_report_discloses_comparability_and_claims_no_improvement() {
    let fixture = fixture();
    let args = args(&fixture, false);
    let cache = cache(&fixture);
    let probe = probe(&fixture, false);
    let mut phases = Vec::new();
    for (mode, cache) in [(Mode::Cold, None), (Mode::Warm, Some(&cache))] {
        let mut host = HostConditions::begin();
        let attempts = (0..2)
            .map(|i| args.attempt(i, mode, &probe, &fixture.scratch, cache).0)
            .collect();
        host.finish();
        phases.push(PhaseReport::new(mode, host, attempts));
    }
    let report = ReadinessReport::new(
        NetworkMode::Allowed,
        probe.readiness_argv().to_vec(),
        Some("1.18.31".into()),
        Some("/home/fixture/cache".into()),
        phases,
    );
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["model_calls"], 0);
    assert_eq!(value["paid_retry"], false);
    assert_eq!(value["forge_contact"], false);
    assert_eq!(value["speedup_claimed"], false);
    assert_eq!(value["plugin_load_proven"], serde_json::Value::Null);
    assert_eq!(value["readiness_command"], serde_json::json!(["--version"]));
    assert_eq!(value["phases"].as_array().unwrap().len(), 2);
    assert_eq!(value["unknown_boundaries"].as_array().unwrap().len(), 3);
    // Comparability is reported as a measured fact (or as unknown), never as
    // an assumption baked into the numbers.
    assert!(value.get("host_conditions_comparable").is_some());
    assert!(value["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("no speedup is claimed")));
    // Both phases measured every observable boundary and aggregated only those.
    for phase in value["phases"].as_array().unwrap() {
        let stages: Vec<&str> = phase["per_stage"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["stage"].as_str().unwrap())
            .collect();
        assert_eq!(
            stages,
            [
                "binary_probe",
                "binding_provision",
                "package_resolution",
                "server_session_ready"
            ]
        );
        assert!(phase["host"]["logical_cpus"].as_u64().unwrap() > 0);
    }
}

#[test]
fn preflight_warming_reports_startup_cost_with_no_model_execution() {
    let fixture = fixture();
    let mut args = args(&fixture, false);
    args.preflight = true;
    let cache = cache(&fixture);
    // Exercised through the same attempt path the phases use.
    let (attempt, version) =
        args.attempt(0, Mode::Warm, &probe(&fixture, false), &fixture.scratch, Some(&cache));
    assert_eq!(attempt.cache, CacheOutcome::Miss);
    assert_eq!(version.as_deref(), Some("1.18.31"));
    // No provider boundary is ever measured by a warming pass.
    for stage in [
        Stage::FirstProviderEvent,
        Stage::FirstTool,
        Stage::Completion,
    ] {
        assert!(matches!(attempt.stage(stage), Some(Observation::Unknown { .. })));
    }
    assert!(args
        .preflight(&probe(&fixture, false), &fixture.scratch, Some(&cache))
        .is_ok());
}
