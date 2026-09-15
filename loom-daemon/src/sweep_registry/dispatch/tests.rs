use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;
use tempfile::tempdir;

/// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
/// scope of a test and restores whatever value (if any) it previously
/// had — including across a mid-test assertion panic, since Rust
/// unwinds through `Drop`. Some host/dev-container shells export
/// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
/// without this guard that ambient value silently outranks the
/// `runtimes.default` config precedence this test exercises (#4739).
struct ClearedLoomRuntimeEnv(Option<String>);

impl ClearedLoomRuntimeEnv {
    fn new() -> Self {
        let prior = std::env::var("LOOM_RUNTIME").ok();
        std::env::remove_var("LOOM_RUNTIME");
        Self(prior)
    }
}

impl Drop for ClearedLoomRuntimeEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("LOOM_RUNTIME", v),
            None => std::env::remove_var("LOOM_RUNTIME"),
        }
    }
}

/// As [`ClearedLoomRuntimeEnv`] but for `GH_CONFIG_DIR` (#6529): the test
/// process — or a PREVIOUS test in this same `#[serial]` suite — may
/// itself have a `GH_CONFIG_DIR` set, which would otherwise leak into a
/// spawned fixture child's environment and make the "unregistered root
/// leaves GH_CONFIG_DIR untouched" test below observe an ambient value
/// instead of a genuine absence. Mirrors `role_runner::tests::
/// ClearedGhConfigDirEnv`, added for the same reason by #5522.
struct ClearedGhConfigDirEnv(Option<String>);

impl ClearedGhConfigDirEnv {
    fn new() -> Self {
        let prior = std::env::var("GH_CONFIG_DIR").ok();
        std::env::remove_var("GH_CONFIG_DIR");
        Self(prior)
    }
}

impl Drop for ClearedGhConfigDirEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("GH_CONFIG_DIR", v),
            None => std::env::remove_var("GH_CONFIG_DIR"),
        }
    }
}

#[test]
#[serial]
fn runtime_rejection_precedes_every_dispatch_side_effect() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempdir().unwrap();
    let workspace = dir.path();
    touch_sweep_command(workspace);
    let config_dir = workspace.join(".loom");
    std::fs::write(config_dir.join("config.json"), r#"{"runtimes":{"default":"codex"}}"#).unwrap();
    std::fs::write(
        config_dir.join("runtimes/codex.json"),
        r#"{"runtime":"codex","capabilities":{"worktreeIsolation":"partial","mcp":"yes"}}"#,
    )
    .unwrap();
    let codex = config_dir.join("scripts/spawn-codex.sh");
    std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&codex).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(codex, perms).unwrap();

    let gh_marker = workspace.join("gh-called");
    let fake_gh = workspace.join("fake-gh.sh");
    std::fs::write(&fake_gh, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", gh_marker.display()))
        .unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    let spawn_marker = workspace.join("spawn-called");
    let fake_spawn = workspace.join("fake-spawn.sh");
    std::fs::write(&fake_spawn, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", spawn_marker.display()))
        .unwrap();
    let mut perms = std::fs::metadata(&fake_spawn).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_spawn, perms).unwrap();

    let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
    config.skip_label_flip = false;
    config.gh_bin = Some(fake_gh);
    config.spawn_bin = Some(fake_spawn);
    config.journal_path = Some(workspace.join("journal.json"));
    let mut registry = SweepRegistry::new(config);
    let bus = Arc::new(EventBus::with_capacity(8));
    let mut events = bus.subscribe(["sweep.global"]);
    registry.set_event_bus(bus);
    let error = registry
        .dispatch(&SweepKind::Issue(4494), None, None, None, None)
        .unwrap_err();
    let rejection = error
        .downcast_ref::<crate::runtime_admission::RuntimeRejection>()
        .unwrap();
    assert_eq!(rejection.runtime, "codex");
    assert_eq!(rejection.unmet_capabilities, vec!["worktreeIsolation"]);
    assert!(!gh_marker.exists(), "forge probe/mutation ran before admission");
    assert!(!spawn_marker.exists(), "child spawn ran before admission");
    assert!(!workspace.join(".loom/locks/issues/4494").exists(), "claim lock was created");
    assert!(!registry.compute_log_path(4494).exists(), "log header was created");
    assert!(registry.entries.is_empty(), "capacity/registry entry was consumed");

    // #4494: refused work IS represented on the bus — and only by the
    // rejection topic (never a `sweep.global.dispatch` for work that was
    // never admitted).
    let event = events.try_recv().expect("a rejection event was published");
    assert_eq!(event.topic(), "sweep.global.runtime_rejected");
    match event {
        Event::SweepGlobalRuntimeRejected {
            kind,
            role,
            runtime,
            runtime_source,
            unmet_capabilities,
            reason,
            repo,
        } => {
            assert_eq!(kind, SweepKind::Issue(4494));
            assert_eq!(role, "sweep-lifecycle");
            assert_eq!(runtime, "codex");
            assert_eq!(runtime_source, crate::types::RuntimeSource::DefaultConfig);
            assert_eq!(unmet_capabilities, vec!["worktreeIsolation"]);
            assert!(reason.contains("worktreeIsolation"), "{reason}");
            // Stamped centrally by `emit_event` (#4201's pattern).
            assert_eq!(repo.as_deref(), Some(workspace.display().to_string().as_str()));
        }
        other => panic!("expected SweepGlobalRuntimeRejected, got {other:?}"),
    }
    assert!(events.try_recv().is_err(), "no further events for refused work");
}

/// Build a temp-workspace registry with a fake spawn binary that
/// records its argv + env into a log and exits immediately. This lets
/// us assert on the dispatch behavior without invoking real `claude`.
///
/// We invoke the fake via `bash -c '...'` (returned from
/// `SweepRegistryConfig.spawn_bin`) rather than relying on a shebang +
/// exec bit, because parallel-test load on macOS occasionally races the
/// chmod with the child's posix_spawn exec call and the script silently
/// fails to launch (no shebang resolution, no exec-bit yet).
/// #4431: the reaper's peer-claim heartbeat must re-advertise every live
/// (`Running`/`Pending`) Issue sweep's claim — and ONLY those (a
/// terminal-state entry is a dead sweep whose claim must be allowed to
/// expire), and be a publisher-less no-op (safehouse disabled).
#[test]
fn readvertise_republishes_live_issue_claims_only() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());

    // No publisher attached (safehouse.enabled false): a silent no-op.
    assert_eq!(registry.readvertise_peer_claims(), 0);

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    registry.set_peer_claim_publisher(tx);

    let mk_info = |sweep_id: &str, issue: u32, state: SweepState, log_path: PathBuf| SweepInfo {
        pgid: None,
        sweep_id: sweep_id.to_string(),
        kind: SweepKind::Issue(issue),
        pid: 0,
        token_name: "unknown".into(),
        runtime: "unknown".into(),
        runtime_source: None,
        log_path,
        idempotency_key: None,
        started_at: Utc::now(),
        state,
        latest_phase: None,
        pr_number: None,
        model: None,
        effort: None,
        depends_on: None,
        repo: None,
    };
    let live_log = registry.compute_log_path(4431);
    let dead_log = registry.compute_log_path(999);
    registry.entries.insert(
        "sweep-live".to_string(),
        mk_info("sweep-live", 4431, SweepState::Running, live_log),
    );
    registry.entries.insert(
        "sweep-dead".to_string(),
        mk_info(
            "sweep-dead",
            999,
            SweepState::Exited {
                code: None,
                at: Utc::now(),
            },
            dead_log,
        ),
    );

    assert_eq!(registry.readvertise_peer_claims(), 1);
    let ad = rx.try_recv().expect("one re-advertisement published");
    assert_eq!(ad.issue, 4431);
    assert_eq!(ad.kind, crate::peer_claims::ClaimKind::Advertise);
    assert!(
        rx.try_recv().is_err(),
        "the terminal-state sweep's claim must NOT be re-advertised"
    );
}

#[test]
#[serial]
fn dispatch_happy_path_records_entry() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(42), None, None, None, None)
        .expect("dispatch should succeed");

    assert!(outcome.was_new);
    assert!(outcome.pid > 0);
    assert_eq!(outcome.token_name, "unknown");
    assert_eq!(registry.len(), 1);

    let info = registry.get(&outcome.sweep_id).unwrap();
    assert!(matches!(info.kind, SweepKind::Issue(42)));
    assert!(matches!(info.state, SweepState::Running));

    // Wait for the fake spawn to record its invocation. We wait for
    // the final line (LOOM_TERMINAL_ID) so the assertion isn't racing
    // mid-write.
    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("argv: -p /loom:sweep 42"),
        "expected argv in recorded log; got: {recorded}"
    );
    // Issue #3477 zero-behavior-change criterion: with model=None the
    // spawned command must NOT receive a --model flag at all.
    assert!(
        !recorded.contains("--model"),
        "model=None must not emit --model; got: {recorded}"
    );
    // Issue #3716: with effort=None the spawned command must likewise NOT
    // receive a --effort flag at all (byte-for-byte unchanged default).
    assert!(
        !recorded.contains("--effort"),
        "effort=None must not emit --effort; got: {recorded}"
    );
    // #3482: model=None dispatches record no model on the entry.
    assert_eq!(registry.get(&outcome.sweep_id).unwrap().model, None);
    // #3716: effort=None dispatches record no effort on the entry.
    assert_eq!(registry.get(&outcome.sweep_id).unwrap().effort, None);

    // The lock dir should exist while Running.
    let lock = dir.path().join(".loom").join("locks").join("issue-42");
    assert!(lock.exists(), "expected lock dir at {}", lock.display());

    // Issue #3824: every daemon-dispatched child must carry
    // --dangerously-skip-permissions (unattended, non-interactive).
    assert!(
        recorded.contains("--dangerously-skip-permissions"),
        "expected --dangerously-skip-permissions in argv; got: {recorded}"
    );
}

/// Issue #4028 fail-open: an unreachable safehouse coordination channel (its
/// receiver dropped ⇒ `try_send` returns `Closed`) must NEVER block or fail a
/// dispatch — the soft claim is an optimization, never a liveness dependency.
/// This is the single most important #4028 test.
#[test]
fn dispatch_proceeds_when_peer_claim_channel_is_closed_fail_open() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    // Attach a publisher whose receiver is immediately dropped, so every
    // `try_send` fails — modeling an absent/refusing safehoused.
    let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(1);
    drop(rx);
    registry.set_peer_claim_publisher(tx);

    let outcome = registry
        .dispatch(&SweepKind::Issue(77), None, None, None, None)
        .expect("dispatch must proceed even when the safehouse channel is closed");
    assert!(outcome.was_new, "the sweep still starts");
    assert_eq!(registry.len(), 1);
}

/// Issue #4028: the work-finder's peer-claim skip set reflects the attached
/// shared view, scoped to this registry's repo, and is empty when no view is
/// attached (byte-for-byte no-op).
#[test]
fn peer_claimed_issues_reflects_the_attached_view() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    // No view attached ⇒ empty (the no-op default).
    assert!(registry.peer_claimed_issues().is_empty());

    let repo = peer_claims::repo_slug(&registry.config().workspace_root);
    let view = Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
    {
        let mut v = view.lock().unwrap();
        let now = Instant::now();
        v.observe_at(&ClaimAd::advertise(500, repo.clone(), "peer".into(), 1, "ts".into()), now);
    }
    registry.set_peer_claims(view);
    assert!(registry.peer_claimed_issues().contains(&500));
}

/// Issue #5789: the 2.95 peer-claim guard turns the soft-claim
/// advertisement from a passive "the work-finder happens to check this
/// first" signal into an enforced gate inside `dispatch()` itself, so
/// EVERY call site (not just the work-finder's own pre-tick filter) backs
/// off on a live peer claim. Simulates two hosts racing on the same issue
/// with two independent `SweepRegistry` instances bridged only by a
/// manually-relayed `ClaimAd` — deterministic, no sleeps, mirroring
/// `peer_claims::tests::two_hosts_one_issue_second_host_backs_off_deterministically`
/// but exercised through the real `dispatch()` call path instead of the
/// bare `PeerClaimView`.
#[test]
fn dispatch_backs_off_when_a_peer_hosts_claim_is_already_live() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();

    // Host A dispatches first and advertises its claim.
    let (mut registry_a, _log_a) = fixture_registry(dir_a.path());
    let (tx_a, mut rx_a) = tokio::sync::mpsc::channel(8);
    registry_a.set_peer_claim_publisher(tx_a);

    let outcome_a = registry_a
        .dispatch(&SweepKind::Issue(9789), None, None, None, None)
        .expect("host A should win the race and dispatch normally");
    assert!(outcome_a.was_new);
    assert_eq!(registry_a.len(), 1, "host A recorded exactly one sweep");

    let ad = rx_a
        .try_recv()
        .expect("host A published an advertise ad for #9789");
    assert_eq!(ad.issue, 9789);
    assert_eq!(ad.kind, crate::peer_claims::ClaimKind::Advertise);

    // Host B's inbound safehouse coordination task observes A's ad before
    // B ever attempts its own dispatch — the peer-claim view this closes
    // over is exactly what `set_peer_claims` wires into a live daemon.
    let (mut registry_b, _log_b) = fixture_registry(dir_b.path());
    let repo_b = peer_claims::repo_slug(&registry_b.config().workspace_root);
    // The ad carries host A's own repo slug; re-key it under B's (in this
    // fixture the two temp dirs have different basenames, so re-derive
    // the ad with B's repo identity — a real fleet shares one `LOOM_REPO`
    // across hosts, which this mirrors).
    let relayed = ClaimAd::advertise(ad.issue, repo_b, ad.host, ad.pid, ad.ts);
    let view_b = Arc::new(Mutex::new(PeerClaimView::new(
        "hostB".into(),
        peer_claims::DEFAULT_PEER_CLAIM_TTL,
    )));
    view_b.lock().unwrap().observe_at(&relayed, Instant::now());
    registry_b.set_peer_claims(view_b);

    let err = registry_b
        .dispatch(&SweepKind::Issue(9789), None, None, None, None)
        .expect_err("host B must back off — a peer's claim is already live");
    assert!(
        err.downcast_ref::<CollisionDispatchError>()
            .is_some_and(|e| e.issue == 9789 && e.source == CollisionSource::PeerClaim),
        "expected a PeerClaim CollisionDispatchError, got: {err:#}"
    );
    assert_eq!(
        registry_b.len(),
        0,
        "host B must not record a sweep entry — only one host claims #9789"
    );
}

/// Issue #5789: a peer claim on a DIFFERENT issue never blocks this
/// dispatch — the guard is scoped per-issue, not per-repo.
#[test]
fn dispatch_proceeds_when_the_peer_claim_is_for_a_different_issue() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    let repo = peer_claims::repo_slug(&registry.config().workspace_root);
    let view = Arc::new(Mutex::new(PeerClaimView::new(
        "self".into(),
        peer_claims::DEFAULT_PEER_CLAIM_TTL,
    )));
    view.lock()
        .unwrap()
        .observe_at(&ClaimAd::advertise(1, repo, "peer".into(), 1, "ts".into()), Instant::now());
    registry.set_peer_claims(view);

    let outcome = registry
        .dispatch(&SweepKind::Issue(2), None, None, None, None)
        .expect("a peer claim on a different issue must not block this dispatch");
    assert!(outcome.was_new);
}

/// Issue #5789: upgrades the #4085 pre-flip forge-label collision probe
/// from detection-only into enforcement. Simulates the case the 2.95
/// peer-claim guard cannot catch (no safehouse view attached, mirroring a
/// fleet where the winning host's ad never reached this one) but the
/// forge's own label state already shows the peer's `loom:building`
/// flip — this host must back off instead of duplicating the sweep, MUST
/// NOT reach `gh issue edit`, and MUST log the collision (the existing
/// `detect_and_record_collision` diagnostic, reused verbatim).
#[test]
#[serial]
fn dispatch_backs_off_on_confirmed_forge_label_collision() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log) = collision_dispatch_registry(
        dir.path(),
        r#"{"labels":[{"name":"loom:building"},{"name":"loom:curated"}]}"#,
    );
    registry.set_collision_detection(true);

    let err = registry
        .dispatch(&SweepKind::Issue(9790), None, None, None, None)
        .expect_err("a confirmed forge-label collision must back off dispatch");
    assert!(
        matches!(
            err.downcast_ref::<CollisionDispatchError>(),
            Some(CollisionDispatchError {
                issue: 9790,
                source: CollisionSource::ForgeLabel { .. },
            })
        ),
        "expected a ForgeLabel CollisionDispatchError, got: {err:#}"
    );
    assert_eq!(registry.collision_count(), 1);
    assert_eq!(registry.len(), 0, "no sweep entry must be recorded");

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("issue edit"),
        "the label flip must never be attempted on a confirmed collision; gh log: \
             {gh_calls}"
    );
    assert!(!spawn_log.exists(), "no child must ever be spawned on a confirmed collision");
}

/// Issue #5789: with collision detection at its default (disabled), the
/// upgraded 4a guard is a pure no-op — a clean pre-flip label state
/// (`loom:issue` present, `loom:building` absent) still dispatches
/// normally, mirroring the pre-#5789 byte-for-byte-unchanged contract.
#[test]
#[serial]
fn dispatch_proceeds_on_clean_preflip_labels_with_detection_enabled() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log) = collision_dispatch_registry(
        dir.path(),
        r#"{"labels":[{"name":"loom:issue"},{"name":"loom:curated"}]}"#,
    );
    registry.set_collision_detection(true);

    let outcome = registry
        .dispatch(&SweepKind::Issue(9791), None, None, None, None)
        .expect("a clean pre-flip read must not block dispatch");
    assert!(outcome.was_new);
    assert_eq!(registry.collision_count(), 0);

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit"),
        "a clean collision read must still reach the label flip; gh log: {gh_calls}"
    );
    assert!(
        wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
        "the child must still spawn on a clean collision read"
    );
}

// ------------------------------------------------------------------------
// Claim-then-verify-order dedup at dispatch time (Issue #6287, Epic
// #6165 Phase 2)
// ------------------------------------------------------------------------
//
// These two tests together model the "two near-simultaneous dispatches"
// scenario from opposite perspectives, using
// `lease_order_dispatch_registry`'s shared on-disk lease-comment store to
// simulate the forge's own comment-ordering: one pre-seeds a peer's lease
// comment (id 1) that already landed a moment before this dispatch's own
// (id 2) — modeling the LOSING dispatcher — and the other pre-seeds
// nothing, so this dispatch's own lease comment is unambiguously id 1 —
// modeling the WINNING dispatcher. Exactly one of the two proceeds to
// spawn a builder; the other never does.

/// Issue #6287: when a peer's lease comment already exists on the issue
/// (id 1, an earlier forge-assigned comment order) and this dispatcher's
/// own lease write lands second (id 2), the claim-then-verify-order
/// tie-break MUST make this dispatcher yield: no builder spawned, the
/// claim lock released, the peer-claim advertisement retracted, and a
/// `LeaseOrderDispatchError` naming the earlier host/sweep returned. The
/// `loom:building` label flip itself (`issue edit`) MUST still have been
/// attempted — the yield happens strictly after it, never instead of it.
#[test]
#[serial]
fn dispatch_yields_claim_then_verify_order_tie_break_when_a_peer_lease_is_earlier() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log, comments_store) = lease_order_dispatch_registry(
        dir.path(),
        &["<!-- loom:lease host=peer-host sweep=sweep-issue-9820-peer -->"],
    );

    let err = registry
        .dispatch(&SweepKind::Issue(9820), None, None, None, None)
        .expect_err("losing the claim-then-verify-order tie-break must refuse dispatch");
    let lease_err = err
        .downcast_ref::<LeaseOrderDispatchError>()
        .unwrap_or_else(|| panic!("expected a LeaseOrderDispatchError, got: {err:#}"));
    assert_eq!(lease_err.issue, 9820);
    assert_eq!(lease_err.earliest_host, "peer-host");
    assert_eq!(lease_err.earliest_sweep_id, "sweep-issue-9820-peer");

    assert_eq!(registry.len(), 0, "no sweep entry must be recorded for a losing claim");
    assert!(
        !registry.config().locks_dir().join("issue-9820").exists(),
        "the claim lock this host acquired must be released on a losing tie-break"
    );
    assert!(!spawn_log.exists(), "no builder may ever be spawned for a losing claim");

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit"),
        "the label flip must still have been attempted before the tie-break check; gh log: \
             {gh_calls}"
    );
    assert!(
        gh_calls.contains("loom:lease-yield"),
        "a standdown annotation must be posted when yielding; gh log: {gh_calls}"
    );

    // Both lease comments (the pre-seeded peer's and this dispatcher's
    // own, written by the real `write_lease_comment` call) must be
    // present in the shared store, proving the read-back actually saw
    // both — not just the peer's. A third record (the standdown
    // annotation just asserted above) also lands in the store, but its
    // `<!-- loom:lease-yield ...` marker does not match the
    // `<!-- loom:lease host=...` prefix `read_lease_comments`'s real
    // `--jq` filter selects on, so it must never count as a lease
    // record.
    let stored = std::fs::read_to_string(&comments_store).unwrap_or_default();
    assert!(stored.contains("peer-host"), "the peer's lease record must survive: {stored}");
    let lease_lines = stored
        .lines()
        .filter(|line| line.contains("\"body\":\"<!-- loom:lease host="))
        .count();
    assert!(
        stored.contains("sweep-issue-9820-peer") && lease_lines == 2,
        "this dispatcher's own lease write must also have landed, as the SECOND lease \
             record (excluding the non-matching standdown annotation): {stored}"
    );
}

