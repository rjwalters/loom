//! End-to-end dispatch regressions for Issue #8840: a racing dispatcher
//! must yield to a **renewed** in-session lease whose `created_at` has aged
//! past [`LEASE_ORDER_LOOKBACK_SECS`], and must still NOT yield to a merely
//! old one.
//!
//! [`super::lease_episode`]'s own unit tests pin the membership predicate in
//! isolation. These drive the whole `SweepRegistry::dispatch` path against
//! the shared fake-forge harness, so they also pin the properties the
//! predicate alone cannot express: that the yield happens **before** any
//! builder spawn or worktree access, that the winning claimant's
//! `loom:building` label is left intact, and that this host's own local
//! side effects (claim lock, peer-claim advertisement) are unwound.

use super::test_support::*;
use super::*;
use chrono::Utc;
use serial_test::serial;
use std::path::Path;
use tempfile::tempdir;

/// Seconds between the in-session lease's creation and the racing daemon
/// dispatch, recovered from the loom#8787 forge evidence (21:37:54Z ->
/// 21:42:16Z). 2.9× [`LEASE_ORDER_LOOKBACK_SECS`], so leg 1 of
/// [`super::lease_episode::in_claim_episode`] cannot be what admits it.
const INCIDENT_CREATED_SECS_AGO: i64 = 262;

/// Seconds between the in-session lease's last successful renewal and the
/// racing daemon dispatch (21:42:08Z -> 21:42:16Z). A maximally live lease.
const INCIDENT_RENEWED_SECS_AGO: i64 = 8;

/// Overwrite the harness's shared lease-comment store with a single
/// pre-existing record carrying an explicit `created_at`/`updated_at` pair.
///
/// The harness's own `&[&str]` seeding form always stamps both timestamps
/// at "now", which cannot express the one shape these tests need: a record
/// created long ago and renewed **in place** moments ago. Writing the store
/// directly keeps `test_support.rs` untouched (it is over the file-size
/// ratchet) while modeling exactly what the forge returned during the
/// incident.
///
/// `id` is 1, so this dispatcher's own lease write — which the fake `gh`
/// numbers as `existing_count + 1` — lands as id 2 and is therefore the
/// LATER claim, mirroring the peer daemon's position in the incident.
fn seed_renewed_lease(store: &Path, marker: &str, created_secs_ago: i64, updated_secs_ago: i64) {
    let now = Utc::now();
    let created = now - chrono::Duration::seconds(created_secs_ago);
    let updated = now - chrono::Duration::seconds(updated_secs_ago);
    let escaped = marker.replace('\\', "\\\\").replace('"', "\\\"");
    std::fs::write(
        store,
        format!(
            "{{\"id\":1,\"created_at\":\"{}\",\"updated_at\":\"{}\",\"body\":\"{escaped}\"}}\n",
            created.to_rfc3339(),
            updated.to_rfc3339(),
        ),
    )
    .unwrap();
}

