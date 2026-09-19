//! Unit tests for [`super`] — the provider-scoped account-health surface.
//!
//! Extracted to a sibling file (#8058 Phase 2) when the class-scoped
//! `MODEL_CREDITS_EXHAUSTED` work pushed `health.rs` over the 1000-line
//! ratchet: `.loom/docs/file-size-policy.md` names test-module extraction as
//! the intended split, and it is a pure move — every test below is unchanged
//! by the extraction itself.

use super::*;
use crate::tokens_pool::account_registry::{CredentialKind, InventoryProvenance};

fn descriptor(provider: AccountProvider, name: &str) -> AccountDescriptor {
    AccountDescriptor {
        id: AccountId {
            provider,
            name: name.into(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: PathBuf::from(name),
        enabled: true,
        provenance: InventoryProvenance::Shared,
        email: None,
    }
}

#[test]
fn exhausted_fails_over_and_expires_at_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b"
    );
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
    )
    .is_ok());
}

/// #8058 Phase 2 — the divergence this phase exists to create: a credit
/// exhaustion that names its model records a **class-scoped** hold, while
/// `TOKEN_EXHAUSTED` still records the account-wide one. Replaces
/// `model_credits_exhausted_is_recorded_identically_to_token_exhausted`,
/// which pinned the pre-#8058 equivalence.
#[test]
fn model_credits_exhausted_diverges_from_token_exhausted_when_the_class_is_known() {
    assert_eq!(
        "MODEL_CREDITS_EXHAUSTED"
            .parse::<TerminalClassification>()
            .unwrap(),
        TerminalClassification::ModelCreditsExhausted
    );

    // TOKEN_EXHAUSTED: account-wide, verbatim as before.
    let tmp = tempfile::tempdir().unwrap();
    let account = descriptor(AccountProvider::Codex, "a");
    record_terminal_for_model_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &account.id).unwrap().unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
    assert!(
        entry.class_cooldowns.is_empty(),
        "a plan/quota exhaustion is an account-level fact, never class-scoped"
    );

    // MODEL_CREDITS_EXHAUSTED: class-scoped, no account-wide cooldown.
    let tmp = tempfile::tempdir().unwrap();
    record_terminal_for_model_at(
        tmp.path(),
        &account.id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &account.id).unwrap().unwrap();
    assert_eq!(entry.reason, HealthReason::ModelCreditsExhausted);
    assert_eq!(entry.cooldown_until, None);
    assert_eq!(
        entry.class_cooldowns.get("gpt-5-codex").copied(),
        Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS)
    );
}

/// The starvation fix itself: one class hitting its ceiling must not cost
/// the account every other class — while the account-wide question keeps
/// its pre-#8058 answer, exactly as Phase 1's `is_bad` does.
#[test]
fn a_class_scoped_credit_hold_starves_only_its_own_class() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();

    // The class that ran out is blocked...
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        101
    )
    .is_err());
    // ...every other class is not...
    assert_eq!(
        select_healthy_for_model_at(
            tmp.path(),
            AccountProvider::Codex,
            &accounts,
            Some("gpt-5-mini"),
            101
        )
        .unwrap()
        .id
        .name,
        "a"
    );
    // ...and the account-wide question is unchanged: still blocked, and
    // still reported as a cooldown rather than a bare "unavailable".
    let error = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!(
            "model_class=gpt-5-codex cooldown_until={}",
            100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
        )),
        "{error}"
    );
    // Capacity must agree with selection, not report a phantom free seat.
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!((capacity.healthy, capacity.cooldown), (0, 1));

    // The hold ages out on its own schedule, and takes the reason with it.
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
    )
    .is_ok());
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.reason, HealthReason::Healthy);
    assert!(entry.class_cooldowns.is_empty());
}

