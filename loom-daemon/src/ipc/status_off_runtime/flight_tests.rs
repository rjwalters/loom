//! Tests for the single-flight status build (Issue #10861).
//!
//! These drive [`single_flight`] with injected builds — counting, gated and
//! panicking closures — so no registry or socket is involved. A waiter is a
//! `tokio_test` task polled by hand: one `Pending` poll proves the request
//! has entered its flight, which makes "N requests are waiting on one build"
//! a fact rather than a sleep.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::status_section::StatusSection;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio_test::{assert_pending, task};

const WAIT: Duration = Duration::from_secs(20);

/// An injected build. Boxed so builds of different shapes are one type.
type Build = Box<dyn FnOnce() -> DaemonStatusReport + Send>;

/// A build's test double: counts its runs, and holds each run until released.
#[derive(Clone, Default)]
struct Gate {
    runs: Arc<AtomicUsize>,
    open: Arc<AtomicBool>,
}

impl Gate {
    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    fn enter(&self) {
        self.runs.fetch_add(1, Ordering::SeqCst);
        while !self.open.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A build that reports `configured_max = marker` once released.
    fn build(&self, marker: usize) -> Build {
        let gate = self.clone();
        Box::new(move || {
            gate.enter();
            DaemonStatusReport {
                configured_max: marker,
                ..DaemonStatusReport::default()
            }
        })
    }

    /// A build that panics with `cause` once released.
    fn panicking(&self, cause: &'static str) -> Build {
        let gate = self.clone();
        Box::new(move || {
            gate.enter();
            panic!("{cause}")
        })
    }
}

fn flights() -> Arc<StatusFlights> {
    Arc::new(StatusFlights::default())
}

fn started(flights: &StatusFlights) -> usize {
    flights.builds_started.load(Ordering::SeqCst)
}

fn in_flight(flights: &StatusFlights) -> usize {
    flights.inflight().len()
}

async fn landed<T>(waiter: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, waiter)
        .await
        .expect("a status waiter hung")
}

/// **AC.** 8 concurrent requests run the build once and all get its report —
/// the same report, not eight equal ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eight_concurrent_requests_run_one_build() {
    let flights = flights();
    let gate = Gate::default();
    let mut waiters: Vec<_> = (0..8)
        .map(|_| task::spawn(single_flight(&flights, SectionSet::all(), gate.build(7))))
        .collect();
    for waiter in &mut waiters {
        assert_pending!(waiter.poll());
    }
    assert_eq!(started(&flights), 1, "every later request joined the first build");

    gate.release();
    let mut reports = Vec::new();
    for waiter in waiters {
        reports.push(landed(waiter).await.expect("the build succeeded"));
    }
    assert_eq!(gate.runs(), 1, "the build closure ran more than once");
    assert!(reports.iter().all(|r| r.configured_max == 7));
    assert!(
        reports.iter().all(|r| Arc::ptr_eq(r, &reports[0])),
        "a request that arrived during the build must get that build's report"
    );
    assert_eq!(in_flight(&flights), 0, "the slot is cleared once the build lands");
}

/// **AC.** No reuse window: a request arriving after a build has completed
/// starts a new build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_after_a_finished_build_starts_a_new_one() {
    let flights = flights();
    let gate = Gate::default();
    gate.release();

    let first = single_flight(&flights, SectionSet::all(), gate.build(1)).await;
    // The slot is cleared before the report is published, so there is no
    // instant at which a finished build can still be joined.
    assert_eq!(in_flight(&flights), 0);
    let second = single_flight(&flights, SectionSet::all(), gate.build(2)).await;

    assert_eq!(gate.runs(), 2);
    assert_eq!(started(&flights), 2);
    assert_eq!(first.unwrap().configured_max, 1);
    assert_eq!(second.unwrap().configured_max, 2, "the second request got a fresh report");
}

