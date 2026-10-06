//! `route_read` (W4-B): home placement, split, spill latch, owners filter.
//!
//! The withdrawal maps, the bucket book and the latch table are
//! process-global and `cargo test` runs these in parallel threads of one
//! process, so every test uses app ids and repos unique to itself. App ids
//! are numeric because the bucket book only books `app-<digits>` accounts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::forge_bucket_book::{Reading, Source};
use std::time::{Duration, SystemTime};

fn reader(app: &str, owners: Option<&[&str]>) -> Identity {
    Identity {
        app_id: app.to_string(),
        slug: None,
        private_key_path: PathBuf::from(format!("/keys/{app}.pem")),
        owners: owners.map(|o| o.iter().map(|s| (*s).to_string()).collect()),
    }
}

fn roster(readers: Vec<Identity>) -> Roster {
    Roster {
        writer: Some(reader("100", None)),
        readers,
        legacy_logins: vec![],
    }
}

/// A workspace with a fresh token for every reader × owner.
fn workspace(r: &Roster, owners: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(50)).to_rfc3339();
    for id in &r.readers {
        for owner in owners {
            let dir = super::super::reader_dir(tmp.path(), owner, id);
            super::super::publish(&dir, "ghs_test", id, "1", &expires).unwrap();
        }
    }
    tmp
}

fn v2(cfg: &RoutingConfig) -> RouteEnv<'_> {
    RouteEnv {
        mode: RoutingMode::Scoped,
        egress_forbidden: false,
        cfg,
    }
}

fn req<'a>(owner_repo: &'a str, key: Option<&'a str>) -> RouteRequest<'a> {
    RouteRequest::gate(owner_repo, None, Resource::Core).affinity(key)
}

fn app_of(d: &RouteDecision) -> &str {
    match d {
        RouteDecision::Reader { app_id, .. } => app_id,
        other => panic!("expected a reader, got {other:?}"),
    }
}

fn placement_of(d: &RouteDecision) -> Placement {
    match d {
        RouteDecision::Reader { placement, .. } => *placement,
        other => panic!("expected a reader, got {other:?}"),
    }
}

/// Book `app`'s `owner` core bucket as projected to `pct` at `now`: half the
/// window elapsed, so `used = pct / 2` of the limit.
fn book_projected(app: &str, owner: &str, pct: f64, now: SystemTime) {
    let now_e = epoch(now);
    let limit = 5000_u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let used = (limit as f64 * pct / 200.0).round() as u64;
    forge_bucket_book::insert(
        BucketKey::new(&format!("app-{app}"), owner, Resource::Core),
        Reading {
            limit: Some(limit),
            remaining: Some(limit - used),
            used: Some(used),
            reset_epoch: now_e + 1800,
            observed_at: now_e,
            source: Source::Header,
        },
    );
}

fn urls(repo: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("repos/{repo}/issues/{i}")).collect()
}

// ---- home placement ---------------------------------------------------------

