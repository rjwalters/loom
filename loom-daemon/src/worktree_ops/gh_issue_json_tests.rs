//! #10512: the reaper's issue probes ([`super::issue_state_rest`],
//! [`super::issue_closed_at_rest`]) are conditional reads through the shared
//! ETag store — a re-probe of an unchanged issue is a free `304`, the entry
//! survives a restart on disk, and every failure keeps its fail-closed answer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{issue_closed_at_rest, issue_state_rest};
use crate::forge_call_stats;
use crate::forge_etag_store as store;
use crate::types::ForgeCallCounts;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const CLOSED: &str = r#"{"state":"closed","closed_at":"2026-10-06T00:00:00Z"}"#;

/// A `gh` stub logging its argv: `304` when `If-None-Match` is sent, else
/// `200` with an ETag and `body`.
fn conditional_gh(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let gh = dir.join("gh-issue");
    let script = format!(
        r#"#!/bin/sh
echo "$*" >> {log}
case "$*" in
  *If-None-Match*) printf 'HTTP/2.0 304 Not Modified\r\nEtag: W/"i1"\r\n\r\n'; exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nEtag: W/"i1"\r\n\r\n%s' '{body}'"#,
        log = log.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (gh, log)
}

/// Runs `body` with `gh` as the resolved binary and a private call-stats
/// sink; returns the host-window rows.
fn with_gh(gh: &Path, body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_GH_BIN", gh);
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    std::env::remove_var("LOOM_GH_BIN");
    report.host_window.unwrap_or_default()
}

fn counts(rows: &[ForgeCallCounts], caller: &str) -> (u64, u64) {
    let of = |f: fn(&ForgeCallCounts) -> u64| -> u64 {
        rows.iter().filter(|r| r.caller == caller).map(f).sum()
    };
    (of(|r| r.ok), of(|r| r.not_modified))
}

fn argv(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Test 1: `200` then `If-None-Match` → `304`, both `CLOSED`; the
/// `closed_at` read of the same issue is also a `304`.
#[test]
#[serial(loom_config_env)]
fn a_reprobe_of_an_unchanged_issue_is_a_free_304() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let (gh, log) = conditional_gh(tmp.path(), CLOSED);
    let mut got = Vec::new();
    let mut closed_at = None;
    let rows = with_gh(&gh, || {
        got.push(issue_state_rest(&root, 7));
        got.push(issue_state_rest(&root, 7));
        closed_at = Some(issue_closed_at_rest(&root, 7));
    });
    assert_eq!(got, ["CLOSED", "CLOSED"]);
    assert_eq!(closed_at, Some(Some("2026-10-06T00:00:00Z".to_string())));
    let lines = argv(&log);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(!lines[0].contains("If-None-Match"), "{lines:?}");
    assert!(lines[1].contains(r#"If-None-Match: W/"i1""#), "{lines:?}");
    assert!(lines[2].contains("If-None-Match"), "{lines:?}");
    assert_eq!(counts(&rows, "worktree.issue_state_rest"), (1, 1), "{rows:?}");
    assert_eq!(counts(&rows, "worktree.issue_closed_at"), (0, 1), "{rows:?}");
}

/// Test 2: a `404` and a non-HTTP stdout are "no answer" — `UNKNOWN` / `None`.
#[test]
#[serial(loom_config_env)]
fn a_404_or_garbage_is_unknown_and_none() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let not_found = tmp.path().join("gh-404");
    std::fs::write(
        &not_found,
        "#!/bin/sh\nprintf 'HTTP/2.0 404 Not Found\\r\\n\\r\\n{\"message\":\"Not Found\"}'\nexit 1\n",
    )
    .unwrap();
    let garbage = tmp.path().join("gh-garbage");
    std::fs::write(&garbage, "#!/bin/sh\necho closed\n").unwrap();
    for p in [&not_found, &garbage] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (mut state, mut closed_at) = (None, None);
    with_gh(&not_found, || {
        state = Some(issue_state_rest(&root, 8));
        closed_at = Some(issue_closed_at_rest(&root, 8));
    });
    assert_eq!(state.as_deref(), Some("UNKNOWN"));
    assert_eq!(closed_at, Some(None));
    with_gh(&garbage, || {
        state = Some(issue_state_rest(&root, 9));
        closed_at = Some(issue_closed_at_rest(&root, 9));
    });
    assert_eq!(state.as_deref(), Some("UNKNOWN"), "non-HTTP stdout");
    assert_eq!(closed_at, Some(None));
}

/// Test 3: an entry already on disk (a previous daemon's) makes the very
/// first call of a fresh process conditional — restart survival.
#[test]
#[serial(loom_config_env)]
fn a_persisted_entry_makes_the_first_call_conditional() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let store_dir = tmp.path().join("store");
    store::set_test_daemon_store_dir(Some(store_dir.clone()));
    let url = "repos/{owner}/{repo}/issues/11";
    let key = store::daemon_cache_key(Some(&root), &store::resolve_target(Some(&root), None), url);
    store::write_disk_entry(
        &store::entry_path_with_prefix(&store_dir, "issue-", &key),
        &store::DiskEntry {
            etag: "W/\"i1\"".to_string(),
            body: CLOSED.to_string(),
        },
    );
    // The stub would answer OPEN on a 200: only the stored body says CLOSED.
    let (gh, log) = conditional_gh(tmp.path(), r#"{"state":"open","closed_at":null}"#);
    let mut state = None;
    let rows = with_gh(&gh, || state = Some(issue_state_rest(&root, 11)));
    store::set_test_daemon_store_dir(None);
    assert_eq!(state.as_deref(), Some("CLOSED"));
    let lines = argv(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(r#"If-None-Match: W/"i1""#), "{lines:?}");
    assert_eq!(counts(&rows, "worktree.issue_state_rest"), (0, 1), "{rows:?}");
}

/// Test 4: an explicit `LOOM_REPO` names the repo in the URL, not the
/// checkout (or gh's placeholder).
#[test]
#[serial(loom_config_env)]
fn loom_repo_names_the_repo_in_the_url() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let (gh, log) = conditional_gh(tmp.path(), CLOSED);
    std::env::set_var("LOOM_REPO", "acme/widget-10512");
    let mut state = None;
    with_gh(&gh, || state = Some(issue_state_rest(&root, 12)));
    std::env::remove_var("LOOM_REPO");
    assert_eq!(state.as_deref(), Some("CLOSED"));
    let lines = argv(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("repos/acme/widget-10512/issues/12"), "{lines:?}");
    assert!(!lines[0].contains("{owner}"), "{lines:?}");
}
