//! #11900: the registry applies the step-outcome ledger's effects, and every
//! broadcast window stays inside 24 h.

use super::*;
use crate::peer_claims::{ClaimKind, PeerClaimView};
use crate::step_dispatch::outcome_ledger::OutcomeClass;
use crate::sweep_registry::test_support::fixture_registry;
use serial_test::serial;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

const DAY: u64 = 24 * 3600;

#[derive(Default)]
struct FakeForge {
    body: String,
    writes: usize,
}

impl BodyForge for FakeForge {
    fn body(&mut self, _number: u64) -> Result<String, String> {
        Ok(self.body.clone())
    }
    fn set_body(&mut self, _number: u64, body: &str) -> Result<(), String> {
        self.body = body.to_string();
        self.writes += 1;
        Ok(())
    }
}

fn run(item: u32, step: StepRole, class: OutcomeClass, reason: &str, secs: u64) -> StepRun {
    StepRun {
        item,
        step,
        sweep_id: format!("step-{step}-{item}-1"),
        outcome: StepOutcome::new(class, reason),
        ran_for: Duration::from_secs(secs),
        failure_class: None,
    }
}

type Ads = tokio::sync::mpsc::Receiver<ClaimAd>;

fn registry(dir: &std::path::Path) -> (SweepRegistry, Ads) {
    let (mut registry, _) = fixture_registry(dir);
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    registry.set_peer_claim_publisher(tx);
    (registry, rx)
}

fn drain(rx: &mut Ads) -> Vec<ClaimAd> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn step_ads(ads: &[ClaimAd]) -> Vec<(u32, Option<StepRole>, u64)> {
    ads.iter()
        .filter(|a| a.kind == ClaimKind::StepCooldownArmed)
        .map(|a| (a.issue, a.step, a.remaining_secs.unwrap()))
        .collect()
}

#[test]
fn a_noop_cools_locally_and_is_broadcast_per_item_step() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    let rec = reg.record_step_outcome(
        &run(9, StepRole::Judge, OutcomeClass::Noop, "ci_pending", 600),
        &mut FakeForge::default(),
    );
    assert!(rec.cooling_until.is_some());
    let now = Utc::now();
    assert!(reg.step_cooldowns(now).is_cooling(9, StepRole::Judge, now));
    assert!(!reg.step_cooldowns(now).is_cooling(9, StepRole::Doctor, now));
    let ads = step_ads(&drain(&mut rx));
    assert_eq!(ads.len(), 1);
    assert_eq!((ads[0].0, ads[0].1), (9, Some(StepRole::Judge)));
    assert!((3590..=3600).contains(&ads[0].2), "{ads:?}");
}

/// The refusal cooldown is in the draw's query as soon as the step is
/// recorded, before the next tick, and peers get it too.
#[test]
fn a_pool_gate_refusal_is_in_the_query_before_the_next_tick() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    reg.record_step_outcome(
        &run(3, StepRole::Curator, OutcomeClass::Refused, "pool_gate", 5),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(3, StepRole::Curator, now));
    // #11909: a refusal is this host's own (its pool, its claim race); a
    // peer with a healthy pool must not cool the item, so nothing is
    // broadcast.
    assert!(step_ads(&drain(&mut rx)).is_empty());
}

#[test]
#[serial]
fn landed_arms_the_landed_cooldown_and_a_24h_builder_cooldown() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    reg.record_step_outcome(
        &run(12, StepRole::Curator, OutcomeClass::Verdict, "closed", 300),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    assert!(reg.landed_cooldown_active(12));
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(12, StepRole::Builder, now));
    let ads = drain(&mut rx);
    assert!(ads
        .iter()
        .any(|a| a.kind == ClaimKind::DispatchBackoffArmed && a.issue == 12));
    let builder: Vec<_> = step_ads(&ads)
        .into_iter()
        .filter(|a| a.1 == Some(StepRole::Builder))
        .collect();
    // The verdict's lift (zero) goes first, then the 24 h Builder window,
    // so a peer applying them in order ends with the window.
    assert_eq!(builder.len(), 2, "{builder:?}");
    assert_eq!(builder[0].2, 0);
    assert!(builder[1].2 <= DAY && builder[1].2 > DAY - 10, "{builder:?}");
}

