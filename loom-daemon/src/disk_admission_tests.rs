#![allow(clippy::unwrap_used)]

use super::*;
use crate::disk_footprint::{observe, record_pending, LiveUnit};

const NOW: i64 = 1_000_000;

fn charge(repo: &str, gb: u64, source: ChargeSource) -> RepoCharge {
    RepoCharge {
        repo: repo.into(),
        gb,
        source,
    }
}

/// A store sampled at `NOW` with `history` per repo and `inflight` units of
/// `(key, repo, written GB)`.
fn store(history: &[(&str, &[u64])], inflight: &[(&str, &str, u64)]) -> Store {
    let mut s = Store::default();
    for (repo, gbs) in history {
        s.repos
            .insert((*repo).into(), gbs.iter().map(|g| g * GIB).collect());
    }
    let live: Vec<LiveUnit> = inflight
        .iter()
        .map(|(key, repo, gb)| LiveUnit {
            key: (*key).into(),
            repo: (*repo).into(),
            issue: Some(1),
            bytes: gb * GIB,
        })
        .collect();
    let ended = observe(&mut s, &live, NOW);
    assert!(ended.is_empty());
    s
}

// --- the per-repo charge ---------------------------------------------------

#[test]
fn a_learned_charge_is_the_high_water_mark_plus_margin() {
    let s = store(&[("loom", &[24, 26, 3])], &[]);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("loom");
    std::fs::create_dir_all(&root).unwrap();
    // 26 GB + 10% = 28.6 -> 29, from the max of the history, not the latest.
    assert_eq!(charge_for_root(&root, &s, 8), charge("loom", 29, ChargeSource::Observed));
}

#[test]
fn the_fallback_order_is_observed_then_repo_config_then_global() {
    assert_eq!(charge_gb(Some(26 * GIB), Some(2), 8), (29, ChargeSource::Observed));
    assert_eq!(charge_gb(None, Some(2), 8), (2, ChargeSource::RepoConfig));
    assert_eq!(charge_gb(None, None, 8), (8, ChargeSource::Default));
    // A light repo's mark stays small: 0.3 GB -> 1 GB.
    assert_eq!(charge_gb(Some(GIB * 3 / 10), None, 8), (1, ChargeSource::Observed));
}

#[test]
fn the_repo_config_key_is_read_from_the_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("docs-site");
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(".loom/config.json"),
        r#"{"autonomous":{"workFinder":{"diskChargeGb":2}}}"#,
    )
    .unwrap();
    let empty = Store::default();
    assert_eq!(
        charge_for_root(&root, &empty, 8),
        charge("docs-site", 2, ChargeSource::RepoConfig)
    );
    // History outranks the config.
    let s = store(&[("docs-site", &[5])], &[]);
    assert_eq!(charge_for_root(&root, &s, 8).source, ChargeSource::Observed);
    // No config, no history: the global default.
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(charge_for_root(&bare, &empty, 8), charge("bare", 8, ChargeSource::Default));
}

// --- the in-flight reservation ---------------------------------------------

#[test]
fn the_reservation_refuses_a_heavy_repo_and_admits_a_light_one() {
    // The #11191 acceptance case: loom's mark is 25 GB (charge 28), one loom
    // sweep is in flight with 5 GB written, 40 GB free, 3 GB floor.
    let s = store(&[("loom", &[25])], &[("loom#1", "loom", 5)]);
    // Both charges derive from the measured marks: 25 GB and a non-Rust 0.3 GB.
    let (loom_gb, loom_src) = charge_gb(Some(25 * GIB), None, 8);
    let (docs_gb, docs_src) = charge_gb(Some(GIB * 3 / 10), None, 8);
    assert_eq!((loom_gb, docs_gb), (28, 1));
    let charges = vec![
        charge("loom", loom_gb, loom_src),
        charge("docs", docs_gb, docs_src),
    ];
    let b = assess(40, 3, &s, charges, 8, NOW, None);
    assert_eq!(b.reserved_gb, 23, "28 - 5 still to be written");
    assert_eq!(b.remaining_gb, 14, "40 - 3 - 23");
    assert!(!b.fits(0), "a second loom sweep would overrun: 28 > 14");
    assert!(b.fits(1), "a 0.3 GB repo still admits");
    assert_eq!(b.held(), vec![true, false]);
    assert!(b
        .deferral_detail(0)
        .contains("loom charge 28GB (observed) exceeds remaining 14GB"));
    // The legacy flat term would have admitted four more (40 / 8 = 5 > 1).
    assert!(crate::disk_headroom::disk_headroom(40, 8) > 1);
}

#[test]
fn a_unit_charges_only_what_it_has_not_yet_written() {
    // A sweep past its high-water mark reserves nothing more.
    let s = store(&[("loom", &[25])], &[("loom#1", "loom", 30)]);
    let b = assess(40, 3, &s, vec![charge("loom", 28, ChargeSource::Observed)], 8, NOW, None);
    assert_eq!(b.reserved_gb, 0);
    assert_eq!(b.remaining_gb, 37);
}

