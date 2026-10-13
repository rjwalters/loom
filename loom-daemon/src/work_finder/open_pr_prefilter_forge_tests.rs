//! The batched open-PR pre-filter through the **production** dispatcher
//! ([`RegistryDispatcher`] over a real [`SweepRegistry`]) and a fake `gh`
//! that logs every call (Issue #8865).
//!
//! The registry here has its guards ON (`skip_label_flip = false`), so a
//! candidate the pre-filter lets through runs the real `dispatch()`: the
//! `prepare` phase, then the 2.5 closed-issue read and the 2.6 open-PR probe.
//! That is what makes the before/after comparison in
//! [`measurement_attempts_and_forge_calls_per_tick`] a comparison of the two
//! real paths rather than of two fakes.
//!
//! Every test holds the default `serial_test` key: the write-scope gate that
//! `prepare` runs reads process-global environment
//! (`crate::write_scope_test_support`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serial_test::serial;

use super::super::*;
use crate::claim_reconciliation::open_pr_listing::test_support::{listing, row, Row};
use crate::sweep_registry::test_support::{
    fake_gh_graphql_arm, fake_gh_pulls_state_arm, fake_gh_timeline_rest_arm, state_probe_json,
    touch_sweep_command,
};
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use crate::write_scope_test_support::WritableRoot;

struct OneShotSource(Option<Vec<WorkItem>>);

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

fn source(numbers: &[u32]) -> OneShotSource {
    OneShotSource(Some(
        numbers
            .iter()
            .map(|n| WorkItem::new(*n, vec!["loom:issue".to_string()]))
            .collect(),
    ))
}

