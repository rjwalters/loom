use super::*;
use serde_json::json;

fn ident(app: &str, slug: Option<&str>) -> Identity {
    Identity {
        app_id: app.to_string(),
        slug: slug.map(str::to_string),
        private_key_path: PathBuf::from(format!("/keys/{app}.pem")),
    }
}

fn split_roster() -> Roster {
    Roster {
        writer: Some(ident("100", Some("loom-fleet-dispatch"))),
        readers: vec![
            ident("201", Some("loom-fleet-reader-1")),
            ident("202", Some("loom-fleet-reader-2")),
        ],
        legacy_logins: vec!["loom-fleet-dispatch-0".to_string()],
    }
}

// ---- roster resolution -----------------------------------------------------

#[test]
fn identities_block_is_parsed_with_writer_readers_and_legacy() {
    let cfg = json!({"forge": {"identities": {
        "writer": {"appId": "100", "slug": "loom-fleet-dispatch", "privateKeyPath": "/k/w.pem"},
        "readers": [
            {"appId": 201, "slug": "Loom-Fleet-Reader-1", "privateKeyPath": "/k/r1.pem"},
            {"appId": "202", "slug": "loom-fleet-reader-2[bot]", "privateKeyPath": "/k/r2.pem"}
        ],
        "legacyLogins": ["app/loom-fleet-dispatch-0", ""]
    }}});
    let r = from_config(&cfg, None, &[]);
    assert_eq!(r.writer.as_ref().unwrap().app_id, "100");
    assert_eq!(r.readers.len(), 2);
    assert_eq!(r.readers[0].app_id, "201", "numeric appId accepted");
    assert_eq!(r.readers[0].slug.as_deref(), Some("loom-fleet-reader-1"), "slug normalised");
    assert_eq!(r.readers[1].slug.as_deref(), Some("loom-fleet-reader-2"), "[bot] stripped");
    assert_eq!(r.legacy_logins, vec!["loom-fleet-dispatch-0".to_string()], "blank dropped");
}

#[test]
fn without_identities_the_pre_9537_keys_are_the_roster() {
    let cfg = json!({"forge": {"githubApp": {"appId": "100", "privateKeyPath": "/k/w.pem"}}});
    let pool = vec![PoolMember {
        app_id: "201".into(),
        private_key_path: "/k/r1.pem".into(),
    }];
    let r = from_config(&cfg, None, &pool);
    assert_eq!(r.writer.as_ref().unwrap().app_id, "100");
    assert_eq!(r.writer.as_ref().unwrap().slug, None);
    assert_eq!(r.readers.len(), 1);
    assert_eq!(r.readers[0].app_id, "201");
    assert!(r.legacy_logins.is_empty());
}

#[test]
fn nothing_configured_is_an_empty_roster() {
    assert_eq!(from_config(&json!({}), None, &[]), Roster::default());
}

#[test]
fn slug_env_overrides_the_writer_slug() {
    let cfg = json!({"forge": {"githubApp": {"appId": "100", "slug": "old", "privateKeyPath": "/k/w.pem"}}});
    let r = from_config(&cfg, Some(" App/New-Slug "), &[]);
    assert_eq!(r.writer.unwrap().slug.as_deref(), Some("new-slug"));
}

#[test]
fn a_reader_that_is_also_the_writer_is_dropped() {
    let cfg = json!({"forge": {"identities": {
        "writer": {"appId": "100", "privateKeyPath": "/k/w.pem"},
        "readers": [{"appId": "100", "privateKeyPath": "/k/w.pem"}, {"appId": "201", "privateKeyPath": "/k/r.pem"}]
    }}});
    let r = from_config(&cfg, None, &[]);
    assert_eq!(
        r.readers
            .iter()
            .map(|x| x.app_id.as_str())
            .collect::<Vec<_>>(),
        vec!["201"]
    );
}

