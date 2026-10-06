//! `eta::doctor` verdicts (#10391): a healthy baseline, then one broken fact
//! per case, asserting the exact status and that the remedy names the fix.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use chrono::TimeZone;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap()
}

fn hours_ago(h: i64) -> DateTime<Utc> {
    now() - Duration::hours(h)
}

fn healthy() -> Facts {
    Facts {
        now: now(),
        config: ConfigFacts {
            eta_enabled: true,
            fit_enabled: true,
            fleet_refresh_enabled: true,
            interval_secs: 3600,
            otlp_exporter: true,
            native_exporter: true,
            authority: AuthorityFacts {
                host: Some("robb-studio".into()),
                reason: "fleet_refresh".into(),
                is_local: true,
                others: Vec::new(),
                detail: "robb-studio (fleet_refresh); this host is the authority".into(),
            },
        },
        data: DataFacts {
            gate: Gate::Captain,
            repos: vec![RepoFacts {
                repo: "acme/alpha".into(),
                has_reader: true,
                unsupported_forge: false,
                snapshot_as_of: Some(now() - Duration::minutes(20)),
                backfill_since: None,
            }],
            refresh_cycle: Some(RefreshCycleState {
                started_at: now() - Duration::minutes(20),
                gate: "captain".into(),
                captain: None,
                interval_secs: 3600,
                stop_reasons: [("complete".to_string(), 1)].into(),
                repos: Vec::new(),
            }),
        },
        fit: FitFacts {
            latest: Some(("fit1".into(), now() - Duration::hours(12))),
            today_exists: true,
            last_check: Some(fit_record("skipped", Some("today_exists"), 1)),
            published: PubStatus::default(),
        },
        serving: ServingFacts {
            fit_loaded: true,
            shadows: vec!["land:land-2026-10-04-twin-otter".into()],
            tallies: vec![HeuristicTally {
                kind: "land".into(),
                heuristic: "land-2026-10-04-twin-otter".into(),
                current: false,
                answered: 4,
                refused: BTreeMap::new(),
            }],
        },
        outcomes: OutcomeFacts {
            calibration_newest: Some(hours_ago(5)),
            pairs: vec![PairFacts {
                key: "land|land-v4|twin".into(),
                pairs: 12,
            }],
            oldest_pending: Some(hours_ago(30)),
            pending: 4,
            drift: Vec::new(),
        },
        backtest: BacktestFacts::default(),
    }
}

fn fit_record(outcome: &str, reason: Option<&str>, started_hours_ago: i64) -> EtaFitRecord {
    crate::observability::eta_fit::record_for(
        &match (outcome, reason) {
            ("skipped", Some("no_snapshots")) => {
                crate::observability::eta_fit::FitCheck::Skipped(run::FitSkip::NoSnapshots)
            }
            ("skipped", Some("today_exists")) => {
                crate::observability::eta_fit::FitCheck::Skipped(run::FitSkip::TodayExists {
                    fit_id: "fit1".into(),
                })
            }
            ("error", _) => crate::observability::eta_fit::FitCheck::Failed("write failed".into()),
            _ => crate::observability::eta_fit::FitCheck::Held,
        },
        crate::observability::eta_fit::Trigger::FleetRefresh,
        "host",
        hours_ago(started_hours_ago),
        1,
        &crate::eta::Provenance::current(),
    )
}

fn find<'a>(checks: &'a [Check], link: &str, check: &str) -> &'a Check {
    checks
        .iter()
        .find(|c| c.link == link && c.check == check)
        .unwrap_or_else(|| panic!("no {link}.{check} in {checks:#?}"))
}

#[test]
fn a_healthy_host_has_no_fail_or_warn_and_every_bad_status_has_a_remedy() {
    let checks = evaluate(&healthy());
    for c in &checks {
        assert!(matches!(c.status, Status::Ok | Status::Skip), "{}", c.render());
        assert!(c.remedy.is_none(), "{}", c.render());
    }
    assert!(!has_fail(&checks));
    let links: Vec<&str> = checks.iter().map(|c| c.link).collect();
    let mut dedup = links.clone();
    dedup.dedup();
    assert_eq!(
        dedup,
        [
            "config",
            "data",
            "fit",
            "serving",
            "snapshot_feed",
            "outcomes",
            "backtest"
        ]
    );
}