/// Process environment a dispatch reads, cleared for the test and restored
/// after it.
struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Env {
    fn isolated(root: &Path) -> Self {
        let cleared = [
            "LOOM_REPO",
            "GH_REPO",
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_MODEL",
            "LOOM_MODEL_PROFILE",
            "LOOM_MODEL_EXPERIMENT",
            "LOOM_BACKSTOP_LEASE_DIR",
            "LOOM_OPEN_PR_MEMO",
        ];
        let emptied = [
            "LOOM_CONFIG_DEFAULTS_FILE",
            "LOOM_SHARED_TOKENS_DIR",
            "LOOM_SHARED_API_KEYS_DIR",
        ];
        let prior = cleared
            .into_iter()
            .chain(emptied)
            .map(|key| (key, std::env::var_os(key)))
            .collect();
        for key in cleared {
            std::env::remove_var(key);
        }
        for key in emptied {
            std::env::set_var(key, "");
        }
        std::env::set_var("LOOM_BACKSTOP_LEASE_DIR", root.join("backstop"));
        Self(prior)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// How the fake `gh` answers the open-PR listing.
enum Pulls {
    /// `200` with these rows and an ETag; `304` when the ETag is presented.
    Rows(Vec<Row>),
    /// The listing after it changed: `200` with these rows whatever ETag is
    /// presented.
    Changed(Vec<Row>),
    /// A forge outage (`502`).
    Failing,
}

/// A registered workspace, its registry, and the fake `gh`'s call log.
struct Fixture {
    registry: Arc<Mutex<SweepRegistry>>,
    log: PathBuf,
    /// The script the registry's `gh` runs; [`Self::reanswer`] rewrites it.
    inner: PathBuf,
    ws: WritableRoot,
    _env: Env,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// `pulls(repo)` builds the listing for the fixture's `owner/repo`.
    /// `graphql_prs` is what the closes-graph fallback answers (open PR
    /// numbers, whitespace separated).
    fn new(pulls: impl FnOnce(&str) -> Pulls, graphql_prs: &str) -> Self {
        Self::build(pulls, graphql_prs, false)
    }

    /// [`Self::new`] with the registry's guards on (`false`) or off (`true`).
    fn build(pulls: impl FnOnce(&str) -> Pulls, graphql_prs: &str, skip_label_flip: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let env = Env::isolated(&root);
        let inner = root.join("fake-gh.sh");
        let ws = WritableRoot::register_with_gh(&root, &inner);
        let log = root.join("gh.log");

        let script =
            fake_gh(&root, &ws.repo, pulls(&ws.repo), &fake_gh_graphql_arm(graphql_prs, 0));
        write_executable(&inner, &script);

        let spawn = root.join(".loom/scripts/spawn-claude.sh");
        std::fs::create_dir_all(spawn.parent().unwrap()).unwrap();
        write_executable(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n");
        touch_sweep_command(&root);

        let mut config = SweepRegistryConfig::new(root.clone());
        config.spawn_bin = Some(spawn);
        config.gh_bin = Some(ws.gh.clone());
        config.skip_label_flip = skip_label_flip;
        config.journal_path = Some(root.join("test-sweeps-journal.json"));
        Self {
            registry: Arc::new(Mutex::new(SweepRegistry::new(config))),
            log,
            inner,
            ws,
            _env: env,
            _dir: dir,
        }
    }

    /// Change what the forge answers from now on: the listing, and the legs
    /// behind it (see [`fake_gh`]).
    fn reanswer(&self, pulls: impl FnOnce(&str) -> Pulls, legs: &str) {
        let root = self.inner.parent().unwrap();
        let script = fake_gh(root, &self.ws.repo, pulls(&self.ws.repo), legs);
        write_executable(&self.inner, &script);
    }

    /// The fresh #6788 memo entry's PR for `issue`, if any.
    fn fresh_memo(&self, issue: u32) -> Option<u32> {
        let reg = self.registry.lock().unwrap();
        reg.fresh_open_pr_memo(issue, chrono::Utc::now())
            .map(|memo| memo.pr)
    }

    fn dispatcher(&self) -> RegistryDispatcher {
        RegistryDispatcher::new(Arc::clone(&self.registry))
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Calls made after the first `skip`, as a [`Calls`] tally.
    fn tally_from(&self, skip: usize) -> Calls {
        let calls = self.calls();
        let calls = &calls[skip.min(calls.len())..];
        let count = |pred: &dyn Fn(&str) -> bool| calls.iter().filter(|c| pred(c)).count();
        Calls {
            total: calls.len(),
            listing: count(&|c| c.contains("pulls?state=open")),
            issue: count(&|c| c.contains("/issues/")),
            graphql: count(&|c| c.contains("graphql")),
        }
    }

    fn failure_count(&self, issue: u32) -> u32 {
        self.registry.lock().unwrap().dispatch_failure_count(issue)
    }

    fn backed_off(&self) -> HashSet<u32> {
        self.registry
            .lock()
            .unwrap()
            .dispatch_backoff_issues(chrono::Utc::now())
    }
}

/// `gh` calls of one kind made during a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Calls {
    total: usize,
    /// Open-PR listing page reads.
    listing: usize,
    /// Per-issue reads (`repos/…/issues/<n>…`).
    issue: usize,
    graphql: usize,
}

/// The fake `gh`: logs each call, answers the open-PR listing per `pulls`,
/// then `legs` (arms for the guard's other reads, tried before the catch-all),
/// then `repo view` and an open-issue body for any other `api repos/…` read.
fn fake_gh(root: &Path, repo: &str, pulls: Pulls, legs: &str) -> String {
    let pulls_arm = match pulls {
        Pulls::Rows(rows) => pages_arm(root, &rows, true),
        Pulls::Changed(rows) => pages_arm(root, &rows, false),
        Pulls::Failing => "case \"$*\" in api*'pulls?state=open'*)\n  \
             echo 'gh: Something went wrong (HTTP 502)' >&2; exit 1 ;;\nesac\n"
            .to_string(),
    };
    format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         {pulls_arm}\
         {legs}\
         if [[ \"$1\" == repo && \"$2\" == view ]]; then printf '{repo}\\n'; exit 0; fi\n\
         if [[ \"$1\" == api && \"$2\" == repos/* ]]; then printf '%s\\n' '{state}'; exit 0; fi\n\
         exit 0\n",
        log = root.join("gh.log").display(),
        state = state_probe_json("open", false),
    )
}

/// [`fake_gh`] legs for the #6788 outage shape: the closes-graph and the
/// timeline walk both fail, and only the targeted `pulls/<n>` read answers
/// (`open`).
fn only_the_pull_read_answers() -> String {
    format!(
        "{}{}{}",
        fake_gh_graphql_arm("", 1),
        fake_gh_timeline_rest_arm("", 1),
        fake_gh_pulls_state_arm("open", 0)
    )
}

fn write_executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_all();
    }
}

/// A `bash` arm serving `rows` as 100-row listing pages from files under
/// `root`, each `200` with a per-page ETag, or `304` (exit 1, like real
/// `gh`) when the caller presents one and `conditional` is set.
fn pages_arm(root: &Path, rows: &[Row], conditional: bool) -> String {
    let not_modified = "case \"$*\" in api*'pulls?state=open'*'If-None-Match'*)\n  \
         printf 'HTTP/2.0 304 Not Modified\\r\\n\\r\\n'; exit 1 ;;\nesac\n";
    let mut arm = String::from(if conditional { not_modified } else { "" });
    let tag = if conditional { "p" } else { "q" };
    let pages: Vec<&[Row]> = if rows.is_empty() {
        vec![rows]
    } else {
        rows.chunks(100).collect()
    };
    // A full last page is followed by an empty one, as on the forge.
    let trailing_empty = rows.len().is_multiple_of(100) && !rows.is_empty();
    for (i, page) in pages.iter().enumerate() {
        let file = root.join(format!("pulls-page-{}.json", i + 1));
        std::fs::write(&file, listing(page)).unwrap();
        arm.push_str(&format!(
            "case \"$*\" in api*'pulls?state=open'*'&page={n}'|api*'pulls?state=open'*'&page={n} '*)\n  \
             printf 'HTTP/2.0 200 OK\\r\\nEtag: W/\"{tag}{n}\"\\r\\n\\r\\n'; cat '{file}'; exit 0 ;;\nesac\n",
            n = i + 1,
            file = file.display(),
        ));
    }
    if trailing_empty {
        arm.push_str(
            "case \"$*\" in api*'pulls?state=open'*)\n  \
             printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'; echo '[]'; exit 0 ;;\nesac\n",
        );
    }
    arm
}

/// An open same-repo PR `number` on a topic branch, linking `issue` by body.
fn closing(repo: &str, number: u32, issue: u32) -> Row {
    row(number, &[])
        .head(&format!("topic-{number}"))
        .repo(repo)
        .body(&format!("Closes #{issue}"))
}

fn tick_one<D: WorkDispatcher>(disp: D, numbers: &[u32], cap: usize) -> (TickReport, D) {
    let mut multi = vec![(source(numbers), disp)];
    let report = tick_multi(&mut multi, &[1], cap, &[false]);
    let (_, disp) = multi.remove(0);
    (report, disp)
}

/// The production dispatcher as it was before #8865: everything delegated,
/// `open_pr_blocked` left at the trait default (nothing pre-filtered).
struct BeforePrefilter(RegistryDispatcher);

impl WorkDispatcher for BeforePrefilter {
    fn in_flight(&self) -> HashSet<u32> {
        self.0.in_flight()
    }
    fn occupancy(&self) -> usize {
        self.0.occupancy()
    }
    fn backed_off(&self) -> HashSet<u32> {
        self.0.backed_off()
    }
    fn pr_open_backed_off(&self) -> HashSet<u32> {
        self.0.pr_open_backed_off()
    }
    fn workspace_commands_missing(&self) -> bool {
        self.0.workspace_commands_missing()
    }
    fn dispatch(&mut self, issue: u32) -> Result<bool> {
        self.0.dispatch(issue)
    }
    fn dispatch_with(&mut self, issue: u32, overflow: bool) -> Result<bool> {
        self.0.dispatch_with(issue, overflow)
    }
}

const N: u32 = 12;

fn blocked_numbers() -> Vec<u32> {
    (8601..8601 + N).collect()
}

fn blocked_rows(repo: &str) -> Vec<Row> {
    blocked_numbers()
        .into_iter()
        .map(|issue| closing(repo, issue + 1000, issue))
        .collect()
}

/// AC1, AC2, AC5: N PR-blocked candidates cost ONE listing read and no
/// per-issue read, no `dispatch()` attempt is logged, and no backoff is armed.
#[test]
#[serial]
fn n_blocked_candidates_cost_one_listing_read_and_no_issue_read() {
    let fx = Fixture::new(|repo| Pulls::Rows(blocked_rows(repo)), "");
    let numbers = blocked_numbers();
    let before = fx.calls().len();

    let mut report = None;
    let logs = crate::test_log_capture::capture_logs(|| {
        report = Some(tick_one(fx.dispatcher(), &numbers, 5).0);
    });
    let report = report.unwrap();

    assert_eq!(report.skipped_pr_open, N as usize);
    assert_eq!(report.pr_open_prefiltered, N as usize);
    assert_eq!((report.dispatched, report.errors), (0, 0));
    assert!(report.admissions.is_empty());

    let calls = fx.tally_from(before);
    assert_eq!(calls.listing, 1, "one listing read for the workspace: {:?}", fx.calls());
    assert_eq!(calls.issue, 0, "no per-candidate issue read: {:?}", fx.calls());
    assert_eq!(calls.graphql, 0, "{:?}", fx.calls());

    for forbidden in [
        "work_finder: attempting issue",
        "runtime_preference:",
        "sweep dispatch: attempting",
    ] {
        assert!(
            !logs.iter().any(|(_, msg)| msg.contains(forbidden)),
            "`{forbidden}` must not be logged for a pre-filtered candidate: {logs:?}"
        );
    }
    assert!(
        !logs
            .iter()
            .any(|(_, msg)| msg.contains("dispatch backoff armed")),
        "{logs:?}"
    );
    for issue in numbers {
        assert_eq!(fx.failure_count(issue), 0, "issue #{issue}");
    }
    assert!(fx.backed_off().is_empty(), "no dispatch backoff armed");
}

/// AC3: with the listing unreadable the pre-filter skips nothing, and each
/// candidate is refused by `dispatch()`'s own guard through its GraphQL
/// fallback, exactly as on the pre-#8865 path.
#[test]
#[serial]
fn an_unreadable_listing_leaves_every_candidate_to_the_dispatch_guard() {
    let numbers = [8701, 8702, 8703];
    let fx = Fixture::new(|_| Pulls::Failing, "9700");
    let (report, _) = tick_one(fx.dispatcher(), &numbers, 5);

    assert_eq!(report.pr_open_prefiltered, 0, "nothing verified, nothing pre-filtered");
    assert_eq!(report.skipped_pr_open, 3, "the in-dispatch guard refused all three");
    assert_eq!(report.admissions.len(), 3, "each was a real dispatch attempt");
    assert_eq!((report.dispatched, report.errors), (0, 0));
    let calls = fx.tally_from(0);
    assert!(
        calls.graphql >= 3,
        "the guard's closes-graph fallback ran per candidate: {calls:?}"
    );
    // The failed read backs off (`LISTING_RETRY_AFTER`): the outage costs one
    // listing call, not one for the pre-filter plus one per candidate.
    assert_eq!(calls.listing, 1, "{calls:?}");

    // The same tick through the pre-#8865 dispatcher is the same report.
    let fx_before = Fixture::new(|_| Pulls::Failing, "9700");
    let (before, _) = tick_one(BeforePrefilter(fx_before.dispatcher()), &numbers, 5);
    assert_eq!(report, before);
}

/// What links and what does not is the guard's own classifier: a fork PR from
/// an untrusted author does not block (H14), a passing mention does not block
/// (#8940), a `feature/issue-N` head with no closing phrase does.
#[test]
#[serial]
fn the_prefilter_uses_the_guards_classifier() {
    let fx = Fixture::new(
        |repo| {
            Pulls::Rows(vec![
                closing(repo, 9801, 8801),
                row(8802, &[]).repo(repo).body("no phrase"), // head feature/issue-8802
                row(9803, &[])
                    .head("topic")
                    .repo(repo)
                    .body("filed #8803 to track it"),
                row(9804, &[])
                    .head("topic")
                    .repo("outsider/loom")
                    .author("rando", "NONE")
                    .body("Closes #8804"),
                row(9805, &[])
                    .head("topic")
                    .repo("friend/loom")
                    .author("pal", "COLLABORATOR")
                    .body("Closes #8805"),
            ])
        },
        "",
    );
    let blocked = fx
        .dispatcher()
        .open_pr_blocked(&[8801, 8802, 8803, 8804, 8805, 8806]);
    let mut got: Vec<(u32, u32)> = blocked.into_iter().collect();
    got.sort_unstable();
    assert_eq!(got, vec![(8801, 9801), (8802, 8802), (8805, 9805)]);
    assert_eq!(fx.tally_from(0).listing, 1);
}

/// A repo with more than 100 open PRs: the multi-page listing is walked once
/// per tick however many candidates it blocks (two pages plus the walk's own
/// page-1 consistency re-read), not once per candidate.
#[test]
#[serial]
fn a_multi_page_listing_is_walked_once_per_tick() {
    let pulls = |repo: &str| {
        let mut rows = blocked_rows(repo);
        rows.extend((1..=95).map(|i| closing(repo, 20_000 + i, 30_000 + i)));
        Pulls::Rows(rows)
    };
    let walk = |numbers: &[u32]| {
        let fx = Fixture::new(pulls, "");
        let (report, _) = tick_one(fx.dispatcher(), numbers, 5);
        assert_eq!(report.pr_open_prefiltered, numbers.len());
        let calls = fx.tally_from(0);
        assert_eq!(calls.issue, 0, "{calls:?}");
        calls.listing
    };
    let one = walk(&blocked_numbers()[..1]);
    let all = walk(&blocked_numbers());
    assert_eq!(one, 3, "page 1, page 2, and the re-read of page 1");
    assert_eq!(all, one, "the walk does not grow with the number of blocked candidates");
}

/// Mixed queue through the real guard chain: the blocked candidate is
/// pre-filtered and the unblocked one passes `dispatch()`'s own 2.5/2.6
/// guards and is dispatched in the same tick.
#[test]
#[serial]
fn an_unblocked_candidate_is_dispatched_in_the_same_tick() {
    let fx = Fixture::new(|repo| Pulls::Rows(vec![closing(repo, 9901, 8901)]), "");
    let (first, _) = tick_one(fx.dispatcher(), &[8901, 8902], 5);
    assert_eq!(first.pr_open_prefiltered, 1);
    assert_eq!(first.dispatched, 1, "the unblocked #8902 is dispatched in the same tick");
    assert_eq!(fx.failure_count(8901), 0);
    assert!(!fx.backed_off().contains(&8901));
}

/// The #6788 backstop stays fed (PR #11430 review). A pre-filtered tick
/// records the verified PR in the memo although the 2.6 probe never ran for
/// it, so on a following tick where the listing, the closes-graph and the
/// timeline all fail, `dispatch()` still refuses the issue: at 2.5 while the
/// entry is fresh, and through the `pulls/<n>` recheck once it is not.
#[test]
#[serial]
fn a_prefiltered_block_still_holds_when_every_probe_leg_fails() {
    let fx = Fixture::new(|repo| Pulls::Rows(vec![closing(repo, 9951, 8951)]), "");
    let (first, _) = tick_one(fx.dispatcher(), &[8951], 5);
    assert_eq!(first.pr_open_prefiltered, 1);
    assert_eq!(fx.fresh_memo(8951), Some(9951), "the pre-filter recorded its verified verdict");

    // Outage, memo still fresh: refused at 2.5 without a probe.
    fx.reanswer(|_| Pulls::Failing, &only_the_pull_read_answers());
    let before = fx.calls().len();
    let (fresh, _) = tick_one(fx.dispatcher(), &[8951], 5);
    assert_eq!((fresh.skipped_pr_open, fresh.pr_open_prefiltered), (1, 0));
    assert_eq!((fresh.dispatched, fresh.errors), (0, 0));
    assert_eq!(fresh.admissions.len(), 1, "refused by dispatch() itself");
    assert_eq!(fx.tally_from(before).graphql, 0, "{:?}", fx.calls());

    // Same outage with the entry past its freshness window (and the #4485
    // window the refusal above armed cleared): the known-PR recheck holds.
    {
        let mut reg = fx.registry.lock().unwrap();
        reg.seed_open_pr_memo(8951, 9951, chrono::Utc::now() - chrono::Duration::hours(3));
        reg.clear_dispatch_backoff(8951);
    }
    assert_eq!(fx.fresh_memo(8951), None);
    let before = fx.calls().len();
    let (stale, _) = tick_one(fx.dispatcher(), &[8951], 5);
    assert_eq!((stale.skipped_pr_open, stale.pr_open_prefiltered), (1, 0));
    assert_eq!((stale.dispatched, stale.errors), (0, 0));
    let calls = fx.calls()[before..].to_vec();
    assert!(calls.iter().any(|c| c.contains("graphql")), "the probe ran: {calls:?}");
    assert!(calls.iter().any(|c| c.contains("pulls/9951")), "the recheck ran: {calls:?}");

    // Control: the same outage with no pre-filtered tick before it has no
    // entry to recheck, and the guard falls open (the documented contract).
    let cold = Fixture::new(|_| Pulls::Failing, "");
    cold.reanswer(|_| Pulls::Failing, &only_the_pull_read_answers());
    let (open, _) = tick_one(cold.dispatcher(), &[8951], 5);
    assert_eq!((open.dispatched, open.skipped_pr_open), (1, 0));
}

/// A PR that closes frees its issue on the next tick (PR #11430 review): the
/// listing's verified "no open PR" removes the memo entry the earlier tick
/// recorded, so 2.5 does not refuse the issue for the rest of the 900 s
/// freshness window and no backoff is armed.
#[test]
#[serial]
fn a_closed_pr_frees_its_issue_on_the_next_tick() {
    let fx = Fixture::new(|repo| Pulls::Rows(vec![closing(repo, 9961, 8961)]), "");
    let (first, _) = tick_one(fx.dispatcher(), &[8961], 5);
    assert_eq!((first.pr_open_prefiltered, first.dispatched), (1, 0));
    assert_eq!(fx.fresh_memo(8961), Some(9961));

    fx.reanswer(|_| Pulls::Changed(Vec::new()), &fake_gh_graphql_arm("", 0));
    let (next, _) = tick_one(fx.dispatcher(), &[8961], 5);
    assert_eq!(fx.fresh_memo(8961), None, "the verified absence removed the entry");
    assert_eq!((next.dispatched, next.skipped_pr_open, next.errors), (1, 0, 0));
    assert_eq!(fx.failure_count(8961), 0, "no open-PR backoff was armed");
}

/// An unreadable listing is no verdict: it neither records nor removes a memo
/// entry.
#[test]
#[serial]
fn an_unreadable_listing_leaves_the_memo_alone() {
    let fx = Fixture::new(|_| Pulls::Failing, "");
    let at = chrono::Utc::now() - chrono::Duration::hours(3);
    fx.registry
        .lock()
        .unwrap()
        .seed_open_pr_memo(8971, 9971, at);
    assert!(fx.dispatcher().open_pr_blocked(&[8971, 8972]).is_empty());
    let reg = fx.registry.lock().unwrap();
    let later = at + chrono::Duration::seconds(1);
    assert_eq!(
        reg.fresh_open_pr_memo(8971, later)
            .map(|m| (m.pr, m.verified_at)),
        Some((9971, at))
    );
    assert!(reg.fresh_open_pr_memo(8972, chrono::Utc::now()).is_none());
}

/// Guards off (`skip_label_flip`, as in a credential-less fixture): the
/// pre-filter reads nothing, mirroring steps 2.5/2.6.
#[test]
#[serial]
fn guards_off_means_no_prefilter_read() {
    let fx = Fixture::build(|repo| Pulls::Rows(blocked_rows(repo)), "", true);
    let before = fx.calls().len();
    assert!(fx
        .dispatcher()
        .open_pr_blocked(&blocked_numbers())
        .is_empty());
    assert_eq!(fx.tally_from(before).total, 0, "{:?}", fx.calls());
}

/// The before/after measurement the issue asks for (#8865), on the two real
/// paths: one tick over N PR-blocked ready issues through the production
/// dispatcher with the pre-filter left out (`BeforePrefilter`, the pre-#8865
/// tick) and with it. Run with `--no-capture` to print the table.
///
/// The fake `gh` is a local `bash` script, so the wall times here are a floor:
/// they show the shape (N serialized attempts against one read), not what a
/// real `gh` round trip costs.
#[test]
#[serial]
fn measurement_attempts_and_forge_calls_per_tick() {
    let numbers = blocked_numbers();
    let measure = |before: bool| {
        let fx = Fixture::new(|repo| Pulls::Rows(blocked_rows(repo)), "");
        // Resolve the repo and warm the listing's ETag first, as a running
        // daemon has (its reconciliation passes read this listing every pass):
        // the measured tick then sees conditional `304` reads on both paths.
        // Asked about an issue outside the set, so the warm-up leaves no
        // #6788 memo entry for a measured candidate: both paths start cold.
        let _ = fx.dispatcher().open_pr_blocked(&[1]);
        let skip = fx.calls().len();
        let started = Instant::now();
        let report = if before {
            tick_one(BeforePrefilter(fx.dispatcher()), &numbers, 5).0
        } else {
            tick_one(fx.dispatcher(), &numbers, 5).0
        };
        let elapsed = started.elapsed();
        let armed = numbers.iter().filter(|n| fx.failure_count(**n) > 0).count();
        (report, fx.tally_from(skip), elapsed, armed)
    };
    let (before, before_calls, before_time, before_armed) = measure(true);
    let (after, after_calls, after_time, after_armed) = measure(false);

    let line = |name: &str, r: &TickReport, c: Calls, t: Duration, armed: usize| {
        format!(
            "{name:<7} attempts={:>2} gh_calls={:>2} (listing={} issue={}) backoffs_armed={:>2} \
             tick={:>7.1}ms per_blocked_candidate={:>6.2}ms",
            r.admissions.len(),
            c.total,
            c.listing,
            c.issue,
            armed,
            t.as_secs_f64() * 1e3,
            t.as_secs_f64() * 1e3 / f64::from(N),
        )
    };
    eprintln!("#8865 measurement, one tick over {N} PR-blocked ready issues (fake gh):");
    eprintln!("{}", line("before", &before, before_calls, before_time, before_armed));
    eprintln!("{}", line("after", &after, after_calls, after_time, after_armed));

    // Same outcome for every candidate...
    assert_eq!(before.skipped_pr_open, N as usize);
    assert_eq!(after.skipped_pr_open, N as usize);
    assert_eq!((before.dispatched, after.dispatched), (0, 0));
    // ...at N attempts before and none after,
    assert_eq!(before.admissions.len(), N as usize);
    assert_eq!(after.admissions.len(), 0);
    // one issue read and one listing read per candidate before, one listing
    // read in total after,
    assert_eq!(before_calls.issue, N as usize, "{before_calls:?}");
    assert_eq!(before_calls.listing, N as usize, "{before_calls:?}");
    assert_eq!((after_calls.issue, after_calls.listing), (0, 1), "{after_calls:?}");
    // and a backoff armed per candidate before, none after.
    assert_eq!((before_armed, after_armed), (N as usize, 0));
}
