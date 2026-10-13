//! Several owners — organizations AND user accounts — per cycle (Issue
//! #9188): config/env resolution, per-owner kind probing and caching,
//! `orgs/` vs `users/` discovery, the unchanged `org`-only path, and story
//! stitching for a user-owned repo.
//!
//! The user owner `fixture-user` is synthesised from the recorded fixture:
//! its one repo `gamma` replays `fixture-org/beta`'s runs and jobs under the
//! new name, so no new fixture file is committed.

use std::sync::Arc;

use super::*;
use crate::ci_telemetry::owners::{
    parse_logins, resolve_owners, KindCache, Owner, OwnerKind, SOURCE_CONFIG_ORG,
    SOURCE_CONFIG_OWNERS, SOURCE_DEFAULT, SOURCE_ENV_ORG, SOURCE_ENV_OWNERS,
};
use crate::telemetry::repo_identity::RepoIdentity;

const USER: &str = "fixture-user";
const USER_REPOS: &str = "users/fixture-user/repos?per_page=100&type=owner";

fn owner_kind_body(kind: &str) -> Value {
    serde_json::json!({"status": 200, "body": {"login": "x", "type": kind}})
}

/// The fixture org plus `fixture-user` (a User owning `gamma`), and the
/// `GET /users/{owner}` answers for both.
fn two_owner_api() -> FixtureApi {
    let api = FixtureApi::new();
    {
        let mut responses = api.responses.lock().unwrap();
        let cloned: Vec<(String, Value)> = responses
            .iter()
            .filter(|(key, _)| key.starts_with("repos/fixture-org/beta/"))
            .map(|(key, entry)| {
                let text = entry
                    .to_string()
                    .replace("fixture-org/beta", "fixture-user/gamma");
                (
                    key.replace("repos/fixture-org/beta/", "repos/fixture-user/gamma/"),
                    serde_json::from_str(&text).unwrap(),
                )
            })
            .collect();
        responses.extend(cloned);
        responses.insert(
            USER_REPOS.to_string(),
            serde_json::json!({"status": 200, "etag": "W/\"user-repos\"", "body": [
                {"name": "gamma", "full_name": "fixture-user/gamma", "private": false, "archived": false}
            ]}),
        );
        responses.insert(format!("users/{ORG}"), owner_kind_body("Organization"));
        responses.insert(format!("users/{USER}"), owner_kind_body("User"));
    }
    api
}

fn two_owners(root: &Path, kinds: Arc<KindCache>) -> CycleContext<'_> {
    CycleContext {
        owners: vec![Owner::probed(ORG), Owner::probed(USER)],
        owner_kinds: kinds,
        ..ctx(root)
    }
}

fn requested(api: &FixtureApi) -> Vec<String> {
    api.requests().into_iter().map(|(path, _)| path).collect()
}

// ---------------------------------------------------------------------------
// Config: owners list, org alias, env override, precedence
// ---------------------------------------------------------------------------

#[test]
fn owners_resolve_env_over_config_over_default_and_owners_over_org_per_tier() {
    let list = vec!["2amlogic".to_string(), "rjwalters".to_string()];
    let probed = |logins: &[&str]| logins.iter().map(|l| Owner::probed(l)).collect::<Vec<_>>();
    // Default: the pre-#9188 org, declared (never probed).
    assert_eq!(
        resolve_owners(None, None, None, None, DEFAULT_ORG),
        (vec![Owner::org("2amlogic")], SOURCE_DEFAULT)
    );
    // Config tier: `org` alone is the deprecated alias, a declared org...
    assert_eq!(
        resolve_owners(None, None, None, Some("acme"), DEFAULT_ORG),
        (vec![Owner::org("acme")], SOURCE_CONFIG_ORG)
    );
    // ...and `owners` wins over `org` at the same tier.
    assert_eq!(
        resolve_owners(None, None, Some(&list), Some("acme"), DEFAULT_ORG),
        (probed(&["2amlogic", "rjwalters"]), SOURCE_CONFIG_OWNERS)
    );
    // Env tier beats config — the env `org` alias beats config `owners`...
    assert_eq!(
        resolve_owners(None, Some("envorg"), Some(&list), None, DEFAULT_ORG),
        (vec![Owner::org("envorg")], SOURCE_ENV_ORG)
    );
    // ...and env `owners` (comma-separated) beats the env `org` alias.
    assert_eq!(
        resolve_owners(Some(" a , b,A,, "), Some("envorg"), Some(&list), None, DEFAULT_ORG),
        (probed(&["a", "b"]), SOURCE_ENV_OWNERS)
    );
    // An empty value at a tier counts as unset there.
    assert_eq!(
        resolve_owners(Some(" , "), None, Some(&[]), None, DEFAULT_ORG).1,
        SOURCE_DEFAULT
    );
    assert_eq!(parse_logins(["x,y", "X", " z "]), vec!["x", "y", "z"]);
}

