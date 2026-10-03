//! #9986 tests: routing-preserving / no-credential `hosts.yml` publication.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use super::{gateway_owned_preflight, publish_hosts_with_stance};
use crate::forge_egress::policy::PolicySources;
use crate::forge_egress::publication::render_hosts_yaml;
use crate::forge_egress::publication::{self as egress_pub, Stance};
use crate::forge_egress::report::Finding;

const PROXY: &str = "github-proxy.2amlogic.com";

fn observe() -> Stance {
    Stance::Observe {
        logical: "github.com".into(),
        api_host: PROXY.into(),
    }
}

fn required() -> Stance {
    Stance::Required {
        logical: "github.com".into(),
    }
}

fn clean(_: &Path) -> Vec<Finding> {
    vec![]
}

fn profile_dir(ws: &Path) -> PathBuf {
    ws.join(".loom").join("gh-config")
}

fn policy_sources(ws: &Path, api: &str, dirs: &[PathBuf]) -> PolicySources {
    let mut data: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/forge-egress/policy.example.json"))
            .unwrap();
    data["enforcement"]["api"] = api.into();
    data["principal"]["ghConfigDirs"] = serde_json::json!(dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>());
    let policy_path = ws.join("policy.json");
    std::fs::write(&policy_path, data.to_string()).unwrap();
    PolicySources {
        env_path: Some(policy_path),
        machine_path: None,
        repo_path: None,
    }
}

#[test]
fn no_policy_publication_is_byte_identical_and_never_asserts() {
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    let panic_assert = |_: &Path| -> Vec<Finding> { panic!("assert must not run") };
    publish_hosts_with_stance(&dir, "ghs_a", &Stance::Unconfigured, Some(ws.path()), &panic_assert)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("hosts.yml")).unwrap(),
        render_hosts_yaml("ghs_a", &Stance::Unconfigured)
    );
}

#[test]
fn observe_publication_keeps_api_host_across_token_refresh() {
    // Scenario 17: a refresh must not drop api_host.
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    for token in ["ghs_1", "ghs_2"] {
        publish_hosts_with_stance(&dir, token, &observe(), Some(ws.path()), &clean).unwrap();
        let hosts = std::fs::read_to_string(dir.join("hosts.yml")).unwrap();
        assert!(hosts.contains(&format!("oauth_token: {token}")));
        assert!(hosts.contains(&format!("api_host: {PROXY}")));
    }
}

#[test]
fn observe_publication_raises_no_finding_for_its_own_dir_under_the_real_validator() {
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    let sources = policy_sources(ws.path(), "observe", std::slice::from_ref(&dir));
    let stance = egress_pub::stance_from(&sources);
    let Stance::Observe { api_host, .. } = &stance else {
        panic!("expected observe, got {stance:?}");
    };
    assert!(!api_host.is_empty());
    let real = |w: &Path| {
        crate::forge_egress::run_with(&sources, w, crate::forge_egress::Mode::Assert).routing
    };
    publish_hosts_with_stance(&dir, "ghs_x", &stance, Some(ws.path()), &real).unwrap();
    assert!(egress_pub::read_rollback(ws.path()).is_none());
    assert!(dir.join("hosts.yml").exists());
}

#[test]
fn token_only_profile_is_caught_by_the_real_validator_and_rolled_back() {
    // A stale token-only profile (legacy shape) fails the real assert;
    // publishing it under an observe stance must be rolled back.
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    let sources = policy_sources(ws.path(), "observe", std::slice::from_ref(&dir));
    let real = |w: &Path| {
        crate::forge_egress::run_with(&sources, w, crate::forge_egress::Mode::Assert).routing
    };
    // Simulate a regression: observe stance with no api_host to render.
    let broken = Stance::Observe {
        logical: "github.com".into(),
        api_host: String::new(),
    };
    let err =
        publish_hosts_with_stance(&dir, "ghs_x", &broken, Some(ws.path()), &real).unwrap_err();
    assert!(err.to_string().contains("apiconfig.api-host-missing"));
    assert!(!dir.join("hosts.yml").exists());
    assert!(egress_pub::read_rollback(ws.path()).is_some());
}

#[test]
fn failing_assert_rolls_back_to_the_previous_profile_and_records_it() {
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    publish_hosts_with_stance(&dir, "ghs_old", &Stance::Unconfigured, Some(ws.path()), &clean)
        .unwrap();
    let before = std::fs::read(dir.join("hosts.yml")).unwrap();

    let shown = dir.display().to_string();
    let failing = move |_: &Path| {
        vec![Finding::new("apiconfig.api-host-mismatch", "x")
            .observed(format!("{shown}: api_host evil"))]
    };
    let err = publish_hosts_with_stance(&dir, "ghs_new", &observe(), Some(ws.path()), &failing)
        .unwrap_err();
    assert!(err.to_string().contains("apiconfig.api-host-mismatch"));
    assert_eq!(std::fs::read(dir.join("hosts.yml")).unwrap(), before);
    assert!(!dir.join("hosts.yml.tmp").exists());
    let rec = egress_pub::read_rollback(ws.path()).unwrap();
    assert_eq!(rec["codes"][0], "apiconfig.api-host-mismatch");
    assert!(!rec.to_string().contains("ghs_"));

    // A later clean publication clears the record.
    publish_hosts_with_stance(&dir, "ghs_new", &observe(), Some(ws.path()), &clean).unwrap();
    assert!(egress_pub::read_rollback(ws.path()).is_none());
}

#[test]
fn findings_about_other_profiles_do_not_trigger_a_rollback() {
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    let other = |_: &Path| {
        vec![Finding::new("apiconfig.api-host-missing", "x").observed("/home/u/.config/gh: x")]
    };
    publish_hosts_with_stance(&dir, "ghs_new", &observe(), Some(ws.path()), &other).unwrap();
    assert!(dir.join("hosts.yml").exists());
}

#[test]
fn required_publishes_no_token_and_never_asserts() {
    let ws = tempfile::tempdir().unwrap();
    let dir = ws.path().join(".loom/gh-config-by-owner/2AMLogic");
    let panic_assert = |_: &Path| -> Vec<Finding> { panic!("assert must not run") };
    publish_hosts_with_stance(&dir, "ghs_secret", &required(), Some(ws.path()), &panic_assert)
        .unwrap();
    let hosts = std::fs::read_to_string(dir.join("hosts.yml")).unwrap();
    assert_eq!(hosts, "github.com:\n    git_protocol: https\n");
    assert!(!hosts.contains("ghs_secret") && !hosts.contains("oauth_token"));
}

#[test]
fn required_stance_resolves_from_policy_and_unreadable_fails_closed() {
    let ws = tempfile::tempdir().unwrap();
    let sources = policy_sources(ws.path(), "required", &[]);
    assert!(matches!(egress_pub::stance_from(&sources), Stance::Required { .. }));
    let bad = ws.path().join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    let sources = PolicySources {
        env_path: Some(bad),
        machine_path: None,
        repo_path: None,
    };
    assert!(matches!(egress_pub::stance_from(&sources), Stance::Required { .. }));
    assert_eq!(egress_pub::stance_from(&PolicySources::default()), Stance::Unconfigured);
}

#[test]
fn gateway_owned_preflight_holds_no_token() {
    let p = gateway_owned_preflight();
    assert!(p.minted_gh_token.is_none());
    assert_eq!(p.report.mechanism, "gateway");
}
