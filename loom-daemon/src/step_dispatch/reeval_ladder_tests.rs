//! #11900: the Champion re-evaluation ladder on the forge.

use super::*;
use chrono::TimeZone;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

#[test]
fn the_ladder_runs_from_6h_to_7d() {
    let hours: Vec<u64> = (0..8).map(|r| rung_delay(r).as_secs() / 3600).collect();
    assert_eq!(hours, vec![6, 12, 24, 48, 96, 168, 168, 168]);
    assert_eq!(rung_delay(u32::MAX), LAST_RUNG);
}

#[test]
fn advance_appends_then_climbs_and_leaves_the_park_record_alone() {
    let park =
        "<!-- loom:park Blocked by: (unstated) by=champion reason=\"champion merge-risk hold\" -->";
    let body = format!("Fixes the thing.\n\n{park}\n");
    let (b1, r1) = advance(&body, StepRole::ChampionMerge, t0());
    assert_eq!(r1.rung, 0);
    assert_eq!(r1.next, t0() + chrono::Duration::hours(6));
    assert!(b1.contains(park), "{b1}");
    assert_eq!(crate::park_record::parse(&b1), crate::park_record::parse(&body));
    assert_eq!(due(&b1, StepRole::ChampionMerge), Some(r1.next));
    assert_eq!(due(&b1, StepRole::ChampionPromote), None);

    let later = t0() + chrono::Duration::hours(7);
    let (b2, r2) = advance(&b1, StepRole::ChampionMerge, later);
    assert_eq!(r2.rung, 1);
    assert_eq!(r2.next, later + chrono::Duration::hours(12));
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
    assert_eq!(strip(body).as_deref(), Some("Body."));
    assert_eq!(strip("Body."), None);
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
    assert_eq!(due(&forge.body, StepRole::ChampionPromote), Some(r.next));

    assert!(clear(&mut forge, 12).unwrap());
    assert_eq!(forge.body, "Body.");
    assert!(!clear(&mut forge, 12).unwrap(), "no marker, no write");
    assert_eq!(forge.writes.len(), 2);

    forge.fail_read = true;
    assert!(record(&mut forge, 12, StepRole::ChampionPromote, t0()).is_err());
}
