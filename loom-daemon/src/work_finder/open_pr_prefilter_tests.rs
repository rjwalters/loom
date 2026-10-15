//! The batched open-PR pre-filter at the tick seam (Issue #8865), with a fake
//! dispatcher. Its own file with its own fakes because `work_finder/tests.rs`
//! is over the file-size ratchet. The production dispatcher and a fake `gh`
//! are driven in `open_pr_prefilter_forge_tests.rs`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Result;

use super::super::*;
use super::log_changes;
use crate::sweep_registry::OpenPrDispatchError;
use crate::types::{QueueDisposition, ReadyQueueRow};

struct OneShotSource(Option<Vec<WorkItem>>);

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

/// How the fake answers [`WorkDispatcher::open_pr_blocked`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// The listing reads: every asked issue in `open_pr` is reported.
    Readable,
    /// The listing cannot be read: nothing is reported.
    Unreadable,
    /// The pre-#8865 dispatcher: the trait default, which reports nothing.
    NoPrefilter,
    /// #12025: the listing reads, and every asked issue gets a verdict
    /// (`Open` or `NoneOpen`), except those in `Disp::no_verdict`.
    Verified,
    /// #12025: the listing could not be read (`OpenPrAnswer::Unreadable`).
    Failed,
}

/// A dispatcher whose forge knows `open_pr` (issue to open linked PR). Its
/// `dispatch` refuses those issues with the typed error the real #4123 guard
/// returns, so a candidate the pre-filter lets through meets the same refusal
/// as before.
struct Disp {
    open_pr: HashMap<u32, u32>,
    listing: Listing,
    in_flight: HashSet<u32>,
    /// How many times `open_pr_blocked` was asked, and with what.
    asked: RefCell<Vec<Vec<u32>>>,
    /// Every `dispatch_with` call, in order (refused ones included).
    attempts: Vec<u32>,
    dispatched: Vec<u32>,
    /// `dispatch_with` calls across every workspace sharing this log.
    fleet_attempts: Rc<RefCell<Vec<u32>>>,
    backoffs_armed: Cell<usize>,
    /// Issues a `Verified` listing gives no verdict for.
    no_verdict: HashSet<u32>,
}

impl Disp {
    fn new(open_pr: &[(u32, u32)], listing: Listing) -> Self {
        Self {
            open_pr: open_pr.iter().copied().collect(),
            listing,
            in_flight: HashSet::new(),
            asked: RefCell::new(Vec::new()),
            attempts: Vec::new(),
            dispatched: Vec::new(),
            fleet_attempts: Rc::new(RefCell::new(Vec::new())),
            backoffs_armed: Cell::new(0),
            no_verdict: HashSet::new(),
        }
    }

    fn sharing(mut self, log: &Rc<RefCell<Vec<u32>>>) -> Self {
        self.fleet_attempts = Rc::clone(log);
        self
    }
}

impl WorkDispatcher for Disp {
    fn in_flight(&self) -> HashSet<u32> {
        self.in_flight.clone()
    }
    fn occupancy(&self) -> usize {
        self.in_flight.len()
    }
    fn open_pr_blocked(&self, candidates: &[u32]) -> HashMap<u32, u32> {
        self.asked.borrow_mut().push(candidates.to_vec());
        if !matches!(self.listing, Listing::Readable | Listing::Verified) {
            return HashMap::new();
        }
        candidates
            .iter()
            .filter_map(|n| self.open_pr.get(n).map(|pr| (*n, *pr)))
            .collect()
    }
    fn open_pr_answer(&self, candidates: &[u32]) -> open_pr_prefilter::OpenPrAnswer {
        use open_pr_prefilter::OpenPrAnswer;
        match self.listing {
            Listing::Failed => {
                self.asked.borrow_mut().push(candidates.to_vec());
                OpenPrAnswer::Unreadable
            }
            Listing::Verified => {
                let blocked = self.open_pr_blocked(candidates);
                let none_open = candidates
                    .iter()
                    .copied()
                    .filter(|n| !blocked.contains_key(n) && !self.no_verdict.contains(n))
                    .collect();
                OpenPrAnswer::Listed { blocked, none_open }
            }
            _ => OpenPrAnswer::NotAsked(self.open_pr_blocked(candidates)),
        }
    }
    fn dispatch(&mut self, issue: u32) -> Result<bool> {
        self.attempts.push(issue);
        self.fleet_attempts.borrow_mut().push(issue);
        if let Some(pr) = self.open_pr.get(&issue) {
            // What the real guard does on a refusal: arm the #4485 ladder.
            self.backoffs_armed.set(self.backoffs_armed.get() + 1);
            return Err(OpenPrDispatchError { issue, pr: *pr }.into());
        }
        self.dispatched.push(issue);
        self.in_flight.insert(issue);
        Ok(true)
    }
}

