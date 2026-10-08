//! Shadow lifecycle from the nightly folds (#10525): the promotion short-list
//! and retirement proposals.

use crate::eta::heuristics::{LAND_EVEN_LARK, LAND_TWIN_OTTER_B};
use crate::eta::nightly_folds::DayRecords;
use crate::eta::shadow_lifecycle::{
    dedup_key, file_proposals, retirement_proposals, shortlist, ProposalForge, MAX_STALE_DAYS,
    RETIREMENT_MIN_DAYS, SHORTLIST_SIZE,
};
use crate::eta::{Provenance, Registry, Tier};
use crate::telemetry::kinds::eta_backtest::EtaBacktestFoldRecord;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};

const CUR: &str = "land-v1";

/// Per-heuristic daily numbers: (delta vs current, pinball, coverage, late).
type Day = (f64, f64, f64, f64);

fn fold(id: &str, day: &str, d: Day) -> EtaBacktestFoldRecord {
    EtaBacktestFoldRecord {
        fold_id: format!("{id}-{day}"),
        heuristic: id.to_string(),
        kind: "land".into(),
        day: day.to_string(),
        cutoff: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
        compared_to: CUR.into(),
        is_current: false,
        n_cases: 40,
        n_answered: 40,
        answer_rate: Some(1.0),
        pinball4_loss_sec: Some(d.1),
        cov_25_75: Some(d.2),
        late_surprise: Some(d.3),
        paired_pairs: 40,
        delta_pinball4_loss_sec: Some(d.0),
        delta_answer_rate: Some(0.0),
        delta_late_surprise: Some(0.0),
        win: Some(d.0 < 0.0),
        fit_id: None,
        loom: Provenance::current(),
    }
}

fn day_str(start: NaiveDate, i: usize) -> String {
    (start + Duration::days(i as i64))
        .format("%Y-%m-%d")
        .to_string()
}

/// `n` days starting 2026-09-01; `rows[i]` maps id -> numbers for each day.
fn days(n: usize, per_id: &[(&str, Day)]) -> Vec<DayRecords> {
    let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    (0..n)
        .map(|i| {
            let day = day_str(start, i);
            DayRecords {
                day: day.clone(),
                folds: per_id.iter().map(|(id, d)| fold(id, &day, *d)).collect(),
                summaries: Vec::new(),
            }
        })
        .collect()
}

/// Wobble so the day-level variance is nonzero but the sign stays put.
fn wobbled(mut days: Vec<DayRecords>) -> Vec<DayRecords> {
    for (i, d) in days.iter_mut().enumerate() {
        for f in &mut d.folds {
            let w = if i % 2 == 0 { 0.5 } else { -0.5 };
            f.delta_pinball4_loss_sec = f.delta_pinball4_loss_sec.map(|v| v + w);
        }
    }
    days
}

fn now_after(days: &[DayRecords]) -> DateTime<Utc> {
    let last = NaiveDate::parse_from_str(&days.last().unwrap().day, "%Y-%m-%d").unwrap();
    Utc.from_utc_datetime(&(last + Duration::days(1)).and_hms_opt(6, 0, 0).unwrap())
}

#[test]
fn shortlist_keeps_top_two_by_paired_pinball_with_id_tiebreak() {
    let d = days(
        5,
        &[
            ("c-worse", (50.0, 900.0, 0.5, 0.1)),
            ("c-b", (-10.0, 800.0, 0.5, 0.1)),
            ("c-a", (-10.0, 800.0, 0.5, 0.1)),
            ("c-best", (-30.0, 700.0, 0.5, 0.1)),
        ],
    );
    let ids = ["c-worse", "c-b", "c-a", "c-best"];
    let s = shortlist(&d, CUR, &ids, &ids, now_after(&d));
    assert_eq!(s.selected, vec!["c-best", "c-a"]);
    assert_eq!(s.selected.len(), SHORTLIST_SIZE);
    assert!(s.admits("c-best").is_ok());
    let err = s.admits("c-worse").unwrap_err();
    assert!(err.contains("outside the top 2"), "{err}");
    // Deterministic across calls.
    assert_eq!(s, shortlist(&d, CUR, &ids, &ids, now_after(&d)));
}

