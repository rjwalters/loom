//! Tests for the bounded zero-row settle.
//!
//! These are the assertions that used to live as scenarios (e)-(i) of
//! `defaults/scripts/tests/test-merge-pr-wait-for-checks-empty-settle.sh`,
//! moved here with the decision they exercise (#9091's Doctor pass). The shell
//! suite keeps the wiring — that `merge-pr.sh` consults this subcommand, obeys
//! its verdict, and fails closed when it cannot be run — because that is what
//! is still shell. What is decided, and under which knob values, is here.
//!
//! The through-line of every case below is that the bounded settle is a
//! NARROWING of #6169's wait, admissible only where the forge itself says no
//! gate can be pending. Two of these tests exist to prove it cannot be
//! widened: not by an operator knob, and not by a failed lookup.

use super::*;

/// The production defaults, with the knobs as [`settle_polls`] /
/// [`settle_interval`] would have resolved them when unset.
fn inputs(required: Required, polls: u64) -> Inputs {
    Inputs {
        pr: "42".to_string(),
        base_ref: "main".to_string(),
        polls,
        required,
        deadline_reached: false,
        settle_polls: settle_polls(None),
        settle_interval: settle_interval(None, 30),
        poll_interval: 30,
        timeout: 600,
    }
}

// --- (e) THE #9091 bug: a no-CI repo must settle in seconds, not 600s ------

#[test]
fn an_unprotected_base_settles_after_the_default_poll_count() {
    // Polls 1 and 2 keep waiting -- #6169's "never trust a single empty read"
    // is preserved verbatim, and is why the floor exists at all.
    for polls in 1..DEFAULT_SETTLE_POLLS {
        let d = decide(&inputs(Required::None, polls));
        assert_eq!(d.action, Action::Wait, "poll {polls} must not settle yet");
    }
    let d = decide(&inputs(Required::None, DEFAULT_SETTLE_POLLS));
    assert_eq!(d.action, Action::Settle);
    assert!(
        d.message.contains("requires no status-check contexts"),
        "the narration must say WHY the empty read was trusted early: {}",
        d.message
    );
}

#[test]
fn the_whole_bounded_settle_costs_seconds_not_the_ceiling() {
    // The regression bar #9091 states: the accumulated wait on a no-CI repo
    // must be far under the 600s LOOM_AUTO_MERGE_TIMEOUT that killed the
    // caller's process. Summing the sleeps the decisions ask for is the only
    // honest measure -- the bounded path deliberately uses a SHORTER spacing
    // than the pending path, so counting polls alone would not show it.
    let total: u64 = (1..=DEFAULT_SETTLE_POLLS)
        .map(|p| decide(&inputs(Required::None, p)).sleep_secs)
        .sum();
    assert!(
        total < 30,
        "bounded settle must cost seconds (got {total}s), not the 600s ceiling"
    );
    assert_eq!(total, (DEFAULT_SETTLE_POLLS - 1) * DEFAULT_SETTLE_INTERVAL);
}

// --- (f) Fail closed: unknown protection is not absent protection ----------

#[test]
fn a_failed_lookup_keeps_the_full_wait() {
    // Same disposition the sibling failing-check branch takes: a protection
    // lookup that errored tells us nothing, and "nothing" is never a licence
    // to shorten a guard.
    for polls in [1, DEFAULT_SETTLE_POLLS, 50] {
        let d = decide(&inputs(Required::LookupFailed, polls));
        assert_eq!(d.action, Action::Wait, "poll {polls} must keep waiting");
        assert_eq!(
            d.sleep_secs, 30,
            "and at the ORDINARY poll interval, not the short settle spacing"
        );
    }
    let mut late = inputs(Required::LookupFailed, 99);
    late.deadline_reached = true;
    let d = decide(&late);
    assert_eq!(d.action, Action::TimedOut);
    assert!(
        d.message.contains("remained empty"),
        "the fail-closed path still ends in #6169's whole-wait-elapsed line: {}",
        d.message
    );
}

#[test]
fn a_required_context_present_keeps_the_full_wait() {
    // #6169's actual danger: a required context that has not registered yet is
    // a gate that CAN block, so merging early would bypass it.
    for polls in [1, DEFAULT_SETTLE_POLLS, 50] {
        let d = decide(&inputs(Required::Present, polls));
        assert_eq!(d.action, Action::Wait, "poll {polls} must keep waiting");
        assert_eq!(d.sleep_secs, 30);
    }
}

#[test]
fn the_deadline_never_overrides_a_settle() {
    // Ordering matters: once the bounded settle applies there is no reason to
    // narrate a timeout the caller did not actually wait out.
    let mut inp = inputs(Required::None, DEFAULT_SETTLE_POLLS);
    inp.deadline_reached = true;
    assert_eq!(decide(&inp).action, Action::Settle);
}

// --- (g) The lookup is resolved once, and the caller carries the cache -----

