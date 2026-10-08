//! The gating-on path through [`gated_list`], with a real feed status cell and
//! real `apply_page` ingestion (#9255): what the pure `PollGate` tests cannot
//! see — the feed-state read, repo resolution, error handling and ingestion
//! ordering.
use std::sync::Arc;

use serial_test::serial;

use super::*;
use crate::forge_events::{
    global_state, register_global_status, EventBus, FeedClient, FeedErrorClass, FeedPage,
    FeedPaths, FeedStatus, ResolvedFeed, DEFAULT_POLL_INTERVAL_SECS,
};
use crate::work_finder::WorkItem;

const REPO: &str = "o/a";
const BASE: Duration = Duration::from_secs(60);

fn listing(numbers: &[u32]) -> WorkItems {
    numbers
        .iter()
        .map(|n| WorkItem::with_created_at(*n, vec!["loom:issue".to_string()], None))
        .collect()
}

/// One `gated_list` read for [`REPO`]; returns the listing and whether the
/// forge closure ran.
fn read(numbers: &[u32]) -> (WorkItems, bool) {
    let mut polled = false;
    let out: Result<_, ()> = gated_list(None, Some(REPO), || {
        polled = true;
        Ok(listing(numbers))
    });
    (out.expect("listing"), polled)
}

fn page(cursor: u64, clamped: bool, events: &str) -> FeedPage {
    serde_json::from_str(&format!(
        r#"{{"host_id":"mac-studio","cursor":{cursor},"clamped":{clamped},"events":[{events}],"has_more":false}}"#
    ))
    .expect("page")
}

/// The only test that registers the process-global feed status (a `OnceLock`),
/// so the unregistered phase below is deterministic. Phases run in order.
#[test]
#[serial]
fn gated_list_follows_the_feed_state_events_and_clamped_pages() {
    configure(true, BASE);
    let numbers = [1, 2];

    // Unregistered feed ⇒ `Disabled`: every read polls the forge.
    assert_eq!(global_state(), ForgeEventsState::Disabled);
    assert!(read(&numbers).1);
    assert!(read(&numbers).1);

    let dir = tempfile::tempdir().expect("tempdir");
    let feed = ResolvedFeed {
        endpoint: "https://events.internal".to_string(),
        host_id: "mac-studio".to_string(),
        key_file: dir.path().join("key"),
        poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
        page_size: 100,
        paths: FeedPaths::in_dir(dir.path().to_path_buf()),
    };
    let status = Arc::new(FeedStatus::started(&feed, 0));
    let mut client = FeedClient::new(feed, EventBus::new(), status.clone()).expect("client");
    register_global_status(status.clone());

    // `Connecting` is not `healthy`: still polls, never serves a held listing.
    assert!(read(&numbers).1);
    assert!(read(&numbers).1);

    // Healthy and quiet: the held listing is served with no forge call.
    client.apply_page(page(1, false, ""));
    assert_eq!(global_state(), ForgeEventsState::Healthy);
    let (held, polled) = read(&[9]);
    assert!(!polled);
    assert_eq!(held.iter().map(|i| i.number).collect::<Vec<_>>(), [1, 2]);

    // An event for another repo leaves the hold in place.
    client.apply_page(page(2, false, r#"{"repo":"o/other"}"#));
    assert!(!read(&[9]).1);

    // An event for this repo, ingested by `apply_page` before the status reads
    // `healthy`, forces exactly one re-poll.
    client.apply_page(page(3, false, r#"{"repo":"O/A"}"#));
    assert_eq!(global_state(), ForgeEventsState::Healthy);
    let (fresh, polled) = read(&[7]);
    assert!(polled);
    assert_eq!(fresh.iter().map(|i| i.number).collect::<Vec<_>>(), [7]);
    assert!(!read(&[9]).1, "the event is consumed by the poll that followed it");

    // A failed listing records nothing: the pending event is not consumed, so
    // the next read polls again.
    client.apply_page(page(4, false, r#"{"repo":"o/a"}"#));
    let failed: Result<WorkItems, &str> = gated_list(None, Some(REPO), || Err("forge down"));
    assert_eq!(failed.unwrap_err(), "forge down");
    assert!(read(&[7]).1);

    // A clamped page (events may have been lost) invalidates the workspace.
    client.apply_page(page(5, true, ""));
    assert!(read(&[7]).1);
    assert!(!read(&[7]).1);

    // A degraded feed restores the base cadence on the very next read.
    status.record_failure(FeedErrorClass::AuthFailed, "attempt".to_string());
    assert_ne!(global_state(), ForgeEventsState::Healthy);
    assert!(read(&[7]).1);

    // With no resolvable repo (no override, and a root with no git remote) a workspace is never gated.
    client.apply_page(page(6, false, ""));
    assert_eq!(global_state(), ForgeEventsState::Healthy);
    for _ in 0..2 {
        let mut polled = false;
        let _: Result<_, ()> = gated_list(Some(dir.path()), None, || {
            polled = true;
            Ok(listing(&[3]))
        });
        assert!(polled);
    }

    configure(false, BASE);
}

/// `ingest_page(.., clamped = true)` alone invalidates every workspace, even
/// one whose repo no event named.
#[test]
#[serial]
fn a_clamped_page_invalidates_a_held_workspace_through_ingest_page() {
    let mut gate: PollGate<WorkItems> = PollGate::new(true, BASE);
    let t0 = Instant::now();
    let clock = gate.clock();
    gate.record_poll("/ws/a", REPO, PollReason::FirstPoll, clock, 0, vec![], t0);
    let decide =
        |g: &mut PollGate<WorkItems>| g.decide("/ws/a", Some(REPO), ReadKind::Discovery, true, t0);
    assert_eq!(decide(&mut gate), Decision::Skip);

    configure(true, BASE);
    let before = lock().clock();
    ingest_page(&[], true);
    assert_eq!(lock().clock(), before + 1);
    configure(false, BASE);

    gate.note_unattributable();
    assert_eq!(decide(&mut gate), Decision::Poll(PollReason::Event));
}
