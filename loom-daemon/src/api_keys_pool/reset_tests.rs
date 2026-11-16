//! Tests for [`super`] (issue #11286 item 2).
//!
//! The fixtures marked SYNTHETIC were not captured from any provider; they
//! exercise the generic shapes the parser accepts. The Z.ai tests at the end
//! use the real captures of 2026-11-16 (#12395), which live in
//! [`super::super::classify`].

use super::super::classify::{
    classify, CAPTURED_ZAI_FIVE_HOUR_LIMIT_EVENT, CAPTURED_ZAI_RATE_LIMIT_EVENT,
    CAPTURED_ZAI_WEEKLY_LIMIT_EVENT,
};
use super::*;

/// 2026-10-10T00:00:00Z.
const NOW: u64 = 1_791_590_400;

#[test]
fn reads_an_rfc3339_reset_field_from_a_json_error_body() {
    // SYNTHETIC: a provider-neutral JSON error body with a reset field.
    let body =
        r#"{"error":{"code":"limit","message":"limit reached","reset_at":"2026-10-12T00:00:00Z"}}"#;
    assert_eq!(parse_reset_at(body, NOW), Some(NOW + 2 * 86_400));
}

#[test]
fn reads_epoch_seconds_and_milliseconds_fields() {
    let secs = format!(r#"{{"error":{{"resetAt":{}}}}}"#, NOW + 3600);
    assert_eq!(parse_reset_at(&secs, NOW), Some(NOW + 3600));
    let millis = format!(r#"{{"next_reset_time":{}}}"#, (NOW + 7200) * 1000);
    assert_eq!(parse_reset_at(&millis, NOW), Some(NOW + 7200));
}

#[test]
fn reads_a_relative_retry_after() {
    assert_eq!(parse_reset_at(r#"{"error":{"retry_after":120}}"#, NOW), Some(NOW + 120));
    assert_eq!(parse_reset_at("upstream said Retry-After: 45", NOW), Some(NOW + 45));
}

#[test]
fn reads_a_prose_reset_line_with_a_timezone_less_timestamp_as_utc() {
    // SYNTHETIC prose in the generic "will reset at <timestamp>" shape.
    let line = "Error: usage limit reached. Your limit will reset at 2026-10-10 05:00:00.";
    assert_eq!(parse_reset_at(line, NOW), Some(NOW + 5 * 3600));
    let rfc = "quota exhausted; resets at 2026-10-17T00:00:00+08:00";
    assert_eq!(parse_reset_at(rfc, NOW), Some(NOW + 7 * 86_400 - 8 * 3600));
}

#[test]
fn the_last_plausible_match_wins() {
    let text = format!("{{\"reset_at\":{}}}\n{{\"reset_at\":{}}}\n", NOW + 60, NOW + 3600);
    assert_eq!(parse_reset_at(&text, NOW), Some(NOW + 3600));
}

#[test]
fn implausible_instants_are_ignored_not_guessed() {
    // In the past, too far out, a small ambiguous integer, garbage.
    for text in [
        r#"{"reset_at":"2020-01-01T00:00:00Z"}"#.to_string(),
        format!(r#"{{"reset_at":{}}}"#, NOW + MAX_RESET_HORIZON_SECS + 1),
        r#"{"reset_at":42}"#.to_string(),
        "will reset at soon".to_string(),
        "nothing to see".to_string(),
        String::new(),
    ] {
        assert_eq!(parse_reset_at(&text, NOW), None, "{text:?}");
    }
}

#[test]
fn a_parsed_reset_outranks_the_configured_window_and_the_default() {
    let text = format!(r#"{{"reset_at":{}}}"#, NOW + 5 * 86_400);
    assert_eq!(
        resolve_cooldown(Classification::Exhausted, &text, NOW, Some(3600)),
        Some((5 * 86_400, CooldownSource::ProviderReset))
    );
    // A rate limit that names its own reset uses it too.
    assert_eq!(
        resolve_cooldown(Classification::RateLimited, &text, NOW, None),
        Some((5 * 86_400, CooldownSource::ProviderReset))
    );
}

#[test]
fn the_configured_window_applies_to_exhaustion_only() {
    let week = 7 * 86_400;
    assert_eq!(
        resolve_cooldown(Classification::Exhausted, "insufficient balance", NOW, Some(week)),
        Some((week, CooldownSource::ConfiguredWindow))
    );
    assert_eq!(
        resolve_cooldown(Classification::RateLimited, "rate limit", NOW, Some(week)),
        Some((60, CooldownSource::Default))
    );
}

#[test]
fn no_reset_and_no_window_keeps_the_pre_11286_default() {
    assert_eq!(
        resolve_cooldown(Classification::Exhausted, "insufficient balance", NOW, None),
        Some((6 * 3600, CooldownSource::Default))
    );
    assert_eq!(
        resolve_cooldown(Classification::Exhausted, "", NOW, Some(0)),
        Some((6 * 3600, CooldownSource::Default))
    );
}

#[test]
fn a_credential_failure_never_gets_a_horizon_even_with_a_reset_in_the_text() {
    let text = format!(r#"{{"reset_at":{}}}"#, NOW + 3600);
    assert_eq!(
        resolve_cooldown(Classification::CredentialFailure, &text, NOW, Some(3600)),
        None
    );
}

/// 2026-11-16T16:49:39Z, the weekly capture's own `date` header.
const ZAI_WEEKLY_SEEN: u64 = 1_794_847_779;
/// 2026-11-16T11:07:07Z, the 5-hour capture's own `date` header.
const ZAI_FIVE_HOUR_SEEN: u64 = 1_794_827_227;

/// #12395, LIVE: the weekly refusal names `2026-11-17 01:52:07` with no
/// timezone, inside a JSON string. The event's `x-log-id` against its `date`
/// header puts that clock at UTC+8, so the seat is back at 17:52:07Z the same
/// day: 1 h 2 min 28 s after the capture, not 9 h.
#[test]
fn the_real_zai_weekly_limit_event_resolves_to_its_stated_reset_in_utc_plus_8() {
    let classification = classify(CAPTURED_ZAI_WEEKLY_LIMIT_EVENT, 1).unwrap();
    assert_eq!(classification, Classification::Exhausted);
    assert_eq!(
        resolve_cooldown(classification, CAPTURED_ZAI_WEEKLY_LIMIT_EVENT, ZAI_WEEKLY_SEEN, None),
        Some((3600 + 2 * 60 + 28, CooldownSource::ProviderReset))
    );
    // In a retained launch log, among other lines.
    let log = format!(
        "# LOOM_LAUNCH {{\"schema\":1}}\n{CAPTURED_ZAI_WEEKLY_LIMIT_EVENT}\n# LOOM_CLI_EXIT 1\n"
    );
    assert_eq!(parse_reset_at(&log, ZAI_WEEKLY_SEEN), Some(ZAI_WEEKLY_SEEN + 3748));
}

/// #12395, LIVE: the 5-hour refusal seen at 11:07:07Z names `19:09:58`, which
/// is 11:09:58Z. Read as UTC it would have held the seat eight hours past its
/// reset; the seat in fact gave no such refusal after 11:07Z.
#[test]
fn the_real_zai_five_hour_limit_event_resolves_to_minutes_not_hours() {
    let classification = classify(CAPTURED_ZAI_FIVE_HOUR_LIMIT_EVENT, 1).unwrap();
    assert_eq!(
        resolve_cooldown(
            classification,
            CAPTURED_ZAI_FIVE_HOUR_LIMIT_EVENT,
            ZAI_FIVE_HOUR_SEEN,
            Some(7 * 86_400)
        ),
        Some((2 * 60 + 51, CooldownSource::ProviderReset))
    );
}

/// #12395, LIVE: the request rate limit names no reset and keeps the 60 s
/// default. Its headers carry a date and a log id, which are not a reset.
#[test]
fn the_real_zai_rate_limit_event_names_no_reset_and_keeps_the_default() {
    let classification = classify(CAPTURED_ZAI_RATE_LIMIT_EVENT, 1).unwrap();
    assert_eq!(
        resolve_cooldown(classification, CAPTURED_ZAI_RATE_LIMIT_EVENT, 1_794_828_816, None),
        Some((60, CooldownSource::Default))
    );
}

/// The offset is taken only from the event's own evidence. SYNTHETIC
/// variations on the live shape.
#[test]
fn a_timezone_less_stamp_is_utc_unless_the_event_proves_its_wall_clock() {
    let event = |headers: &str| {
        format!(
            r#"{{"type":"error","error":{{"data":{{"message":"Your limit will reset at 2026-10-10 09:00:00","responseHeaders":{{{headers}}}}}}}}}"#
        )
    };
    // No headers: UTC, the conservative reading.
    assert_eq!(parse_reset_at(&event(""), NOW), Some(NOW + 9 * 3600));
    // Date and log id eight hours apart: UTC+8.
    let plus_8 = r#""date":"Sat, 10 Oct 2026 00:00:00 GMT","x-log-id":"20261010080001abc""#;
    assert_eq!(parse_reset_at(&event(plus_8), NOW), Some(NOW + 3600));
    // A log id that is not a stamp, or a difference that is no quarter-hour
    // offset, proves nothing.
    for unproven in [
        r#""date":"Sat, 10 Oct 2026 00:00:00 GMT","x-log-id":"abcdef0123456789""#,
        r#""date":"Sat, 10 Oct 2026 00:00:00 GMT","x-log-id":"20261010080700abc""#,
        r#""date":"Sat, 10 Oct 2026 00:00:00 GMT","x-log-id":"20261012000000abc""#,
        r#""date":"not a date","x-log-id":"20261010080001abc""#,
    ] {
        assert_eq!(parse_reset_at(&event(unproven), NOW), Some(NOW + 9 * 3600), "{unproven}");
    }
    // An explicit zone is never shifted.
    let explicit = format!(
        r#"{{"error":{{"message":"resets at 2026-10-10T09:00:00Z","headers":{{{plus_8}}}}}}}"#
    );
    assert_eq!(parse_reset_at(&explicit, NOW), Some(NOW + 9 * 3600));
}

/// SYNTHETIC: a stamp glued to trailing JSON on a line that does not parse as
/// JSON (a truncated event) is still read, as UTC.
#[test]
fn a_stamp_followed_by_json_punctuation_is_still_read() {
    let truncated =
        r#"{"message":"Your limit will reset at 2026-10-10 05:00:00","statusCode":429,"isRetr"#;
    assert_eq!(parse_reset_at(truncated, NOW), Some(NOW + 5 * 3600));
}

/// #12395, LIVE body: the egress proxy sees only the response body, with the
/// headers as separate lines. The offset evidence there is the same two
/// headers, so the hold is the same 2 min 51 s as the log reading, not 8 h.
#[test]
fn a_bare_zai_error_body_takes_its_offset_from_header_lines_beside_it() {
    let body = r#"{"error":{"code":"1308","message":"Usage limit reached for 5 hour. Your limit will reset at 2026-11-16 19:09:58"}}"#;
    let with_headers =
        format!("{body}\ndate: Mon, 16 Nov 2026 11:07:07 GMT\nx-log-id: 20261116190707REDACTED");
    assert_eq!(
        parse_reset_at(&with_headers, ZAI_FIVE_HOUR_SEEN),
        Some(ZAI_FIVE_HOUR_SEEN + 171)
    );
    // Without them the conservative UTC reading stands.
    assert_eq!(
        parse_reset_at(body, ZAI_FIVE_HOUR_SEEN),
        Some(ZAI_FIVE_HOUR_SEEN + 171 + 8 * 3600)
    );
}

/// A reset the event's own clock puts in the past is no reset at all. It
/// must not be re-read as UTC and come back eight hours in the future.
#[test]
fn a_lapsed_zai_reset_is_not_reread_as_utc() {
    let after_reset = ZAI_FIVE_HOUR_SEEN + 181;
    assert_eq!(parse_reset_at(CAPTURED_ZAI_FIVE_HOUR_LIMIT_EVENT, after_reset), None);
}

/// SYNTHETIC: a quoted or parenthesised stamp still parses, as it did before
/// stamp tokens were cut at their first non-stamp byte.
#[test]
fn a_quoted_or_parenthesised_stamp_is_still_read() {
    for line in [
        r#"limit resets at "2026-10-10 05:00:00""#,
        "limit resets at (2026-10-10 05:00:00)",
    ] {
        assert_eq!(parse_reset_at(line, NOW), Some(NOW + 5 * 3600), "{line}");
    }
}
