//! Unit tests for [`super`] — the Codex availability probe (issue #8407).
//!
//! The rollout-log fixtures below are the **captured signal shape** the
//! research AC records: a `token_count` event carrying a `rate_limits` object
//! with `primary`/`secondary` windows. `nested_info_shape` pins the other
//! nesting the CLI has shipped, so a version bump that moves the object does
//! not silently blind the probe.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Duration, TimeZone, Utc};
use serial_test::serial;

use super::*;
use crate::tokens_pool::account_registry::{AccountId, CredentialKind, InventoryProvenance};
use crate::tokens_pool::health::{record_terminal_at, TerminalClassification};

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().unwrap()
}

fn descriptor(name: &str, dir: &Path, enabled: bool) -> AccountDescriptor {
    AccountDescriptor {
        id: AccountId {
            provider: AccountProvider::Codex,
            name: name.into(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: dir.to_path_buf(),
        enabled,
        provenance: InventoryProvenance::Shared,
        email: None,
    }
}

/// One `token_count` rollout line in the shape the CLI writes.
fn rollout_line(timestamp: &str, primary_percent: f64, secondary_percent: f64) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"output_tokens":5}}}},"rate_limits":{{"primary":{{"used_percent":{primary_percent},"window_minutes":300,"resets_in_seconds":600}},"secondary":{{"used_percent":{secondary_percent},"window_minutes":10080,"resets_in_seconds":86400}}}}}}}}"#
    )
}

/// Pin a file's mtime, so "newest log wins" is deterministic on hosts whose
/// filesystem timestamps are too coarse to separate two writes.
fn set_mtime(path: &Path, epoch_secs: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs);
    file.set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();
}

/// Write a rollout log into `<profile>/sessions/2026/09/21/`.
fn write_rollout(profile: &Path, name: &str, lines: &[String]) -> PathBuf {
    let dir = profile.join("sessions").join("2026").join("09").join("21");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    path
}

// ---------------------------------------------------------------------------
// Signal extraction
// ---------------------------------------------------------------------------

#[test]
fn extracts_both_windows_from_the_captured_event_shape() {
    let text = rollout_line("2026-09-21T10:00:00Z", 42.5, 7.0);
    let snapshot = extract_usage_snapshot(&text, at(0)).unwrap();
    assert_eq!(snapshot.observed_at, at(1_789_984_800)); // 2026-09-21T10:00:00Z
    let primary = snapshot.primary.unwrap();
    assert!((primary.used_fraction - 0.425).abs() < 1e-9);
    assert_eq!(primary.window_minutes, Some(300));
    assert_eq!(primary.resets_at, Some(snapshot.observed_at + Duration::seconds(600)));
    let secondary = snapshot.secondary.unwrap();
    assert!((secondary.used_fraction - 0.07).abs() < 1e-9);
    assert_eq!(secondary.window_minutes, Some(10080));
}

#[test]
fn takes_the_last_reading_and_skips_unparseable_lines() {
    let text = format!(
        "{}\n{{ not json\n{}\n",
        rollout_line("2026-09-21T10:00:00Z", 10.0, 1.0),
        rollout_line("2026-09-21T11:00:00Z", 80.0, 9.0),
    );
    let snapshot = extract_usage_snapshot(&text, at(0)).unwrap();
    assert!((snapshot.primary.unwrap().used_fraction - 0.80).abs() < 1e-9);
}

#[test]
fn nested_info_shape_is_still_found() {
    // The same object one level deeper — the other nesting the CLI has
    // shipped. A fixed-path reader would go blind here; the recursive search
    // must not.
    let text = r#"{"timestamp":"2026-09-21T10:00:00Z","msg":{"type":"token_count","info":{"rate_limits":{"primary":{"used_percent":99.5,"window_minutes":300,"resets_in_seconds":60}}}}}"#;
    let snapshot = extract_usage_snapshot(text, at(0)).unwrap();
    assert!((snapshot.primary.unwrap().used_fraction - 0.995).abs() < 1e-9);
    assert!(snapshot.secondary.is_none());
}

#[test]
fn an_absolute_reset_instant_is_accepted_when_there_is_no_countdown() {
    let text =
        r#"{"rate_limits":{"secondary":{"used_percent":50.0,"resets_at":"2026-09-28T00:00:00Z"}}}"#;
    let snapshot = extract_usage_snapshot(text, at(1_000)).unwrap();
    assert_eq!(
        snapshot.secondary.unwrap().resets_at,
        Some(at(1_790_553_600)) // 2026-09-28T00:00:00Z
    );
}

