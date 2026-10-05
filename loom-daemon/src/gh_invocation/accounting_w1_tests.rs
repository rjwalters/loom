#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! W1: every facade row names its credential bucket, its repo origin and
//! its page count, without changing how the call runs.

use super::*;
use crate::forge_bucket_book::{self, BucketKey, Resource};
use crate::gh_invocation::{AccessIntent, GhTarget, Operation, ParentContext};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn inv(op: &'static str, args: &[&str], program: &Path) -> GhInvocation {
    GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .program(program)
    .args(args.iter().copied())
}

/// Run `body` with this thread's sink at a fresh dir; return every raw line.
fn raw_lines_after(body: impl FnOnce()) -> Vec<serde_json::Value> {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    crate::forge_call_stats::set_test_sink_dir(None);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(sink.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("calls-") {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            out.extend(text.lines().map(|l| serde_json::from_str(l).unwrap()));
        }
    }
    out
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A tempdir git checkout whose `origin` is `url`.
fn checkout(url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["remote", "add", "origin", url]);
    dir
}

// ===== 1. repo fill from the working directory's origin =====

#[test]
fn an_untargeted_call_in_a_checkout_is_attributed_to_its_origin_without_a_forge_call() {
    let repo = checkout("https://github.com/acme/widget");
    let bin = tempfile::tempdir().unwrap();
    let count = bin.path().join("spawns");
    let gh = stub(bin.path(), "gh-count", &format!("echo x >> '{}'; echo '{{}}'", count.display()));
    let call = inv("issue.view", &["issue", "view", "1"], &gh).current_dir(repo.path());

    let (id, ro) = resolve_with(&call, None, None);
    assert_eq!(id.repo.as_deref(), Some("acme/widget"));
    assert_eq!(ro, RepoOrigin::Remote);

    let lines = raw_lines_after(|| {
        let _ = call.run();
    });
    let spawns = std::fs::read_to_string(&count).unwrap().lines().count();
    assert_eq!(spawns, 1, "resolution itself must spawn no gh");
    assert_eq!(lines.len(), 1, "{lines:?}");
    if std::env::var("LOOM_REPO").map_or(true, |v| v.is_empty()) {
        assert_eq!(lines[0]["rp"], "acme/widget");
        assert_eq!(lines[0]["ro"], "remote");
    }
}

#[test]
fn the_repo_precedence_is_site_then_target_then_loom_repo_then_remote() {
    let repo = checkout("git@github.com:acme/from-remote.git");
    let base = |target: GhTarget| {
        GhInvocation::new(
            Operation::new("api.rest"),
            AccessIntent::Read,
            target,
            Duration::from_secs(10),
        )
        .args(["api", "x"])
        .current_dir(repo.path())
    };
    let typed = || GhTarget::repo("acme/typed").unwrap();
    let cases: Vec<(GhInvocation, Option<String>, &str, RepoOrigin)> = vec![
        (
            base(typed()).identity_scope(None, Some("acme/site")),
            Some("acme/env".into()),
            "acme/site",
            RepoOrigin::Site,
        ),
        (base(typed()), Some("acme/env".into()), "acme/typed", RepoOrigin::Target),
        (base(GhTarget::None), Some("acme/env".into()), "acme/env", RepoOrigin::LoomRepo),
        (
            base(GhTarget::None),
            Some(String::new()),
            "acme/from-remote",
            RepoOrigin::Remote,
        ),
        (base(GhTarget::None), None, "acme/from-remote", RepoOrigin::Remote),
    ];
    for (call, loom_repo, want, origin) in cases {
        let (id, ro) = resolve_with(&call, None, loom_repo);
        assert_eq!((id.repo.as_deref(), ro), (Some(want), origin));
    }
    // No cwd, no target, no env: unresolved, exactly as before W1.
    let none = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    );
    assert_eq!(resolve_with(&none, None, None).1, RepoOrigin::None);
    assert_eq!(resolved_identity_with(&none, None, None).repo, None);
}

// ===== 2. credential attribution =====