/// **The #8840 regression.** An in-session sweep published its lease before
/// curation and has been renewing it ever since; a daemon on another host
/// then dispatches the same issue eight seconds after that lease's most
/// recent renewal.
///
/// Pre-#8840 the in-session record was excluded from the comparison for
/// having a `created_at` 262 seconds old, and the daemon proceeded —
/// producing the two live owners observed on loom#8787. It must now yield:
/// no builder spawned, no worktree touched, its own claim lock released,
/// and a `LeaseOrderDispatchError` naming the in-session host/sweep.
///
/// The winning claimant's `loom:building` label is deliberately NOT
/// reverted (loom#5270: reverting it destroys the winner's only cross-host
/// mutex), so the assertion here is that the flip was attempted and the
/// standdown is an annotation, not a label change.
#[test]
#[serial]
fn dispatch_yields_to_a_renewed_in_session_lease_created_before_the_lookback() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log, comments_store) =
        lease_order_dispatch_registry(dir.path(), &[]);
    seed_renewed_lease(
        &comments_store,
        "<!-- loom:lease host=host-d9142cf3 sweep=sweep-20260924T213744Z-76692-1b436e14 -->",
        INCIDENT_CREATED_SECS_AGO,
        INCIDENT_RENEWED_SECS_AGO,
    );

    let err = registry
        .dispatch(&SweepKind::Issue(8787), None, None, None, None)
        .expect_err("a live, freshly renewed in-session lease must refuse this dispatch");
    let lease_err = err
        .downcast_ref::<LeaseOrderDispatchError>()
        .unwrap_or_else(|| panic!("expected a LeaseOrderDispatchError, got: {err:#}"));
    assert_eq!(lease_err.issue, 8787);
    assert_eq!(lease_err.earliest_host, "host-d9142cf3");
    assert_eq!(lease_err.earliest_sweep_id, "sweep-20260924T213744Z-76692-1b436e14");

    assert_eq!(
        registry.len(),
        0,
        "no sweep entry may be recorded for a dispatch that lost to a live lease"
    );
    assert!(
        !spawn_log.exists(),
        "the yield must land BEFORE any builder spawn — this is the acceptance criterion the \
         incident violated"
    );
    assert!(
        !registry.config().locks_dir().join("issue-8787").exists(),
        "this host's own claim lock must be released when it stands down"
    );
    assert!(
        !dir.path().join(".loom/worktrees/issue-8787").exists(),
        "the yield must land before any worktree access"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("loom:lease-yield"),
        "a standdown annotation must record the yield; gh log: {gh_calls}"
    );
    assert!(
        !gh_calls.contains("--add-label loom:issue")
            && !gh_calls.contains("--remove-label loom:building"),
        "the winning claimant's loom:building label must be left intact (loom#5270); gh log: \
         {gh_calls}"
    );

    let stored = std::fs::read_to_string(&comments_store).unwrap_or_default();
    assert!(
        stored.contains("host-d9142cf3"),
        "the winning lease record must survive untouched: {stored}"
    );
}

/// The anti-wedge complement, and the property
/// [`LEASE_ORDER_LOOKBACK_SECS`] was introduced for: an OLD lease record
/// that was never renewed (a real forge sets `updated_at == created_at` at
/// creation) is historical noise from a finished claim round. Admitting it
/// would make every ordinary, uncontested dispatch lose to some long-dead
/// claim. Dispatch must proceed and spawn.
#[test]
#[serial]
fn dispatch_proceeds_past_an_old_never_renewed_lease() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log, store) = lease_order_dispatch_registry(dir.path(), &[]);
    seed_renewed_lease(
        &store,
        "<!-- loom:lease host=long-finished-host sweep=sweep-issue-8841-old -->",
        3600,
        3600,
    );

    let outcome = registry
        .dispatch(&SweepKind::Issue(8841), None, None, None, None)
        .expect("a stale, never-renewed lease record must not block a fresh dispatch");
    assert!(outcome.was_new);
    assert!(
        wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
        "an uncontested dispatch must still spawn its builder"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("loom:lease-yield"),
        "no standdown may be posted when nothing live was racing; gh log: {gh_calls}"
    );
}

/// A lease whose renewal loop STOPPED — the shape a finished or crashed
/// sweep leaves behind — must free the issue once its last renewal ages
/// past the reclamation TTL. Renewed 30 minutes ago against a 15-minute
/// default TTL: an abandoned claim can never wedge redispatch forever, so
/// the new membership leg cannot be turned into a denial-of-service by an
/// owner that simply died.
#[test]
#[serial]
fn dispatch_proceeds_past_a_lease_whose_last_renewal_aged_out_of_the_ttl() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log, store) = lease_order_dispatch_registry(dir.path(), &[]);
    seed_renewed_lease(
        &store,
        "<!-- loom:lease host=crashed-host sweep=sweep-issue-8842-abandoned -->",
        7200,
        1800,
    );

    let outcome = registry
        .dispatch(&SweepKind::Issue(8842), None, None, None, None)
        .expect("a lease whose renewal stopped long ago must not block redispatch");
    assert!(outcome.was_new);
    assert!(
        wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
        "redispatch after a lease genuinely aged out must spawn its builder"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("loom:lease-yield"),
        "an aged-out lease must not produce a standdown; gh log: {gh_calls}"
    );
}