#[test]
fn a_log_with_no_rate_limits_yields_no_snapshot() {
    let text = r#"{"timestamp":"2026-09-21T10:00:00Z","type":"event_msg","payload":{"type":"agent_message","message":"hello"}}"#;
    assert!(extract_usage_snapshot(text, at(0)).is_none());
}

#[test]
fn a_rate_limits_object_with_no_usable_window_is_not_evidence() {
    let text = r#"{"rate_limits":{"primary":{"window_minutes":300}}}"#;
    assert!(extract_usage_snapshot(text, at(0)).is_none());
}

#[test]
fn latest_usage_snapshot_reads_the_newest_rollout_log() {
    let profile = tempfile::tempdir().unwrap();
    write_rollout(
        profile.path(),
        "rollout-2026-09-21T09-00-00-aaaa.jsonl",
        &[rollout_line("2026-09-21T09:00:00Z", 5.0, 1.0)],
    );
    let newer = write_rollout(
        profile.path(),
        "rollout-2026-09-21T12-00-00-bbbb.jsonl",
        &[rollout_line("2026-09-21T12:00:00Z", 64.0, 3.0)],
    );
    // Make the intended winner unambiguously newest on hosts with coarse
    // mtime resolution.
    set_mtime(&newer, 2_000_000_000);
    let snapshot = latest_usage_snapshot(profile.path()).unwrap();
    assert!((snapshot.primary.unwrap().used_fraction - 0.64).abs() < 1e-9);
}

#[test]
fn a_profile_with_no_sessions_directory_yields_no_snapshot() {
    let profile = tempfile::tempdir().unwrap();
    assert!(latest_usage_snapshot(profile.path()).is_none());
}

#[test]
fn the_probe_never_opens_auth_json() {
    // Defense in depth for the secret-free AC: a credential file sitting in
    // the profile root must be invisible to every surface this module
    // produces.
    let profile = tempfile::tempdir().unwrap();
    fs::write(profile.path().join("auth.json"), "recognizable-secret").unwrap();
    write_rollout(
        profile.path(),
        "rollout-2026-09-21T09-00-00-aaaa.jsonl",
        &[rollout_line("2026-09-21T09:00:00Z", 5.0, 1.0)],
    );
    let snapshot = latest_usage_snapshot(profile.path()).unwrap();
    assert!(!format!("{snapshot:?}").contains("recognizable-secret"));
    let account = descriptor("work", profile.path(), true);
    let row = assess_account(&account, None, Some(&snapshot), at(1_789_984_860));
    assert!(!format!("{:?}", row.to_json()).contains("recognizable-secret"));
}

// ---------------------------------------------------------------------------
// Assessment
// ---------------------------------------------------------------------------

/// A snapshot observed at `observed` with the given live-window percentages.
fn snapshot(observed: i64, primary: f64, secondary: f64) -> UsageSnapshot {
    UsageSnapshot {
        observed_at: at(observed),
        primary: Some(RateLimitWindow {
            used_fraction: primary,
            window_minutes: Some(300),
            resets_at: Some(at(observed) + Duration::seconds(600)),
        }),
        secondary: Some(RateLimitWindow {
            used_fraction: secondary,
            window_minutes: Some(10080),
            resets_at: Some(at(observed) + Duration::seconds(86_400)),
        }),
    }
}

#[test]
fn headroom_reads_available_and_carries_the_five_hour_utilization() {
    let profile = tempfile::tempdir().unwrap();
    let account = descriptor("work", profile.path(), true);
    let row = assess_account(&account, None, Some(&snapshot(1_000, 0.2, 0.05)), at(1_100));
    assert_eq!(row.status, "available");
    assert_eq!(row.s5h_utilization, Some(0.2));
    assert_eq!(row.s7d_utilization, Some(0.05));
}

#[test]
fn a_window_at_the_ceiling_reads_exhausted_with_its_reset_horizon() {
    let profile = tempfile::tempdir().unwrap();
    let account = descriptor("work", profile.path(), true);
    let row = assess_account(&account, None, Some(&snapshot(1_000, 0.30, 1.0)), at(1_100));
    assert_eq!(row.status, "exhausted");
    // `exhausted` binds to the long window, so the reported horizon is the
    // weekly reset — `check::limit_reset`'s own rule, reused not re-derived.
    assert_eq!(row.limit_reset(), Some(iso(at(1_000) + Duration::seconds(86_400)).as_str()));
}