#[test]
#[serial_test::serial]
fn config_owners_list_is_read_and_wins_over_the_org_alias() {
    if std::env::var(OWNERS_ENV).is_ok() || std::env::var(ORG_ENV).is_ok() {
        return;
    }
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    let config = dir.path().join(".loom/config.json");
    std::fs::write(
        &config,
        r#"{"autonomous":{"ciTelemetry":{"org":"acme","owners":["2amlogic","rjwalters",7,""]}}}"#,
    )
    .unwrap();
    let read = read_config(dir.path());
    assert_eq!(read.owners, Some(vec!["2amlogic".to_string(), "rjwalters".to_string()]));
    let resolved = resolve(&read);
    assert_eq!(resolved.owners, vec![Owner::probed("2amlogic"), Owner::probed("rjwalters")]);
    assert_eq!(resolved.owners_source, SOURCE_CONFIG_OWNERS);

    // AC3 regression: an `org`-only config resolves exactly as before — one
    // declared org, which the cycle discovers through `orgs/` unprobed.
    std::fs::write(&config, r#"{"autonomous":{"ciTelemetry":{"org":"acme"}}}"#).unwrap();
    let resolved = resolve(&read_config(dir.path()));
    assert_eq!(resolved.owners, vec![Owner::org("acme")]);
    assert_eq!(resolved.owners_source, SOURCE_CONFIG_ORG);
    assert_eq!(CycleContext::new(dir.path(), &resolved).owners, vec![Owner::org("acme")]);
}

#[test]
#[serial_test::serial]
fn env_owners_override_config_owners_and_the_env_org_alias() {
    let config = CiTelemetryConfig {
        owners: Some(vec!["from-config".into()]),
        org: Some("config-org".into()),
        ..CiTelemetryConfig::default()
    };
    let org_only = resolve_with_env(&config, &fixed_env(&[(ORG_ENV, "env-org")]));
    let both = resolve_with_env(
        &config,
        &fixed_env(&[(ORG_ENV, "env-org"), (OWNERS_ENV, "rjwalters, 2amlogic")]),
    );
    assert_eq!(org_only.owners, vec![Owner::org("env-org")]);
    assert_eq!(org_only.owners_source, SOURCE_ENV_ORG);
    assert_eq!(both.owners, vec![Owner::probed("rjwalters"), Owner::probed("2amlogic")]);
    assert_eq!(both.owners_source, SOURCE_ENV_OWNERS);
}

// ---------------------------------------------------------------------------
// Discovery: users/ for a User, orgs/ for an Organization
// ---------------------------------------------------------------------------

#[test]
fn a_user_owner_is_discovered_via_users_and_an_org_via_orgs() {
    let dir = TempDir::new().unwrap();
    let api = two_owner_api();
    let report = run_cycle(&two_owners(dir.path(), Arc::default()), &api).unwrap();
    assert!(report.repo_errors.is_empty(), "{:?}", report.repo_errors);
    assert_eq!(report.summary.repos_polled, 3, "alpha + beta (org) + gamma (user)");
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (9, 36));
    assert_no_duplicates(dir.path());

    let paths = requested(&api);
    assert!(paths.contains(&USER_REPOS.to_string()), "{paths:?}");
    assert!(paths.contains(&format!("orgs/{ORG}/repos?per_page=100&type=all")));
    assert!(!paths.iter().any(|p| p.starts_with(&format!("orgs/{USER}"))), "{paths:?}");
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with(&format!("users/{ORG}/repos"))),
        "{paths:?}"
    );
    assert!(paths
        .iter()
        .any(|p| p.starts_with("repos/fixture-user/gamma/actions/runs")));

    let expected = [
        (ORG, OwnerKind::Organization, 2),
        (USER, OwnerKind::User, 1),
    ];
    let status = state::load_status(&state_dir(dir.path()));
    for rows in [&report.owners, &status.owners] {
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.owner.as_str(), r.kind.unwrap(), r.repos.unwrap()))
            .collect();
        assert_eq!(got, expected);
    }
    assert_eq!(status.org.as_deref(), Some("fixture-org,fixture-user"));
}

