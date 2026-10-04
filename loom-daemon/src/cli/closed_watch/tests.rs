//! Tests for the closed-item poll (#10150). The `gh` calls are behind the
//! `list`/`scan` seams of `poll_with`; the scan core's idempotency is covered
//! in `notify_cleared_blockers/tests.rs`.

use std::cell::RefCell;

use super::*;

fn item(n: i64, closed: &str, updated: &str) -> ClosedItem {
    ClosedItem {
        number: n,
        closed_at: closed.into(),
        updated_at: updated.into(),
        merged_pr: false,
    }
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-04T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn close_with_no_merge_pr_script_triggers_scan_and_advances_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let scanned = RefCell::new(Vec::new());
    let out = poll_with(
        dir.path(),
        now(),
        |since| {
            // First run is bounded to the 24h lookback.
            assert_eq!(since, "2026-10-03T12:00:00Z");
            Ok(vec![item(7, "2026-10-04T10:00:00Z", "2026-10-04T10:00:00Z")])
        },
        |fresh| {
            scanned.borrow_mut().extend(fresh.iter().map(|i| i.number));
            Ok(())
        },
    );
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(*scanned.borrow(), vec![7]);
    assert_eq!(load_cursor(dir.path()).as_deref(), Some("2026-10-04T10:00:00Z"));
}

#[test]
fn cursor_persists_across_polls_and_idle_tick_never_scans() {
    let dir = tempfile::tempdir().unwrap();
    save_cursor(dir.path(), "2026-10-04T10:00:00Z").unwrap();
    let out = poll_with(
        dir.path(),
        now(),
        |since| {
            assert_eq!(since, "2026-10-04T10:00:00Z");
            Ok(vec![])
        },
        |_| panic!("scan (list_blocked) must not run when nothing closed"),
    );
    assert_eq!(out, PollOutcome::Idle);
}

#[test]
fn comment_on_old_closed_item_is_not_a_new_close() {
    let dir = tempfile::tempdir().unwrap();
    save_cursor(dir.path(), "2026-10-04T10:00:00Z").unwrap();
    let out = poll_with(
        dir.path(),
        now(),
        |_| Ok(vec![item(3, "2026-10-01T00:00:00Z", "2026-10-04T11:00:00Z")]),
        |_| panic!("an old close must not rescan"),
    );
    assert_eq!(out, PollOutcome::Idle);
    // The cursor still moves past the comment's updated_at.
    assert_eq!(load_cursor(dir.path()).as_deref(), Some("2026-10-04T11:00:00Z"));
}

#[test]
fn failed_scan_holds_the_cursor_so_the_next_tick_retries() {
    let dir = tempfile::tempdir().unwrap();
    save_cursor(dir.path(), "2026-10-04T09:00:00Z").unwrap();
    let out = poll_with(
        dir.path(),
        now(),
        |_| Ok(vec![item(7, "2026-10-04T10:00:00Z", "2026-10-04T10:00:00Z")]),
        |_| Err("boom".into()),
    );
    assert_eq!(out, PollOutcome::Failed("boom".into()));
    assert_eq!(load_cursor(dir.path()).as_deref(), Some("2026-10-04T09:00:00Z"));
}

#[test]
fn failed_listing_holds_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let out = poll_with(dir.path(), now(), |_| Err("rate".into()), |_| Ok(()));
    assert_eq!(out, PollOutcome::Failed("rate".into()));
    assert_eq!(load_cursor(dir.path()), None);
}

#[test]
fn parse_page_reads_issues_and_prs_and_drops_open_items() {
    let json = br#"[
      {"number":1,"closed_at":"2026-10-04T10:00:00Z","updated_at":"2026-10-04T10:01:00Z"},
      {"number":2,"closed_at":"2026-10-04T10:00:00Z","updated_at":"2026-10-04T10:00:00Z",
       "pull_request":{"merged_at":"2026-10-04T10:00:00Z"}},
      {"number":3,"closed_at":null,"updated_at":"2026-10-04T10:00:00Z"}
    ]"#;
    let items = parse_page(json).unwrap();
    assert_eq!(items.iter().map(|i| i.number).collect::<Vec<_>>(), vec![1, 2]);
    assert!(!items[0].merged_pr && items[1].merged_pr);
    assert!(parse_page(b"nope").is_err());
}