fn item(n: u32, created: &str) -> WorkItem {
    WorkItem::with_created_at(n, vec!["loom:issue".to_string()], Some(created.to_string()))
}

/// Issues `numbers`, each a day older than the next.
fn items(numbers: &[u32]) -> Vec<WorkItem> {
    numbers
        .iter()
        .enumerate()
        .map(|(i, n)| item(*n, &format!("2026-09-{:02}T00:00:00Z", i + 1)))
        .collect()
}

fn workspace(numbers: &[u32], disp: Disp) -> (OneShotSource, Disp) {
    (OneShotSource(Some(items(numbers))), disp)
}

fn rows(report: &TickReport) -> Vec<ReadyQueueRow> {
    let roots = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
    ready_queue::finish(&report.queue, &roots)
}

fn dispositions(report: &TickReport) -> Vec<(u32, QueueDisposition, Option<String>)> {
    let mut out: Vec<_> = rows(report)
        .into_iter()
        .map(|r| (r.issue, r.disposition, r.detail))
        .collect();
    out.sort_by_key(|r| r.0);
    out
}

/// AC1 + AC2: N blocked candidates cost one question per workspace and no
/// dispatch attempt, and are still reported `OpenPr` with the PR named.
#[test]
fn blocked_candidates_are_skipped_without_a_dispatch_attempt() {
    let blocked: Vec<(u32, u32)> = (1..=12).map(|n| (n, 900 + n)).collect();
    let numbers: Vec<u32> = (1..=12).collect();
    let mut multi = vec![workspace(&numbers, Disp::new(&blocked, Listing::Readable))];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);

    let disp = &multi[0].1;
    assert!(disp.attempts.is_empty(), "no dispatch_with for a blocked candidate");
    assert_eq!(disp.asked.borrow().len(), 1, "one listing question for the workspace");
    let mut asked = disp.asked.borrow()[0].clone();
    asked.sort_unstable();
    assert_eq!(asked, numbers, "asked about every candidate of the workspace at once");
    assert_eq!(disp.backoffs_armed.get(), 0, "no refusal, so no backoff armed");

    assert_eq!(report.skipped_pr_open, 12);
    assert_eq!(report.pr_open_prefiltered, 12);
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.errors, 0);
    assert!(report.admissions.is_empty(), "no attempt was made, so no admission span");
    for (issue, disposition, detail) in dispositions(&report) {
        assert_eq!(disposition, QueueDisposition::OpenPr, "issue #{issue}");
        assert_eq!(detail, Some(format!("open PR #{}", 900 + issue)));
    }
}

/// AC3: an unreadable listing skips nothing. Every candidate reaches
/// `dispatch()` and is refused by its own guard, exactly as before #8865.
#[test]
fn an_unreadable_listing_skips_nothing() {
    let blocked: Vec<(u32, u32)> = (1..=4).map(|n| (n, 900 + n)).collect();
    let mut multi = vec![workspace(
        &[1, 2, 3, 4, 5],
        Disp::new(&blocked, Listing::Unreadable),
    )];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);

    let disp = &multi[0].1;
    assert_eq!(disp.attempts.len(), 5, "every candidate was offered to dispatch()");
    assert_eq!(disp.dispatched, vec![5]);
    assert_eq!(disp.asked.borrow().len(), 1, "asked once, not once per candidate");
    assert_eq!(report.skipped_pr_open, 4, "the in-dispatch guard still refuses them");
    assert_eq!(report.pr_open_prefiltered, 0);
    assert_eq!(report.admissions.len(), 5);

    // Identical, field for field, to the pre-#8865 dispatcher's tick.
    let mut before = vec![workspace(
        &[1, 2, 3, 4, 5],
        Disp::new(&blocked, Listing::NoPrefilter),
    )];
    let report_before = tick_multi(&mut before, &[1], 5, &[false]);
    assert_eq!(report, report_before);
}

