//! Credential pool classification (#9872).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::os::unix::fs::PermissionsExt;

/// A token-shaped value no output may ever contain (assembled at runtime so
/// the source carries no credential-shaped literal).
fn secret() -> String {
    format!("{}_{}", "ghp", "SECRETvalueNEVERprinted".repeat(2))
}

fn report(mechanism: &str) -> CredentialPreflightReport {
    CredentialPreflightReport::new(true, mechanism, None, "ok".to_string(), Utc::now())
}

fn user_ok() -> UserLookup {
    Ok(("turian".to_string(), Some(65918)))
}

#[test]
fn a_github_app_report_is_an_installation_pool_without_a_lookup() {
    let pool = classify(&report("github-app"), None, || panic!("no lookup for an App")).unwrap();
    assert_eq!(pool.kind, KIND_INSTALLATION);
    assert!(warning(&pool).is_none());
}

#[test]
fn an_env_installation_token_is_an_installation_pool() {
    let pool = classify(&report("GH_TOKEN"), Some("ghs_abc123"), || panic!("no lookup")).unwrap();
    assert_eq!(pool.kind, KIND_INSTALLATION);
}

#[test]
fn a_personal_token_is_a_user_pool_with_login_and_id_and_warns() {
    let pool = classify(&report("GITHUB_TOKEN"), Some(secret().as_str()), user_ok).unwrap();
    assert_eq!(pool.kind, KIND_USER);
    assert_eq!(pool.login.as_deref(), Some("turian"));
    assert_eq!(pool.user_id, Some(65918));
    let msg = warning(&pool).unwrap();
    assert!(msg.contains("turian") && msg.contains("65918"), "{msg}");
    assert!(msg.contains("every personal access token"), "{msg}");
}

#[test]
fn no_token_value_reaches_the_report_or_the_warning() {
    let mut r = report("GITHUB_TOKEN");
    r.pool = classify(&r, Some(secret().as_str()), user_ok);
    let json = serde_json::to_string(&r).unwrap();
    let msg = warning(r.pool.as_ref().unwrap()).unwrap();
    for out in [&json, &msg] {
        assert!(!out.contains(&secret()), "token leaked: {out}");
        assert!(!out.contains("ghp_"), "token prefix leaked: {out}");
    }
}

#[test]
fn an_integration_refusal_of_user_is_an_installation() {
    let refusal = || Err("HTTP 403: Resource not accessible by integration".to_string());
    let pool = classify(&report("keyring"), None, refusal).unwrap();
    assert_eq!(pool.kind, KIND_INSTALLATION);
}

#[test]
fn no_credential_or_a_gateway_has_no_pool() {
    let mut failed = report("keyring");
    failed.ok = false;
    for r in [failed, report("gateway"), report("none"), report("unknown")] {
        assert!(classify(&r, None, || panic!("no lookup")).is_none(), "{}", r.mechanism);
    }
}

#[test]
fn the_pool_field_is_optional_on_the_wire() {
    let json = serde_json::to_value(report("keyring")).unwrap();
    assert!(json.get("pool").is_none(), "absent pool is omitted: {json}");
    let old: CredentialPreflightReport = serde_json::from_value(json).unwrap();
    assert!(old.pool.is_none());
}

#[test]
fn attach_resolves_the_login_through_a_stub_gh() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = tmp.path().join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\n[ \"$1 $2\" = 'api user' ] || exit 9\necho '{\"login\":\"turian\",\"id\":65918}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = attach_with(report("keyring"), Some(gh.to_str().unwrap()));
    let pool = out.pool.unwrap();
    assert_eq!(
        (pool.kind.as_str(), pool.login.as_deref(), pool.user_id),
        (KIND_USER, Some("turian"), Some(65918))
    );
    // App mode never spends the call.
    let app = attach_with(report("github-app"), Some("/nonexistent/gh"));
    assert_eq!(app.pool.unwrap().kind, KIND_INSTALLATION);
}