#[test]
fn knob_is_default_off_with_env_over_config_precedence() {
    let on = ClosedWatchConfig {
        enabled: Some(true),
        interval_secs: Some(60),
    };
    let off = ClosedWatchConfig::default();
    assert!(!resolve_enabled_with(None, &off));
    assert!(resolve_enabled_with(None, &on));
    assert!(!resolve_enabled_with(Some("0"), &on));
    assert!(resolve_enabled_with(Some("1"), &off));
    assert_eq!(resolve_interval_secs(None, &off), DEFAULT_INTERVAL_SECS);
    assert_eq!(resolve_interval_secs(None, &on), 60);
    assert_eq!(resolve_interval_secs(Some("30"), &on), 30);
    assert_eq!(resolve_interval_secs(Some("bad"), &on), 60);
}

// ---------------------------------------------------------------------------
// Real poll -> scan path against a fake `gh` (Judge P1 on PR #10180): a failed
// candidate read or closing-reference expansion must hold the cursor, the
// next pass must actually post, and a further pass must not duplicate it.
// ---------------------------------------------------------------------------

/// `LOOM_GH_BIN` is process-global; serialise the tests that point it at a
/// fake.
static GH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Points `LOOM_GH_BIN` at `bin`, restoring the prior value on drop.
struct GhBin(Option<std::ffi::OsString>);

impl GhBin {
    fn set(bin: &Path) -> Self {
        let prior = std::env::var_os("LOOM_GH_BIN");
        std::env::set_var("LOOM_GH_BIN", bin);
        Self(prior)
    }
}

impl Drop for GhBin {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("LOOM_GH_BIN", v),
            None => std::env::remove_var("LOOM_GH_BIN"),
        }
    }
}

/// A fake `gh` serving fixtures from `dir`:
/// `api ...` -> `closed.json`; `issue list` -> `issue-list.json`; `pr list`
/// -> `[]`; `<issue|pr> view N` -> `<entity>-N.json` (`{}` if absent) unless
/// `fail-<entity>-N` exists, which makes it exit 1 (a transient read
/// failure); `<issue|pr> comment N --body B` -> logs `<entity> comment N` to
/// `calls.log` and writes B to `body-<entity>-N.txt`.
fn fake_gh(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let d = dir.display();
    let script = format!(
        r#"#!/bin/sh
D='{d}'
entity="$1"; verb="$2"; num="$3"
case "$entity:$verb" in
  api:*) cat "$D/closed.json"; exit 0 ;;
  issue:list) cat "$D/issue-list.json" 2>/dev/null || echo '[]'; exit 0 ;;
  pr:list) echo '[]'; exit 0 ;;
  issue:view|pr:view)
    if [ -e "$D/fail-$entity-$num" ]; then echo "transient read failure" >&2; exit 1; fi
    cat "$D/$entity-$num.json" 2>/dev/null || echo '{{}}'
    exit 0 ;;
  issue:comment|pr:comment)
    echo "$entity comment $num" >> "$D/calls.log"
    shift 3
    while [ $# -gt 0 ]; do
      if [ "$1" = "--body" ]; then shift; printf '%s' "$1" > "$D/body-$entity-$num.txt"; fi
      shift
    done
    exit 0 ;;
esac
echo "fake gh: unhandled: $*" >&2
exit 3
"#
    );
    let p = dir.join("gh");
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

fn comment_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Simulate the forge: the comment the fake `gh` just recorded on #201 now
/// shows up in #201's comment list (so the marker is visible to a re-read).
fn reflect_posted_comment_on_201(gh_dir: &Path) {
    let posted = std::fs::read_to_string(gh_dir.join("body-issue-201.txt")).unwrap();
    let view = serde_json::json!({
        "body": "Blocked by #200: needs that first.",
        "comments": [{"author": {"login": "loom-fleet-dispatch"}, "body": posted}],
        "closedByPullRequestsReferences": []
    });
    write(gh_dir, "issue-201.json", &view.to_string());
}