#[test]
fn owner_kind_is_probed_once_per_process_and_each_owners_etag_cache_survives() {
    let dir = TempDir::new().unwrap();
    let kinds: Arc<KindCache> = Arc::default();
    run_cycle(&two_owners(dir.path(), kinds.clone()), &two_owner_api()).unwrap();
    let api = two_owner_api();
    let report = run_cycle(&two_owners(dir.path(), kinds), &api).unwrap();
    assert!(report.repo_errors.is_empty(), "{:?}", report.repo_errors);
    assert_eq!(report.summary.repos_polled, 3);
    let requests = api.requests();
    assert!(
        !requests
            .iter()
            .any(|(p, _)| p.starts_with("users/") && !p.contains("/repos")),
        "the kind is cached for the process lifetime: {requests:?}"
    );
    // Both owners' discovery pages were cached side by side: every discovery
    // request of cycle 2 is conditional (neither owner clobbered the other).
    let discovery: Vec<_> = requests
        .iter()
        .filter(|(p, _)| p.contains("/repos?"))
        .collect();
    assert_eq!(discovery.len(), 3, "{discovery:?}");
    assert!(discovery.iter().all(|(_, etag)| etag.is_some()), "{discovery:?}");
}

/// A missing owner, the user owner, and an owner of an unsupported type.
fn with_ghost(root: &Path, kinds: Arc<KindCache>) -> CycleContext<'_> {
    CycleContext {
        owners: vec![
            Owner::probed("ghost"),
            Owner::probed(USER),
            Owner::probed("robot"),
        ],
        ..two_owners(root, kinds)
    }
}

#[test]
fn a_failed_kind_probe_skips_only_that_owner_names_it_and_retries_next_cycle() {
    let dir = TempDir::new().unwrap();
    let kinds: Arc<KindCache> = Arc::default();
    let api = two_owner_api();
    api.responses
        .lock()
        .unwrap()
        .insert("users/robot".into(), owner_kind_body("Bot"));
    let report = run_cycle(&with_ghost(dir.path(), kinds.clone()), &api).unwrap();
    // Never guessed: no discovery at all for the unresolved owners.
    let paths = requested(&api);
    assert!(!paths
        .iter()
        .any(|p| p.contains("ghost/repos") || p.contains("robot/repos")));
    assert_eq!(report.summary.repos_polled, 1, "the user owner is still polled");
    assert_eq!(report.repo_errors.len(), 2, "{:?}", report.repo_errors);
    assert!(report.repo_errors[0].starts_with("owner ghost: discovery-failed: HTTP 404"));
    assert!(report.repo_errors[1].contains("unsupported owner type \"Bot\""));
    let status = state::load_status(&state_dir(dir.path()));
    assert!(status.owners[0].error.is_some() && status.owners[0].kind.is_none());

    let api = two_owner_api();
    run_cycle(&with_ghost(dir.path(), kinds), &api).unwrap();
    let paths = requested(&api);
    assert!(paths.contains(&"users/ghost".to_string()), "a failure is retried: {paths:?}");
    assert!(!paths.contains(&format!("users/{USER}")), "a success is cached: {paths:?}");
}

#[test]
fn a_rate_limit_on_the_second_owners_kind_lookup_aborts_the_whole_cycle() {
    // #9197 item 4: a rate limit is org-wide, so it must abort the cycle even
    // when it surfaces on the *second* owner's `resolve_kind` probe, not on
    // discovery or a per-repo request.
    let dir = TempDir::new().unwrap();
    let api = two_owner_api();
    api.set_override(
        &format!("users/{USER}"),
        ApiResponse {
            status: 429,
            retry_after_secs: Some(60),
            ..ApiResponse::default()
        },
    );
    let before = super::poll::BREAKER_NOTIFICATIONS.with(std::cell::Cell::get);
    let result = run_cycle(&two_owners(dir.path(), Arc::default()), &api);
    let Err(error @ CycleError::RateLimited { .. }) = result else {
        panic!("a rate limit on the second owner's kind lookup must abort the cycle: {result:?}");
    };
    assert!(!matches!(error, CycleError::CredentialRejected { .. }));
    assert_eq!(super::poll::BREAKER_NOTIFICATIONS.with(std::cell::Cell::get), before + 1);
    // Nothing after the first owner's discovery ran: no repo request at all.
    let paths = requested(&api);
    assert!(
        !paths.iter().any(|p| p.contains("/actions/runs")),
        "the cycle aborted before any repo was polled: {paths:?}"
    );
}