#[test]
fn without_split_or_readings_the_home_choice_is_the_pre_w4b_walk() {
    let r = roster(vec![reader("81001", None), reader("81002", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    for i in 0..40 {
        let repo = format!("acme/home-{i}");
        let home = forge_read_pool::assignment_index(&repo, 2).unwrap();
        for key in [None, Some("api\u{1f}repos/x/issues/1"), Some("other")] {
            let d = route_read_in(ws.path(), &r, &req(&repo, key), &v2(&cfg), now);
            assert_eq!(app_of(&d), r.readers[home].app_id, "{repo} {key:?}");
            assert_eq!(placement_of(&d), Placement::Home);
        }
        // Byte-identical to the legacy wrapper's answer.
        assert_eq!(
            super::super::read_credential_in(ws.path(), &r, &repo, now)
                .unwrap()
                .1,
            r.readers[home].app_id
        );
    }
}

#[test]
fn a_withdrawn_or_stale_home_walks_forward_as_before() {
    let r = roster(vec![reader("81011", None), reader("81012", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default().with_env(Some("0"), None);
    let now = SystemTime::now();
    let repo = "acme/walk";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let home_app = r.readers[home].app_id.clone();
    super::super::withdraw_reader_for_repo_until(&home_app, repo, now + Duration::from_secs(60));
    let d = route_read_in(ws.path(), &r, &req(repo, None), &v2(&cfg), now);
    assert_eq!(app_of(&d), r.readers[1 - home].app_id);
    assert_eq!(placement_of(&d), Placement::Spill, "served off its placement");
}

#[test]
fn every_reader_withdrawn_is_exhausted_until_the_earliest_return() {
    let r = roster(vec![reader("81021", None), reader("81022", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    let soon = now + Duration::from_secs(120);
    let later = now + Duration::from_secs(900);
    forge_read_pool::withdraw_scoped_budget_until(
        "81021",
        "acme",
        forge_read_pool::ResourceScope::Core,
        later,
    );
    forge_read_pool::withdraw_scoped_until(
        "81022",
        "acme",
        forge_read_pool::ResourceScope::All,
        soon,
    );
    let d = route_read_in(ws.path(), &r, &req("acme/dry", None), &v2(&cfg), now);
    // 81021 is out of budget, but 81022's withdrawal is not a rate limit
    // (a plain scoped withdrawal, as a refused credential records): not a
    // budget exhaustion, so nothing may be shed on it.
    assert_eq!(
        d,
        RouteDecision::Exhausted {
            until: soon,
            cause: ExhaustCause::Unavailable
        }
    );
    assert_eq!(d.into_credential(), None, "a Gate caller goes to the writer");
    // The scoped withdrawal is core-only for 81021: graphql still routes.
    let g = RouteRequest::gate("acme/dry", None, Resource::Graphql);
    assert_eq!(app_of(&route_read_in(ws.path(), &r, &g, &v2(&cfg), now)), "81021");
}

// ---- no pool -----------------------------------------------------------------

#[test]
fn an_empty_roster_a_foreign_host_or_no_owner_is_no_pool() {
    let r = roster(vec![]);
    let ws = tempfile::tempdir().unwrap();
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    assert_eq!(
        route_read_in(ws.path(), &r, &req("acme/x", None), &v2(&cfg), now),
        RouteDecision::NoPool
    );
    let r = roster(vec![reader("81031", None)]);
    let ghe = RouteRequest::gate("acme/x", Some("ghe.example.com"), Resource::Core);
    assert_eq!(route_read_in(ws.path(), &r, &ghe, &v2(&cfg), now), RouteDecision::NoPool);
    assert_eq!(
        route_read_in(ws.path(), &r, &req("", None), &v2(&cfg), now),
        RouteDecision::NoPool
    );
}

#[test]
fn a_forbidden_egress_is_no_pool_in_v2_and_ignored_in_legacy() {
    let r = roster(vec![reader("81041", None), reader("81042", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    let forbidden = RouteEnv {
        mode: RoutingMode::Scoped,
        egress_forbidden: true,
        cfg: &cfg,
    };
    assert_eq!(
        route_read_in(ws.path(), &r, &req("acme/egress", None), &forbidden, now),
        RouteDecision::NoPool,
        "NoPool, not Exhausted: nothing is shed"
    );
    // Legacy read_credential never checked egress: same answer as pre-change.
    let legacy = RouteEnv {
        mode: RoutingMode::Legacy,
        egress_forbidden: true,
        cfg: &cfg,
    };
    let home = forge_read_pool::assignment_index("acme/egress", 2).unwrap();
    let d = route_read_in(ws.path(), &r, &req("acme/egress", None), &legacy, now);
    assert_eq!(app_of(&d), r.readers[home].app_id);
}

// ---- owners filter -------------------------------------------------------------

#[test]
fn a_reader_limited_to_one_owner_leaves_other_owners_untouched() {
    let base = roster(vec![reader("81051", None), reader("81052", None)]);
    let mut with_third = base.clone();
    with_third.readers.push(reader("81053", Some(&["acme"])));
    let ws = workspace(&with_third, &["acme", "other"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    for i in 0..30 {
        let repo = format!("other/tool-{i}");
        let before = route_read_in(ws.path(), &base, &req(&repo, None), &v2(&cfg), now);
        let after = route_read_in(ws.path(), &with_third, &req(&repo, None), &v2(&cfg), now);
        assert_eq!(app_of(&before), app_of(&after), "{repo}: home index unchanged");
        assert_ne!(app_of(&after), "81053", "{repo}: excluded from other owners' walk");
    }
    // acme gets n = 3.
    let seen: std::collections::BTreeSet<String> = (0..60)
        .map(|i| {
            let repo = format!("acme/svc-{i}");
            app_of(&route_read_in(ws.path(), &with_third, &req(&repo, None), &v2(&cfg), now))
                .to_string()
        })
        .collect();
    assert!(seen.contains("81053"), "acme repos use the third reader: {seen:?}");
}

#[test]
fn owners_parse_validates_and_lowercases() {
    let cfg = serde_json::json!({"forge": {"identities": {"readers": [
        {"appId": "1", "privateKeyPath": "/k/1.pem", "owners": ["Acme", "bad owner", "acme"]},
        {"appId": "2", "privateKeyPath": "/k/2.pem"},
        {"appId": "3", "privateKeyPath": "/k/3.pem", "owners": []}
    ]}}});
    let r = super::super::from_config(&cfg, None, &[]);
    assert_eq!(r.readers[0].owners.as_deref(), Some(&["acme".to_string()][..]));
    assert_eq!(r.readers[1].owners, None);
    assert!(r.readers[0].serves_owner("ACME"));
    assert!(!r.readers[0].serves_owner("other"));
    assert!(r.readers[1].serves_owner("anyone"));
    let w = super::super::config_warnings(&cfg);
    assert!(w.iter().any(|m| m.contains("bad owner")), "{w:?}");
    assert!(
        w.iter()
            .any(|m| m.contains("app 3") && m.contains("no valid owner")),
        "{w:?}"
    );
}

// ---- split -----------------------------------------------------------------------

fn split_cfg(repo: &str) -> RoutingConfig {
    RoutingConfig {
        split_repos: vec![repo.to_string()],
        ..RoutingConfig::default()
    }
}

#[test]
fn a_split_repo_spreads_urls_45_to_55_percent_and_each_url_is_sticky() {
    let r = roster(vec![reader("81061", None), reader("81062", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = split_cfg("acme/hot");
    let now = SystemTime::now();
    let mut first = 0;
    for url in urls("acme/hot", 1000) {
        let key = crate::gh_invocation::url_affinity_key(&url);
        let d = route_read_in(ws.path(), &r, &req("acme/hot", Some(&key)), &v2(&cfg), now);
        assert_eq!(placement_of(&d), Placement::Split);
        let again = route_read_in(ws.path(), &r, &req("acme/hot", Some(&key)), &v2(&cfg), now);
        assert_eq!(app_of(&d), app_of(&again), "{url} moved");
        if app_of(&d) == "81061" {
            first += 1;
        }
    }
    assert!((450..=550).contains(&first), "reader-1 share {first}/1000");
}

#[test]
fn split_falls_back_to_home_without_a_key_or_when_disabled() {
    let r = roster(vec![reader("81071", None), reader("81072", None)]);
    let ws = workspace(&r, &["acme"]);
    let now = SystemTime::now();
    let home = &r.readers[forge_read_pool::assignment_index("acme/hot2", 2).unwrap()].app_id;
    let cfg = split_cfg("acme/hot2");
    let d = route_read_in(ws.path(), &r, &req("acme/hot2", None), &v2(&cfg), now);
    assert_eq!((app_of(&d), placement_of(&d)), (home.as_str(), Placement::Home));
    for off in [
        split_cfg("acme/hot2").with_env(None, Some("0")),
        split_cfg("acme/hot2").with_env(Some("0"), None),
    ] {
        for url in urls("acme/hot2", 20) {
            let d = route_read_in(ws.path(), &r, &req("acme/hot2", Some(&url)), &v2(&off), now);
            assert_eq!(app_of(&d), home, "split disabled: home only");
        }
    }
    let legacy = RouteEnv {
        mode: RoutingMode::Legacy,
        egress_forbidden: false,
        cfg: &cfg,
    };
    for url in urls("acme/hot2", 20) {
        let d = route_read_in(ws.path(), &r, &req("acme/hot2", Some(&url)), &legacy, now);
        assert_eq!(app_of(&d), home, "legacy: hash only");
    }
}

// ---- spill latch -------------------------------------------------------------------

fn latch_cfg() -> RoutingConfig {
    RoutingConfig::default()
}

#[test]
fn step_engages_partial_at_70_full_at_90_and_holds_until_release() {
    let cfg = latch_cfg();
    let t0 = SystemTime::now();
    let reset = t0 + Duration::from_secs(1500);
    let at = |pct: Option<f64>| HomeState {
        projected_pct: pct,
        reset: Some(reset),
        withdrawn_until: None,
    };
    let (l, tr) = step(None, &at(Some(69.0)), &cfg, t0);
    assert_eq!((l, tr), (None, vec![]));
    let (l, tr) = step(None, &at(Some(72.0)), &cfg, t0);
    let l = l.unwrap();
    assert_eq!(l.mode, LatchMode::Partial);
    assert_eq!(l.release_at, reset, "releases at the home reset");
    assert_eq!(tr, vec![Transition::Engaged(LatchMode::Partial)]);
    // Better readings do not release it.
    let t1 = t0 + Duration::from_secs(600);
    let (held, tr) = step(Some(l), &at(Some(10.0)), &cfg, t1);
    assert_eq!((held, tr), (Some(l), vec![]));
    // 91% escalates to Full.
    let (full, tr) = step(Some(l), &at(Some(91.0)), &cfg, t1);
    assert_eq!(full.unwrap().mode, LatchMode::Full);
    assert_eq!(tr, vec![Transition::Engaged(LatchMode::Full)]);
    // ...and never steps back down before release.
    let (still, tr) = step(full, &at(Some(75.0)), &cfg, t1);
    assert_eq!((still, tr), (full, vec![]));
    // At the reset it releases.
    let (gone, tr) = step(full, &at(None), &cfg, reset);
    assert_eq!((gone, tr), (None, vec![Transition::Released]));
}

#[test]
fn an_unknown_reading_never_engages() {
    let cfg = latch_cfg();
    let none = HomeState {
        projected_pct: None,
        reset: None,
        withdrawn_until: None,
    };
    assert_eq!(step(None, &none, &cfg, SystemTime::now()), (None, vec![]));
}

#[test]
fn a_withdrawal_with_no_reset_is_full_for_the_default_window() {
    let cfg = latch_cfg();
    let t0 = SystemTime::now();
    let home = HomeState {
        projected_pct: None,
        reset: None,
        withdrawn_until: Some(t0 + Duration::from_secs(60)),
    };
    let l = step(None, &home, &cfg, t0).0.unwrap();
    assert_eq!(l.mode, LatchMode::Full);
    assert_eq!(l.release_at, t0 + Duration::from_secs(3600));
}

#[test]
fn a_credential_withdrawal_with_a_known_reset_releases_at_the_later() {
    let cfg = latch_cfg();
    let t0 = SystemTime::now();
    // Withdrawal (300 s) outlasts the reset (120 s).
    let home = HomeState {
        projected_pct: Some(40.0),
        reset: Some(t0 + Duration::from_secs(120)),
        withdrawn_until: Some(t0 + Duration::from_secs(300)),
    };
    let l = step(None, &home, &cfg, t0).0.unwrap();
    assert_eq!((l.mode, l.release_at), (LatchMode::Full, t0 + Duration::from_secs(300)));
    // Reset (2000 s) outlasts the withdrawal.
    let home = HomeState {
        reset: Some(t0 + Duration::from_secs(2000)),
        ..home
    };
    let l = step(None, &home, &cfg, t0).0.unwrap();
    assert_eq!(l.release_at, t0 + Duration::from_secs(2000));
    // Capped at entry + 3660 s.
    assert_eq!(
        release_at(t0, Some(t0 + Duration::from_secs(9000)), None),
        t0 + Duration::from_secs(3660)
    );
}

#[test]
fn partial_moves_exactly_the_spill_hash_one_urls_and_full_moves_all() {
    let r = roster(vec![reader("81081", None), reader("81082", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = latch_cfg();
    let now = SystemTime::now();
    let repo = "acme/latch-partial";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let (home_app, other) = (r.readers[home].app_id.clone(), r.readers[1 - home].app_id.clone());
    book_projected(&home_app, "acme", 72.0, now);
    let mut moved = 0;
    for url in urls(repo, 200) {
        let d = route_read_in(ws.path(), &r, &req(repo, Some(&url)), &v2(&cfg), now);
        let bit = forge_read_pool::spill_index(repo, &url, 2) == Some(1);
        if bit {
            assert_eq!((app_of(&d), placement_of(&d)), (other.as_str(), Placement::Spill), "{url}");
            moved += 1;
        } else {
            assert_eq!(app_of(&d), home_app, "{url}");
        }
    }
    assert!(moved > 0 && moved < 200);
    // A request without a key stays home in Partial.
    let d = route_read_in(ws.path(), &r, &req(repo, None), &v2(&cfg), now);
    assert_eq!(app_of(&d), home_app);

    // 91% → Full: every request moves.
    let repo = "acme/latch-full";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let home_app = r.readers[home].app_id.clone();
    // A distinct reader pair would share buckets with the test above, so
    // use fresh ids.
    let r2 = roster(vec![reader("81083", None), reader("81084", None)]);
    let ws2 = workspace(&r2, &["acme"]);
    let home_app2 = r2.readers[home].app_id.clone();
    book_projected(&home_app2, "acme", 91.0, now);
    for url in urls(repo, 50) {
        let d = route_read_in(ws2.path(), &r2, &req(repo, Some(&url)), &v2(&cfg), now);
        assert_ne!(app_of(&d), home_app2, "{url}");
    }
    let d = route_read_in(ws2.path(), &r2, &req(repo, None), &v2(&cfg), now);
    assert_ne!(app_of(&d), home_app2, "Full moves keyless requests too");
    let _ = home_app;
}

#[test]
fn the_latch_holds_while_readings_improve() {
    let r = roster(vec![reader("81091", None), reader("81092", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = latch_cfg();
    let now = SystemTime::now();
    let repo = "acme/latch-hold";
    let home_app = r.readers[forge_read_pool::assignment_index(repo, 2).unwrap()]
        .app_id
        .clone();
    book_projected(&home_app, "acme", 95.0, now);
    let d = route_read_in(ws.path(), &r, &req(repo, Some("k")), &v2(&cfg), now);
    assert_ne!(app_of(&d), home_app);
    let later = now + Duration::from_secs(5);
    book_projected(&home_app, "acme", 5.0, later);
    let d = route_read_in(ws.path(), &r, &req(repo, Some("k")), &v2(&cfg), later);
    assert_ne!(app_of(&d), home_app, "held until release");
    let live = live_latches(later);
    assert!(live
        .iter()
        .any(|(rp, _, app, l)| rp == repo && *app == home_app && l.mode == LatchMode::Full));
}

#[test]
fn a_spill_never_targets_a_reader_projected_at_60_or_more() {
    let r = roster(vec![reader("81101", None), reader("81102", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = latch_cfg();
    let now = SystemTime::now();
    let repo = "acme/both-hot";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let (home_app, other) = (r.readers[home].app_id.clone(), r.readers[1 - home].app_id.clone());
    book_projected(&home_app, "acme", 85.0, now);
    book_projected(&other, "acme", 85.0, now);
    for url in urls(repo, 30) {
        let d = route_read_in(ws.path(), &r, &req(repo, Some(&url)), &v2(&cfg), now);
        assert_eq!(app_of(&d), home_app, "{url}: no target with headroom → stay home");
    }
    // Target at 60.0 exactly is still too hot; home at 95 (Full).
    let r = roster(vec![reader("81103", None), reader("81104", None)]);
    let ws = workspace(&r, &["acme"]);
    let repo = "acme/edge-hot";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    book_projected(&r.readers[home].app_id, "acme", 95.0, now);
    book_projected(&r.readers[1 - home].app_id, "acme", 60.0, now);
    let d = route_read_in(ws.path(), &r, &req(repo, Some("k")), &v2(&cfg), now);
    assert_eq!(app_of(&d), r.readers[home].app_id);
}

#[test]
fn a_held_latch_keeps_its_target_while_the_targets_projection_crosses_60() {
    // Review finding on #10466: the target used to be re-chosen from its
    // live projection on every call, so a target oscillating around
    // targetMaxPct flapped spilled URLs between readers (one 200 per move).
    let r = roster(vec![reader("81121", None), reader("81122", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = latch_cfg();
    let t0 = SystemTime::now();
    let repo = "acme/pinned-target";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let (home_app, other) = (r.readers[home].app_id.clone(), r.readers[1 - home].app_id.clone());
    book_projected(&home_app, "acme", 95.0, t0);
    let keys = urls(repo, 20);
    for (i, pct) in [55.0, 65.0, 50.0, 85.0].into_iter().enumerate() {
        let at = t0 + Duration::from_secs(5 * i as u64);
        book_projected(&other, "acme", pct, at);
        for url in &keys {
            let d = route_read_in(ws.path(), &r, &req(repo, Some(url)), &v2(&cfg), at);
            assert_eq!(
                (app_of(&d), placement_of(&d)),
                (other.as_str(), Placement::Spill),
                "{url} at target {pct}%: a held latch never re-picks a usable target"
            );
        }
        assert_eq!(pinned_target(repo, Resource::Core, &home_app).as_deref(), Some(other.as_str()));
    }
}

#[test]
fn a_withdrawn_or_full_target_is_re_picked() {
    let r = roster(vec![
        reader("81131", None),
        reader("81132", None),
        reader("81133", None),
    ]);
    let ws = workspace(&r, &["acme"]);
    let cfg = latch_cfg();
    let t0 = SystemTime::now();
    let repo = "acme/repick";
    let home = forge_read_pool::assignment_index(repo, 3).unwrap();
    let home_app = r.readers[home].app_id.clone();
    let first = r.readers[(home + 1) % 3].app_id.clone();
    let second = r.readers[(home + 2) % 3].app_id.clone();
    book_projected(&home_app, "acme", 95.0, t0);
    let route = |at| {
        app_of(&route_read_in(ws.path(), &r, &req(repo, Some("k")), &v2(&cfg), at)).to_string()
    };
    assert_eq!(route(t0), first, "the first reader with headroom is pinned");

    // The pinned target reaches spillFullPct: re-picked to the next one.
    let t1 = t0 + Duration::from_secs(5);
    book_projected(&first, "acme", 91.0, t1);
    assert_eq!(route(t1), second);
    assert_eq!(pinned_target(repo, Resource::Core, &home_app).as_deref(), Some(second.as_str()));

    // The first reader cools off: the new pin holds, nothing moves back.
    let t2 = t0 + Duration::from_secs(10);
    book_projected(&first, "acme", 10.0, t2);
    assert_eq!(route(t2), second);

    // The pinned target is withdrawn: re-picked to the first reader again.
    let t3 = t0 + Duration::from_secs(15);
    forge_read_pool::withdraw_scoped_until(
        &second,
        "acme",
        forge_read_pool::ResourceScope::Core,
        t3 + Duration::from_secs(600),
    );
    assert_eq!(route(t3), first);
    assert_eq!(pinned_target(repo, Resource::Core, &home_app).as_deref(), Some(first.as_str()));
}

#[test]
fn an_unknown_home_reading_never_moves_anything() {
    let r = roster(vec![reader("81111", None), reader("81112", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = latch_cfg();
    let now = SystemTime::now();
    let repo = "acme/unknown";
    let home_app = r.readers[forge_read_pool::assignment_index(repo, 2).unwrap()]
        .app_id
        .clone();
    for url in urls(repo, 30) {
        let d = route_read_in(ws.path(), &r, &req(repo, Some(&url)), &v2(&cfg), now);
        assert_eq!(app_of(&d), home_app);
    }
    assert!(!live_latches(now).iter().any(|(rp, ..)| rp == repo));
}

#[test]
fn spill_disabled_keeps_home_whatever_the_readings() {
    let r = roster(vec![reader("81121", None), reader("81122", None)]);
    let ws = workspace(&r, &["acme"]);
    let now = SystemTime::now();
    let repo = "acme/no-spill";
    let home_app = r.readers[forge_read_pool::assignment_index(repo, 2).unwrap()]
        .app_id
        .clone();
    book_projected(&home_app, "acme", 99.0, now);
    for cfg in [
        RoutingConfig {
            spill: false,
            ..RoutingConfig::default()
        },
        RoutingConfig::default().with_env(Some("0"), None),
    ] {
        let d = route_read_in(ws.path(), &r, &req(repo, Some("k")), &v2(&cfg), now);
        assert_eq!(app_of(&d), home_app);
    }
}

// ---- config -------------------------------------------------------------------------

#[test]
fn routing_config_parses_validates_and_falls_back() {
    let (cfg, w) = RoutingConfig::parse(&serde_json::json!({}));
    assert_eq!((cfg, w.len()), (RoutingConfig::default(), 0));
    let (cfg, w) = RoutingConfig::parse(&serde_json::json!({"forge": {"readPool": {"routing": {
        "spill": false, "splitRepos": ["Acme/Hot", "not-a-repo", "acme/hot"],
        "spillProjectedPct": 75, "spillFullPct": 95, "targetMaxPct": 50
    }}}}));
    assert!(!cfg.spill);
    assert_eq!(cfg.split_repos, vec!["acme/hot".to_string()]);
    assert_eq!(
        (cfg.spill_projected_pct, cfg.spill_full_pct, cfg.target_max_pct),
        (75.0, 95.0, 50.0)
    );
    assert!(cfg.splits("ACME/hot"));
    assert_eq!(w.len(), 1, "{w:?}");
    // An inverted set falls back to the defaults, all three.
    let (cfg, w) = RoutingConfig::parse(&serde_json::json!({"forge": {"readPool": {"routing": {
        "spillProjectedPct": 95, "spillFullPct": 90
    }}}}));
    let d = RoutingConfig::default();
    assert_eq!(
        (cfg.spill_projected_pct, cfg.spill_full_pct, cfg.target_max_pct),
        (d.spill_projected_pct, d.spill_full_pct, d.target_max_pct)
    );
    assert_eq!(w.len(), 1, "{w:?}");
    let (_, w) = RoutingConfig::parse(&serde_json::json!({"forge": {"readPool": {"routing": {
        "spillFullPct": 101
    }}}}));
    assert_eq!(w.len(), 1, "full > 100 rejected: {w:?}");
}

#[test]
fn env_overrides_turn_layers_off() {
    let cfg = split_cfg("acme/hot");
    let off = cfg.clone().with_env(Some("0"), None);
    assert!(!off.spill && off.split_repos.is_empty());
    let nosplit = cfg.clone().with_env(None, Some("0"));
    assert!(nosplit.spill && nosplit.split_repos.is_empty());
    assert_eq!(cfg.clone().with_env(Some("1"), Some("")), cfg);
}

// ---- W4-C: class-aware fallback -----------------------------------------------

#[test]
fn past_a_withdrawn_home_a_deferrable_read_needs_headroom_and_a_gate_read_does_not() {
    let r = roster(vec![reader("81201", None), reader("81202", None)]);
    let ws = workspace(&r, &["acme"]);
    // Spill off: only the fallback walk is under test.
    let cfg = RoutingConfig::default().with_env(Some("0"), None);
    let now = SystemTime::now();
    let repo = "acme/class-walk";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let (home_app, other) = (r.readers[home].app_id.clone(), r.readers[1 - home].app_id.clone());
    forge_read_pool::withdraw_scoped_budget_until(
        &home_app,
        "acme",
        forge_read_pool::ResourceScope::Core,
        now + Duration::from_secs(600),
    );
    let class = |c: ReadClass| RouteRequest {
        class: c,
        ..req(repo, Some("k"))
    };
    // Unknown reading on the other reader: every class may move to it.
    for c in [
        ReadClass::Gate,
        ReadClass::Hygiene,
        ReadClass::Observability,
    ] {
        let d = route_read_in(ws.path(), &r, &class(c), &v2(&cfg), now);
        assert_eq!(app_of(&d), other, "{c:?}");
    }
    // The other reader projected at 75% (≥ targetMaxPct 60): a Gate read
    // still takes it; a deferrable read finds no reader and is exhausted.
    book_projected(&other, "acme", 75.0, now);
    let d = route_read_in(ws.path(), &r, &class(ReadClass::Gate), &v2(&cfg), now);
    assert_eq!(app_of(&d), other);
    for c in [ReadClass::Hygiene, ReadClass::Observability] {
        let d = route_read_in(ws.path(), &r, &class(c), &v2(&cfg), now);
        assert!(
            matches!(
                d,
                RouteDecision::Exhausted {
                    cause: ExhaustCause::Budget,
                    ..
                }
            ),
            "{c:?}: {d:?}"
        );
    }
    // Legacy ignores the class entirely.
    let legacy = RouteEnv {
        mode: RoutingMode::Legacy,
        ..v2(&cfg)
    };
    let d = route_read_in(ws.path(), &r, &class(ReadClass::Hygiene), &legacy, now);
    assert_eq!(app_of(&d), home_app, "legacy has no scoped withdrawal");
}

// ---- W4-C: why a route is exhausted, and the headroom reserve --------------

fn hygiene<'a>(owner_repo: &'a str) -> RouteRequest<'a> {
    RouteRequest {
        class: ReadClass::Hygiene,
        ..req(owner_repo, Some("k"))
    }
}

fn cause_of(d: &RouteDecision) -> ExhaustCause {
    match d {
        RouteDecision::Exhausted { cause, .. } => *cause,
        other => panic!("expected an exhausted route, got {other:?}"),
    }
}

#[test]
fn every_reader_coverage_withdrawn_is_not_a_budget_exhaustion() {
    let r = roster(vec![reader("81301", None), reader("81302", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    let repo = "acme/unseen";
    for app in ["81301", "81302"] {
        super::super::withdraw_reader_for_repo_until(app, repo, now + Duration::from_secs(3600));
    }
    let d = route_read_in(ws.path(), &r, &hygiene(repo), &v2(&cfg), now);
    assert_eq!(
        cause_of(&d),
        ExhaustCause::Unavailable,
        "no reader sees it: the writer reads it"
    );
    // Another repo of the same owner is unaffected.
    let d = route_read_in(ws.path(), &r, &hygiene("acme/seen"), &v2(&cfg), now);
    assert!(matches!(d, RouteDecision::Reader { .. }), "{d:?}");
}

#[test]
fn no_fresh_reader_dir_is_not_a_budget_exhaustion() {
    let r = roster(vec![reader("81311", None), reader("81312", None)]);
    // Token dirs published for another owner only: none for `acme`.
    let ws = workspace(&r, &["other"]);
    let cfg = RoutingConfig::default();
    let d = route_read_in(ws.path(), &r, &hygiene("acme/no-dir"), &v2(&cfg), SystemTime::now());
    assert_eq!(cause_of(&d), ExhaustCause::Unavailable);
}

#[test]
fn a_credential_withdrawal_beside_a_budget_one_is_not_a_budget_exhaustion() {
    let r = roster(vec![reader("81321", None), reader("81322", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    let until = now + Duration::from_secs(600);
    forge_read_pool::withdraw_scoped_budget_until(
        "81321",
        "acme",
        forge_read_pool::ResourceScope::Core,
        until,
    );
    // A refused credential: scoped, All, but no rate limit behind it.
    forge_read_pool::withdraw_scoped_until(
        "81322",
        "acme",
        forge_read_pool::ResourceScope::All,
        until,
    );
    let d = route_read_in(ws.path(), &r, &hygiene("acme/mixed"), &v2(&cfg), now);
    assert_eq!(cause_of(&d), ExhaustCause::Unavailable);
}

#[test]
fn every_reader_rate_limited_is_a_budget_exhaustion() {
    let r = roster(vec![reader("81331", None), reader("81332", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    for app in ["81331", "81332"] {
        forge_read_pool::withdraw_scoped_budget_until(
            app,
            "acme",
            forge_read_pool::ResourceScope::Core,
            now + Duration::from_secs(600),
        );
    }
    let d = route_read_in(ws.path(), &r, &hygiene("acme/dry-budget"), &v2(&cfg), now);
    assert_eq!(cause_of(&d), ExhaustCause::Budget);
    // A coverage-withdrawn reader is outside the repo's pool: one reader
    // rate-limited and the other unable to see the repo is still budget.
    let r = roster(vec![reader("81333", None), reader("81334", None)]);
    let ws = workspace(&r, &["acme"]);
    let repo = "acme/half-seen";
    forge_read_pool::withdraw_scoped_budget_until(
        "81333",
        "acme",
        forge_read_pool::ResourceScope::All,
        now + Duration::from_secs(60),
    );
    super::super::withdraw_reader_for_repo_until("81334", repo, now + Duration::from_secs(3600));
    let d = route_read_in(ws.path(), &r, &hygiene(repo), &v2(&cfg), now);
    assert_eq!(cause_of(&d), ExhaustCause::Budget);
}

#[test]
fn the_headroom_reserve_sheds_a_deferrable_read_before_a_real_limit() {
    let r = roster(vec![reader("81341", None), reader("81342", None)]);
    let ws = workspace(&r, &["acme"]);
    // Spill latch off: only the reserve is under test.
    let cfg = RoutingConfig::default().with_env(Some("0"), None);
    assert_eq!(cfg.shed_pct, DEFAULT_SHED_PCT);
    let now = SystemTime::now();
    let repo = "acme/reserve";
    let home = forge_read_pool::assignment_index(repo, 2).unwrap();
    let (home_app, other) = (r.readers[home].app_id.clone(), r.readers[1 - home].app_id.clone());

    // Home below shedPct: a deferrable read stays home.
    book_projected(&home_app, "acme", 75.0, now);
    let d = route_read_in(ws.path(), &r, &hygiene(repo), &v2(&cfg), now);
    assert_eq!(app_of(&d), home_app);

    // Home at shedPct, the other reader with headroom: it moves there.
    book_projected(&home_app, "acme", 85.0, now);
    book_projected(&other, "acme", 30.0, now);
    let d = route_read_in(ws.path(), &r, &hygiene(repo), &v2(&cfg), now);
    assert_eq!(app_of(&d), other);
    assert_eq!(placement_of(&d), Placement::Spill);

    // No spill target with headroom: shed (budget), with no withdrawal.
    book_projected(&other, "acme", 65.0, now);
    let d = route_read_in(ws.path(), &r, &hygiene(repo), &v2(&cfg), now);
    assert_eq!(cause_of(&d), ExhaustCause::Budget);
    // ... while a Gate read keeps the reserve: it is served at home.
    let gate = req(repo, Some("k"));
    let d = route_read_in(ws.path(), &r, &gate, &v2(&cfg), now);
    assert_eq!(app_of(&d), home_app);
}

#[test]
fn a_lone_reader_past_the_reserve_sheds_deferrable_reads() {
    let r = roster(vec![reader("81351", None)]);
    let ws = workspace(&r, &["acme"]);
    let cfg = RoutingConfig::default();
    let now = SystemTime::now();
    book_projected("81351", "acme", 82.0, now);
    let d = route_read_in(ws.path(), &r, &hygiene("acme/lone"), &v2(&cfg), now);
    assert_eq!(cause_of(&d), ExhaustCause::Budget);
    let d = route_read_in(ws.path(), &r, &req("acme/lone", Some("k")), &v2(&cfg), now);
    assert_eq!(app_of(&d), "81351", "Gate is never held back by the reserve");
}

#[test]
fn shed_pct_is_validated_between_target_and_full() {
    let parse = |v: serde_json::Value| {
        RoutingConfig::parse(&serde_json::json!({"forge": {"readPool": {"routing": v}}}))
    };
    let (cfg, w) = parse(serde_json::json!({"shedPct": 85}));
    assert_eq!((cfg.shed_pct, w.len()), (85.0, 0));
    let (cfg, w) = parse(serde_json::json!({"shedPct": 90}));
    assert_eq!((cfg.shed_pct, w.len()), (90.0, 0), "shedPct may equal spillFullPct");
    for bad in [
        serde_json::json!(60),
        serde_json::json!(95),
        serde_json::json!("x"),
    ] {
        let (cfg, w) = parse(serde_json::json!({"shedPct": bad}));
        assert_eq!((cfg.shed_pct, w.len()), (DEFAULT_SHED_PCT, 1), "{bad}: {w:?}");
    }
    // Unset under a lower spillFullPct: the default is clamped to it.
    let (cfg, w) = parse(serde_json::json!({
        "targetMaxPct": 50, "spillProjectedPct": 60, "spillFullPct": 75
    }));
    assert_eq!((cfg.shed_pct, w.len()), (75.0, 0));
}