/// `(name, mutation, link, check, status, remedy needle)`.
type Case = (&'static str, fn(&mut Facts), &'static str, &'static str, Status, &'static str);

#[test]
fn each_broken_fact_gets_its_status_and_remedy() {
    let cases: Vec<Case> = vec![
        (
            "no_reader",
            |f| f.data.repos[0].has_reader = false,
            "data",
            "repo acme/alpha",
            Status::Fail,
            "fleet reader-App provisioning step",
        ),
        (
            "unsupported_forge is not told to install a reader App",
            |f| {
                f.data.repos[0].has_reader = false;
                f.data.repos[0].unsupported_forge = true;
            },
            "data",
            "repo acme/alpha",
            Status::Warn,
            "github.com repos only",
        ),
        (
            "stand_down with 0 snapshots",
            |f| {
                f.data.gate = Gate::StandDown {
                    captain: "robb-studio".into(),
                };
                f.data.repos[0].snapshot_as_of = None;
            },
            "data",
            "captain_gate",
            Status::Fail,
            "LOOM_ETA_FLEET_SNAPSHOT_DIR",
        ),
        (
            "stale fit over 36h",
            |f| f.fit.latest = Some(("old".into(), hours_ago(40))),
            "fit",
            "coefficient_file",
            Status::Fail,
            "eta fit --dry-run",
        ),
        (
            "no_snapshots",
            |f| f.fit.last_check = Some(fit_record("skipped", Some("no_snapshots"), 1)),
            "fit",
            "last_check",
            Status::Fail,
            "`data` link",
        ),
        (
            "fit error",
            |f| f.fit.last_check = Some(fit_record("error", None, 1)),
            "fit",
            "last_check",
            Status::Fail,
            "eta fit --dry-run",
        ),
        (
            "stalled refresh loop",
            |f| f.data.refresh_cycle.as_mut().unwrap().started_at = hours_ago(4),
            "data",
            "refresh_loop",
            Status::Fail,
            "loom-daemon",
        ),
        (
            "twin-otter no_model",
            |f| {
                f.serving.fit_loaded = false;
                f.serving.tallies[0].answered = 0;
                f.serving.tallies[0].refused = [("no_model".to_string(), 4)].into();
            },
            "serving",
            "land land-2026-10-04-twin-otter",
            Status::Fail,
            "`fit` link",
        ),
        (
            "ancient snapshot",
            |f| f.data.repos[0].snapshot_as_of = Some(hours_ago(30)),
            "data",
            "repo acme/alpha",
            Status::Fail,
            "refresh loop",
        ),
        (
            "snapshot behind",
            |f| f.data.repos[0].snapshot_as_of = Some(hours_ago(3)),
            "data",
            "repo acme/alpha",
            Status::Warn,
            "refresh_loop",
        ),
        (
            "no native exporter",
            |f| f.config.native_exporter = false,
            "snapshot_feed",
            "native_exporter",
            Status::Fail,
            "https",
        ),
        (
            "fit disabled",
            |f| f.config.fit_enabled = false,
            "config",
            "fit",
            Status::Fail,
            "LOOM_ETA_FIT_ENABLED",
        ),
        (
            "fit check loop silent",
            |f| f.fit.last_check = Some(fit_record("skipped", Some("today_exists"), 5)),
            "fit",
            "last_check",
            Status::Warn,
            "not checking",
        ),
    ];
    for (name, mutate, link, check, status, needle) in cases {
        let mut facts = healthy();
        mutate(&mut facts);
        let checks = evaluate(&facts);
        let c = find(&checks, link, check);
        assert_eq!(c.status, status, "{name}: {}", c.render());
        assert!(
            c.remedy.as_deref().is_some_and(|r| r.contains(needle)),
            "{name}: remedy should mention {needle:?}: {}",
            c.render()
        );
        assert_eq!(has_fail(&checks), checks.iter().any(|c| c.status == Status::Fail));
    }
}

