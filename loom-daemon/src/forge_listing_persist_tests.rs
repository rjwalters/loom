//! Daemon listing cache: resolved-identity key + disk persistence (#9252), and
//! per-caller accounting of 200s and 304s (#9251).
//!
//! Each test opts its own thread into a tempdir store/sink
//! ([`crate::forge_etag_store::set_test_daemon_store_dir`],
//! [`crate::forge_call_stats::set_test_sink_dir`]); no process-global env
//! var is touched, so these run in parallel with everything else.

use super::*;
use crate::forge_etag_store::{daemon_cache_key, resolve_target, set_test_daemon_store_dir};

/// A cwd-agnostic fake `gh` that logs its argv to `log`, answers `200` +
/// `W/"shared"` (with free rate-limit headers) unconditionally, and `304`
/// when that ETag is presented.
fn write_logging_fake_gh(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("fake-gh-logging.sh");
    let script = format!(
        r#"#!/bin/sh
echo "$*" >> {log}
case "$*" in
  *'If-None-Match: W/"shared"'*)
    printf 'HTTP/2.0 304 Not Modified\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 4999\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1
    ;;
  *)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"shared"\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 4998\r\n\r\n'
    printf '[{{"number": 5, "state": "open", "labels": [{{"name": "loom:issue"}}]}}]\n'
    ;;
esac
"#,
        log = log.display()
    );
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn logged_calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// AC1 (#9252) + #9251: the same repo reached from two different `cwd`s (a
/// workspace root and a worktree-like checkout with the same `origin`) shares
/// ONE cache entry — the second call presents `If-None-Match` and gets a free
/// `304` — and both outcomes are recorded against the caller.
#[test]
fn two_cwds_for_the_same_repo_share_one_entry_and_both_outcomes_are_counted() {
    let base = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let sink = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    let worktree = base.path().join("worktree");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    let nwo = format!("test-owner/shared-{}", std::process::id());
    init_git_repo_with_remote(&root, &nwo);
    init_git_repo_with_remote(&worktree, &nwo);
    let log = base.path().join("calls.log");
    let gh = write_logging_fake_gh(base.path(), &log);

    set_test_daemon_store_dir(Some(store.path().to_path_buf()));
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    let caller = "test_shared_entry";
    let first = list_issues_cached_as(caller, &gh, Some(&root), None, "loom:issue", "open");
    let second = list_issues_cached_as(caller, &gh, Some(&worktree), None, "loom:issue", "open");
    let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);
    set_test_daemon_store_dir(None);
    crate::forge_call_stats::set_test_sink_dir(None);

    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first, second);
    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(!calls[0].contains("If-None-Match"), "first call is unconditional");
    assert!(
        calls[1].contains("If-None-Match: W/\"shared\""),
        "the worktree call must reuse the root's entry: {calls:?}"
    );

    let window = report.host_window.expect("sink opted in");
    let row = window.iter().find(|r| r.caller == caller).unwrap();
    assert_eq!((row.pool.as_str(), row.ok, row.not_modified), ("core", 1, 1));
    let core = report.budget.iter().find(|b| b.pool == "core").unwrap();
    assert_eq!(core.source, "headers");
}

/// AC2 (#9252): after a simulated restart (the in-memory entry dropped, the
/// disk store kept) the first call is already conditional and a `304` is
/// served from the disk body.
#[test]
fn an_etag_survives_a_simulated_restart_via_the_disk_store() {
    let base = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let log = base.path().join("calls.log");
    let gh = write_logging_fake_gh(base.path(), &log);
    let repo = format!("test/restart-{}", std::process::id());

    set_test_daemon_store_dir(Some(store.path().to_path_buf()));
    let first = list_issues_cached(&gh, Some(base.path()), Some(&repo), "loom:issue", "open");
    // "Restart": forget only this test's in-memory entry (never the whole map,
    // which parallel tests are using).
    let url = build_issues_url(Some(&repo), "loom:issue", "open");
    let key =
        daemon_cache_key(Some(base.path()), &resolve_target(Some(base.path()), Some(&repo)), &url);
    cache().lock().unwrap().remove(&key);
    let after_restart =
        list_issues_cached(&gh, Some(base.path()), Some(&repo), "loom:issue", "open");
    set_test_daemon_store_dir(None);

    assert_eq!(first.unwrap(), after_restart.unwrap());
    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].contains("If-None-Match: W/\"shared\""),
        "the post-restart call must be conditional: {calls:?}"
    );
    assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 1);
}