/// The fail-safe half of the design constraint: a class-scoped mark may
/// only ever be NARROWER than an account-wide one. Every route that yields
/// no usable class must reproduce the pre-#8058 account-wide hold.
#[test]
fn a_credit_exhaustion_without_a_usable_class_stays_account_wide() {
    for model in [
        None,
        Some(""),
        Some("   "),
        Some("gpt 5 codex"),
        Some("\"x\""),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = vec![descriptor(AccountProvider::Codex, "a")];
        record_terminal_for_model_at(
            tmp.path(),
            &accounts[0].id,
            TerminalClassification::ModelCreditsExhausted,
            model,
            "adapter_v1",
            100,
        )
        .unwrap();
        let entry = account_health(tmp.path(), &accounts[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.reason, HealthReason::PlanExhausted, "model={model:?}");
        assert_eq!(
            entry.cooldown_until,
            Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS),
            "model={model:?}"
        );
        assert!(entry.class_cooldowns.is_empty(), "model={model:?}");
        // Account-wide means account-wide: naming a class cannot escape it.
        assert!(
            select_healthy_for_model_at(
                tmp.path(),
                AccountProvider::Codex,
                &accounts,
                Some("gpt-5-mini"),
                101
            )
            .is_err(),
            "model={model:?}"
        );
    }
}

/// Classes are independent of each other, and a class-scoped success
/// releases only the class it names.
#[test]
fn class_holds_are_independent_and_success_clears_only_its_own_class() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    for (model, at) in [("gpt-5-codex", 100), ("gpt-5-mini", 200)] {
        record_terminal_for_model_at(
            tmp.path(),
            &accounts[0].id,
            TerminalClassification::ModelCreditsExhausted,
            Some(model),
            "adapter_v1",
            at,
        )
        .unwrap();
    }
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.class_cooldowns.len(), 2);
    // Each ages from its own timestamp.
    assert_eq!(
        entry.class_cooldowns.get("gpt-5-codex").copied(),
        Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS)
    );
    assert_eq!(
        entry.class_cooldowns.get("gpt-5-mini").copied(),
        Some(200 + DEFAULT_EXHAUSTED_COOLDOWN_SECS)
    );

    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Success,
        Some("gpt-5-codex"),
        "adapter_v1",
        300,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.class_cooldowns.keys().collect::<Vec<_>>(), ["gpt-5-mini"]);
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        301
    )
    .is_ok());
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-mini"),
        301
    )
    .is_err());

    // A class-less success is the account-wide statement it always was.
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Success,
        "adapter_v1",
        302,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert!(entry.class_cooldowns.is_empty());
    assert_eq!(entry.reason, HealthReason::Healthy);
}

/// A narrower fact must never overwrite a wider live hold: an account
/// already held account-wide keeps that hold (and its reason) when a
/// class-scoped credit signal lands on top of it.
#[test]
fn a_class_hold_never_narrows_a_live_account_wide_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-mini"),
        "adapter_v1",
        101,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        102
    )
    .is_err());
}

/// Claude-family models collapse to the class Phase 1 already uses, so the
/// two phases can never disagree about a shared vocabulary; everything
/// else normalizes to itself.
#[test]
fn model_classes_share_phase_ones_vocabulary_where_it_applies() {
    assert_eq!(model_class_of("claude-opus-5").as_deref(), Some("opus"));
    assert_eq!(model_class_of("opus").as_deref(), Some("opus"));
    assert_eq!(model_class_of("OPUS").as_deref(), Some("opus"));
    assert_eq!(model_class_of("gpt-5-codex").as_deref(), Some("gpt-5-codex"));
    assert_eq!(model_class_of(" GPT-5-Codex ").as_deref(), Some("gpt-5-codex"));
    assert_eq!(model_class_of(""), None);
    assert_eq!(model_class_of("gpt 5 codex"), None);
}