#[test]
fn a_401_on_the_second_owners_kind_lookup_aborts_the_cycle_as_credential_rejected() {
    // #9197 item 4: a rejected credential aborts the cycle exactly as it does
    // during discovery or a repo request (#8850) — including when the 401
    // lands on the second owner's `GET /users/{owner}` kind probe.
    let dir = TempDir::new().unwrap();
    let api = two_owner_api();
    api.set_override(
        &format!("users/{USER}"),
        ApiResponse {
            status: 401,
            body: "{\"message\":\"Bad credentials\"}".into(),
            ..ApiResponse::default()
        },
    );
    let result = run_cycle(&two_owners(dir.path(), Arc::default()), &api);
    let Err(CycleError::CredentialRejected { .. }) = result else {
        panic!(
            "a 401 on the second owner's kind lookup must abort as CredentialRejected: {result:?}"
        );
    };
    let paths = requested(&api);
    assert!(
        !paths.iter().any(|p| p.contains("/actions/runs")),
        "the cycle aborted before any repo was polled: {paths:?}"
    );
}

#[test]
fn a_resolved_kind_is_still_reported_when_discovery_itself_fails() {
    // #9197 item 3: `resolve_kind` can succeed (the owner IS a User/Org) while
    // the follow-on `discover()` call fails — a transient 404/5xx on the
    // listing endpoint, not on `GET /users/{owner}`. `status` must still
    // report the resolved kind, not `null`, for that owner.
    let dir = TempDir::new().unwrap();
    let api = two_owner_api();
    api.set_override(
        USER_REPOS,
        ApiResponse {
            status: 404,
            body: "{\"message\":\"Not Found\"}".into(),
            ..ApiResponse::default()
        },
    );
    let report = run_cycle(&two_owners(dir.path(), Arc::default()), &api).unwrap();
    let user_row = report
        .owners
        .iter()
        .find(|row| row.owner == USER)
        .expect("the user owner is still reported, just as failed");
    assert_eq!(user_row.kind, Some(OwnerKind::User), "the kind probe succeeded");
    assert!(user_row.error.is_some(), "{user_row:?}");

    let status = state::load_status(&state_dir(dir.path()));
    let persisted = status.owners.iter().find(|row| row.owner == USER).unwrap();
    assert_eq!(persisted.kind, Some(OwnerKind::User), "{persisted:?}");
}

#[test]
fn only_when_every_owner_fails_is_the_cycle_discovery_failed() {
    let dir = TempDir::new().unwrap();
    let ctx = CycleContext {
        owners: vec![Owner::probed("ghost"), Owner::probed("phantom")],
        ..ctx(dir.path())
    };
    let result = run_cycle(&ctx, &FixtureApi::new());
    let Err(error @ CycleError::Discovery(_)) = result else {
        panic!("every owner failing is a discovery failure: {result:?}");
    };
    assert!(error
        .to_string()
        .starts_with("discovery-failed: HTTP 404 for users/ghost"));
}

// ---------------------------------------------------------------------------
// AC3: an `org`-only config behaves exactly as before
// ---------------------------------------------------------------------------

#[test]
fn a_declared_org_makes_exactly_the_pre_owners_requests() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.repos_polled, 2);
    let paths = requested(&api);
    assert_eq!(paths[0], format!("orgs/{ORG}/repos?per_page=100&type=all"));
    assert!(!paths.iter().any(|p| p.starts_with("users/")), "no kind probe: {paths:?}");
    assert_eq!(report.summary.requests, paths.len());
    let status = state::load_status(&state_dir(dir.path()));
    assert_eq!(status.org.as_deref(), Some(ORG));
    // The discovery cache holds exactly the org's pages, as before.
    let cache = state::load_discovery_cache(&state_dir(dir.path()));
    let keys: Vec<_> = cache.keys().cloned().collect();
    assert_eq!(
        keys,
        vec![
            format!("orgs/{ORG}/repos?per_page=100&type=all"),
            format!("orgs/{ORG}/repos?per_page=100&type=all&page=2"),
        ]
    );
}