/// The pre-filter never admits: a candidate it does not report still goes
/// through `dispatch()`, whose guard refuses it. Here the listing reads but
/// misses issue 2 (a link only the closes-graph sees).
#[test]
fn a_candidate_the_listing_misses_still_meets_the_dispatch_guard() {
    struct Partial(Disp);
    impl WorkDispatcher for Partial {
        fn in_flight(&self) -> HashSet<u32> {
            self.0.in_flight()
        }
        fn open_pr_blocked(&self, candidates: &[u32]) -> HashMap<u32, u32> {
            let mut seen = self.0.open_pr_blocked(candidates);
            seen.remove(&2);
            seen
        }
        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            self.0.dispatch(issue)
        }
    }
    let disp = Disp::new(&[(1, 901), (2, 902)], Listing::Readable);
    let mut multi = vec![(OneShotSource(Some(items(&[1, 2, 3]))), Partial(disp))];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);

    assert_eq!(multi[0].1 .0.attempts, vec![2, 3], "1 is pre-filtered; 2 and 3 are attempted");
    assert_eq!(multi[0].1 .0.dispatched, vec![3]);
    assert_eq!(report.skipped_pr_open, 2);
    assert_eq!(report.pr_open_prefiltered, 1);
}

/// A report with the fields that legitimately differ between a pre-filtered
/// skip and an in-dispatch refusal blanked: the admission records (one per
/// attempt) and the pre-filter's own subset counter.
fn comparable(mut report: TickReport) -> TickReport {
    report.admissions.clear();
    report.pr_open_prefiltered = 0;
    report
}

/// Two workspaces of mixed blocked/unblocked candidates. Returns the report,
/// every `dispatch_with` call in fleet order, and the dispatched issues.
fn mixed_tick(listing: Listing, seed: u64, cap: usize) -> (TickReport, Vec<u32>, Vec<u32>) {
    let blocked_a = [(11, 911), (13, 913), (14, 914), (17, 917)];
    let blocked_b = [(21, 921), (22, 922), (25, 925)];
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut multi = vec![
        workspace(&[11, 12, 13, 14, 15, 16, 17, 18], Disp::new(&blocked_a, listing).sharing(&log)),
        workspace(&[21, 22, 23, 24, 25, 26], Disp::new(&blocked_b, listing).sharing(&log)),
    ];
    let report =
        workspace_draw::with_seed(seed, || tick_multi(&mut multi, &[1, 3], cap, &[false; 2]));
    let mut dispatched = multi[0].1.dispatched.clone();
    dispatched.extend(&multi[1].1.dispatched);
    let attempts = log.borrow().clone();
    (report, attempts, dispatched)
}

/// The pre-filter does not change selection: for every draw seed and
/// capacity, the candidates that survive it are attempted in exactly the
/// order the pre-#8865 tick attempted them (which interleaved the refused
/// ones), the same issues are dispatched, the #11103 draw log is identical,
/// and so is every queue row and counter.
#[test]
fn surviving_order_is_identical_with_and_without_the_prefilter() {
    let blocked: HashSet<u32> = [11, 13, 14, 17, 21, 22, 25].into_iter().collect();
    for seed in 0..40 {
        for cap in [1, 2, 3, 5, 100] {
            let (with, attempts_with, dispatched_with) = mixed_tick(Listing::Readable, seed, cap);
            let (without, attempts_without, dispatched_without) =
                mixed_tick(Listing::NoPrefilter, seed, cap);
            let ctx = format!("seed {seed}, cap {cap}");

            let survivors_without: Vec<u32> = attempts_without
                .iter()
                .copied()
                .filter(|n| !blocked.contains(n))
                .collect();
            assert_eq!(attempts_with, survivors_without, "attempt order of survivors ({ctx})");
            assert!(attempts_with.iter().all(|n| !blocked.contains(n)), "{ctx}");
            assert_eq!(dispatched_with, dispatched_without, "dispatched issues ({ctx})");

            assert_eq!(with.workspace_draw, without.workspace_draw, "draw log ({ctx})");
            assert_eq!(with.plan_order, without.plan_order, "plan order ({ctx})");
            assert_eq!(rows(&with), rows(&without), "queue rows ({ctx})");
            assert_eq!(
                with.pr_open_prefiltered,
                attempts_without.len() - attempts_with.len(),
                "every refused attempt became a pre-filtered skip ({ctx})"
            );
            assert_eq!(comparable(with), comparable(without), "tick report ({ctx})");
        }
    }
}