#[test]
fn shortlist_fails_closed_without_fresh_evidence() {
    let ids = ["c-a"];
    let none = shortlist(&[], CUR, &ids, &ids, Utc::now());
    assert!(none.selected.is_empty());
    assert!(none.admits("c-a").unwrap_err().contains("no nightly fold"));

    let d = days(5, &[("c-a", (-10.0, 800.0, 0.5, 0.1))]);
    let stale = now_after(&d) + Duration::days(MAX_STALE_DAYS + 1);
    let s = shortlist(&d, CUR, &ids, &ids, stale);
    assert!(s.admits("c-a").unwrap_err().contains("stale"));
}

#[test]
fn shortlist_refuses_unknown_thin_nonfinite_and_changed_current() {
    let mut d = days(
        5,
        &[
            ("c-a", (-10.0, 800.0, 0.5, 0.1)),
            ("c-nan", (-1.0, 1.0, 0.5, 0.1)),
        ],
    );
    for day in &mut d {
        day.folds[1].delta_pinball4_loss_sec = Some(f64::NAN);
    }
    let ids = ["c-a", "c-nan", "c-thin"];
    let requested = ["c-a", "c-nan", "c-thin", "land-v1", "ghost"];
    let s = shortlist(&d, CUR, &ids, &requested, now_after(&d));
    assert_eq!(s.selected, vec!["c-a"]);
    for refused in ["c-nan", "c-thin"] {
        assert!(s.admits(refused).unwrap_err().contains("needed"), "{refused}");
    }
    for refused in ["land-v1", "ghost"] {
        assert!(s.admits(refused).unwrap_err().contains("not an eligible"), "{refused}");
    }
    // A different `current` makes every fold incomparable.
    let s2 = shortlist(&d, "land-v2", &ids, &ids, now_after(&d));
    assert!(s2.selected.is_empty());
}

fn retire_fixture(n: usize) -> Vec<DayRecords> {
    wobbled(days(
        n,
        &[
            // Worse than current, dominated by `good`.
            ("bad", (40.0, 900.0, 0.8, 0.2)),
            // Better than current on every axis.
            ("good", (-20.0, 700.0, 0.5, 0.1)),
            // Worse than current, but not dominated: its late surprise is lowest.
            ("odd", (30.0, 880.0, 0.5, 0.05)),
        ],
    ))
}

#[test]
fn retirement_proposes_only_the_dominated_worse_candidate() {
    let d = retire_fixture(RETIREMENT_MIN_DAYS);
    let p = retirement_proposals(&d, CUR, &["bad", "good", "odd"]);
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!(p[0].heuristic, "bad");
    assert_eq!(p[0].dominated_by, "good");
    assert!(p[0].delta_ci95.0 > 0.0);
    assert_eq!(p[0].decided_days, RETIREMENT_MIN_DAYS);
    // Reproducible.
    assert_eq!(p, retirement_proposals(&d, CUR, &["bad", "good", "odd"]));
}