#[test]
fn a_tilde_key_path_is_expanded() {
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = json!({"forge": {"githubApp": {"appId": "100", "privateKeyPath": "~/.loom/github-app/w.pem"}}});
    let r = from_config(&cfg, None, &[]);
    assert_eq!(
        r.writer.unwrap().private_key_path,
        PathBuf::from(home).join(".loom/github-app/w.pem")
    );
}

// ---- "is this login ours?" -------------------------------------------------

#[test]
fn every_configured_identity_and_legacy_login_is_the_fleet_in_every_spelling() {
    let fl = FleetLogins::of(&split_roster());
    for login in [
        "loom-fleet-dispatch",
        "loom-fleet-dispatch[bot]",
        "app/loom-fleet-dispatch",
        "loom-fleet-reader-1[bot]",
        "app/loom-fleet-reader-2",
        "LOOM-FLEET-DISPATCH-0",
    ] {
        assert!(fl.contains(login), "{login} should be the fleet");
    }
}

#[test]
fn a_configured_roster_believes_nothing_else() {
    let fl = FleetLogins::of(&split_roster());
    for login in [
        "loom-fleet-dispatch-evil[bot]",
        "loom-fleet-dispatcher",
        "someone",
        "",
        "app/",
    ] {
        assert!(!fl.contains(login), "{login} must not be the fleet");
    }
}

#[test]
fn the_default_family_is_believed_by_name_or_number_never_by_prefix() {
    let fl = FleetLogins::of(&Roster::default());
    assert!(
        FleetLogins::of(&split_roster()).contains("loom-fleet-dispatch-7"),
        "configured rosters too"
    );
    assert!(fl.contains("loom-fleet-dispatch"));
    assert!(fl.contains("app/loom-fleet-dispatch-0"));
    assert!(fl.contains("loom-fleet-dispatch-12[bot]"));
    assert!(
        !fl.contains("loom-fleet-dispatch-evil[bot]"),
        "the old prefix match trusted this"
    );
    assert!(!fl.contains("loom-fleet-dispatch-"));
    assert!(!fl.contains("loom-fleet-dispatcher"));
}

#[test]
fn single_is_exactly_one_login() {
    let fl = FleetLogins::single("app/custom-bot");
    assert!(fl.contains("custom-bot[bot]"));
    assert!(!fl.contains("loom-fleet-dispatch"));
}

#[test]
fn role_of_names_the_role() {
    let r = split_roster();
    assert_eq!(role_of(&r, "app/loom-fleet-dispatch"), Some("writer"));
    assert_eq!(role_of(&r, "loom-fleet-reader-2[bot]"), Some("reader"));
    assert_eq!(role_of(&r, "loom-fleet-dispatch-0"), Some("legacy"));
    assert_eq!(role_of(&r, "stranger"), None);
    assert_eq!(role_of(&Roster::default(), "loom-fleet-dispatch-3"), Some("default"));
}

// ---- read selection & delivery ---------------------------------------------

#[test]
fn reads_go_to_the_hashed_reader_deterministically() {
    let r = split_roster();
    let first = reader_for(&r, "2AMLogic/2am").unwrap().app_id.clone();
    for _ in 0..5 {
        assert_eq!(reader_for(&r, "2AMLogic/2am").unwrap().app_id, first);
    }
    // Same mapping as forge_read_pool's golden table (N=2 is its N=4 mod 2).
    let idx = forge_read_pool::assignment_index("2AMLogic/2am", 2).unwrap();
    assert_eq!(first, r.readers[idx].app_id);
}

#[test]
fn no_readers_means_no_read_credential() {
    let r = Roster {
        writer: Some(ident("100", None)),
        ..Roster::default()
    };
    assert!(reader_for(&r, "o/r").is_none());
}

fn write_reader_dir(
    ws: &Path,
    owner: &str,
    reader: &Identity,
    expires: chrono::DateTime<chrono::Utc>,
) -> PathBuf {
    let dir = reader_dir(ws, owner, reader);
    publish(&dir, "ghs_test", reader, "999", &expires.to_rfc3339()).unwrap();
    dir
}