/// Test builds default the daemon disk layer OFF: a stub test that never opts
/// in writes nothing to any store directory, and keeps per-`cwd` keys.
#[test]
fn the_daemon_disk_layer_is_off_by_default_in_tests() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let gh = write_logging_fake_gh(dir.path(), &log);
    let repo = format!("test/default-off-{}", std::process::id());
    let url = build_issues_url(Some(&repo), "loom:issue", "open");
    assert!(crate::forge_etag_store::daemon_store_dir().is_none());
    assert!(daemon_cache_key(
        Some(dir.path()),
        &resolve_target(Some(dir.path()), Some(&repo)),
        &url
    )
    .starts_with(&dir.path().display().to_string()));
    list_issues_cached(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open").unwrap();
}

/// #9252 blocking fix: a `304` validates exactly the ETag that was SENT, so it
/// must serve the body stored with that ETag — even when a concurrent writer
/// replaced the entry under the same key between the send and the `304`.
#[test]
fn a_304_serves_the_body_stored_with_the_sent_etag_not_a_swapped_entry() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let gh = write_logging_fake_gh(dir.path(), &log);
    let repo = format!("test/swap-race-{}", std::process::id());
    let first = list_issues_cached(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open");
    let first = first.unwrap();

    // Mid-flight, another writer stores a DIFFERENT (etag, body) under the key.
    let url = build_issues_url(Some(&repo), "loom:issue", "open");
    let key =
        daemon_cache_key(Some(dir.path()), &resolve_target(Some(dir.path()), Some(&repo)), &url);
    let swapped = Arc::new(vec![RestIssue {
        number: 666,
        ..first[0].clone()
    }]);
    let hook_key = key.clone();
    super::set_after_send_hook(Some(Box::new(move || {
        let entry = CacheEntry {
            etag: "W/\"other\"".to_string(),
            issues: Arc::clone(&swapped),
        };
        cache().lock().unwrap().insert(hook_key.clone(), entry);
    })));
    let second = list_issues_cached(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open");
    super::set_after_send_hook(None);

    assert!(logged_calls(&log)[1].contains("If-None-Match: W/\"shared\""));
    assert_eq!(second.unwrap(), first, "the 304 must serve the W/\"shared\" body");
    assert_eq!(cache().lock().unwrap().get(&key).unwrap().etag, "W/\"other\"");
}

/// #9252 blocking fix: with no explicit repo, the request URL names the
/// `origin`-resolved repo the key uses — never gh's `{owner}/{repo}`
/// placeholder, which prefers an `upstream` remote and would query a
/// different repo under the same key.
/// `#[serial]`: relies on `LOOM_REPO` being unset, which serial tests mutate.
#[test]
#[serial_test::serial]
fn the_url_names_the_origin_repo_even_when_an_upstream_remote_exists() {
    let dir = tempfile::tempdir().unwrap();
    let origin = format!("fork-owner/key-coherence-{}", std::process::id());
    init_git_repo_with_remote(dir.path(), &origin);
    assert!(Command::new("git")
        .args([
            "remote",
            "add",
            "upstream",
            "https://github.com/upstream-owner/other.git"
        ])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let log = dir.path().join("calls.log");
    let gh = write_logging_fake_gh(dir.path(), &log);

    list_issues_cached(&gh, Some(dir.path()), None, "loom:issue", "open").unwrap();
    let call = &logged_calls(&log)[0];
    assert!(call.contains(&format!("repos/{origin}/issues?")), "{call}");
    assert!(!call.contains("{owner}") && !call.contains("upstream-owner"), "{call}");
    assert!(call.contains("--hostname github.com"), "{call}");

    let target = resolve_target(Some(dir.path()), None);
    assert_eq!(target.repo.as_deref(), Some(origin.as_str()));
}

/// Resolution failure keeps today's behaviour: the placeholder URL, keyed by
/// the raw `cwd`.
/// `#[serial]`: relies on `LOOM_REPO` being unset, which serial tests mutate.
#[test]
#[serial_test::serial]
fn an_unresolvable_cwd_keeps_the_placeholder_url() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let gh = write_logging_fake_gh(dir.path(), &log);
    list_issues_cached(&gh, Some(dir.path()), None, "loom:issue", "open").unwrap();
    let call = &logged_calls(&log)[0];
    assert!(call.contains("repos/{owner}/{repo}/issues?"), "{call}");
    assert!(!call.contains("--hostname"), "{call}");
}
