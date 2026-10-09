//! Tests for the closed-item poll (#10150). The `gh` calls are behind the
//! `list`/`scan` seams of `poll_with`, and the real-path tests below drive
//! `scan_fresh` -> `scan_cleared_with` (the batched gatherer, `classify`, and
//! the post) against a fake forge.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::anyhow;
use loom_daemon::dep_recheck::extract;
use loom_daemon::forge_identity::FleetLogins;
use loom_daemon::forge_listing::RestIssue;
use loom_daemon::stale_blocked::batch::{self, ClosingRef, RefState, StaleBlockedForge};
use loom_daemon::stale_blocked::budget::{Budget, Floor, Meter};
use loom_daemon::stale_blocked::Artifact;

use super::super::notify_cleared_blockers::scan_cleared_with;
use super::super::stale_blocked::DEFAULT_LIMIT;
use super::*;

fn ts(s: &str) -> DateTime<Utc> {
    parse_ts(s).unwrap()
}

/// A closed item as listed; its `closed_at` plays no part in the cursor.
fn item(n: i64, updated: &str) -> ClosedItem {
    ClosedItem {
        number: n,
        updated_at: ts(updated),
        merged_pr: false,
    }
}

fn now() -> DateTime<Utc> {
    ts("2026-10-04T12:00:00Z")
}

/// The forge listing as `gh api .../issues?state=closed&since=S&sort=updated
/// &direction=asc&per_page=100&page=P` serves it: `updated_at >= since`,
/// oldest-updated first (ties in listing order), one page of [`PAGE_SIZE`].
fn serve(rows: &[ClosedItem], since: &str, page: usize) -> Vec<ClosedItem> {
    let since = ts(since);
    let mut hits: Vec<ClosedItem> = rows
        .iter()
        .filter(|r| r.updated_at >= since)
        .cloned()
        .collect();
    hits.sort_by_key(|r| r.updated_at);
    hits.into_iter()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
        .collect()
}

fn cursor_at(root: &Path) -> Option<String> {
    load_cursor(root).map(|c| fmt_ts(c.at))
}

fn save_at(root: &Path, at: &str) {
    save_cursor(root, &Cursor::starting_at(ts(at))).unwrap();
}

/// `secs` seconds after `base`, formatted.
fn plus(base: &str, secs: i64) -> String {
    fmt_ts(ts(base) + ChronoDuration::seconds(secs))
}

#[test]
fn close_with_no_merge_pr_script_triggers_scan_and_advances_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let scanned = RefCell::new(Vec::new());
    let out = poll_with(
        dir.path(),
        now(),
        |since, page| {
            // First run is bounded to the 24h lookback (less the 1s overlap).
            assert_eq!((since, page), ("2026-10-03T11:59:59Z", 1));
            Ok(vec![item(7, "2026-10-04T10:00:00Z")])
        },
        |batch| {
            scanned.borrow_mut().extend(batch.iter().map(|i| i.number));
            Ok(())
        },
    );
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(*scanned.borrow(), vec![7]);
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T10:00:00Z"));
}

/// The cursor is written into the primary clone, so the managed `.gitignore`
/// block must ignore it (and its `.tmp` sibling) or `check-main-clean.sh`
/// reads main as dirty and `--quarantine` can stash the cursor away. Paths come
/// from `cursor_path` itself, answered by real `git check-ignore`.
#[test]
fn cursor_file_is_ignored_by_the_managed_gitignore_block() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run git")
    };
    assert!(git(&["init", "-q"]).status.success(), "git init failed");
    loom_daemon::init::update_gitignore(root).unwrap();
    let cursor = cursor_path(root);
    let tmp = cursor.with_extension("json.tmp");
    for abs in [&cursor, &tmp] {
        let rel = abs.strip_prefix(root).unwrap().to_str().unwrap();
        assert!(
            git(&["check-ignore", "-q", "--no-index", rel])
                .status
                .success(),
            "{rel} is daemon runtime state but NOT ignored by the managed block"
        );
    }
}