/// Mixed queue: the blocked candidates are skipped and the first unblocked
/// one takes the single slot in the same tick.
#[test]
fn the_first_unblocked_candidate_is_dispatched_in_the_same_tick() {
    let blocked: Vec<(u32, u32)> = (1..=9).map(|n| (n, 900 + n)).collect();
    let numbers: Vec<u32> = (1..=11).collect();
    let mut multi = vec![workspace(&numbers, Disp::new(&blocked, Listing::Readable))];
    let report = tick_multi(&mut multi, &[1], 1, &[false]);

    assert_eq!(multi[0].1.attempts, vec![10], "one attempt, for the first unblocked issue");
    assert_eq!(report.dispatched, 1);
    assert_eq!(report.skipped_pr_open, 9);
    assert_eq!(report.deferred_capacity, 1, "#11 waits for the slot #10 took");
}

/// The read is lazy: a tick in which no candidate reaches the dispatch point
/// asks nothing, so an idle, saturated, at-capacity or halted tick adds no
/// forge read.
#[test]
fn a_tick_that_cannot_place_anything_asks_nothing() {
    let blocked = [(1, 901), (2, 902)];
    let asked = |multi: &[(OneShotSource, Disp)]| multi[0].1.asked.borrow().len();

    // At capacity.
    let mut multi = vec![workspace(
        &[1, 2, 3],
        Disp::new(&blocked, Listing::Readable),
    )];
    let report = tick_multi(&mut multi, &[1], 0, &[false]);
    assert_eq!((asked(&multi), report.deferred_capacity), (0, 3));

    // Saturation brake.
    let mut multi = vec![workspace(
        &[1, 2, 3],
        Disp::new(&blocked, Listing::Readable),
    )];
    let report = tick_multi_with_saturation_brake(&mut multi, &[1], 5, &[false], usize::MAX, true);
    assert_eq!((asked(&multi), report.deferred_saturation), (0, 3));

    // Ramp cap of zero admissions.
    let mut multi = vec![workspace(
        &[1, 2, 3],
        Disp::new(&blocked, Listing::Readable),
    )];
    let report = tick_multi_with_admission_cap(&mut multi, &[1], 5, &[false], 0);
    assert_eq!((asked(&multi), report.deferred_ramp_cap), (0, 3));

    // Halted repo.
    let mut multi = vec![workspace(
        &[1, 2, 3],
        Disp::new(&blocked, Listing::Readable),
    )];
    tick_multi(&mut multi, &[1], 5, &[true]);
    assert_eq!(asked(&multi), 0);

    // Nothing ready.
    let mut multi = vec![workspace(&[], Disp::new(&blocked, Listing::Readable))];
    tick_multi(&mut multi, &[1], 5, &[false]);
    assert_eq!(asked(&multi), 0);
}

/// One question per workspace per tick, each about its own candidates only.
#[test]
fn each_workspace_is_asked_once_about_its_own_candidates() {
    let mut multi = vec![
        workspace(&[1, 2, 3], Disp::new(&[(1, 901), (3, 903)], Listing::Readable)),
        workspace(&[7, 8], Disp::new(&[(8, 908)], Listing::Readable)),
    ];
    let report = tick_multi(&mut multi, &[1, 1], 10, &[false, false]);

    for (idx, want) in [(0, vec![1, 2, 3]), (1, vec![7, 8])] {
        let asked = multi[idx].1.asked.borrow();
        assert_eq!(asked.len(), 1, "workspace #{idx}");
        let mut got = asked[0].clone();
        got.sort_unstable();
        assert_eq!(got, want, "workspace #{idx}");
    }
    assert_eq!(multi[0].1.dispatched, vec![2]);
    assert_eq!(multi[1].1.dispatched, vec![7]);
    assert_eq!((report.dispatched, report.skipped_pr_open), (2, 3));
}

/// Candidates dropped in pass 1 (here: in flight) are not asked about.
#[test]
fn only_surviving_candidates_are_asked_about() {
    let mut disp = Disp::new(&[(2, 902)], Listing::Readable);
    disp.in_flight.insert(1);
    let mut multi = vec![workspace(&[1, 2, 3], disp)];
    tick_multi(&mut multi, &[1], 10, &[false]);
    let mut asked = multi[0].1.asked.borrow()[0].clone();
    asked.sort_unstable();
    assert_eq!(asked, vec![2, 3]);
}