#[test]
fn a_full_five_hour_window_reads_rate_limited_with_the_five_hour_horizon() {
    // The honesty rule: a full 5h window with weekly headroom is a
    // *recoverable* refusal that clears in minutes, so it must not be
    // reported as `exhausted` — which would advertise the days-out weekly
    // reset as this account's return date.
    let profile = tempfile::tempdir().unwrap();
    let account = descriptor("work", profile.path(), true);
    let row = assess_account(&account, None, Some(&snapshot(1_000, 1.0, 0.40)), at(1_100));
    assert_eq!(row.status, "rate_limited");
    assert_eq!(row.limit_reset(), Some(iso(at(1_000) + Duration::seconds(600)).as_str()));
}

#[test]
fn the_hold_a_reading_arms_uses_the_same_horizon_the_row_reports() {
    // One derivation, two consumers: the row's advertised reset and the
    // health hold's deadline can never disagree.
    let full_5h = snapshot(1_000, 1.0, 0.40);
    let (constraint, resets_at) = full_5h.constraint_at(at(1_100)).unwrap();
    assert_eq!(constraint, MeasuredConstraint::RateLimited);
    assert_eq!(resets_at, Some(at(1_000) + Duration::seconds(600)));

    let full_weekly = snapshot(1_000, 0.10, 1.0);
    let (constraint, resets_at) = full_weekly.constraint_at(at(1_100)).unwrap();
    assert_eq!(constraint, MeasuredConstraint::Exhausted);
    assert_eq!(resets_at, Some(at(1_000) + Duration::seconds(86_400)));

    assert!(snapshot(1_000, 0.10, 0.10)
        .constraint_at(at(1_100))
        .is_none());
}

#[test]
fn an_expired_reading_is_discarded_rather_than_carried_forward() {
    // The 2026-09 Claude-side trap (#7420) in Codex form: a week-old 100%
    // must not pin a healthy subscription out of rotation. Both windows'
    // resets are long past `now`, so neither is evidence.
    let profile = tempfile::tempdir().unwrap();
    let account = descriptor("work", profile.path(), true);
    let row = assess_account(&account, None, Some(&snapshot(1_000, 1.0, 1.0)), at(900_000));
    assert_eq!(row.status, "available");
    assert_eq!(row.s5h_utilization, None);
    assert_eq!(row.s7d_utilization, None);
}

#[test]
fn a_disabled_account_is_skipped_and_a_reauth_hold_is_blocked() {
    let profile = tempfile::tempdir().unwrap();
    let disabled = descriptor("off", profile.path(), false);
    assert_eq!(assess_account(&disabled, None, None, at(1_000)).status, "skipped");

    let workspace = tempfile::tempdir().unwrap();
    let account = descriptor("work", profile.path(), true);
    health::record_probe_at(
        workspace.path(),
        &account.id,
        health::ProbeOutcome::NotLoggedIn,
        "test",
        1_000,
    )
    .unwrap();
    let stored = health::account_health(workspace.path(), &account.id).unwrap();
    let row = assess_account(&account, stored.as_ref(), None, at(1_100));
    assert_eq!(row.status, "blocked");
    assert_eq!(row.error.as_deref(), Some("reauth_required"));
}

#[test]
fn a_recorded_exhaustion_hold_outranks_a_healthy_reading() {
    let workspace = tempfile::tempdir().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let account = descriptor("work", profile.path(), true);
    record_terminal_at(
        workspace.path(),
        &account.id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        1_000,
    )
    .unwrap();
    let stored = health::account_health(workspace.path(), &account.id).unwrap();
    let row = assess_account(&account, stored.as_ref(), Some(&snapshot(900, 0.1, 0.1)), at(1_100));
    assert_eq!(row.status, "exhausted");
    assert_eq!(row.error.as_deref(), Some("plan_exhausted"));
    assert!(row.limit_reset().is_some());
}

// ---------------------------------------------------------------------------
// run_check
// ---------------------------------------------------------------------------

fn registry(workspace: &Path, json: &str) {
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(workspace.join(".loom").join("accounts.json"), json).unwrap();
}