/// A lease record already annotated as YIELDED by its own writer is still
/// just a lease record on the forge — but the standdown annotation carries
/// a `loom:lease-yield` marker, not the `loom:lease host=` prefix
/// `read_lease_comments` selects on, so it must never be mistaken for a
/// live claim no matter how recently it was written. Guards the "cover
/// stale/yielded leases" criterion against a marker-prefix regression.
#[test]
#[serial]
fn a_yield_annotation_is_never_read_back_as_a_lease_record() {
    let dir = tempdir().unwrap();
    let (mut registry, _gh_log, spawn_log, store) = lease_order_dispatch_registry(dir.path(), &[]);
    let now = Utc::now();
    std::fs::write(
        &store,
        format!(
            "{{\"id\":1,\"created_at\":\"{ts}\",\"updated_at\":\"{ts}\",\"body\":\"<!-- \
             loom:lease-yield host=peer-host sweep=sweep-issue-8843-yielded -->\"}}\n",
            ts = now.to_rfc3339(),
        ),
    )
    .unwrap();

    let outcome = registry
        .dispatch(&SweepKind::Issue(8843), None, None, None, None)
        .expect("a standdown annotation must never be read back as a live lease record");
    assert!(outcome.was_new);
    assert!(wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS));
}

/// Rewrite the harness's fake `gh` so its `issue view --json labels` arm
/// reports `labels`, instead of the empty set it hardcodes.
///
/// Patching the generated script in place keeps `test_support.rs` — which is
/// over the file-size ratchet — untouched, and keeps every other arm
/// (comment write, comment read-back, edit, repo view) byte-for-byte the
/// shared harness's.
fn set_preflip_labels(ws: &Path, labels: &[&str]) {
    let fake_gh = ws.join("fake-gh.sh");
    let payload = labels
        .iter()
        .map(|l| format!("{{\"name\":\"{l}\"}}"))
        .collect::<Vec<_>>()
        .join(",");
    let script = std::fs::read_to_string(&fake_gh).unwrap();
    let before = "printf '{\"labels\":[]}\\n'";
    assert!(
        script.contains(before),
        "the shared harness's label arm changed shape; update this patch"
    );
    std::fs::write(
        &fake_gh,
        script.replace(before, &format!("printf '{{\"labels\":[{payload}]}}\\n'")),
    )
    .unwrap();
}

/// Make the harness's fake `gh` fail every lease-comment READ-back, while
/// leaving the comment WRITE arm working — the shape a rate limit, timeout
/// or transient forge error takes for
/// [`SweepRegistry::read_lease_comments`](super::SweepRegistry::read_lease_comments).
fn break_lease_comment_reads(ws: &Path, store: &Path) {
    let fake_gh = ws.join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake_gh).unwrap();
    let marker = format!("if [[ -f \"{}\" ]]; then", store.display());
    assert!(
        script.contains(&marker),
        "the shared harness's comment read-back arm changed shape; update this patch"
    );
    std::fs::write(&fake_gh, script.replacen(&marker, &format!("exit 1\n{marker}"), 1)).unwrap();
}

/// RAII guard for [`COLLISION_DETECT_ENV`], restoring whatever the ambient
/// process environment actually had. Mirrors `HostIdentityEnvGuard` in
/// `mod.rs`: a test that unconditionally `remove_var`s leaks into the next
/// test's `resolve_collision_detection()` result.
struct CollisionDetectEnvGuard(Option<String>);

impl CollisionDetectEnvGuard {
    fn enable() -> Self {
        let prior = std::env::var(guards::COLLISION_DETECT_ENV).ok();
        std::env::set_var(guards::COLLISION_DETECT_ENV, "1");
        Self(prior)
    }
}