#[test]
fn cursor_persists_across_polls_and_idle_tick_never_scans() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T10:00:00Z");
    let out = poll_with(
        dir.path(),
        now(),
        |since, _| {
            assert_eq!(since, "2026-10-04T09:59:59Z");
            Ok(vec![])
        },
        |_| panic!("the loom:blocked scan must not run when nothing is past the cursor"),
    );
    assert_eq!(out, PollOutcome::Idle);
}

#[test]
fn cursor_round_trips_its_seen_set_and_reads_the_old_format() {
    let dir = tempfile::tempdir().unwrap();
    let c = Cursor {
        at: ts("2026-10-04T10:00:00Z"),
        seen: BTreeSet::from([4, 9]),
        resume: 3,
    };
    save_cursor(dir.path(), &c).unwrap();
    assert_eq!(load_cursor(dir.path()), Some(c));
    // The first #10150 format had no `seen`: it loads with an empty set.
    std::fs::write(cursor_path(dir.path()), r#"{"cursor":"2026-10-04T10:00:00Z"}"#).unwrap();
    assert_eq!(load_cursor(dir.path()), Some(Cursor::starting_at(ts("2026-10-04T10:00:00Z"))));
}

/// A row already scanned at the cursor second is not rescanned; the same
/// closed item updated later (say, commented on) is, and the per-number
/// marker makes that rescan a no-op (see the fake-forge tests below).
#[test]
fn scanned_rows_stay_scanned_but_a_later_update_rescans() {
    let dir = tempfile::tempdir().unwrap();
    save_cursor(
        dir.path(),
        &Cursor {
            at: ts("2026-10-04T10:00:00Z"),
            seen: BTreeSet::from([3]),
            resume: 1,
        },
    )
    .unwrap();
    let rows = vec![item(3, "2026-10-04T10:00:00Z")];
    let out = poll_with(
        dir.path(),
        now(),
        |since, page| Ok(serve(&rows, since, page)),
        |_| panic!("an already-scanned key must not rescan"),
    );
    assert_eq!(out, PollOutcome::Idle);

    let rows = vec![item(3, "2026-10-04T11:00:00Z")];
    let out = poll_with(dir.path(), now(), |since, page| Ok(serve(&rows, since, page)), |_| Ok(()));
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T11:00:00Z"));
}

#[test]
fn failed_scan_holds_the_cursor_so_the_next_tick_retries() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T09:00:00Z");
    let out = poll_with(
        dir.path(),
        now(),
        |_, _| Ok(vec![item(7, "2026-10-04T10:00:00Z")]),
        |_| Err("boom".into()),
    );
    assert_eq!(out, PollOutcome::Failed("boom".into()));
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T09:00:00Z"));
}

#[test]
fn failed_listing_holds_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let out = poll_with(dir.path(), now(), |_, _| Err("rate".into()), |_| Ok(()));
    assert_eq!(out, PollOutcome::Failed("rate".into()));
    assert_eq!(load_cursor(dir.path()), None);
}

/// A read failing mid-walk fails the whole pass: nothing is scanned and the
/// cursor holds, rather than advancing over the rows already read.
#[test]
fn failed_later_page_holds_the_cursor_and_scans_nothing() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T09:00:00Z");
    let rows: Vec<ClosedItem> = (1..=150)
        .map(|n| item(n, &plus("2026-10-04T10:00:00Z", n)))
        .collect();
    let calls = RefCell::new(0);
    let out = poll_with(
        dir.path(),
        now(),
        |since, page| {
            *calls.borrow_mut() += 1;
            if *calls.borrow() == 2 {
                return Err("rate".into());
            }
            Ok(serve(&rows, since, page))
        },
        |_| panic!("a partial walk must not scan"),
    );
    assert_eq!(out, PollOutcome::Failed("rate".into()));
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T09:00:00Z"));
}

