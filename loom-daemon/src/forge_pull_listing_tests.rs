//! Unit tests for [`crate::forge_pull_listing`] (#10349).

use super::*;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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
        Kind::Listing,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("HTTP 502"), "{err}");
}

/// Clears this thread's opted-in daemon store on drop, even on a panic.
struct StoreGuard;
impl Drop for StoreGuard {
    fn drop(&mut self) {
        store::set_test_daemon_store_dir(None);
    }
}

#[cfg(unix)]
#[test]
fn a_per_pr_entry_stores_only_mergeable_and_stale_ones_are_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    // 0700 regardless of umask: the store purges a dir others could write
    // (a umask-002 host would otherwise wipe the seeded files).
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&store_dir)
            .unwrap();
    }
    store::set_test_daemon_store_dir(Some(store_dir.clone()));
    let _guard = StoreGuard;
    let (gh, _) = write_fake_gh(dir.path(), 0);
    let old = SystemTime::now() - PULL_ENTRY_MAX_AGE - Duration::from_secs(60);

    // A closed PR's leftovers, last written beyond the max age: one on disk,
    // one only in the hot layer. Plus a fresh per-PR file and an old listing
    // page, neither of which may be touched.
    let stale_disk = store_dir.join(format!("{PULL_PREFIX}00000000deadbeef.json"));
    let fresh_disk = store_dir.join(format!("{PULL_PREFIX}00000000cafef00d.json"));
    let old_listing = store_dir.join("listing-00000000feedface.json");
    for p in [&stale_disk, &fresh_disk, &old_listing] {
        std::fs::write(p, r#"{"etag":"W/\"x\"","body":"{}"}"#).unwrap();
    }
    for p in [&stale_disk, &old_listing] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let hot_key = format!("stale-hot-{}", dir.path().display());
    let listing_key = format!("old-listing-hot-{}", dir.path().display());
    {
        let mut guard = cache().lock().unwrap();
        let entry = |pull_written| CacheEntry {
            etag: "W/\"x\"".to_string(),
            body: Arc::new("{}".to_string()),
            pull_written,
        };
        guard.insert(hot_key.clone(), entry(Some(old)));
        guard.insert(listing_key.clone(), entry(None));
    }

    let m = pull_mergeable_cached_as("test", &gh, Some(dir.path()), None, 42).unwrap();
    assert_eq!(m, Some(false));

    assert!(!stale_disk.exists(), "a per-PR file past the max age is pruned");
    assert!(fresh_disk.exists(), "a fresh per-PR file survives");
    assert!(old_listing.exists(), "listing pages are never age-pruned");
    let guard = cache().lock().unwrap();
    assert!(!guard.contains_key(&hot_key), "a stale hot per-PR entry is pruned");
    assert!(guard.contains_key(&listing_key), "a hot listing entry is not");
    drop(guard);

    // The new entry holds only the mergeable value, never the full PR body.
    let written: Vec<_> = std::fs::read_dir(&store_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p != &fresh_disk && p != &old_listing)
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    let entry = store::read_disk_entry(&written[0]).unwrap();
    assert_eq!(entry.body, r#"{"mergeable":false}"#);
    cache().lock().unwrap().remove(&hot_key);
    cache().lock().unwrap().remove(&listing_key);
}

/// A fake `gh` for `pulls/<n>/files` pages (#10382): pages `1..=full_pages`
/// carry [`PER_PAGE`] files each, the next page one file. Each page has its
/// own ETag and answers `304` when that ETag is presented. `pulls?head=`
/// answers with `head_rows`.
fn write_files_gh(dir: &Path, full_pages: usize, head_rows: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *'pulls?head='*)
    printf 'HTTP/2.0 200 OK\r\n\r\n'
    echo '{head_rows}'
    exit 0 ;;
esac
p="${{3##*page=}}"
case "$*" in
  *"If-None-Match: W/\"f$p\""*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nEtag: W/"f%s"\r\n\r\n' "$p"
if [ "$p" -le {full_pages} ]; then n={per_page}; else n=1; fi
i=0; sep='['
while [ "$i" -lt "$n" ]; do
  printf '%s{{"filename":"p%s/f%s.rs","patch":"@@ big"}}' "$sep" "$p" "$i"; sep=','; i=$((i+1))
done
echo ']'
"#,
        log = log.display(),
        per_page = PER_PAGE,
    );
    let bin = dir.join("fake-files-gh.sh");
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, log)
}