/// Issue #6350 (Ask 2, generalizing #4485): losing the claim-then-
/// verify-order tie-break is a no-progress outcome for THIS host exactly
/// like the reaper's #4366 backstop, so it must arm the SAME per-issue
/// dispatch backoff a failed dispatch does — otherwise nothing stops the
/// very next work-finder tick from immediately re-losing the identical
/// race against the same still-live earlier claimant (the "9 same-host
/// re-acquisitions" shape observed on 2AMLogic/klayout-tools#994, where
/// one of the two hosts' lease-order yields was this exact mechanism).
#[test]
#[serial]
fn dispatch_arms_backoff_when_losing_claim_then_verify_order_tie_break() {
    let dir = tempdir().unwrap();
    let (mut registry, _gh_log, _spawn_log, _comments_store) = lease_order_dispatch_registry(
        dir.path(),
        &["<!-- loom:lease host=peer-host sweep=sweep-issue-9822-peer -->"],
    );

    assert_eq!(
        registry.dispatch_failure_count(9822),
        0,
        "no backoff must be armed before the first dispatch attempt"
    );

    let err = registry
        .dispatch(&SweepKind::Issue(9822), None, None, None, None)
        .expect_err("losing the claim-then-verify-order tie-break must refuse dispatch");
    assert!(
        err.downcast_ref::<LeaseOrderDispatchError>().is_some(),
        "expected a LeaseOrderDispatchError, got: {err:#}"
    );

    assert_eq!(
        registry.dispatch_failure_count(9822),
        1,
        "a lost lease-order tie-break must arm this issue's dispatch backoff (#6350) so the \
             next tick does not immediately repeat the same losing race"
    );
    assert!(
        registry
            .dispatch_backoff_remaining(9822, Utc::now())
            .is_some(),
        "the armed backoff must actually be in effect immediately after the yield"
    );
}

/// Issue #6287: the complementary case — no peer lease comment predates
/// this dispatch's own, so its lease comment is (unambiguously) the
/// earliest live one. The tie-break must be a no-op: dispatch succeeds
/// exactly like the pre-#6287 behavior, and a builder is spawned.
#[test]
#[serial]
fn dispatch_proceeds_when_this_sweeps_own_lease_is_the_earliest() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log, spawn_log, comments_store) =
        lease_order_dispatch_registry(dir.path(), &[]);

    let outcome = registry
        .dispatch(&SweepKind::Issue(9821), None, None, None, None)
        .expect("the earliest (only) lease comment must not block its own dispatch");
    assert!(outcome.was_new);

    assert!(
        wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
        "a winning tie-break must still spawn the builder"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("loom:lease-yield"),
        "a winning tie-break must never post a standdown annotation; gh log: {gh_calls}"
    );

    let stored = std::fs::read_to_string(&comments_store).unwrap_or_default();
    assert_eq!(
        stored.lines().count(),
        1,
        "exactly this dispatcher's own lease comment must exist: {stored}"
    );
}

// ------------------------------------------------------------------------
// Epic #6165 Phase 2 combined regression suite (Issue #6288, Scenario 3):
// "no duplicate builds when safehouse is down"
// ------------------------------------------------------------------------
//
// Scenarios 1 (reclaim-race) and 2 (acquisition-race) already have
// dedicated unit coverage of their own mechanism in isolation:
// `reconcile_workspace_keeps_claim_when_lease_is_fresh_even_with_channel_absent`
// (`claim_reconciliation.rs`, Issue #6286) and
// `dispatch_yields_claim_then_verify_order_tie_break_when_a_peer_lease_is_earlier`
// / `dispatch_proceeds_when_this_sweeps_own_lease_is_the_earliest` (this
// file, Issue #6287). This test is the higher-level composition Issue
// #6288 actually asks for: the exact shape of the historical duplicate-
// build incidents (loom#6147, loom#6129, klayout-tools#939) — two hosts
// racing to claim the same issue while the peer-claims/safehouse channel
// gives neither of them any evidence about the other — with BOTH Phase 2
// guards composed against the SAME lease-comment record, not two
// independently-scripted fakes.

/// Epic #6165's own end-to-end success criterion ("stopping `safehoused`
/// on every host produces zero duplicate builds"), scoped to the Phase 2
/// mechanisms (#6286 reclamation lease-freshness guard, #6287
/// claim-then-verify-order dedup) — regression coverage for the
/// duplicate-build shape behind loom#6147, loom#6129, and
/// klayout-tools#939.
///
/// Models two hosts racing to dispatch the SAME issue with the
/// peer-claims/safehouse channel simulated fully absent for the whole
/// scenario (no `PeerClaimView` ever registered — "no evidence either
/// way", the exact reading a host with `safehoused` killed produces).
/// Since Epic #6165 Phase 4 (#6317) that channel is never consulted by
/// the reclamation decision at all — the call below goes through the
/// plain [`crate::claim_reconciliation::forge::reconcile_workspace`],
/// with no coordination-evidence seam left to inject in the first
/// place, so this scenario now proves #6286/#6287 are load-bearing on
/// their own by construction rather than by an explicit `None` stand-in:
///
/// 1. A peer host's dispatcher already won the race: its lease comment
///    (id 1) is pre-seeded in the shared store, modeling a live, already-
///    dispatched sweep on that host.
/// 2. This host's own dispatch of the identical issue writes its lease
///    comment second (id 2) and MUST yield before spawning a builder —
///    the claim-then-verify-order tie-break (#6287) — so at most one
///    builder is ever spawned for the issue.
/// 3. A reclamation pass then runs against the peer's surviving claim,
///    with LOCAL evidence (a dead-pid journal entry) that would normally
///    fire an UNCONDITIONAL, immediate reclaim — and is refused anyway,
///    because the peer's lease record (the same id-1 comment this
///    dispatch's own tie-break just read) is still fresh — the
///    reclamation lease-freshness guard (#6286).
///
/// Together: exactly one live claim survives the combined race, and the
/// peer-claims/safehouse channel contributed no evidence to either
/// decision.
#[test]
#[serial]
fn combined_dispatch_and_reclaim_race_produces_no_duplicate_build_with_safehouse_down() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();
    const ISSUE: u32 = 9830;

    // The reconciliation pass below resolves its journal via the
    // process-global env override, independent of this registry's own
    // `config.journal_path` -- point it at a scratch file for this test.
    let journal_path = dir.path().join("reconcile-sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Local evidence a reconciliation pass would normally act on
    // immediately and unconditionally: a journal entry recording a
    // now-dead pid (0) for the SAME issue the peer's lease claims,
    // exactly like the #6286 fixture this scenario composes with.
    let mut journal = sweep_journal::SweepJournal::default();
    journal.entries.push(sweep_journal::JournalEntry {
        repo: repo_str.clone(),
        issue: ISSUE,
        pid: 0,
        started_at: Utc::now(),
    });
    sweep_journal::save(&journal_path, &journal).unwrap();

    let label_updated_at = Utc::now().to_rfc3339();
    let (mut registry, gh_log, spawn_log, comments_store) = safehouse_down_combined_registry(
        &repo_root,
        &["<!-- loom:lease host=peer-host sweep=sweep-issue-9830-peer -->"],
        ISSUE,
        &label_updated_at,
    );

    // --- Step 1 (Scenario 2's mechanism): this host loses the
    //     acquisition race and yields before spawning a builder. ---
    let err = registry
        .dispatch(&SweepKind::Issue(ISSUE), None, None, None, None)
        .expect_err(
            "losing the claim-then-verify-order tie-break must refuse this host's dispatch",
        );
    let lease_err = err
        .downcast_ref::<LeaseOrderDispatchError>()
        .unwrap_or_else(|| panic!("expected a LeaseOrderDispatchError, got: {err:#}"));
    assert_eq!(lease_err.earliest_host, "peer-host");
    assert_eq!(lease_err.earliest_sweep_id, "sweep-issue-9830-peer");
    assert!(
        !spawn_log.exists(),
        "no builder may ever be spawned for the losing side of the acquisition race"
    );

    let stored_after_dispatch = std::fs::read_to_string(&comments_store).unwrap_or_default();
    let lease_lines = stored_after_dispatch
        .lines()
        .filter(|line| line.contains("\"body\":\"<!-- loom:lease host="))
        .count();
    assert_eq!(
        lease_lines, 2,
        "both the peer's pre-seeded lease and this host's own losing write must be in the \
             shared store the reclamation pass below reads: {stored_after_dispatch}"
    );

    // --- Step 2 (Scenario 1's mechanism, composed): a reclamation pass
    //     against the peer's surviving claim, with the safehouse channel
    //     simulated fully absent, must still refuse -- the peer's lease
    //     (the very comment this dispatch's own tie-break just read) is
    //     still fresh. ---
    let gh_bin = registry.config().gh_bin.clone().unwrap();
    let (checked, reclaimed) =
        crate::claim_reconciliation::forge::reconcile_workspace(&gh_bin, &repo_root, false);
    assert_eq!(
        checked, 1,
        "the surviving claim is still inspected -- only the ACTION is frozen"
    );
    assert_eq!(
        reclaimed, 0,
        "the peer's fresh lease must block reclamation even though the dead-pid journal \
             evidence alone would normally reclaim immediately, and even with the \
             peer-claims/safehouse channel contributing no evidence at all (#6286, composed \
             with #6287's own acquisition-race tie-break above -- Issue #6288 Scenario 3)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit"),
        "the label flip from the dispatch race must still have been attempted; gh log: \
             {gh_calls}"
    );
    assert!(
        !gh_calls.contains("--add-label loom:issue"),
        "no gh issue edit reverting the surviving claim's loom:building label may be issued \
             while its lease is fresh; gh log: {gh_calls}"
    );

    // Nothing was reclaimed, so the journal entry the reclamation pass
    // read from must survive untouched.
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, ISSUE).is_some());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// Issue #3953: `dispatch` persists a liveness record to the sweep
/// journal (repo/issue/pid/started_at), and the reaper's dead-PID path
/// removes it once the child is confirmed dead — end-to-end wiring,
/// not just the `sweep_journal` module's own unit tests.
#[test]
fn dispatch_and_reap_wire_the_sweep_journal() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let journal_path = registry.config().journal_path.clone().unwrap();

    let outcome = registry
        .dispatch(&SweepKind::Issue(4600), None, None, None, None)
        .expect("dispatch should succeed");

    let journal = crate::sweep_journal::load(&journal_path);
    let repo = dir.path().display().to_string();
    let entry = crate::sweep_journal::find(&journal, &repo, 4600)
        .expect("dispatch should have recorded a journal entry for #4600");
    assert_eq!(entry.pid, outcome.pid);

    // The fixture's fake spawn-claude.sh exits immediately, so the pid is
    // dead almost immediately; wait for the reaper's dead-PID path. The
    // budget is deliberately generous (#3985) so host CPU starvation — not
    // a code fault — can never redden this via a missed deadline.
    let dead = wait_for_condition(FIXTURE_CHILD_WAIT_MS, || !is_pid_alive(outcome.pid));
    assert!(dead, "fixture child did not exit within the wait budget");

    registry.reap_once();

    let journal = crate::sweep_journal::load(&journal_path);
    assert!(
        crate::sweep_journal::find(&journal, &repo, 4600).is_none(),
        "reap_once should have removed the dead sweep's journal entry"
    );
}

/// Issue #3824: `spawn_child` unconditionally appends
/// `--dangerously-skip-permissions` to the child argv so a detached,
/// non-interactive `claude -p` sweep never stalls on a permission prompt.
/// With no model/effort/depends-on the flag directly follows the
/// `--claim-owned <N>` marker (#4111, always emitted for a daemon
/// dispatch), appended AFTER it (verified by the exact positional form).
#[test]
#[serial]
fn dispatch_appends_dangerously_skip_permissions() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4242), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains(
            "argv: -p /loom:sweep 4242 --claim-owned 4242 --dangerously-skip-permissions"
        ),
        "expected --claim-owned then --dangerously-skip-permissions appended after the \
             prompt; got: {recorded}"
    );
}

/// Issue #4255: a daemon dispatch routes the child through
/// `claude-wrapper.sh` by appending `--use-wrapper` immediately AFTER
/// `--dangerously-skip-permissions`, so a transient API death (rate-limit /
/// 5xx / overloaded / bare `Execution error`) is retried instead of killing
/// the whole sweep on the first failure. Serialized on the named
/// `loom_use_wrapper_env` lock shared with every other test that reads or
/// mutates `LOOM_USE_WRAPPER` (this module + `role_runner`), so a concurrent
/// opt-out test cannot flip the flag mid-run.
#[test]
#[serial(loom_use_wrapper_env)]
fn dispatch_appends_use_wrapper_flag() {
    std::env::remove_var("LOOM_USE_WRAPPER");
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4255), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("--dangerously-skip-permissions --use-wrapper"),
        "expected --use-wrapper appended after --dangerously-skip-permissions; got: {recorded}"
    );
    // The flag must be its OWN argv token (spawn-claude.sh consumes it), not
    // folded into the prompt like --claim-owned.
    assert!(
        recorded.contains("arg: --use-wrapper"),
        "expected --use-wrapper as a standalone argv token; got: {recorded}"
    );
}

/// Issue #4255: the `LOOM_USE_WRAPPER=0` debug opt-out restores the legacy
/// single-shot argv — no `--use-wrapper` token — so an operator can
/// reproduce a raw first-shot failure. Shares the named `loom_use_wrapper_env`
/// lock so it never races the presence tests that assume the wrapper-on default.
#[test]
#[serial(loom_use_wrapper_env)]
fn dispatch_opt_out_omits_use_wrapper_flag() {
    std::env::set_var("LOOM_USE_WRAPPER", "0");
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4256), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    std::env::remove_var("LOOM_USE_WRAPPER");
    assert!(
        !recorded.contains("--use-wrapper"),
        "LOOM_USE_WRAPPER=0 must suppress --use-wrapper; got: {recorded}"
    );
    // The rest of the argv contract is unchanged.
    assert!(
        recorded.contains("--dangerously-skip-permissions"),
        "opt-out must not drop --dangerously-skip-permissions; got: {recorded}"
    );
}

/// Issue #3823 (Option A): `spawn_child` exports the claim-ownership
/// marker `LOOM_SWEEP_CLAIM_OWNED=<issue>` into the dispatched child so its
/// `/loom:sweep` pre-flight recognises the daemon's own pre-dispatch
/// loom:building flip as its OWN claim (and proceeds to build) rather than
/// self-skipping. The value is exactly the dispatched issue number.
#[test]
#[serial]
fn dispatch_exports_claim_ownership_marker() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4243), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("LOOM_SWEEP_CLAIM_OWNED=4243"),
        "expected claim-ownership marker for issue 4243; got: {recorded}"
    );
}

/// Issue #4111 (Option 1, the positional half of the fix): in addition to
/// the `LOOM_SWEEP_CLAIM_OWNED` env var above, `spawn_child` appends
/// `--claim-owned <issue>` to the child's own argv. This is the primary
/// signal — positional in the model's context by construction — that
/// `/loom:sweep`'s mandatory Step 1a pre-flight check consumes. Asserts
/// BOTH channels are present on the same dispatch (belt-and-suspenders,
/// per the issue's explicit "keep the env var exported regardless for
/// backward compatibility" guidance) and that the flag carries exactly
/// the dispatched issue number, unconditionally (unlike the
/// optional --model/--effort/--depends-on flags, this one is never
/// absent on a daemon dispatch).
#[test]
#[serial]
fn dispatch_appends_claim_owned_flag() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4246), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("argv: -p /loom:sweep 4246 --claim-owned 4246"),
        "expected --claim-owned 4246 in argv immediately after the prompt; got: {recorded}"
    );
    // Regression for the #4120 review: `--claim-owned` MUST be embedded in
    // the `-p` prompt string, NOT appended as a sibling argv token. The
    // real `claude` CLI rejects `--claim-owned` as an unknown option and
    // exits 1 if it arrives as its own token; only text inside the single
    // `-p "<prompt>"` value reaches the `/loom:sweep` skill's `$ARGUMENTS`.
    // The fixture records each argv token on its own `arg: ` line, so we
    // can assert the flag is part of the prompt VALUE (one token that also
    // carries `/loom:sweep`) and NOT a standalone `arg: --claim-owned`
    // token — the `$*`-substring assertion above cannot tell these apart
    // (which is precisely how the original sibling-arg bug slipped through).
    assert!(
        recorded.contains("arg: /loom:sweep 4246 --claim-owned 4246"),
        "expected --claim-owned inside the single -p prompt token; got: {recorded}"
    );
    assert!(
        !recorded.contains("arg: --claim-owned"),
        "--claim-owned must NOT be a standalone argv token (the real claude CLI \
             rejects it as an unknown option); got: {recorded}"
    );
    // Belt-and-suspenders: the env var must still be present too (#3823
    // backward compatibility, per #4111's explicit guidance to keep it).
    assert!(
        recorded.contains("LOOM_SWEEP_CLAIM_OWNED=4246"),
        "expected the LOOM_SWEEP_CLAIM_OWNED env var alongside the flag; got: {recorded}"
    );
}

// -- #6529: per-owner GH_CONFIG_DIR forwarded to sweep-dispatch children --
//
// Same root-cause class already fixed for the daemon-internal call sites
// (#5401/#5431) and role-runner-dispatched children (#5508/#5522): a
// sweep child spawned by `spawn_child` inherits `current_dir(&self.config
// .workspace_root)`, so it must carry the SAME per-owner `GH_CONFIG_DIR`
// — otherwise it can inherit whatever `GH_CONFIG_DIR` the daemon process
// (or a previously dispatched sweep child for a DIFFERENT workspace)
// happens to have, and every forge call the spawned `/loom:sweep`
// session makes 404s silently before the sweep's first checkpoint.

/// A sweep child dispatched for a workspace registered under a
/// non-default owner (mirrors a cross-owner managed repo, #5401/#5431)
/// must carry that owner's `GH_CONFIG_DIR` on its spawned
/// `spawn-claude.sh` process.
#[test]
#[serial]
fn dispatch_forwards_owner_gh_config_dir_for_a_registered_root() {
    let dir = tempdir().unwrap();
    crate::credential_preflight::clear_owner_root_registry();
    let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
    crate::credential_preflight::register_root_gh_config_dir(dir.path(), &owner_dir);

    let (mut registry, record_log) = fixture_registry(dir.path());
    let outcome = registry
        .dispatch(&SweepKind::Issue(6529), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    crate::credential_preflight::clear_owner_root_registry();
    assert!(
        recorded.contains(&format!("GH_CONFIG_DIR={}", owner_dir.display())),
        "expected the registered owner's GH_CONFIG_DIR on the sweep child; got: {recorded}"
    );
}

/// The flip side: a workspace that is NOT registered under a non-default
/// owner (the common single-owner fleet, or the root owner's own repos)
/// must be a byte-identical no-op — the sweep child's `GH_CONFIG_DIR` is
/// left untouched so it inherits the daemon's own process-global
/// default, never a stale value left behind by a PREVIOUSLY dispatched
/// sweep child for a different workspace.
#[test]
#[serial]
fn dispatch_leaves_gh_config_dir_untouched_for_an_unregistered_root() {
    let _env_guard = ClearedGhConfigDirEnv::new();
    let dir = tempdir().unwrap();
    crate::credential_preflight::clear_owner_root_registry();

    let (mut registry, record_log) = fixture_registry(dir.path());
    let outcome = registry
        .dispatch(&SweepKind::Issue(6530), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("GH_CONFIG_DIR=unset"),
        "expected no GH_CONFIG_DIR forwarded for an unregistered root; got: {recorded}"
    );
}

/// Issue #3943: `spawn_child` pins
/// `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` on the dispatched child env so
/// the print-mode harness does not reap the sweep's long-running
/// Builder/Judge background subagents at the 600s ceiling (which caused
/// loom:building<->loom:issue label ping-pong). The value is exactly "0"
/// (no cap).
#[test]
#[serial]
fn dispatch_disables_print_bg_wait_ceiling() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4244), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0"),
        "expected print-mode bg-wait ceiling disabled (=0); got: {recorded}"
    );
}

/// Issue #3477 (Phase 1): a `model` dispatch param threads through to
/// the spawn command as an explicit `--model <value>` argument.
#[test]
#[serial]
fn dispatch_with_model_appends_model_arg() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(43), None, Some("claude-sonnet-4-6"), None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("argv: -p /loom:sweep 43 --claim-owned 43 --model claude-sonnet-4-6"),
        "expected --model in argv; got: {recorded}"
    );
    // #3482 (Phase 3a): the dispatch model is carried on the registry
    // entry so list_sweeps / get_sweep_status report it.
    assert_eq!(
        registry.get(&outcome.sweep_id).unwrap().model.as_deref(),
        Some("claude-sonnet-4-6"),
        "dispatch model must be recorded on the SweepInfo entry"
    );
}

/// Issue #3477: an empty-string model is treated as unset — `--model ""`
/// must never be emitted (acceptance criterion: no flag at all, not an
/// empty flag).
#[test]
#[serial]
fn dispatch_with_empty_model_emits_no_model_flag() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(44), None, Some(""), None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        !recorded.contains("--model"),
        "empty model must not emit --model; got: {recorded}"
    );
    // #3482: empty-string model normalizes to None on the entry too.
    assert_eq!(
        registry.get(&outcome.sweep_id).unwrap().model,
        None,
        "empty model must be recorded as None on the SweepInfo entry"
    );
}