/// Nothing outlives the tick: an issue whose PR closes unmerged is dispatched
/// on the next tick whose listing shows it gone.
#[test]
fn a_closed_pr_frees_its_issue_on_the_next_tick() {
    let mut multi = vec![workspace(&[1], Disp::new(&[(1, 901)], Listing::Readable))];
    let first = tick_multi(&mut multi, &[1], 5, &[false]);
    assert_eq!((first.dispatched, first.skipped_pr_open), (0, 1));

    let mut multi = vec![workspace(&[1], Disp::new(&[], Listing::Readable))];
    let second = tick_multi(&mut multi, &[1], 5, &[false]);
    assert_eq!((second.dispatched, second.skipped_pr_open), (1, 0));
    assert_eq!(multi[0].1.dispatched, vec![1]);
}

/// The lockout metrics (#9674) key on `OpenPr` rows: a pre-filtered candidate
/// produces one on every tick it is blocked, never an `OpenPrBackoff` row in
/// between (no backoff is armed), so the repo's lock is not read as cleared.
#[test]
fn a_prefiltered_candidate_is_an_open_pr_row_on_every_tick() {
    use crate::observability::ops::lockout::frozen_by_repo;
    for tick in 0..3 {
        let mut multi = vec![workspace(
            &[1, 2],
            Disp::new(&[(1, 901), (2, 902)], Listing::Readable),
        )];
        let report = tick_multi(&mut multi, &[1], 5, &[false]);
        let queue = rows(&report);
        let frozen = frozen_by_repo(queue.iter().map(|r| (r.repo.as_str(), r.disposition, None)));
        assert_eq!(frozen["/repo/a"].candidates, 2, "tick {tick}");
        assert_eq!(report.skipped_pr_open, 2, "the pr-open-skip tick counter, tick {tick}");
        assert_eq!(report.skipped_pr_open_backoff, 0, "tick {tick}");
    }
}

fn skip_lines(root: &Path, blocked: &[(u32, u32)]) -> Vec<String> {
    let blocked: HashMap<u32, u32> = blocked.iter().copied().collect();
    crate::test_log_capture::capture_logs(|| log_changes(root, &blocked))
        .into_iter()
        .filter(|(level, _)| *level == log::Level::Info)
        .map(|(_, msg)| msg)
        .collect()
}

/// The skip line is logged when an issue becomes blocked (or its PR changes),
/// not on every tick it stays blocked.
#[test]
fn the_skip_line_is_logged_once_per_state_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let first = skip_lines(root, &[(8136, 8131), (8200, 8201)]);
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(first[0].contains("skipping issue #8136"), "{first:?}");
    assert!(first[0].contains("open linked PR #8131"), "{first:?}");
    assert!(first[0].contains("#4123 open-PR guard"), "{first:?}");

    assert!(
        skip_lines(root, &[(8136, 8131), (8200, 8201)]).is_empty(),
        "steady state is quiet"
    );

    // #8200 gets a different PR; #8136 is unchanged.
    let changed = skip_lines(root, &[(8136, 8131), (8200, 8300)]);
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].contains("#8200") && changed[0].contains("#8300"), "{changed:?}");

    // #8136 is freed, then blocked again: that is a new state.
    assert!(skip_lines(root, &[(8200, 8300)]).is_empty());
    assert_eq!(skip_lines(root, &[(8136, 8131), (8200, 8300)]).len(), 1);

    // Another workspace keeps its own state.
    let other = tempfile::tempdir().unwrap();
    assert_eq!(skip_lines(other.path(), &[(8136, 8131)]).len(), 1);
}

/// A source whose listing returned only part of its queue (#11139).
struct PartialSource(Option<Vec<WorkItem>>);

impl WorkSource for PartialSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
    fn listing_complete(&self) -> bool {
        false
    }
}

