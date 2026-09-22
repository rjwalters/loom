//! Fixture tests for [`super`] — the Codex usage-limit reset-horizon parser
//! (issue #8539, acceptance box 4).
//!
//! The fixture that matters most is [`CAPTURED_USAGE_LIMIT_REFUSAL`]: the
//! wording an operator captured verbatim from a headless `codex exec` on a host
//! where every registered account was walled. Every other fixture here is a
//! deliberate *near miss* — a shape this parser must refuse rather than guess
//! at, because a wrong horizon puts a real deadline on a real account while a
//! missing one only costs the configured cooldown.

use chrono::{Datelike, Local, TimeZone, Timelike};

use super::*;

/// The host-local instant a fixture date denotes, as this module reads it.
fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Local
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("fixture instants are unambiguous in any sane host timezone")
        .with_timezone(&Utc)
}

fn refusal_with(horizon: &str) -> String {
    format!(
        "ERROR: You've hit your usage limit. Visit \
         https://chatgpt.com/codex/settings/usage to purchase more credits or try again at \
         {horizon}"
    )
}

/// The captured wording still says what this module was written against. If
/// this fails, the constant was edited — re-capture, do not "fix" the test.
#[test]
fn captured_refusal_keeps_its_shape() {
    let lowered = CAPTURED_USAGE_LIMIT_REFUSAL.to_ascii_lowercase();
    assert!(lowered.contains("hit your usage limit"));
    assert!(lowered.contains(TRY_AGAIN_AT));
    assert!(CAPTURED_USAGE_LIMIT_REFUSAL.ends_with("September 25, 2026 3:00 PM."));
}

/// Acceptance box 4, the PM form: the verbatim capture yields the instant it
/// names, in host-local time.
#[test]
fn captured_refusal_yields_its_pm_horizon() {
    assert_eq!(
        usage_limit_reset_in(CAPTURED_USAGE_LIMIT_REFUSAL),
        Some(local(2026, 9, 25, 15, 0)),
    );
}

/// Acceptance box 4, the AM form — the other half of the AM/PM pair the CLI
/// emits. `8:05` also pins that a single-digit, unpadded hour parses.
#[test]
fn am_horizon_parses_and_is_not_confused_with_pm() {
    let am = usage_limit_reset_in(&refusal_with("January 3, 2027 8:05 AM."))
        .expect("the AM form is the same shape as the PM form");
    assert_eq!(am, local(2027, 1, 3, 8, 5));
    assert_ne!(am, local(2027, 1, 3, 20, 5), "8:05 AM is not 20:05");

    let noon = usage_limit_reset_in(&refusal_with("January 3, 2027 12:30 PM."))
        .expect("12:30 PM is early afternoon");
    assert_eq!(noon, local(2027, 1, 3, 12, 30));
    let midnight = usage_limit_reset_in(&refusal_with("January 3, 2027 12:30 AM."))
        .expect("12:30 AM is just after midnight");
    assert_eq!(midnight, local(2027, 1, 3, 0, 30));
}

/// Trailing sentence punctuation belongs to the prose. Both the captured form
/// (period) and a bare end-of-line form must read the same.
#[test]
fn trailing_punctuation_is_not_part_of_the_date() {
    let with_period = usage_limit_reset_in(&refusal_with("March 9, 2026 7:15 AM."));
    let without = usage_limit_reset_in(&refusal_with("March 9, 2026 7:15 AM"));
    assert_eq!(with_period, Some(local(2026, 3, 9, 7, 15)));
    assert_eq!(with_period, without);
}

/// The whole line lowercased (a log pipeline that normalises case) still
/// matches: both the needles and the phrase are case-insensitive.
#[test]
fn matching_is_case_insensitive() {
    let lowered = refusal_with("September 25, 2026 3:00 PM.").to_ascii_lowercase();
    assert_eq!(usage_limit_reset_in(&lowered), Some(local(2026, 9, 25, 15, 0)),);
}