/// Issue #3716: an `effort` dispatch param threads through to the spawn
/// command as an explicit `--effort <level>` argument, mirroring `--model`.
#[test]
#[serial]
fn dispatch_with_effort_appends_effort_arg() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(45), None, None, Some("xhigh"), None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("argv: -p /loom:sweep 45 --claim-owned 45 --effort xhigh"),
        "expected --effort in argv; got: {recorded}"
    );
    // The dispatch effort is carried on the registry entry so
    // list_sweeps / get_sweep_status report it (mirrors #3482 for model).
    assert_eq!(
        registry.get(&outcome.sweep_id).unwrap().effort.as_deref(),
        Some("xhigh"),
        "dispatch effort must be recorded on the SweepInfo entry"
    );
}

/// Issue #3716: `model` + `effort` both set emit both flags, in the
/// order `--model <m> --effort <e>` (effort appended right after model).
#[test]
#[serial]
fn dispatch_with_model_and_effort_appends_both_args() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(46), None, Some("claude-sonnet-4-6"), Some("xhigh"), None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains(
            "argv: -p /loom:sweep 46 --claim-owned 46 --model claude-sonnet-4-6 --effort xhigh"
        ),
        "expected --model then --effort in argv; got: {recorded}"
    );
    let entry = registry.get(&outcome.sweep_id).unwrap();
    assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(entry.effort.as_deref(), Some("xhigh"));
}

/// Issue #3716: an empty-string effort is treated as unset — `--effort ""`
/// must never be emitted (no flag at all, not an empty flag).
#[test]
#[serial]
fn dispatch_with_empty_effort_emits_no_effort_flag() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(47), None, None, Some(""), None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        !recorded.contains("--effort"),
        "empty effort must not emit --effort; got: {recorded}"
    );
    // Empty-string effort normalizes to None on the entry too.
    assert_eq!(
        registry.get(&outcome.sweep_id).unwrap().effort,
        None,
        "empty effort must be recorded as None on the SweepInfo entry"
    );
}

/// Issue #3729 (stacked-PR v1): a `depends_on` dispatch param threads
/// through to the spawn command as an explicit `--depends-on <N>`
/// argument, mirroring `--model` / `--effort`. It is recorded on the
/// SweepInfo entry so the reaper can block the subtree on parent failure.
#[test]
#[serial]
fn dispatch_with_depends_on_appends_depends_on_arg() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(50), None, None, None, Some(49))
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("argv: -p /loom:sweep 50 --claim-owned 50 --depends-on 49"),
        "expected --depends-on in argv; got: {recorded}"
    );
    // Regression for #4121 (mirrors the #4120 `--claim-owned` review): the
    // flattened `argv:` assertion above renders byte-identically whether
    // `--depends-on 49` is embedded in the single `-p` prompt token or
    // appended as its own sibling argv token — so it cannot distinguish
    // the fixed and buggy forms. The real `claude` CLI rejects
    // `--depends-on` as an unknown option if it ever arrives as a
    // standalone token; only text inside the `-p "<prompt>"` value
    // reaches the `/loom:sweep` skill's `$ARGUMENTS`. Use the per-token
    // `arg: ` fixture lines (as `dispatch_appends_claim_owned_flag` does)
    // to assert the flag is part of the prompt VALUE and NOT a
    // standalone token.
    assert!(
        recorded.contains("arg: /loom:sweep 50 --claim-owned 50 --depends-on 49"),
        "expected --depends-on inside the single -p prompt token; got: {recorded}"
    );
    assert!(
        !recorded.contains("arg: --depends-on"),
        "--depends-on must NOT be a standalone argv token (the real claude CLI \
             rejects it as an unknown option); got: {recorded}"
    );
    assert_eq!(
        registry.get(&outcome.sweep_id).unwrap().depends_on,
        Some(49),
        "dispatch depends_on must be recorded on the SweepInfo entry"
    );
}

/// Issue #3729: absent `depends_on`, no `--depends-on` flag is emitted —
/// byte-for-byte unchanged behavior (opt-in, no default-path regression).
#[test]
#[serial]
fn dispatch_without_depends_on_emits_no_flag() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(51), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        !recorded.contains("--depends-on"),
        "depends_on=None must not emit --depends-on; got: {recorded}"
    );
    assert_eq!(
        registry.get(&outcome.sweep_id).unwrap().depends_on,
        None,
        "depends_on=None must be recorded as None on the SweepInfo entry"
    );
}

/// Issue #3730: when the experiment-related env vars are set in the daemon
/// process, `spawn_child` forwards them (via the explicit allowlist) to the
/// detached child, and pins the child's cwd to the workspace root.
#[test]
#[serial]
fn dispatch_forwards_experiment_env_and_sets_cwd() {
    let dir = tempdir().unwrap();
    // Canonicalize because the fixture records `pwd -P` (symlink-resolved),
    // while tempdir() on macOS lives under a /var -> /private/var symlink.
    let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    // Export the experiment vars into the daemon (test) process env just
    // before dispatch — this is exactly the operator scenario #3730 fixes.
    std::env::set_var("LOOM_MODEL_EXPERIMENT", "canary");
    std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
    std::env::set_var("LOOM_TRANSCRIPT_ARCHIVE", "/tmp/loom-archive-3730");

    let outcome = registry
        .dispatch(&SweepKind::Issue(48), None, None, None, None)
        .expect("dispatch should succeed");

    // Clean up the process env immediately so a failure below can't leak
    // into sibling #[serial] tests.
    std::env::remove_var("LOOM_MODEL_EXPERIMENT");
    std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
    std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains("LOOM_MODEL_EXPERIMENT=canary"),
        "expected LOOM_MODEL_EXPERIMENT forwarded to child; got: {recorded}"
    );
    assert!(
        recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=1"),
        "expected LOOM_MODEL_EXPERIMENT_CANARY forwarded to child; got: {recorded}"
    );
    assert!(
        recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=/tmp/loom-archive-3730"),
        "expected LOOM_TRANSCRIPT_ARCHIVE forwarded to child; got: {recorded}"
    );
    assert!(
        recorded.contains(&format!("PWD={}", expected_cwd.display())),
        "expected child cwd pinned to workspace root {}; got: {recorded}",
        expected_cwd.display()
    );
}

/// Issue #3730 no-op criterion: when none of the experiment env vars are
/// set in the daemon process, `spawn_child` does NOT forward them to the
/// child (the child observes them as unset). The cwd is still pinned to
/// the workspace root regardless.
#[test]
#[serial]
fn dispatch_does_not_forward_unset_experiment_env() {
    // Ensure a clean slate — a leaked value from another test would make
    // this a false pass.
    std::env::remove_var("LOOM_MODEL_EXPERIMENT");
    std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
    std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

    let dir = tempdir().unwrap();
    let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(49), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    // The fixture prints `<VAR>=unset` when the child sees the var unset.
    assert!(
        recorded.contains("LOOM_MODEL_EXPERIMENT=unset"),
        "unset LOOM_MODEL_EXPERIMENT must not be forwarded; got: {recorded}"
    );
    assert!(
        recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=unset"),
        "unset LOOM_MODEL_EXPERIMENT_CANARY must not be forwarded; got: {recorded}"
    );
    assert!(
        recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=unset"),
        "unset LOOM_TRANSCRIPT_ARCHIVE must not be forwarded; got: {recorded}"
    );
    // cwd is pinned unconditionally.
    assert!(
        recorded.contains(&format!("PWD={}", expected_cwd.display())),
        "expected child cwd pinned to workspace root {}; got: {recorded}",
        expected_cwd.display()
    );
}

/// Issue #6667: the build-cache group is forwarded on the same terms as
/// the experiment group — a fleet host sets these on the daemon's
/// supervisor so a sweep's `cargo build` shares the S3 object cache
/// instead of cold-compiling its fresh worktree.
#[test]
#[serial]
fn dispatch_forwards_build_cache_env() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    std::env::set_var("RUSTC_WRAPPER", "/opt/homebrew/bin/sccache");
    std::env::set_var("SCCACHE_BUCKET", "fleet-sccache-fixture");
    std::env::set_var("SCCACHE_REGION", "us-east-1");
    std::env::set_var("SCCACHE_SERVER_PORT", "4227");
    std::env::set_var("AWS_PROFILE", "sccache");

    let outcome = registry
        .dispatch(&SweepKind::Issue(6667), None, None, None, None)
        .expect("dispatch should succeed");

    // Clear immediately so a failure below cannot leak into sibling
    // #[serial] tests (same discipline as the #3730 pair above).
    std::env::remove_var("RUSTC_WRAPPER");
    std::env::remove_var("SCCACHE_BUCKET");
    std::env::remove_var("SCCACHE_REGION");
    std::env::remove_var("SCCACHE_SERVER_PORT");
    std::env::remove_var("AWS_PROFILE");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    for expected in [
        "RUSTC_WRAPPER=/opt/homebrew/bin/sccache",
        "SCCACHE_BUCKET=fleet-sccache-fixture",
        "SCCACHE_REGION=us-east-1",
        "SCCACHE_SERVER_PORT=4227",
        "AWS_PROFILE=sccache",
    ] {
        assert!(
            recorded.contains(expected),
            "expected `{expected}` forwarded to child; got: {recorded}"
        );
    }
}

/// Issue #6667 no-op criterion: a host that has not opted into the build
/// cache dispatches byte-for-byte as before — no empty `RUSTC_WRAPPER`
/// reaches the child, which would otherwise point `cargo` at an empty
/// wrapper path and break every build in the sweep.
#[test]
#[serial]
fn dispatch_does_not_forward_unset_or_empty_build_cache_env() {
    std::env::remove_var("SCCACHE_BUCKET");
    std::env::remove_var("SCCACHE_REGION");
    std::env::remove_var("SCCACHE_SERVER_PORT");
    std::env::remove_var("AWS_PROFILE");
    // Empty string is treated as unset, exactly like the experiment group.
    std::env::set_var("RUSTC_WRAPPER", "");

    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(6668), None, None, None, None)
        .expect("dispatch should succeed");

    std::env::remove_var("RUSTC_WRAPPER");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    // The fixture prints `<VAR>=unset` when the child sees the var unset.
    for expected in [
        "RUSTC_WRAPPER=unset",
        "SCCACHE_BUCKET=unset",
        "SCCACHE_REGION=unset",
        "SCCACHE_SERVER_PORT=unset",
        "AWS_PROFILE=unset",
    ] {
        assert!(
            recorded.contains(expected),
            "expected `{expected}` (not forwarded); got: {recorded}"
        );
    }
}

#[test]
#[serial]
fn dispatch_lock_collision_rejected() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let first = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
    assert!(first.is_ok());

    let second = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
    assert!(second.is_err(), "second dispatch for issue #7 should fail (lock collision)");
    let err = second.unwrap_err().to_string();
    assert!(err.contains("lock collision"), "expected lock collision error; got: {err}");
}

#[test]
#[serial]
fn dispatch_idempotency_returns_existing() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let first = registry
        .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
        .unwrap();
    assert!(first.was_new);

    // While still Running, a dispatch with the same key must dedup.
    // Issue #99 is the same kind, but we don't need a different issue —
    // the dedup is purely on the idempotency key.
    let second = registry
        .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
        .unwrap();
    assert!(!second.was_new);
    assert_eq!(first.sweep_id, second.sweep_id);
}

/// Issue #5342: `PrSet` dispatch is no longer rejected — it spawns via
/// the same `spawn_child` path `Issue` uses, is tracked in the registry
/// exactly like an `Issue` sweep (`list`/`get_status` both see it), and
/// its per-PR claim lock (`.loom/locks/pr-<N>/`, distinct from `Issue`'s
/// `.loom/locks/issue-<N>/`) refuses a second dispatch that shares a PR
/// number with an already-dispatched set.
#[test]
#[serial]
fn pr_set_dispatch_succeeds_is_tracked_and_locks_per_pr() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::PrSet(vec![101, 202]), None, None, None, None)
        .expect("PrSet dispatch should succeed (#5342)");
    assert!(outcome.was_new);

    let listed = registry.list(None);
    assert_eq!(listed.len(), 1, "PrSet dispatch should produce exactly one tracked sweep");
    assert_eq!(listed[0].sweep_id, outcome.sweep_id);
    match &listed[0].kind {
        SweepKind::PrSet(prs) => assert_eq!(prs, &vec![101, 202]),
        other => panic!("expected SweepKind::PrSet, got {other:?}"),
    }
    assert!(
        registry.get_status(&outcome.sweep_id).is_some(),
        "get_sweep_status should find the PrSet sweep"
    );

    // A second PrSet dispatch sharing PR #101 collides on the per-PR lock
    // (Issue #5342's "per-issue lock semantics resolved" requirement).
    let collide = registry.dispatch(&SweepKind::PrSet(vec![101, 303]), None, None, None, None);
    let err = collide.unwrap_err().to_string();
    assert!(err.contains("PR #101"), "expected a PR #101 lock-collision error; got: {err}");

    // The sweep can be cancelled (Issue #5342's explicit test AC): the
    // fake spawn fixture exits almost immediately, so `cancel` may
    // observe an already-terminal child, but either way the call
    // succeeds and the sweep_id matches.
    let cancelled = registry
        .cancel(&outcome.sweep_id, Duration::from_millis(500))
        .expect("PrSet sweep should be cancellable");
    assert_eq!(cancelled.sweep_id, outcome.sweep_id);

    // Cancelling releases every PR's lock, so a fresh PrSet dispatch
    // reusing the same PR numbers is no longer blocked.
    let after = registry.dispatch(&SweepKind::PrSet(vec![101, 202]), None, None, None, None);
    assert!(after.is_ok(), "PR locks should be released after cancel: {after:?}");
}

/// Issue #5342: an empty PrSet is refused with a clear error instead of
/// silently spawning a `/loom:sweep --prs` with nothing to sweep.
#[test]
fn pr_set_dispatch_rejects_empty_set() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let outcome = registry.dispatch(&SweepKind::PrSet(vec![]), None, None, None, None);
    let err = outcome.unwrap_err().to_string();
    assert!(err.contains("empty PrSet"), "expected an empty-PrSet refusal; got: {err}");
}

/// The delay curve doubles per consecutive failure and clamps at `max`;
/// a zero streak (and a zero base) is always no delay.
#[test]
fn backoff_delay_doubles_then_clamps_at_max() {
    let base = Duration::from_secs(60);
    let max = Duration::from_secs(900);
    assert_eq!(backoff_delay(0, base, max), Duration::ZERO, "no failures ⇒ no delay");
    assert_eq!(backoff_delay(1, base, max), Duration::from_secs(60));
    assert_eq!(backoff_delay(2, base, max), Duration::from_secs(120));
    assert_eq!(backoff_delay(3, base, max), Duration::from_secs(240));
    assert_eq!(backoff_delay(4, base, max), Duration::from_secs(480));
    assert_eq!(backoff_delay(5, base, max), Duration::from_secs(900), "clamped");
    assert_eq!(backoff_delay(50, base, max), max, "a long streak saturates, never overflows");
    assert_eq!(backoff_delay(3, Duration::ZERO, max), Duration::ZERO, "zero base disables");
}

/// AC: a minimum interval is enforced between same-issue dispatch attempts
/// after a failed dispatch. The refusal carries the typed
/// [`DispatchBackoffError`] (so the work finder can attribute it) and clears
/// the moment the window is released.
#[test]
fn dispatch_refused_while_backoff_window_is_live() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    registry.record_dispatch_failure(4485);
    assert_eq!(registry.dispatch_failure_count(4485), 1);

    let err = registry
        .dispatch(&SweepKind::Issue(4485), None, None, None, None)
        .expect_err("a live backoff window must refuse dispatch");
    let typed = err
        .downcast_ref::<DispatchBackoffError>()
        .expect("refusal must carry the typed DispatchBackoffError");
    assert_eq!(typed.issue, 4485);
    assert_eq!(typed.consecutive, 1);
    assert!(typed.retry_after_secs > 0 && typed.retry_after_secs <= 60);

    // The refusal happens BEFORE the claim lock and the label flip, so
    // nothing was claimed and nothing was spawned.
    assert!(
        !registry.config.locks_dir().join("issue-4485").exists(),
        "a backoff refusal must not acquire the claim lock"
    );
    assert!(registry.entries.is_empty(), "a backoff refusal must not register a sweep");

    // Releasing the window (progress, or the operator's quarantine clear)
    // makes the issue immediately eligible again — the breaker never wedges.
    assert!(registry.clear_dispatch_backoff(4485));
    let outcome = registry
        .dispatch(&SweepKind::Issue(4485), None, None, None, None)
        .expect("dispatch proceeds once the window is cleared");
    assert!(outcome.was_new);
}

/// Issue #7477: arming a LOCAL dispatch-backoff window must also
/// broadcast it fleet-wide over the same peer-claim channel dispatch
/// claims use, so a peer host does not immediately re-attempt the same
/// failing candidate this host just backed off on.
#[test]
fn record_dispatch_failure_broadcasts_fleet_wide() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    registry.set_peer_claim_publisher(tx);

    registry.record_dispatch_failure(4485);

    let ad = rx
        .try_recv()
        .expect("a cooldown/backoff ad must be published");
    assert_eq!(ad.kind, crate::peer_claims::ClaimKind::DispatchBackoffArmed);
    assert_eq!(ad.issue, 4485);
    assert_eq!(ad.remaining_secs, Some(60), "the first failure's delay is `base` (60s)");
}

/// Issue #7477: `dispatch_backoff_issues` must union this host's own
/// local backoff state with a live window a PEER host has broadcast —
/// the fleet-scope fix. A registry with NO local record for an issue
/// still must not offer it while a peer's broadcast window is live.
#[test]
fn dispatch_backoff_issues_reflects_a_peer_armed_window() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    // No local record at all for issue 9001.
    assert!(!registry.dispatch_backoff_issues(Utc::now()).contains(&9001));

    let repo = peer_claims::repo_slug(&registry.config().workspace_root);
    let view = Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
    {
        let mut v = view.lock().unwrap();
        v.observe_dispatch_backoff_at(
            &ClaimAd::dispatch_backoff_armed(9001, repo, "peer".into(), 1, "ts".into(), 60),
            Instant::now(),
        );
    }
    registry.set_peer_claims(view);
    assert!(
        registry.dispatch_backoff_issues(Utc::now()).contains(&9001),
        "a peer-armed backoff window must suppress this host's dispatch too"
    );
}

/// A disabled backoff is byte-for-byte the pre-#4485 path: no window is ever
/// armed and dispatch is never refused.
#[test]
fn disabled_backoff_never_refuses_dispatch() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    registry.set_dispatch_backoff_config(DispatchBackoffConfig {
        enabled: false,
        ..DispatchBackoffConfig::default()
    });

    registry.record_dispatch_failure(77);
    assert_eq!(registry.dispatch_failure_count(77), 0, "disabled ⇒ nothing recorded");
    assert!(registry
        .dispatch_backoff_remaining(77, Utc::now())
        .is_none());
    assert!(registry
        .dispatch(&SweepKind::Issue(77), None, None, None, None)
        .is_ok());
}

/// AC (#6917): a direct `{"Issue": <N>}` dispatch — the exact shape
/// `dispatch_sweep_nonblocking` / `loom-daemon dispatch <N>` /
/// `--claim-owned <N>` all funnel through `begin_issue_dispatch` — is
/// refused with the typed [`NoopCooldownDispatchError`] while a live
/// no-op-release cooldown is armed for the target issue, mirroring
/// [`dispatch_refused_while_backoff_window_is_live`] for the 2.8 backoff
/// guard immediately below it in the guard chain. The refusal happens
/// before the claim lock and label flip, exactly like every other
/// pre-flip guard.
#[test]
fn dispatch_refused_while_noop_cooldown_is_live() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());

    registry.record_noop_release(6917, Some("external blocker unchanged".into()));
    assert_eq!(registry.noop_release_count(6917), 1);

    let err = registry
        .dispatch(&SweepKind::Issue(6917), None, None, None, None)
        .expect_err("a live noop-cooldown window must refuse dispatch");
    let typed = err
        .downcast_ref::<NoopCooldownDispatchError>()
        .expect("refusal must carry the typed NoopCooldownDispatchError");
    assert_eq!(typed.issue, 6917);
    assert!(typed.retry_after_secs > 0);

    // The refusal happens BEFORE the claim lock and the label flip, so
    // nothing was claimed and nothing was spawned.
    assert!(
        !registry.config.locks_dir().join("issue-6917").exists(),
        "a noop-cooldown refusal must not acquire the claim lock"
    );
    assert!(registry.entries.is_empty(), "a noop-cooldown refusal must not register a sweep");

    // Releasing the window (a daemon restart, or the cooldown simply
    // elapsing — exercised separately below) makes the issue immediately
    // eligible again — the guard never wedges the issue permanently.
    assert!(registry.clear_noop_cooldown(6917));
    let outcome = registry
        .dispatch(&SweepKind::Issue(6917), None, None, None, None)
        .expect("dispatch proceeds once the cooldown is cleared");
    assert!(outcome.was_new);
}