impl Drop for CollisionDetectEnvGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var(guards::COLLISION_DETECT_ENV, v),
            None => std::env::remove_var(guards::COLLISION_DETECT_ENV),
        }
    }
}

/// **Concurrent promotion.** The incident's in-session sweep published its
/// lease *before* curation, so across the whole `loom:curating` ->
/// `loom:issue` -> `loom:building` transition the forge label set carried no
/// claim label for the pre-flip guard (#4085/#7873) to refuse on: with
/// `loom:curating` the snapshot classifies `NotYetApproved`, with
/// `loom:issue` it classifies `Clean`, and **both** proceed by design.
///
/// That is precisely why the lease is the protection here and the label is
/// not. With collision detection explicitly ENABLED — its strongest
/// configuration — a renewed in-session lease must still produce the yield
/// from either promotion state, and the refusal must be the lease-order one
/// (not a collision refusal the label guard happened to raise instead).
#[test]
#[serial]
fn a_renewed_lease_yields_the_dispatch_from_either_promotion_state() {
    for (issue, label) in [(8844_u32, "loom:curating"), (8845_u32, "loom:issue")] {
        let dir = tempdir().unwrap();
        let (mut registry, _gh_log, spawn_log, store) =
            lease_order_dispatch_registry(dir.path(), &[]);
        set_preflip_labels(dir.path(), &[label]);
        seed_renewed_lease(
            &store,
            "<!-- loom:lease host=host-d9142cf3 sweep=sweep-20260924T213744Z-76692-1b436e14 -->",
            INCIDENT_CREATED_SECS_AGO,
            INCIDENT_RENEWED_SECS_AGO,
        );
        let _detect = CollisionDetectEnvGuard::enable();

        let err = registry
            .dispatch(&SweepKind::Issue(issue), None, None, None, None)
            .expect_err(&format!(
                "a live renewed lease must refuse this dispatch with pre-flip labels=[{label}]"
            ));
        let lease_err = err
            .downcast_ref::<LeaseOrderDispatchError>()
            .unwrap_or_else(|| {
                panic!("expected a LeaseOrderDispatchError for labels=[{label}], got: {err:#}")
            });
        assert_eq!(lease_err.earliest_host, "host-d9142cf3");
        assert!(
            !spawn_log.exists(),
            "the yield must land before any builder spawn for labels=[{label}]"
        );
    }
}

/// **Forge read failures stay fail-open.** The new renewal-anchored
/// membership leg only ever ADDS a refusal on positive, forge-assigned
/// evidence of a live earlier claim. An unreadable comments endpoint is not
/// evidence of anything, so it must still resolve to `Proceed` exactly as it
/// did pre-#8840 — a forge blip may never wedge the fleet's dispatch, which
/// is the property `resolve_lease_order`'s fail-open contract exists for.
///
/// The store is seeded with the incident's own renewed record, so the ONLY
/// reason this dispatch proceeds is that the read failed: make the read
/// succeed and this is the yielding regression at the top of this file.
#[test]
#[serial]
fn an_unreadable_forge_still_falls_open_to_proceed() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log, store) = lease_order_dispatch_registry(dir.path(), &[]);
    seed_renewed_lease(
        &store,
        "<!-- loom:lease host=host-d9142cf3 sweep=sweep-20260924T213744Z-76692-1b436e14 -->",
        INCIDENT_CREATED_SECS_AGO,
        INCIDENT_RENEWED_SECS_AGO,
    );
    break_lease_comment_reads(dir.path(), &store);

    let outcome = registry
        .dispatch(&SweepKind::Issue(8846), None, None, None, None)
        .expect("an unreadable forge must never be turned into a refusal");
    assert!(outcome.was_new);
    assert!(
        wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
        "a fail-open dispatch must still spawn its builder"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("loom:lease-yield"),
        "no standdown may be posted on an unverifiable read; gh log: {gh_calls}"
    );
}
