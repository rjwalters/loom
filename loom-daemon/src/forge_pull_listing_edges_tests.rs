//! Open-PR listing edges (#10382): rows deduped by number, a listing that
//! moves mid-walk is an error, the #6171 one-time credential-refresh retry on
//! a 404, and pre-#10382 per-PR entries read as a cache miss.

use super::*;
use std::cell::Cell;

/// A listing body of open PRs `numbers`.
fn rows(numbers: impl IntoIterator<Item = u32>) -> String {
    let rows: Vec<String> = numbers
        .into_iter()
        .map(|n| format!(r#"{{"number":{n},"state":"open"}}"#))
        .collect();
    format!("[{}]", rows.join(","))
}

/// A paging stub: page `P` answers `200` + ETag `pP` with `pP.json` (`304`
/// when that ETag is presented). When `shift_on_p2` exists, the page-2 read
/// first swaps page 1 for `p1b.json` under ETag `p1b` (a mid-walk change).
fn paging_stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let script = format!(
        r#"#!/bin/sh
d='{dir}'
printf '%s\n' "$*" >> '{log}'
p="${{3##*page=}}"
if [ "$p" = 2 ] && [ -e "$d/shift_on_p2" ]; then touch "$d/shifted"; fi
f="$d/p$p.json"; e="p$p"
if [ "$p" = 1 ] && [ -e "$d/shifted" ]; then f="$d/p1b.json"; e="p1b"; fi
case "$*" in *"If-None-Match: W/\"$e\""*)
  printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
  exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s"\r\n\r\n' "$e"
cat "$f"
"#,
        dir = dir.display(),
        log = log.display(),
    );
    let bin = dir.join("fake-paging-gh.sh");
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, log)
}

#[cfg(unix)]
#[test]
fn a_pr_repeated_across_pages_appears_once() {
    let dir = tempfile::tempdir().unwrap();
    // Page 1's last row repeats as page 2's first.
    std::fs::write(dir.path().join("p1.json"), rows(1..=100)).unwrap();
    std::fs::write(dir.path().join("p2.json"), rows([100, 101])).unwrap();
    let (gh, log) = paging_stub(dir.path());
    let got = list_open_pulls_cached_as("test", &gh, Some(dir.path()), None, 10).unwrap();
    let numbers: Vec<u32> = got.iter().map(|r| r.number).collect();
    assert_eq!(numbers, (1..=101).collect::<Vec<_>>());
    let lines = log_lines(&log);
    assert_eq!(lines.len(), 3, "page 1, page 2, page 1 revalidated: {lines:?}");
    assert!(lines[2].contains(r#"If-None-Match: W/"p1""#), "a free 304: {lines:?}");
}

#[cfg(unix)]
#[test]
fn a_listing_that_moves_mid_walk_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("p1.json"), rows(1..=100)).unwrap();
    std::fs::write(dir.path().join("p2.json"), rows([101])).unwrap();
    // A new PR shifted everything down one row after page 1 was read.
    std::fs::write(dir.path().join("p1b.json"), rows(0..=99)).unwrap();
    std::fs::write(dir.path().join("shift_on_p2"), "").unwrap();
    let (gh, log) = paging_stub(dir.path());
    let err = list_open_pulls_cached_as("test", &gh, Some(dir.path()), None, 10).unwrap_err();
    assert!(err.to_string().contains("changed mid-walk"), "{err}");
    assert_eq!(log_lines(&log).len(), 3, "{:?}", log_lines(&log));
}

/// A stub whose first call fails with `stderr` (exit 1) and whose later calls
/// answer one short listing page.
fn fail_once_stub(dir: &Path, stderr: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
if [ "$(wc -l < '{log}')" -eq 1 ]; then
  echo '{stderr}' 1>&2
  exit 1
fi
printf 'HTTP/2.0 200 OK\r\n\r\n'
echo '{body}'
"#,
        log = log.display(),
        body = rows([7]),
    );
    let bin = dir.join("fake-fail-once-gh.sh");
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, log)
}

const NOT_FOUND: &str = "gh: Not Found (HTTP 404)";

