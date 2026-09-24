//! [`PeerClaimSink`]'s `ClaimKind::Heartbeat` routing (Issue #8736), kept out
//! of `tests.rs` because that file is at `scripts/file-size-baseline.txt`'s
//! ratchet — same reason `coordination_tests.rs` lives beside `peer_claims.rs`
//! rather than inside it.

use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

/// A `Heartbeat` ad must route to its own bookkeeping
/// (`PeerClaimView::observe_heartbeat_at`) rather than `observe_at`'s
/// dispatch-claims map — mirroring
/// `tests::peer_claim_sink_reads_envelope_body_from_safehoused_push_shape`
/// but for the heartbeat lane.
#[test]
fn peer_claim_sink_routes_heartbeat_ads_to_their_own_bookkeeping() {
    let ad = ClaimAd::heartbeat("loom".into(), "peer-host".into(), 7, "ts".into());
    let push = json!({
        "event": "message",
        "envelope": {
            "v": 1,
            "from": "loom_daemon",
            "to": "*",
            "type": "task",
            "task_id": "0",
            "body": ad.to_body_json(),
        },
    });

    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
    let sink = PeerClaimSink::new(view.clone());
    sink.on_event(&push);

    let mut v = view.lock().unwrap();
    assert!(v.is_empty(), "a heartbeat must never fold into the dispatch-claims map");
    let eval = v.evaluate_coordination(Instant::now(), Duration::from_secs(1), 1);
    assert!(
        eval.reason.contains("nothing to judge"),
        "a heartbeat with no prior advertisement must not manufacture one: {eval:?}"
    );
}
