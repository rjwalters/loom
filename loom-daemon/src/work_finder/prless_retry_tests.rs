//! Work-finder coverage for the PR-less retry bound's skip set (Issues #7972,
//! #9292).
//!
//! Lives in its own file rather than in the shared `work_finder::tests` module
//! for the file-size-ratchet reason `tmpfs_warning` / `registry_refresh` do
//! (`.loom/docs/file-size-policy.md`): `work_finder/tests.rs` is well over the
//! threshold and frozen at its current size. Its `RecordingDispatcher` is
//! consequently not reused — this file carries its own two-field fakes, which
//! is also why every assertion below is about one behaviour rather than an
//! incidentally-shared harness.

use super::*;
use std::collections::HashSet;

/// A one-shot [`WorkSource`] over a fixed candidate list.
struct OneShotSource(Option<Vec<WorkItem>>);

impl OneShotSource {
    fn of(numbers: &[u32]) -> Self {
        Self(Some(
            numbers
                .iter()
                .map(|n| WorkItem::new(*n, vec!["loom:issue".to_string()]))
                .collect(),
        ))
    }
}

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

/// A [`WorkDispatcher`] that records what it was asked to dispatch and reports
/// whatever brake sets the test configures. Every other trait method keeps its
/// default (empty / false) implementation.
#[derive(Default)]
struct BrakeDispatcher {
    dispatched: Vec<u32>,
    quarantined: HashSet<u32>,
    backed_off: HashSet<u32>,
    noop_cooldown: HashSet<u32>,
    prless_retry: HashSet<u32>,
}

impl WorkDispatcher for BrakeDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn quarantined(&self) -> HashSet<u32> {
        self.quarantined.clone()
    }
    fn backed_off(&self) -> HashSet<u32> {
        self.backed_off.clone()
    }
    fn noop_cooldown(&self) -> HashSet<u32> {
        self.noop_cooldown.clone()
    }
    fn prless_retry(&self) -> HashSet<u32> {
        self.prless_retry.clone()
    }
    fn occupancy(&self) -> usize {
        self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32, _complexity: Option<&str>) -> Result<bool> {
        self.dispatched.push(issue);
        Ok(true)
    }
}