#[cfg(unix)]
#[test]
fn a_registered_workspace_404_refreshes_once_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fail_once_stub(dir.path(), NOT_FOUND);
    let refreshes = Cell::new(0);
    let refresh = |root: &Path| {
        assert_eq!(root, dir.path());
        refreshes.set(refreshes.get() + 1);
        true
    };
    let got =
        list_open_pulls_with_refresh("test", &gh, Some(dir.path()), Some("o/r"), 10, &refresh)
            .expect("the retry after the forced refresh succeeds");
    assert_eq!(got.iter().map(|r| r.number).collect::<Vec<_>>(), vec![7]);
    assert_eq!(refreshes.get(), 1);
    assert_eq!(log_lines(&log).len(), 2, "exactly one retry");
}

#[cfg(unix)]
#[test]
fn a_404_whose_refresh_does_nothing_is_the_original_error() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fail_once_stub(dir.path(), NOT_FOUND);
    let refresh = |_: &Path| false;
    let err =
        list_open_pulls_with_refresh("test", &gh, Some(dir.path()), Some("o/r"), 10, &refresh)
            .unwrap_err();
    assert!(err.to_string().contains("HTTP 404"), "{err}");
    assert_eq!(log_lines(&log).len(), 1);
}

#[cfg(unix)]
#[test]
fn a_rate_limit_403_or_a_cwd_less_404_never_refreshes() {
    let never = |_: &Path| -> bool { panic!("no refresh for this failure") };
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fail_once_stub(dir.path(), "gh: API rate limit exceeded (HTTP 403)");
    let err = list_open_pulls_with_refresh("test", &gh, Some(dir.path()), Some("o/r"), 10, &never)
        .unwrap_err();
    assert!(err.to_string().contains("HTTP 403"), "{err}");
    assert_eq!(log_lines(&log).len(), 1);

    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fail_once_stub(dir.path(), NOT_FOUND);
    let repo = format!("o/no-cwd-{}", std::process::id());
    let err = list_open_pulls_with_refresh("test", &gh, None, Some(&repo), 10, &never);
    assert!(err.unwrap_err().to_string().contains("HTTP 404"));
    assert_eq!(log_lines(&log).len(), 1, "no checkout root, nothing to refresh");
}

/// A per-PR entry written before #10382 (`{"mergeable"}` only) must not be
/// revalidated — its `304` would serve no head forever. It is re-read
/// unconditionally and rewritten with the head.
#[cfg(unix)]
#[test]
fn a_legacy_mergeable_only_entry_is_a_miss_and_is_rewritten_with_the_head() {
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
    let (gh, log) = write_fake_gh(dir.path(), 0);
    let target = resolve(Some(dir.path()), None);
    let repo_path = target.repo.as_deref().unwrap_or("{owner}/{repo}");
    let url = format!("repos/{repo_path}/pulls/42");
    let key = store::daemon_cache_key(Some(dir.path()), &target, &url);
    let path = store::entry_path_with_prefix(&store_dir, PULL_PREFIX, &key);
    let legacy = store::DiskEntry {
        etag: r#"W/"m""#.to_string(),
        body: r#"{"mergeable":true}"#.to_string(),
    };
    store::write_disk_entry(&path, &legacy);

    let got = pull_state_cached_as("test", &gh, Some(dir.path()), None, 42).unwrap();
    assert_eq!((got.mergeable, got.head_sha.as_deref()), (Some(false), Some("h")));
    let lines = log_lines(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].contains("If-None-Match"), "the legacy ETag is not sent: {lines:?}");
    let entry = store::read_disk_entry(&path).unwrap();
    assert_eq!(entry.body, r#"{"mergeable":false,"head_sha":"h"}"#);

    // Now current: the next read revalidates and the 304 serves the head.
    let again = pull_state_cached_as("test", &gh, Some(dir.path()), None, 42).unwrap();
    assert_eq!(again, got);
    assert!(log_lines(&log)[1].contains(r#"If-None-Match: W/"m""#));
    cache().lock().unwrap().remove(&key);
}