#[test]
fn a_verdict_lifts_peer_copies_and_a_champion_verdict_clears_the_ladder() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    let mut forge = FakeForge {
        body: "Body.".into(),
        ..FakeForge::default()
    };
    reg.record_step_outcome(
        &run(30, StepRole::ChampionMerge, OutcomeClass::Noop, "requeued", 60),
        &mut forge,
    );
    reg.record_step_outcome(
        &run(30, StepRole::ChampionMerge, OutcomeClass::Noop, "held", 60),
        &mut forge,
    );
    assert!(forge.body.contains("loom:reeval"));
    drain(&mut rx);

    reg.record_step_outcome(
        &run(30, StepRole::ChampionMerge, OutcomeClass::Verdict, "merged", 60),
        &mut forge,
    );
    assert_eq!(forge.body, "Body.", "the ladder is gone");
    let now = Utc::now();
    assert!(!reg
        .step_cooldowns(now)
        .is_cooling(30, StepRole::ChampionMerge, now));
    let zeros: Vec<_> = StepRole::ALL.iter().map(|s| (30, Some(*s), 0)).collect();
    assert_eq!(step_ads(&drain(&mut rx)), zeros, "a clear for every step of the item");
}

/// A verdict lifts a peer's cooldown on the item too: out of this host's
/// view at once, and a zero ad for every step tells the other peers.
#[test]
fn a_verdict_lifts_a_peers_cooldown_on_the_item() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    let view =
        Arc::new(Mutex::new(PeerClaimView::new("this-host".into(), Duration::from_secs(60))));
    let repo = peer_claims::repo_slug(&reg.config.workspace_root);
    for (item, step) in [(50, StepRole::Judge), (51, StepRole::Judge)] {
        let ad = ClaimAd::step_cooldown_armed(
            item,
            step,
            repo.clone(),
            "peer".into(),
            1,
            "t".into(),
            DAY,
        );
        view.lock()
            .unwrap()
            .observe_step_cooldown_at(&ad, Instant::now());
    }
    reg.set_peer_claims(view);
    let now = Utc::now();
    assert!(reg.step_cooldowns(now).is_cooling(50, StepRole::Judge, now));

    reg.record_step_outcome(
        &run(50, StepRole::Doctor, OutcomeClass::Verdict, "pr_opened", 300),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    assert!(!reg.step_cooldowns(now).is_cooling(50, StepRole::Judge, now), "lifted here");
    assert!(
        reg.step_cooldowns(now).is_cooling(51, StepRole::Judge, now),
        "another item kept"
    );
    let ads = step_ads(&drain(&mut rx));
    assert!(ads.contains(&(50, Some(StepRole::Judge), 0)), "{ads:?}");
    assert_eq!(ads.iter().filter(|a| a.2 == 0).count(), StepRole::ALL.len());
}

/// The ladder goes on the forge; locally and on the wire only up to 24 h,
/// even when the rung is two days.
#[test]
fn the_forge_ladder_is_written_and_its_mirror_is_bounded_at_24h() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    let mut forge = FakeForge {
        body:
            "Parked.\n\n<!-- loom:reeval step=champion_promote rung=2 next=2026-01-01T00:00:00Z -->"
                .into(),
        ..FakeForge::default()
    };
    reg.record_step_outcome(
        &run(8, StepRole::ChampionPromote, OutcomeClass::Noop, "blocked", 60),
        &mut forge,
    );
    let written = reeval_ladder::parse(&forge.body);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].rung, 3, "48 h rung");
    let now = Utc::now();
    let local = reg
        .step_cooldowns(now)
        .until(8, StepRole::ChampionPromote)
        .unwrap();
    assert!(local <= now + chrono::Duration::hours(24), "{local}");
    assert!(local > now + chrono::Duration::hours(23));
    let ads = step_ads(&drain(&mut rx));
    assert_eq!(ads.len(), 1);
    assert!(ads[0].2 <= DAY, "{ads:?}");
}