/// AC (#6917 edge case): an EXPIRED cooldown must never hold an issue
/// back — the check is purely `until > now`, so a stale record dispatches
/// exactly like a fresh issue with no record at all. Mirrors
/// [`elapsed_backoff_window_stops_refusing`] for the sibling 2.8 guard.
#[test]
fn dispatch_proceeds_once_noop_cooldown_expires() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    registry.record_noop_release(6918, None);
    if let Some(state) = registry.noop_cooldown.get_mut(&6918) {
        state.until = Utc::now() - chrono::Duration::seconds(1);
    }
    assert!(registry.noop_cooldown_remaining(6918, Utc::now()).is_none());
    assert!(!registry.noop_cooldown_issues(Utc::now()).contains(&6918));
    assert!(registry
        .dispatch(&SweepKind::Issue(6918), None, None, None, None)
        .is_ok());
}

/// AC (#6917 edge case): when an issue is BOTH parked (`loom:blocked`) AND
/// mid-cooldown, the earlier 2.7 park-label guard must still win — guard
/// ordering is unaffected by adding the 2.75 noop-cooldown guard after it.
/// Uses the `park_guard_registry` fixture (label flips enabled) so 2.7
/// actually runs; the noop-cooldown record proves the 2.75 guard would
/// ALSO refuse if reached, so this specifically pins that 2.7 fires first.
#[test]
#[serial]
fn park_guard_fires_before_noop_cooldown_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "", false);
    reg.record_noop_release(4450, Some("still blocked".into()));

    let err = reg
        .dispatch(&SweepKind::Issue(4450), None, None, None, None)
        .expect_err("a parked + cooling-down issue must still be refused");
    let typed = err
        .downcast_ref::<ParkedIssueDispatchError>()
        .expect("the 2.7 park-label guard must win, not the 2.75 noop-cooldown guard");
    assert_eq!(typed.issue, 4450);

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    std::env::remove_var("LOOM_REPO");
}

/// Regression for the #4485 incident shape: an **account-exhaustion**
/// insta-crash is deliberately NOT charged to the issue's quarantine tally
/// (#4122), so three in a row never quarantine — yet before this change the
/// work finder re-dispatched the issue on the very next tick, flapping
/// `loom:issue`/`loom:building` indefinitely. The dispatch backoff must
/// count those deaths and refuse the re-dispatch.
#[test]
fn exhaustion_insta_crashes_arm_backoff_even_though_quarantine_is_carved_out() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");

    for seq in 0..3 {
        insert_dead_running_with_log(
            &mut registry,
            4398,
            seq,
            "agent-9",
            "loom-daemon dispatch: start\nClaude: hit your weekly limit\n",
        );
        registry.reap_once();
    }

    // #4122's carve-out is intact — the issue was never blamed.
    assert_eq!(registry.insta_crash_count(4398), 0, "#4122 carve-out preserved");
    assert!(!registry.is_quarantined(4398), "#4122: exhaustion never quarantines the issue");

    // …but the retry cadence is now bounded regardless of blame.
    assert_eq!(registry.dispatch_failure_count(4398), 3);
    assert!(registry
        .dispatch_backoff_remaining(4398, Utc::now())
        .is_some());
    assert!(registry.dispatch_backoff_issues(Utc::now()).contains(&4398));
    let err = registry
        .dispatch(&SweepKind::Issue(4398), None, None, None, None)
        .expect_err("the re-dispatch that used to flap the label must be refused");
    assert!(err.downcast_ref::<DispatchBackoffError>().is_some(), "got: {err}");
}

/// AC (#4689): a child that exits immediately with the token-selection
/// preflight failure must surface as a hard `Err` from `dispatch()` —
/// never a `DispatchOutcome` (which `mcp__loom__dispatch_sweep` would
/// render as `Success`, `Token: unknown`) — AND the claim taken before
/// the child was spawned (the `loom:issue` -> `loom:building` label flip,
/// the claim lock) must be fully reverted, so the issue is exactly as
/// dispatchable as before this call.
#[test]
#[serial]
fn dispatch_fails_fast_and_reverts_claim_on_immediate_token_selection_failure() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = token_selection_failure_registry(ws);

    let err = reg
        .dispatch(&SweepKind::Issue(4689), None, None, None, None)
        .expect_err("an immediate token-selection death must fail dispatch, not Ok");
    assert!(
        err.to_string().contains("token selection failed"),
        "error must name the real cause, not a generic spawn failure; got: {err}"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 4689 --remove-label loom:issue --add-label loom:building"),
        "the claim WAS taken (label flip happened before the spawn) — got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 4689 --remove-label loom:building --add-label loom:issue"),
        "…and must be REVERTED on the synchronous failure path — got: {calls:?}"
    );

    // No phantom `Running` entry, no leaked lock, no retained child handle.
    assert!(
        running_issue_sweep_id(&reg, 4689).is_none(),
        "a failed dispatch must not leave a Running entry behind"
    );
    assert!(
        !ws.join(".loom/locks/issues/4689").exists(),
        "the claim lock must be released on the synchronous failure path"
    );
    assert!(reg.children.is_empty(), "no child handle should be retained for a dead child");
}

// ------------------------------------------------------------------------
// Cross-issue empty-pool dispatch brake (Issue #6614)
// ------------------------------------------------------------------------

/// AC (#6614): N consecutive token-selection deaths across **different**
/// issues pause dispatch fleet-wide with one loud signal — a state
/// distinct from, and additional to, the per-issue #4485 backoff each of
/// those dispatches also arms.
///
/// End-to-end through the real `dispatch()` path (fake `gh` + a fake
/// `spawn-claude.sh` that reproduces the exit-78 token-selection death),
/// because the gap this closes is precisely that the synchronous #4689
/// branch returns before any entry exists for the reaper to classify —
/// a test that drove `reap_once` instead would pass against the broken
/// pre-#6614 code.
#[test]
#[serial]
fn distinct_issue_token_selection_deaths_pause_dispatch_beyond_per_issue_backoff() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, _gh_log) = token_selection_failure_registry(ws);

    // Two different issues die at token selection. Each arms its OWN
    // per-issue backoff — but two is below the distinct-issue threshold,
    // so the fleet keeps dispatching (the "don't over-trigger" half).
    for issue in [66141_u32, 66142] {
        let err = reg
            .dispatch(&SweepKind::Issue(issue), None, None, None, None)
            .expect_err("an immediate token-selection death must fail dispatch");
        assert!(
            err.downcast_ref::<TokenSelectionDispatchError>().is_some(),
            "the failure must be the typed empty-pool error, not a stringly-typed one; \
                 got: {err}"
        );
        assert_eq!(
            reg.dispatch_failure_count(issue),
            1,
            "issue #{issue}'s own #4485 backoff must be armed by the synchronous path too"
        );
    }
    assert_eq!(reg.token_selection_failure_count(Utc::now()), 2);
    assert!(
        !reg.empty_pool_breaker_tripped(Utc::now()),
        "two distinct issues is below the threshold — the fleet must keep dispatching"
    );
    assert!(
        !reg.preflight_advisory().0,
        "no fleet-wide pause before the threshold is crossed"
    );

    // The third DIFFERENT issue crosses the threshold.
    let err = reg
        .dispatch(&SweepKind::Issue(66143), None, None, None, None)
        .expect_err("an immediate token-selection death must fail dispatch");
    assert!(err.downcast_ref::<TokenSelectionDispatchError>().is_some(), "got: {err}");

    assert_eq!(reg.token_selection_failure_count(Utc::now()), 3);
    assert!(reg.empty_pool_breaker_tripped(Utc::now()));

    // The pause is a WORKSPACE-level state — not merely three per-issue
    // backoffs — and it is the one the work-finder's #5030 hold reads.
    let (tripped, message) = reg.preflight_advisory();
    assert!(tripped, "the fleet-wide advisory must be tripped");
    let message = message.expect("a tripped advisory carries an operator-facing message");
    assert!(
        message.contains("token selection") && message.contains(".bad_tokens"),
        "the signal must name the empty-pool cause and the remedy; got: {message}"
    );
    assert_eq!(
        reg.preflight_dispatch_gate(Utc::now()),
        crate::sweep_registry::PreflightDispatchGate::Held,
        "new dispatch to this workspace must be HELD, not merely per-issue backed off"
    );
    // …and it is genuinely distinct from the per-issue backoff: a FOURTH,
    // never-seen issue has no backoff of its own, yet is covered by the
    // hold above.
    assert_eq!(reg.dispatch_failure_count(66144), 0);
    assert!(!reg.dispatch_backoff_issues(Utc::now()).contains(&66144));
}

/// AC (#6614), the anti-over-trigger case: ONE issue failing repeatedly —
/// the shape its own #4485 backoff already governs — must never pause the
/// fleet, however many times it fails. The counter is distinct issues, not
/// failures.
#[test]
fn one_issue_failing_repeatedly_never_trips_the_fleet_pause() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    for _ in 0..10 {
        registry.record_token_selection_failure(6614);
    }

    assert_eq!(
        registry.token_selection_failure_count(Utc::now()),
        1,
        "ten failures on ONE issue is still one distinct issue"
    );
    assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
    assert!(
        !registry.preflight_advisory().0,
        "one struggling issue must not pause the whole fleet (#6614 over-trigger guard)"
    );
}

/// AC (#6614): a dispatch that gets past token selection clears the brake
/// and releases the hold — no operator action, mirroring how the #5030
/// half-open probe already clears the pre-flight advisory.
#[test]
fn a_dispatch_past_token_selection_clears_the_pause() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    for issue in [1_u32, 2, 3] {
        registry.record_token_selection_failure(issue);
    }
    assert!(registry.empty_pool_breaker_tripped(Utc::now()));
    assert!(registry.preflight_advisory().0);

    assert!(registry.clear_token_selection_failures());

    assert_eq!(registry.token_selection_failure_count(Utc::now()), 0);
    assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
    assert!(!registry.preflight_advisory().0, "the hold must release on the first success");
    assert_eq!(
        registry.preflight_dispatch_gate(Utc::now()),
        crate::sweep_registry::PreflightDispatchGate::Open
    );
    assert!(
        !registry.clear_token_selection_failures(),
        "clearing an already-empty brake is a no-op (no advisory churn)"
    );
}

// ---- role-tick feed into the #6614 brake (Issue #7607) ---------------

/// AC (#7607 proposal item 3): an exhausted pool discovered by *role ticks*
/// trips the same fleet-wide advisory a sweep's token-selection death does.
/// On a host whose work finder is idle, role loops are the only traffic, so
/// before this feed the advisory could never trip no matter how long the
/// pool stayed dry.
#[test]
fn role_ticks_on_an_exhausted_pool_trip_the_same_fleet_pause() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    for role in ["champion", "curator", "judge"] {
        registry.record_role_tick_pool_exhausted(role);
    }

    assert_eq!(registry.token_selection_failure_count(Utc::now()), 3);
    assert!(registry.empty_pool_breaker_tripped(Utc::now()));
    assert!(registry.preflight_advisory().0);
}

/// AC (#7607): the over-trigger guard survives the widened key. ONE role
/// looping on ONE workspace is the role-tick analogue of "one unlucky issue
/// cycling through its own backoff" — it refreshes a single distinct source
/// forever and must never pause the fleet on its own.
#[test]
fn one_role_ticking_repeatedly_never_trips_the_fleet_pause() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    for _ in 0..10 {
        registry.record_role_tick_pool_exhausted("hermit");
    }

    assert_eq!(
        registry.token_selection_failure_count(Utc::now()),
        1,
        "ten skips by ONE role on ONE workspace is still one distinct source"
    );
    assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
    assert!(!registry.preflight_advisory().0);
}

/// AC (#7607): the two feeds share one counter, so a pool that is starving
/// both a sweep and role ticks reaches the threshold on their combined
/// distinct-source count rather than needing N of either kind alone.
#[test]
fn issue_and_role_tick_sources_count_together() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    registry.record_token_selection_failure(7607);
    registry.record_role_tick_pool_exhausted("champion");
    assert!(!registry.empty_pool_breaker_tripped(Utc::now()));

    registry.record_role_tick_pool_exhausted("doctor");
    assert_eq!(registry.token_selection_failure_count(Utc::now()), 3);
    assert!(registry.empty_pool_breaker_tripped(Utc::now()));

    // And the same release path clears both kinds at once: a dispatch that
    // got past token selection is proof the pool works for role ticks too.
    assert!(registry.clear_token_selection_failures());
    assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
}

/// AC (#6614): failures spread further apart than the trailing window are
/// not a systemic fault — they must age out instead of accreting toward a
/// pause over hours. Stale entries are injected directly rather than slept
/// for, so the test is deterministic.
#[test]
fn token_selection_failures_older_than_the_window_stop_counting() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    let stale = Utc::now()
        - chrono::Duration::seconds(DEFAULT_EMPTY_POOL_BREAKER_WINDOW_SECS)
        - chrono::Duration::seconds(60);
    registry
        .token_selection_failures
        .insert(TokenSelectionFailureSource::Issue(11), stale);
    registry
        .token_selection_failures
        .insert(TokenSelectionFailureSource::Issue(22), stale);

    assert_eq!(
        registry.token_selection_failure_count(Utc::now()),
        0,
        "out-of-window failures do not count"
    );

    // A fresh third failure must NOT trip on the back of two stale ones —
    // and must physically prune them.
    registry.record_token_selection_failure(33);
    assert_eq!(registry.token_selection_failure_count(Utc::now()), 1);
    assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
    assert_eq!(registry.token_selection_failures.len(), 1, "stale entries are pruned");
}

/// AC (#5236): when `spawn_child` itself returns `Err` — e.g. a
/// registered workspace whose `spawn_bin` resolves to a nonexistent
/// file, so `Command::spawn()` fails at the OS level before any child
/// ever exists — `dispatch_inner` must unwind the same side effects the
/// #4689 branch above reverts: the claim lock, the `loom:building`
/// label flip, and (implicitly, since nothing was ever advertised
/// without a peer-claim publisher configured in this fixture) the
/// peer-claim advertisement. Without this, the leaked lock's
/// `owner_pid` is this daemon's own (permanently alive, from its own
/// point of view) pid, which the #4556 live-claim guard misreads as a
/// confirmed-live claim on every retry — the exact wedge this issue
/// reports.
#[test]
#[serial]
fn dispatch_reverts_claim_lock_and_label_when_spawn_child_itself_fails() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = spawn_bin_missing_registry(ws);

    let err = reg
        .dispatch(&SweepKind::Issue(5236), None, None, None, None)
        .expect_err("a spawn_child Err must fail dispatch, not Ok");
    assert!(err.to_string().contains("failed to spawn sweep child"), "got: {err}");

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 5236 --remove-label loom:issue --add-label loom:building"),
        "the claim WAS taken (label flip happens before spawn_child) — got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 5236 --remove-label loom:building --add-label loom:issue"),
        "…and must be REVERTED when spawn_child fails — got: {calls:?}"
    );

    assert!(
        running_issue_sweep_id(&reg, 5236).is_none(),
        "a failed dispatch must not leave a Running entry behind"
    );
    assert!(
        !ws.join(".loom/locks/issue-5236").exists(),
        "the claim lock must be released when spawn_child fails, not leaked (#5236)"
    );
    assert!(
        reg.children.is_empty(),
        "no child handle exists — spawn_child never got that far"
    );
}

/// The full repro from this issue's Test Plan: dispatch twice against a
/// workspace whose `spawn_bin` never resolves to anything runnable. The
/// first dispatch fails and — per the AC above — cleans up fully; the
/// second dispatch must reach the SAME `spawn_child` failure again, NOT
/// be refused by the #4556 live-claim guard reading a leaked lock whose
/// `owner_pid` is this daemon's own (still-alive) pid as a confirmed-live
/// claim.
#[test]
#[serial]
fn second_dispatch_after_spawn_child_failure_is_not_refused_by_the_live_claim_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, _gh_log) = spawn_bin_missing_registry(ws);

    let first = reg
        .dispatch(&SweepKind::Issue(5236), None, None, None, None)
        .expect_err("first dispatch must fail — spawn_bin does not exist");
    assert!(
        first.downcast_ref::<LiveClaimDispatchError>().is_none(),
        "the FIRST dispatch must fail on the spawn error itself, not a live-claim refusal \
             (there is nothing to collide with yet) — got: {first}"
    );

    let second = reg
        .dispatch(&SweepKind::Issue(5236), None, None, None, None)
        .expect_err("second dispatch must also fail — spawn_bin still does not exist");
    assert!(
            second.downcast_ref::<LiveClaimDispatchError>().is_none(),
            "the SECOND dispatch must reach the same spawn_child failure again, not be wedged \
             behind a #4556 live-claim refusal from the first attempt's leaked lock — got: {second}"
        );
    assert!(second.to_string().contains("failed to spawn sweep child"), "got: {second}");
}

/// Issue #6615: pins the exact crash window this issue reports — a
/// daemon that dies AFTER `begin_issue_dispatch` completes (claim lock
/// taken, `loom:building` label flipped, child spawned) but BEFORE
/// `finish_issue_dispatch` runs (which is where the sweep journal entry
/// is written, `dispatch.rs`'s `sweep_journal::record_sweep_at` call).
/// Reproduced deterministically by simply not calling
/// `finish_issue_dispatch` — no threads, no timing dependence — exactly
/// like `same_key_retry_during_the_unlocked_poll_window_is_refused_not_double_spawned`
/// reproduces the same window for its own (different) assertion.
///
/// This is a CHARACTERIZATION test: it pins that the gap exists (the
/// label is flipped, but no journal entry and no registered entry exist
/// yet) so a future change to `begin_issue_dispatch`/`finish_issue_dispatch`
/// cannot silently close or reopen this window without a test noticing.
/// The actual recovery is `claim_reconciliation`'s startup-only immediate
/// reclaim (`ReclaimReason::NoRecordAtStartup`) — this test does not
/// duplicate that coverage, it only pins the shape of evidence the crash
/// leaves behind for that pass to act on.
#[test]
#[serial]
fn crash_between_label_flip_and_journal_write_leaves_label_flipped_with_no_journal_entry() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    std::env::remove_var("LOOM_REPO");
    let (mut registry, gh_log) = crash_before_finish_registry(ws);

    let begin = registry
        .begin_issue_dispatch(&SweepKind::Issue(6615), None, None, None, None, None)
        .expect("begin_issue_dispatch must succeed — claim, flip, and spawn all work");
    let prepared = match begin {
        BeginIssueDispatch::Spawned(prepared) => prepared,
        BeginIssueDispatch::Done(result) => panic!("expected Spawned, got Done({result:?})"),
    };

    // The label WAS flipped — this is the pre-spawn claim the issue
    // describes, and it is durable forge state that survives the crash
    // this test simulates by stopping here.
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 6615 --remove-label loom:issue --add-label loom:building"),
        "the claim must be taken (label flip happens before spawn) — got: {calls:?}"
    );
    assert!(
        !calls.contains("--remove-label loom:building --add-label loom:issue"),
        "nothing has reverted the claim — the crash is simulated by simply not finishing, \
             not by any Err this registry ever saw — got: {calls:?}"
    );

    // NO journal entry exists yet — `finish_issue_dispatch` (never
    // called here) is the only place `sweep_journal::record_sweep_at`
    // runs.
    let journal_path = registry.config().resolve_journal_path().unwrap();
    let journal = sweep_journal::load(&journal_path);
    let repo = registry.config().workspace_root.display().to_string();
    assert!(
        sweep_journal::find(&journal, &repo, 6615).is_none(),
        "no journal entry must exist before finish_issue_dispatch runs — this IS the #6615 \
             gap claim_reconciliation's startup pass exists to close"
    );

    // NO in-memory Running entry either — `finish_issue_dispatch` is
    // also where `self.entries.insert` happens.
    assert!(
        running_issue_sweep_id(&registry, 6615).is_none(),
        "no Running entry must exist before finish_issue_dispatch runs"
    );

    // Clean up the still-sleeping child so the test process does not
    // leak it — `prepared.child` is a live std::process::Child this test
    // owns and never handed to the registry (finish_issue_dispatch never
    // ran), so it must be reaped directly rather than via the registry.
    let mut prepared = prepared;
    let _ = prepared.child.kill();
    let _ = prepared.child.wait();
}

/// Edge case (#4689): a child that DID log a token selection before
/// exiting with `EX_CONFIG`/78 (should not happen from the real
/// `spawn-claude.sh` — its token-selection step is the very first thing
/// it does, before any CLI/runtime work — but is worth pinning
/// explicitly) must NOT be misclassified as the synchronous
/// "token-selection failed" fast path. `spawn_child`'s guard is keyed on
/// `token_name == UNKNOWN_TOKEN_NAME`, so once a real account name was
/// captured, `immediate_preflight_death` is unconditionally `None` and
/// `dispatch()` returns its normal `Ok(DispatchOutcome)` carrying that
/// token name — the exit-78 death (for whatever *other* reason) is left
/// to flow through the existing async `reap_once`-driven classification
/// exactly as it does today, unchanged by this issue.
#[test]
#[serial]
fn dispatch_succeeds_when_token_was_logged_before_an_exit_78_death() {
    let dir = tempdir().unwrap();
    // Logs a real selection first (so `token_name` is captured), THEN
    // dies with the same exit code and log prose the genuine
    // token-selection preflight failure uses — deliberately an
    // unrealistic combination, to prove the guard reads `token_name`,
    // not the exit code alone.
    let script = "#!/usr/bin/env bash\n\
set -uo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)\" >&2\n\
echo 'ERROR Token selection failed:' >&2\n\
exit 78\n";
    let mut registry = lifecycle_registry(dir.path(), script);

    let outcome = registry
        .dispatch(&SweepKind::Issue(46_890), None, None, None, None)
        .expect(
            "a child that logged a real token selection must dispatch Ok, even if it then \
                 exits 78 for an unrelated reason — only the no-selection-logged shape is the \
                 #4689 fast-fail case",
        );
    assert_eq!(
        outcome.token_name, "agent3-2amlogic",
        "the captured selection must be preserved, not overwritten by the exit-78 guard"
    );
}

