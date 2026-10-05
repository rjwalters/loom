#![allow(clippy::unwrap_used, clippy::expect_used)]
//! W1: the bucket book — readings, freshness, the free probe, export.

use super::*;
use crate::forge_identity::IdentityRole;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::SystemTime;

fn headers(reset: i64) -> RateLimitHeaders {
    RateLimitHeaders {
        resource: Some("core".into()),
        remaining: Some(4000),
        used: Some(1000),
        reset_epoch: Some(reset),
        limit: Some(5000),
    }
}

#[test]
fn a_reading_is_believed_only_while_its_window_is_open_and_it_is_recent() {
    let key = BucketKey::new("app-test-fresh", "Acme", Resource::Core);
    assert_eq!(key.owner, "acme", "owners are lowercased");
    let t0 = 1_900_000_000;
    observe_at(key.clone(), &headers(t0 + 3000), Source::Header, t0);
    let held = reading(&key, t0 + 1).expect("fresh");
    assert_eq!((held.limit, held.remaining, held.used), (Some(5000), Some(4000), Some(1000)));
    assert_eq!(held.source, Source::Header);
    assert!(reading(&key, t0 + MAX_AGE_SECS).is_none(), "600 s old is too old");

    let short = BucketKey::new("app-test-reset", "acme", Resource::Graphql);
    observe_at(short.clone(), &headers(t0 + 30), Source::Header, t0);
    assert!(reading(&short, t0 + 29).is_some());
    assert!(reading(&short, t0 + 30).is_none(), "a passed reset is stale");

    // An older observation never replaces a newer one.
    let mut old = headers(t0 + 3000);
    old.remaining = Some(1);
    observe_at(key.clone(), &old, Source::Header, t0 - 5);
    assert_eq!(reading(&key, t0 + 1).unwrap().remaining, Some(4000));

    // Headers without a reset say nothing about the window.
    let none = BucketKey::new("app-test-noreset", "acme", Resource::Core);
    observe_at(none.clone(), &RateLimitHeaders::default(), Source::Header, t0);
    assert!(reading(&none, t0).is_none());
}

#[test]
fn classify_dir_reads_only_the_path_shape() {
    let p = |s: &str| classify_dir(Path::new(s));
    assert_eq!(
        p("/w/.loom/gh-config-by-owner/Acme/42"),
        DirClass::Reader {
            owner: "acme".into(),
            app_id: "42".into()
        }
    );
    assert_eq!(
        p("/w/.loom/gh-config-by-owner/acme/"),
        DirClass::OwnerWriter {
            root: PathBuf::from("/w"),
            owner: "acme".into()
        }
    );
    assert_eq!(
        p("/w/.loom/gh-config"),
        DirClass::PrimaryWriter {
            root: PathBuf::from("/w")
        }
    );
    for other in [
        "/home/u/.config/gh",
        "/w/.loom/gh-config-by-owner/acme/not-digits",
        "/w/.loom/gh-config-by-owner/bad_owner",
        "/w/.loom/gh-config/extra",
    ] {
        assert_eq!(p(other), DirClass::Other, "{other}");
    }
}

#[test]
fn a_snapshot_round_trips_through_the_sink_dir() {
    let dir = tempfile::tempdir().unwrap();
    let sink = dir.path().join("sink");
    let now = chrono::Utc::now().timestamp();
    let key = BucketKey::new("app-test-persist", "acme", Resource::Core);
    observe_at(key.clone(), &headers(now + 900), Source::Probe, now);
    persist(&sink, now).unwrap();
    let loaded = load(&sink, now);
    let (_, r) = loaded.iter().find(|(k, _)| *k == key).expect("persisted");
    assert_eq!((r.used, r.source), (Some(1000), Source::Probe));
    assert!(load(&sink, now + 1000).iter().all(|(k, _)| *k != key), "stale on load");
    assert!(load(&dir.path().join("missing"), now).is_empty());
}

#[test]
fn bucket_gauges_carry_an_owner_label_and_quota_points_do_not() {
    use crate::observability::ops::ratelimit::{bucket_points, quota_points};
    use crate::telemetry::ops::MetricName;
    let reading = Reading {
        limit: Some(5000),
        remaining: Some(4000),
        used: Some(1000),
        reset_epoch: 1_900_000_900,
        observed_at: 1_900_000_000,
        source: Source::Probe,
    };
    let points = bucket_points(&[
        (BucketKey::new("app-1", "acme", Resource::Core), reading),
        (BucketKey::new("app-2", "beta", Resource::Graphql), reading),
    ]);
    assert_eq!(points.len(), 6);
    for p in &points {
        assert!(matches!(
            p.name,
            MetricName::GithubRateLimitRemaining
                | MetricName::GithubRateLimitUsed
                | MetricName::GithubRateLimitReset
        ));
        assert_eq!(p.labels.len(), 3, "{p:?}");
        assert!(p.labels.contains_key("owner"));
    }
    assert!(points
        .iter()
        .any(|p| p.labels["owner"] == "beta" && p.labels["resource"] == "graphql"));

    let budget = crate::rate_limit_breaker::BudgetSnapshot {
        core_remaining: 1,
        core_used: Some(2),
        core_reset: chrono::Utc::now(),
        graphql_remaining: 3,
        graphql_used: None,
        graphql_reset: chrono::Utc::now(),
        probed_at: chrono::Utc::now(),
    };
    for p in quota_points(&budget, "app-1") {
        let keys: Vec<&str> = p.labels.keys().map(String::as_str).collect();
        assert_eq!(keys, ["account", "resource"], "unchanged account-only points");
    }
}