/// #7972: an issue whose previous dispatch claimed it, released it, and left
/// no pull request behind — and whose armed window has not elapsed — is
/// skipped, never dispatched, and counted under its own reason, while its
/// healthy siblings dispatch normally. This is the pre-`dispatch()` half of
/// the fix: without it the observed #7893 loop re-claimed the same issue on
/// the very next tick, sometimes within the same minute.
#[test]
fn tick_skips_an_issue_inside_its_prless_retry_window() {
    let mut source = OneShotSource::of(&[1, 2, 3]);
    let mut disp = BrakeDispatcher {
        prless_retry: HashSet::from([2]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_prless_retry, 1, "#2 is inside its PR-less retry window");
    assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
    assert_eq!(disp.dispatched, vec![1, 3], "#2 was never dispatched");
}

/// The skip happens BEFORE the capacity gate (like quarantine, backoff and
/// the no-op cooldown), so an issue that keeps producing nothing never
/// reserves a slot a healthy sibling could have used.
#[test]
fn a_prless_retry_window_does_not_consume_a_capacity_slot() {
    let mut source = OneShotSource::of(&[1, 2]);
    let mut disp = BrakeDispatcher {
        prless_retry: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 1, false).unwrap();

    assert_eq!(report.skipped_prless_retry, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
}

/// #7972's explicit regression requirement: this is a FOURTH, distinct skip
/// set. Each candidate matching exactly one brake is counted under exactly
/// that brake's reason, and none perturbs the others.
#[test]
fn the_prless_skip_set_is_independent_of_the_other_brakes() {
    let mut source = OneShotSource::of(&[1, 2, 3, 4, 5]);
    let mut disp = BrakeDispatcher {
        quarantined: HashSet::from([1]),
        backed_off: HashSet::from([2]),
        noop_cooldown: HashSet::from([3]),
        prless_retry: HashSet::from([4]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_quarantined, 1);
    assert_eq!(report.skipped_backoff, 1);
    assert_eq!(report.skipped_noop_cooldown, 1);
    assert_eq!(report.skipped_prless_retry, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(disp.dispatched, vec![5], "only the healthy #5 dispatches");
}

/// Mirrors the #3939 quarantine / #4485 backoff / #6670 cooldown
/// cross-workspace property: workspace A's only candidate is inside its
/// PR-less retry window, workspace B has a healthy one. With a shared cap of
/// 1, B's issue MUST get the slot — a held candidate never starves a sibling.
#[test]
fn a_prless_held_workspace_does_not_starve_a_sibling() {
    let mut multi = vec![
        (
            OneShotSource::of(&[1]),
            BrakeDispatcher {
                prless_retry: HashSet::from([1]),
                ..Default::default()
            },
        ),
        (OneShotSource::of(&[10]), BrakeDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 1, &[false, false]);

    assert_eq!(report.skipped_prless_retry, 1, "workspace A's #1 produced no PR last time");
    assert_eq!(report.dispatched, 1);
    assert!(multi[0].1.dispatched.is_empty(), "the held workspace dispatches nothing");
    assert_eq!(multi[1].1.dispatched, vec![10], "the healthy sibling gets the shared slot");
}

/// #9292 at the work-finder seam, through the **production** dispatcher rather
/// than a fake: the four tests above inject a skip set directly, so they cannot
/// tell a window this host armed from one a peer did. This one arms nothing
/// locally — the registry has never seen issue #8812 — and lets a single peer
/// ad be the only reason it is skipped, which is the path a second dispatch host
/// actually takes.
///
/// `RegistryDispatcher::prless_retry()` is a pure in-memory read of
/// `SweepRegistry::prless_retry_issues`, and #9292 is what made that read a
/// union of local and peer-advertised windows. Nothing at this layer changed:
/// the assertion is that nothing had to.
#[test]
fn tick_skips_an_issue_only_a_peer_host_armed() {
    use crate::peer_claims::{observe_brake_ad, ClaimAd, PeerClaimView};
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    // Hermetic: no label flips, so `workspace_commands_missing()` stays false
    // without installing `.claude/commands/loom/` on disk.
    config.skip_label_flip = true;
    let mut registry = SweepRegistry::new(config);
    let repo = crate::peer_claims::repo_slug(&registry.config().workspace_root);

    // A peer host's release ad — the only record of #8812 anywhere in this
    // process. `observe_brake_ad` is the exact routing `safehouse::PeerClaimSink`
    // performs on an inbound envelope.
    let view = Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
    {
        let mut v = view.lock().unwrap();
        observe_brake_ad(
            &mut v,
            &ClaimAd::prless_release_armed(
                8812,
                repo,
                "host-e1d4c843".into(),
                7,
                "2026-09-24T07:06:00Z".into(),
                2,
                300,
            ),
            Instant::now(),
        );
    }
    registry.set_peer_claims(view);

    let mut source = OneShotSource::of(&[8812]);
    let mut disp = RegistryDispatcher::new(Arc::new(Mutex::new(registry)));
    assert_eq!(
        disp.prless_retry(),
        HashSet::from([8812]),
        "the production skip-set read must include a peer-armed window (#9292)"
    );

    let report = tick(&mut source, &mut disp, 10, false).unwrap();
    assert_eq!(
        report.skipped_prless_retry, 1,
        "a peer's PR-less release must space out THIS host's next claim too"
    );
    assert_eq!(report.dispatched, 0, "nothing may be dispatched — and no spawn is attempted");
}

/// #10642 AC4: the operator's star (`loom:operator-priority`) is an ordering
/// key, not a retry exemption. A starred issue inside its PR-less window is
/// skipped exactly like an unstarred one — several of the `2AMLogic/2am`
/// issues that looped on `no-phase-signal` deaths were starred.
#[test]
fn a_starred_issue_inside_its_prless_window_is_still_skipped() {
    let mut source = OneShotSource(Some(vec![
        WorkItem::new(
            1,
            vec![
                "loom:issue".to_string(),
                OPERATOR_PRIORITY_LABEL.to_string(),
            ],
        ),
        WorkItem::new(2, vec!["loom:issue".to_string()]),
    ]));
    let mut disp = BrakeDispatcher {
        prless_retry: HashSet::from([1]),
        ..Default::default()
    };
    let report = tick(&mut source, &mut disp, 10, false).unwrap();

    assert_eq!(report.skipped_prless_retry, 1, "the star does not exempt #1");
    assert_eq!(disp.dispatched, vec![2]);
}
