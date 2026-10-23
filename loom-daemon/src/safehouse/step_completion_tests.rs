//! Issue #12152: a step emits no `SweepExited`, so a Champion merge step's
//! end narrates its merged PR's completion, and the reconciliation backstop
//! defers while the rate-limit breaker cools and catches up from its
//! watermark once it clears. The fake `gh` serves `pr view` / `pr list` from
//! files a test rewrites mid-test, and logs `argv` per call.

#![allow(clippy::unwrap_used)]

use super::tests::{stub_server, SafehouseTestPaths};
use super::*;
use crate::rate_limit_breaker::evidence::ResetEvidence;
use crate::rate_limit_breaker::report::{BreakerHandle, BudgetProbe, FailureContext, ProbeMode};
use crate::rate_limit_breaker::{BudgetSnapshot, RateLimitBreakerConfig, SharedRateLimitBreaker};
use crate::step_role::StepRole;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tokio::net::UnixListener;

struct NoProbe;

impl BudgetProbe for NoProbe {
    fn probe(&self, _ctx: &FailureContext, _now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        None
    }
}

/// A breaker cooling for `window_secs` more seconds of wall-clock time.
fn cooling(window_secs: i64) -> Option<BreakerHandle> {
    let breaker = Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: true,
        fallback_cooldown_secs: 60,
    }));
    let tripped = Utc::now() - chrono::Duration::seconds(60 - window_secs);
    breaker
        .trip("test", &ResetEvidence::Fallback { reason: "test" }, tripped)
        .unwrap();
    Some(BreakerHandle {
        breaker,
        probe: Arc::new(NoProbe) as Arc<dyn BudgetProbe>,
        mode: ProbeMode::Inline,
    })
}

struct FakeGh {
    script: PathBuf,
    log: PathBuf,
    pr_view: PathBuf,
    pr_list: PathBuf,
}

impl FakeGh {
    fn new(dir: &Path) -> Self {
        let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
        let fake = FakeGh {
            script: dir.join("fake-gh.sh"),
            log: dir.join("gh-calls.log"),
            pr_view: dir.join("pr-view.json"),
            pr_list: dir.join("pr-list.json"),
        };
        fake.serve_view("{}");
        fake.serve_list("[]");
        let body = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> {log}\n\
             case \"$1 $2\" in\n  'pr view') cat {view} ;;\n  'pr list') cat {list} ;;\n  \
             'repo view') echo '{{\"nameWithOwner\":\"rjwalters/loom\",\"isPrivate\":false}}' ;;\n  \
             *) exit 1 ;;\nesac\n",
            log = quote(&fake.log),
            view = quote(&fake.pr_view),
            list = quote(&fake.pr_list),
        );
        std::fs::write(&fake.script, body).unwrap();
        std::fs::set_permissions(&fake.script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var(GH_BIN_ENV, &fake.script);
        fake
    }

    fn serve_view(&self, json: &str) {
        std::fs::write(&self.pr_view, json).unwrap();
    }