#[test]
fn cred_of_classifies_every_credential_shape_without_leaking_a_path() {
    let ws = checkout("https://github.com/acme/workspace");
    std::fs::create_dir_all(ws.path().join(".loom")).unwrap();
    std::fs::write(
        ws.path().join(".loom/config.json"),
        r#"{"forge": {"githubApp": {"appId": "777", "privateKeyPath": "/k/w.pem"}}}"#,
    )
    .unwrap();
    let reader = ws.path().join(".loom/gh-config-by-owner/acme/4242");
    let owner_dir = ws.path().join(".loom/gh-config-by-owner/Acme");
    let primary = ws.path().join(".loom/gh-config");

    let got = |dir: Option<&Path>, env: bool| {
        let c = cred_of_with(dir, env);
        (c.account, c.owner, c.kind)
    };
    let s = |v: &str| Some(v.to_string());
    let cases = [
        (got(Some(&reader), false), ("app-4242".into(), s("acme"), "reader")),
        (got(Some(&owner_dir), false), ("app-777".into(), s("acme"), "writer")),
        (got(Some(&primary), false), ("app-777".into(), s("acme"), "writer")),
        (got(Some(&reader), true), ("env-token".into(), None, "env")),
        (got(None, false), ("ambient".into(), None, "ambient")),
        (
            got(Some(Path::new("/home/u/.config/gh")), false),
            ("ambient".into(), None, "ambient"),
        ),
    ];
    for (got, want) in &cases {
        assert_eq!(got, want);
        let text = format!("{} {:?} {}", got.0, got.1, got.2);
        assert!(!text.contains('/') && !text.contains("gh-config"), "{text}");
        assert!(crate::forge_call_stats::sanitize(&text).is_some(), "{text}");
    }

    // Through the facade: `without_token_env` ignores any env token, so the
    // explicit directory decides.
    let call = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .gh_config_dir(Some(&reader))
    .without_token_env();
    let c = cred_of(&call);
    assert_eq!(
        (c.account.as_str(), c.owner.as_deref(), c.kind),
        ("app-4242", Some("acme"), "reader")
    );

    // An unconfigured writer is booked, not guessed.
    let bare = tempfile::tempdir().unwrap();
    let c = cred_of_with(Some(&bare.path().join(".loom/gh-config-by-owner/acme")), false);
    assert_eq!(c.account, "app-unknown");
}

// ===== 3. bucket from the response headers =====

#[test]
fn an_include_response_books_limit_resource_and_reset_into_the_row_and_the_book() {
    let tmp = tempfile::tempdir().unwrap();
    let reader = tmp.path().join(".loom/gh-config-by-owner/acme/90001");
    let reset = chrono::Utc::now().timestamp() + 1200;
    let gh = stub(
        tmp.path(),
        "gh-headers",
        &format!(
            "printf 'HTTP/2.0 200 OK\\r\\nX-Ratelimit-Limit: 5000\\r\\nX-Ratelimit-Resource: core\\r\\n\
             X-Ratelimit-Remaining: 4900\\r\\nX-Ratelimit-Used: 100\\r\\nX-Ratelimit-Reset: {reset}\\r\\n\\r\\n{{}}'"
        ),
    );
    let call = inv("claim.pr_get", &["api", "--include", "repos/acme/widget/pulls/1"], &gh)
        .gh_config_dir(Some(&reader))
        .without_token_env();
    let lines = raw_lines_after(|| {
        let _ = call.run();
    });
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_eq!((l["rr"].as_str(), l["rst"].as_i64()), (Some("core"), Some(reset)));
    assert_eq!(
        (l["ca"].as_str(), l["co"].as_str(), l["tk"].as_str()),
        (Some("app-90001"), Some("acme"), Some("reader"))
    );
    let held = forge_bucket_book::reading(
        &BucketKey::new("app-90001", "acme", Resource::Core),
        chrono::Utc::now().timestamp(),
    )
    .expect("the header reading is booked");
    assert_eq!((held.limit, held.remaining, held.used), (Some(5000), Some(4900), Some(100)));
}

#[test]
fn without_headers_the_resource_is_the_argv_pool_and_nothing_is_booked() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-ok", "echo '{}'");
    let lines = raw_lines_after(|| {
        let _ = inv("pr.view", &["pr", "view", "1"], &gh).run();
    });
    assert_eq!(lines[0]["rr"], "graphql");
    assert!(lines[0].get("rst").is_none());
}

// ===== 4. pages =====