#[test]
fn retirement_refuses_short_windows_baselines_and_missing_evidence() {
    let short = retire_fixture(RETIREMENT_MIN_DAYS - 1);
    assert!(retirement_proposals(&short, CUR, &["bad", "good", "odd"]).is_empty());

    let d = retire_fixture(RETIREMENT_MIN_DAYS);
    // A baseline or retired id is never in the eligible list.
    assert!(retirement_proposals(&d, CUR, &["good", "odd"]).is_empty());
    assert!(retirement_proposals(&d, "bad", &["bad", "good", "odd"]).is_empty());

    // Missing a required metric on every would-be dominator removes it.
    let mut missing = retire_fixture(RETIREMENT_MIN_DAYS);
    for day in &mut missing {
        day.folds[1].late_surprise = None;
        day.folds[2].late_surprise = None;
    }
    assert!(retirement_proposals(&missing, CUR, &["bad", "good", "odd"]).is_empty());

    // A worse CI that includes 0 is no proposal.
    let mut noisy = retire_fixture(RETIREMENT_MIN_DAYS);
    for (i, day) in noisy.iter_mut().enumerate() {
        day.folds[0].delta_pinball4_loss_sec = Some(if i % 2 == 0 { 60.0 } else { -55.0 });
    }
    assert!(retirement_proposals(&noisy, CUR, &["bad", "good", "odd"]).is_empty());
}

#[test]
fn domination_refuses_days_whose_scored_items_may_differ() {
    let names = ["bad", "good", "odd"];
    let proposed = |d: &[DayRecords]| retirement_proposals(d, CUR, &names);
    assert_eq!(proposed(&retire_fixture(RETIREMENT_MIN_DAYS)).len(), 1);

    // `odd` also dominates `bad`, so both dominators are made incomparable
    // in turn. `good` answered only part of each day's cohort (its own
    // aggregates are over an easier subset): it cannot dominate `bad`.
    let mut subset = retire_fixture(RETIREMENT_MIN_DAYS);
    for day in &mut subset {
        for f in &mut day.folds[1..3] {
            f.n_answered = 25;
            f.answer_rate = Some(0.625);
        }
    }
    assert!(proposed(&subset).is_empty());

    // Same answer counts, but `good` lacked a p90 on some cases, so its
    // pinball4/late are over fewer items than the cohort.
    let mut no_p90 = retire_fixture(RETIREMENT_MIN_DAYS);
    for day in &mut no_p90 {
        for f in &mut day.folds[1..3] {
            f.paired_pairs = 30;
        }
    }
    assert!(proposed(&no_p90).is_empty());

    // Only the dominator's incomparable days are dropped: with one day of
    // them the common window falls under the minimum.
    let mut one_day = retire_fixture(RETIREMENT_MIN_DAYS);
    for f in &mut one_day[3].folds[1..3] {
        f.n_answered = 39;
    }
    assert!(proposed(&one_day).is_empty());

    // An empty cohort proves nothing.
    let mut empty = retire_fixture(RETIREMENT_MIN_DAYS);
    for day in &mut empty {
        for f in &mut day.folds[1..3] {
            f.n_cases = 0;
            f.n_answered = 0;
            f.paired_pairs = 0;
        }
    }
    assert!(proposed(&empty).is_empty());
}

/// A fake forge: issues are bodies in memory; `find` greps them.
#[derive(Default)]
struct FakeForge {
    issues: Vec<(String, String)>,
    search_down: bool,
    file_down: bool,
}

impl ProposalForge for FakeForge {
    fn find(&mut self, key: &str) -> Result<Option<String>, String> {
        if self.search_down {
            return Err("search rate limited".into());
        }
        Ok(self
            .issues
            .iter()
            .position(|(_, b)| b.contains(key))
            .map(|i| format!("https://example.test/issues/{}", i + 1)))
    }

    fn file(&mut self, title: &str, body: &str) -> Result<String, String> {
        if self.file_down {
            return Err("rate limited".into());
        }
        self.issues.push((title.to_string(), body.to_string()));
        Ok(format!("https://example.test/issues/{}", self.issues.len()))
    }
}