/// Run polls over `rows` until one is idle; return how many times each number
/// was scanned and the largest number of requests any one poll made.
fn drain(
    root: &Path,
    rows: &RefCell<Vec<ClosedItem>>,
    max_ticks: usize,
) -> (HashMap<i64, usize>, usize) {
    let mut scanned: HashMap<i64, usize> = HashMap::new();
    let mut max_requests = 0;
    for _ in 0..max_ticks {
        let requests = RefCell::new(0);
        let out = poll_with(
            root,
            now(),
            |since, page| {
                *requests.borrow_mut() += 1;
                Ok(serve(&rows.borrow(), since, page))
            },
            |batch| {
                for i in batch {
                    *scanned.entry(i.number).or_default() += 1;
                }
                Ok(())
            },
        );
        max_requests = max_requests.max(requests.into_inner());
        match out {
            PollOutcome::Idle => return (scanned, max_requests),
            PollOutcome::Scanned { .. } => {}
            PollOutcome::Failed(e) => panic!("unexpected failure: {e}"),
        }
    }
    panic!("still scanning after {max_ticks} polls");
}

/// Judge P1 on #10180 / #10638. 500 rows closed 10:00 and updated 11:00 fill
/// the 5-page cap; #501 closed 10:30 but was updated (commented on) at 11:30,
/// so the capped first pass never lists it. The old filter (`closed_at >=
/// cursor`, cursor = max `updated_at`) then rejected #501 on the next pass
/// (10:30 < 11:00) and skipped it forever. With one key for both, the second
/// pass takes it, after paging past the same-second run (more than five pages).
#[test]
fn capped_listing_never_drops_a_close_updated_past_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T09:00:00Z");
    let mut rows: Vec<ClosedItem> = (1..=500).map(|n| item(n, "2026-10-04T11:00:00Z")).collect();
    rows.push(item(501, "2026-10-04T11:30:00Z"));

    let requests = RefCell::new(Vec::new());
    let poll = |scanned: &mut Vec<i64>| {
        requests.borrow_mut().clear();
        poll_with(
            dir.path(),
            now(),
            |since, page| {
                requests.borrow_mut().push(page);
                Ok(serve(&rows, since, page))
            },
            |batch| {
                scanned.extend(batch.iter().map(|i| i.number));
                Ok(())
            },
        )
    };

    // Pass 1: the cap stops at the first 500 rows.
    let mut first = Vec::new();
    assert_eq!(poll(&mut first), PollOutcome::Scanned { closed: 500 });
    assert!(!first.contains(&501));
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T11:00:00Z"));

    // Pass 2: #501 is taken, and only #501 (the 500 are not rescanned).
    let mut second = Vec::new();
    assert_eq!(poll(&mut second), PollOutcome::Scanned { closed: 1 });
    assert_eq!(second, vec![501]);
    assert!(
        requests.borrow().iter().any(|p| *p > MAX_PAGES),
        "must page past the same-second run: {:?}",
        requests.borrow()
    );
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T11:30:00Z"));

    // Pass 3: nothing left.
    assert_eq!(poll(&mut Vec::new()), PollOutcome::Idle);
}

/// Equal-timestamp boundaries: same-second runs of several lengths, including
/// runs straddling every page and cap boundary, are each scanned exactly once
/// across polls, and no poll exceeds the request bound.
#[test]
fn same_second_runs_across_page_boundaries_are_scanned_exactly_once() {
    for (total, per_second) in [
        (700, 700),
        (1200, 1),
        (1000, 7),
        (1050, 150),
        (520, 100),
        (2101, 2101),
        (2500, 2000),
    ] {
        let dir = tempfile::tempdir().unwrap();
        save_at(dir.path(), "2026-10-04T09:00:00Z");
        let rows: Vec<ClosedItem> = (1..=total)
            .map(|n| item(n, &plus("2026-10-04T09:30:00Z", (n - 1) / per_second)))
            .collect();
        let rows = RefCell::new(rows);
        let (scanned, max_requests) = drain(dir.path(), &rows, 50);
        let case = format!("{total} rows, {per_second} per second");
        assert_eq!(scanned.len(), usize::try_from(total).unwrap(), "{case}: every row scanned");
        assert!(scanned.values().all(|c| *c == 1), "{case}: no row scanned twice");
        assert!(max_requests <= MAX_REQUESTS, "{case}: {max_requests} requests");
    }
}