#[test]
fn a_fresh_reader_dir_is_used_and_a_near_expiry_one_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    // Unique app ids: the withdrawal registry is process-global.
    let r = Roster {
        writer: Some(ident("100", Some("w"))),
        readers: vec![ident("fresh-1", Some("r1"))],
        legacy_logins: vec![],
    };
    let reader = &r.readers[0];
    let now = SystemTime::now();
    let soon = chrono::Utc::now() + chrono::Duration::seconds(30);
    let dir = write_reader_dir(tmp.path(), "Acme", reader, soon);
    assert!(
        read_credential_in(tmp.path(), &r, "Acme/app", now).is_none(),
        "30s left is below the floor"
    );

    let later = chrono::Utc::now() + chrono::Duration::minutes(40);
    write_reader_dir(tmp.path(), "Acme", reader, later);
    let (got_dir, app) = read_credential_in(tmp.path(), &r, "Acme/app", now).unwrap();
    assert_eq!(got_dir, dir);
    assert_eq!(app, "fresh-1");
    // The token file is the writer's own delivery format.
    assert!(std::fs::read_to_string(dir.join("hosts.yml"))
        .unwrap()
        .contains("ghs_test"));
    // A different owner has no token yet → writer.
    assert!(read_credential_in(tmp.path(), &r, "Other/app", now).is_none());
}

#[test]
fn a_missing_sidecar_or_token_is_not_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!dir_is_fresh(tmp.path(), SystemTime::now()));
    std::fs::write(tmp.path().join("hosts.yml"), "x").unwrap();
    assert!(!dir_is_fresh(tmp.path(), SystemTime::now()), "token without sidecar");
}

#[test]
fn a_withdrawn_reader_sends_reads_elsewhere() {
    let r = Roster {
        writer: Some(ident("100", None)),
        readers: vec![ident("wd-only", None)],
        legacy_logins: vec![],
    };
    assert!(reader_for(&r, "o/r").is_some());
    withdraw_reader("wd-only", "test");
    assert!(reader_for(&r, "o/r").is_none(), "only reader withdrawn → writer");
}

#[test]
fn credential_failures_are_classified() {
    assert!(is_credential_failure("gh: API rate limit exceeded for installation", None));
    assert!(is_credential_failure("You have exceeded a secondary rate limit", None));
    assert!(is_credential_failure(
        "gh: Resource not accessible by integration (HTTP 403)",
        None
    ));
    assert!(is_credential_failure("gh: Not Found (HTTP 404)", None));
    assert!(is_credential_failure("", Some(401)));
    assert!(!is_credential_failure("gh: Server Error (HTTP 502)", Some(502)));
    assert!(!is_credential_failure("could not resolve host", None));
}

// ---- minting ---------------------------------------------------------------

struct FakeMinter {
    outcome: GithubAppOutcome,
}

impl GithubAppMinter for FakeMinter {
    fn mint(&self, _owner_repo: &str) -> GithubAppOutcome {
        self.outcome.clone()
    }
}

#[test]
fn refresh_publishes_each_reader_for_each_owner_and_withdraws_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let r = Roster {
        writer: Some(ident("100", None)),
        readers: vec![ident("rf-ok", Some("ok")), ident("rf-bad", Some("bad"))],
        legacy_logins: vec![],
    };
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(55)).to_rfc3339();
    let exp2 = expires.clone();
    let make = move |id: &Identity| -> Box<dyn GithubAppMinter> {
        Box::new(FakeMinter {
            outcome: if id.app_id == "rf-ok" {
                GithubAppOutcome::Minted {
                    token: "ghs_x".into(),
                    installation_id: "7".into(),
                    app_id: id.app_id.clone(),
                    expires_at: exp2.clone(),
                }
            } else {
                GithubAppOutcome::Error("key not readable".into())
            },
        })
    };
    let owners = vec!["Acme/one".to_string(), "Beta/two".to_string()];
    let out = refresh_reader_credentials(tmp.path(), &r, &owners, &make);
    assert_eq!(out.len(), 4);
    for owner in ["Acme", "Beta"] {
        let dir = reader_dir(tmp.path(), owner, &r.readers[0]);
        assert!(dir_is_fresh(&dir, SystemTime::now()), "{owner} ok-reader published");
        assert_eq!(read_sidecar(&dir).unwrap().installation_id, "7");
        assert!(!reader_dir(tmp.path(), owner, &r.readers[1])
            .join("hosts.yml")
            .exists());
    }
    // A mint failure is owner-level coverage: nothing is withdrawn App-wide;
    // the unpublished directory is what keeps that owner on the writer.
    assert!(!forge_read_pool::is_withdrawn("rf-bad"));
    assert!(!forge_read_pool::is_withdrawn("rf-ok"));
    assert!(out
        .iter()
        .filter(|o| o.app_id == "rf-ok")
        .all(|o| o.result == Ok(expires.clone())));
}