/// #12025: an unreadable listing fails the workspace closed. No candidate is
/// offered to `dispatch()` (whose guard would probe each one), none is
/// dispatched, and each counts one `forge_listing` error, its row naming why.
#[test]
fn an_unreadable_listing_fails_closed() {
    let blocked: Vec<(u32, u32)> = (1..=4).map(|n| (n, 900 + n)).collect();
    let mut multi = vec![workspace(
        &[1, 2, 3, 4, 5],
        Disp::new(&blocked, Listing::Failed),
    )];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);

    let disp = &multi[0].1;
    assert!(disp.attempts.is_empty(), "no dispatch(), so no per-issue probe");
    assert_eq!(disp.asked.borrow().len(), 1, "asked once, not once per candidate");
    assert_eq!(disp.backoffs_armed.get(), 0);
    assert_eq!((report.dispatched, report.skipped_pr_open, report.errors), (0, 0, 5));
    assert_eq!(report.error_kind(), Some("forge_listing"));
    assert!(report.admissions.is_empty());
    for (issue, disposition, detail) in dispositions(&report) {
        assert_eq!(disposition, QueueDisposition::DispatchError, "issue #{issue}");
        assert_eq!(detail.as_deref(), Some(open_pr_prefilter::FAIL_CLOSED_DETAIL));
    }
}

/// #12025: the hold is per workspace. A healthy sibling dispatches as if the
/// failed one were not there.
#[test]
fn a_failed_listing_holds_only_its_own_workspace() {
    let mut multi = vec![
        workspace(&[1, 2, 3], Disp::new(&[(1, 901)], Listing::Failed)),
        workspace(&[7, 8, 9], Disp::new(&[(8, 908)], Listing::Verified)),
    ];
    let report = tick_multi(&mut multi, &[1, 1], 10, &[false, false]);
    assert!(multi[0].1.attempts.is_empty());
    assert_eq!(multi[1].1.attempts, vec![7, 9], "8 is pre-filtered, 7 and 9 dispatched");
    assert_eq!((report.dispatched, report.pr_open_prefiltered, report.errors), (2, 1, 3));
    assert_eq!(report.error_kind(), Some("forge_listing"));
}

/// #12025: a healthy, verified listing behaves exactly as the pre-#12025
/// pre-filter did, field for field.
#[test]
fn a_verified_listing_behaves_as_before() {
    let blocked: Vec<(u32, u32)> = (1..=3).map(|n| (n, 900 + n)).collect();
    for cap in [1, 2, 5] {
        let mut now = vec![workspace(
            &[1, 2, 3, 4, 5],
            Disp::new(&blocked, Listing::Verified),
        )];
        let mut then = vec![workspace(
            &[1, 2, 3, 4, 5],
            Disp::new(&blocked, Listing::Readable),
        )];
        let a = tick_multi(&mut now, &[1], cap, &[false]);
        let b = tick_multi(&mut then, &[1], cap, &[false]);
        assert_eq!(a, b, "cap {cap}");
        assert_eq!(now[0].1.attempts, then[0].1.attempts, "cap {cap}");
        assert_eq!(a.errors, 0);
    }
}

/// #12025: with the ready-issue listing incomplete, a candidate the open-PR
/// listing gave no verdict for is held; verified ones go on as before. With
/// the ready listing whole, the same unverified candidate is offered to
/// `dispatch()` as before.
#[test]
fn an_incomplete_ready_listing_holds_only_unverified_candidates() {
    let mut disp = Disp::new(&[(1, 901)], Listing::Verified);
    disp.no_verdict.insert(2);
    let mut multi = vec![(PartialSource(Some(items(&[1, 2, 3]))), disp)];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);
    assert_eq!(multi[0].1.attempts, vec![3], "1 is pre-filtered, 2 is held, 3 is dispatched");
    assert_eq!((report.dispatched, report.pr_open_prefiltered, report.errors), (1, 1, 1));
    assert_eq!(report.error_kind(), Some("forge_listing"));
    assert_eq!(report.listing_incomplete, vec![0]);

    let mut disp = Disp::new(&[(1, 901)], Listing::Verified);
    disp.no_verdict.insert(2);
    let mut multi = vec![workspace(&[1, 2, 3], disp)];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);
    assert_eq!(multi[0].1.attempts, vec![2, 3]);
    assert_eq!(report.errors, 0);
}

/// #12025: a dispatcher that consults no listing (`NotAsked`, the trait
/// default) never fails closed, even on an incomplete ready listing.
#[test]
fn no_listing_consulted_never_fails_closed() {
    let disp = Disp::new(&[(1, 901)], Listing::NoPrefilter);
    let mut multi = vec![(PartialSource(Some(items(&[1, 2]))), disp)];
    let report = tick_multi(&mut multi, &[1], 5, &[false]);
    assert_eq!(multi[0].1.attempts, vec![1, 2]);
    assert_eq!((report.dispatched, report.skipped_pr_open, report.errors), (1, 1, 0));
}
