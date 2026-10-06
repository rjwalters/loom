//! Tests for the closed-item poll (#10150). The `gh` calls are behind the
//! `list`/`scan` seams of `poll_with`, and the real-path tests below drive
//! `scan_fresh` -> `scan_cleared_with` (the batched gatherer, `classify`, and
//! the post) against a fake forge.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use anyhow::anyhow;
use loom_daemon::dep_recheck::extract;
use loom_daemon::forge_identity::FleetLogins;
use loom_daemon::forge_listing::RestIssue;
use loom_daemon::stale_blocked::batch::{self, ClosingRef, RefState, StaleBlockedForge};
use loom_daemon::stale_blocked::budget::{Budget, Floor, Meter};
use loom_daemon::stale_blocked::Artifact;

use super::super::notify_cleared_blockers::scan_cleared_with;
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
    save_cursor(dir.path(), "2026-10-04T10:00:00Z").unwrap();
    let out = poll_with(
        dir.path(),
        now(),
        |since| {
            assert_eq!(since, "2026-10-04T10:00:00Z");
            Ok(vec![])
        },
        |_| panic!("the loom:blocked scan must not run when nothing closed"),
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
    posted: Vec<(i64, String)>,
}

impl StaleBlockedForge for World {
    fn list_blocked(&mut self) -> anyhow::Result<Vec<RestIssue>> {
        Ok(vec![RestIssue {
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
        }])
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
            limit: DEFAULT_LIMIT,
            no_prs: false,
            floor: Floor::default(),
        };
        poll_with(
            root,
            now(),
            |_| Ok(listing),
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

fn merged(n: i64, closed: &str, updated: &str) -> ClosedItem {
    ClosedItem {
        merged_pr: true,
        ..item(n, closed, updated)
    }
}

const SINCE: &str = "2026-10-04T09:00:00Z";
const LATEST: &str = "2026-10-04T11:00:00Z";

#[test]
fn failed_candidate_read_holds_cursor_then_retry_posts_once() {
    let root = tempfile::tempdir().unwrap();
    let (none, targets) = (HashSet::new(), HashMap::new());
    // #300 is an unrelated close updated later in the same window, so a
    // wrongly-advanced cursor would land on 11:00, past #200's 10:00 close.
    let listing = || {
        vec![
            item(200, "2026-10-04T10:00:00Z", "2026-10-04T10:00:00Z"),
            item(300, "2026-10-04T10:30:00Z", LATEST),
        ]
    };
    save_cursor(root.path(), SINCE).unwrap();
    let mut world = World {
        comments_fail: true,
        ..World::default()
    };

    // Pass 1: #201's comment read fails transiently.
    let out = world.poll(root.path(), listing(), &none, &targets);
    assert!(matches!(out, PollOutcome::Failed(_)), "{out:?}");
    assert_eq!(load_cursor(root.path()).as_deref(), Some(SINCE), "cursor must hold");
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
    assert_eq!(load_cursor(root.path()).as_deref(), Some(LATEST));

    // Pass 3: the same listing again. #200's close is now behind the cursor.
    let out = world.poll(root.path(), listing(), &none, &targets);
    assert_eq!(out, PollOutcome::Idle);

    // Pass 4: #200 re-listed as a fresh close (reopened and re-closed): the
    // scan runs, but the marker on #201 makes it a no-op.
    let relisted = vec![item(200, "2026-10-04T11:30:00Z", "2026-10-04T11:30:00Z")];
    let out = world.poll(root.path(), relisted, &none, &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 1 });
    assert_eq!(world.posted.len(), 1, "no duplicate notice");
}

#[test]
fn failed_closing_reference_expansion_holds_cursor_then_retry_posts() {
    let root = tempfile::tempdir().unwrap();
    // Merged PR #400 closed #200; only the expansion names #200 here.
    let targets = HashMap::from([(400, vec![200])]);
    let listing = || {
        vec![
            merged(400, "2026-10-04T10:00:00Z", "2026-10-04T10:00:00Z"),
            item(300, "2026-10-04T10:30:00Z", LATEST),
        ]
    };
    save_cursor(root.path(), SINCE).unwrap();
    let mut world = World::default();

    // Pass 1: PR #400's closing-reference read fails.
    let out = world.poll(root.path(), listing(), &HashSet::from([400]), &targets);
    assert!(matches!(out, PollOutcome::Failed(_)), "{out:?}");
    assert_eq!(load_cursor(root.path()).as_deref(), Some(SINCE), "cursor must hold");
    assert!(world.posted.is_empty());

    // Pass 2: expansion recovers; #201 is notified about #200.
    let out = world.poll(root.path(), listing(), &HashSet::new(), &targets);
    assert_eq!(out, PollOutcome::Scanned { closed: 2 });
    assert_eq!(world.posted.len(), 1);
    assert_eq!(world.posted[0].0, 201);
    assert_eq!(load_cursor(root.path()).as_deref(), Some(LATEST));

    // Pass 3: nothing new; no duplicate.
    let out = world.poll(root.path(), listing(), &HashSet::new(), &targets);
    assert_eq!(out, PollOutcome::Idle);
    assert_eq!(world.posted.len(), 1, "no duplicate notice");
}