#[test]
fn a_stand_down_host_does_not_demand_a_reader_and_no_remedy_on_ok_or_skip() {
    let mut f = healthy();
    f.data.gate = Gate::StandDown {
        captain: "robb-studio".into(),
    };
    f.data.repos[0].has_reader = false;
    let checks = evaluate(&f);
    assert_eq!(find(&checks, "data", "repo acme/alpha").status, Status::Ok);
    assert_eq!(find(&checks, "data", "captain_gate").status, Status::Ok);
    for c in &checks {
        assert_eq!(
            c.remedy.is_some(),
            matches!(c.status, Status::Warn | Status::Fail),
            "{}",
            c.render()
        );
    }
}

#[test]
fn the_alternates_check_is_skipped_until_10390() {
    let checks = evaluate(&healthy());
    let c = find(&checks, "snapshot_feed", "alternates");
    assert_eq!(c.status, Status::Skip);
    assert!(c.detail.contains("#10390"));
}

#[test]
fn due_now_reports_the_wait_for_fresh_snapshots() {
    let mut f = healthy();
    f.now = Utc.with_ymd_and_hms(2026, 10, 5, 2, 0, 0).unwrap();
    f.fit.today_exists = false;
    f.data.repos[0].snapshot_as_of = Some(Utc.with_ymd_and_hms(2026, 10, 4, 20, 0, 0).unwrap());
    let checks = evaluate(&f);
    assert!(find(&checks, "fit", "due_now")
        .detail
        .contains("waiting for fresh snapshots until 06:00Z"));
}

#[test]
fn render_puts_the_remedy_on_an_indented_line() {
    let mut f = healthy();
    f.data.repos[0].has_reader = false;
    let c = find(&evaluate(&f), "data", "repo acme/alpha").clone();
    let text = c.render();
    assert!(text.starts_with("FAIL data.repo acme/alpha: no_reader"));
    assert!(text.contains("\n    remedy: "));
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["status"], "FAIL");
    assert_eq!(json["link"], "data");
}

#[test]
fn published_fit_absent_is_a_skip_naming_the_fallback() {
    let c = evaluate(&healthy());
    let c = find(&c, "fit", "published_fit");
    assert_eq!(c.status, Status::Skip);
    assert!(c.detail.contains("no_model"), "{}", c.render());
}

#[test]
fn published_fit_states_map_to_status() {
    let mut f = healthy();
    f.fit.published = PubStatus {
        kind: Some(FetchKind::Installed),
        fit_id: Some("fitX".into()),
        captain_host: Some("cap".into()),
        published_at: Some(hours_ago(2)),
        ..PubStatus::default()
    };
    let c = evaluate(&f);
    let c = find(&c, "fit", "published_fit");
    assert_eq!(c.status, Status::Ok);
    assert!(c.detail.contains("fitX") && c.detail.contains("cap"), "{}", c.render());

    f.fit.published.kind = Some(FetchKind::Stale);
    let c = evaluate(&f);
    let c = find(&c, "fit", "published_fit");
    assert_eq!(c.status, Status::Warn);
    assert!(c.remedy.is_some() && c.detail.contains("own fit"), "{}", c.render());

    f.fit.published.kind = Some(FetchKind::Refused);
    f.fit.published.reason = Some("bad_signature".into());
    let c = evaluate(&f);
    let c = find(&c, "fit", "published_fit");
    assert_eq!(c.status, Status::Warn);
    assert!(c.detail.contains("bad_signature"), "{}", c.render());

    f.fit.published = PubStatus {
        publish_error: Some("403".into()),
        ..PubStatus::default()
    };
    let c = evaluate(&f);
    let c = find(&c, "fit", "published_fit");
    assert_eq!(c.status, Status::Warn);
    assert!(c.detail.contains("403"), "{}", c.render());
}

#[test]
fn the_authority_is_printed_and_a_missing_one_warns() {
    let ok = authority(&healthy().config.authority);
    assert_eq!(ok.status, Status::Ok);
    assert!(ok.detail.contains("robb-studio"), "{}", ok.detail);
    let none = AuthorityFacts {
        host: None,
        reason: "no_candidate".into(),
        is_local: false,
        others: Vec::new(),
        detail: "none (no_candidate)".into(),
    };
    let warn = authority(&none);
    assert_eq!(warn.status, Status::Warn);
    assert!(warn.remedy.unwrap().contains("fleet.etaAuthority"));
}