#[test]
fn paginated_include_rows_count_their_pages_and_bare_paginate_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let three = stub(
        tmp.path(),
        "gh-pages",
        "printf 'HTTP/2.0 200 OK\\r\\nX-Ratelimit-Resource: core\\r\\n\\r\\n[1]\\n\
         HTTP/2.0 200 OK\\r\\n\\r\\n[2]\\nHTTP/1.1 200 OK\\r\\n\\r\\n[3]\\n'",
    );
    let plain = stub(tmp.path(), "gh-plain", "echo '[]'");
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    let _ =
        inv("list.comments", &["api", "--paginate", "--include", "repos/o/r/issues"], &three).run();
    let _ = inv("list.plain", &["api", "--paginate", "repos/o/r/issues"], &plain).run();
    crate::forge_call_stats::set_test_sink_dir(None);

    let now = chrono::Utc::now().timestamp();
    let agg = crate::forge_call_stats::buckets::aggregate_since(
        sink.path(),
        now - 60,
        now + 1,
        crate::forge_call_stats::buckets::GroupBy::Caller,
    );
    let group = |c: &str| {
        agg.groups
            .iter()
            .find(|g| g.key == [c.to_string()])
            .unwrap()
            .clone()
    };
    assert_eq!((group("list.comments").rows, group("list.comments").charged), (1, 3));
    assert_eq!(group("list.plain").charged, 1);
    assert_eq!(group("list.plain").pages_unknown, 1);

    let (pg, pu) = pages(&[OsString::from("run"), OsString::from("download")], false, None);
    assert_eq!((pg, pu), (Some(2), None), "run download is at least two requests");
}

// ===== 5. the free probe stays free =====

#[test]
fn a_rate_limit_probe_is_booked_other_whatever_its_headers_say() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(
        tmp.path(),
        "gh-rl",
        "printf 'HTTP/2.0 200 OK\\r\\nX-Ratelimit-Resource: core\\r\\n\\r\\n{}'",
    );
    let lines = raw_lines_after(|| {
        let _ = inv("api.rate_limit", &["api", "--include", "rate_limit"], &gh).run();
    });
    assert_eq!(
        (lines[0]["p"].as_str(), lines[0]["rr"].as_str()),
        (Some("other"), Some("other"))
    );
}

// ===== 6. loom.forge.calls =====

#[test]
fn recorded_calls_drain_once_as_forge_calls_points() {
    use crate::observability::ops::{capture::capture, forge_calls};
    use crate::telemetry::ops::MetricName;
    let tmp = tempfile::tempdir().unwrap();
    let ok = stub(tmp.path(), "gh-ok", "echo '{}'");
    let fail = stub(tmp.path(), "gh-fail", "echo 'HTTP 404: Not Found' >&2; exit 1");
    let (points, _) = capture(|| {
        let _ = forge_calls::drain_points();
        for _ in 0..4 {
            let _ = inv("metric.view", &["pr", "view", "1"], &ok)
                .identity_scope(None, Some("Acme/widget"))
                .run();
        }
        let _ = inv("metric.view", &["pr", "view", "1"], &fail).run();
        forge_calls::drain_points()
    });
    let ours: Vec<_> = points
        .iter()
        .filter(|p| p.name == MetricName::ForgeCalls && p.labels["caller"] == "metric.view")
        .collect();
    assert_eq!(ours.len(), 2, "{ours:?}");
    let ok_point = ours.iter().find(|p| p.labels["outcome"] == "ok").unwrap();
    assert_eq!(ok_point.value, crate::telemetry::ops::MetricValue::Int(4));
    let keys: Vec<&str> = ok_point.labels.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "account",
            "caller",
            "cred_owner",
            "op",
            "outcome",
            "resource",
            "role",
            "target_owner"
        ]
    );
    assert_eq!(ok_point.labels["target_owner"], "acme");
    assert_eq!(ok_point.labels["resource"], "graphql");
    assert_eq!(ok_point.labels["role"], "writer");
    let err = ours
        .iter()
        .find(|p| p.labels["outcome"] == "error")
        .unwrap();
    assert_eq!(
        (err.value, err.labels["target_owner"].as_str()),
        (crate::telemetry::ops::MetricValue::Int(1), "unknown")
    );

    let (again, _) = capture(forge_calls::drain_points);
    assert!(again.is_empty(), "delta semantics: {again:?}");
}

#[test]
fn without_an_ops_sink_nothing_accumulates() {
    use crate::observability::ops::forge_calls;
    let tmp = tempfile::tempdir().unwrap();
    let ok = stub(tmp.path(), "gh-ok", "echo '{}'");
    let _ = inv("metric.nosink", &["pr", "view", "1"], &ok).run();
    assert!(forge_calls::drain_points().is_empty());
}
