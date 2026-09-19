//! Parser coverage for the `LOOM_TERMINAL_RESULT` **v2** record's `model=`
//! field (#8277) — the producer side of #8058 Phase 2's class-scoped health
//! marks.
//!
//! In its own sibling module rather than `crash_signals.rs`'s `mod tests`:
//! that file sits just under the file-size ratchet's 1000-line threshold and
//! adding these cases inline would push it over (see
//! `.loom/docs/file-size-policy.md`). Mirrors the existing
//! `crash_signals_empty_pool_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

/// Issue #8277: the v2 record adds a `model=` field so a class-scoped health
/// mark ([`crate::tokens_pool::health::record_terminal_for_model_at`]) has a
/// producer. A v1 record — an older adapter, or a sibling adapter that never
/// learned about v2 — still parses exactly as before, with no model at all:
/// the fail-safe degrade to account-wide health this issue requires, never a
/// dropped signal.
#[test]
fn terminal_result_parser_threads_the_model_field_on_v2() {
    assert_eq!(
        parse_terminal_result_after(
            "sweep_id=current\n# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-a \
             category=MODEL_CREDITS_EXHAUSTED exit_code=1 model=gpt-5-codex",
            "sweep_id=current"
        ),
        Some(TerminalResult {
            provider: AccountProvider::Codex,
            account: "profile-a".into(),
            category: TerminalClassification::ModelCreditsExhausted,
            exit_code: 1,
            model: Some("gpt-5-codex".into()),
        })
    );
    // `model=none` — the sentinel the adapter emits when no model was in
    // flight — carries the same "no model" meaning as an absent v1 field.
    assert_eq!(
        parse_terminal_result_after(
            "sweep_id=current\n# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-a \
             category=SUCCESS exit_code=0 model=none",
            "sweep_id=current"
        ),
        Some(TerminalResult {
            provider: AccountProvider::Codex,
            account: "profile-a".into(),
            category: TerminalClassification::Success,
            exit_code: 0,
            model: None,
        })
    );
    // v1 (5 fields, no model=) still parses, with model always None.
    assert_eq!(
        parse_terminal_result_after(
            "sweep_id=current\n# LOOM_TERMINAL_RESULT v=1 provider=codex account=profile-a \
             category=SUCCESS exit_code=0",
            "sweep_id=current"
        )
        .and_then(|result| result.model),
        None
    );
    // A v2 record missing its 6th field entirely (malformed — 5 fields like
    // v1, but tagged v=2) fails closed rather than silently parsing as v1.
    assert!(parse_terminal_result_after(
        "sweep_id=current\n# LOOM_TERMINAL_RESULT v=2 provider=codex account=a \
         category=SUCCESS exit_code=0",
        "sweep_id=current"
    )
    .is_none());
}
