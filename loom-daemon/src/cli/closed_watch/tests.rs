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