/// An update during a walk moves a row to the end of the listing, shifting
/// every later page by one. Numbered paging then skips the row that slid onto
/// the previous page; the re-anchored walk does not.
#[test]
fn update_reordering_the_listing_mid_walk_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T09:00:00Z");
    let rows = RefCell::new(
        (1..=300)
            .map(|n| item(n, &plus("2026-10-04T10:00:00Z", n)))
            .collect::<Vec<_>>(),
    );
    let first = RefCell::new(true);
    let mut scanned: HashSet<i64> = HashSet::new();
    let out = poll_with(
        dir.path(),
        now(),
        |since, page| {
            let out = serve(&rows.borrow(), since, page);
            if first.replace(false) {
                // #50 (already on page 1) is commented on after page 1 is read.
                rows.borrow_mut()[49].updated_at = ts("2026-10-04T11:59:00Z");
            }
            Ok(out)
        },
        |batch| {
            scanned.extend(batch.iter().map(|i| i.number));
            Ok(())
        },
    );
    assert_eq!(out, PollOutcome::Scanned { closed: 300 });
    assert_eq!(scanned, (1..=300).collect::<HashSet<_>>());
}

/// Judge P1 on #10180 (round 2). 300 rows share one second, a sentinel comes
/// later. #50, already read, is updated right after the second read of page 1,
/// which slides unread #101 onto page 1 while the walk is on page 2. The walk
/// must not move past the shared second until it has re-read the run.
#[test]
fn update_during_same_second_paging_does_not_skip_an_unseen_row() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T09:00:00Z");
    let mut rows: Vec<ClosedItem> = (1..=300).map(|n| item(n, "2026-10-04T10:00:00Z")).collect();
    rows.push(item(301, "2026-10-04T10:30:00Z"));
    let rows = RefCell::new(rows);
    let calls = RefCell::new(0);
    let mut scanned: HashSet<i64> = HashSet::new();
    let fetch = |since: &str, page: usize| {
        let out = serve(&rows.borrow(), since, page);
        *calls.borrow_mut() += 1;
        if *calls.borrow() == 2 {
            rows.borrow_mut()[49].updated_at = ts("2026-10-04T11:59:00Z");
        }
        Ok(out)
    };
    let out = poll_with(dir.path(), now(), fetch, |batch| {
        scanned.extend(batch.iter().map(|i| i.number));
        Ok(())
    });
    assert!(matches!(out, PollOutcome::Scanned { .. }), "{out:?}");
    // Every row of the shared second, including #50 (read before it moved),
    // #101 (the one a numbered walk skips) and the sentinel.
    assert_eq!(scanned, (1..=301).collect::<HashSet<_>>());
}

/// Judge P2 on #10180 (round 2). 2,101 rows at one second exceed what
/// `MAX_REQUESTS` pages can cover; the later-second sentinel behind them must
/// still be reached, and every row scanned exactly once.
#[test]
fn run_longer_than_the_request_bound_still_reaches_a_later_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    save_at(dir.path(), "2026-10-04T09:00:00Z");
    let mut rows: Vec<ClosedItem> = (1..=2101)
        .map(|n| item(n, "2026-10-04T10:00:00Z"))
        .collect();
    rows.push(item(2102, "2026-10-04T10:30:00Z"));
    assert!(rows.len() > MAX_REQUESTS * PAGE_SIZE);
    let rows = RefCell::new(rows);
    let (scanned, max_requests) = drain(dir.path(), &rows, 50);
    assert_eq!(scanned.len(), 2102);
    assert!(scanned.values().all(|c| *c == 1));
    assert!(max_requests <= MAX_REQUESTS);
    assert_eq!(cursor_at(dir.path()).as_deref(), Some("2026-10-04T10:30:00Z"));
}