#[test]
fn the_resolved_state_is_echoed_for_the_caller_to_cache() {
    // This is the single-lookup property, expressed where it actually lives:
    // each invocation is a fresh process, so "resolve once" means the caller
    // gets the answer back in a form it can replay. `Required::parse` of a
    // rendered decision's token must round-trip, or the cache silently
    // degrades into a per-poll re-lookup.
    for state in [Required::None, Required::Present, Required::LookupFailed] {
        let d = decide(&inputs(state, 1));
        assert_eq!(d.required, state);
        let token = render(&d).split(' ').nth(2).unwrap().to_string();
        assert_eq!(Required::parse(&token), state, "{state} must round-trip");
    }
}

#[test]
fn an_unrecognised_cache_token_re_resolves_rather_than_guesses() {
    for raw in ["", "unknown", "NONE", "lookup_failed", "true", "  "] {
        assert_eq!(Required::parse(raw), Required::Unknown, "{raw:?}");
    }
    assert_eq!(Required::parse(" none "), Required::None);
}

// --- (h) The knob cannot be turned back into the #6169 bug -----------------

#[test]
fn settle_polls_is_floored_at_two() {
    assert_eq!(settle_polls(None), DEFAULT_SETTLE_POLLS);
    assert_eq!(settle_polls(Some("")), DEFAULT_SETTLE_POLLS);
    // Settling on ONE empty read is #6169 itself -- no operator value, and no
    // typo, may restore it.
    for raw in ["1", "0", "abc", "-5", "2.5", "1 2"] {
        assert_eq!(settle_polls(Some(raw)), MIN_SETTLE_POLLS, "{raw:?}");
    }
    assert_eq!(settle_polls(Some("7")), 7);
}

#[test]
fn the_floor_is_enforced_through_the_decision_too() {
    // Not just in the parser: a caller that hands in a floor-violating value
    // directly must still not settle on the first read.
    let mut inp = inputs(Required::None, 1);
    inp.settle_polls = settle_polls(Some("1"));
    assert_eq!(decide(&inp).action, Action::Wait);
    inp.polls = 2;
    assert_eq!(decide(&inp).action, Action::Settle);
}

// --- (i) A garbage interval must never reach `sleep` -----------------------

#[test]
fn settle_interval_falls_back_to_the_poll_interval() {
    assert_eq!(settle_interval(None, 30), DEFAULT_SETTLE_INTERVAL);
    assert_eq!(settle_interval(Some(""), 30), DEFAULT_SETTLE_INTERVAL);
    for raw in ["not-a-number", "-1", "5s", "1e3"] {
        assert_eq!(settle_interval(Some(raw), 30), 30, "{raw:?}");
    }
    assert_eq!(settle_interval(Some("11"), 30), 11);
}

#[test]
fn a_garbage_interval_produces_the_conservative_longer_spacing() {
    let mut inp = inputs(Required::None, 1);
    inp.settle_interval = settle_interval(Some("not-a-number"), inp.poll_interval);
    let d = decide(&inp);
    assert_eq!(d.action, Action::Wait);
    assert_eq!(d.sleep_secs, 30, "falls back to the poll interval, never to 0");
    assert!(d.message.contains("re-polling in 30s"));
}

// --- The line contract the shell parses ------------------------------------

#[test]
fn the_rendered_line_is_sentinel_first_and_single_line() {
    let d = decide(&inputs(Required::None, 1));
    let line = render(&d);
    assert!(line.starts_with(WAIT), "{line}");
    assert!(!line.contains('\n'), "{line}");
    let mut f = line.splitn(4, ' ');
    assert_eq!(f.next(), Some(WAIT));
    assert_eq!(f.next(), Some("5"));
    assert_eq!(f.next(), Some("none"));
    assert_eq!(f.next(), Some(d.message.as_str()));
}

#[test]
fn every_action_renders_a_distinct_sentinel_with_a_numeric_sleep() {
    let mut settled = inputs(Required::None, DEFAULT_SETTLE_POLLS);
    settled.deadline_reached = false;
    let mut timed_out = inputs(Required::Present, 9);
    timed_out.deadline_reached = true;

    for (inp, want) in [
        (inputs(Required::Present, 1), WAIT),
        (settled, SETTLE),
        (timed_out, TIMEOUT),
    ] {
        let line = render(&decide(&inp));
        let mut f = line.split(' ');
        assert_eq!(f.next(), Some(want), "{line}");
        assert!(
            f.next().unwrap().parse::<u64>().is_ok(),
            "the sleep field must always be numeric so `sleep $2` cannot fault: {line}"
        );
    }
}

#[test]
fn a_newline_bearing_base_ref_cannot_forge_a_second_decision_line() {
    // `base_ref` comes from the forge's PR JSON -- untrusted external content.
    // The caller reads exactly one line, so a newline here would let the tail
    // of the narration be parsed as a decision nobody made.
    let mut inp = inputs(Required::Present, 1);
    inp.base_ref = format!("main\n{SETTLE} 0 none pwned");
    let line = render(&decide(&inp));
    assert!(!line.contains('\n'), "{line}");
    assert!(line.starts_with(WAIT), "{line}");
}