/// A workspace with two provisioned codex profiles, `alpha` and `beta`.
fn two_account_workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    fs::create_dir(profiles.path().join("alpha")).unwrap();
    fs::create_dir(profiles.path().join("beta")).unwrap();
    registry(
        workspace.path(),
        r#"{"version":1,"accounts":[
            {"provider":"codex","name":"alpha","credential_kind":"codex_home","credential_reference":"alpha","enabled":true},
            {"provider":"codex","name":"beta","credential_kind":"codex_home","credential_reference":"beta","enabled":true}
        ]}"#,
    );
    (workspace, profiles)
}

#[test]
#[serial]
fn a_host_with_no_codex_profiles_reports_an_empty_report() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
    let (report, effects) =
        run_check(workspace.path(), CheckOptions::default(), at(1_000)).unwrap();
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    assert!(report.accounts.is_empty());
    assert_eq!(effects.ranking_written, None);
}

#[test]
#[serial]
fn ranking_mode_writes_the_provider_namespaced_file_in_the_shared_format() {
    let (workspace, profiles) = two_account_workspace();
    write_rollout(
        &profiles.path().join("alpha"),
        "rollout-a.jsonl",
        &[rollout_line("2026-09-21T10:00:00Z", 3.0, 1.0)],
    );
    write_rollout(
        &profiles.path().join("beta"),
        "rollout-b.jsonl",
        &[rollout_line("2026-09-21T10:00:00Z", 100.0, 100.0)],
    );
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
    let now = at(1_789_984_860); // one minute after the readings above
    let (report, effects) = run_check(
        workspace.path(),
        CheckOptions {
            write_ranking: true,
        },
        now,
    )
    .unwrap();
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");

    assert_eq!(effects.ranking_written, Some(ranking_path(workspace.path())));
    let text = fs::read_to_string(ranking_path(workspace.path())).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // `available` sorts ahead of `exhausted`, and every row is the shared
    // `name|status|5h_util|limit_reset` shape.
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("alpha|available|0.03"));
    assert!(lines[1].starts_with("beta|exhausted|1.00|"));
    assert_eq!(report.accounts.len(), 2);
    assert_eq!(effects.marked_exhausted, vec!["beta".to_string()]);
    assert!(!text.contains("auth"));
}

#[test]
#[serial]
fn selection_lands_on_the_healthy_account_after_the_probe_marks_the_other() {
    // AC3: two accounts, one measured at its ceiling — consecutive
    // selections must all land on the healthy one, and neither the refusal
    // nor the choice may involve the Claude pool.
    let (workspace, profiles) = two_account_workspace();
    write_rollout(
        &profiles.path().join("alpha"),
        "rollout-a.jsonl",
        &[rollout_line("2026-09-21T10:00:00Z", 3.0, 1.0)],
    );
    write_rollout(
        &profiles.path().join("beta"),
        "rollout-b.jsonl",
        &[rollout_line("2026-09-21T10:00:00Z", 100.0, 100.0)],
    );
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
    let now = at(1_789_984_860);
    run_check(
        workspace.path(),
        CheckOptions {
            write_ranking: true,
        },
        now,
    )
    .unwrap();

    let inventory =
        crate::tokens_pool::account_inventory(workspace.path(), AccountProvider::Codex).unwrap();
    let now_epoch = u64::try_from(now.timestamp()).unwrap();
    for _ in 0..4 {
        let chosen = health::select_healthy_at(
            workspace.path(),
            AccountProvider::Codex,
            &inventory,
            now_epoch,
        )
        .unwrap();
        assert_eq!(chosen.id.name, "alpha");
    }
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn both_accounts_exhausted_produces_the_codex_scoped_refusal() {
    let (workspace, profiles) = two_account_workspace();
    for name in ["alpha", "beta"] {
        write_rollout(
            &profiles.path().join(name),
            "rollout-both.jsonl",
            &[rollout_line("2026-09-21T10:00:00Z", 100.0, 100.0)],
        );
    }
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
    let now = at(1_789_984_860);
    run_check(
        workspace.path(),
        CheckOptions {
            write_ranking: true,
        },
        now,
    )
    .unwrap();
    let inventory =
        crate::tokens_pool::account_inventory(workspace.path(), AccountProvider::Codex).unwrap();
    let error = health::select_healthy_at(
        workspace.path(),
        AccountProvider::Codex,
        &inventory,
        u64::try_from(now.timestamp()).unwrap(),
    )
    .unwrap_err();
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("Codex"), "{rendered}");
    assert!(!rendered.contains(".loom/tokens"), "{rendered}");
}