/// The same property for the OTHER quarantine carve-out: a claude-wrapper
/// pre-flight death (#4386) leaves the insta-crash tally untouched but must
/// still bound its own re-dispatch cadence.
#[test]
fn preflight_death_arms_backoff_even_though_quarantine_is_carved_out() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    // No `# CLAUDE_CLI_START` in the tail ⇒ classified as a pre-flight death.
    insert_dead_running_with_log(
        &mut registry,
        4399,
        0,
        "agent-9",
        "==== loom-daemon dispatch: sweep-issue-4399-0 ====\nspawn-claude: preflight failed\n",
    );
    registry.reap_once();

    assert_eq!(registry.insta_crash_count(4399), 0, "#4386 carve-out preserved");
    assert_eq!(registry.dispatch_failure_count(4399), 1, "backoff still counts it");
    assert!(registry
        .dispatch_backoff_remaining(4399, Utc::now())
        .is_some());
}

// ------------------------------------------------------------------------
// Durable terminal-outcome journal (Issue #4644)
// ------------------------------------------------------------------------

/// AC: a child that dies at `spawn-claude.sh`'s token-selection step
/// (exit 78, `EX_CONFIG`) is (a) classified with the specific
/// `preflight-token-selection-failed` death_class rather than the generic
/// `preflight-no-cli-start` fallback, (b) still arms the #4485 dispatch
/// backoff exactly like any other pre-flight death (confirming the
/// existing backoff machinery already covers this shape — no extension
/// needed), and (c) is written to the durable outcomes journal with the
/// exit code, death_class, token_name, and duration a post-hoc reader
/// needs, all without reading log prose.
#[test]
fn exit_78_token_selection_death_is_classified_backed_off_and_journaled() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    // The exact prose `defaults/scripts/spawn-claude.sh` logs immediately
    // before `exit 78` when `loom-daemon tokens select` itself fails
    // (typically because every account is exhausted/blocked) — no
    // `# CLAUDE_CLI_START` anywhere in the tail, because the CLI was
    // never reached.
    insert_dead_running_with_log(
        &mut registry,
        4644,
        0,
        "agent-9",
        "==== loom-daemon dispatch: sweep-issue-4644-0 ====\n\
             [2026-07-30T00:00:00Z] ERROR Token selection failed:\n\
             [2026-07-30T00:00:00Z] ERROR Run 'loom-daemon tokens bootstrap' to populate \
             <repo>/.loom/tokens/,\n",
    );
    registry.reap_once();

    // (a) Specific death_class, not the generic fallback.
    let path = registry.config().resolve_outcomes_journal_path();
    let records = sweep_outcomes::read_all(&path);
    let record = records
        .iter()
        .find(|r| r.issue == 4644)
        .expect("terminal outcome must be journaled");
    assert_eq!(
        record.death_class.as_deref(),
        Some("preflight-token-selection-failed"),
        "exit-78 token-selection death must carry the specific #4644 death_class"
    );
    assert_eq!(record.token_name, "agent-9");
    assert!(record.duration_sec >= 0);
    assert_eq!(record.sweep_id, "sweep-issue-4644-0");

    // (b) #4485 backoff already covers this shape — confirmed, not
    // re-implemented.
    assert_eq!(registry.insta_crash_count(4644), 0, "#4386 carve-out preserved");
    assert_eq!(registry.dispatch_failure_count(4644), 1, "backoff still counts it");
    assert!(registry
        .dispatch_backoff_remaining(4644, Utc::now())
        .is_some());
}

/// Issue #5697 (daemon-path half of #5687): a sweep killed by per-model
/// credit exhaustion is journaled with a `crash_classification` distinct
/// from a plan/quota (`TOKEN_EXHAUSTED`-family) exhaustion — the AC this
/// issue exists to satisfy — while `death_class` (the UNRELATED
/// pre-flight-workspace-tripwire classifier) correctly stays `None`:
/// account exhaustion is deliberately excluded from that classifier
/// (`classify_preflight_outcome`'s `Unknown` arm), so this exercises the
/// two classifiers' independence, not just the new label.
#[test]
fn credit_exhaustion_death_is_journaled_with_a_distinct_crash_classification() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    insert_dead_running_with_log(
        &mut registry,
        5697,
        0,
        "agent-1",
        "==== loom-daemon dispatch: sweep-issue-5697-0 ====\n\
             # CLAUDE_CLI_START\n\
             Claude: You're out of usage credits for this model.\n",
    );
    registry.reap_once();

    let path = registry.config().resolve_outcomes_journal_path();
    let records = sweep_outcomes::read_all(&path);
    let record = records
        .iter()
        .find(|r| r.issue == 5697)
        .expect("terminal outcome must be journaled");
    assert_eq!(
        record.crash_classification.as_deref(),
        Some("account-exhausted:model-credits-exhausted"),
        "credit exhaustion must carry its own crash_classification, distinct from \
             account-exhausted:rate-limited"
    );
    assert_eq!(
        record.death_class, None,
        "account exhaustion is neutral to the UNRELATED pre-flight-death classifier"
    );
}

/// A cold streak restarts at 1: a failure whose predecessor is older than
/// `max` is a fresh incident, not the continuation of an old one — so an
/// issue that fails once in a blue moon never accretes a long backoff.
#[test]
fn stale_failure_streak_restarts_from_one() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 120);

    registry.record_dispatch_failure(31);
    registry.record_dispatch_failure(31);
    assert_eq!(registry.dispatch_failure_count(31), 2);

    // Age the recorded failure well past `max` (120s) and record again.
    let stale = Utc::now() - chrono::Duration::seconds(600);
    if let Some(state) = registry.dispatch_backoff.get_mut(&31) {
        state.last_failure_at = stale;
        state.until = stale;
    }
    registry.record_dispatch_failure(31);
    assert_eq!(registry.dispatch_failure_count(31), 1, "cold streak restarts");
}

/// An elapsed window reads as "no backoff" without any explicit expiry pass —
/// the check is purely `until > now`, so a stale record can never hold an
/// issue back (and a daemon restart clears the map entirely).
#[test]
fn elapsed_backoff_window_stops_refusing() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    registry.record_dispatch_failure(32);
    if let Some(state) = registry.dispatch_backoff.get_mut(&32) {
        state.until = Utc::now() - chrono::Duration::seconds(1);
    }
    assert!(registry
        .dispatch_backoff_remaining(32, Utc::now())
        .is_none());
    assert!(registry.dispatch_backoff_issues(Utc::now()).is_empty());
    assert!(registry
        .dispatch(&SweepKind::Issue(32), None, None, None, None)
        .is_ok());
}

/// A run that made real progress clears the window immediately, so a
/// recovering issue is never held back by an earlier failure.
#[test]
fn checkpoint_progress_clears_the_backoff_window() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    registry.record_dispatch_failure(33);
    assert_eq!(registry.dispatch_failure_count(33), 1);

    // A dead run whose checkpoint was (re)written by THIS run = progress.
    let started_at = Utc::now() - chrono::Duration::seconds(5);
    insert_dead_running_at(&mut registry, 33, 9, started_at);
    let checkpoint_dir = registry.config.checkpoint_dir();
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::write(checkpoint_dir.join("issue-33.json"), r#"{"phase":"builder-done"}"#).unwrap();
    registry.reap_once();

    assert_eq!(registry.dispatch_failure_count(33), 0, "progress clears the streak");
    assert!(registry
        .dispatch_backoff_remaining(33, Utc::now())
        .is_none());
}

/// A **slow** checkpoint-less death does NOT arm the backoff: that shape is
/// the mid-build-death (#3895) / review-stall (#3910) watchdogs' remit, each
/// already bounded to one retry per issue, and arming a window there would
/// risk a refusal burning that single allowed attempt.
#[test]
fn slow_checkpoint_less_death_does_not_arm_the_backoff() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    // `insta_crash_secs` defaults to 60s: start the run well outside it.
    let started_at = Utc::now() - chrono::Duration::seconds(600);
    insert_dead_running_at(&mut registry, 36, 0, started_at);
    registry.reap_once();

    assert_eq!(
        registry.dispatch_failure_count(36),
        0,
        "a slow death is the watchdogs' remit, not the flap breaker's"
    );
    assert!(registry
        .dispatch_backoff_remaining(36, Utc::now())
        .is_none());
}

/// Flap detection: this registry's own label writes are counted per issue and
/// warn once the trailing window holds `DEFAULT_FLAP_THRESHOLD` of them. Below
/// the threshold nothing is flagged (a healthy dispatch writes exactly 2).
#[test]
fn label_flip_flap_detector_flags_only_above_threshold() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());

    for _ in 0..(DEFAULT_FLAP_THRESHOLD - 1) {
        registry.note_label_flip(4398);
    }
    assert!(
        !registry.flap_warned_at.contains_key(&4398),
        "below threshold: a normal dispatch/complete rhythm never warns"
    );

    registry.note_label_flip(4398);
    assert!(
        registry.flap_warned_at.contains_key(&4398),
        "at threshold: the flap is surfaced in the daemon log"
    );
    let first_warn = registry.flap_warned_at[&4398];

    // A sustained flap warns at most once per window (no log spam).
    registry.note_label_flip(4398);
    assert_eq!(registry.flap_warned_at[&4398], first_warn);

    // Flips outside the trailing window are pruned, so an issue that flipped
    // long ago does not carry stale credit toward the threshold.
    let stale = Utc::now() - chrono::Duration::seconds(DEFAULT_FLAP_WINDOW_SECS + 60);
    registry
        .label_flip_log
        .insert(1234, std::iter::repeat_n(stale, DEFAULT_FLAP_THRESHOLD * 2).collect());
    registry.note_label_flip(1234);
    assert!(!registry.flap_warned_at.contains_key(&1234), "stale flips are pruned");
}

/// Config resolution honors precedence env > config > default (#4485), and a
/// `maxSecs` below `baseSecs` is clamped up rather than inverting the curve.
#[test]
#[serial]
fn resolve_dispatch_backoff_config_env_overrides() {
    let dir = tempdir().unwrap();
    for var in [
        DISPATCH_BACKOFF_ENABLE_ENV,
        DISPATCH_BACKOFF_BASE_ENV,
        DISPATCH_BACKOFF_MAX_ENV,
    ] {
        std::env::remove_var(var);
    }

    let base = resolve_dispatch_backoff_config(dir.path());
    assert!(base.enabled, "defaults ON — it is a safety backstop");
    assert_eq!(base.base, Duration::from_secs(DEFAULT_DISPATCH_BACKOFF_BASE_SECS));
    assert_eq!(base.max, Duration::from_secs(DEFAULT_DISPATCH_BACKOFF_MAX_SECS));

    std::env::set_var(DISPATCH_BACKOFF_ENABLE_ENV, "off");
    std::env::set_var(DISPATCH_BACKOFF_BASE_ENV, "30");
    std::env::set_var(DISPATCH_BACKOFF_MAX_ENV, "10");
    let resolved = resolve_dispatch_backoff_config(dir.path());
    for var in [
        DISPATCH_BACKOFF_ENABLE_ENV,
        DISPATCH_BACKOFF_BASE_ENV,
        DISPATCH_BACKOFF_MAX_ENV,
    ] {
        std::env::remove_var(var);
    }
    assert!(!resolved.enabled, "LOOM_DISPATCH_BACKOFF=off disables");
    assert_eq!(resolved.base, Duration::from_secs(30));
    assert_eq!(resolved.max, Duration::from_secs(30), "max clamped up to base");
}

/// Config-file parsing of `autonomous.workFinder.dispatchBackoff` (#4485),
/// including the all-`None` (absent block) case that preserves env/default
/// resolution for repos that never configure it.
#[test]
#[serial]
fn read_dispatch_backoff_file_config_parses_block() {
    for var in [
        DISPATCH_BACKOFF_ENABLE_ENV,
        DISPATCH_BACKOFF_BASE_ENV,
        DISPATCH_BACKOFF_MAX_ENV,
    ] {
        std::env::remove_var(var);
    }

    let dir = tempdir().unwrap();
    let loom = dir.path().join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    std::fs::write(
            loom.join("config.json"),
            r#"{"autonomous":{"workFinder":{"dispatchBackoff":{"enabled":false,"baseSecs":15,"maxSecs":300}}}}"#,
        )
        .unwrap();

    let file = read_dispatch_backoff_file_config(dir.path());
    assert_eq!(file.enabled, Some(false));
    assert_eq!(file.base_secs, Some(15));
    assert_eq!(file.max_secs, Some(300));

    let resolved = resolve_dispatch_backoff_config(dir.path());
    assert!(!resolved.enabled);
    assert_eq!(resolved.base, Duration::from_secs(15));
    assert_eq!(resolved.max, Duration::from_secs(300));

    let empty = tempdir().unwrap();
    let absent = read_dispatch_backoff_file_config(empty.path());
    assert_eq!(absent, DispatchBackoffFileConfig::default());
}

/// The headline #4556 guard: a lock whose owner PID is **live** refuses a
/// dispatch with the typed [`LiveClaimDispatchError`], before any lock or
/// label write happens.
#[test]
fn dispatch_refuses_when_the_claim_lock_owner_is_live() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep = FakeSweep::spawn(4556);
    write_lock_owner(&registry, 4556, "sweep-issue-4556-live", sweep.pid());

    let err = registry
        .dispatch(&SweepKind::Issue(4556), None, None, None, None)
        .unwrap_err();
    let typed = err
        .downcast_ref::<LiveClaimDispatchError>()
        .expect("a live claim lock must surface the typed #4556 refusal");
    assert_eq!(typed.issue, 4556);
    assert!(matches!(typed.evidence, crate::live_claim::LiveClaimEvidence::ClaimLock { .. }));
    assert!(registry.entries.is_empty(), "a refused dispatch must not record an entry");
}

/// The gap #4463's ownership-checked *release* could not close: the lock is
/// already gone (a watchdog / reaper released it on a false-dead verdict) but
/// the sweep process is still alive. The machine-level journal survives that
/// release, so the guard still refuses.
#[test]
fn dispatch_refuses_when_the_journal_records_a_live_sweep_and_the_lock_is_gone() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    assert!(
        !registry.config.locks_dir().join("issue-4557").exists(),
        "precondition: no lock — the released-lock state this guard covers"
    );
    let sweep = FakeSweep::spawn(4557);
    write_journal_entry(&registry, &dir.path().display().to_string(), 4557, sweep.pid());

    let err = registry
        .dispatch(&SweepKind::Issue(4557), None, None, None, None)
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<LiveClaimDispatchError>()
            .map(|e| &e.evidence),
        Some(crate::live_claim::LiveClaimEvidence::Journal { .. })
    ));
}

/// A daemon rooted at `<repo>/.loom/worktrees/issue-N` — the stray
/// debug-build instance that produced 3 of #4275's 7 dispatches — records
/// its claims in the same machine-level journal under a *nested* repo path.
/// The parent checkout's daemon must see that claim.
#[test]
fn dispatch_refuses_when_a_nested_worktree_daemon_holds_the_claim() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let nested = dir
        .path()
        .join(".loom")
        .join("worktrees")
        .join("issue-4385")
        .display()
        .to_string();
    let sweep = FakeSweep::spawn(4558);
    write_journal_entry(&registry, &nested, 4558, sweep.pid());

    let err = registry
        .dispatch(&SweepKind::Issue(4558), None, None, None, None)
        .unwrap_err();
    assert!(
        err.downcast_ref::<LiveClaimDispatchError>().is_some(),
        "a nested-worktree daemon's live claim must refuse the parent's dispatch; got: {err}"
    );
}

/// Test Plan item 2 / the issue's headline acceptance criterion: **N**
/// dispatch requests for one issue inside a bounded window produce exactly
/// **one** live sweep. Here the live sweep is already running (its claim
/// proven by the journal) and every one of the next five requests — the
/// shape of #4275's storm, where the work-finder, the reconciler, and two
/// watchdogs each fired — is refused without a lock, a label write, or an
/// entry.
#[test]
fn n_dispatch_requests_for_a_live_issue_produce_zero_extra_sweeps() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep = FakeSweep::spawn(4559);
    write_journal_entry(&registry, &dir.path().display().to_string(), 4559, sweep.pid());

    for attempt in 1..=5 {
        let err = registry
            .dispatch(&SweepKind::Issue(4559), None, None, None, None)
            .unwrap_err();
        assert!(
            err.downcast_ref::<LiveClaimDispatchError>().is_some(),
            "attempt {attempt} must be refused by the live-claim guard; got: {err}"
        );
    }
    assert!(registry.entries.is_empty(), "no duplicate sweep may be recorded");
    assert!(
        !registry.config.locks_dir().join("issue-4559").exists(),
        "a refusal must cost no lock write"
    );
}

/// A dead recorded PID must NOT refuse — the guard fails open so a stale
/// journal entry or an abandoned lock can never wedge an issue.
#[test]
fn dispatch_proceeds_when_every_recorded_claim_pid_is_dead() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    // PID 0 is always treated as dead by `is_pid_alive`.
    write_journal_entry(&registry, &dir.path().display().to_string(), 4560, 0);

    assert!(
        registry.live_claim_evidence(4560).is_none(),
        "a dead journal PID is not live-claim evidence"
    );
    let outcome = registry
        .dispatch(&SweepKind::Issue(4560), None, None, None, None)
        .expect("a dead recorded claim must not block a legitimate dispatch");
    assert!(outcome.was_new);
}

/// A journal claim for the SAME issue number in an UNRELATED repo must not
/// refuse: issue numbers are per-repo, so a sibling checkout's #N is a
/// different issue.
#[test]
fn dispatch_ignores_a_live_claim_from_an_unrelated_repo() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep = FakeSweep::spawn(4561);
    write_journal_entry(&registry, "/some/other/checkout", 4561, sweep.pid());

    assert!(registry.live_claim_evidence(4561).is_none());
    assert!(registry
        .dispatch(&SweepKind::Issue(4561), None, None, None, None)
        .is_ok());
}

/// Regression (Test Plan item 2, retained guard): two dispatch requests for
/// the same issue racing in one tick produce exactly ONE sweep — the second
/// `acquire_lock` collides on the atomic mkdir claim and is refused. Already
/// passes today; kept so the cross-instance mutual-exclusion property does
/// not silently regress.
#[test]
fn second_same_issue_dispatch_collides_on_lock() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    registry
        .acquire_lock(4468, "sweep-issue-4468-first")
        .unwrap();
    let err = registry
        .acquire_lock(4468, "sweep-issue-4468-second")
        .expect_err("a second same-issue claim must collide on the lock");
    assert!(
        err.to_string().contains("lock collision"),
        "the second dispatch must be refused with a lock collision; got: {err}"
    );
}

/// AC #3: assert that the spawned child receives a
/// `CLAUDE_CODE_OAUTH_TOKEN` env var that came from `.loom/tokens/`.
/// We achieve this with a fixture tokens dir and a fixture spawn-claude
/// that selects from it. The real `spawn-claude.sh` would invoke the
/// Python selector; here we substitute a thin shell that picks the
/// first token file and exports it, so the test exercises the dispatch
/// path end-to-end without depending on a working Python install.
#[test]
#[serial]
fn dispatch_propagates_oauth_token_from_tokens_dir() {
    let dir = tempdir().unwrap();
    let workspace = dir.path();

    // Build a fixture tokens dir with one token.
    let tokens_dir = workspace.join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens_dir).unwrap();
    let token_value = "sk-ant-oat01-fixture-token-value";
    let token_path = tokens_dir.join("agent-1.token");
    std::fs::write(&token_path, token_value).unwrap();
    let mut perms = std::fs::metadata(&token_path).unwrap().permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&token_path, perms).unwrap();

    // Build a fake spawn-claude that selects the first token file and
    // records the exported CLAUDE_CODE_OAUTH_TOKEN. This is a stand-in
    // for the real wrapper's Python-backed selection — the assertion
    // is that the *registry's* dispatch path produces a child whose
    // OAuth token came from `.loom/tokens/`.
    let scripts_dir = workspace.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    let record_log = workspace.join("oauth-record.log");
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
ws="${{LOOM_WORKSPACE:-{ws}}}"
tokens_dir="$ws/.loom/tokens"
token_file="$(ls "$tokens_dir"/*.token 2>/dev/null | head -n1)"
if [ -z "$token_file" ]; then
  echo "no token files in $tokens_dir" >&2
  exit 78
fi
export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$token_file")"
{{
  echo "TOKEN_SOURCE=$token_file"
  echo "CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_CODE_OAUTH_TOKEN"
  echo "argv: $*"
}} >> "{rec}"
exit 0
"#,
        ws = workspace.display(),
        rec = record_log.display()
    );
    std::fs::write(&fake_bin, script).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();

    let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
    config.spawn_bin = Some(fake_bin);
    config.skip_label_flip = true;
    config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
    let mut registry = SweepRegistry::new(config);

    let outcome = registry
        .dispatch(&SweepKind::Issue(123), None, None, None, None)
        .unwrap();
    assert!(outcome.was_new);

    let needle = format!("CLAUDE_CODE_OAUTH_TOKEN={token_value}");
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains(".loom/tokens/agent-1.token"),
        "expected TOKEN_SOURCE to point at .loom/tokens/; got: {recorded}"
    );
}

