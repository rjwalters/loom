#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089 (increment 5): the three hand-instrumented `gh api --include`
//! surfaces — the shared ETag store, the CI poller's client and the fleet
//! store transport — now run through [`GhInvocation`]. They are recorded
//! exactly once per call by the facade (their manual `record_gh_api` calls
//! are gone, so nothing is double-counted), with the status-line
//! classification they had: a `304` is a free `not_modified`, not an error.

use super::GhInvocation;
use crate::forge_call_stats;
use crate::forge_call_stats::ops;
use crate::forge_etag_store::{fetch_conditional, ConditionalRead, Target};
use crate::gh_invocation::{AccessIntent, GhTarget, Operation};
use crate::types::ForgeCallCounts;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn rows_after(body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    report.host_window.unwrap_or_default()
}

fn row<'a>(rows: &'a [ForgeCallCounts], caller: &str) -> &'a ForgeCallCounts {
    let mut matching = rows.iter().filter(|r| r.caller == caller);
    let found = matching
        .next()
        .unwrap_or_else(|| panic!("no {caller} row in {rows:?}"));
    assert!(matching.next().is_none(), "one pool per caller: {rows:?}");
    found
}

/// A `gh` stub that logs its argv and answers like `gh api --include`:
/// a `304` (exit 1, as gh does) when an `If-None-Match` header is sent,
/// else a `200` carrying an ETag and core rate-limit headers.
fn conditional_stub(dir: &Path, log: &Path) -> PathBuf {
    stub(
        dir,
        "gh-conditional",
        &format!(
            r#"echo "$*" >> {log}
case "$*" in
  *If-None-Match*) printf 'HTTP/2.0 304 Not Modified\r\nEtag: W/"e1"\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 4000\r\n\r\n'; exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nEtag: W/"e1"\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 4001\r\n\r\n[]'"#,
            log = log.display()
        ),
    )
}

#[test]
fn etag_store_fetch_is_counted_once_per_call_and_a_304_is_free() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("gh.log");
    let gh = conditional_stub(tmp.path(), &log);
    let target = Target {
        repo: Some("o/r".to_string()),
        host: None,
    };
    let url = "repos/o/r/issues?labels=loom:issue";
    let mut statuses = Vec::new();
    let rows = rows_after(|| {
        for etag in [None, Some("W/\"e1\""), Some("W/\"e1\"")] {
            let (_, response, _) = fetch_conditional(
                ConditionalRead::new("work_finder", ops::ISSUE_LIST),
                &gh,
                Some(tmp.path()),
                &target,
                url,
                etag,
            )
            .unwrap();
            statuses.push(response.map(|r| r.status));
        }
    });
    assert_eq!(statuses, [Some(200), Some(304), Some(304)]);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 3, "one spawn per call");
    let counted = row(&rows, "work_finder");
    assert_eq!(counted.pool, "core");
    assert_eq!((counted.ok, counted.not_modified, counted.error), (1, 2, 0), "{rows:?}");
}

#[test]
fn an_explicit_config_dir_wins_over_the_working_directory_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let reader = tmp.path().join("reader-app");
    let base = GhInvocation::new(
        Operation::new("test.config_dir"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(5),
    )
    .current_dir(tmp.path());
    let config_dir = |inv: &GhInvocation| {
        inv.env_plan(None)
            .into_iter()
            .find(|e| e.key == "GH_CONFIG_DIR")
            .and_then(|e| e.value)
    };
    let explicit = base.clone().gh_config_dir(Some(&reader));
    assert_eq!(config_dir(&explicit), Some(reader.clone().into_os_string()));
    assert_eq!(config_dir(&base.gh_config_dir(None)), None, "None keeps the (empty) lookup");
}

#[test]
#[serial(loom_config_env)]
fn ci_poller_reads_and_downloads_are_counted_under_their_operations() {
    use crate::ci_telemetry::api::{ApiError, GhCliApi, GithubApi};
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("gh.log");
    let gh = stub(
        tmp.path(),
        "gh-ci",
        &format!(
            r#"echo "$*" >> {log}
case "$1" in
  run) echo 'HTTP 404: artifact not found' >&2; exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 3000\r\n\r\n{{"total_count":0}}'"#,
            log = log.display()
        ),
    );
    std::env::set_var("LOOM_GH_BIN", &gh);
    let api = GhCliApi::from_env();
    let (mut read, mut download) = (None, None);
    let rows = rows_after(|| {
        read = Some(api.get("repos/o/r/actions/runs?per_page=5", None));
        download = Some(api.download_artifact("o/r", 7, "logs", tmp.path()));
    });
    std::env::remove_var("LOOM_GH_BIN");
    assert_eq!(read.unwrap().unwrap().status, 200);
    assert!(matches!(download, Some(Err(ApiError::Transport(_)))), "{download:?}");
    let reads = row(&rows, "ci_telemetry");
    assert_eq!((reads.pool.as_str(), reads.ok, reads.error), ("core", 1, 0), "{rows:?}");
    assert_eq!(row(&rows, "ci_telemetry.download").error, 1, "{rows:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
}

#[test]
#[serial(loom_config_env)]
fn fleet_store_read_and_write_are_counted_and_the_body_reaches_gh() {
    use crate::fleet_store::fetch::Transport;
    use crate::fleet_store::gh::GhTransport;
    let tmp = tempfile::tempdir().unwrap();
    // Writes echo the `--input` file back as the body, so the test sees
    // exactly what gh would have sent.
    let gh = stub(
        tmp.path(),
        "gh-store",
        r#"input=""; prev=""
for a in "$@"; do [ "$prev" = "--input" ] && input="$a"; prev="$a"; done
printf 'HTTP/2.0 200 OK\r\nEtag: "s1"\r\nX-Ratelimit-Resource: core\r\n\r\n'
if [ -n "$input" ]; then cat "$input"; else printf '{"read":true}'; fi"#,
    );
    std::env::set_var("LOOM_GH_BIN", &gh);
    let transport = GhTransport::new(tmp.path(), "o/store");
    let (mut read, mut wrote) = (None, None);
    let rows = rows_after(|| {
        read = Some(
            transport
                .get("repos/o/store/contents/a.json", None, None)
                .unwrap(),
        );
        wrote = Some(
            transport
                .write_raw("PUT", "repos/o/store/contents/a.json", &serde_json::json!({"k": 1}))
                .unwrap(),
        );
    });
    std::env::remove_var("LOOM_GH_BIN");
    let read = read.unwrap();
    assert_eq!((read.status, read.body.as_str()), (200, r#"{"read":true}"#));
    assert_eq!(wrote.unwrap().body, r#"{"k":1}"#, "the request body reaches gh via --input");
    assert_eq!(row(&rows, "fleet_store").ok, 1, "{rows:?}");
    assert_eq!(row(&rows, "fleet_store_write").ok, 1, "{rows:?}");
}