/// A resume hint past the end of the listing (rows left it) is only a hint.
#[test]
fn stale_resume_hint_falls_back_to_page_one() {
    let dir = tempfile::tempdir().unwrap();
    save_cursor(
        dir.path(),
        &Cursor {
            at: ts("2026-10-04T10:00:00Z"),
            seen: BTreeSet::new(),
            resume: 9,
        },
    )
    .unwrap();
    let rows = vec![
        item(1, "2026-10-04T10:00:00Z"),
        item(2, "2026-10-04T10:00:00Z"),
    ];
    let mut scanned = Vec::new();
    let out = poll_with(
        dir.path(),
        now(),
        |since, page| Ok(serve(&rows, since, page)),
        |b| {
            scanned.extend(b.iter().map(|i| i.number));
            Ok(())
        },
    );
    assert_eq!(out, PollOutcome::Scanned { closed: 2 });
    assert_eq!(scanned, vec![1, 2]);
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
    assert_eq!(items[0].updated_at, ts("2026-10-04T10:01:00Z"));
    assert!(parse_page(b"nope").is_err());
    // An unreadable updated_at fails the page instead of guessing a key.
    let bad = br#"[{"number":1,"closed_at":"2026-10-04T10:00:00Z","updated_at":"later"}]"#;
    assert!(parse_page(bad).is_err());
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
// Real poll -> scan path against a fake forge (Judge P1 on PR #10180): a
// failed candidate read or closing-reference expansion must hold the cursor,
// the next pass must actually post, and a further pass must not duplicate it.
// ---------------------------------------------------------------------------

/// A forge holding open `loom:blocked` #201 ("Blocked by #200"), with #200
/// closed. `comments_fail` makes #201's comment read fail (a transient read
/// failure); `posted` records each notice, which then shows up as a comment.
#[derive(Default)]
struct World {
    comments_fail: bool,
    comments: Vec<extract::Comment>,
    /// Open `loom:blocked` issues listed ahead of #201 that cite nothing closed.
    unrelated_ahead: u32,
    posted: Vec<(i64, String)>,
}

impl StaleBlockedForge for World {
    /// Required since #10562; the notify path (`gather_cited`) never probes
    /// it, and this world's repository is live.
    fn archived(&mut self) -> Result<bool, String> {
        Ok(false)
    }

    fn list_blocked(&mut self) -> anyhow::Result<Vec<RestIssue>> {
        let mut rows: Vec<RestIssue> = (0..self.unrelated_ahead)
            .map(|i| RestIssue {
                number: 1000 + i,
                title: Some("Waits on design".into()),
                body: Some("Waiting on a design review.".into()),
                comments: 0,
                ..blocked_row()
            })
            .collect();
        rows.push(RestIssue {
            number: 201,
            title: Some("Waits on 200".into()),
            labels: vec!["loom:blocked".into()],
            created_at: None,
            updated_at: None,
            closed_at: None,
            state: "open".into(),
            body: Some("Blocked by #200: needs that first.".into()),
            author: None,
            is_pull_request: false,
            comments: 1 + u32::try_from(self.comments.len()).unwrap(),
        });
        Ok(rows)
    }

    fn comments(&mut self, number: u32) -> anyhow::Result<Vec<extract::Comment>> {
        assert_eq!(number, 201);
        if self.comments_fail {
            return Err(anyhow!("transient read failure"));
        }
        let mut all = vec![extract::Comment {
            author: extract::Author {
                login: "a-human".into(),
            },
            body: "still waiting on it".into(),
        }];
        all.extend(self.comments.iter().cloned());
        Ok(all)
    }

    fn ref_state(&mut self, _repo: Option<&str>, number: i64) -> anyhow::Result<Option<RefState>> {
        Ok((number == 200).then(|| RefState {
            state: "CLOSED".into(),
            labels: Vec::new(),
            is_pr: false,
        }))
    }

    fn pr_merge_state(&mut self, number: u32) -> anyhow::Result<(String, String)> {
        Err(anyhow!("no PR #{number} in this world"))
    }

    fn closing_refs_batch(
        &mut self,
        issues: &[u32],
    ) -> anyhow::Result<HashMap<u32, Vec<ClosingRef>>> {
        Ok(issues.iter().map(|n| (*n, Vec::new())).collect())
    }

    fn budget(&mut self) -> Option<Budget> {
        Some(Budget {
            core_remaining: 5000,
            graphql_remaining: 5000,
        })
    }

    fn breaker_open(&mut self) -> bool {
        false
    }

    fn meter(&self) -> Meter {
        Meter::default()
    }
}

fn blocked_row() -> RestIssue {
    RestIssue {
        number: 0,
        title: None,
        labels: vec!["loom:blocked".into()],
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: "open".into(),
        body: None,
        author: None,
        is_pull_request: false,
        comments: 0,
    }
}

impl World {
    /// One poll through the real `scan_fresh` -> `scan_cleared_with` path.
    /// `listing` is the closed-items page; `fail_expansion` names merged PRs
    /// whose closing-reference read fails; `close_targets` answers the rest.
    fn poll(
        &mut self,
        root: &Path,
        listing: Vec<ClosedItem>,
        fail_expansion: &HashSet<i64>,
        close_targets: &HashMap<i64, Vec<i64>>,
    ) -> PollOutcome {
        let fleet = FleetLogins::single(extract::DEFAULT_BOT_LOGIN);
        let gather = batch::Options {
            limit: SCAN_LIMIT,
            no_prs: false,
            floor: Floor::default(),
        };
        poll_with(
            root,
            now(),
            |since, page| Ok(serve(&listing, since, page)),
            |fresh| {
                scan_fresh(
                    fresh,
                    &mut |pr| {
                        if fail_expansion.contains(&pr) {
                            Err("transient read failure".into())
                        } else {
                            Ok(close_targets.get(&pr).cloned().unwrap_or_default())
                        }
                    },
                    |closed| {
                        let mut posted = Vec::new();
                        let rep = scan_cleared_with(
                            &mut *self,
                            &fleet,
                            gather,
                            closed,
                            false,
                            &mut |kind, number, cited, reasons| {
                                assert_eq!(kind, Artifact::Issue);
                                let body = super::super::notify_cleared_blockers::comment_body(
                                    kind, cited, reasons,
                                );
                                posted.push((number, body));
                                true
                            },
                        );
                        // The forge now shows the posted notice as a comment.
                        for (number, body) in posted {
                            self.comments.push(extract::Comment {
                                author: extract::Author {
                                    login: extract::DEFAULT_BOT_LOGIN.into(),
                                },
                                body: body.clone(),
                            });
                            self.posted.push((number, body));
                        }
                        rep
                    },
                )
            },
        )
    }
}

fn merged(n: i64, updated: &str) -> ClosedItem {
    ClosedItem {
        merged_pr: true,
        ..item(n, updated)
    }
}

const SINCE: &str = "2026-10-04T09:00:00Z";
const LATEST: &str = "2026-10-04T11:00:00Z";

#[test]
fn failed_candidate_read_holds_cursor_then_retry_posts_once() {
    let root = tempfile::tempdir().unwrap();
    let (none, targets) = (HashSet::new(), HashMap::new());
    // #300 is an unrelated close updated later in the same window, so a
    // wrongly-advanced cursor would land on 11:00, past #200's 10:00 key.
    let listing = || vec![item(200, "2026-10-04T10:00:00Z"), item(300, LATEST)];
    save_at(root.path(), SINCE);
    let mut world = World {
        comments_fail: true,
        ..World::default()
    };

    // Pass 1: #201's comment read fails transiently.
    let out = world.poll(root.path(), listing(), &none, &targets);
    assert!(matches!(out, PollOutcome::Failed(_)), "{out:?}");
    assert_eq!(cursor_at(root.path()).as_deref(), Some(SINCE), "cursor must hold");
    assert!(world.posted.is_empty());

    // Pass 2: the read recovers; the notice is actually posted.
    world.comments_fail = false;
    let out = world.poll(root.path(), listing(), &none, &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 2 });
    assert_eq!(world.posted.len(), 1);
    assert_eq!(world.posted[0].0, 201);
    assert!(world.posted[0]
        .1
        .contains("<!-- loom:blocker-cleared:#200 -->"));
    assert_eq!(cursor_at(root.path()).as_deref(), Some(LATEST));

    // Pass 3: the same listing again. Both keys are now behind the cursor.
    let out = world.poll(root.path(), listing(), &none, &targets);
    assert_eq!(out, PollOutcome::Idle);

    // Pass 4: #200 updated again (a comment, or reopened and re-closed): the
    // scan runs, but the marker on #201 makes it a no-op.
    let relisted = vec![item(200, "2026-10-04T11:30:00Z")];
    let out = world.poll(root.path(), relisted, &none, &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(world.posted.len(), 1, "no duplicate notice");
}

#[test]
fn failed_closing_reference_expansion_holds_cursor_then_retry_posts() {
    let root = tempfile::tempdir().unwrap();
    // Merged PR #400 closed #200; only the expansion names #200 here.
    let targets = HashMap::from([(400, vec![200])]);
    let listing = || vec![merged(400, "2026-10-04T10:00:00Z"), item(300, LATEST)];
    save_at(root.path(), SINCE);
    let mut world = World::default();

    // Pass 1: PR #400's closing-reference read fails.
    let out = world.poll(root.path(), listing(), &HashSet::from([400]), &targets);
    assert!(matches!(out, PollOutcome::Failed(_)), "{out:?}");
    assert_eq!(cursor_at(root.path()).as_deref(), Some(SINCE), "cursor must hold");
    assert!(world.posted.is_empty());

    // Pass 2: expansion recovers; #201 is notified about #200.
    let out = world.poll(root.path(), listing(), &HashSet::new(), &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 2 });
    assert_eq!(world.posted.len(), 1);
    assert_eq!(world.posted[0].0, 201);
    assert_eq!(cursor_at(root.path()).as_deref(), Some(LATEST));

    // Pass 3: nothing new; no duplicate.
    let out = world.poll(root.path(), listing(), &HashSet::new(), &targets);
    assert_eq!(out, PollOutcome::Idle);
    assert_eq!(world.posted.len(), 1, "no duplicate notice");
}

#[test]
fn citer_beyond_the_old_population_cap_is_still_notified_once() {
    let root = tempfile::tempdir().unwrap();
    let (none, targets) = (HashSet::new(), HashMap::new());
    let listing = || vec![item(200, "2026-10-04T10:00:00Z")];
    save_at(root.path(), SINCE);
    // #201, the only citer of #200, is the 101st open blocked issue.
    let mut world = World {
        unrelated_ahead: DEFAULT_LIMIT,
        ..World::default()
    };

    let out = world.poll(root.path(), listing(), &none, &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(world.posted.len(), 1, "the citer past the old cap is notified");
    assert_eq!(world.posted[0].0, 201);
    assert_eq!(cursor_at(root.path()).as_deref(), Some("2026-10-04T10:00:00Z"));

    // A later poll that rescans #200 posts nothing more (marker dedupe).
    let relisted = vec![item(200, "2026-10-04T11:30:00Z")];
    let out = world.poll(root.path(), relisted, &none, &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(world.posted.len(), 1, "no duplicate notice");
}