#[test]
#[serial]
fn a_bare_check_writes_nothing() {
    let (workspace, profiles) = two_account_workspace();
    write_rollout(
        &profiles.path().join("beta"),
        "rollout-b.jsonl",
        &[rollout_line("2026-09-21T10:00:00Z", 100.0, 100.0)],
    );
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
    let (report, effects) =
        run_check(workspace.path(), CheckOptions::default(), at(1_789_984_860)).unwrap();
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    assert_eq!(report.accounts.len(), 2);
    assert!(effects.marked_exhausted.is_empty());
    assert!(!ranking_path(workspace.path()).exists());
    assert!(!workspace.path().join(".loom/account-health.json").exists());
}

// ---------------------------------------------------------------------------
// Health feedback contract
// ---------------------------------------------------------------------------

#[test]
fn a_newer_headroom_reading_releases_a_probe_written_hold() {
    let workspace = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "work".into(),
    };
    health::record_availability_at(
        workspace.path(),
        &id,
        AvailabilityOutcome::Exhausted { until: 5_000 },
        AVAILABILITY_PROVENANCE,
        1_000,
    )
    .unwrap();
    let effect = health::record_availability_at(
        workspace.path(),
        &id,
        AvailabilityOutcome::Available { observed_at: 2_000 },
        AVAILABILITY_PROVENANCE,
        2_100,
    )
    .unwrap();
    assert_eq!(effect, health::AvailabilityEffect::ClearedExhaustionHold);
    let stored = health::account_health(workspace.path(), &id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.cooldown_until, None);
}

#[test]
fn a_stale_headroom_reading_never_releases_a_newer_hold() {
    let workspace = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "work".into(),
    };
    record_terminal_at(
        workspace.path(),
        &id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        2_000,
    )
    .unwrap();
    let effect = health::record_availability_at(
        workspace.path(),
        &id,
        AvailabilityOutcome::Available { observed_at: 1_000 },
        AVAILABILITY_PROVENANCE,
        2_100,
    )
    .unwrap();
    assert_eq!(effect, health::AvailabilityEffect::Unchanged);
    let stored = health::account_health(workspace.path(), &id)
        .unwrap()
        .unwrap();
    assert!(stored.cooldown_until.is_some_and(|until| until > 2_000));
}

#[test]
fn an_availability_reading_never_shortens_an_existing_hold_or_clears_a_reauth_one() {
    let workspace = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "work".into(),
    };
    health::record_availability_at(
        workspace.path(),
        &id,
        AvailabilityOutcome::Exhausted { until: 9_000 },
        AVAILABILITY_PROVENANCE,
        1_000,
    )
    .unwrap();
    health::record_availability_at(
        workspace.path(),
        &id,
        AvailabilityOutcome::Exhausted { until: 3_000 },
        AVAILABILITY_PROVENANCE,
        1_100,
    )
    .unwrap();
    let stored = health::account_health(workspace.path(), &id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.cooldown_until, Some(9_000));

    let reauth = AccountId {
        provider: AccountProvider::Codex,
        name: "dead".into(),
    };
    health::record_probe_at(
        workspace.path(),
        &reauth,
        health::ProbeOutcome::NotLoggedIn,
        "test",
        1_000,
    )
    .unwrap();
    let effect = health::record_availability_at(
        workspace.path(),
        &reauth,
        AvailabilityOutcome::Available { observed_at: 2_000 },
        AVAILABILITY_PROVENANCE,
        2_100,
    )
    .unwrap();
    assert_eq!(effect, health::AvailabilityEffect::Unchanged);
    let stored = health::account_health(workspace.path(), &reauth)
        .unwrap()
        .unwrap();
    assert_eq!(stored.reason, health::HealthReason::ReauthRequired);
}

#[test]
fn ranking_file_state_reports_absence_then_presence() {
    let workspace = tempfile::tempdir().unwrap();
    assert_eq!(ranking_file_state(workspace.path()), (false, None));
    fs::create_dir_all(workspace.path().join(".loom")).unwrap();
    fs::write(ranking_path(workspace.path()), "work|available\n").unwrap();
    let (present, age) = ranking_file_state(workspace.path());
    assert!(present);
    assert!(age.unwrap() < 60);
}