/// Issue #4768: the sweep child must receive `LOOM_ROLE=sweep-lifecycle`
/// (the ALREADY-ADMITTED role), not just `LOOM_RUNTIME`. Without this, a
/// Codex-runtime sweep child reaches `spawn-codex.sh`'s mutable-role
/// hook-trust preflight with no role signal at all, which is
/// indistinguishable there from an unrecognized role and silently takes
/// the read-only fallback instead of failing closed.
#[test]
#[serial]
fn dispatch_sets_loom_role_from_admitted_role() {
    let dir = tempdir().unwrap();
    let workspace = dir.path();
    // Installs the runtime-admission fixture (`.loom/roles/builder.json`
    // + `.loom/runtimes/claude.json`, satisfied by the built-in `claude`
    // runtime) AND the `/loom:sweep` command marker the 2.x guards
    // require with `skip_label_flip = false`.
    touch_sweep_command(workspace);

    // A permissive fake `gh` so every dispatch-path guard (closed-issue,
    // open-PR, park-label) passes and dispatch reaches `spawn_child`.
    let fake_gh = workspace.join("fake-gh.sh");
    let gh_script = format!(
        "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        state = state_probe_json("open", false),
    );
    std::fs::write(&fake_gh, &gh_script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();

    // Overwrite the fixture's stub spawn-claude.sh with one that records
    // LOOM_ROLE (and LOOM_RUNTIME, for good measure) to a log file.
    let scripts_dir = workspace.join(".loom").join("scripts");
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    let record_log = workspace.join("role-record.log");
    let script = format!(
        r#"#!/usr/bin/env bash
{{
  printf 'LOOM_ROLE=%s\n' "${{LOOM_ROLE:-unset}}"
  printf 'LOOM_RUNTIME=%s\n' "${{LOOM_RUNTIME:-unset}}"
}} >> "{rec}"
exit 0
"#,
        rec = record_log.display()
    );
    std::fs::write(&fake_bin, script).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_bin) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
    // Bypass the runtime-dispatch seam (`resolve_spawn_bin()` would
    // otherwise find the REAL `defaults/scripts/spawn-worker.sh` and exec
    // through to the real `spawn-claude.sh`): point `spawn_bin` directly
    // at the recording fixture, exactly like every other fixture in this
    // module.
    config.spawn_bin = Some(fake_bin);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
    let mut registry = SweepRegistry::new(config);

    let outcome = registry
        .dispatch(&SweepKind::Issue(4768), None, None, None, None)
        .unwrap();
    assert!(outcome.was_new);

    let recorded = assert_child_wrote(&record_log, "LOOM_ROLE=");
    assert!(
        recorded.contains("LOOM_ROLE=sweep-lifecycle"),
        "expected the admitted role (sweep-lifecycle) in the child env; got: {recorded}"
    );
    assert!(
        recorded.contains("LOOM_RUNTIME=claude"),
        "expected the admitted (built-in) runtime alongside it; got: {recorded}"
    );
}

/// End-to-end: a dispatched sweep whose (fake) `spawn-claude.sh` logs the
/// `using OAuth account '<name>'` line records that account as the registry
/// `token_name` — reported by both `DispatchOutcome` and the stored
/// `SweepInfo` (which `list_sweeps` / `get_sweep_status` read from). This
/// closes the "always unknown" gap (issue #3802). Mirrors the live-dispatch
/// finding: issue #3780 selected account `agent3-2amlogic`.
#[test]
#[serial]
fn dispatch_captures_selected_account_into_token_name() {
    let dir = tempdir().unwrap();
    // A fake wrapper that logs the selection to stderr exactly as the real
    // spawn-claude.sh does, then lingers briefly (mimicking `exec claude`,
    // which keeps running long after the selection is logged). The daemon
    // already captures this stderr into the per-sweep log.
    let script = "#!/usr/bin/env bash\n\
set -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)\" >&2\n\
sleep 0.5\n\
exit 0\n";
    let mut registry = lifecycle_registry(dir.path(), script);

    let outcome = registry
        .dispatch(&SweepKind::Issue(3780), None, None, None, None)
        .unwrap();
    assert!(outcome.was_new);
    assert_eq!(
        outcome.token_name, "agent3-2amlogic",
        "DispatchOutcome should carry the selected account, not 'unknown'"
    );

    let info = registry
        .get_status(&outcome.sweep_id)
        .expect("dispatched sweep should be in the registry");
    assert_eq!(
        info.token_name, "agent3-2amlogic",
        "stored SweepInfo (what list_sweeps/get_sweep_status report) should \
             carry the selected account"
    );
}

/// The `LOOM_SPAWN_NO_EXPORT` bypass path selects no account, so nothing is
/// logged — `token_name` must remain `unknown` (not a regression, the
/// expected "nothing to report" case). Verified here with a fixture that
/// exits without logging a selection: the `try_wait` early-exit means this
/// resolves promptly rather than waiting out the capture timeout.
#[test]
#[serial]
fn dispatch_token_name_unknown_when_no_selection_logged() {
    let dir = tempdir().unwrap();
    let script = "#!/usr/bin/env bash\nset -euo pipefail\nexit 0\n";
    let mut registry = lifecycle_registry(dir.path(), script);

    let outcome = registry
        .dispatch(&SweepKind::Issue(4242), None, None, None, None)
        .unwrap();
    assert_eq!(
        outcome.token_name, UNKNOWN_TOKEN_NAME,
        "no selection logged => token_name stays 'unknown'"
    );
}

// ===================================================================
// Concurrent dispatch does not serialize on the account-selection poll
// (Issue #6592)
// ===================================================================

/// Drives `begin_issue_dispatch` -> `poll_and_classify_spawned_child` ->
/// `finish_issue_dispatch` the same way `ipc.rs`'s
/// `dispatch_sweep_nonblocking` does: lock only for the two brief
/// bookend steps, poll UNLOCKED in between. Proves — not merely asserts
/// — that a burst of concurrent dispatches' polls overlap instead of
/// serializing behind the registry mutex: the fixture spawn script stays
/// alive and logs its account selection only after a deliberate delay,
/// so each dispatch's poll genuinely blocks for that long. If the
/// registry mutex were held across the poll (the pre-#6592 shape, still
/// exercised by the plain `dispatch()` — see
/// `dispatch_captures_selected_account_into_token_name` above), N
/// concurrent dispatches would take N times as long; this test asserts
/// the burst completes in well under that serialized bound.
#[test]
#[serial]
fn concurrent_issue_dispatches_do_not_serialize_on_the_account_selection_poll() {
    let dir = tempdir().unwrap();
    let poll_delay = Duration::from_millis(700);
    let script = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nsleep {:.2}\n\
             echo \"spawn-claude: using OAuth account 'agent-burst' (mode=random)\" >&2\n\
             sleep 5\n",
        poll_delay.as_secs_f64()
    );
    let registry = Arc::new(Mutex::new(lifecycle_registry(dir.path(), &script)));

    const BURST: u32 = 10;
    let start = Instant::now();
    let handles: Vec<std::thread::JoinHandle<DispatchOutcome>> = (0..BURST)
        .map(|i| {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                let begin = {
                    let mut sr = registry.lock().unwrap();
                    sr.begin_issue_dispatch(
                        &SweepKind::Issue(81_000 + i),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .expect("begin_issue_dispatch should succeed for a fresh issue")
                };
                let mut prepared = match begin {
                    BeginIssueDispatch::Spawned(prepared) => prepared,
                    BeginIssueDispatch::Done(result) => {
                        panic!("expected Spawned, got Done({result:?})")
                    }
                };
                // The genuinely multi-second wait — deliberately run
                // WITHOUT the registry mutex held, mirroring
                // `dispatch_sweep_nonblocking`'s unlocked phase.
                let (token_name, runtime, immediate_preflight_death) =
                    poll_and_classify_spawned_child(
                        &mut prepared.child,
                        &prepared.log_path,
                        &prepared.header_anchor,
                    );
                let mut sr = registry.lock().unwrap();
                sr.finish_issue_dispatch(*prepared, token_name, runtime, immediate_preflight_death)
                    .expect("finish_issue_dispatch should succeed")
            })
        })
        .collect();

    let outcomes: Vec<DispatchOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("dispatch thread panicked"))
        .collect();
    let elapsed = start.elapsed();

    assert_eq!(outcomes.len(), BURST as usize);
    for outcome in &outcomes {
        assert_eq!(
            outcome.token_name, "agent-burst",
            "every dispatch in the burst should have captured the fixture's selection"
        );
    }

    // Serialized (pre-#6592: registry mutex held across the poll) would
    // take roughly BURST * poll_delay (7s for 10x700ms). Concurrent
    // (post-#6592) should complete close to ONE poll_delay plus
    // guard-chain/spawn overhead. Assert well under the serialized
    // bound — and well under the 30s client ack deadline (AC2) this
    // issue targets.
    let serialized_bound = poll_delay * BURST;
    assert!(
            elapsed < serialized_bound / 2,
            "burst of {BURST} concurrent dispatches took {elapsed:?} (poll_delay={poll_delay:?}) \
             — looks serialized behind the registry mutex (serialized bound ~{serialized_bound:?}), \
             not concurrent"
        );
    assert!(
        elapsed < Duration::from_secs(30),
        "burst took {elapsed:?}, at or over the 30s client ack deadline this issue targets"
    );

    // Best-effort cleanup — don't leak the fixture's long-lived children.
    for outcome in &outcomes {
        let mut sr = registry.lock().unwrap();
        let _ = sr.cancel(&outcome.sweep_id, Duration::from_millis(50));
    }
}

// ===================================================================
// Work-finder dispatch does not hold the registry lock across the
// account-selection poll (Issue #6688)
// ===================================================================

/// Proves the #6688 fix at the level `RegistryDispatcher::dispatch`
/// actually calls: [`dispatch_issue_releasing_poll_lock`] — the entry
/// point extending #6592's begin/poll/finish split to the work-finder's
/// synchronous (non-IPC) dispatch call site.
///
/// Mirrors `concurrent_issue_dispatches_do_not_serialize_on_the_account_selection_poll`
/// above, but drives the actual production entry point directly (instead
/// of re-implementing the split inline) and — matching this issue's own
/// Test Plan — asserts the more targeted property the issue is about: a
/// concurrent **status-style read** (a plain `registry.lock()` +
/// `list(None)`, standing in for `build_daemon_status`'s per-root
/// `registry.lock()`) is not blocked for the duration of the dispatch's
/// account-selection poll. Before the #6688 split,
/// `RegistryDispatcher::dispatch` called `SweepRegistry::dispatch` ->
/// `dispatch_inner`, which (by design, see its doc comment) holds the
/// registry mutex across the *entire* poll — so this status-style read
/// would have queued behind it for the poll's full duration.
#[test]
#[serial]
fn concurrent_status_read_is_not_blocked_behind_work_finder_dispatch_poll() {
    let dir = tempdir().unwrap();
    let poll_delay = Duration::from_millis(700);
    let script = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nsleep {:.2}\n\
             echo \"spawn-claude: using OAuth account 'agent-6688' (mode=random)\" >&2\n\
             sleep 5\n",
        poll_delay.as_secs_f64()
    );
    let registry = Arc::new(Mutex::new(lifecycle_registry(dir.path(), &script)));

    // Kick off the dispatch (which will be mid-poll, holding NO lock,
    // for ~poll_delay) on its own thread — exactly the shape
    // `RegistryDispatcher::dispatch` runs it in (a synchronous call with
    // no `.await` of its own).
    let dispatch_registry = Arc::clone(&registry);
    let dispatch_handle = std::thread::spawn(move || {
        dispatch_issue_releasing_poll_lock(
            &dispatch_registry,
            &SweepKind::Issue(83_000),
            Some("statusread-6688".to_string()),
            None,
            None,
            None,
        )
        .expect("dispatch_issue_releasing_poll_lock should succeed")
    });

    // Give the dispatch a moment to get past `begin_issue_dispatch`
    // (guard chain + spawn) and into the unlocked poll before racing a
    // status-style read against it.
    std::thread::sleep(Duration::from_millis(150));

    let read_registry = Arc::clone(&registry);
    let read_start = Instant::now();
    let read_handle = std::thread::spawn(move || {
        let sr = read_registry.lock().unwrap();
        let _ = sr.list(None);
    });
    read_handle
        .join()
        .expect("status-style read thread panicked");
    let read_elapsed = read_start.elapsed();

    assert!(
        read_elapsed < poll_delay / 2,
        "a concurrent status-style read took {read_elapsed:?} — should complete in well \
             under the {poll_delay:?} account-selection poll delay if the dispatch released \
             the registry lock for the poll (Issue #6688); looks blocked behind it instead"
    );

    let outcome = dispatch_handle.join().expect("dispatch thread panicked");
    assert!(outcome.was_new);
    assert_eq!(outcome.token_name, "agent-6688");

    let mut sr = registry.lock().unwrap();
    let _ = sr.cancel(&outcome.sweep_id, Duration::from_millis(50));
}

/// Regression coverage for the idempotency-key dedup contract this
/// issue's fix must preserve (Issue #6592, test plan item 3):
/// `find_running_by_key` must still short-circuit a same-key retry
/// against a `Running` entry through the SPLIT `begin_issue_dispatch`
/// path, exactly as it always has through `dispatch()` (see
/// `dispatch_idempotency_returns_existing` /
/// `n_dispatch_requests_for_a_live_issue_produce_zero_extra_sweeps`
/// above for the pre-existing `dispatch()`-level coverage of the same
/// contract) — no double-spawn, `was_new: false` on the retry.
#[test]
#[serial]
fn begin_issue_dispatch_idempotency_hit_returns_done_without_spawning() {
    let dir = tempdir().unwrap();
    let script = "#!/usr/bin/env bash\nset -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent-idem' (mode=random)\" >&2\nsleep 5\n";
    let mut registry = lifecycle_registry(dir.path(), script);

    // First dispatch: a real spawn, going through the full begin -> poll
    // -> finish split.
    let begin = registry
        .begin_issue_dispatch(
            &SweepKind::Issue(82_001),
            Some("retry-key-6592".to_string()),
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
    let (token_name, runtime, death) = poll_and_classify_spawned_child(
        &mut prepared.child,
        &prepared.log_path,
        &prepared.header_anchor,
    );
    let first = registry
        .finish_issue_dispatch(*prepared, token_name, runtime, death)
        .unwrap();
    assert!(first.was_new);

    // Second dispatch, SAME idempotency key: must hit the fast
    // `Done(Ok(..))` path — no `Spawned` variant, no second child.
    let retry = registry
        .begin_issue_dispatch(
            &SweepKind::Issue(82_001),
            Some("retry-key-6592".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    match retry {
        BeginIssueDispatch::Done(Ok(outcome)) => {
            assert!(!outcome.was_new, "same-key retry against a Running entry must not spawn");
            assert_eq!(outcome.sweep_id, first.sweep_id);
        }
        BeginIssueDispatch::Done(Err(e)) => panic!("expected an idempotent Ok, got Err: {e}"),
        BeginIssueDispatch::Spawned(_) => {
            panic!("same-key retry against a Running entry must not reach Spawned (double-spawn)")
        }
    }

    let _ = registry.cancel(&first.sweep_id, Duration::from_millis(50));
}

/// Pins the *caller-visible* behavior of a same-idempotency-key retry that
/// lands inside the begin/poll/finish window this issue's lock split opens
/// (Issue #6592, Judge review of PR #6600).
///
/// `find_running_by_key` matches only entries already in `self.entries`,
/// and a dispatch's entry is not inserted until `finish_issue_dispatch`
/// (post-poll, re-locked). So between `begin_issue_dispatch` returning
/// `Spawned` and `finish_issue_dispatch` recording the entry — the window
/// the registry mutex is deliberately released for, bounded by
/// `TOKEN_NAME_CAPTURE_TIMEOUT` (~5s) — a same-key retry MISSES the
/// idempotency short-circuit and falls through to the guard chain.
///
/// **The safety property still holds — no double-spawn** — because the
/// guard chain refuses the retry before any second `Command::spawn()`. Two
/// independent mechanisms can do the refusing, and *which one wins is
/// platform-dependent*, so this test deliberately accepts either:
///
/// 1. The #4556 live-claim guard's process-scan leg
///    (`live_claim::live_sweep_process_in`), which matches the
///    just-spawned child by **argv** and so needs neither a tracked entry
///    nor lock ownership. It fires where the platform exposes another
///    process's argv (Linux `/proc`) — observed refusing this exact retry
///    on CI. The guard's *bookkeeping* legs are indeed blind here
///    (`has_tracked_sweep_for` is still false, and the lock's `owner.json`
///    still carries this daemon's own pid because
///    `record_child_pid_in_lock` runs in `finish_issue_dispatch`) — the
///    argv leg is not.
/// 2. The atomic `acquire_lock` mkdir, which is unconditional and
///    platform-independent — the backstop that refuses the retry with a
///    `lock collision` wherever leg 1 cannot see the child (observed on
///    macOS).
///
/// What *does* differ inside this window is the retry's caller-visible
/// outcome: a hard `Err` either way, not the graceful `was_new: false` a
/// retry gets before or after. This test pins that, so the distinction is
/// a checked behavior rather than a surprise rediscovered later. It is
/// deterministic: the window is reproduced by simply not calling
/// `finish_issue_dispatch` yet — no threads, no timing dependence, and no
/// dependence on which of the two guards happens to fire.
///
/// Contrast `begin_issue_dispatch_idempotency_hit_returns_done_without_spawning`
/// above, which covers a retry *after* completion (the realistic client
/// case: a retry follows a 30s ack timeout, long past the ~5s window).
#[test]
#[serial]
fn same_key_retry_during_the_unlocked_poll_window_is_refused_not_double_spawned() {
    let dir = tempdir().unwrap();
    let script = "#!/usr/bin/env bash\nset -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent-race' (mode=random)\" >&2\nsleep 5\n";
    let mut registry = lifecycle_registry(dir.path(), script);

    // First dispatch: stop right after `begin` returns `Spawned`. The
    // child exists, but no entry has been recorded yet — this IS the
    // unlocked-poll window, reproduced without any timing dependence.
    let begin = registry
        .begin_issue_dispatch(
            &SweepKind::Issue(82_002),
            Some("race-key-6592".to_string()),
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

    // Same-key retry landing INSIDE that window. It misses the
    // idempotency-key short-circuit (no entry to match yet) and must be
    // REFUSED by the guard chain — never a second spawn. Either refusal
    // mechanism is acceptable and both are asserted for explicitly (see
    // this test's doc comment): the #4556 live-claim argv probe where the
    // platform exposes the child's argv, otherwise the atomic
    // `acquire_lock` mkdir.
    let racing = registry.begin_issue_dispatch(
        &SweepKind::Issue(82_002),
        Some("race-key-6592".to_string()),
        None,
        None,
        None,
        None,
    );
    match racing {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("lock collision") || msg.contains("#4556 live-claim guard"),
                "a same-key retry inside the unlocked-poll window must be refused by the \
                     claim lock or the #4556 live-claim guard; got an unrecognized error: {msg}"
            );
        }
        Ok(BeginIssueDispatch::Spawned(_)) => {
            panic!("same-key retry inside the unlocked-poll window must NOT spawn a second child")
        }
        Ok(BeginIssueDispatch::Done(result)) => panic!(
            "same-key retry inside the unlocked-poll window is expected to be refused by the \
                 guard chain, not to short-circuit: Done({result:?})"
        ),
    }

    // Now close the window and confirm it was genuinely transient: once
    // `finish_issue_dispatch` records the entry, the SAME retry gets the
    // graceful idempotent hand-back instead of the lock-collision error.
    let (token_name, runtime, death) = poll_and_classify_spawned_child(
        &mut prepared.child,
        &prepared.log_path,
        &prepared.header_anchor,
    );
    let first = registry
        .finish_issue_dispatch(*prepared, token_name, runtime, death)
        .unwrap();
    assert!(first.was_new);

    let after = registry
        .begin_issue_dispatch(
            &SweepKind::Issue(82_002),
            Some("race-key-6592".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    match after {
        BeginIssueDispatch::Done(Ok(outcome)) => {
            assert!(
                !outcome.was_new,
                "once the entry is recorded, a same-key retry dedups gracefully again"
            );
            assert_eq!(outcome.sweep_id, first.sweep_id);
        }
        BeginIssueDispatch::Done(Err(e)) => {
            panic!("expected a graceful idempotency hit after finish; got Err: {e}")
        }
        BeginIssueDispatch::Spawned(_) => {
            panic!("expected a graceful idempotency hit after finish; got a second spawn")
        }
    }

    let _ = registry.cancel(&first.sweep_id, Duration::from_millis(50));
}

// ===================================================================
// Startup-race mitigation: stagger + watchdog (Issue #3887)
// ===================================================================

// --- stagger_wait pure function ---

#[test]
fn stagger_wait_zero_stagger_never_waits() {
    let now = Instant::now();
    assert_eq!(stagger_wait(None, Duration::ZERO, now), Duration::ZERO);
    assert_eq!(
        stagger_wait(Some(now), Duration::ZERO, now + Duration::from_secs(1)),
        Duration::ZERO
    );
}

#[test]
fn stagger_wait_no_prior_spawn_never_waits() {
    let now = Instant::now();
    assert_eq!(stagger_wait(None, Duration::from_secs(2), now), Duration::ZERO);
}

#[test]
fn stagger_wait_returns_remaining_gap() {
    let base = Instant::now();
    let stagger = Duration::from_millis(2000);
    // 500ms elapsed since the last spawn ⇒ 1500ms still to wait.
    let now = base + Duration::from_millis(500);
    assert_eq!(stagger_wait(Some(base), stagger, now), Duration::from_millis(1500));
}

#[test]
fn stagger_wait_elapsed_past_stagger_is_zero() {
    let base = Instant::now();
    let stagger = Duration::from_millis(2000);
    // 3s elapsed ⇒ the full gap has passed, no wait.
    let now = base + Duration::from_millis(3000);
    assert_eq!(stagger_wait(Some(base), stagger, now), Duration::ZERO);
}

// --- set/get dispatch stagger ---

#[test]
fn dispatch_stagger_setter_roundtrips() {
    let tmp = tempdir().unwrap();
    let (mut reg, _rec) = fixture_registry(tmp.path());
    assert_eq!(reg.dispatch_stagger(), Duration::ZERO, "default is zero");
    reg.set_dispatch_stagger(Duration::from_millis(1500));
    assert_eq!(reg.dispatch_stagger(), Duration::from_millis(1500));
}

#[test]
fn dispatch_applies_configured_stagger_between_spawns() {
    // With a small stagger, two back-to-back dispatches are spaced by at
    // least the stagger (the second waits out the gap in `dispatch`).
    let tmp = tempdir().unwrap();
    let (mut reg, rec) = fixture_registry(tmp.path());
    reg.set_dispatch_stagger(Duration::from_millis(400));

    let start = Instant::now();
    reg.dispatch(&SweepKind::Issue(8001), None, None, None, None)
        .unwrap();
    reg.dispatch(&SweepKind::Issue(8002), None, None, None, None)
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(400),
        "second dispatch should have waited out the stagger; elapsed={elapsed:?}"
    );
    // Both fake children ran. Generous budget (#3985): under host CPU
    // starvation the children can be slow to be scheduled onto the record
    // log, so a tight 5s bound made this red for a host-load reason.
    assert!(wait_for_contents(&rec, "issue=8002", FIXTURE_CHILD_WAIT_MS) || rec.exists());
}

/// AC6: `dispatch` for a closed issue is refused, and it must NOT flip any
/// labels (no `issue edit`) — a watchdog re-dispatch can never re-claim a
/// closed/merged issue.
#[test]
#[serial]
fn dispatch_refuses_closed_issue_without_flipping_labels() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", false), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(4078), None, None, None, None)
        .expect_err("a closed issue must be refused");
    assert!(
        err.to_string().contains("closed"),
        "error explains the closed-issue guard; got: {err}"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4078"),
        "the guard probed issue state over REST; got: {calls:?}"
    );
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    // No lock was acquired and no entry recorded.
    assert!(running_issue_sweep_id(&reg, 4078).is_none());
}

/// #4504 case (b): a dispatch number that resolves to a **merged** pull
/// request is refused. REST reports a merged PR as `state: "closed"` with a
/// `pull_request` key, so this case is caught by BOTH legs of the guard —
/// the point is that it can no longer reach the `_ => None` fail-open arm the
/// way `gh issue view`'s GraphQL `MERGED` state did.
#[test]
#[serial]
fn dispatch_refuses_merged_pr_number_without_flipping_labels() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", true), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(4501), None, None, None, None)
        .expect_err("a merged PR number must be refused");
    assert!(
        err.to_string().contains("pull request"),
        "error names the PR-number case; got: {err}"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4501"),
        "the guard probed the number over REST; got: {calls:?}"
    );
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    assert!(running_issue_sweep_id(&reg, 4501).is_none(), "no lock, no entry");
}

/// #4504 case (c), the load-bearing one: a dispatch number that resolves to
/// an **open** pull request is refused too. Its `state` is `"open"` — byte
/// identical to an open issue's — so only the structural `pull_request`
/// discriminator can catch it. A fix that merely appended `"MERGED"` to the
/// old state-string match would dispatch this happily.
#[test]
#[serial]
fn dispatch_refuses_open_pr_number_without_flipping_labels() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("open", true), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(4502), None, None, None, None)
        .expect_err("an open PR number must be refused");
    assert!(
        err.to_string().contains("pull request"),
        "error names the PR-number case; got: {err}"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4502"),
        "the guard probed the number over REST; got: {calls:?}"
    );
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    assert!(running_issue_sweep_id(&reg, 4502).is_none(), "no lock, no entry");
}

