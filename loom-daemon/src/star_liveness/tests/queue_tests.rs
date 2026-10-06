//! #10214: a starred issue deferred on capacity reports where it stands and
//! what limits the cap, counts a draining queue as progress, and names the
//! deferral reason and queue position when it does escalate.

use std::path::Path;
use std::sync::Arc;

use super::fake::*;
use crate::star_liveness::queue;
use crate::types::{
    AskKind, CapLimiter, CapView, CapacityWait, LandingStage, QueueDisposition, ReadyQueueRow,
};

/// The 2026-10-04 cap: configured 6, disk headroom 2 (4 GB free at 2 GB per
/// worktree), plenty of RAM.
fn disk_cap() -> CapView {
    CapView {
        configured: 6,
        disk: Some(2),
        ram: Some(30),
    }
}

/// `count` starred, capacity-deferred rows for `root`, issues 1000.., ranks
/// 1.. in dispatch order.
fn starred_queue(root: &Path, count: u32) -> Vec<ReadyQueueRow> {
    (0..count)
        .map(|i| ReadyQueueRow {
            rank: (i + 1) as usize,
            ..tick_row(root, 1000 + i, QueueDisposition::DeferredCapacity)
        })
        .collect()
}

/// A host whose last tick queued `ahead` older stars before issue 5, then
/// `behind` newer ones.
fn queued(slug: &str, ahead: u32, behind: u32) -> super::super::task::RepoInput {
    let mut r = repo_input(slug);
    let mut rows = starred_queue(&r.root, ahead);
    rows.push(ReadyQueueRow {
        rank: (ahead + 1) as usize,
        ..tick_row(&r.root, 5, QueueDisposition::DeferredCapacity)
    });
    rows.extend(starred_queue(&r.root, behind).into_iter().map(|mut row| {
        row.issue += 500;
        row.rank += (ahead + 1) as usize;
        row
    }));
    r.tick_rows = rows.clone();
    r.host_queue = Arc::new(rows);
    r.cap = Some(disk_cap());
    r
}

#[test]
fn cap_view_names_the_binding_term() {
    assert_eq!(disk_cap().effective(), 2);
    assert_eq!(disk_cap().limiter(), CapLimiter::Disk);
    assert!(disk_cap().resource_limited());
    let ram = CapView {
        configured: 6,
        disk: Some(9),
        ram: Some(3),
    };
    assert_eq!(ram.limiter(), CapLimiter::Ram);
    let configured = CapView::from_terms(4, usize::MAX, 10);
    assert_eq!(configured.disk, None, "an unmeasured term is not a clamp");
    assert_eq!(configured.limiter(), CapLimiter::Configured);
    assert!(!configured.resource_limited());
    assert_eq!(configured.effective(), 4);
}

#[test]
fn position_is_host_wide_among_waiting_stars() {
    let root = Path::new("/r");
    let mut rows = starred_queue(root, 3);
    // An unstarred deferral and a running star do not count.
    rows.push(ReadyQueueRow {
        rank: 4,
        operator_priority: false,
        ..tick_row(root, 77, QueueDisposition::DeferredCapacity)
    });
    rows.push(ReadyQueueRow {
        rank: 0,
        ..tick_row(root, 78, QueueDisposition::InFlight)
    });
    assert_eq!(queue::position(&rows, &rows[2]), (3, 3));
    let w = queue::wait(&rows[1], &rows, &[], Some(disk_cap())).unwrap();
    assert_eq!(w.summary(), "queued #2 of 3 (cap 2, disk-limited)");
    // A host refusal is not a queue: no position.
    let host = tick_row(root, 9, QueueDisposition::HostConstraint);
    let w = queue::wait(&host, &rows, &[], None).unwrap();
    assert!(!w.queued());
    assert_eq!(w.position, None);
    assert!(w.summary().starts_with("not for this host"));
}