// ===== probe_all =====

fn write_sidecar(dir: &Path, app_id: &str, expires: chrono::DateTime<chrono::Utc>) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("hosts.yml"), "").unwrap();
    std::fs::write(
        dir.join("identity.json"),
        serde_json::json!({
            "appId": app_id,
            "slug": null,
            "installationId": "1",
            "expiresAt": expires.to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn probe_all_probes_each_published_dir_once_under_its_own_credential() {
    let ws = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(ws.path())
            .output()
            .unwrap()
            .status
            .success());
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "https://github.com/acme/ws"]);
    let loom = ws.path().join(".loom");
    let primary = loom.join("gh-config");
    let owner = loom.join("gh-config-by-owner/acme");
    let reader = owner.join("42");
    let stale = owner.join("43");
    let reader_only_owner = loom.join("gh-config-by-owner/beta");
    for d in [&primary, &owner] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("hosts.yml"), "").unwrap();
    }
    let now = chrono::Utc::now();
    write_sidecar(&reader, "42", now + chrono::Duration::hours(1));
    write_sidecar(&stale, "43", now - chrono::Duration::minutes(1));
    write_sidecar(&reader_only_owner.join("44"), "44", now - chrono::Duration::minutes(1));

    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("probes");
    let reset = now.timestamp() + 1800;
    let body = format!(
        r#"{{"resources":{{"core":{{"limit":5000,"used":120,"remaining":4880,"reset":{reset}}},"graphql":{{"limit":5000,"used":7,"remaining":4993,"reset":{reset}}}}}}}"#
    );
    let gh = bin.path().join("gh-probe");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$GH_CONFIG_DIR|${{GH_TOKEN-unset}}|${{GITHUB_TOKEN-unset}}|$*\" >> '{}'\n\
             printf 'HTTP/2.0 200 OK\\r\\nX-Ratelimit-Resource: core\\r\\n\\r\\n%s' '{body}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let targets = probe_targets(ws.path(), SystemTime::now());
    assert_eq!(
        targets
            .iter()
            .map(|t| (t.dir.clone(), t.role))
            .collect::<Vec<_>>(),
        vec![
            (primary.clone(), IdentityRole::Writer),
            (owner.clone(), IdentityRole::Writer),
            (reader.clone(), IdentityRole::Reader),
        ],
        "a stale sidecar and an owner dir without hosts.yml are skipped"
    );
    for t in &targets {
        let plan = probe_invocation(t, Some(&gh)).env_plan(None);
        for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
            assert!(plan.iter().any(|e| e.key == key && e.value.is_none()), "{key} removed");
        }
        assert!(plan
            .iter()
            .any(|e| e.key == "GH_CONFIG_DIR" && e.value.as_deref() == Some(t.dir.as_os_str())));
    }

    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    let probes = probe_all_with(ws.path(), Some(&gh), SystemTime::now());
    let rows = crate::forge_call_stats::status_report(chrono::Utc::now(), None)
        .host_window
        .unwrap_or_default();
    crate::forge_call_stats::set_test_sink_dir(None);

    assert_eq!(probes, 3);
    let calls = std::fs::read_to_string(&log).unwrap();
    let dirs: Vec<&str> = calls
        .lines()
        .map(|l| l.split('|').next().unwrap())
        .collect();
    assert_eq!(
        dirs,
        [
            primary.to_str().unwrap(),
            owner.to_str().unwrap(),
            reader.to_str().unwrap()
        ]
    );
    for l in calls.lines() {
        let f: Vec<&str> = l.split('|').collect();
        assert_eq!((f[1], f[2], f[3]), ("unset", "unset", "api --include rate_limit"), "{l}");
    }
    let rl = rows.iter().find(|r| r.caller == PROBE_OPERATION).unwrap();
    assert_eq!((rl.pool.as_str(), rl.ok), ("other", 3), "free: booked to other");

    let writer = writer_account(ws.path());
    let at = chrono::Utc::now().timestamp();
    let core = reading(&BucketKey::new(&writer, "acme", Resource::Core), at).unwrap();
    assert_eq!((core.used, core.limit, core.source), (Some(120), Some(5000), Source::Probe));
    let gql = reading(&BucketKey::new("app-42", "acme", Resource::Graphql), at).unwrap();
    assert_eq!(gql.remaining, Some(4993));
    let snap = load(sink.path(), at);
    assert!(snap.iter().any(|(k, _)| k.account == "app-42"), "snapshot persisted");
}

#[test]
fn parse_probe_reads_core_graphql_and_search() {
    let body = r#"{"resources":{"core":{"limit":5000,"used":1,"remaining":4999,"reset":1900000000},
        "search":{"limit":30,"used":0,"remaining":30,"reset":1900000060},
        "code_search":{"limit":10,"used":0,"remaining":10,"reset":1900000060}}}"#;
    let got = parse_probe(body, 1_899_999_000);
    let resources: Vec<Resource> = got.iter().map(|(r, _)| *r).collect();
    assert_eq!(resources, [Resource::Core, Resource::Search]);
    assert!(parse_probe("not json", 0).is_empty());
}