/// Shapes this parser refuses rather than guesses at. Each would be a *wrong*
/// deadline on a real account; `None` only costs the configured cooldown.
#[test]
fn unrecognised_horizon_shapes_yield_nothing() {
    for horizon in [
        "3:00 PM.",                      // no date: today or tomorrow is unknowable
        "September 25, 2026.",           // no time of day
        "2026-09-25T15:00:00Z.",         // an ISO instant the CLI does not emit
        "Septober 25, 2026 3:00 PM.",    // not a month
        "September 25, 2026 15:00 GMT.", // 24h clock with a zone
        "September 25, 2026 3:00 XM.",   // not a meridiem
    ] {
        assert_eq!(
            usage_limit_reset_in(&refusal_with(horizon)),
            None,
            "horizon {horizon:?} must not be guessed at"
        );
    }
    // A relative horizon carries no instant to read.
    assert_eq!(
        usage_limit_reset_in(
            "ERROR: You've hit your usage limit. Try again in 3 hours 12 minutes."
        ),
        None,
    );
}

/// The horizon is only ever read out of the refusal family itself. A line that
/// says "try again at <date>" for any other reason is not a usage limit.
#[test]
fn a_horizon_outside_the_refusal_family_is_ignored() {
    assert_eq!(
        usage_limit_reset_in(
            "warning: the forge is rate limited, try again at September 25, 2026 3:00 PM."
        ),
        None,
    );
    // The refusal family with no horizon at all is also nothing to read.
    assert_eq!(
        usage_limit_reset_in("ERROR: You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits."),
        None,
    );
}

/// Module-docs property 2: the CLI's fatal refusal is the last thing written
/// before exit, so an agent that quoted a refusal mid-run cannot displace it.
#[test]
fn the_last_refusal_in_the_region_wins() {
    let region = format!(
        "[agent] I hit a wall earlier today: \"You've hit your usage limit ... try again at \
         January 1, 2030 1:00 AM.\" — retrying now.\n\
         {}\n",
        refusal_with("September 25, 2026 3:00 PM.")
    );
    assert_eq!(
        usage_limit_reset_in(&region),
        Some(local(2026, 9, 25, 15, 0)),
        "the provider's own final refusal is the one that names the horizon"
    );
}

/// Region discipline: a horizon printed by an *earlier* run sharing the same
/// log file is never attributed to this dispatch.
#[test]
fn only_this_dispatch_region_is_scanned() {
    let contents = format!(
        "sweep_id=old\n{}\nsweep_id=current\nnothing to see here\n",
        refusal_with("January 1, 2030 1:00 AM.")
    );
    assert_eq!(usage_limit_reset_after(&contents, "sweep_id=current"), None);
    assert_eq!(
        usage_limit_reset_after(&contents, "sweep_id=old"),
        Some(local(2030, 1, 1, 1, 0)),
    );
    // An anchor that does not appear at all reads nothing, rather than falling
    // back to the whole file.
    assert_eq!(usage_limit_reset_after(&contents, "sweep_id=absent"), None);
}

/// The epoch helper the health path consumes agrees with the parsed instant.
#[test]
fn epoch_helper_matches_the_parsed_instant() {
    let contents = format!("anchor\n{}\n", CAPTURED_USAGE_LIMIT_REFUSAL);
    let instant = usage_limit_reset_after(&contents, "anchor").expect("captured form parses");
    assert_eq!(
        usage_limit_reset_epoch_after(&contents, "anchor"),
        Some(u64::try_from(instant.timestamp()).expect("a 2026 instant is positive")),
    );
}

/// A horizon is read as **host-local**, not as UTC. Pinning the wall-clock
/// fields rather than the epoch keeps this assertion true in every timezone CI
/// might run in — while still failing if the parser ever silently switched to
/// UTC on a host whose offset is non-zero.
#[test]
fn horizon_is_interpreted_in_host_local_time() {
    let parsed = usage_limit_reset_in(CAPTURED_USAGE_LIMIT_REFUSAL).expect("captured form parses");
    let wall = parsed.with_timezone(&Local);
    assert_eq!(
        (wall.year(), wall.month(), wall.day(), wall.hour(), wall.minute()),
        (2026, 9, 25, 15, 0),
    );
}