// ---- write paths never see a reader ---------------------------------------

/// Structural guard (#9537 AC): reader credentials are only ever requested
/// from the reviewed READ call sites. A new caller of `read_credential` /
/// `apply_read_credential` must be added here deliberately, by someone who
/// has checked that the call it serves is a read.
#[test]
fn only_reviewed_read_paths_request_reader_credentials() {
    const ALLOWED: &[&str] = &[
        "forge_identity.rs",
        "forge_identity/tests.rs",
        "forge_etag_store.rs", // issue listings + cached views (GET, conditional)
        "ci_telemetry/api.rs", // repos/<o>/<r>/actions/... GETs
        "fleet_store/gh.rs",   // fleet-config: commit/tree/blob GETs (`--method GET`)
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let re = regex::Regex::new(r"\b(read_credential|read_credential_in|apply_read_credential)\b")
        .unwrap();
    let mut offenders = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            // Whole identifiers in any file that references forge_identity
            // at all: a brace-list import (`use crate::forge_identity::{…}`)
            // or a call after a glob import counts as much as a path call,
            // while another module's own `read_credential` (api_keys_pool
            // has one) does not.
            let calls = text.contains("forge_identity") && re.is_match(&text);
            if calls && !ALLOWED.contains(&rel.as_str()) {
                offenders.push(rel);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "reader credentials requested outside reviewed read paths: {offenders:?}"
    );
}

#[test]
fn failures_are_scoped_app_wide_or_to_one_repo() {
    use super::Failure::{App, Coverage};
    assert_eq!(
        classify_failure("gh: API rate limit exceeded for installation (HTTP 403)", Some(403)),
        Some(App)
    );
    assert_eq!(
        classify_failure("secondary rate limit", Some(403)),
        Some(App),
        "rate-limited 403 is App-wide"
    );
    assert_eq!(classify_failure("", Some(429)), Some(App));
    assert_eq!(classify_failure("Bad credentials (HTTP 401)", Some(401)), Some(App));
    assert_eq!(classify_failure("gh: Not Found (HTTP 404)", Some(404)), Some(Coverage));
    assert_eq!(
        classify_failure("Resource not accessible by integration (HTTP 403)", Some(403)),
        Some(Coverage)
    );
    assert_eq!(classify_failure("Server Error (HTTP 502)", Some(502)), None);
}

#[test]
fn a_coverage_withdrawal_affects_only_that_repo() {
    let r = Roster {
        writer: Some(ident("100", None)),
        readers: vec![ident("cov-a", None), ident("cov-b", None)],
        legacy_logins: vec![],
    };
    // Find two repos that hash to the same reader.
    let first = reader_for(&r, "owner/repo-0").unwrap().app_id.clone();
    let other = (1..200)
        .map(|i| format!("owner/repo-{i}"))
        .find(|repo| reader_for(&r, repo).unwrap().app_id == first)
        .unwrap();
    withdraw_after(&first, "owner/repo-0", Failure::Coverage, None, "test");
    assert_ne!(
        reader_for(&r, "owner/repo-0").unwrap().app_id,
        first,
        "uncovered repo moves to the next reader"
    );
    assert_eq!(
        reader_for(&r, &other).unwrap().app_id,
        first,
        "every other repo keeps its reader"
    );
    assert!(!forge_read_pool::is_withdrawn(&first), "not App-wide");
}

#[test]
fn an_app_withdrawal_honours_the_reported_reset() {
    let reset = SystemTime::now() + Duration::from_secs(1800);
    withdraw_after("reset-app", "o/r", Failure::App, Some(reset), "test");
    assert!(forge_read_pool::is_withdrawn_at(
        "reset-app",
        SystemTime::now() + Duration::from_secs(1700)
    ));
    assert!(!forge_read_pool::is_withdrawn_at("reset-app", reset + Duration::from_secs(1)));
}

#[test]
fn the_writer_is_always_forge_github_app() {
    // identities with readers but NO writer: forge.githubApp is still the writer.
    let cfg = json!({"forge": {
        "githubApp": {"appId": "100", "privateKeyPath": "/k/w.pem"},
        "identities": {"readers": [{"appId": "201", "slug": "r1", "privateKeyPath": "/k/r1.pem"}]}
    }});
    let r = from_config(&cfg, None, &[]);
    assert_eq!(
        r.writer.as_ref().unwrap().app_id,
        "100",
        "readers-only identities keep the configured writer"
    );
    assert_eq!(r.readers.len(), 1);

    // identities.writer restating the same App contributes its slug.
    let cfg = json!({"forge": {
        "githubApp": {"appId": "100", "privateKeyPath": "/k/w.pem"},
        "identities": {"writer": {"appId": "100", "slug": "loom-fleet-dispatch", "privateKeyPath": "/k/w.pem"}}
    }});
    assert_eq!(
        from_config(&cfg, None, &[]).writer.unwrap().slug.as_deref(),
        Some("loom-fleet-dispatch")
    );

    // A DIFFERENT identities.writer cannot redirect writes: githubApp wins.
    let cfg = json!({"forge": {
        "githubApp": {"appId": "100", "privateKeyPath": "/k/w.pem"},
        "identities": {"writer": {"appId": "999", "slug": "other", "privateKeyPath": "/k/o.pem"}}
    }});
    let w = from_config(&cfg, None, &[]).writer.unwrap();
    assert_eq!(w.app_id, "100");
    assert_eq!(w.slug, None, "the other App's slug is not borrowed");
}

#[test]
fn config_warnings_name_a_mismatched_or_orphaned_writer() {
    let ok = json!({"forge": {"githubApp": {"appId": "100", "privateKeyPath": "/k"},
        "identities": {"writer": {"appId": "100", "privateKeyPath": "/k"}}}});
    assert!(config_warnings(&ok).is_empty());
    let mismatch = json!({"forge": {"githubApp": {"appId": "100", "privateKeyPath": "/k"},
        "identities": {"writer": {"appId": "999", "privateKeyPath": "/o"}}}});
    assert!(config_warnings(&mismatch)[0].contains("differs"));
    let orphan =
        json!({"forge": {"identities": {"writer": {"appId": "999", "privateKeyPath": "/o"}}}});
    assert!(config_warnings(&orphan)[0].contains("ambient gh auth"));
}

#[test]
fn a_stale_reader_token_yields_to_the_next_reader_not_the_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let r = Roster {
        writer: Some(ident("100", None)),
        readers: vec![ident("nx-a", None), ident("nx-b", None)],
        legacy_logins: vec![],
    };
    let repo = "Acme/app";
    let first = reader_for(&r, repo).unwrap().clone();
    let second = r
        .readers
        .iter()
        .find(|x| x.app_id != first.app_id)
        .unwrap()
        .clone();
    // Only the SECOND reader has a fresh token for Acme.
    write_reader_dir(
        tmp.path(),
        "Acme",
        &second,
        chrono::Utc::now() + chrono::Duration::minutes(30),
    );
    let (_, app) = read_credential_in(tmp.path(), &r, repo, SystemTime::now()).unwrap();
    assert_eq!(app, second.app_id);
}
