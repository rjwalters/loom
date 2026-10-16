//! #11900: the Champion re-evaluation ladder on the forge.

use super::*;
use chrono::TimeZone;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

#[test]
fn the_ladder_runs_from_1h_to_7d() {
    // #12513 (operator ruling): 1, 1, 1, 2, 4, 8, 24, 48, 96 h, then 7 d.
    let hours: Vec<u64> = (0..12).map(|r| rung_delay(r).as_secs() / 3600).collect();
    assert_eq!(hours, vec![1, 1, 1, 2, 4, 8, 24, 48, 96, 168, 168, 168]);
    assert_eq!(FIRST_RUNG, Duration::from_secs(3600));
}

#[test]
fn the_cap_holds_at_7d_for_high_rungs() {
    for rung in [9, 10, 16, 31, 32, 1000, u32::MAX] {
        assert_eq!(rung_delay(rung), LAST_RUNG, "rung {rung}");
    }
    assert_eq!(LAST_RUNG, Duration::from_secs(7 * 24 * 3600));
    // A high rung's 7 d marker is honoured, not read as stale.
    let r = Reeval {
        step: StepRole::ChampionMerge,
        rung: 40,
        next: t0() + chrono::Duration::days(7),
    };
    assert_eq!(r.due_at(t0()), r.next);
}

/// #12513: a marker written under the old 6 h-doubling ladder (rung 3 was
/// 48 h; a `next` 4 d away) is further off than the new rung 3 delay (2 h),
/// so it is due now, and the Champion's next verdict writes one on the new
/// ladder, which is honoured: the rule fires once.
#[test]
fn a_marker_from_the_old_ladder_is_due_now_once() {
    let now = t0();
    let old = format!(
        "Body.\n\n{}",
        render(&Reeval {
            step: StepRole::ChampionMerge,
            rung: 3,
            next: now + chrono::Duration::days(4),
        })
    );
    assert_eq!(due(&old, StepRole::ChampionMerge, now), Some(now));
    let mut cooldowns = crate::step_dispatch::outcome_ledger::StepCooldowns::default();
    cooldowns.insert(9, StepRole::ChampionMerge, due(&old, StepRole::ChampionMerge, now).unwrap());
    assert!(!cooldowns.is_cooling(9, StepRole::ChampionMerge, now), "drawable this tick");

    let (rewritten, rec) = advance(&old, StepRole::ChampionMerge, now);
    assert_eq!(rec.rung, 4);
    assert_eq!(rec.next, now + chrono::Duration::hours(4));
    assert_eq!(due(&rewritten, StepRole::ChampionMerge, now), Some(rec.next));
    let later = now + chrono::Duration::hours(1);
    assert_eq!(
        due(&rewritten, StepRole::ChampionMerge, later),
        Some(rec.next),
        "still honoured"
    );
}

#[test]
fn a_marker_within_its_rung_delay_is_honoured() {
    let now = t0();
    let body = render(&Reeval {
        step: StepRole::ChampionPromote,
        rung: 0,
        next: now + chrono::Duration::minutes(30),
    });
    assert_eq!(
        due(&body, StepRole::ChampionPromote, now),
        Some(now + chrono::Duration::minutes(30))
    );
    // Clock skew between hosts: a rung-0 marker up to SKEW_GRACE past 1 h
    // stands; past it, it is from an older ladder.
    let skewed = Reeval {
        step: StepRole::ChampionPromote,
        rung: 0,
        next: now + chrono::Duration::minutes(64),
    };
    assert_eq!(skewed.due_at(now), skewed.next);
    let stale = Reeval {
        next: now + chrono::Duration::minutes(66),
        ..skewed
    };
    assert_eq!(stale.due_at(now), now);
    // A lapsed marker is just lapsed.
    let lapsed = Reeval {
        next: now - chrono::Duration::hours(3),
        ..stale
    };
    assert_eq!(lapsed.due_at(now), lapsed.next);
}