#[test]
fn units_of_repos_outside_the_tick_use_their_own_history_or_the_default() {
    // `anvil` is not one of this tick's roots: charged from its own history
    // (20 GB -> 22). `other` has none: the global default (8).
    let s = store(&[("anvil", &[20])], &[("anvil#1", "anvil", 2), ("other:judge-1", "other", 1)]);
    let (bytes, sweeps) = reserved_bytes(&s, &[], 8, NOW, None);
    assert_eq!(bytes, (22 - 2) * GIB + (8 - 1) * GIB);
    assert_eq!(sweeps, 2, "an issue unit and a run-dir unit both count");
}

#[test]
fn a_stale_store_reserves_nothing_from_old_samples() {
    let s = store(&[("loom", &[25])], &[("loom#1", "loom", 5)]);
    let later = NOW + crate::disk_footprint::STALE_AFTER_SECS;
    let b = assess(40, 3, &s, vec![charge("loom", 28, ChargeSource::Observed)], 8, later, None);
    assert_eq!(b.reserved_gb, 0, "fail open once the sampler has stopped");
}

#[test]
fn budget_debits_each_admission_and_the_cap_term_uses_the_smallest_charge() {
    let s = store(&[], &[("loom#1", "loom", 28)]);
    let charges = vec![
        charge("loom", 28, ChargeSource::Observed),
        charge("docs", 1, ChargeSource::Observed),
    ];
    let mut b = assess(60, 3, &s, charges, 8, NOW, None);
    assert_eq!(b.remaining_gb, 57);
    assert_eq!(b.cap_term(), 1 + 57, "one running + 57 more at 1 GB");
    b.debit(0);
    assert_eq!(b.remaining_gb, 29);
    assert!(b.fits(0));
    b.debit(0);
    assert_eq!(b.remaining_gb, 1);
    assert!(!b.fits(0));
    assert!(b.fits(1));
    assert!(b.fits(9), "an unknown index fails open");
}

/// Replay of loom-worker-1 at 15:59 on 2026-10-09 (#11191's table): loom's
/// history holds the 24 GB PR-set run and a 26 GB build; #8726 has written
/// its 26 GB, #9356 (re-dispatched at 15:52) about 2 GB; ~50 GB free.
#[test]
fn replaying_the_incident_refuses_9518_at_1559() {
    let s = store(&[("loom", &[24, 26])], &[("loom#8726", "loom", 26), ("loom#9356", "loom", 2)]);
    let global = 8;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("loom");
    std::fs::create_dir_all(&root).unwrap();
    let loom = charge_for_root(&root, &s, global);
    assert_eq!(loom.gb, 29);
    for free in [48, 50, 55] {
        let b = assess(free, 3, &s, vec![loom.clone()], global, NOW, None);
        assert_eq!(b.reserved_gb, 3 + 27);
        assert!(!b.fits(0), "#9518 must not be admitted at {free} GB free");
    }
    // The flat term in force that day admitted it: floor(50 / 8) = 6 > 2.
    assert!(crate::disk_headroom::disk_headroom(50, 8) > 2);
}

// --- the dispatch seam -----------------------------------------------------

#[test]
fn decide_records_a_pending_unit_so_the_next_dispatch_reserves_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("loom");
    std::fs::create_dir_all(&root).unwrap();
    let mut s = store(&[("loom", &[25])], &[]);
    // 70 GB free: room for two 28 GB sweeps (70 - 3 = 67), not three.
    decide(&root, &SweepKind::Issue(1), 70, 3, &mut s, 8, NOW).unwrap();
    assert_eq!(s.inflight["loom#1"].pending_since, Some(NOW));
    decide(&root, &SweepKind::PrSet(vec![9, 4]), 70, 3, &mut s, 8, NOW).unwrap();
    assert!(s.inflight.contains_key("loom#prs-4"));
    let refused = decide(&root, &SweepKind::Issue(2), 70, 3, &mut s, 8, NOW).unwrap_err();
    assert_eq!((refused.reserved_gb, refused.charge_gb), (56, 28));
    assert!(refused
        .to_string()
        .contains("free 70GB - floor 3GB - reserved 56GB"));
    // Re-dispatching issue 1 replaces its own pending unit rather than adding.
    decide(&root, &SweepKind::Issue(1), 70, 3, &mut s, 8, NOW).unwrap();
}

#[test]
fn the_seam_is_inert_in_tests_until_a_probe_is_installed() {
    let tmp = tempfile::tempdir().unwrap();
    admit_dispatch(tmp.path(), &SweepKind::Issue(1)).unwrap();
    TEST_SEAM.with(|c| *c.borrow_mut() = Some((2, 3, Store::default())));
    let err = admit_dispatch(tmp.path(), &SweepKind::Issue(1)).unwrap_err();
    assert!(err.downcast_ref::<DiskAdmissionRefused>().is_some());
    TEST_SEAM.with(|c| *c.borrow_mut() = None);
}

#[test]
fn a_pending_unit_is_reserved_even_before_any_sample() {
    let mut s = Store::default();
    record_pending(&mut s, "loom#3", "loom", Some(3), NOW);
    let b = assess(30, 3, &s, vec![charge("loom", 20, ChargeSource::RepoConfig)], 8, NOW, None);
    assert_eq!(b.reserved_gb, 20);
    assert_eq!(b.sweeps_in_flight, 1);
    assert!(!b.fits(0));
}
