//! `GhWorkSource`'s two-listing merge (#9244 §4) against a scripted fake `gh`.

use super::*;
use std::os::unix::fs::PermissionsExt;

const ISSUES: &str = r#"[
  {"number":1,"labels":[{"name":"loom:issue"},{"name":"loom:operator-priority"}],"created_at":"2026-01-01T00:00:00Z"},
  {"number":2,"labels":[{"name":"loom:issue"}],"created_at":"2026-01-02T00:00:00Z"}
]"#;

const STARRED: &str = r#"[
  {"number":1,"labels":[{"name":"loom:issue"},{"name":"loom:operator-priority"}]},
  {"number":3,"labels":[{"name":"loom:triage"},{"name":"loom:operator-priority"}]},
  {"number":4,"labels":[{"name":"loom:operator-priority"}],"pull_request":{}},
  {"number":5,"labels":[{"name":"loom:building"},{"name":"loom:operator-priority"}]}
]"#;

/// A fake `gh` answering the two listings and the timeline read, logging
/// every timeline call. The starred listing fails while `fail-starred` exists.
fn fake_gh(dir: &Path) -> PathBuf {
    let d = dir.display();
    std::fs::write(dir.join("issues.json"), ISSUES).unwrap();
    std::fs::write(dir.join("starred.json"), STARRED).unwrap();
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *labels=loom:issue\&*) printf 'HTTP/2.0 200 OK\r\n\r\n'; cat {d}/issues.json ;;
  *labels=loom:operator-priority\&*)
    if [ -f {d}/fail-starred ]; then echo 'HTTP 502' >&2; exit 1; fi
    printf 'HTTP/2.0 200 OK\r\n\r\n'; cat {d}/starred.json ;;
  *timeline*) echo "$*" >> {d}/timeline.log; echo '2026-09-01T00:00:00Z' ;;
  *) exit 1 ;;
esac
"#
    );
    let path = dir.join("gh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn source(dir: &Path) -> GhWorkSource {
    GhWorkSource {
        gh_bin: fake_gh(dir),
        repo: Some("acme/app".into()),
        cwd: Some(dir.to_path_buf()),
    }
}

fn timeline_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("timeline.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn starred_rows_are_merged_deduped_and_stamped_with_starred_at() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(dir.path());
    let items = src.list_ready_issues().unwrap();
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    // #1 deduped, #4 (a PR) and #5 (already claimed) dropped.
    assert_eq!(numbers, vec![1, 2, 3]);
    assert_eq!(items[0].operator_priority_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    assert_eq!(items[1].operator_priority_at, None, "unstarred");
    assert_eq!(items[2].operator_priority_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    let calls = timeline_calls(dir.path());
    assert_eq!(calls.len(), 2, "one timeline read per starred issue: {calls:?}");
    assert!(calls.iter().all(|c| c.contains("repos/acme/app/issues/")));

    // The next tick reads no timeline at all: starred-at is cached.
    src.list_ready_issues().unwrap();
    assert_eq!(timeline_calls(dir.path()).len(), 2);
}

#[test]
fn a_failed_starred_listing_keeps_the_loom_issue_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(dir.path());
    std::fs::write(dir.path().join("fail-starred"), "").unwrap();
    let items = src.list_ready_issues().expect("loom:issue rows still used");
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![1, 2]);
}
