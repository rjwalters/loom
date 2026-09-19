//! End-to-end coverage for threading the in-flight model from the adapter's
//! `LOOM_TERMINAL_RESULT` v2 record into #8058 Phase 2's class-scoped health
//! marks (#8277) — `apply_provider_health_feedback`'s side of the plumbing.
//!
//! Kept in a sibling file rather than an inline `mod tests`, matching the
//! `*_empty_pool_tests.rs` precedent, so the production module stays small
//! enough to hold in view (see `.loom/docs/file-size-policy.md`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::sweep_registry::test_support::fixture_registry;
use tempfile::tempdir;

/// Insert a `codex`-runtime entry with `token` already captured (so
/// [`SweepRegistry::apply_provider_health_feedback`] does not bail on the
/// `UNKNOWN_TOKEN_NAME` guard) and write `log_body` to its log path,
/// anchored so `parse_terminal_result_after` finds it.
fn insert_codex_entry_with_log(
    registry: &mut SweepRegistry,
    issue: u32,
    token: &str,
    log_body: &str,
) -> String {
    let sweep_id = format!("sweep-issue-{issue}-codex-health");
    let log_path = registry.compute_log_path(issue);
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(issue),
            pid: 2_147_483_640,
            token_name: token.to_string(),
            runtime: "codex".into(),
            runtime_source: None,
            log_path: log_path.clone(),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::write(&log_path, format!("sweep_id={sweep_id} issue={issue} ====\n{log_body}"))
        .unwrap();
    sweep_id
}

/// AC #1: a v2 `MODEL_CREDITS_EXHAUSTED` record naming its model produces a
/// `class_cooldowns` entry — not the account-wide `plan_exhausted`
/// cooldown — end to end from the adapter's log line through
/// `apply_provider_health_feedback` and `record_terminal_for_model`.
#[test]
fn provider_health_feedback_threads_the_model_into_a_class_scoped_hold() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        70,
        "profile-a",
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-a \
         category=MODEL_CREDITS_EXHAUSTED exit_code=1 model=gpt-5-codex\n",
    );

    registry.apply_provider_health_feedback(&sweep_id, Some(1));

    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "profile-a".into(),
    };
    let health = tokens_pool::account_health(dir.path(), &id)
        .unwrap()
        .expect("health record written");
    assert!(
        health.class_cooldowns.contains_key("gpt-5-codex"),
        "the named model class carries its own cooldown: {:?}",
        health.class_cooldowns
    );
    assert!(
        health.cooldown_until.is_none(),
        "a class-scoped hold must not also set the account-wide cooldown"
    );
}

/// AC #2: a record that OMITS the model (v1, or v2's `model=none`) still
/// produces today's account-wide hold — the fail-safe direction #8058
/// requires never regresses.
#[test]
fn provider_health_feedback_with_no_model_stays_account_wide() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        71,
        "profile-b",
        "# LOOM_TERMINAL_RESULT v=1 provider=codex account=profile-b \
         category=MODEL_CREDITS_EXHAUSTED exit_code=1\n",
    );

    registry.apply_provider_health_feedback(&sweep_id, Some(1));

    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "profile-b".into(),
    };
    let health = tokens_pool::account_health(dir.path(), &id)
        .unwrap()
        .expect("health record written");
    assert!(
        health.class_cooldowns.is_empty(),
        "no model named ⇒ no class-scoped entry: {:?}",
        health.class_cooldowns
    );
    assert!(
        health.cooldown_until.is_some(),
        "no model named ⇒ falls back to the account-wide cooldown"
    );
}

/// AC #2: an UNRECOGNIZED model name (fails [`health::model_class_of`]'s
/// normalization — here, a stray quote, which the `model=` charset would
/// never emit but the parser deliberately does not validate) degrades to the
/// account-wide hold rather than guessing a class.
///
/// This used to be exercised with `model=gpt-5@pinned`; #8380 made the `model@…`
/// suffix *recognized* (it collapses onto the bare model's class — see
/// `provider_health_feedback_with_a_pinned_model_narrows_to_the_bare_class`
/// below), so the fail-safe path needs an example that is still genuinely
/// unclassifiable.
#[test]
fn provider_health_feedback_with_unrecognized_model_stays_account_wide() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        72,
        "profile-c",
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-c \
         category=MODEL_CREDITS_EXHAUSTED exit_code=1 model=gpt-5\"pinned\n",
    );

    registry.apply_provider_health_feedback(&sweep_id, Some(1));

    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "profile-c".into(),
    };
    let health = tokens_pool::account_health(dir.path(), &id)
        .unwrap()
        .expect("health record written");
    assert!(
        health.class_cooldowns.is_empty(),
        "an unrecognized model name must not fabricate a class: {:?}",
        health.class_cooldowns
    );
    assert!(
        health.cooldown_until.is_some(),
        "an unrecognized model name still degrades to the account-wide cooldown"
    );
}

/// #8380: a PINNED/suffixed `model@…` value — the shape `spawn-codex.sh`'s
/// `model=` charset deliberately keeps `@` for — narrows to the bare model's
/// class instead of degrading to the account-wide hold. Without this, a fleet
/// that pins suffixed model IDs got zero benefit from #8058 Phase 2: every
/// credit exhaustion was still a whole-account outage.
#[test]
fn provider_health_feedback_with_a_pinned_model_narrows_to_the_bare_class() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        73,
        "profile-d",
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-d \
         category=MODEL_CREDITS_EXHAUSTED exit_code=1 model=gpt-5-codex@2026-01-01\n",
    );

    registry.apply_provider_health_feedback(&sweep_id, Some(1));

    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "profile-d".into(),
    };
    let health = tokens_pool::account_health(dir.path(), &id)
        .unwrap()
        .expect("health record written");
    assert_eq!(
        health.class_cooldowns.keys().collect::<Vec<_>>(),
        vec!["gpt-5-codex"],
        "the pinned ID is keyed by its bare class, never by the pin: {:?}",
        health.class_cooldowns
    );
    assert!(
        health.cooldown_until.is_none(),
        "a class-scoped hold must not also set the account-wide cooldown"
    );
}