#[test]
fn file_rows_parse_from_rest_objects_and_the_reduced_name_array() {
    let rest = r#"[{"filename":"a.rs","status":"modified","patch":"@@"},{"filename":"b.md"}]"#;
    assert_eq!(parse_files(rest).unwrap(), vec!["a.rs", "b.md"]);
    assert_eq!(parse_files(r#"["a.rs","b.md"]"#).unwrap(), vec!["a.rs", "b.md"]);
    assert!(parse_files(r#"{"message":"Not Found"}"#).is_err());
}

#[cfg(unix)]
#[test]
fn changed_files_page_until_a_short_page_and_union_the_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = write_files_gh(dir.path(), 1, "[]");
    let files = pull_files_cached_as("test", &gh, Some(dir.path()), None, 7).unwrap();
    assert_eq!(files.len(), PER_PAGE + 1);
    assert!(files.contains("p1/f0.rs") && files.contains("p2/f0.rs"), "{files:?}");
    let lines = log_lines(&log);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("pulls/7/files?per_page=100&page=1"), "{lines:?}");
    assert!(lines[1].contains("pulls/7/files?per_page=100&page=2"), "{lines:?}");
}

#[cfg(unix)]
#[test]
fn a_walk_that_fills_githubs_file_cap_is_an_error_not_a_truncated_set() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = write_files_gh(dir.path(), MAX_FILE_PAGES, "[]");
    let err = pull_files_cached_as("test", &gh, Some(dir.path()), None, 8).unwrap_err();
    assert!(err.to_string().contains("truncated"), "{err}");
    assert_eq!(log_lines(&log).len(), MAX_FILE_PAGES, "never reads past the cap");
}

#[cfg(unix)]
#[test]
fn a_files_entry_stores_only_names_serves_a_304_and_is_age_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&store_dir)
            .unwrap();
    }
    store::set_test_daemon_store_dir(Some(store_dir.clone()));
    let _guard = StoreGuard;
    let old = SystemTime::now() - PULL_ENTRY_MAX_AGE - Duration::from_secs(60);
    let stale = store_dir.join(format!("{FILES_PREFIX}00000000deadbeef.json"));
    let old_listing = store_dir.join("listing-00000000feedface.json");
    for p in [&stale, &old_listing] {
        std::fs::write(p, r#"{"etag":"W/\"x\"","body":"[]"}"#).unwrap();
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(old).unwrap();
    }

    let (gh, log) = write_files_gh(dir.path(), 0, "[]");
    let first = pull_files_cached_as("test", &gh, Some(dir.path()), None, 9).unwrap();
    assert!(!stale.exists(), "a files- entry past the max age is pruned");
    assert!(old_listing.exists(), "listing pages are never age-pruned");

    let written: Vec<_> = std::fs::read_dir(&store_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p != &old_listing)
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    let name = written[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(name.starts_with(FILES_PREFIX), "{name}");
    let entry = store::read_disk_entry(&written[0]).unwrap();
    assert_eq!(entry.body, r#"["p1/f0.rs"]"#, "names only, never the patch");

    // A second read presents the ETag; the 304 serves the reduced body.
    let second = pull_files_cached_as("test", &gh, Some(dir.path()), None, 9).unwrap();
    assert_eq!(first, second);
    let lines = log_lines(&log);
    assert!(lines[1].contains(r#"If-None-Match: W/"f1""#), "{lines:?}");
    cache().lock().unwrap().retain(|_, e| e.etag != r#"W/"f1""#);
}

#[cfg(unix)]
#[test]
fn the_by_head_lookup_keeps_only_open_rows_on_that_exact_branch() {
    let dir = tempfile::tempdir().unwrap();
    // GitHub ignores a `head` it cannot resolve and lists every open PR.
    let rows = r#"[{"number":1,"state":"open","head":{"ref":"main"}},
{"number":2,"state":"open","head":{"ref":"feature/issue-5"}},
{"number":3,"state":"closed","head":{"ref":"feature/issue-5"}},
{"number":4,"state":"open","head":{"ref":"feature/issue-50"}}]"#
        .replace('\n', "");
    let (gh, log) = write_files_gh(dir.path(), 0, &rows);
    let prs = open_pulls_for_head_as("test", &gh, Some(dir.path()), Some("o/r"), "feature/issue-5");
    assert_eq!(prs.unwrap(), vec![2]);
    let lines = log_lines(&log);
    assert!(
        lines[0].contains("repos/o/r/pulls?head=o:feature/issue-5&state=open&per_page=100"),
        "{lines:?}"
    );
    assert!(!lines[0].contains("If-None-Match"), "{lines:?}");

    let (gh, _) = write_files_gh(dir.path(), 0, "[]");
    let none = open_pulls_for_head_as("test", &gh, Some(dir.path()), Some("o/r"), "feature/x");
    assert_eq!(none.unwrap(), Vec::<u32>::new(), "a definitive empty answer");
}

#[cfg(unix)]
#[test]
fn a_failed_by_head_lookup_is_an_error_never_an_empty_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, _) = write_fake_gh(dir.path(), 0); // falls to its HTTP 502 arm
    let err = open_pulls_for_head_as("test", &gh, Some(dir.path()), Some("o/r"), "feature/x");
    assert!(err.unwrap_err().to_string().contains("HTTP 502"));
}
