//! Coverage for Issue #9572: `begin_prepared_issue_dispatch`'s in-flight
//! idempotency-key map (`SweepRegistry::inflight_idempotency`) must never be
//! wedged by an attempt that claims the lock and then fails before spawning
//! (or before `finish_issue_dispatch` records the entry). See that field's
//! doc comment in `sweep_registry/mod.rs` and step 3.05 in
//! `begin_prepared_issue_dispatch` for the full design.
//!
//! Sibling file rather than an inline `mod` on `dispatch/tests.rs`: that file
//! is over the file-size ratchet threshold (`scripts/file-size-baseline.txt`)
//! and already grew from this issue's own test rename, so new coverage is
//! declared directly from `dispatch.rs` instead of adding to it further.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use tempfile::tempdir;

/// AC (#9572): a `begin` attempt that claims the lock and then fails —
/// here, `spawn_child_process` itself errors because `spawn_bin` resolves to
/// nothing runnable, mirroring `dispatch_reverts_claim_lock_and_label_when_spawn_child_itself_fails`
/// — must release its in-flight idempotency key. A later dispatch reusing
/// the SAME key must reach the ordinary guard chain (and the same spawn
/// failure) again, not a wedged/wrong graceful hand-back to a sweep that
/// never actually spawned.
#[test]
#[serial]
fn spawn_failure_releases_the_inflight_idempotency_key() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, _gh_log) = spawn_bin_missing_registry(ws);

    let key = "spawn-failure-key-9572".to_string();

    let first = reg
        .dispatch(&SweepKind::Issue(9572), Some(key.clone()), None, None, None)
        .expect_err("first dispatch must fail — spawn_bin does not exist");
    assert!(first.to_string().contains("failed to spawn sweep child"), "got: {first}");
    assert!(
        reg.inflight_idempotency.is_empty(),
        "a begin attempt that errors out after claiming the lock must release its \
         in-flight idempotency key, not leave it recorded: {:?}",
        reg.inflight_idempotency
    );

    // The retry must reach the SAME spawn failure again — not a graceful
    // `Done(Ok(DispatchOutcome { was_new: false, .. }))` hand-back to a
    // sweep_id that never actually spawned (which would be the wedge this
    // test guards against), and not a #4556 live-claim refusal either (there
    // is nothing live to collide with — the first attempt never spawned).
    let second = reg
        .dispatch(&SweepKind::Issue(9572), Some(key), None, None, None)
        .expect_err(
            "second dispatch with the same key must also fail — spawn_bin still \
                     does not exist",
        );
    assert!(
        second.to_string().contains("failed to spawn sweep child"),
        "the retry must reach the same spawn failure again, not a stale idempotency hit \
         or a live-claim refusal — got: {second}"
    );
    assert!(
        reg.inflight_idempotency.is_empty(),
        "the second failed attempt must ALSO release the key: {:?}",
        reg.inflight_idempotency
    );
}

/// The `PreparedIssueDispatch`-consuming half of the same guarantee, at the
/// lower `begin_issue_dispatch`/`finish_issue_dispatch` layer this issue's
/// unlocked-poll-window fix operates at: a healthy attempt records the key
/// at claim time and clears it once `finish_issue_dispatch` records the
/// entry — the map must never accumulate a stale entry for a sweep that DID
/// complete normally.
#[test]
#[serial]
fn healthy_dispatch_clears_its_own_inflight_key_on_finish() {
    let dir = tempdir().unwrap();
    let script = "#!/usr/bin/env bash\nset -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent-ok' (mode=random)\" >&2\n";
    let mut registry = lifecycle_registry(dir.path(), script);

    let begin = registry
        .begin_issue_dispatch(
            &SweepKind::Issue(90_001),
            Some("healthy-key-9572".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    let mut prepared = match begin {
        BeginIssueDispatch::Spawned(prepared) => prepared,
        BeginIssueDispatch::Done(result) => panic!("expected Spawned, got Done({result:?})"),
    };
    assert!(
        registry
            .inflight_idempotency
            .contains_key("healthy-key-9572"),
        "the key must be recorded once the attempt claims the lock"
    );

    let (token_name, runtime, death) = poll_and_classify_spawned_child(
        &mut prepared.child,
        &prepared.log_path,
        &prepared.header_anchor,
    );
    let outcome = registry
        .finish_issue_dispatch(*prepared, token_name, runtime, death)
        .unwrap();
    assert!(
        !registry
            .inflight_idempotency
            .contains_key("healthy-key-9572"),
        "finish_issue_dispatch must clear the in-flight key once the entry is recorded"
    );

    let _ = registry.cancel(&outcome.sweep_id, Duration::from_millis(50));
}