#[test]
fn filing_is_idempotent_across_runs_and_hosts_and_keeps_failures_for_retry() {
    let d = retire_fixture(RETIREMENT_MIN_DAYS);
    let p = retirement_proposals(&d, CUR, &["bad", "good", "odd"]);
    let host_a = tempfile::tempdir().unwrap();
    let host_b = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut forge = FakeForge {
        search_down: true,
        ..FakeForge::default()
    };

    // A forge that cannot be searched refuses the filing (fails closed).
    assert!(file_proposals(host_a.path(), &p, now, &mut forge).is_err());
    forge.search_down = false;
    // A failed filing is reported and retried next run.
    forge.file_down = true;
    assert!(file_proposals(host_a.path(), &p, now, &mut forge).is_err());
    forge.file_down = false;
    assert!(forge.issues.is_empty());

    let report = file_proposals(host_a.path(), &p, now, &mut forge).unwrap();
    assert_eq!(report.filed, vec!["bad"]);
    // The unchanged window, re-run: nothing new.
    let again = file_proposals(host_a.path(), &p, now, &mut forge).unwrap();
    assert!(again.filed.is_empty());
    assert_eq!(again.already, vec!["bad"]);
    // A slid window still dedups on the heuristic.
    let later = retire_fixture(RETIREMENT_MIN_DAYS + 3);
    let p2 = retirement_proposals(&later, CUR, &["bad", "good", "odd"]);
    assert_ne!(p[0].evidence_id, p2[0].evidence_id);
    assert!(file_proposals(host_a.path(), &p2, now, &mut forge)
        .unwrap()
        .filed
        .is_empty());
    // Another host with its own empty ledger finds the marker on the forge.
    let other = file_proposals(host_b.path(), &p2, now, &mut forge).unwrap();
    assert!(other.filed.is_empty());
    assert_eq!(other.already, vec!["bad"]);

    assert_eq!(forge.issues.len(), 1);
    let (title, body) = &forge.issues[0];
    assert!(title.contains("bad") && title.contains("good"), "{title}");
    assert!(body.contains(&dedup_key("bad")));
    assert!(body.contains(&p[0].evidence_id));
    assert!(body.contains("proposal only"));
    // Nothing is unregistered: the registry is a compile-time table, and a
    // proposal for a real candidate leaves it registered.
    assert!(Registry::builtin().get(LAND_EVEN_LARK).is_some());
}

#[test]
fn a_real_candidate_is_proposed_from_its_folds_and_stays_registered() {
    let registry = Registry::builtin();
    let d = wobbled(days(
        RETIREMENT_MIN_DAYS,
        &[
            (LAND_EVEN_LARK, (40.0, 900.0, 0.8, 0.2)),
            (LAND_TWIN_OTTER_B, (-20.0, 700.0, 0.5, 0.1)),
        ],
    ));
    let eligible: Vec<&str> = registry
        .ids()
        .into_iter()
        .filter(|id| *id != CUR && registry.tier_of(id) == Some(Tier::Candidate))
        .collect();
    let p = retirement_proposals(&d, CUR, &eligible);
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!(p[0].heuristic, LAND_EVEN_LARK);
    assert_eq!(registry.tier_of(LAND_EVEN_LARK), Some(Tier::Candidate));
}

#[test]
fn builtin_candidates_are_the_only_eligible_ids() {
    let registry = Registry::builtin();
    for id in registry.ids() {
        let eligible = registry.tier_of(id) == Some(Tier::Candidate);
        assert_eq!(
            eligible,
            !matches!(id, "start-v1" | "finish-v1" | "land-v1" | "little-v0"),
            "{id}"
        );
    }
}