/// A state file written before #8058 Phase 2 (no `class_cooldowns` key)
/// must still read back at the unchanged `SCHEMA_VERSION`, as account-wide
/// as the day it was written — the field is additive and optional, exactly
/// like `last_probe` (#6927) before it.
#[test]
fn pre_phase_two_state_reads_back_unchanged_and_stays_account_wide() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".loom")).unwrap();
    fs::write(
        state_path(tmp.path()),
        r#"{"version":1,"accounts":[{"provider":"codex","name":"a","reason":"plan_exhausted",
           "updated_at":100,"signal_provenance":"adapter_v1","cooldown_until":500,
           "consecutive_transient_failures":0}],"cursors":{}}"#,
    )
    .unwrap();
    let entry = read_state(tmp.path()).unwrap().accounts.remove(0);
    assert!(entry.class_cooldowns.is_empty());
    assert!(!entry.is_eligible_for_class_at(101, Some("gpt-5-codex")));
    assert!(entry.is_eligible_for_class_at(500, Some("gpt-5-codex")));

    // An account with no class hold serializes byte-identically to before:
    // the new key is skipped entirely when empty.
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".into(),
    };
    record_terminal_at(tmp.path(), &id, TerminalClassification::TokenExhausted, "adapter_v1", 1)
        .unwrap();
    let written = fs::read_to_string(state_path(tmp.path())).unwrap();
    assert!(!written.contains("class_cooldowns"), "{written}");
    assert!(written.contains("\"version\": 1"), "{written}");
}

#[test]
fn expired_survives_time_and_success_until_explicit_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let account = descriptor(AccountProvider::Codex, "a");
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::Success,
        "adapter_v1",
        u64::MAX - 1,
    )
    .unwrap();
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        std::slice::from_ref(&account),
        u64::MAX
    )
    .is_err());
    clear_reauth(tmp.path(), &account.id, "verified_reauth").unwrap();
    assert!(select_healthy_at(tmp.path(), AccountProvider::Codex, &[account], u64::MAX).is_ok());
}

#[test]
fn expired_survives_all_later_runtime_feedback_until_explicit_clear() {
    // #8058 Phase 2: the class-scoped signal is swallowed by the sticky
    // reauth hold exactly like every other terminal category — a broken
    // credential is broken for every class, so a narrower mark must not be
    // able to displace the wider permanent one.
    for (classification, model) in [
        (TerminalClassification::TokenExhausted, None),
        (TerminalClassification::Recoverable, None),
        (TerminalClassification::SessionLimit, None),
        (TerminalClassification::ModelCreditsExhausted, Some("gpt-5-codex")),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let account = descriptor(AccountProvider::Codex, "a");
        record_terminal_at(
            tmp.path(),
            &account.id,
            TerminalClassification::TokenExpired,
            "adapter_v1",
            1,
        )
        .unwrap();
        record_terminal_for_model_at(
            tmp.path(),
            &account.id,
            classification,
            model,
            "adapter_v1",
            2,
        )
        .unwrap();

        let health = account_health(tmp.path(), &account.id).unwrap().unwrap();
        assert_eq!(health.reason, HealthReason::ReauthRequired);
        assert_eq!(health.cooldown_until, None);
        assert!(health.class_cooldowns.is_empty());
        assert!(select_healthy_at(
            tmp.path(),
            AccountProvider::Codex,
            std::slice::from_ref(&account),
            u64::MAX
        )
        .is_err());
        // ...including for a caller that names a different class.
        assert!(select_healthy_for_model_at(
            tmp.path(),
            AccountProvider::Codex,
            std::slice::from_ref(&account),
            Some("gpt-5-mini"),
            u64::MAX
        )
        .is_err());
    }
}

#[test]
fn expired_transient_backoff_restores_fair_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Recoverable,
        "adapter_v1",
        100,
    )
    .unwrap();

    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b"
    );

    let deadline = 100 + DEFAULT_RECOVERABLE_BACKOFF_SECS;
    let first = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, deadline).unwrap();
    let second =
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, deadline).unwrap();
    assert_ne!(first.id, second.id);
    assert!([first.id.name, second.id.name].contains(&"a".to_string()));

    let recovered = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.reason, HealthReason::Healthy);
    assert_eq!(recovered.cooldown_until, None);
    assert_eq!(recovered.consecutive_transient_failures, 0);
}

#[test]
fn neutral_failures_do_not_poison_and_same_names_are_provider_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let codex = descriptor(AccountProvider::Codex, "same");
    let claude = descriptor(AccountProvider::Claude, "same");
    record_terminal_at(
        tmp.path(),
        &codex.id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    for category in [
        TerminalClassification::Timeout,
        TerminalClassification::Fatal,
        TerminalClassification::CwdDeleted,
        TerminalClassification::ModelRefusal,
    ] {
        record_terminal_at(tmp.path(), &claude.id, category, "adapter_v1", 2).unwrap();
    }
    assert!(account_health(tmp.path(), &claude.id).unwrap().is_none());
    assert_eq!(
        account_health(tmp.path(), &codex.id)
            .unwrap()
            .unwrap()
            .reason,
        HealthReason::ReauthRequired
    );
}

