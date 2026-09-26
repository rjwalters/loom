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
use chrono::{Local, Timelike};
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
    insert_codex_entry_with_prefixed_log(registry, issue, token, "", log_body)
}

/// [`insert_codex_entry_with_log`], with `log_prefix` written *before* this
/// sweep's `sweep_id=` anchor — i.e. text belonging to an earlier run that
/// happened to share the log file. Nothing there may be attributed to this
/// sweep (#8539: a call site passing the wrong anchor is exactly what this
/// distinguishes).
fn insert_codex_entry_with_prefixed_log(
    registry: &mut SweepRegistry,
    issue: u32,
    token: &str,
    log_prefix: &str,
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
    std::fs::write(
        &log_path,
        format!("{log_prefix}sweep_id={sweep_id} issue={issue} ====\n{log_body}"),
    )
    .unwrap();
    sweep_id
}

/// A horizon `days` out from now, as the (instant, refusal-line) pair a test
/// needs: the wall-clock rendering the Codex CLI would print, and the instant
/// the daemon should end up holding the account until.
///
/// Derived from *now* rather than hard-coded so these tests do not start
/// failing once a fixed fixture date drifts into the past — the
/// already-past-horizon rejection in `health::exhaustion_deadline` would
/// otherwise silently turn them into assertions about the fallback cooldown.
fn refusal_naming_a_horizon(days: i64) -> (u64, String) {
    let instant = (Local::now() + chrono::Duration::days(days))
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let rendered = instant.format("%B %d, %Y %I:%M %p").to_string();
    let epoch = u64::try_from(instant.timestamp()).unwrap();
    (
        epoch,
        format!(
            "ERROR: You've hit your usage limit. Visit \
             https://chatgpt.com/codex/settings/usage to purchase more credits or try again at \
             {rendered}.\n"
        ),
    )
}

/// #8539 call-site wiring: an exhaustion whose refusal names a reset horizon
/// holds the account until *that* instant, not until `now + cooldown`.
///
/// The end-to-end shape is the point — `codex_reset`'s own unit tests call the
/// parser directly and so cannot catch this call site passing the wrong
/// contents or the wrong anchor.
#[test]
fn provider_health_feedback_honours_the_refusals_own_reset_horizon() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let (expected_epoch, refusal) = refusal_naming_a_horizon(2);
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        74,
        "profile-e",
        &format!(
            "{refusal}# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-e \
             category=TOKEN_EXHAUSTED exit_code=1 model=none\n"
        ),
    );

    registry.apply_provider_health_feedback(&sweep_id, Some(1));

    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "profile-e".into(),
    };
    let health = tokens_pool::account_health(dir.path(), &id)
        .unwrap()
        .expect("health record written");
    assert_eq!(
        health.cooldown_until,
        Some(expected_epoch),
        "the hold must end at the horizon the provider itself named"
    );
}

/// The anchor argument at this call site is load-bearing: a refusal printed by
/// an *earlier* run sharing the log file is outside this sweep's region and
/// must not set its deadline. A call site that passed the whole file (or a
/// wrong anchor) would read that stale horizon and fail here.
#[test]
fn provider_health_feedback_ignores_a_horizon_outside_its_own_region() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let (stale_epoch, stale_refusal) = refusal_naming_a_horizon(30);
    let sweep_id = insert_codex_entry_with_prefixed_log(
        &mut registry,
        75,
        "profile-f",
        &format!("sweep_id=sweep-issue-1-earlier issue=1 ====\n{stale_refusal}"),
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-f \
         category=TOKEN_EXHAUSTED exit_code=1 model=none\n",
    );

    registry.apply_provider_health_feedback(&sweep_id, Some(1));

    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "profile-f".into(),
    };
    let health = tokens_pool::account_health(dir.path(), &id)
        .unwrap()
        .expect("health record written");
    let cooldown_until = health
        .cooldown_until
        .expect("an exhaustion sets a cooldown");
    assert_ne!(
        cooldown_until, stale_epoch,
        "the previous run's horizon must not become this sweep's deadline"
    );
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

// ---- #8931: reason-classified `loom.pool.account_marks` at this seam ----

