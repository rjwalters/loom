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
        ..PolicySources::default()
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
        ..PolicySources::default()
    };
    assert!(matches!(egress_pub::stance_from(&sources), Stance::Required { .. }));
    assert_eq!(egress_pub::stance_from(&PolicySources::default()), Stance::Unconfigured);
}

#[test]
fn gateway_owned_preflight_holds_no_token() {
    let p = gateway_owned_preflight();
    assert!(p.minted.is_none());
    assert_eq!(p.report.mechanism, "gateway");
}

/// [`policy_sources`] with the document moved to the repo tier
/// (`forge.egress.policyPath`, i.e. a committed file).
fn repo_sources(ws: &Path, api: &str) -> PolicySources {
    let env = policy_sources(ws, api, &[]);
    PolicySources {
        env_path: None,
        machine_path: None,
        repo_path: env.env_path,
        ..PolicySources::default()
    }
}

#[test]
fn repo_origin_observe_policy_never_emits_api_host() {
    // #9986 review item 1: a committed policy must not decide where the minted
    // token goes. It falls back to the legacy token-only shape.
    let ws = tempfile::tempdir().unwrap();
    let mut data: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/forge-egress/policy.example.json"))
            .unwrap();
    data["github"]["apiOrigin"] = "https://attacker.example".into();
    let path = ws.path().join("committed-policy.json");
    std::fs::write(&path, data.to_string()).unwrap();
    let sources = PolicySources {
        env_path: None,
        machine_path: None,
        repo_path: Some(path),
        ..PolicySources::default()
    };
    let stance = egress_pub::stance_from(&sources);
    assert_eq!(stance, Stance::Unconfigured);

    let dir = profile_dir(ws.path());
    publish_hosts_with_stance(&dir, "ghs_minted", &stance, Some(ws.path()), &clean).unwrap();
    let hosts = std::fs::read_to_string(dir.join("hosts.yml")).unwrap();
    assert!(!hosts.contains("api_host"), "{hosts}");
    assert!(!hosts.contains("attacker.example"), "{hosts}");
    assert_eq!(hosts, render_hosts_yaml("ghs_minted", &Stance::Unconfigured));
}

#[test]
fn repo_origin_required_policy_is_still_honoured() {
    // Removing credentials is safe from any origin.
    let ws = tempfile::tempdir().unwrap();
    assert!(matches!(
        egress_pub::stance_from(&repo_sources(ws.path(), "required")),
        Stance::Required { .. }
    ));
}

#[test]
fn schema_invalid_observe_policy_is_never_actuated_even_from_a_trusted_origin() {
    let ws = tempfile::tempdir().unwrap();
    let mut data: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/forge-egress/policy.example.json"))
            .unwrap();
    data["github"]["apiOrigin"] = "https://evil.example/path?x=1".into();
    let path = ws.path().join("policy.json");
    std::fs::write(&path, data.to_string()).unwrap();
    let sources = PolicySources {
        env_path: Some(path),
        machine_path: None,
        repo_path: None,
        ..PolicySources::default()
    };
    assert!(!crate::forge_egress::policy::assert_policy_shape(&data).is_empty());
    assert_eq!(egress_pub::stance_from(&sources), Stance::Unconfigured);
}

#[test]
fn findings_for_dir_matches_whole_paths_not_prefixes() {
    // #9986 review item 2: `.loom/gh-config` is a string prefix of
    // `.loom/gh-config-by-owner/<o>`, and `acme` of `acme-corp`.
    let primary = Path::new("/w/.loom/gh-config");
    let owner = Path::new("/w/.loom/gh-config-by-owner/acme");
    let findings = vec![
        Finding::new("apiconfig.api-host-missing", "x")
            .observed("/w/.loom/gh-config-by-owner/o: entry has no api_host"),
        Finding::new("apiconfig.api-host-missing", "x")
            .observed("/w/.loom/gh-config-by-owner/acme-corp: entry has no api_host"),
        Finding::new("apiconfig.shadowed-profile", "x").observed("/w/.loom/gh-config-other"),
    ];
    assert!(egress_pub::findings_for_dir(&findings, primary).is_empty());
    assert!(egress_pub::findings_for_dir(&findings, owner).is_empty());

    let own = vec![
        Finding::new("apiconfig.api-host-missing", "x")
            .observed("/w/.loom/gh-config: entry has no api_host"),
        Finding::new("apiconfig.shadowed-profile", "x").observed("/w/.loom/gh-config"),
    ];
    assert_eq!(egress_pub::findings_for_dir(&own, primary).len(), 2);
}