/// Issue #6927: a proactive probe that finds an account logged out must
/// exclude it from selection through the SAME `ReauthRequired` mechanism
/// a reactive `TOKEN_EXPIRED` uses — before any dispatch is attempted.
#[test]
fn a_not_logged_in_probe_excludes_the_account_and_is_reported_as_such() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    assert_eq!(
        record_probe_at(
            tmp.path(),
            &accounts[0].id,
            ProbeOutcome::NotLoggedIn,
            "session_probe",
            100,
        )
        .unwrap(),
        ProbeEffect::MarkedReauthRequired
    );
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.reason, HealthReason::ReauthRequired);
    assert_eq!(entry.last_probe, Some(100));
    assert_eq!(entry.cooldown_until, None);
    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b"
    );
    // Re-probing an already-held account is idempotent, not a re-mark.
    assert_eq!(
        record_probe_at(
            tmp.path(),
            &accounts[0].id,
            ProbeOutcome::NotLoggedIn,
            "session_probe",
            200,
        )
        .unwrap(),
        ProbeEffect::Unchanged
    );
}

/// A healthy probe IS the "independently verified reauth" `clear_reauth`
/// demands — it observed the live credential, which ordinary runtime
/// feedback cannot.
#[test]
fn a_logged_in_probe_releases_a_reauth_hold_but_touches_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let account = descriptor(AccountProvider::Codex, "a");
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        std::slice::from_ref(&account),
        2
    )
    .is_err());

    assert_eq!(
        record_probe_at(tmp.path(), &account.id, ProbeOutcome::LoggedIn, "session_probe", 3,)
            .unwrap(),
        ProbeEffect::ClearedReauthHold
    );
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        std::slice::from_ref(&account),
        4
    )
    .is_ok());

    // A healthy auth probe says nothing about quota, so it must not
    // rescue an exhausted account from its cooldown.
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        5,
    )
    .unwrap();
    assert_eq!(
        record_probe_at(tmp.path(), &account.id, ProbeOutcome::LoggedIn, "session_probe", 6,)
            .unwrap(),
        ProbeEffect::Unchanged
    );
    let entry = account_health(tmp.path(), &account.id).unwrap().unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(5 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
    assert_eq!(entry.last_probe, Some(6));
    // Nor does it fabricate a dispatch success.
    assert_eq!(entry.last_success, None);
}

#[test]
fn probe_records_survive_a_read_write_round_trip_and_reject_empty_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".into(),
    };
    record_probe_at(tmp.path(), &id, ProbeOutcome::NotLoggedIn, "session_probe", 7).unwrap();
    // Written state must still parse under `deny_unknown_fields` (the new
    // `last_probe` field is part of the schema, not a stowaway).
    assert_eq!(read_state(tmp.path()).unwrap().accounts[0].last_probe, Some(7));
    assert!(record_probe_at(tmp.path(), &id, ProbeOutcome::LoggedIn, "", 8).is_err());
}

#[test]
fn round_robin_is_persistent_and_capacity_is_honest() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    let first = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
    let second = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
    assert_ne!(first.id, second.id);
    let capacity = provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
    assert_eq!((capacity.raw, capacity.enabled, capacity.healthy), (2, 2, 2));
}

#[test]
fn state_never_contains_credentials_or_raw_output() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "safe-name".into(),
    };
    record_terminal_at(tmp.path(), &id, TerminalClassification::Recoverable, "adapter_v1", 1)
        .unwrap();
    let state = fs::read_to_string(state_path(tmp.path())).unwrap();
    assert!(!state.contains("auth.json"));
    assert!(!state.contains("recognizable-secret"));
}

#[test]
fn malformed_and_unknown_schema_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".loom")).unwrap();
    fs::write(state_path(tmp.path()), r#"{"version":99,"accounts":[]}"#).unwrap();
    assert!(read_state(tmp.path()).is_err());
    fs::write(state_path(tmp.path()), "{broken").unwrap();
    assert!(read_state(tmp.path()).is_err());
}