#[test]
fn a_new_star_behind_a_disk_limited_cap_reads_queued_and_posts_nothing_while_it_drains() {
    let world = World::default();
    let slug = "o/backlog";
    world.add(slug, issue(5, &[STAR, "loom:issue"]));
    let mut host = Host::new("worker-1");

    // 87 older stars ahead, 18 behind: #88 of 106.
    let r = host.pass(&world, &[queued(slug, 87, 18)], Vec::new(), t(6, 0));
    let row = &r.rows[0];
    assert_eq!(row.stage, LandingStage::NoCapacity);
    assert_eq!(row.no_capacity.as_deref(), Some("queued #88 of 106 (cap 2, disk-limited)"));
    let wait = row.capacity_wait.as_ref().unwrap();
    assert_eq!((wait.position, wait.total), (Some(88), Some(106)));
    assert_eq!(wait.limiter, Some(CapLimiter::Disk));
    assert_eq!((wait.cap, wait.configured_cap), (Some(2), Some(6)));

    // The queue drains slowly, one older star dispatched every 20 min, for
    // two hours: well past the 30 min window, and never a stall.
    for step in 1..=6u32 {
        let r = host.pass(
            &world,
            &[queued(slug, 87 - step, 18)],
            Vec::new(),
            t(6, 0) + chrono::Duration::minutes(i64::from(step) * 20),
        );
        assert!(r.rows[0].ask.is_none(), "step {step}: a draining queue is progress");
        assert_eq!(
            r.rows[0].no_capacity.as_deref(),
            Some(format!("queued #{} of {} (cap 2, disk-limited)", 88 - step, 106 - step).as_str())
        );
    }
    assert!(world.posted(slug).is_empty(), "no no-progress comment while the queue drains");
}

#[test]
fn a_queue_that_stops_moving_escalates_once_naming_reason_and_position() {
    let world = World::default();
    let slug = "o/stuck";
    world.add(slug, issue(5, &[STAR, "loom:issue"]));
    let mut host = Host::new("worker-1");
    let input = [queued(slug, 87, 18)];
    host.pass(&world, &input, Vec::new(), t(6, 0));
    assert!(host.pass(&world, &input, Vec::new(), t(6, 29)).rows[0]
        .ask
        .is_none());

    let r = host.pass(&world, &input, Vec::new(), t(6, 31));
    let ask = r.rows[0]
        .ask
        .as_ref()
        .expect("a queue that has not moved for the window escalates");
    assert_eq!(ask.kind, AskKind::NoProgress);
    assert!(ask.text.contains("queued #88 of 106 (cap 2, disk-limited)"), "{}", ask.text);
    assert!(ask.text.contains("deferred: disk-limited"), "{}", ask.text);
    assert!(ask.text.contains("below the configured 6"), "{}", ask.text);
    assert!(ask.text.contains("free disk"), "{}", ask.text);
    assert!(!ask.text.contains("Check why the work-finder"), "{}", ask.text);

    host.pass(&world, &input, Vec::new(), t(7, 0));
    assert_eq!(world.posted(slug).len(), 1, "one comment for one stall");
    let (_, body) = &world.posted(slug)[0];
    assert!(body.contains("queued #88 of 106 (cap 2, disk-limited)"), "{body}");
}

#[test]
fn the_wait_round_trips_and_an_old_row_has_none() {
    let w = CapacityWait {
        gate: "capacity".into(),
        limiter: Some(CapLimiter::Ram),
        position: Some(3),
        total: Some(9),
        cap: Some(1),
        configured_cap: Some(4),
    };
    let back: CapacityWait = serde_json::from_value(serde_json::to_value(&w).unwrap()).unwrap();
    assert_eq!(back, w);
    assert_eq!(w.summary(), "queued #3 of 9 (cap 1, ram-limited)");
    let row: crate::types::StarLandingRow = serde_json::from_value(serde_json::json!({
        "repo": "o/r", "issue": 1, "stage": "no-capacity", "next_actor": "work-finder",
        "no_capacity": "waiting: concurrency cap full"
    }))
    .unwrap();
    assert_eq!(row.capacity_wait, None);
}