#[test]
fn drift_shows_the_tri_state_and_never_claims_serving_is_adjusted() {
    use crate::eta::regime::DriftState;
    let mut f = healthy();
    let row = |stage: &str, n_recent: u64, state: DriftState| DriftFacts {
        stage: stage.into(),
        heuristic: "land-v2".into(),
        n_recent,
        state,
    };
    f.outcomes.drift = vec![
        row("building", 2, DriftState::Unknown),
        row("judging", 9, DriftState::Stable),
        row("doctoring", 9, DriftState::Drifted),
    ];
    let checks = evaluate(&f);

    let unknown = find(&checks, "outcomes", "drift building");
    assert_eq!(unknown.status, Status::Ok);
    assert!(unknown.detail.contains("drift unknown"), "{}", unknown.detail);
    assert!(!unknown.detail.contains("no drift"), "{}", unknown.detail);

    let stable = find(&checks, "outcomes", "drift judging");
    assert_eq!(stable.status, Status::Ok);
    assert!(stable.detail.contains("no drift"), "{}", stable.detail);
    assert!(stable.detail.contains("land-v2"), "{}", stable.detail);

    let drifted = find(&checks, "outcomes", "drift doctoring");
    assert_eq!(drifted.status, Status::Warn);
    assert!(drifted.remedy.is_some());
    // Slice 1 does not apply the factor to served ETAs (#10563 review).
    assert!(drifted.detail.contains("NOT adjusted"), "{}", drifted.detail);
    assert!(!drifted.detail.contains("are scaled"), "{}", drifted.detail);
}

fn summary(ready: bool) -> crate::telemetry::kinds::eta_backtest::EtaBacktestSummaryRecord {
    crate::telemetry::kinds::eta_backtest::EtaBacktestSummaryRecord {
        summary_id: "s".into(),
        heuristic: "land-v4".into(),
        kind: "land".into(),
        compared_to: "land-v1".into(),
        as_of_day: "2026-10-04".into(),
        cutoff: now(),
        cases: 40,
        days: 8,
        wins: 7,
        ties: 0,
        win_rate: Some(0.875),
        ci_low: Some(0.529),
        ci_high: Some(0.978),
        min_folds: 7,
        gate_ready: ready,
        gate_detail: "land-v4 wins".into(),
        fit_id: None,
        loom: crate::eta::Provenance {
            version: "0.0.0".into(),
            revision: "0".repeat(40),
            tree_state: "clean".into(),
            complete: true,
        },
    }
}

#[test]
fn the_backtest_scoreboard_lists_each_challenger_and_warns_when_stale() {
    let mut f = healthy();
    f.backtest = BacktestFacts {
        enabled: true,
        state: Some(crate::eta::nightly_folds::State {
            written_at: hours_ago(5),
            day: "2026-10-04".into(),
            summaries: vec![summary(true)],
        }),
    };
    let checks = evaluate(&f);
    assert_eq!(find(&checks, "backtest", "nightly_folds").status, Status::Ok);
    let row = find(&checks, "backtest", "scoreboard land-v4");
    assert_eq!(row.status, Status::Ok);
    assert!(
        row.detail.contains("won 7/8") && row.detail.contains("READY"),
        "{}",
        row.render()
    );

    f.backtest.state.as_mut().unwrap().written_at = hours_ago(24 * 5);
    let checks = evaluate(&f);
    let c = find(&checks, "backtest", "nightly_folds");
    assert_eq!(c.status, Status::Warn);
    assert!(c.remedy.as_deref().unwrap().contains("fleet.captain"), "{}", c.render());

    f.backtest = BacktestFacts {
        enabled: true,
        state: None,
    };
    f.data.gate = Gate::StandDown {
        captain: "cap".into(),
    };
    let c = evaluate(&f);
    let c = find(&c, "backtest", "nightly_folds");
    assert_eq!(c.status, Status::Skip);
    assert!(c.detail.contains("cap"), "{}", c.render());

    // Fail-closed with no captain: the doctor says how to turn it on.
    f.data.gate = Gate::NoCaptain;
    let c = evaluate(&f);
    let c = find(&c, "backtest", "nightly_folds");
    assert_eq!(c.status, Status::Skip);
    assert!(c.detail.contains("fleet.captain"), "{}", c.render());
}
