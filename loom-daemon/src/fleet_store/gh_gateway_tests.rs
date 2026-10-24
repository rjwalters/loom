//! 2am#4367 P4 break-glass: the `fleet.json` fetch never depends on the
//! forge gateway, so a broken gateway can always be unset from the store.
#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn every_store_request_is_exempt_from_the_gateway() {
    let tmp = tempfile::tempdir().unwrap();
    let t = GhTransport::new(tmp.path(), "acme/fleet-state");
    let dir = tmp.path().join("cfg");
    for cred in [
        Credential::Ambient,
        Credential::ConfigDir {
            dir: dir.clone(),
            label: "writer app".into(),
        },
    ] {
        for intent in [AccessIntent::Read, AccessIntent::Write] {
            let inv = t.invocation("fleet_store", ops::GIT_READ_OBJECTS, intent, &cred);
            assert!(inv.is_gateway_exempt(), "{cred:?} {intent:?}");
        }
    }
}

/// The exempt router is the reader pool alone, even while a gateway profile
/// is live for the store's owner.
#[test]
fn the_exempt_router_never_answers_with_the_gateway() {
    use crate::forge_bucket_book::Resource;
    use crate::forge_identity::RouteRequest;
    let tmp = tempfile::tempdir().unwrap();
    let live = |_: &str| Some(tmp.path().to_path_buf());
    let req = RouteRequest::gate("gw-fleet-store/fleet-state", None, Resource::Core);
    let now = SystemTime::now();
    // The gateway would serve it ...
    assert!(crate::forge_gateway::decide(&req, now, &live).is_some());
    // ... but the exempt router never consults it: outside a daemon the
    // reader pool has no workspace, so the answer is the writer.
    let routed = crate::gh_invocation::read_router(true)(&req, now);
    assert_eq!(routed, crate::forge_identity::RouteDecision::NoPool);
}