/// #4504 belt-and-suspenders: an Issue-shaped node that reports `MERGED` is
/// terminal exactly like `CLOSED` — it must never fall through to the
/// fail-open arm (the original #4088 bug).
#[test]
#[serial]
fn dispatch_refuses_merged_state_on_issue_shaped_node() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("MERGED", false), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(4505), None, None, None, None)
        .expect_err("a MERGED state must be refused like CLOSED");
    assert!(
        err.to_string().contains("closed"),
        "error explains the closed-issue guard; got: {err}"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    assert!(running_issue_sweep_id(&reg, 4505).is_none(), "no lock, no entry");
}

/// AC6 fail-open: a forge lookup error (non-zero `gh`) must NOT wedge
/// dispatch — the guard returns `None` and dispatch proceeds normally.
#[test]
#[serial]
fn dispatch_fails_open_when_issue_state_lookup_errors() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    // The state probe exits non-zero ⇒ state unknown ⇒ fail open.
    let (mut reg, gh_log) = closed_guard_registry(ws, "", 1);

    let out = reg
        .dispatch(&SweepKind::Issue(4079), None, None, None, None)
        .expect("a gh outage must not wedge dispatch (fail-open)");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4079"),
        "the guard probed issue state; got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 4079"),
        "dispatch proceeded to the label flip after failing open; got: {calls:?}"
    );

    if let Some(id) = running_issue_sweep_id(&reg, 4079) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

/// AC6 fail-open (unparseable): a `gh` that exits 0 but emits output the
/// probe cannot parse into `{state, is_pr}` is a genuine lookup failure, not
/// a verdict — dispatch must proceed.
#[test]
#[serial]
fn dispatch_fails_open_when_issue_state_output_is_unparseable() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, "not json at all", 0);

    let out = reg
        .dispatch(&SweepKind::Issue(4080), None, None, None, None)
        .expect("an unparseable probe answer must not wedge dispatch (fail-open)");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 4080"),
        "dispatch proceeded to the label flip after failing open; got: {calls:?}"
    );

    if let Some(id) = running_issue_sweep_id(&reg, 4080) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

/// `dispatch` for an open issue that already has an open linked PR is refused
/// with the typed [`OpenPrDispatchError`] (downcast-matchable, not string
/// matching), and it must NOT acquire the claim lock or flip any labels — a
/// re-dispatch of already-in-review work would duplicate it.
#[test]
#[serial]
fn dispatch_refuses_open_pr_without_flipping_labels() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = open_pr_guard_registry(ws, "4200", 0, false);

    let err = reg
        .dispatch(&SweepKind::Issue(4123), None, None, None, None)
        .expect_err("an issue with an open linked PR must be refused");
    let typed = err
        .downcast_ref::<OpenPrDispatchError>()
        .expect("refusal must carry the typed OpenPrDispatchError");
    assert_eq!(typed.issue, 4123);
    assert_eq!(typed.pr, 4200);

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(calls.contains("api graphql"), "the guard queried the closes-graph");
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    // No lock acquired, no entry recorded.
    assert!(running_issue_sweep_id(&reg, 4123).is_none());
    std::env::remove_var("LOOM_REPO");
}

/// Issue #6593: the refusal must not be a dead end. The candidate class the
/// open-PR guard refuses is exactly the class `sweep.md`'s aggressive
/// taxonomy says to drive to merge — but the child whose per-issue pre-flight
/// would perform that routing never gets spawned, so the refusal text itself
/// has to name the reachable alternative: a `PrSet` dispatch (#5342) for the
/// very PR the guard found. Assert the rendered `Display` carries a
/// copy-pasteable `kind={"PrSet":[<pr>]}` naming that PR.
#[test]
#[serial]
fn open_pr_refusal_text_names_the_prset_alternative() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = open_pr_guard_registry(ws, "4200", 0, false);

    let err = reg
        .dispatch(&SweepKind::Issue(4123), None, None, None, None)
        .expect_err("an issue with an open linked PR must be refused");
    let rendered = err.to_string();

    assert!(
        rendered.contains(r#"kind={"PrSet":[4200]}"#),
        "the refusal must hand the caller the exact PrSet dispatch for the PR it found \
             (#6593); got: {rendered:?}"
    );
    assert!(
        rendered.contains("#5342"),
        "the refusal must cite the PrSet dispatch support it points at (#5342); got: \
             {rendered:?}"
    );
    // The pre-#6593 diagnosis must survive alongside the new hint.
    assert!(
        rendered.contains("#4123 open-PR guard"),
        "the refusal must still name the guard that fired; got: {rendered:?}"
    );

    // The same text is what a downcast-matching caller renders, too.
    let typed = err
        .downcast_ref::<OpenPrDispatchError>()
        .expect("refusal must still carry the typed OpenPrDispatchError");
    assert_eq!(typed.to_string(), rendered);

    std::env::remove_var("LOOM_REPO");
}

/// Issue #6350 (Ask 1) regression: "host A opens a PR closing issue N;
/// host B's work finder skips N on its next tick (no new lease...)". The
/// open-PR guard's probe is a forge (GraphQL closes-graph) query, not any
/// host-local state, so it refuses identically regardless of which host
/// actually opened the linked PR — this test names that property
/// explicitly, on top of the sibling test above's narrower "no label
/// flip" assertion: a refused dispatch must never even reach
/// `write_lease_comment` (`gh issue comment`), since that call only
/// happens after a CONFIRMED successful label flip.
#[test]
#[serial]
fn dispatch_refuses_open_pr_without_writing_a_lease_comment() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = open_pr_guard_registry(ws, "4200", 0, false);

    let err = reg
        .dispatch(&SweepKind::Issue(4123), None, None, None, None)
        .expect_err("an issue with an open linked PR must be refused, cross-host");
    assert!(err.downcast_ref::<OpenPrDispatchError>().is_some());

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("issue comment"),
        "no lease record may be written for a dispatch the open-PR guard refused; got: \
             {calls:?}"
    );
    std::env::remove_var("LOOM_REPO");
}

/// Fail-open (the single most safety-critical property): a forge error on the
/// open-PR probe (non-zero `gh api graphql`) must NOT wedge dispatch — the
/// guard returns `None` and dispatch proceeds to spawn + label flip.
#[test]
#[serial]
fn dispatch_fails_open_when_open_pr_lookup_errors() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    // `api graphql` exits non-zero ⇒ open-PR state unknown ⇒ fail open.
    let (mut reg, gh_log) = open_pr_guard_registry(ws, "", 1, false);

    let out = reg
        .dispatch(&SweepKind::Issue(4124), None, None, None, None)
        .expect("a forge error on the open-PR probe must not wedge dispatch (fail-open)");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(calls.contains("api graphql"), "the guard attempted the closes-graph query");
    assert!(
        calls.contains("issue edit 4124"),
        "dispatch proceeded to the label flip after failing open; got: {calls:?}"
    );
    if let Some(id) = running_issue_sweep_id(&reg, 4124) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// An issue whose only linked PR is merged/closed is NOT blocked: the
/// `state == "OPEN"` `--jq` filter yields no PR number, so the probe returns
/// nothing and dispatch proceeds. Regression guard against an
/// `includeClosedPrs` misconfiguration that would strand every issue whose
/// PR ever merged.
#[test]
#[serial]
fn dispatch_open_pr_guard_ignores_merged_only_pr() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    // Empty post-`--jq` output = no OPEN-state PR (only merged/closed ones).
    let (mut reg, _gh_log) = open_pr_guard_registry(ws, "", 0, false);

    let out = reg
        .dispatch(&SweepKind::Issue(4125), None, None, None, None)
        .expect("an issue whose only linked PR is merged/closed must not be blocked");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));
    if let Some(id) = running_issue_sweep_id(&reg, 4125) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// `skip_label_flip = true` bypasses the open-PR guard entirely (test-fixture
/// path): even a fake `gh` that WOULD report an open PR is never consulted,
/// and dispatch proceeds.
#[test]
#[serial]
fn dispatch_skip_label_flip_bypasses_open_pr_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = open_pr_guard_registry(ws, "4200", 0, true);

    let out = reg
        .dispatch(&SweepKind::Issue(4126), None, None, None, None)
        .expect("skip_label_flip must bypass the open-PR guard entirely");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("api graphql"),
        "no forge call at all when label flips are disabled; got: {calls:?}"
    );
    if let Some(id) = running_issue_sweep_id(&reg, 4126) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// The 2.5 closed-issue guard (#4088) fires BEFORE the 2.6 open-PR guard: a
/// closed issue (with a merged PR) is refused by 2.5 and the open-PR probe
/// never runs — no regression to the existing closed-issue path.
#[test]
#[serial]
fn closed_issue_guard_fires_before_open_pr_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", false), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(4200), None, None, None, None)
        .expect_err("a closed issue must still be refused by the 2.5 guard");
    assert!(
        err.to_string().contains("closed"),
        "the 2.5 closed-issue guard wins; got: {err}"
    );
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("api graphql"),
        "the open-PR (2.6) probe must never run once 2.5 refuses; got: {calls:?}"
    );
}

/// AC: `dispatch` for an issue carrying `loom:blocked` is refused with the
/// typed [`ParkedIssueDispatchError`], and it must NOT acquire the claim lock
/// or flip any labels — a deliberate park must survive every dispatch route.
/// The probe rides the REST bucket (`gh api repos/.../issues/N`), not
/// GraphQL.
#[test]
#[serial]
fn dispatch_refuses_blocked_issue_without_flipping_labels() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "", false);

    let err = reg
        .dispatch(&SweepKind::Issue(4444), None, None, None, None)
        .expect_err("a parked issue must be refused");
    let typed = err
        .downcast_ref::<ParkedIssueDispatchError>()
        .expect("refusal must carry the typed ParkedIssueDispatchError");
    assert_eq!(typed.issue, 4444);
    assert_eq!(typed.label, "loom:blocked");

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4444 --jq .labels[].name"),
        "the guard must probe labels over REST, not GraphQL; got: {calls:?}"
    );
    assert!(
        !calls.contains("issue edit"),
        "no label flip on a refused dispatch; got: {calls:?}"
    );
    assert!(running_issue_sweep_id(&reg, 4444).is_none(), "no lock, no entry");
    std::env::remove_var("LOOM_REPO");
}

/// AC: `loom:operator-only` is the second park label and refuses identically
/// — the daemon must never dispatch work a human has claimed for themselves.
#[test]
#[serial]
fn dispatch_refuses_operator_only_issue() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = park_guard_registry(ws, "loom:operator-only", 0, "", false);

    let err = reg
        .dispatch(&SweepKind::Issue(4445), None, None, None, None)
        .expect_err("an operator-only issue must be refused");
    let typed = err
        .downcast_ref::<ParkedIssueDispatchError>()
        .expect("refusal must carry the typed ParkedIssueDispatchError");
    assert_eq!(typed.label, "loom:operator-only");
    assert!(running_issue_sweep_id(&reg, 4445).is_none());
    std::env::remove_var("LOOM_REPO");
}

/// AC (the load-bearing exclusion): `loom:building` ALONE must NOT refuse.
/// It is legitimately present on the daemon's own in-flight claim, so a guard
/// keyed on the full `SKIP_LABELS` set would break the review-stall
/// watchdog's cancel-and-re-dispatch and the reaper's checkpoint-resume.
#[test]
#[serial]
fn dispatch_park_guard_allows_building_label_alone() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:building loom:curated", 0, "", false);

    let out = reg
        .dispatch(&SweepKind::Issue(4446), None, None, None, None)
        .expect("loom:building alone must never refuse dispatch");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4446 --jq .labels[].name"),
        "the guard still probed; it just did not refuse; got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 4446"),
        "dispatch proceeded to the label flip; got: {calls:?}"
    );
    if let Some(id) = running_issue_sweep_id(&reg, 4446) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// AC (fail-open, the single most safety-critical property): a forge error on
/// the REST label probe (non-zero `gh api`) must NOT wedge dispatch — the
/// probe returns `None` and dispatch proceeds to spawn + label flip, exactly
/// like the 2.5/2.6 guards.
#[test]
#[serial]
fn dispatch_fails_open_when_park_label_probe_errors() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    // The park label IS present, but the probe fails ⇒ unknown ⇒ fail open.
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 1, "", false);

    let out = reg
        .dispatch(&SweepKind::Issue(4447), None, None, None, None)
        .expect("a gh outage on the park probe must not wedge dispatch (fail-open)");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/4447 --jq .labels[].name"),
        "the guard attempted the REST probe; got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 4447"),
        "dispatch proceeded to the label flip after failing open; got: {calls:?}"
    );
    if let Some(id) = running_issue_sweep_id(&reg, 4447) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// AC: `skip_label_flip = true` (test fixtures without `gh` credentials)
/// never attempts the probe at all — not even the REST call — mirroring the
/// 2.5/2.6 skip condition.
#[test]
#[serial]
fn dispatch_skip_label_flip_bypasses_park_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "", true);

    let out = reg
        .dispatch(&SweepKind::Issue(4448), None, None, None, None)
        .expect("skip_label_flip must bypass the park guard entirely");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("api repos/"),
        "no forge call at all when label flips are disabled; got: {calls:?}"
    );
    if let Some(id) = running_issue_sweep_id(&reg, 4448) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// Guard ordering: 2.6 (open-PR) runs before 2.7 (park label), so an ordinary
/// dispatch of a parked issue that ALSO has an open PR is attributed to the
/// cheaper-to-explain open-PR refusal and never pays for the REST probe.
#[test]
#[serial]
fn open_pr_guard_fires_before_park_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "4500", false);

    let err = reg
        .dispatch(&SweepKind::Issue(4449), None, None, None, None)
        .expect_err("an issue with an open linked PR must be refused");
    assert!(
        err.downcast_ref::<OpenPrDispatchError>().is_some(),
        "the 2.6 open-PR guard wins for an ordinary dispatch; got: {err}"
    );
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains(".labels[].name"),
        "the 2.7 REST label probe must not run once 2.6 refuses; got: {calls:?}"
    );
    std::env::remove_var("LOOM_REPO");
}

/// AC (Test Plan item 2, regression): the recovery path must NOT weaken
/// the #4123 guard for ordinary dispatches. After a reaper-driven resume
/// has fired for an issue (so it now has a fresh Running sweep AND an
/// open PR), a plain `dispatch()` call for the SAME issue — simulating an
/// unrelated later work-finder tick — is still refused with the typed
/// `OpenPrDispatchError`. The resume bypass is unreachable from the
/// public `dispatch()` entry point.
#[tokio::test]
#[serial]
async fn ordinary_dispatch_still_refused_after_a_resume() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = open_pr_guard_registry(ws, "4302", 0, false);

    write_checkpoint(&reg, 4259, "doctor-done");
    insert_dead_running_entry(&mut reg, 4259, "sweep-issue-4259-crashed");
    let changed = reg.reap_once();
    assert!(changed >= 1);
    assert!(
        running_issue_sweep_id(&reg, 4259).is_some(),
        "the resume dispatch must have created a fresh Running entry first"
    );

    // A later, ordinary re-dispatch attempt for the same issue (e.g. a
    // stray work-finder tick, or a watchdog) must still be refused — the
    // open PR is still open, and this call carries no resume exemption.
    let err = reg
        .dispatch(&SweepKind::Issue(4259), None, None, None, None)
        .expect_err("an ordinary dispatch must still be refused by the #4123 guard");
    assert!(
        err.downcast_ref::<OpenPrDispatchError>().is_some(),
        "must be the typed OpenPrDispatchError, not some other failure; got: {err}"
    );
    std::env::remove_var("LOOM_REPO");
}

// --- open-PR guard <-> #4485 backoff ladder integration (Issue #7606) ---

/// AC1: an open-PR-guard refusal arms the existing #4485 backoff ladder
/// for that issue, tagged as an open-PR-guard cause so
/// `open_pr_backoff_issues` (not just the generic `dispatch_backoff_issues`)
/// reports it — the signal the work-finder's `pr_open_backed_off()` reads.
#[test]
#[serial]
fn open_pr_guard_refusal_arms_the_4485_backoff_ladder() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = open_pr_guard_registry(ws, "4302", 0, false);

    assert_eq!(reg.dispatch_failure_count(4260), 0, "no ladder entry before the first refusal");

    let err = reg
        .dispatch(&SweepKind::Issue(4260), None, None, None, None)
        .expect_err("an issue with an open linked PR must be refused");
    assert!(err.downcast_ref::<OpenPrDispatchError>().is_some());

    assert_eq!(
        reg.dispatch_failure_count(4260),
        1,
        "the open-PR refusal must arm the #4485 ladder (#7606)"
    );
    let now = Utc::now();
    assert!(
        reg.dispatch_backoff_remaining(4260, now).is_some(),
        "the ladder's window must be live immediately after the refusal"
    );
    assert!(
        reg.dispatch_backoff_issues(now).contains(&4260),
        "the generic backoff set must also report it (unions every cause)"
    );
    assert!(
        reg.open_pr_backoff_issues(now).contains(&4260),
        "the window must be tagged as open-PR-guard-caused (#7606)"
    );

    std::env::remove_var("LOOM_REPO");
}