    fn serve_list(&self, json: &str) {
        std::fs::write(&self.pr_list, json).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn calls_of(&self, verb: &str) -> usize {
        self.calls().iter().filter(|c| c.starts_with(verb)).count()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        std::env::remove_var(GH_BIN_ENV);
    }
}

/// PR `pr` for issue `issue`, merged `ago` minutes ago (`None`: still open).
fn pr_row(pr: u32, issue: u32, ago: Option<i64>) -> String {
    let merged_at = ago.map_or_else(
        || "null".to_owned(),
        |m| format!("\"{}\"", (Utc::now() - chrono::Duration::minutes(m)).to_rfc3339()),
    );
    format!(
        r#"{{"number":{pr},"headRefName":"feature/issue-{issue}","url":"https://github.com/rjwalters/loom/pull/{pr}","mergedAt":{merged_at},"createdAt":"{}","title":"t","additions":1,"deletions":1}}"#,
        (Utc::now() - chrono::Duration::hours(3)).to_rfc3339(),
    )
}

fn step_ended(item: u32, role: StepRole, class: &str, reason: &str, root: &str) -> Event {
    Event::StepEnded {
        item,
        role,
        class: class.to_owned(),
        reason: reason.to_owned(),
        ran_for_sec: 120,
        repo: Some(root.to_owned()),
    }
}

#[test]
fn only_a_merge_steps_verdict_is_looked_up() {
    use StepRole::{Builder, ChampionMerge, ChampionPromote, Curator, Doctor, Judge};
    assert!(step_narrates_completion(ChampionMerge, "verdict", "merged"));
    // The line was missing but the forge showed the PR closed: still looked
    // up; the forge's `mergedAt` decides.
    assert!(step_narrates_completion(ChampionMerge, "verdict", "closed"));
    assert!(step_narrates_completion(ChampionMerge, "verdict", "unclassified"));
    assert!(!step_narrates_completion(ChampionMerge, "verdict", "rejected"));
    // A PR triage's verdicts (#12499) label a PR, never merge one.
    for reason in ["triaged", "triage_blocked", "triage_operator"] {
        assert!(!step_narrates_completion(ChampionMerge, "verdict", reason), "{reason}");
    }
    for (class, reason) in [
        ("noop", "held"),
        ("noop", "ci_pending"),
        ("refused", "not_eligible"),
        ("refused", "head_moved"),
        ("failed", "failed"),
        ("failed", "probe_failed"),
    ] {
        assert!(!step_narrates_completion(ChampionMerge, class, reason), "{class} {reason}");
    }
    // A Builder's opened PR is unfinished work, never a completion.
    for role in [Builder, Judge, Doctor, Curator, ChampionPromote] {
        assert!(!step_narrates_completion(role, "verdict", "merged"), "{role}");
    }
}

#[tokio::test]
#[serial]
async fn a_merged_step_narrates_once_and_reconciliation_then_posts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = ws.to_string_lossy().into_owned();
    let gh = FakeGh::new(dir.path());
    gh.serve_view(&pr_row(501, 12152, Some(2)));
    gh.serve_list(&format!("[{}]", pr_row(501, 12152, Some(2))));
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    forge_gate::scoped(None, async {
        let first = completion_for_merged_step(
            "loom_daemon",
            &root,
            501,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await
        .expect("a merged step narrates its completion");
        assert_eq!(first.kind, "completion");
        assert_eq!(
            first.meta.as_ref().unwrap()["ref"],
            "https://github.com/rjwalters/loom/pull/501"
        );
        assert!(
            first.body.contains("#12152") && first.body.contains("PR #501"),
            "{}",
            first.body
        );
        assert!(completed.contains(&(root.clone(), 12152, 501)));
        // The same step's end seen again (a peer's, a replay): nothing.
        let again = completion_for_merged_step(
            "loom_daemon",
            &root,
            501,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(again.is_none());
        // The backstop sees the same (workspace, issue, PR) and posts nothing.
        let reconciled = reconcile_recent_merges(
            "loom_daemon",
            &root,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(reconciled.is_empty(), "reconciliation must not re-post the step's merge");
    })
    .await;
}

#[tokio::test]
#[serial]
async fn an_unmerged_pr_narrates_nothing_and_stays_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = ws.to_string_lossy().into_owned();
    let gh = FakeGh::new(dir.path());
    gh.serve_view(&pr_row(502, 7, None));
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    forge_gate::scoped(None, async {
        let out = completion_for_merged_step(
            "loom_daemon",
            &root,
            502,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(out.is_none() && completed.is_empty());
    })
    .await;
    // While the breaker cools the step path makes no forge call and records
    // nothing, so reconciliation still finds the merge later.
    forge_gate::scoped(cooling(600), async {
        gh.serve_view(&pr_row(502, 7, Some(1)));
        let out = completion_for_merged_step(
            "loom_daemon",
            &root,
            502,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(out.is_none() && completed.is_empty());
    })
    .await;
    assert_eq!(gh.calls_of("pr view"), 1, "{:?}", gh.calls());
}

/// Through the live sink: a merged Champion merge step posts exactly one
/// completion; `noop`, `refused` and `failed` steps, a rejected merge, a
/// Builder verdict and a duplicate end of the merged step post nothing and
/// (except the duplicate) never reach the forge.
#[tokio::test]
#[serial]
async fn the_sink_narrates_one_completion_per_merged_step_and_none_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let _paths = SafehouseTestPaths::set(dir.path());
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = ws.to_string_lossy().into_owned();
    let gh = FakeGh::new(dir.path());
    gh.serve_view(&pr_row(503, 77, Some(1)));
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // Ask for two sends: the server returns with what it got when the sink
    // hangs up, so a second (duplicate) completion would show up here.
    let server = tokio::spawn(stub_server(listener, false, 2));
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(forge_gate::scoped(
        None,
        run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
            None,
        ),
    ));
    for event in [
        step_ended(503, StepRole::ChampionMerge, "noop", "held", &root),
        step_ended(503, StepRole::ChampionMerge, "refused", "not_eligible", &root),
        step_ended(503, StepRole::ChampionMerge, "failed", "unclassified", &root),
        step_ended(503, StepRole::ChampionMerge, "verdict", "rejected", &root),
        step_ended(77, StepRole::Builder, "verdict", "pr_opened", &root),
        step_ended(503, StepRole::ChampionMerge, "verdict", "merged", &root),
        step_ended(503, StepRole::ChampionMerge, "verdict", "merged", &root),
    ] {
        bus.publish(event).unwrap();
    }
    // Let the sink drain the queue, then hang up.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the stub server returns once the sink hangs up")
        .unwrap();
    let completions: Vec<_> = received
        .iter()
        .filter(|r| r["type"] == "completion")
        .collect();
    assert_eq!(completions.len(), 1, "exactly one completion: {received:?}");
    assert_eq!(received.len(), 1, "a step end narrates nothing else: {received:?}");
    assert!(completions[0]["body"].as_str().unwrap().contains("PR #503"));
    // Only the two merge verdicts looked the PR up; nothing else did.
    assert_eq!(gh.calls_of("pr view"), 2, "{:?}", gh.calls());
}

/// The backstop: a tick the breaker defers is not spent, and the first tick
/// after it clears reads the workspace from its watermark, so a merge that
/// landed during the deferral is narrated exactly once.
#[tokio::test]
#[serial]
async fn the_backstop_defers_while_cooling_and_catches_up_from_its_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let _paths = SafehouseTestPaths::set(dir.path());
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = ws.to_string_lossy().into_owned();
    let targets = vec![root.clone()];
    let gh = FakeGh::new(dir.path());
    let mut state = ReconcileState::new(false);
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();

    // 1. An answered pass with nothing merged sets the watermark.
    let first = forge_gate::scoped(None, async {
        state
            .tick(&targets, "loom_daemon", &mut slug_cache, &mut completed, None, None, None)
            .await
    })
    .await;
    assert!(first.envelopes.is_empty());
    let watermark = state
        .watermarks
        .since(&root)
        .expect("an answered pass sets it");
    assert_eq!(gh.calls_of("pr list"), 1);
    assert!(!gh.calls()[0].contains("--search"), "the first pass is unwatermarked");

    // 2. The breaker trips; a merge lands meanwhile. Deferred ticks make no
    // forge call, keep the cursor and the watermark.
    gh.serve_list(&format!("[{}]", pr_row(504, 88, Some(1))));
    let breaker = cooling(2);
    for _ in 0..3 {
        let deferred = forge_gate::scoped(breaker.clone(), async {
            state
                .tick(&targets, "loom_daemon", &mut slug_cache, &mut completed, None, None, None)
                .await
        })
        .await;
        assert!(deferred.envelopes.is_empty());
    }
    assert_eq!(gh.calls_of("pr list"), 1, "no forge call while cooling: {:?}", gh.calls());
    assert_eq!(state.cursor, 1, "a deferred tick does not advance the cursor");
    assert!(state.deferred_since.is_some());
    assert_eq!(state.watermarks.since(&root), Some(watermark));

    // 3. The breaker clears: the next tick reads from the watermark and
    // narrates the merge once.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let caught_up = forge_gate::scoped(breaker.clone(), async {
        state
            .tick(&targets, "loom_daemon", &mut slug_cache, &mut completed, None, None, None)
            .await
    })
    .await;
    assert_eq!(caught_up.envelopes.len(), 1, "the deferred merge is narrated");
    assert_eq!(caught_up.workspace_root.as_deref(), Some(root.as_str()));
    assert!(completed.contains(&(root.clone(), 88, 504)));
    assert!(state.deferred_since.is_none());
    let query = gh
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("pr list"))
        .nth(1)
        .unwrap();
    let expected = format!("merged:>={}", watermark.to_rfc3339_opts(SecondsFormat::Secs, true));
    assert!(query.contains(&expected), "watermarked query: {query}");
    assert!(query.contains("--limit 100"), "{query}");
    assert!(state.watermarks.since(&root).unwrap() > watermark, "the watermark advances");

    // 4. Once narrated, never again.
    let again = forge_gate::scoped(None, async {
        state
            .tick(&targets, "loom_daemon", &mut slug_cache, &mut completed, None, None, None)
            .await
    })
    .await;
    assert!(again.envelopes.is_empty());
}

