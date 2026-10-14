//! Issue #11444: the IPC `DispatchSweep` request carries who is asking.
//! Operator surfaces (the `loom-daemon dispatch` CLI, the `dispatch_sweep`
//! MCP tool) send `operator`; a request without the field
//! (a legacy client, or any IPC client that does not say) records `unknown`,
//! never `operator`. In its own sibling module because `ipc/tests.rs` is over
//! the file-size ratchet (`.loom/docs/file-size-policy.md`).

use super::*;
use crate::telemetry::DispatchOrigin;

/// The `dispatch_origin` start fact of the one sweep `sr` holds for `issue`.
fn recorded_origin(sr: &Arc<Mutex<SweepRegistry>>, issue: u32) -> Option<String> {
    let sr = sr.lock().unwrap();
    let sweep_id = sr
        .list(None)
        .into_iter()
        .find(|info| info.kind == SweepKind::Issue(issue))
        .map(|info| info.sweep_id)
        .expect("the dispatched sweep");
    sr.start_facts_snapshot()
        .get(&sweep_id)
        .and_then(|facts| facts.dispatch_origin.clone())
}

fn dispatch_request(issue: u32, dispatch_origin: Option<DispatchOrigin>) -> Request {
    Request::DispatchSweep {
        kind: SweepKind::Issue(issue),
        idempotency_key: None,
        model: None,
        effort: None,
        depends_on: None,
        workspace_root: None,
        force: false,
        runtime: None,
        model_profile: None,
        dispatch_origin,
    }
}

/// A request that names `operator` records it; one that names nothing
/// records `unknown`.
#[test]
#[serial_test::serial]
fn ipc_dispatch_records_the_requested_origin_or_unknown() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let _registry_guard = seed_temp_registry(&[]);

    for (issue, origin) in [
        (1_144_470, Some(DispatchOrigin::Operator)),
        (1_144_471, None),
    ] {
        match handle_request(dispatch_request(issue, origin), &tm, &db, &sr, &bus, &test_pool()) {
            Response::SweepDispatched { .. } => {}
            other => panic!("expected SweepDispatched, got {other:?}"),
        }
    }
    assert_eq!(recorded_origin(&sr, 1_144_470).as_deref(), Some("operator"));
    assert_eq!(recorded_origin(&sr, 1_144_471).as_deref(), Some("unknown"));
}

/// The non-blocking handler (the one the socket loop runs) records the
/// origin it is handed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn nonblocking_ipc_dispatch_records_the_operator_origin() {
    let (_, _, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let _registry_guard = seed_temp_registry(&[]);

    let response = dispatch_sweep_nonblocking(
        &sr,
        &test_pool(),
        &bus,
        SweepKind::Issue(1_144_472),
        None,
        None,
        None,
        None,
        None,
        false,
        None,
        None,
        DispatchOrigin::from_request(Some(DispatchOrigin::Operator)),
    )
    .await;
    assert!(matches!(response, Response::SweepDispatched { .. }), "{response:?}");
    assert_eq!(recorded_origin(&sr, 1_144_472).as_deref(), Some("operator"));
}

/// Wire compatibility: a pre-#11444 payload (no `dispatch_origin`) still
/// parses and reads as `unknown`; an origin this daemon does not know reads
/// as `unknown` rather than refusing the request; an absent origin is not
/// serialized, so a new client talking to an old daemon sends the old shape.
#[test]
fn dispatch_origin_is_optional_on_the_wire() {
    let legacy = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null}}"#;
    let Request::DispatchSweep {
        dispatch_origin, ..
    } = serde_json::from_str(legacy).expect("a pre-#11444 payload must parse")
    else {
        panic!("expected DispatchSweep");
    };
    assert_eq!(dispatch_origin, None);
    assert_eq!(DispatchOrigin::from_request(dispatch_origin), DispatchOrigin::Unknown);

    let newer = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"dispatch_origin":"some_future_origin"}}"#;
    let Request::DispatchSweep {
        dispatch_origin, ..
    } = serde_json::from_str(newer).expect("an unknown origin must not refuse the request")
    else {
        panic!("expected DispatchSweep");
    };
    assert_eq!(dispatch_origin, Some(DispatchOrigin::Unknown));

    let operator = serde_json::to_value(dispatch_request(42, Some(DispatchOrigin::Operator)))
        .expect("serialize");
    assert_eq!(operator["payload"]["dispatch_origin"], "operator");
    let absent = serde_json::to_value(dispatch_request(42, None)).expect("serialize");
    assert!(absent["payload"].get("dispatch_origin").is_none(), "{absent}");
}
