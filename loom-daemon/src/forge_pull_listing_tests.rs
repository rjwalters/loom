//! Unit tests for [`crate::forge_pull_listing`] (#10349).

use super::*;
use std::path::PathBuf;

const FULL_ROW: &str = r#"[{
  "number": 502, "state": "open", "draft": true, "title": "t",
  "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-02T00:00:00Z",
  "user": {"login": "alice"},
  "labels": [{"name": "loom:reviewing"}, {"name": "loom:pr"}],
  "head": {"ref": "feature/issue-77", "sha": "abc"},
  "base": {"ref": "main"},
  "mergeable": true
}]"#;

#[test]
fn parses_every_field_the_passes_read() {
    let rows = parse_rest_pulls(FULL_ROW).unwrap();
    assert_eq!(
        rows,
        vec![RestPull {
            number: 502,
            state: "open".to_string(),
            draft: true,
            title: Some("t".to_string()),
            created_at: Some("2026-10-01T00:00:00Z".to_string()),
            updated_at: Some("2026-10-02T00:00:00Z".to_string()),
            author: Some("alice".to_string()),
            labels: vec!["loom:reviewing".to_string(), "loom:pr".to_string()],
            head_ref: Some("feature/issue-77".to_string()),
            head_sha: Some("abc".to_string()),
            base_ref: Some("main".to_string()),
        }]
    );
    assert!(rows[0].has_label("loom:pr"));
    assert!(!rows[0].has_label("loom:treating"));
}

#[test]
fn a_sparse_row_parses_leniently_and_garbage_does_not() {
    let rows = parse_rest_pulls(r#"[{"number": 3, "draft": null, "head": null}]"#).unwrap();
    assert_eq!(rows[0].number, 3);
    assert!(!rows[0].draft);
    assert_eq!((rows[0].head_sha.as_deref(), rows[0].base_ref.as_deref()), (None, None));
    assert!(rows[0].labels.is_empty());
    assert!(parse_rest_pulls("not json").is_err());
    assert!(parse_rest_pulls(r#"{"message": "Not Found"}"#).is_err());
}

#[test]
fn mergeable_maps_true_false_null_and_absent() {
    assert_eq!(parse_mergeable(r#"{"mergeable": true}"#).unwrap(), Some(true));
    assert_eq!(parse_mergeable(r#"{"mergeable": false}"#).unwrap(), Some(false));
    assert_eq!(parse_mergeable(r#"{"mergeable": null}"#).unwrap(), None);
    assert_eq!(parse_mergeable(r#"{"number": 1}"#).unwrap(), None);
    assert!(parse_mergeable("nope").is_err());
}

#[test]
fn the_url_lists_open_prs_newest_first_one_page_at_a_time() {
    assert_eq!(
        build_pulls_url(Some("o/r"), 2),
        "repos/o/r/pulls?state=open&sort=created&direction=desc&per_page=100&page=2"
    );
    assert!(build_pulls_url(None, 1).starts_with("repos/{owner}/{repo}/pulls?state=open"));
}

/// A fake `gh` that logs its argv, answers page 1 with `page1` rows and every
/// other page with `[]`, with `200 + ETag` unless the caller presents the
/// page's ETag (then `304` + exit 1, like real gh). `pulls/<n>` answers
/// `{"mergeable": false}` with its own ETag on the same rule.
fn write_fake_gh(dir: &Path, page1_rows: usize) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let rows: Vec<String> = (1..=page1_rows)
        .map(|n| format!(r#"{{"number":{n},"head":{{"sha":"s{n}"}}}}"#))
        .collect();
    let page1 = format!("[{}]", rows.join(","));
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *'If-None-Match'*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1 ;;
  *'pulls?state=open'*'&page=1'*)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"p1"\r\n\r\n'
    echo '{page1}' ;;
  *'pulls?state=open'*)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"p2"\r\n\r\n'
    echo '[]' ;;
  *'/pulls/'*)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"m"\r\n\r\n'
    echo '{{"mergeable": false}}' ;;
  *)
    echo 'gh: Something went wrong (HTTP 502)' 1>&2
    exit 1 ;;
esac
"#,
        log = log.display()
    );
    let bin = dir.join("fake-gh.sh");
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, log)
}

fn log_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[cfg(unix)]
#[test]
fn a_second_listing_is_served_by_a_304() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = write_fake_gh(dir.path(), 2);
    let first = list_open_pulls_cached_as("test", &gh, Some(dir.path()), None, 3).unwrap();
    let second = list_open_pulls_cached_as("test", &gh, Some(dir.path()), None, 3).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first, second, "the 304 serves the cached rows");
    let lines = log_lines(&log);
    assert_eq!(lines.len(), 2, "a short page ends paging: {lines:?}");
    assert!(!lines[0].contains("If-None-Match"), "{lines:?}");
    assert!(lines[1].contains(r#"If-None-Match: W/"p1""#), "{lines:?}");
}

#[cfg(unix)]
#[test]
fn a_full_page_pages_on_and_the_cap_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = write_fake_gh(dir.path(), PER_PAGE);
    let rows = list_open_pulls_cached_as("test", &gh, Some(dir.path()), None, 3).unwrap();
    assert_eq!(rows.len(), PER_PAGE);
    assert_eq!(log_lines(&log).len(), 2, "page 2 was short");

    let capped = tempfile::tempdir().unwrap();
    let (gh, log) = write_fake_gh(capped.path(), PER_PAGE);
    let rows = list_open_pulls_cached_as("test", &gh, Some(capped.path()), None, 1).unwrap();
    assert_eq!(rows.len(), PER_PAGE);
    assert_eq!(log_lines(&log).len(), 1, "max_pages = 1 never reads page 2");
}

#[cfg(unix)]
#[test]
fn mergeable_is_a_conditional_single_pr_read() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = write_fake_gh(dir.path(), 0);
    for _ in 0..2 {
        let m = pull_mergeable_cached_as("test", &gh, Some(dir.path()), None, 42).unwrap();
        assert_eq!(m, Some(false));
    }
    let lines = log_lines(&log);
    assert!(lines[0].contains("/pulls/42"), "{lines:?}");
    assert!(lines[1].contains(r#"If-None-Match: W/"m""#), "{lines:?}");
}

#[cfg(unix)]
#[test]
fn errors_carry_stderr_for_the_rate_limit_classifier() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, _) = write_fake_gh(dir.path(), 0);
    let err = conditional_get(
        store::ConditionalRead::new("test", PR_LIST_OPEN),
        &gh,
        Some(dir.path()),
        &store::resolve_target(Some(dir.path()), Some("o/r")),
        "repos/o/r/other",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("HTTP 502"), "{err}");
}