#[test]
fn sibling_per_owner_finding_does_not_roll_back_the_primary_under_the_real_validator() {
    // A per-owner dir written token-only before #9986 has no api_host. That is
    // a finding about the per-owner dir, not about the primary.
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    let owner = ws.path().join(".loom/gh-config-by-owner/o");
    publish_hosts_with_stance(&owner, "ghs_old", &Stance::Unconfigured, None, &clean).unwrap();
    let sources = policy_sources(ws.path(), "observe", &[dir.clone(), owner.clone()]);
    let stance = egress_pub::stance_from(&sources);
    assert!(matches!(stance, Stance::Observe { .. }), "{stance:?}");
    let real = |w: &Path| {
        crate::forge_egress::run_with(&sources, w, crate::forge_egress::Mode::Assert).routing
    };
    let shown = owner.display().to_string();
    assert!(
        real(ws.path())
            .iter()
            .any(|f| f.code == "apiconfig.api-host-missing" && f.observed.starts_with(&shown)),
        "the sibling must actually carry a finding for this test to mean anything"
    );
    publish_hosts_with_stance(&dir, "ghs_new", &stance, Some(ws.path()), &real).unwrap();
    assert!(egress_pub::read_rollback(ws.path()).is_none());
    let hosts = std::fs::read_to_string(dir.join("hosts.yml")).unwrap();
    assert!(
        hosts.contains("oauth_token: ghs_new") && hosts.contains(&format!("api_host: {PROXY}"))
    );
}

#[test]
fn required_startup_scrubs_pre_existing_per_owner_tokens_and_assert_is_clean() {
    // #9986 review item 3: a host moving to `required` must not keep the
    // per-owner tokens Loom published earlier (bypass + permanent refusal).
    let ws = tempfile::tempdir().unwrap();
    let dir = profile_dir(ws.path());
    let owners = [
        ws.path().join(".loom/gh-config-by-owner/acme"),
        ws.path().join(".loom/gh-config-by-owner/acme-corp"),
    ];
    publish_hosts_with_stance(&dir, "ghs_primary", &Stance::Unconfigured, None, &clean).unwrap();
    for o in &owners {
        publish_hosts_with_stance(o, "ghs_owner", &Stance::Unconfigured, None, &clean).unwrap();
    }
    let mut all = vec![dir.clone()];
    all.extend(owners.iter().cloned());
    let sources = policy_sources(ws.path(), "required", &all);
    // Only Loom's own profiles: the host's ambient `~/.config/gh` is also
    // enumerated and may legitimately hold a token on a dev machine.
    let ours = ws.path().display().to_string();
    let token_findings = || {
        crate::forge_egress::run_with(&sources, ws.path(), crate::forge_egress::Mode::Assert)
            .routing
            .into_iter()
            .filter(|f| f.code == "apiconfig.github-token-present" && f.observed.starts_with(&ours))
            .count()
    };
    assert_eq!(token_findings(), 3, "precondition: every profile holds a token");
    let side = crate::forge_identity::Sidecar {
        app_id: "1".into(),
        installation_id: "2".into(),
        ..Default::default()
    };
    for d in &all {
        crate::forge_identity::sidecar::write_sidecar(d, &side).unwrap();
    }

    egress_pub::publish_tokenless_everywhere(&dir, "github.com").unwrap();

    for d in &all {
        let hosts = std::fs::read_to_string(d.join("hosts.yml")).unwrap();
        assert_eq!(hosts, "github.com:\n    git_protocol: https\n", "{}", d.display());
        assert!(!crate::forge_egress::probe::profile_holds_token(d));
        // #10571: a token-less profile names no minted identity.
        assert!(crate::forge_identity::read_sidecar(d).is_none(), "{}", d.display());
    }
    assert_eq!(token_findings(), 0, "assert must be clean after the scrub");
}
