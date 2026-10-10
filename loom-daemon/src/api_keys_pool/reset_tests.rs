//! Tests for [`super`] (issue #11286 item 2).
//!
//! **Every fixture in this file is SYNTHETIC / UNVERIFIED.** None was
//! captured from a real Z.ai (or any other) exhausted seat — see the module
//! docs' "Honest limits". They exercise the generic shapes the parser accepts,
//! and must be replaced or confirmed once real captures exist.

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