// ---------------------------------------------------------------------------
// #9197 item 2: page-2+ links use GitHub's real `organizations/{id}/…` /
// `user/{id}/…` shape, not the owner's login — a stale one must still prune.
// ---------------------------------------------------------------------------

/// A repo listing page whose body is exactly `names`.
fn repo_page(names: &[&str], next: Option<&str>) -> ApiResponse {
    let body = serde_json::to_string(
        &names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "full_name": format!("{ORG}/{name}"),
                    "private": false,
                    "archived": false,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    ApiResponse {
        status: 200,
        etag: Some(format!("W/\"{}\"", names.join("-"))),
        next: next.map(str::to_string),
        body,
        ..ApiResponse::default()
    }
}

#[test]
fn a_stale_page_2_in_githubs_real_organizations_id_shape_is_pruned_once_it_stops_appearing() {
    // GitHub answers `GET /orgs/{org}/repos`'s page 2 at
    // `organizations/{id}/repos?...`, never at `orgs/{org}/repos?...&page=2`
    // (the shape every other fixture in this file happens to use, which
    // never exercised the bug: its page-2 path shares the first page's
    // login-based prefix, so the old base-string-match pruning accidentally
    // matched anyway).
    let dir = TempDir::new().unwrap();
    let first = format!("orgs/{ORG}/repos?per_page=100&type=all");
    let real_page_2 = "organizations/987654/repos?per_page=100&type=all&page=2".to_string();

    // Cycle 1: two pages, the second in the real `organizations/{id}` shape.
    let api = FixtureApi::new();
    api.set_override(&first, repo_page(&["alpha"], Some(&real_page_2)));
    api.set_override(&real_page_2, repo_page(&["beta"], None));
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.repos_polled, 2, "{:?}", report.repo_errors);
    let cache = state::load_discovery_cache(&state_dir(dir.path()));
    let mut keys: Vec<_> = cache.keys().cloned().collect();
    keys.sort();
    let mut expected = vec![first.clone(), real_page_2.clone()];
    expected.sort();
    assert_eq!(keys, expected, "both pages, including the real-shape page 2, are cached");

    // Cycle 2: the owner now fits on one page — `beta` merged away, no
    // `next` at all. The stale `organizations/987654/...` entry must be
    // pruned, not left to rot in the shared cache file forever.
    let api = FixtureApi::new();
    api.set_override(&first, repo_page(&["alpha"], None));
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.repos_polled, 1, "{:?}", report.repo_errors);
    let cache = state::load_discovery_cache(&state_dir(dir.path()));
    let keys: Vec<_> = cache.keys().cloned().collect();
    assert_eq!(
        keys,
        vec![first],
        "the stale real-shape page-2 entry was pruned, not left behind"
    );
}

// ---------------------------------------------------------------------------
// Story stitching for a user-owned repo
// ---------------------------------------------------------------------------

fn gamma_identity(name: &str) -> Option<RepoIdentity> {
    (name == "fixture-user/gamma").then(|| RepoIdentity {
        id: 1_073_994_527,
        full_name: name.to_string(),
    })
}

#[test]
fn a_user_owned_repos_run_stitches_into_its_story() {
    let dir = TempDir::new().unwrap();
    let api = two_owner_api();
    api.edit("repos/fixture-user/gamma/actions/runs?per_page=100", |page| {
        for row in page["body"]["workflow_runs"].as_array_mut().unwrap() {
            if row["id"] == 2002 {
                row["head_branch"] = "feature/issue-9188".into();
            }
        }
    });
    let ctx = CycleContext {
        owners: vec![Owner::probed(USER)],
        repo_identity: Some(gamma_identity),
        ..two_owners(dir.path(), Arc::default())
    };
    let report = run_cycle(&ctx, &api).unwrap();
    assert!(report.repo_errors.is_empty(), "{:?}", report.repo_errors);
    assert_eq!(report.summary.story_runs_stitched, 1, "{}", report.summary());
    let stories: BTreeSet<String> = journal(dir.path())
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(span) => span.attributes.get("loom.story").cloned(),
            _ => None,
        })
        .collect();
    assert_eq!(stories, BTreeSet::from(["fixture-user/gamma#9188".to_string()]));
}