// ============================================================================
// Per-model-class capacity reporting (#8058 Phase 3)
// ============================================================================

/// AC2's degradation clause on this surface: a provider with no class-scoped
/// state reports an empty map, so every consumer falls back to `healthy` and
/// sees exactly the pre-#8058 shape. It must also stay off disk entirely.
#[test]
fn capacity_reports_no_class_state_when_there_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!((capacity.healthy, capacity.cooldown), (1, 1));
    assert!(capacity.healthy_by_class.is_empty());
    let json = serde_json::to_value(&capacity).unwrap();
    assert!(
        json.get("healthy_by_class").is_none(),
        "an empty map must not appear on the wire: {json}"
    );
}

/// The observability gap this phase closes: one class held on most accounts
/// reads as a nearly-dead provider account-wide, while the per-class count
/// shows the capacity that is actually still there.
#[test]
fn capacity_reports_healthy_counts_per_class() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
        descriptor(AccountProvider::Codex, "c"),
    ];
    for account in &accounts[..2] {
        record_terminal_for_model_at(
            tmp.path(),
            &account.id,
            TerminalClassification::ModelCreditsExhausted,
            Some("gpt-5-codex"),
            "adapter_v1",
            100,
        )
        .unwrap();
    }
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    // Account-wide: a live class hold still blocks the class-less question.
    assert_eq!((capacity.healthy, capacity.cooldown), (1, 2));
    // Per class: the hold really is on two accounts, and nothing else is.
    assert_eq!(capacity.healthy_by_class, BTreeMap::from([("gpt-5-codex".to_string(), 1)]));
    assert_eq!(
        serde_json::to_value(&capacity).unwrap()["healthy_by_class"],
        serde_json::json!({"gpt-5-codex": 1})
    );
}

/// Two classes held on disjoint accounts are counted independently — neither
/// class inherits the other's hold.
#[test]
fn capacity_counts_each_class_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
        descriptor(AccountProvider::Codex, "c"),
    ];
    for (account, model) in accounts.iter().zip(["gpt-5-codex", "gpt-5-mini"]) {
        record_terminal_for_model_at(
            tmp.path(),
            &account.id,
            TerminalClassification::ModelCreditsExhausted,
            Some(model),
            "adapter_v1",
            100,
        )
        .unwrap();
    }
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!(capacity.healthy, 1);
    assert_eq!(
        capacity.healthy_by_class,
        BTreeMap::from([
            ("gpt-5-codex".to_string(), 2),
            ("gpt-5-mini".to_string(), 2)
        ])
    );
}

/// Narrower, never wider: no class count may fall below the account-wide
/// `healthy`, because every account-wide hold is checked first and
/// identically. An account-wide exhaustion and a sticky reauth are invisible
/// to the class filter and stay counted out for every class.
#[test]
fn no_class_count_is_ever_below_the_account_wide_healthy_count() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
        descriptor(AccountProvider::Codex, "c"),
        descriptor(AccountProvider::Codex, "d"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_at(
        tmp.path(),
        &accounts[1].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[2].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!(capacity.healthy, 1);
    for (class, count) in &capacity.healthy_by_class {
        assert!(
            *count >= capacity.healthy,
            "class {class} reported {count}, below the account-wide {}",
            capacity.healthy
        );
    }
    // Only `d` is free of every hold; `c` adds itself back for no class but
    // its own, so `gpt-5-codex` stays at 1 while nothing else is reported.
    assert_eq!(capacity.healthy_by_class, BTreeMap::from([("gpt-5-codex".to_string(), 1)]));
}

/// An EXPIRED class hold names no live class, so it is not reported at all —
/// a class whose count would simply equal `healthy` tells an operator
/// nothing, and reporting it would make the map look permanently populated.
#[test]
fn an_expired_class_hold_is_not_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let capacity = provider_capacity_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS,
    )
    .unwrap();
    assert_eq!(capacity.healthy, 1);
    assert!(capacity.healthy_by_class.is_empty());
}