/// **AC / #4279.** A panicking shared build gives every waiter an error
/// frame naming the cause, and does not poison the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_build_gives_every_waiter_the_cause() {
    let flights = flights();
    let gate = Gate::default();
    let drain = DrainState::new();
    let mut waiters: Vec<_> = (0..3)
        .map(|_| {
            task::spawn(single_flight(
                &flights,
                SectionSet::all(),
                gate.panicking("intentional shared status build panic"),
            ))
        })
        .collect();
    for waiter in &mut waiters {
        assert_pending!(waiter.poll());
    }

    gate.release();
    for waiter in waiters {
        match reply(landed(waiter).await, &drain) {
            Response::Error { message } => {
                assert!(message.contains("daemon failed to build status report"), "{message}");
                assert!(message.contains("intentional shared status build panic"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
    assert_eq!(gate.runs(), 1);
    assert_eq!(in_flight(&flights), 0, "a panicked flight left its slot behind");

    let next = single_flight(&flights, SectionSet::all(), gate.build(3)).await;
    assert_eq!(next.unwrap().configured_max, 3, "the request after a panic builds afresh");
}

/// **AC.** The build is detached from the request that started it: aborting
/// the leader's request mid-build leaves the other waiters their report, and
/// the slot is cleared for the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_leader_does_not_strand_the_waiters() {
    let flights = flights();
    let gate = Gate::default();
    let leader = tokio::spawn({
        let (flights, build) = (flights.clone(), gate.build(11));
        async move { single_flight(&flights, SectionSet::all(), build).await }
    });
    landed(async {
        while gate.runs() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;

    let mut waiters: Vec<_> = (0..3)
        .map(|_| task::spawn(single_flight(&flights, SectionSet::all(), gate.build(99))))
        .collect();
    for waiter in &mut waiters {
        assert_pending!(waiter.poll());
    }
    leader.abort();
    assert!(leader.await.unwrap_err().is_cancelled());
    assert_eq!(in_flight(&flights), 1, "the build outlives the request that started it");

    gate.release();
    for waiter in waiters {
        let report = landed(waiter)
            .await
            .expect("the leader's build still landed");
        assert_eq!(report.configured_max, 11, "the waiters get the aborted leader's build");
    }
    assert_eq!(gate.runs(), 1);

    let next = single_flight(&flights, SectionSet::all(), gate.build(12)).await;
    assert_eq!(next.unwrap().configured_max, 12);
    assert_eq!(gate.runs(), 2, "the slot was not cleared after the leader's build");
}

/// **AC / #4279.** A build task that ends without publishing (aborted before
/// it could send — what dropping its `Flight` is) gives every waiter an
/// error frame instead of a hang, and clears the slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_sender_gives_every_waiter_an_error_frame() {
    let flights = flights();
    let gate = Gate::default();
    let drain = DrainState::new();
    let (leader_slot, flight) = flights.enter(SectionSet::all());
    let flight = flight.expect("the first request leads");
    let mut waiters: Vec<_> = (0..3)
        .map(|_| task::spawn(single_flight(&flights, SectionSet::all(), gate.build(99))))
        .collect();
    for waiter in &mut waiters {
        assert_pending!(waiter.poll());
    }

    drop(flight);
    assert_eq!(in_flight(&flights), 0, "a dead flight must not stay joinable");
    for waiter in waiters {
        let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("a waiter hung on a dropped sender");
        match reply(outcome, &drain) {
            Response::Error { message } => {
                assert!(message.contains("daemon failed to build status report"), "{message}");
                assert!(message.contains(NO_OUTCOME), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
    let leader = tokio::time::timeout(Duration::from_secs(1), await_outcome(leader_slot))
        .await
        .expect("the leader hung on its own dropped sender");
    assert_eq!(leader.unwrap_err(), NO_OUTCOME);
    assert_eq!(gate.runs(), 0, "no waiter's own build ran");
}

/// A slot whose sender is dropped with no flight machinery at all — the
/// barest form of the case above.
#[tokio::test]
async fn a_bare_slot_with_a_dropped_sender_does_not_hang() {
    let (tx, slot) = watch::channel::<Option<Outcome>>(None);
    let waiters: Vec<_> = (0..3).map(|_| await_outcome(slot.clone())).collect();
    drop(tx);
    for waiter in waiters {
        let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("a waiter hung on a dropped sender");
        assert_eq!(outcome.unwrap_err(), NO_OUTCOME);
    }
}

/// A guard only ever clears its own flight's entry, never a newer flight
/// that took the same key.
#[tokio::test]
async fn a_stale_guard_leaves_a_newer_flight_in_place() {
    let flights = flights();
    let (_old_slot, old) = flights.enter(SectionSet::all());
    let old = old.unwrap();
    flights.inflight().remove(&SectionSet::all());
    let (new_slot, new) = flights.enter(SectionSet::all());
    let new = new.expect("the key was free again");

    drop(old);
    assert_eq!(in_flight(&flights), 1, "the old flight's guard removed the newer flight");
    let (joined, lead) = flights.enter(SectionSet::all());
    assert!(lead.is_none() && joined.same_channel(&new_slot));

    new.land(Ok(Arc::new(DaemonStatusReport::default())));
    assert_eq!(in_flight(&flights), 0);
}

/// **AC (#10787 interaction).** Flights are keyed by the normalized section
/// set: different sets build concurrently and never wait on each other,
/// identical sets coalesce, and a full request shares its key with an
/// all-sections request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flights_are_keyed_by_the_normalized_section_set() {
    let flights = flights();
    let (cheap_gate, heavy_gate) = (Gate::default(), Gate::default());
    let cheap = SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate]);
    let heavy = SectionSet::only([StatusSection::PerRepo]);

    let mut heavy_waiter = task::spawn(single_flight(&flights, heavy.clone(), heavy_gate.build(1)));
    assert_pending!(heavy_waiter.poll());
    let mut cheap_waiter = task::spawn(single_flight(&flights, cheap, cheap_gate.build(2)));
    assert_pending!(cheap_waiter.poll());
    assert_eq!(started(&flights), 2, "two different section sets are two builds");

    // The cheap build lands while the heavy one is still held.
    cheap_gate.release();
    assert_eq!(landed(cheap_waiter).await.unwrap().configured_max, 2);
    assert_pending!(heavy_waiter.poll());

    // The same set in another order, with a duplicate, joins the cheap key…
    let cheap_gate = Gate::default();
    let reordered = SectionSet::only([
        StatusSection::AutoUpdate,
        StatusSection::DaemonBuild,
        StatusSection::AutoUpdate,
    ]);
    let again = SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate]);
    let mut first = task::spawn(single_flight(&flights, again, cheap_gate.build(3)));
    assert_pending!(first.poll());
    let mut second = task::spawn(single_flight(&flights, reordered, cheap_gate.build(4)));
    assert_pending!(second.poll());
    assert_eq!(started(&flights), 3, "identical section sets coalesce into one build");
    // …and a second heavy request joins the heavy build still in flight.
    let mut heavy_joiner = task::spawn(single_flight(&flights, heavy, heavy_gate.build(5)));
    assert_pending!(heavy_joiner.poll());
    assert_eq!(started(&flights), 3);

    cheap_gate.release();
    heavy_gate.release();
    assert_eq!(landed(first).await.unwrap().configured_max, 3);
    assert_eq!(landed(second).await.unwrap().configured_max, 3);
    assert_eq!(landed(heavy_waiter).await.unwrap().configured_max, 1);
    assert_eq!(landed(heavy_joiner).await.unwrap().configured_max, 1);

    // A plain `DaemonStatus` and a request naming every section are one key.
    let full_gate = Gate::default();
    let every = SectionSet::only(StatusSection::all().iter().copied());
    let mut full = task::spawn(single_flight(&flights, SectionSet::all(), full_gate.build(6)));
    assert_pending!(full.poll());
    let mut named = task::spawn(single_flight(&flights, every, full_gate.build(7)));
    assert_pending!(named.poll());
    assert_eq!(started(&flights), 4, "an all-sections request shares the full build");
    full_gate.release();
    assert_eq!(landed(full).await.unwrap().configured_max, 6);
    assert_eq!(landed(named).await.unwrap().configured_max, 6);
    assert_eq!(full_gate.runs(), 1);
}

/// **AC.** The drain overlay is applied per request, after the shared build:
/// a drain begun while a build is in flight shows in every reply to that
/// build — including a request that joined after the drain — although the
/// build itself never saw it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_begun_mid_build_is_reported_by_requests_sharing_that_build() {
    let flights = flights();
    let gate = Gate::default();
    let drain = DrainState::new();
    let mut before = task::spawn(single_flight(&flights, SectionSet::all(), gate.build(1)));
    assert_pending!(before.poll());

    let deadline = match drain.begin(Duration::from_secs(300), false, false) {
        super::super::DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };
    let mut after = task::spawn(single_flight(&flights, SectionSet::all(), gate.build(2)));
    assert_pending!(after.poll());
    assert_eq!(started(&flights), 1, "the post-drain request joined the pre-drain build");

    gate.release();
    let shared = landed(before).await.unwrap();
    assert!(!shared.draining, "the shared build is drain-agnostic");
    for outcome in [Ok(shared), landed(after).await] {
        match reply(outcome, &drain) {
            Response::DaemonStatus(report) => {
                assert!(report.draining, "a reply after `drain` must report the drain");
                assert_eq!(report.drain_deadline, Some(deadline));
                assert!(report.drain_roll.is_some());
                assert_eq!(report.configured_max, 1);
            }
            other => panic!("expected DaemonStatus, got {other:?}"),
        }
    }
}

/// **AC.** `loom.daemon.ipc.status_builds` counts once per build, by outcome
/// — so the failure log line beside it is once per build too — however many
/// requests shared the build. On a current-thread runtime the detached build
/// task records on this thread, where the capture is listening.
#[test]
fn builds_are_counted_once_each_by_outcome() {
    use crate::observability::ops::{capture::capture, ipc_latency};
    use crate::telemetry::ops::{MetricName, MetricValue};

    let (points, _) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let flights = flights();
            let (ok, bad) = (Gate::default(), Gate::default());
            let mut shared: Vec<_> = (0..8)
                .map(|_| task::spawn(single_flight(&flights, SectionSet::all(), ok.build(1))))
                .collect();
            let failing = SectionSet::only([StatusSection::Drain]);
            let mut failed: Vec<_> = (0..3)
                .map(|_| {
                    task::spawn(single_flight(&flights, failing.clone(), bad.panicking("counted")))
                })
                .collect();
            for waiter in shared.iter_mut().chain(failed.iter_mut()) {
                assert_pending!(waiter.poll());
            }
            ok.release();
            bad.release();
            for waiter in shared {
                assert!(landed(waiter).await.is_ok());
            }
            for waiter in failed {
                assert!(landed(waiter).await.is_err());
            }
        });
        ipc_latency::drain_points()
    });
    let builds = |outcome: &str| {
        points
            .iter()
            .find(|p| {
                p.name == MetricName::DaemonIpcStatusBuilds
                    && p.labels.get("outcome").map(String::as_str) == Some(outcome)
            })
            .map(|p| p.value)
    };
    assert_eq!(builds("ok"), Some(MetricValue::Int(1)), "{points:?}");
    assert_eq!(builds("panic"), Some(MetricValue::Int(1)), "{points:?}");
    assert_eq!(builds("join_error"), None, "{points:?}");
}