/// A workspace whose saved folds make `even-lark` a dominated, worse
/// candidate (against `twin-otter-b`), declaring `captain` as `fleet.captain`.
fn scheduled_root(captain: Option<&str>) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let text = captain
        .map_or_else(|| "{}".to_string(), |c| format!(r#"{{"fleet": {{"captain": "{c}"}}}}"#));
    std::fs::write(&config, text).unwrap();
    let d = wobbled(days(
        RETIREMENT_MIN_DAYS,
        &[
            (LAND_EVEN_LARK, (40.0, 900.0, 0.8, 0.2)),
            (LAND_TWIN_OTTER_B, (-20.0, 700.0, 0.5, 0.1)),
        ],
    ));
    std::fs::create_dir_all(crate::eta::nightly_folds::dir(root.path())).unwrap();
    for day in &d {
        let date = NaiveDate::parse_from_str(&day.day, "%Y-%m-%d").unwrap();
        std::fs::write(
            crate::eta::nightly_folds::day_path(root.path(), date),
            serde_json::to_string(day).unwrap(),
        )
        .unwrap();
    }
    root
}

fn gate(root: &std::path::Path, host: &str) -> crate::eta::job_owner::Owner {
    crate::eta::job_owner::resolve_with(root, host, |_| None)
}

#[test]
fn scheduled_filing_files_exactly_once_across_reruns_and_removes_nothing() {
    use crate::eta::retire_filing::{file_gated, proposals_for_root};
    let root = scheduled_root(Some("w1"));
    let before = Registry::builtin().ids();
    let mut forge = FakeForge::default();
    let now = Utc::now();
    for run in 0..3 {
        let proposed = proposals_for_root(root.path()).unwrap();
        assert_eq!(proposed.proposals.len(), 1, "{:?}", proposed.proposals);
        let report =
            file_gated(root.path(), &gate(root.path(), "w1"), &proposed.proposals, now, &mut forge)
                .unwrap();
        assert_eq!(report.filed.len(), usize::from(run == 0), "run {run}");
    }
    assert_eq!(forge.issues.len(), 1);
    assert!(forge.issues[0].1.contains(&dedup_key(LAND_EVEN_LARK)));
    assert_eq!(Registry::builtin().ids(), before, "nothing unregistered");
}

#[test]
fn scheduled_filing_refuses_a_non_captain_and_a_captainless_fleet() {
    use crate::eta::retire_filing::{file_gated, proposals_for_root};
    for captain in [Some("w1"), None] {
        let root = scheduled_root(captain);
        let proposed = proposals_for_root(root.path()).unwrap();
        let mut forge = FakeForge::default();
        let err = file_gated(
            root.path(),
            &gate(root.path(), "w2"),
            &proposed.proposals,
            Utc::now(),
            &mut forge,
        )
        .unwrap_err();
        assert!(err.contains("refusing to file"), "{err}");
        assert!(forge.issues.is_empty());
        assert!(!crate::eta::shadow_lifecycle::filed_path(root.path()).exists());
    }
}

#[test]
fn scheduled_filing_refuses_when_the_forge_cannot_be_searched_then_retries() {
    use crate::eta::retire_filing::{file_gated, proposals_for_root};
    let root = scheduled_root(Some("w1"));
    let proposed = proposals_for_root(root.path()).unwrap();
    let g = gate(root.path(), "w1");
    let mut forge = FakeForge {
        search_down: true,
        ..FakeForge::default()
    };
    let err = file_gated(root.path(), &g, &proposed.proposals, Utc::now(), &mut forge).unwrap_err();
    assert!(err.contains("not filing"), "{err}");
    assert!(forge.issues.is_empty());
    assert!(!crate::eta::shadow_lifecycle::filed_path(root.path()).exists());
    forge.search_down = false;
    let ok = file_gated(root.path(), &g, &proposed.proposals, Utc::now(), &mut forge).unwrap();
    assert_eq!(ok.filed, vec![LAND_EVEN_LARK]);
}

#[test]
fn retirement_filing_is_off_by_default_with_config_and_env_switches() {
    use crate::eta::config::resolve;
    let no_env = |_: &str| None;
    assert!(!resolve(&serde_json::json!({}), no_env).retirement_filing_enabled);
    let on =
        serde_json::json!({"autonomous": {"eta": {"nightlyFolds": {"retirementFiling": true}}}});
    assert!(resolve(&on, no_env).retirement_filing_enabled);
    let env_off = |k: &str| (k == "LOOM_ETA_RETIREMENT_FILING_ENABLED").then(|| "0".to_string());
    assert!(!resolve(&on, env_off).retirement_filing_enabled, "env beats config");
}
