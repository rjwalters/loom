//! Coverage for Issue #9928: the step 2.75 no-op-cooldown dispatch guard must
//! honour a **peer** host's fleet-broadcast window (#7477), not only this
//! host's own local one.
//!
//! `record_noop_release` has broadcast the window it arms since #7477, and the
//! work-finder pre-filter has read the fleet-unioned
//! `SweepRegistry::noop_cooldown_issues` ever since. The guard #6917 added to
//! cover every OTHER dispatch route — the IPC/CLI `{"Issue": <N>}` RPC behind
//! `loom-daemon dispatch <N>` / `mcp__loom__dispatch_sweep` / a
//! `--claim-owned <N>` re-dispatch, the epic supervisor, all three watchdogs —
//! read the HOST-LOCAL `noop_cooldown_remaining` instead, so a window armed by
//! host A was invisible to a direct dispatch on hosts B/C/D. See
//! `SweepRegistry::noop_cooldown_dispatch_block`'s doc comment for the incident
//! shape (`2AMLogic/gf180-pll#127`).
//!
//! Sibling file rather than an inline `mod` on `dispatch/tests.rs`: that file
//! is over the file-size ratchet threshold (`scripts/file-size-baseline.txt`),
//! so new coverage is declared directly from `dispatch.rs` instead of growing
//! it further — the same pattern `idempotency_inflight_tests.rs` uses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::peer_claims::{ClaimAd, PeerClaimView};
use crate::sweep_registry::test_support::*;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tempfile::tempdir;

/// Attach a peer-claim view to `registry` carrying a `remaining_secs`-long
/// no-op cooldown that a PEER host armed for `issue` — nothing recorded
/// locally.
fn attach_peer_armed_cooldown(registry: &mut SweepRegistry, issue: u32, remaining_secs: u64) {
    let repo = crate::peer_claims::repo_slug(&registry.config.workspace_root);
    let view = Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
    {
        let mut v = view.lock().unwrap();
        v.observe_noop_cooldown_at(
            &ClaimAd::noop_cooldown_armed(
                issue,
                repo,
                "peer-host".into(),
                4242,
                "ts".into(),
                remaining_secs,
            ),
            Instant::now(),
        );
    }
    registry.set_peer_claims(view);
}

/// THE #9928 regression: a direct `{"Issue": <N>}` dispatch must be refused
/// while a peer host's fleet-broadcast no-op cooldown is live, with no local
/// record of its own.
///
/// Pre-fix this dispatch proceeded (`was_new: true`): host A's sweep
/// self-reported "no actionable delta", armed + broadcast the window, and
/// hosts B/C/D re-claimed the same issue inside it — four hosts re-dispatching
/// one unchanged tracker issue in ~10 minutes, two of those passes landing
/// after the no-op release. Mirrors
/// `tests::dispatch_refused_while_noop_cooldown_is_live` (the local-window
/// half of the same guard), including its pre-flip contract: no claim lock, no
/// registered sweep.
#[test]
fn dispatch_refused_while_a_peer_armed_noop_cooldown_is_live() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    attach_peer_armed_cooldown(&mut registry, 9928, 3600);

    assert_eq!(
        registry.noop_release_count(9928),
        0,
        "no local record — the window exists only on the peer's broadcast"
    );

    let err = registry
        .dispatch(&SweepKind::Issue(9928), None, None, None, None)
        .expect_err("a peer-armed no-op cooldown must refuse a direct dispatch too");
    let typed = err
        .downcast_ref::<NoopCooldownDispatchError>()
        .expect("refusal must carry the typed NoopCooldownDispatchError");
    assert_eq!(typed.issue, 9928);
    assert!(typed.retry_after_secs > 0);

    assert!(
        !registry.config.locks_dir().join("issue-9928").exists(),
        "a peer-armed-cooldown refusal must not acquire the claim lock"
    );
    assert!(registry.entries.is_empty(), "a noop-cooldown refusal must not register a sweep");
}

/// The guard on the fix: an EXPIRED (or malformed, `remaining_secs: 0`) peer
/// ad must not hold the issue back at all — the guard reads `expiry > now`,
/// never "a peer mentioned this issue once". Mirrors
/// `tests::dispatch_proceeds_once_noop_cooldown_expires` for the local half.
#[test]
fn dispatch_proceeds_when_the_peer_armed_window_has_lapsed() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    attach_peer_armed_cooldown(&mut registry, 9931, 0);

    assert!(registry
        .noop_cooldown_dispatch_block(9931, chrono::Utc::now())
        .is_none());
    let outcome = registry
        .dispatch(&SweepKind::Issue(9931), None, None, None, None)
        .expect("a lapsed peer window must not refuse dispatch");
    assert!(outcome.was_new);
}

/// A disabled mechanism (`autonomous.workFinder.noopCooldown.enabled: false`)
/// must ignore a peer-armed window on this route too — the dispatch path's
/// mirror of `noop_cooldown::tests::disabled_mechanism_ignores_peer_armed_window`.
#[test]
fn disabled_mechanism_never_refuses_on_a_peer_armed_window() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    registry.set_noop_cooldown_config(crate::sweep_registry::NoopCooldownConfig {
        enabled: false,
        cooldown: Duration::from_secs(60),
        ..crate::sweep_registry::NoopCooldownConfig::default()
    });
    attach_peer_armed_cooldown(&mut registry, 9932, 3600);

    let outcome = registry
        .dispatch(&SweepKind::Issue(9932), None, None, None, None)
        .expect("a disabled cooldown mechanism must never refuse dispatch");
    assert!(outcome.was_new);
}