#[test]
fn an_issue_step_fast_failure_charges_the_ladder_and_mirrors_its_window() {
    let dir = tempdir().unwrap();
    let (mut reg, _rx) = registry(dir.path());
    reg.record_step_outcome(
        &run(21, StepRole::Builder, OutcomeClass::Failed, "crashed", 30),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    let ladder = reg
        .dispatch_backoff_until(21, now)
        .expect("the #11854 ladder armed");
    assert_eq!(reg.step_cooldowns(now).until(21, StepRole::Builder), Some(ladder));
}

/// Judge review on #11962: a PR step driven well past the fast-fail park
/// threshold never reaches the issue quarantine. No `gh` call (no label, no
/// park record, no comment), no body write, no quarantine entry, no issue
/// ladder; only the ledger's own window, broadcast. #11969 widens it to
/// Curator and Champion promotion: the park's release restores `loom:issue`,
/// which would let a curated or revised issue skip promotion.
#[test]
fn a_non_builder_step_past_the_fast_fail_threshold_writes_nothing_to_the_forge() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    let gh_log = dir.path().join("gh-calls.log");
    let fake_gh = dir.path().join("fake-gh-recording.sh");
    std::fs::write(
        &fake_gh,
        format!("#!/bin/bash\necho \"$*\" >> '{}'\nexit 0\n", gh_log.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    reg.config.gh_bin = Some(fake_gh);
    let mut forge = FakeForge {
        body: "PR body.".into(),
        ..FakeForge::default()
    };
    let steps = [
        StepRole::Curator,
        StepRole::ChampionPromote,
        StepRole::Judge,
        StepRole::Doctor,
        StepRole::ChampionMerge,
    ];
    for step in steps {
        for _ in 0..6 {
            let rec = reg.record_step_outcome(
                &run(77, step, OutcomeClass::Failed, "crashed", 20),
                &mut forge,
            );
            assert_eq!(rec.effect, Effect::FastFailWindow, "{step}");
        }
    }
    assert!(!gh_log.exists(), "no forge call: {:?}", std::fs::read_to_string(&gh_log));
    assert_eq!(forge.writes, 0, "no body write");
    assert_eq!(forge.body, "PR body.");
    assert!(!reg.is_quarantined(77), "never the issue quarantine");
    assert_eq!(reg.dispatch_backoff_until(77, Utc::now()), None, "never the issue ladder");
    let now = Utc::now();
    for step in steps {
        assert!(reg.step_cooldowns(now).is_cooling(77, step, now), "{step}");
    }
    let ads = step_ads(&drain(&mut rx));
    assert!(ads.iter().all(|a| a.0 == 77 && a.2 <= DAY && a.2 > 0), "{ads:?}");
    assert!(drain(&mut rx).is_empty());
}

#[test]
fn claim_lost_is_counted() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    reg.record_step_outcome(
        &run(5, StepRole::Doctor, OutcomeClass::Refused, "claim_lost", 1),
        &mut FakeForge::default(),
    );
    assert_eq!(reg.step_ledger_counters().claim_lost, 1);
    assert!(step_ads(&drain(&mut rx)).is_empty(), "nothing to broadcast");
}

/// #11969: a second consecutive lost claim arms the 10-min refusal cooldown,
/// in the draw's query and broadcast like every window.
#[test]
fn a_second_claim_lost_cools_locally() {
    let dir = tempdir().unwrap();
    let (mut reg, mut rx) = registry(dir.path());
    let lost = run(5, StepRole::Curator, OutcomeClass::Refused, "claim_lost", 1);
    reg.record_step_outcome(&lost, &mut FakeForge::default());
    assert!(step_ads(&drain(&mut rx)).is_empty());
    let rec = reg.record_step_outcome(&lost, &mut FakeForge::default());
    assert_eq!(rec.effect, Effect::Redraw);
    assert!(rec.cooling_until.is_some());
    let now = Utc::now();
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(5, StepRole::Curator, now));
    // Local only (#11909): a lost claim race is this host's.
    assert!(step_ads(&drain(&mut rx)).is_empty());
    assert_eq!(reg.step_ledger_counters().claim_lost, 2);
}

#[test]
fn peer_step_cooldowns_join_the_query() {
    let dir = tempdir().unwrap();
    let (mut reg, _rx) = registry(dir.path());
    let view =
        Arc::new(Mutex::new(PeerClaimView::new("this-host".into(), Duration::from_secs(60))));
    let repo = peer_claims::repo_slug(&reg.config.workspace_root);
    let ad = ClaimAd::step_cooldown_armed(
        44,
        StepRole::Builder,
        repo,
        "peer".into(),
        1,
        "t".into(),
        3 * DAY,
    );
    view.lock()
        .unwrap()
        .observe_step_cooldown_at(&ad, Instant::now());
    reg.set_peer_claims(view);
    let now = Utc::now();
    let until = reg
        .step_cooldowns(now)
        .until(44, StepRole::Builder)
        .unwrap();
    assert!(until <= now + chrono::Duration::hours(24), "a peer's window is capped too");
    assert!(until > now + chrono::Duration::hours(23));
}