/// A pass whose forge read fails keeps the watermark, so the next answered
/// pass still reads the whole window.
#[tokio::test]
#[serial]
async fn a_failed_pass_keeps_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let root = ws.to_string_lossy().into_owned();
    let targets = vec![root.clone()];
    let gh = FakeGh::new(dir.path());
    let mut state = ReconcileState::new(false);
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    forge_gate::scoped(None, async {
        state
            .tick(&targets, "loom_daemon", &mut slug_cache, &mut completed, None, None, None)
            .await;
        let watermark = state.watermarks.since(&root).unwrap();
        gh.serve_list("not json");
        state
            .tick(&targets, "loom_daemon", &mut slug_cache, &mut completed, None, None, None)
            .await;
        assert_eq!(state.watermarks.since(&root), Some(watermark));
    })
    .await;
}

#[test]
fn watermarks_never_move_backwards_and_carry_the_slack() {
    let mut marks = ReconcileWatermarks::default();
    assert_eq!(marks.since("a"), None);
    let t = Utc::now();
    marks.advance("a", t);
    marks.advance("a", t - chrono::Duration::minutes(5));
    assert_eq!(marks.since("a"), Some(t - RECONCILE_WATERMARK_SLACK));
    let (args, limit) = reconcile_query_args(None);
    assert_eq!(limit, RECONCILE_PR_LIMIT);
    assert!(!args.iter().any(|a| a == "--search"));
    let (args, limit) = reconcile_query_args(Some(t));
    assert_eq!(limit, RECONCILE_CATCHUP_LIMIT);
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--search" && w[1].starts_with("merged:>=")));
}