/// The Codex half emits exactly one mark, with the persisted category's
/// reason, and nothing naming the account.
#[test]
fn a_persisted_codex_mark_emits_exactly_one_reason_classified_point() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        71,
        "profile-secret-name",
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-secret-name \
         category=SESSION_LIMIT exit_code=1 model=none\n",
    );
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        registry.apply_provider_health_feedback(&sweep_id, Some(1));
    });
    assert_eq!(captured.metrics.len(), 1, "{:?}", captured.metrics);
    let point = &captured.metrics[0];
    assert_eq!(point.name.as_str(), "loom.pool.account_marks");
    assert_eq!(point.labels["provider"], "codex");
    assert_eq!(point.labels["reason"], "session_limit");
    assert!(!serde_json::to_string(point).unwrap().contains("secret"));
}

/// A Codex outcome that records no hold (`SUCCESS`) emits no mark.
#[test]
fn a_codex_success_emits_no_mark() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        72,
        "profile-a",
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=profile-a \
         category=SUCCESS exit_code=0 model=none\n",
    );
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        registry.apply_provider_health_feedback(&sweep_id, Some(0));
    });
    assert!(captured.metrics.is_empty(), "{:?}", captured.metrics);
}

/// The native half: a pool-selected OpenCode launch whose harness reported
/// an exhaustion is bad-marked, and emits exactly one `exhausted` point
/// labelled with the pool namespace — never the account.
#[test]
fn a_native_api_key_mark_emits_exactly_one_reason_classified_point() {
    use crate::api_keys_pool::{ingest::LAUNCH_RECORD_PREFIX, paths, registry as keys};
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let pool = paths::per_repo_api_keys_dir(&registry.config.workspace_root);
    keys::add(&pool, "loomtest", "alpha-secret", "LOOM_TEST_KEY_8931", "fake-key", false).unwrap();
    let launch = serde_json::json!({
        "schema": 1, "runtime": "opencode", "provider": "zai-coding-plan",
        "model": "glm-5.3-flash", "profile": "zai-flash", "effort": null,
        "credentialSource": "pool", "credentialProvider": "loomtest",
        "credentialAccount": "alpha-secret", "usage": "native-json-events",
        "billing": "not-measured",
    });
    let sweep_id = insert_codex_entry_with_log(
        &mut registry,
        73,
        UNKNOWN_TOKEN_NAME,
        &format!(
            "{LAUNCH_RECORD_PREFIX}{launch}\nspawn-worker: runtime=opencode (from config)\n\
             # LOOM_CLI_START runtime=opencode\nError: insufficient balance for this account\n"
        ),
    );
    registry.entries.get_mut(&sweep_id).unwrap().runtime = "opencode".into();
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        registry.apply_provider_health_feedback(&sweep_id, Some(1));
    });
    assert_eq!(captured.metrics.len(), 1, "{:?}", captured.metrics);
    let point = &captured.metrics[0];
    assert_eq!(point.labels["provider"], "loomtest");
    assert_eq!(point.labels["reason"], "exhausted");
    assert!(!serde_json::to_string(point).unwrap().contains("alpha"));
}

/// The Claude insta-crash seam (`quarantine.rs`, frozen — its mark now goes
/// through [`SweepRegistry::mark_exhausted_account`]): one mark, classified
/// from the matched signature, never the account name or the banner text.
#[test]
fn a_claude_insta_crash_mark_emits_exactly_one_reason_classified_point() {
    use crate::sweep_registry::test_support::{insert_dead_running_with_log, seed_token_pool};
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    seed_token_pool(dir.path(), "agent-secret-3");
    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        74,
        0,
        "agent-secret-3",
        "loom-daemon dispatch: start\nYou're out of usage credits for this model.\n",
    );
    let (marked, captured) = crate::observability::ops::capture::capture(|| {
        registry.insta_crash_is_account_exhaustion(&sweep_id, 74)
    });
    assert!(marked);
    assert!(crate::tokens_pool::bad_tokens::is_bad(dir.path(), "agent-secret-3"));
    assert_eq!(captured.metrics.len(), 1, "{:?}", captured.metrics);
    let point = &captured.metrics[0];
    assert_eq!(point.labels["provider"], "claude");
    assert_eq!(point.labels["reason"], "model_credits");
    let wire = serde_json::to_string(point).unwrap();
    assert!(!wire.contains("secret") && !wire.contains("usage credits"), "{wire}");
}