/// AC2: once the #6788 open-PR memo is fresh (a previous dispatch attempt
/// already verified the open linked PR), a SUBSEQUENT dispatch attempt for
/// the same issue is refused at the 2.5 closed-issue guard's position with
/// ZERO forge calls — neither the 2.5 REST closed-issue probe nor 2.6's own
/// GraphQL/REST probe runs a second time. Mirrors
/// `closed_issue_guard_fires_before_open_pr_guard`'s style, for the new
/// short-circuit direction (Issue #7606).
#[test]
#[serial]
fn open_pr_guard_memo_short_circuits_before_closed_issue_probe() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = open_pr_guard_registry(ws, "4302", 0, false);

    // First dispatch: no memo yet, so the full 2.5/2.6 chain runs and 2.6
    // refuses, verifying (and memoizing) PR #4302 as open.
    let err = reg
        .dispatch(&SweepKind::Issue(4261), None, None, None, None)
        .expect_err("an issue with an open linked PR must be refused");
    assert!(err.downcast_ref::<OpenPrDispatchError>().is_some());
    let calls_after_first = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls_after_first.contains("api graphql"),
        "the first dispatch must run the real probe to populate the memo; got: \
             {calls_after_first:?}"
    );

    // Isolate the second call's invocations (if any) from the first's.
    std::fs::write(&gh_log, "").unwrap();

    // Second dispatch, immediately after: the memo is fresh, so refusal
    // must happen at 2.5's position with NO forge call at all.
    let err2 = reg
        .dispatch(&SweepKind::Issue(4261), None, None, None, None)
        .expect_err("the memo-fresh issue must still be refused");
    let typed = err2
        .downcast_ref::<OpenPrDispatchError>()
        .expect("refusal must still carry the typed OpenPrDispatchError");
    assert_eq!(typed.pr, 4302);
    let calls_after_second = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls_after_second.is_empty(),
        "a fresh open-PR memo must refuse with ZERO forge calls (#7606); got: \
             {calls_after_second:?}"
    );

    std::env::remove_var("LOOM_REPO");
}

/// AC (fail-open, edge case): a memo MISS — no prior verified probe for
/// this issue — must fall straight through to the unchanged 2.5/2.6 checks
/// rather than ever refusing on the strength of an absent memo (Issue
/// #7606). Regression guard: `closed_guard_registry` reports the issue as
/// genuinely closed, and a fresh registry has no memo for it at all, so
/// this exercises the "no ladder entry yet" fast path end to end.
#[test]
#[serial]
fn open_pr_guard_memo_miss_falls_through_to_closed_issue_guard() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", false), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(4262), None, None, None, None)
        .expect_err("a closed issue must still be refused by the 2.5 guard");
    assert!(
        err.to_string().contains("closed"),
        "with no memo, the 2.5 closed-issue guard must still win; got: {err}"
    );
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !calls.contains("api graphql"),
        "the open-PR (2.6) probe must never run once 2.5 refuses; got: {calls:?}"
    );
    assert_eq!(
        reg.dispatch_failure_count(4262),
        0,
        "a closed-issue-guard refusal (not an open-PR-guard refusal) must not touch the \
             #4485 ladder"
    );
}

// --- workspace-commands dispatch guard (Issue #4027) ---

/// A workspace that "looks like" a repo (`.git`/`.loom` present, so
/// `looks_like_workspace()` in `workspace_registry.rs` would pass) but
/// was never `loom-daemon init`-ed — the reproduction from #4027 (a
/// second daemon host with a bare `git clone`). `dispatch` must refuse
/// BEFORE spending any forge call or token: no `gh` invocation at all
/// (not even the closed-issue probe), no spawned child, no registry
/// entry, and the error must name the `loom-daemon init` remediation.
#[test]
#[serial]
fn dispatch_refuses_workspace_missing_sweep_command() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("open", false), 0);
    // `closed_guard_registry` installs the marker by default (so its own
    // AC6 tests reach the closed-issue guard under test there) — remove
    // it here to simulate the #4027 wedge scenario.
    std::fs::remove_file(
        ws.join(".claude")
            .join("commands")
            .join("loom")
            .join("sweep.md"),
    )
    .unwrap();

    let err = reg
        .dispatch(&SweepKind::Issue(4222), None, None, None, None)
        .expect_err("a workspace missing installed commands must be refused");
    assert!(
        err.to_string().contains("loom-daemon init"),
        "error names the remediation; got: {err}"
    );
    assert!(
        err.to_string().contains("sweep.md"),
        "error names the missing marker; got: {err}"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.is_empty(),
        "no forge call whatsoever (no closed-issue probe, no label flip) on a \
             workspace-commands-refused dispatch; got: {calls:?}"
    );
    assert!(
        running_issue_sweep_id(&reg, 4222).is_none(),
        "no registry entry recorded on a refused dispatch"
    );

    // Issue #6440: this refusal must be the typed, downcast-matchable
    // `WorkspaceCommandsMissingDispatchError` — not a string-matched
    // generic `anyhow!` — so `work_finder` can attribute it to its own
    // `workspace-commands-missing` counter and skip the whole workspace's
    // candidate batch instead of re-discovering the same refusal once per
    // ready issue every tick (the 865-refusals-in-an-hour incident).
    let typed = err
        .downcast_ref::<WorkspaceCommandsMissingDispatchError>()
        .expect("refusal must carry the typed WorkspaceCommandsMissingDispatchError");
    assert_eq!(typed.workspace, ws);
}

/// Regression guard: a workspace WITH the marker installed dispatches
/// exactly as before — the #4027 guard is a pure no-op for a properly
/// initialized workspace.
#[test]
#[serial]
fn dispatch_proceeds_when_sweep_command_present() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("open", false), 0);

    let out = reg
        .dispatch(&SweepKind::Issue(4223), None, None, None, None)
        .expect("a properly initialized workspace must dispatch normally");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 4223"),
        "dispatch reached the label flip; got: {calls:?}"
    );

    if let Some(id) = running_issue_sweep_id(&reg, 4223) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

// ------------------------------------------------------------------------
// Dispatch-time lease-renewal hand-off (Issue #7672)
// ------------------------------------------------------------------------

/// Install a recording stub at the workspace path the dispatch path
/// resolves `sweep-lease-renew.sh` from. `body_tail` is appended after the
/// argv/env recording preamble, so a caller can choose what the stub does
/// once it has recorded the call (print a loop pid, fail, fork a real
/// detached child, …). Returns the record-log path.
fn install_recording_lease_renew(workspace: &Path, body_tail: &str) -> PathBuf {
    let record_log = workspace.join("lease-renew-invocations.log");
    let script = workspace.join(LEASE_RENEW_SCRIPT_REL);
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/env bash\n\
                 {{\n\
                 printf 'argv: %s\\n' \"$*\"\n\
                 for tok in \"$@\"; do printf 'arg: %s\\n' \"$tok\"; done\n\
                 printf 'PWD=%s\\n' \"$(pwd -P)\"\n\
                 }} >> \"{rec}\" 2>/dev/null\n\
                 {body_tail}",
            rec = record_log.display(),
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&script) {
        let _ = f.sync_all();
    }
    record_log
}

/// Issue #7672: every `--claim-owned` child is told, mechanically, that the
/// daemon which spawned it starts the lease-renewal loop on its behalf —
/// `LOOM_SWEEP_LEASE_RENEW_DISPATCHED=<issue>`, scoped exactly like the
/// `LOOM_SWEEP_CLAIM_OWNED` marker beside it.
///
/// This marker is what lets `sweep.md`'s Step 1a withdraw its own
/// `sweep-lease-renew.sh start` **conditionally** instead of outright. The
/// installed prompt and the daemon binary do not roll together — a plain
/// `git pull` refreshes `.claude/commands/loom/sweep.md` on a host whose
/// binary is only rebuilt by `loom update` — so an unconditional withdrawal
/// would leave every sweep dispatched in that skew window with no renewal
/// loop from *either* side, which is precisely the stale-lease reclamation
/// this issue exists to prevent. Absent marker ⇒ pre-#7672 daemon ⇒ the
/// session starts the loop itself, exactly as before.
#[test]
#[serial]
fn dispatch_exports_the_lease_renewal_capability_marker() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(76_725), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains(&format!("{LEASE_RENEW_STARTED_ENV}=76725")),
        "expected the lease-renewal capability marker for issue 76725 so Step 1a can skip \
             starting a second loop (#7672); got: {recorded}"
    );

    if let Some(id) = running_issue_sweep_id(&registry, 76_725) {
        let _ = registry.cancel(&id, Duration::from_millis(50));
    }
}

/// Issue #7672: the capability marker is `Issue`-scoped, exactly like the
/// `LOOM_SWEEP_CLAIM_OWNED` marker it sits beside — a `PrSet` child claims
/// no issue, holds no lease, and has nothing to renew, so advertising the
/// hand-off to it would be a lie its Step 1a could act on.
#[test]
#[serial]
fn pr_set_dispatch_exports_no_lease_renewal_marker() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::PrSet(vec![76_726]), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains(&format!("{LEASE_RENEW_STARTED_ENV}=unset")),
        "a PrSet child must not be told a renewal loop was started for it (#7672); \
             got: {recorded}"
    );

    let ids: Vec<String> = registry.entries.keys().cloned().collect();
    for id in ids {
        let _ = registry.cancel(&id, Duration::from_millis(50));
    }
}

/// AC (#7672): a `--claim-owned` dispatch starts the sweep's lease-renewal
/// loop itself, from dispatch code — `sweep-lease-renew.sh start <N>
/// --watch-pid <child_pid> --host <published host> --sweep-id <id>` — so
/// Step 1a can no longer be skipped by a session that does not follow its
/// prose (the 2.5h claim/yield thrash + near-miss shared-worktree
/// double-claim in 2AMLogic/klayout-tools#1658).
///
/// The three argument values are the load-bearing part: the watched pid
/// must be **this dispatch's own child** (so the loop's lifetime tracks
/// the sweep, not the daemon — see the `#6129` discussion on
/// `start_lease_renewal_loop`), and `--host`/`--sweep-id` must be the pair
/// this dispatch actually published in its lease comment, or `renew-once`
/// falls back to "newest wins" and can keep a PEER's lease fresh instead
/// (#6485).
///
/// `#[serial]`: asserts against `published_host_id()`, which reads the
/// process-global `LOOM_HOST_ID`/`HOSTNAME` env other `#[serial]` tests
/// in this crate mutate.
#[test]
#[serial]
fn claim_owned_dispatch_starts_the_lease_renewal_loop_for_its_own_child() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let renew_log = install_recording_lease_renew(dir.path(), "echo 4242\nexit 0\n");

    let outcome = registry
        .dispatch(&SweepKind::Issue(7672), None, None, None, None)
        .expect("dispatch should succeed");

    // `dispatch()` hands the `start` handshake to a detached thread (so it
    // never runs under the registry mutex — see `start_lease_renewal_loop`),
    // so wait for the recording stub's LAST line rather than reading the
    // log the instant dispatch returns. A few seconds is a generous budget
    // for a `bash` fork — a genuinely missing invocation still fails fast.
    assert!(
        wait_for_contents(&renew_log, "PWD=", 5_000),
        "dispatch must invoke {LEASE_RENEW_SCRIPT_REL} — Step 1a is now the daemon's job, \
             not the spawned session's prose (#7672)"
    );
    let recorded = std::fs::read_to_string(&renew_log).unwrap();
    let expected = format!(
        "argv: start 7672 --watch-pid {pid} --host {host} --sweep-id {sweep}",
        pid = outcome.pid,
        host = registry.published_host_id(),
        sweep = outcome.sweep_id,
    );
    assert!(recorded.contains(&expected), "expected `{expected}`; got: {recorded}");
    // Exactly once per dispatch — a second loop would double every
    // renewal PATCH for the sweep's whole lifetime.
    assert_eq!(
        recorded.matches("argv: start ").count(),
        1,
        "the renewal loop must be started exactly once per dispatch; got: {recorded}"
    );
    // Run in the registry's own workspace, so the helper's `gh` resolves
    // this repo in a multi-workspace daemon (#3928/#3937).
    assert!(
        recorded.contains(&format!("PWD={}", std::fs::canonicalize(dir.path()).unwrap().display())),
        "the helper must run with the workspace root as cwd; got: {recorded}"
    );

    if let Some(id) = running_issue_sweep_id(&registry, 7672) {
        let _ = registry.cancel(&id, Duration::from_millis(50));
    }
}

/// AC (#7672), edge case (a) from the issue's test plan: a child that dies
/// immediately at `spawn-claude.sh`'s token-selection preflight (#4689)
/// takes the unwind path — claim lock, label and peer-claim advertisement
/// are all reverted and dispatch returns `Err`. No renewal loop may be
/// started for it: there is no claim left to keep alive, and the pid it
/// would watch is already gone.
///
/// This is why the `start` lives in `finish_issue_dispatch` (after the
/// #4689 check) rather than next to `Command::spawn()`.
///
/// Asserted through a bounded *wait* for evidence rather than a bare
/// `!exists()`: the `start` handshake now runs on a detached thread, so a
/// regression that moved the call before the #4689 check would otherwise be
/// able to win a race against an instantaneous assertion.
#[test]
#[serial]
fn immediate_preflight_death_starts_no_renewal_loop() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::remove_var("LOOM_REPO");
    let (mut reg, _gh_log) = token_selection_failure_registry(ws);
    let renew_log = install_recording_lease_renew(ws, "echo 4242\nexit 0\n");

    let err = reg
        .dispatch(&SweepKind::Issue(76_721), None, None, None, None)
        .expect_err("an immediate token-selection death must fail dispatch");
    assert!(err.downcast_ref::<TokenSelectionDispatchError>().is_some(), "got: {err}");

    assert!(
        !wait_for_contents(&renew_log, "argv: start ", 1_000),
        "a dispatch that unwound its own claim must not leave a renewal loop watching a \
             dead pid (#7672); got: {:?}",
        std::fs::read_to_string(&renew_log).unwrap_or_default()
    );
}

/// AC (#7672), edge case (b): the loop the daemon starts is **detached**,
/// not a supervised child — that is what keeps this hand-off from
/// reintroducing #6129 (a daemon restart expiring a live sweep's lease).
///
/// Driven through a stub shaped like the real `start` (fork a background
/// loop, `disown`, print its pid, exit 0): the call must return the loop's
/// pid promptly rather than blocking on the loop, the loop must still be
/// alive afterwards, the registry must not retain a handle to it, and it
/// must survive the registry being dropped — the closest in-process
/// analogue of the daemon going away underneath it.
#[test]
#[serial]
fn the_started_renewal_loop_is_detached_and_outlives_the_registry() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());
    // Mirrors `cmd_start`'s own detach shape: background subshell with its
    // stdio redirected away, `disown`, then print the loop pid.
    install_recording_lease_renew(
        dir.path(),
        "( sleep 30 ) < /dev/null > /dev/null 2>&1 &\n\
             loop_pid=$!\n\
             disown \"$loop_pid\" 2>/dev/null || true\n\
             echo \"$loop_pid\"\n\
             exit 0\n",
    );
    let log_path = dir.path().join("sweep-issue-76722.log");

    let started = Instant::now();
    let handshake = registry
        .start_lease_renewal_loop(76_722, "sweep-76722-0", std::process::id(), &log_path)
        .expect("the handshake thread must spawn");
    let elapsed = started.elapsed();
    let loop_pid = handshake
        .join()
        .expect("the handshake thread must not panic")
        .expect("the stub prints a loop pid, so one must be returned");

    assert!(
        elapsed < LEASE_RENEW_START_TIMEOUT,
        "the dispatch path must not block on the renewal loop's own lifetime — it returned \
             only after {elapsed:?}"
    );
    assert!(
        wait_until_alive(loop_pid, 2_000),
        "the detached loop (pid {loop_pid}) must be running after `start` returns"
    );
    assert!(
        registry.children.is_empty(),
        "the renewal loop must NOT be retained as a supervised child of this registry — \
             its lifetime is the sweep's, never the daemon's (#6129/#7672)"
    );

    drop(registry);
    assert!(
        is_pid_alive(loop_pid),
        "the renewal loop must survive the daemon-side registry going away (#7672 edge \
             case (b): a daemon restart shortly after dispatch)"
    );
    // Housekeeping: this is a real detached process, so reap it here
    // rather than leaving a 30s sleeper behind per test run.
    let _ = libc_kill(loop_pid as i32, libc::SIGTERM);
}

/// A workspace without the helper installed (an older install, a
/// hand-rolled checkout) must dispatch exactly as it did before #7672:
/// no panic, no failure, no loop — the lease simply ages out as it always
/// had. Posting a lease record is best-effort, and so is renewing it.
#[test]
#[serial]
fn missing_lease_renew_helper_is_a_silent_noop() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    assert!(!dir.path().join(LEASE_RENEW_SCRIPT_REL).exists());

    let outcome = registry
        .dispatch(&SweepKind::Issue(76_723), None, None, None, None)
        .expect("a workspace with no lease-renewal helper must still dispatch");
    assert!(outcome.was_new);

    if let Some(id) = running_issue_sweep_id(&registry, 76_723) {
        let _ = registry.cancel(&id, Duration::from_millis(50));
    }
}

/// A helper that exits non-zero must not panic, must not fail the
/// dispatch, and must report `None` — the claim keeps its label, the
/// dispatch keeps running, and only the lease's freshness is lost.
#[test]
#[serial]
fn a_failing_lease_renew_helper_never_fails_the_dispatch() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());
    let renew_log = install_recording_lease_renew(dir.path(), "echo boom >&2\nexit 1\n");
    let log_path = dir.path().join("sweep-issue-76724.log");

    let started = registry
        .start_lease_renewal_loop(76_724, "sweep-76724-0", std::process::id(), &log_path)
        .expect("the handshake thread must spawn")
        .join()
        .expect("the handshake thread must not panic");

    assert!(started.is_none(), "a failed `start` must report no loop pid");
    assert!(renew_log.exists(), "the helper was still invoked");
    // #6541: the helper's stderr lands in the sweep's own log, where an
    // operator already looks — never silently discarded.
    let sweep_log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        sweep_log.contains("boom"),
        "the helper's stderr must be captured into the sweep log; got: {sweep_log:?}"
    );
}

/// Liveness regression (PR #7693 review): the `start` handshake must not be
/// performed while the registry mutex is held.
///
/// `start_lease_renewal_loop` is called from `finish_issue_dispatch`, which
/// every caller — `ipc.rs`'s `dispatch_sweep_nonblocking` Phase 3,
/// `dispatch_issue_releasing_poll_lock`'s Phase 3, `dispatch_inner`, the
/// reaper's resume path — invokes holding the global
/// `Arc<Mutex<SweepRegistry>>`, the first two directly on a tokio worker.
/// A pathological helper (a wedged filesystem, a `bash` that never execs)
/// would therefore pin that mutex for up to `LEASE_RENEW_START_TIMEOUT` on
/// every `Issue` dispatch, starving `list_sweeps` / `cancel` / concurrent
/// dispatches — the same hazard
/// `list_sweeps_is_not_starved_behind_a_concurrent_dispatch_burst`
/// (`ipc.rs`, #7307) pins for the Phase 2 account-selection poll, which that
/// test cannot see on this Phase 3 path.
///
/// Driven with a helper that sleeps far longer than any lock-sensitive
/// budget, invoked from inside a lock scope shaped exactly like
/// `finish_issue_dispatch`'s. Reverting the call to a synchronous,
/// inline wait fails both assertions below.
#[test]
#[serial]
fn a_slow_lease_renew_start_never_holds_the_registry_lock() {
    const SLOW_HELPER_SECS: u64 = 5;
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());
    let renew_log = install_recording_lease_renew(
        dir.path(),
        &format!("sleep {SLOW_HELPER_SECS}\necho 4242\nexit 0\n"),
    );
    let log_path = dir.path().join("sweep-issue-76727.log");
    let registry = Arc::new(Mutex::new(registry));

    let (handshake, call_elapsed, waiter) = {
        // The lock scope `finish_issue_dispatch` runs under.
        let sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A `list_sweeps`-shaped reader, already queued on the same mutex
        // before the call below: it measures how long the dispatch path
        // keeps the lock, which is the whole point.
        let waiter = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                let queued = Instant::now();
                let _guard = registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queued.elapsed()
            })
        };
        std::thread::sleep(Duration::from_millis(50));

        let called = Instant::now();
        let handshake =
            sr.start_lease_renewal_loop(76_727, "sweep-76727-0", std::process::id(), &log_path);
        (handshake, called.elapsed(), waiter)
    };

    let slow = Duration::from_secs(SLOW_HELPER_SECS);
    assert!(
        call_elapsed < slow / 2,
        "the lock-scoped call must return without waiting on the helper; it took \
             {call_elapsed:?} against a helper that sleeps {SLOW_HELPER_SECS}s"
    );
    let blocked_for = waiter.join().expect("the waiter thread must not panic");
    assert!(
        blocked_for < slow / 2,
        "a concurrent registry-lock consumer (list_sweeps/cancel/another dispatch) must not \
             be starved behind the lease-renewal helper; it waited {blocked_for:?}"
    );

    // ...and the handshake still completes, off-lock, with the loop pid.
    let loop_pid = handshake
        .expect("the handshake thread must spawn")
        .join()
        .expect("the handshake thread must not panic");
    assert_eq!(loop_pid, Some(4242), "the off-lock handshake must still be performed");
    assert!(renew_log.exists(), "the helper was invoked");
}