/// The shared population: open `loom:blocked` #201 cites #200, which is
/// closed. #300 is an unrelated close updated later in the same window, so a
/// wrongly-advanced cursor would land on 11:00, past #200's 10:00 close.
fn seed_population(gh_dir: &Path) {
    write(gh_dir, "issue-list.json", r#"[{"number":201,"title":"Waits on 200"}]"#);
    write(gh_dir, "issue-200.json", r#"{"state":"CLOSED"}"#);
    write(
        gh_dir,
        "issue-201.json",
        r#"{"body":"Blocked by #200: needs that first.","comments":[],"closedByPullRequestsReferences":[]}"#,
    );
}

const SINCE: &str = "2026-10-04T09:00:00Z";
const LATEST: &str = "2026-10-04T11:00:00Z";

#[test]
fn failed_candidate_read_holds_cursor_then_retry_posts_once() {
    let _lock = GH_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    let gh_dir = tempfile::tempdir().unwrap();
    let _gh = GhBin::set(&fake_gh(gh_dir.path()));
    let g = gh_dir.path();
    seed_population(g);
    write(
        g,
        "closed.json",
        r#"[{"number":200,"closed_at":"2026-10-04T10:00:00Z","updated_at":"2026-10-04T10:00:00Z"},
            {"number":300,"closed_at":"2026-10-04T10:30:00Z","updated_at":"2026-10-04T11:00:00Z"}]"#,
    );
    save_cursor(root.path(), SINCE).unwrap();

    // Pass 1: #201's body/comments read fails transiently.
    write(g, "fail-issue-201", "");
    assert!(matches!(poll_once(root.path()), PollOutcome::Failed(_)));
    assert_eq!(load_cursor(root.path()).as_deref(), Some(SINCE), "cursor must hold");
    assert!(comment_calls(g).is_empty());

    // Pass 2: the read recovers; the notice is actually posted.
    std::fs::remove_file(g.join("fail-issue-201")).unwrap();
    assert_eq!(poll_once(root.path()), PollOutcome::Scanned { closed: 2 });
    assert_eq!(comment_calls(g), vec!["issue comment 201".to_string()]);
    let body = std::fs::read_to_string(g.join("body-issue-201.txt")).unwrap();
    assert!(body.contains("<!-- loom:blocker-cleared:#200 -->"));
    assert_eq!(load_cursor(root.path()).as_deref(), Some(LATEST));

    // Pass 3: the same listing again. #200's close is now behind the cursor.
    reflect_posted_comment_on_201(g);
    assert_eq!(poll_once(root.path()), PollOutcome::Idle);

    // Pass 4: #200 re-listed as a fresh close (reopened and re-closed): the
    // scan runs, but the marker on #201 makes it a no-op.
    write(
        g,
        "closed.json",
        r#"[{"number":200,"closed_at":"2026-10-04T11:30:00Z","updated_at":"2026-10-04T11:30:00Z"}]"#,
    );
    assert_eq!(poll_once(root.path()), PollOutcome::Scanned { closed: 1 });
    assert_eq!(comment_calls(g).len(), 1, "no duplicate notice");
}

#[test]
fn failed_closing_reference_expansion_holds_cursor_then_retry_posts() {
    let _lock = GH_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    let gh_dir = tempfile::tempdir().unwrap();
    let _gh = GhBin::set(&fake_gh(gh_dir.path()));
    let g = gh_dir.path();
    seed_population(g);
    // Merged PR #400 closed #200; only the expansion names #200 here.
    write(
        g,
        "closed.json",
        r#"[{"number":400,"closed_at":"2026-10-04T10:00:00Z","updated_at":"2026-10-04T10:00:00Z",
             "pull_request":{"merged_at":"2026-10-04T10:00:00Z"}},
            {"number":300,"closed_at":"2026-10-04T10:30:00Z","updated_at":"2026-10-04T11:00:00Z"}]"#,
    );
    write(g, "pr-400.json", r#"{"closingIssuesReferences":[{"number":200}]}"#);
    save_cursor(root.path(), SINCE).unwrap();

    // Pass 1: `gh pr view 400 --json closingIssuesReferences` fails.
    write(g, "fail-pr-400", "");
    assert!(matches!(poll_once(root.path()), PollOutcome::Failed(_)));
    assert_eq!(load_cursor(root.path()).as_deref(), Some(SINCE), "cursor must hold");
    assert!(comment_calls(g).is_empty());

    // Pass 2: expansion recovers; #201 is notified about #200.
    std::fs::remove_file(g.join("fail-pr-400")).unwrap();
    assert_eq!(poll_once(root.path()), PollOutcome::Scanned { closed: 2 });
    assert_eq!(comment_calls(g), vec!["issue comment 201".to_string()]);
    assert_eq!(load_cursor(root.path()).as_deref(), Some(LATEST));

    // Pass 3: nothing new; no duplicate.
    reflect_posted_comment_on_201(g);
    assert_eq!(poll_once(root.path()), PollOutcome::Idle);
    assert_eq!(comment_calls(g).len(), 1, "no duplicate notice");
}
