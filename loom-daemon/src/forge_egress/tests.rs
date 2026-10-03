#![allow(clippy::unwrap_used)]

use super::checks::{GhBuild, Profile, ProfileSource};
use super::gate::{admission_for, drift_payload, Admission};
use super::policy::Origin;
use super::*;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn example() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/forge-egress/policy.example.json"))
        .unwrap()
}

fn doc(data: Value) -> PolicyDoc {
    PolicyDoc {
        data,
        path: PathBuf::from("/etc/loom/forge-egress/policy.json"),
        origin: Origin::Machine,
        ignored: vec![],
    }
}

/// An observation that is aligned with the example policy.
fn aligned() -> Observed {
    Observed {
        gh: GhBuild {
            path: Some(PathBuf::from("/usr/local/bin/gh")),
            version: Some((2, 102, 0)),
            raw: "gh version 2.102.0 (2026-09-30)".into(),
        },
        launcher_exists: true,
        profiles: vec![Profile {
            path: PathBuf::from("/home/u/.config/gh"),
            source: ProfileSource::Default,
            api_host: ApiHost::Present("github-proxy.2amlogic.com".into()),
        }],
        loom_otlp_exporter: true,
        ..Observed::default()
    }
}

#[test]
fn unconfigured_is_exit_zero_with_the_documented_json() {
    let r = run_with(&PolicySources::default(), Path::new("/nonexistent"), Mode::Doctor);
    assert!(!r.is_configured());
    let j = r.to_json();
    assert_eq!(j["policy"]["origin"], "unconfigured");
    assert_eq!(j["exit_code"], 0);
    for s in ["routing", "git", "runtime", "telemetry"] {
        assert_eq!(j[s]["exit_code"], 0, "{s}");
    }
    assert_eq!(admission_for(&r), Admission::Unconfigured);
}

#[test]
fn aligned_routing_with_incomplete_side_sections_still_exits_zero() {
    let r = evaluate(&doc(example()), &aligned(), Mode::Doctor);
    assert_eq!(r.exit_code(), 0, "{:?}", r.routing);
    let j = r.to_json();
    assert_eq!(j["git"]["exit_code"], 2, "git.unqualified is incomplete, not a finding");
    assert_eq!(j["runtime"]["exit_code"], 2);
    assert_eq!(j["telemetry"]["exit_code"], 0);
    assert_eq!(j["exit_code"], j["routing"]["exit_code"], "the verdict is the routing one");
    assert_eq!(j["policy"]["enforcement"], "observe");
    assert_eq!(j["observed"]["apiHostHonoured"], true);
    for key in [
        "schemaVersion",
        "policy",
        "observed",
        "routing",
        "git",
        "runtime",
        "telemetry",
        "exit_code",
    ] {
        assert!(j.get(key).is_some(), "2am report key {key} missing");
    }
}

#[test]
fn assert_mode_carries_routing_only() {
    let mut obs = aligned();
    obs.gh_host = Some("github-proxy.2amlogic.com".into());
    let r = evaluate(&doc(example()), &obs, Mode::Assert);
    assert_eq!(r.routing_codes(), vec!["ghhost.points-at-gateway".to_string()]);
    assert!(r.git.is_empty() && r.runtime.is_empty() && r.telemetry.is_empty());
    assert!(r.to_json().get("observed").is_none());
}

#[test]
fn required_refuses_observe_admits_and_unreadable_fails_closed() {
    let mut required = example();
    required["enforcement"]["api"] = json!("required");
    let mut obs = aligned();
    obs.gh.version = Some((2, 97, 0));
    match admission_for(&evaluate(&doc(required.clone()), &obs, Mode::Assert)) {
        Admission::Refused(r) => {
            assert_eq!(r.codes, vec!["toolchain.below-api-host-floor".to_string()]);
            assert_eq!(r.exit_code, 1);
            assert!(r.message().contains("loom-daemon forge egress doctor"));
            let payload = r.event_payload(json!({"type": "Issue", "value": 7}));
            assert_eq!(payload["reason"], "forge-egress");
            assert_eq!(payload["source"], "forge-egress");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        admission_for(&evaluate(&doc(example()), &obs, Mode::Assert)),
        Admission::Observed(vec!["toolchain.below-api-host-floor".to_string()])
    );
    assert_eq!(
        admission_for(&evaluate(&doc(required), &aligned(), Mode::Assert)),
        Admission::Aligned
    );
    let dir = tempfile::tempdir().unwrap();
    let unreadable = PolicySources {
        env_path: Some(dir.path().join("missing.json")),
        ..PolicySources::default()
    };
    let r = run_with(&unreadable, dir.path(), Mode::Assert);
    assert_eq!(r.exit_code(), 2);
    assert!(
        matches!(admission_for(&r), Admission::Refused(_)),
        "never observe-only when unreadable"
    );
    // A schema-invalid enforcement value is not "observe" either.
    let mut weird = example();
    weird["enforcement"]["api"] = json!("optional");
    assert!(matches!(
        admission_for(&evaluate(&doc(weird), &aligned(), Mode::Assert)),
        Admission::Refused(_)
    ));
}

#[test]
fn drift_fires_on_change_only() {
    let mut obs = aligned();
    let clean = evaluate(&doc(example()), &obs, Mode::Doctor);
    assert!(drift_payload(None, &clean).is_none(), "a clean first pass is not drift");
    obs.gh.version = Some((2, 97, 0));
    let bad = evaluate(&doc(example()), &obs, Mode::Doctor);
    let payload = drift_payload(None, &bad).unwrap();
    assert_eq!(payload["codes"], json!(["toolchain.below-api-host-floor"]));
    let same: BTreeSet<String> = bad.routing_codes().into_iter().collect();
    assert!(drift_payload(Some(&same), &bad).is_none());
    assert!(drift_payload(Some(&same), &clean).is_some(), "recovery is drift too");
}

#[test]
fn cache_round_trips_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forge-egress-doctor.json");
    let r = evaluate(&doc(example()), &aligned(), Mode::Doctor);
    gate::write_cache(&path, dir.path(), &r).unwrap();
    let back = gate::read_cache(&path).unwrap();
    assert_eq!(back["report"], r.to_json());
    assert_eq!(back["daemon_pid"], std::process::id());
}

#[test]
fn repo_origin_policies_never_run_the_canary() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let mut p = example();
    p["enforcement"]["negativeCanary"] = json!(format!("touch {}", marker.display()));
    let path = dir.path().join("policy.json");
    std::fs::write(&path, p.to_string()).unwrap();
    let sources = PolicySources {
        repo_path: Some(path),
        ..PolicySources::default()
    };
    let r = run_with(&sources, dir.path(), Mode::Doctor);
    assert!(!marker.exists(), "a repo-local policy made the validator run a command");
    assert!(report::codes(&r.runtime).contains(&"runtime.unverifiable"));
}