#[test]
fn advance_appends_then_climbs_and_leaves_the_park_record_alone() {
    let park =
        "<!-- loom:park Blocked by: (unstated) by=champion reason=\"champion merge-risk hold\" -->";
    let body = format!("Fixes the thing.\n\n{park}\n");
    let (b1, r1) = advance(&body, StepRole::ChampionMerge, t0());
    assert_eq!(r1.rung, 0);
    assert_eq!(r1.next, t0() + chrono::Duration::hours(1));
    assert!(b1.contains(park), "{b1}");
    assert_eq!(crate::park_record::parse(&b1), crate::park_record::parse(&body));
    assert_eq!(due(&b1, StepRole::ChampionMerge, t0()), Some(r1.next));
    assert_eq!(due(&b1, StepRole::ChampionPromote, t0()), None);

    let later = t0() + chrono::Duration::hours(7);
    let (b2, r2) = advance(&b1, StepRole::ChampionMerge, later);
    assert_eq!(r2.rung, 1);
    assert_eq!(r2.next, later + chrono::Duration::hours(1));
    assert_eq!(parse(&b2).len(), 1, "one record, replaced in place");
    assert!(b2.starts_with("Fixes the thing."));
}

#[test]
fn render_and_parse_round_trip_and_garbage_is_ignored() {
    let r = Reeval {
        step: StepRole::ChampionPromote,
        rung: 3,
        next: t0(),
    };
    let line = render(&r);
    assert_eq!(
        line,
        "<!-- loom:reeval step=champion_promote rung=3 next=2026-10-14T12:00:00Z -->"
    );
    assert_eq!(parse(&line), vec![r]);
    assert!(parse("<!-- loom:reeval step=nope rung=1 next=2026-10-14T12:00:00Z -->").is_empty());
    assert!(parse("<!-- loom:reeval step=judge rung=x -->").is_empty());
}

#[test]
fn strip_removes_only_the_marker() {
    let body = "Body.\n\n<!-- loom:reeval step=champion_merge rung=0 next=2026-10-14T18:00:00Z -->";
    assert_eq!(strip(body, StepRole::ChampionMerge).as_deref(), Some("Body."));
    assert_eq!(strip(body, StepRole::ChampionPromote), None, "another step's marker stays");
    assert_eq!(strip("Body.", StepRole::ChampionMerge), None);

    let both = format!(
        "{body}\n<!-- loom:reeval step=champion_promote rung=1 next=2026-10-14T18:00:00Z -->"
    );
    let (advanced, _) = advance(&both, StepRole::ChampionMerge, t0());
    assert_eq!(
        due(&advanced, StepRole::ChampionPromote, t0()),
        due(&both, StepRole::ChampionPromote, t0())
    );
    assert_eq!(parse(&advanced).len(), 2, "advance strips only its own step");
}

#[derive(Default)]
struct FakeForge {
    body: String,
    writes: Vec<String>,
    fail_read: bool,
}

impl BodyForge for FakeForge {
    fn body(&mut self, _number: u64) -> Result<String, String> {
        if self.fail_read {
            return Err("HTTP 502".into());
        }
        Ok(self.body.clone())
    }
    fn set_body(&mut self, _number: u64, body: &str) -> Result<(), String> {
        self.body = body.to_string();
        self.writes.push(body.to_string());
        Ok(())
    }
}

#[test]
fn record_and_clear_go_through_the_forge() {
    let mut forge = FakeForge {
        body: "Body.".into(),
        ..FakeForge::default()
    };
    let r = record(&mut forge, 12, StepRole::ChampionPromote, t0()).unwrap();
    assert_eq!(r.rung, 0);
    assert_eq!(due(&forge.body, StepRole::ChampionPromote, t0()), Some(r.next));

    assert!(!clear(&mut forge, 12, StepRole::ChampionMerge).unwrap(), "not its step");
    assert!(clear(&mut forge, 12, StepRole::ChampionPromote).unwrap());
    assert_eq!(forge.body, "Body.");
    assert!(
        !clear(&mut forge, 12, StepRole::ChampionPromote).unwrap(),
        "no marker, no write"
    );
    assert_eq!(forge.writes.len(), 2);

    forge.fail_read = true;
    assert!(record(&mut forge, 12, StepRole::ChampionPromote, t0()).is_err());
}
