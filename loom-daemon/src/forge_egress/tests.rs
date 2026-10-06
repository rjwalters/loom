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
        path_gh: Some(PathBuf::from("/usr/local/bin/gh")),
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
fn unconfigured_is_exit_zero_with_a_visible_notice() {
    let r = run_with(&PolicySources::default(), Path::new("/nonexistent"), Mode::Doctor);
    assert!(!r.is_configured());
    let j = r.to_json();
    assert_eq!(j["policy"]["origin"], "unconfigured");
    assert_eq!(j["exit_code"], 0);
    assert_eq!(r.routing_codes(), ["policy.unconfigured"]);
    assert_eq!(j["routing"]["findings"][0]["code"], "policy.unconfigured");
    assert_eq!(j["routing"]["findings"][0]["severity"], "notice");
    for s in ["routing", "git", "runtime", "telemetry"] {
        assert_eq!(j[s]["exit_code"], 0, "{s}");
    }
    assert_eq!(admission_for(&r), Admission::Unconfigured);
}

#[test]
fn unconfigured_on_a_managed_host_is_exit_two() {
    let sources = PolicySources {
        managed: true,
        ..PolicySources::default()
    };
    let r = run_with(&sources, Path::new("/nonexistent"), Mode::Doctor);
    assert!(!r.is_configured());
    assert_eq!(r.exit_code(), 2);
    let j = r.to_json();
    assert_eq!(j["exit_code"], 2);
    assert_eq!(j["policy"]["origin"], "unconfigured");
    assert_eq!(j["routing"]["findings"][0]["code"], "policy.unconfigured");
    assert_eq!(j["routing"]["findings"][0]["severity"], "incomplete");
    // The daemon gate still admits: an unconfigured host is never refused.
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

// ---- #9986: no GitHub credential on `required` hosts ----

fn with_api(mode: &str) -> PolicyDoc {
    let mut p = example();
    p["enforcement"]["api"] = mode.into();
    doc(p)
}

#[test]
fn token_in_an_enumerated_profile_is_a_finding_only_under_required() {
    let mut obs = aligned();
    obs.token_profiles = vec![PathBuf::from("/w/.loom/gh-config")];
    let required = evaluate(&with_api("required"), &obs, Mode::Assert);
    assert!(required
        .routing_codes()
        .contains(&"apiconfig.github-token-present".to_string()));
    let observe = evaluate(&with_api("observe"), &obs, Mode::Assert);
    assert!(!observe
        .routing_codes()
        .contains(&"apiconfig.github-token-present".to_string()));
}

#[test]
fn gh_git_credential_helper_is_a_git_finding_only_under_required() {
    let mut obs = aligned();
    obs.git_helper_is_gh = true;
    let required = evaluate(&with_api("required"), &obs, Mode::Doctor);
    assert!(required
        .git
        .iter()
        .any(|f| f.code == "git.credential-from-api-profile"));
    let observe = evaluate(&with_api("observe"), &obs, Mode::Doctor);
    assert!(!observe
        .git
        .iter()
        .any(|f| f.code == "git.credential-from-api-profile"));
    // The routing verdict (process exit code) is not affected by the git finding.
    assert!(!required
        .routing_codes()
        .contains(&"git.credential-from-api-profile".to_string()));
    obs.git_helper_is_gh = false;
    assert!(evaluate(&with_api("required"), &obs, Mode::Doctor)
        .git
        .iter()
        .all(|f| f.code != "git.credential-from-api-profile"));
}

#[test]
fn hosts_token_detection_reads_verdict_only() {
    use super::probe::hosts_text_has_token;
    assert!(hosts_text_has_token("github.com:\n    oauth_token: ghs_x\n"));
    assert!(!hosts_text_has_token("github.com:\n    git_protocol: https\n"));
    assert!(!hosts_text_has_token("github.com:\n    oauth_token: \"\"\n"));
}

/// #9995: the version floor reads the exec target; launcher-not-first reads
/// PATH's `gh`, even when the exec target is the launcher itself.
#[test]
fn launcher_not_first_and_the_floor_measure_different_gh() {
    let launcher = PathBuf::from("/usr/local/bin/gh"); // example launcherPath
    let mut obs = aligned();
    obs.gh.path = Some(launcher.clone());
    obs.path_gh = Some(PathBuf::from("/opt/unmanaged/bin/gh"));
    let r = evaluate(&doc(example()), &obs, Mode::Doctor);
    assert_eq!(r.routing_codes(), vec!["toolchain.launcher-not-first".to_string()]);
    assert_eq!(
        r.to_json()["observed"]["pathGhPath"],
        "/opt/unmanaged/bin/gh",
        "the report shows both"
    );

    // PATH is right but the exec target is below the floor: only the floor.
    let mut obs = aligned();
    obs.path_gh = Some(launcher);
    obs.gh.version = Some((2, 97, 0));
    let r = evaluate(&doc(example()), &obs, Mode::Doctor);
    assert_eq!(r.routing_codes(), vec!["toolchain.below-api-host-floor".to_string()]);
}

/// #9995 review: an exec target that is neither the existing launcher nor
/// PATH's `gh` (`LOOM_GH_BIN` with the policy rung declined) is a routing
/// finding, even though PATH is correct and the version clears the floor.
#[test]
fn exec_target_off_the_launcher_and_off_path_is_policy_launcher_declined() {
    let code = "toolchain.policy-launcher-declined".to_string();
    let mut obs = aligned();
    obs.gh.path = Some(PathBuf::from("/opt/unmanaged/bin/gh"));
    obs.gh_source = Some("env_override");
    let r = evaluate(&doc(example()), &obs, Mode::Assert);
    assert_eq!(r.routing_codes(), vec![code.clone()]);
    assert!(super::checks::LOOM_ONLY_CODES.contains(&code.as_str()));

    // Exec target == PATH's gh: launcher-not-first alone covers it.
    obs.path_gh = obs.gh.path.clone();
    let r = evaluate(&doc(example()), &obs, Mode::Assert);
    assert_eq!(r.routing_codes(), vec!["toolchain.launcher-not-first".to_string()]);

    // Launcher absent on this host: the rung could not have won; no finding.
    let mut obs = aligned();
    obs.gh.path = Some(PathBuf::from("/opt/unmanaged/bin/gh"));
    obs.launcher_exists = false;
    let r = evaluate(&doc(example()), &obs, Mode::Assert);
    assert!(!r.routing_codes().contains(&code), "{:?}", r.routing_codes());
}
