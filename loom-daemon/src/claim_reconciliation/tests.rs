use super::*;
use chrono::Duration;
use serial_test::serial;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

fn issue(number: u32, updated_at: Option<DateTime<Utc>>) -> BuildingIssue {
    BuildingIssue { number, updated_at }
}

fn journal_entry(repo: &str, issue: u32, pid: u32) -> JournalEntry {
    JournalEntry {
        repo: repo.to_string(),
        issue,
        pid,
        started_at: Utc::now(),
    }
}

#[test]
fn decide_keeps_when_journal_entry_pid_alive() {
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let action =
        decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| true, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_reclaims_when_journal_entry_pid_dead() {
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let action =
        decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| false, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
}

// --- #4556 live-claim veto ---

fn live_lock_evidence() -> crate::live_claim::LiveClaimEvidence {
    crate::live_claim::LiveClaimEvidence::ClaimLock {
        pid: 111,
        sweep_id: "sweep-issue-4275-live".to_string(),
    }
}

#[test]
fn live_claim_veto_downgrades_a_dead_pid_reclaim_to_keep() {
    // The confirmed #4275 misfire: `DeadPid { pid: 2781227 }` reverted
    // loom:building -> loom:issue at 03:08:15Z while the sweep was alive.
    let action = ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 2_781_227 });
    assert_eq!(
        apply_live_claim_veto(action, Some(&live_lock_evidence())),
        ReconcileAction::Keep
    );
}

#[test]
fn live_claim_veto_applies_to_every_reclaim_reason() {
    // A live sweep means the claim is legitimate regardless of which rule
    // proposed dropping it.
    for reason in [
        ReclaimReason::DeadPid { pid: 1 },
        ReclaimReason::DeadRunRegistry { pid: 1 },
        ReclaimReason::NoRecordStale { age_hours: 99.0 },
    ] {
        assert_eq!(
            apply_live_claim_veto(ReconcileAction::Reclaim(reason), Some(&live_lock_evidence())),
            ReconcileAction::Keep,
            "{reason:?} must be vetoed by a live claim"
        );
    }
}

#[test]
fn live_claim_veto_is_a_no_op_without_live_evidence() {
    let action = ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 1 });
    assert_eq!(apply_live_claim_veto(action, None), action, "no evidence => unchanged");
    assert_eq!(
        apply_live_claim_veto(ReconcileAction::Keep, Some(&live_lock_evidence())),
        ReconcileAction::Keep,
        "Keep is never escalated"
    );
}

#[test]
fn decide_keeps_when_no_record_and_within_grace() {
    let now = Utc::now();
    let recent = now - Duration::hours(1);
    let action =
        decide(&issue(42, Some(recent)), None, None, None, 10.0, &|_| true, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_reclaims_when_no_record_and_past_stale_threshold() {
    let now = Utc::now();
    let old = now - Duration::hours(5);
    let action = decide(&issue(42, Some(old)), None, None, None, 10.0, &|_| true, 4.0, now, false);
    match action {
        ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { age_hours }) => {
            assert!(age_hours >= 4.0);
        }
        other => panic!("expected NoRecordStale reclaim, got {other:?}"),
    }
}

#[test]
fn decide_keeps_at_exact_boundary_minus_epsilon() {
    let now = Utc::now();
    // Just under the threshold: still within grace.
    let almost = now - Duration::minutes(239); // 3h59m < 4h
    let action =
        decide(&issue(42, Some(almost)), None, None, None, 10.0, &|_| true, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_keeps_when_no_record_and_no_age_evidence() {
    let now = Utc::now();
    let action = decide(&issue(42, None), None, None, None, 10.0, &|_| true, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Keep, "fail-safe: no evidence => Keep");
}

// ------------------------------------------------------------------
// Startup-only immediate reclaim on total evidence absence (Issue #6615)
// ------------------------------------------------------------------

/// AC: on the STARTUP pass (`is_startup = true`), a `loom:building` claim
/// with no journal entry, no run-registry pid, and no no-progress
/// checkpoint evidence is reclaimed IMMEDIATELY -- even though the label
/// is fresh (well within `stale_hours`, which the periodic rule would
/// still respect) and even with no age evidence at all. This is the exact
/// shape a crash between `begin_issue_dispatch`'s label flip and
/// `finish_issue_dispatch`'s journal write leaves behind.
#[test]
fn decide_reclaims_immediately_at_startup_with_zero_evidence_and_fresh_label() {
    let now = Utc::now();
    let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
    let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now, true);
    assert_eq!(
        action,
        ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup),
        "total absence of evidence must reclaim immediately on the startup pass, not wait \
             out stale_hours"
    );
}

#[test]
fn decide_reclaims_immediately_at_startup_even_with_no_age_evidence() {
    // No `updated_at` at all -- the periodic rule's fail-safe (Keep) must
    // NOT apply here; the startup rule fires ahead of it.
    let now = Utc::now();
    let action = decide(&issue(42, None), None, None, None, 10.0, &|_| true, 4.0, now, true);
    assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup));
}

/// Edge case 1 (curator's Test Plan): the exact same zero-evidence shape,
/// mid-steady-state (`is_startup = false`), must NOT be reclaimed early --
/// this is what protects a manually-spawned `/loom:sweep` that has not
/// yet written a journal entry. Byte-for-byte the pre-#6615 behavior.
#[test]
fn decide_does_not_reclaim_zero_evidence_mid_steady_state() {
    let now = Utc::now();
    let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
    let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now, false);
    assert_eq!(
        action,
        ReconcileAction::Keep,
        "is_startup=false must preserve the existing age-gated behavior"
    );
}

/// Edge case 2 (curator's Test Plan): a daemon restart with a genuinely
/// live child from a prior dispatch (journal entry recording a live pid)
/// must be KEPT even on the startup pass -- the journal/run-registry
/// checks are unconditional and run before the `is_startup` fallback is
/// ever consulted.
#[test]
fn decide_keeps_live_journal_pid_across_restart_even_at_startup() {
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let action =
        decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| true, 4.0, now, true);
    assert_eq!(
        action,
        ReconcileAction::Keep,
        "a live journal pid must win regardless of is_startup"
    );
}

#[test]
fn decide_keeps_live_run_registry_pid_across_restart_even_at_startup() {
    let now = Utc::now();
    let action =
        decide(&issue(42, None), None, Some(222), None, 10.0, &|pid| pid == 222, 4.0, now, true);
    assert_eq!(
        action,
        ReconcileAction::Keep,
        "a live run-registry pid must win regardless of is_startup"
    );
}

/// A DEAD journal pid at startup must still surface as `DeadPid`, not
/// the new `NoRecordAtStartup` reason -- the more specific evidence takes
/// priority even when `is_startup` is set.
#[test]
fn decide_dead_journal_pid_at_startup_keeps_deadpid_reason() {
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let action =
        decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| false, 4.0, now, true);
    assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
}

#[test]
fn plan_reclaims_zero_evidence_issue_immediately_at_startup() {
    let now = Utc::now();
    let journal = SweepJournal::default();
    let issues = vec![issue(1, Some(now - Duration::minutes(1)))];
    let decisions = plan(
        "/repo/a",
        &issues,
        &journal,
        &|_| None,
        &|_| None,
        10.0,
        &|_| true,
        4.0,
        now,
        true,
    );
    assert_eq!(decisions[0], (1, ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup)));
}

#[test]
fn decide_dead_pid_overrides_label_age() {
    // Even a freshly-labeled issue must be reclaimed once its recorded
    // PID is provably dead — the journal is authoritative when present.
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
    let action = decide(&fresh_issue, Some(&entry), None, None, 10.0, &|_| false, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
}

// ------------------------------------------------------------------
// Run-registry evidence source (Issue #4348)
// ------------------------------------------------------------------

#[test]
fn decide_keeps_when_run_registry_pid_alive_and_no_journal_entry() {
    let now = Utc::now();
    let action = decide(
        &issue(42, None),
        None,
        Some(222),
        None,
        10.0,
        &|pid| pid == 222,
        4.0,
        now,
        false,
    );
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_reclaims_when_run_registry_pid_dead_and_no_journal_entry() {
    let now = Utc::now();
    // A fresh label would normally still be within the age-rule grace
    // period, but the run-registry evidence is provable and immediate,
    // no age grace, exactly like the journal's DeadPid branch.
    let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
    let action = decide(&fresh_issue, None, Some(999), None, 10.0, &|_| false, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 999 }));
}

#[test]
fn decide_journal_entry_takes_priority_over_run_registry_pid() {
    // Both evidence sources present: the journal (more authoritative)
    // decides, and the run-registry pid is never even consulted.
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let action = decide(
        &issue(42, None),
        Some(&entry),
        Some(999),
        None,
        10.0,
        &|pid| pid == 111,
        4.0,
        now,
        false,
    );
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_falls_back_to_age_rule_when_run_registry_pid_absent() {
    let now = Utc::now();
    let old = now - Duration::hours(5);
    let action = decide(&issue(42, Some(old)), None, None, None, 10.0, &|_| true, 4.0, now, false);
    match action {
        ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. }) => {}
        other => panic!("expected NoRecordStale reclaim, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// Exit-0/no-progress fast reclaim (Issue #4462)
// ------------------------------------------------------------------

#[test]
fn decide_reclaims_exited_no_progress_ahead_of_age_gate() {
    // No journal entry, no run-registry pid (the in-session sweep's entry
    // was cleaned up at exit), a FRESH label (well within the age grace),
    // but the caller's no-progress evidence is set: a checkpoint stalled at
    // curator-done with no open PR, and its own timestamp is well past the
    // no-progress grace period. This is the #4462 orphan the age gate
    // would otherwise sit on for hours -- reclaim NOW.
    let now = Utc::now();
    let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
    let stale_checkpoint = NoProgressEvidence {
        checkpoint_timestamp: now - Duration::minutes(35),
    };
    let action = decide(
        &fresh_issue,
        None,
        None,
        Some(stale_checkpoint),
        10.0,
        &|_| true,
        4.0,
        now,
        false,
    );
    assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::ExitedNoProgress));
}

/// Issue #4616: the exact race a resumed Builder retry produces —
/// checkpoint at `curator-done`, no PR yet, no journal entry, and the
/// checkpoint→run-registry join resolves to `None` (the ORIGINAL run's
/// registry entry was already pruned) — must NOT be reclaimed while the
/// checkpoint's own timestamp is still within the grace window. This is
/// indistinguishable, by this evidence alone, from a brand-new resumed
/// Builder run that has not yet linked back to the checkpoint or opened a
/// PR.
#[test]
fn decide_keeps_exited_no_progress_within_grace_window() {
    let now = Utc::now();
    // The `loom:building` label itself may be old (from the ORIGINAL
    // claim, long before this resumed attempt) -- deliberately outside
    // the age-rule's own grace, so a Keep here can only be explained by
    // the no-progress grace window, not a fall-through to the age rule.
    let old_label_issue = issue(42, Some(now - Duration::hours(5)));
    let fresh_checkpoint = NoProgressEvidence {
        checkpoint_timestamp: now - Duration::minutes(2),
    };
    let action = decide(
        &old_label_issue,
        None,
        None,
        Some(fresh_checkpoint),
        10.0,
        &|_| true,
        4.0,
        now,
        false,
    );
    assert_eq!(
        action,
        ReconcileAction::Keep,
        "a checkpoint within the no-progress grace window must Keep, not Reclaim or fall \
             through to the (stale) age rule"
    );
}

#[test]
fn decide_no_progress_never_overrides_a_live_journal_pid() {
    // A live journal pid is authoritative: even with no-progress evidence
    // past its grace period, the running sweep must be kept. Ordering
    // guarantee -- spurious no-progress evidence can never reclaim a live
    // sweep.
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let stale_checkpoint = NoProgressEvidence {
        checkpoint_timestamp: now - Duration::minutes(35),
    };
    let action = decide(
        &issue(42, None),
        Some(&entry),
        None,
        Some(stale_checkpoint),
        10.0,
        &|_| true,
        4.0,
        now,
        false,
    );
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_no_progress_never_overrides_a_live_run_registry_pid() {
    let now = Utc::now();
    let stale_checkpoint = NoProgressEvidence {
        checkpoint_timestamp: now - Duration::minutes(35),
    };
    let action = decide(
        &issue(42, None),
        None,
        Some(222),
        Some(stale_checkpoint),
        10.0,
        &|pid| pid == 222,
        4.0,
        now,
        false,
    );
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn decide_no_progress_none_still_falls_through_to_age_gate() {
    // no_progress=None must not disturb the existing age-rule behavior:
    // a fresh label with no evidence is still Kept.
    let now = Utc::now();
    let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
    let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now, false);
    assert_eq!(action, ReconcileAction::Keep);
}

#[test]
fn plan_consults_no_progress_only_when_no_pid_evidence() {
    // #1 has a live journal pid; #2 has a dead run-registry pid; #3 has
    // neither. The no_progress closure records which issues it was asked
    // about and returns stale evidence for all -- it must be consulted
    // for #3 ONLY (mirroring decide()'s priority), and #1/#2 must be
    // decided by their pid evidence, never ExitedNoProgress.
    use std::cell::RefCell;
    let now = Utc::now();
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry("/repo/a", 1, 111)); // alive
    let issues = vec![
        issue(1, None),
        issue(2, Some(now - Duration::minutes(1))),
        issue(3, Some(now - Duration::minutes(1))),
    ];
    let run_registry_pid_for = |n: u32| -> Option<u32> {
        match n {
            2 => Some(999), // dead
            _ => None,
        }
    };
    let asked: RefCell<Vec<u32>> = RefCell::new(Vec::new());
    let no_progress_for = |n: u32| {
        asked.borrow_mut().push(n);
        Some(NoProgressEvidence {
            checkpoint_timestamp: now - Duration::minutes(35),
        })
    };
    let is_alive = |pid: u32| pid == 111;
    let decisions = plan(
        "/repo/a",
        &issues,
        &journal,
        &run_registry_pid_for,
        &no_progress_for,
        10.0,
        &is_alive,
        4.0,
        now,
        false,
    );
    assert_eq!(decisions[0], (1, ReconcileAction::Keep));
    assert_eq!(
        decisions[1],
        (2, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 999 }))
    );
    assert_eq!(decisions[2], (3, ReconcileAction::Reclaim(ReclaimReason::ExitedNoProgress)));
    assert_eq!(
        *asked.borrow(),
        vec![3],
        "no_progress evidence must be consulted ONLY for the issue with no pid evidence"
    );
}

#[test]
fn plan_maps_each_issue_to_its_own_journal_entry() {
    let now = Utc::now();
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry("/repo/a", 1, 111)); // will be dead
    journal.entries.push(journal_entry("/repo/a", 2, 222)); // will be alive
                                                            // #3 has no journal entry and is stale.
    let issues = vec![
        issue(1, None),
        issue(2, None),
        issue(3, Some(now - Duration::hours(10))),
    ];

    let decisions = plan(
        "/repo/a",
        &issues,
        &journal,
        &|_| None,
        &|_| None,
        10.0,
        &|pid| pid == 222,
        4.0,
        now,
        false,
    );

    assert_eq!(decisions.len(), 3);
    assert_eq!(decisions[0], (1, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 })));
    assert_eq!(decisions[1], (2, ReconcileAction::Keep));
    match decisions[2] {
        (3, ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. })) => {}
        ref other => panic!("expected #3 to be a NoRecordStale reclaim, got {other:?}"),
    }
}

#[test]
fn plan_scopes_journal_lookup_by_repo_string() {
    // Same issue number, different repos — must not cross-contaminate.
    let now = Utc::now();
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry("/repo/other", 42, 111));

    let issues = vec![issue(42, Some(now - Duration::hours(10)))];
    let decisions = plan(
        "/repo/a",
        &issues,
        &journal,
        &|_| None,
        &|_| None,
        10.0,
        &|_| true,
        4.0,
        now,
        false,
    );

    // No entry under "/repo/a" -> falls through to the age check, which
    // is stale here, so it reclaims (not "Keep" from the other repo's
    // live pid).
    match decisions[0] {
        (42, ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. })) => {}
        ref other => panic!("expected repo-scoped NoRecordStale reclaim, got {other:?}"),
    }
}

#[test]
fn plan_consults_run_registry_only_when_journal_entry_absent() {
    let now = Utc::now();
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry("/repo/a", 1, 111)); // journal-backed, alive

    let issues = vec![issue(1, None), issue(2, Some(now - Duration::minutes(1)))];

    // #1 has a journal entry with an alive pid (111) -- the run-registry
    // closure below would say "dead" (999) for issue 1 if it were ever
    // consulted, so a wrong result there proves priority is broken. #2
    // has no journal entry; the run-registry closure resolves a dead pid
    // for it, which must reclaim immediately despite the fresh label.
    let run_registry_pid_for = |issue_num: u32| -> Option<u32> {
        match issue_num {
            1 => Some(999), // must be ignored: journal entry takes priority
            2 => Some(555),
            _ => None,
        }
    };
    let is_alive = |pid: u32| pid == 111; // only the journal's pid is alive

    let decisions = plan(
        "/repo/a",
        &issues,
        &journal,
        &run_registry_pid_for,
        &|_| None,
        10.0,
        &is_alive,
        4.0,
        now,
        false,
    );

    assert_eq!(decisions[0], (1, ReconcileAction::Keep));
    assert_eq!(
        decisions[1],
        (2, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 555 }))
    );
}

#[test]
#[serial]
fn reconciliation_enabled_resolves_env_precedence() {
    std::env::remove_var(RECONCILE_ENABLED_ENV);
    assert!(reconciliation_enabled(), "defaults to enabled");

    for off in ["0", "false", "no", "off", "OFF", "False"] {
        std::env::set_var(RECONCILE_ENABLED_ENV, off);
        assert!(!reconciliation_enabled(), "{off} should disable");
    }

    std::env::set_var(RECONCILE_ENABLED_ENV, "1");
    assert!(reconciliation_enabled());

    std::env::remove_var(RECONCILE_ENABLED_ENV);
}

#[test]
#[serial]
fn resolve_stale_hours_defaults_and_overrides() {
    std::env::remove_var(STALE_HOURS_ENV);
    assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);

    std::env::set_var(STALE_HOURS_ENV, "2.5");
    assert!((resolve_stale_hours() - 2.5).abs() < f64::EPSILON);

    // Non-positive / unparseable falls back to the default.
    std::env::set_var(STALE_HOURS_ENV, "0");
    assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);
    std::env::set_var(STALE_HOURS_ENV, "garbage");
    assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);

    std::env::remove_var(STALE_HOURS_ENV);
}

#[test]
#[serial]
fn resolve_no_progress_grace_minutes_defaults_and_overrides() {
    std::env::remove_var(NO_PROGRESS_GRACE_MINUTES_ENV);
    assert!(
        (resolve_no_progress_grace_minutes() - DEFAULT_NO_PROGRESS_GRACE_MINUTES).abs()
            < f64::EPSILON
    );

    std::env::set_var(NO_PROGRESS_GRACE_MINUTES_ENV, "3.5");
    assert!((resolve_no_progress_grace_minutes() - 3.5).abs() < f64::EPSILON);

    // Non-positive / unparseable falls back to the default.
    std::env::set_var(NO_PROGRESS_GRACE_MINUTES_ENV, "0");
    assert!(
        (resolve_no_progress_grace_minutes() - DEFAULT_NO_PROGRESS_GRACE_MINUTES).abs()
            < f64::EPSILON
    );
    std::env::set_var(NO_PROGRESS_GRACE_MINUTES_ENV, "garbage");
    assert!(
        (resolve_no_progress_grace_minutes() - DEFAULT_NO_PROGRESS_GRACE_MINUTES).abs()
            < f64::EPSILON
    );

    std::env::remove_var(NO_PROGRESS_GRACE_MINUTES_ENV);
}

// ------------------------------------------------------------------
// Periodic-interval resolution (Issue #4348)
// ------------------------------------------------------------------

/// Write a `.loom/config.json` with the given `safehouse` block into a
/// fresh tempdir root (Issue #4431 interval tests).
fn root_with_safehouse_config(dir: &std::path::Path, safehouse_json: &str) -> std::path::PathBuf {
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(".loom").join("config.json"),
        format!(r#"{{"safehouse": {safehouse_json}}}"#),
    )
    .unwrap();
    root
}

/// #4431: precedence env > config override > safehouse-mode default >
/// legacy default, floor always enforced.
#[test]
#[serial]
fn resolve_reconcile_interval_for_is_safehouse_aware() {
    std::env::remove_var(RECONCILE_INTERVAL_ENV);
    // The safehouse env toggle would shadow the config file — clear it so
    // the test exercises the config layer (hosts like loom-worker-1 set
    // it in the daemon unit, but tests must not depend on that).
    std::env::remove_var("LOOM_SAFEHOUSE_ENABLED");
    let dir = tempdir().unwrap();

    // Safehouse enabled → the slow healing cadence.
    let root = root_with_safehouse_config(dir.path(), r#"{"enabled": true}"#);
    assert_eq!(
        resolve_reconcile_interval_for(&root),
        std::time::Duration::from_secs(DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS)
    );

    // Safehouse disabled (or absent) → byte-for-byte the pre-#4431 default.
    let root = root_with_safehouse_config(dir.path(), r#"{"enabled": false}"#);
    assert_eq!(
        resolve_reconcile_interval_for(&root),
        std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
    );
    assert_eq!(
        resolve_reconcile_interval_for(dir.path()),
        std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS),
        "no config file at all must resolve to the legacy default"
    );

    // Per-repo config override wins over the safehouse-mode default…
    let root = root_with_safehouse_config(
        dir.path(),
        r#"{"enabled": true, "claimReconcileIntervalSecs": 900}"#,
    );
    assert_eq!(resolve_reconcile_interval_for(&root), std::time::Duration::from_secs(900));
    // …but a zero/invalid override is ignored, and the floor still holds.
    let root = root_with_safehouse_config(
        dir.path(),
        r#"{"enabled": true, "claimReconcileIntervalSecs": 0}"#,
    );
    assert_eq!(
        resolve_reconcile_interval_for(&root),
        std::time::Duration::from_secs(DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS)
    );
    let root = root_with_safehouse_config(
        dir.path(),
        r#"{"enabled": true, "claimReconcileIntervalSecs": 5}"#,
    );
    assert_eq!(
        resolve_reconcile_interval_for(&root),
        std::time::Duration::from_secs(MIN_RECONCILE_INTERVAL_SECS)
    );

    // The operator env var beats everything, on any host.
    std::env::set_var(RECONCILE_INTERVAL_ENV, "300");
    let root = root_with_safehouse_config(dir.path(), r#"{"enabled": true}"#);
    assert_eq!(resolve_reconcile_interval_for(&root), std::time::Duration::from_secs(300));
    std::env::remove_var(RECONCILE_INTERVAL_ENV);
}

#[test]
#[serial]
fn resolve_reconcile_interval_defaults_and_overrides() {
    std::env::remove_var(RECONCILE_INTERVAL_ENV);
    assert_eq!(
        resolve_reconcile_interval(),
        std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
    );

    std::env::set_var(RECONCILE_INTERVAL_ENV, "900");
    assert_eq!(resolve_reconcile_interval(), std::time::Duration::from_secs(900));

    std::env::remove_var(RECONCILE_INTERVAL_ENV);
}

#[test]
#[serial]
fn resolve_reconcile_interval_enforces_floor() {
    std::env::set_var(RECONCILE_INTERVAL_ENV, "5");
    assert_eq!(
        resolve_reconcile_interval(),
        std::time::Duration::from_secs(MIN_RECONCILE_INTERVAL_SECS),
        "an interval below the floor must be clamped up, not honored literally"
    );
    std::env::remove_var(RECONCILE_INTERVAL_ENV);
}

#[test]
#[serial]
fn resolve_reconcile_interval_ignores_non_positive_or_unparseable() {
    std::env::set_var(RECONCILE_INTERVAL_ENV, "0");
    assert_eq!(
        resolve_reconcile_interval(),
        std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
    );
    std::env::set_var(RECONCILE_INTERVAL_ENV, "garbage");
    assert_eq!(
        resolve_reconcile_interval(),
        std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
    );
    std::env::remove_var(RECONCILE_INTERVAL_ENV);
}

#[test]
#[serial]
fn run_reconciliation_pass_noops_when_disabled() {
    // Kill-switch interaction: the periodic pass and the startup pass
    // share this exact gate, so disabling it must short-circuit BEFORE
    // any `gh` invocation -- this test has no fake `gh` on `PATH` at all,
    // so a regression that skipped the gate would fail with a spawn
    // error instead of returning quietly.
    std::env::set_var(RECONCILE_ENABLED_ENV, "0");
    let dir = tempdir().unwrap();
    run_reconciliation_pass(dir.path(), true);
    std::env::remove_var(RECONCILE_ENABLED_ENV);
}

// ------------------------------------------------------------------
// Checkpoint -> run-registry join (Issue #4348)
// ------------------------------------------------------------------

fn seed_checkpoint_task_id(root: &std::path::Path, issue: u32, task_id: &str) {
    let dir = root.join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join(format!("issue-{issue}.json")),
            format!(
                r#"{{"phase":"builder","task_id":"{task_id}","timestamp":"2026-01-01T00:00:00Z","pr_number":null}}"#
            ),
        )
        .unwrap();
}

fn seed_run_registry(root: &std::path::Path, task_id: &str, pid: u32) {
    let dir = root.join(".loom").join("sweep-run");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{task_id}.json")),
        format!(r#"{{"run_id":"{task_id}","pid":{pid},"timestamp":"2026-01-01T00:00:00Z"}}"#),
    )
    .unwrap();
}

/// Seed a checkpoint recording an arbitrary `phase` (and no run-registry
/// join by default -- callers add one separately if needed). Used by the
/// Issue #4462 exit-0/no-progress tests, which need a `curator-done`
/// checkpoint with NO surviving run-registry entry. The checkpoint
/// `timestamp` is a fixed date far in the past, well outside any
/// no-progress grace window, so this helper's checkpoints already read as
/// "stale enough to reclaim" by construction.
fn seed_checkpoint_phase(root: &std::path::Path, issue: u32, phase: &str) {
    seed_checkpoint_phase_with_timestamp(root, issue, phase, "2026-01-01T00:00:00Z");
}

/// Like [`seed_checkpoint_phase`], but with an explicit `timestamp` —
/// needed by the Issue #4616 grace-window tests, which must control
/// whether the checkpoint reads as "just resumed" or "aged past grace".
fn seed_checkpoint_phase_with_timestamp(
    root: &std::path::Path,
    issue: u32,
    phase: &str,
    timestamp: &str,
) {
    let dir = root.join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join(format!("issue-{issue}.json")),
            format!(
                r#"{{"phase":"{phase}","task_id":"sweep-{issue}","timestamp":"{timestamp}","pr_number":null}}"#
            ),
        )
        .unwrap();
}

#[test]
fn resolve_run_registry_pid_returns_none_when_checkpoint_missing() {
    let dir = tempdir().unwrap();
    assert_eq!(resolve_run_registry_pid(dir.path(), 42), None);
}

#[test]
fn resolve_run_registry_pid_returns_none_when_task_id_missing() {
    let dir = tempdir().unwrap();
    let checkpoint_dir = dir.path().join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::write(checkpoint_dir.join("issue-42.json"), r#"{"phase":"builder"}"#).unwrap();
    assert_eq!(resolve_run_registry_pid(dir.path(), 42), None);
}

#[test]
fn resolve_run_registry_pid_returns_none_when_run_registry_entry_missing() {
    let dir = tempdir().unwrap();
    seed_checkpoint_task_id(dir.path(), 42, "sweep-abc123");
    // No `.loom/sweep-run/sweep-abc123.json` written.
    assert_eq!(resolve_run_registry_pid(dir.path(), 42), None);
}

#[test]
fn resolve_run_registry_pid_joins_checkpoint_and_run_registry() {
    let dir = tempdir().unwrap();
    seed_checkpoint_task_id(dir.path(), 42, "sweep-abc123");
    seed_run_registry(dir.path(), "sweep-abc123", 4242);
    assert_eq!(resolve_run_registry_pid(dir.path(), 42), Some(4242));
}

#[test]
fn resolve_run_registry_pid_returns_none_on_malformed_checkpoint_json() {
    let dir = tempdir().unwrap();
    let checkpoint_dir = dir.path().join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::write(checkpoint_dir.join("issue-42.json"), "not json at all").unwrap();
    assert_eq!(
        resolve_run_registry_pid(dir.path(), 42),
        None,
        "malformed checkpoint JSON must degrade fail-safe, never panic or fake a pid"
    );
}

#[test]
fn resolve_run_registry_pid_returns_none_on_malformed_run_registry_json() {
    let dir = tempdir().unwrap();
    seed_checkpoint_task_id(dir.path(), 42, "sweep-abc123");
    let run_dir = dir.path().join(".loom").join("sweep-run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("sweep-abc123.json"), "{ not valid").unwrap();
    assert_eq!(
        resolve_run_registry_pid(dir.path(), 42),
        None,
        "malformed run-registry JSON must degrade fail-safe, never panic or fake a pid"
    );
}

/// Regression test for #3975: `reconcile_workspace` used to prune dead
/// journal entries *before* deciding, which erased the exact evidence the
/// `DeadPid` branch needs. A claim with a provably-dead recorded PID must
/// be reclaimed immediately by the daemon's own startup pass, even when
/// the `loom:building` label is only seconds old (well inside the
/// `NoRecordStale` grace window) -- two incidents (#6170/#6173 in a
/// downstream workspace) were exactly this: SIGTERMed sweeps left a dead
/// PID in the journal, and the pre-decide prune silently downgraded them
/// to "no record", so they sat un-reclaimed for hours.
#[test]
#[serial]
fn reconcile_workspace_reclaims_dead_pid_entry_even_when_label_is_fresh() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Seed a dead-PID entry (pid 0 is always dead per `is_pid_alive`).
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    // Fake `gh`: the REST listing (#4428) reports one loom:building issue
    // labeled *just now* -- fresh enough that the NoRecordStale
    // (age-based) path would say Keep. Only the DeadPid evidence should
    // trigger a reclaim.
    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 99, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "a dead-PID journal entry must be reclaimed immediately regardless of \
             how fresh the loom:building label is (#3975)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"),
        "expected reclaim to flip labels for #99; got: {gh_calls:?}"
    );

    // The reclaimed issue's journal entry is cleaned up as part of
    // recovery -- confirms cleanup still happens, just after (not
    // before) the decision that needed the evidence.
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// Write a fake `gh` script (tests only, #7367) identical in shape to
/// [`write_fake_gh`] (a dead-PID-in-journal candidate that would
/// otherwise reclaim unconditionally), except its `gh api
/// repos/{owner}/{repo}/issues/<N> --jq .state` response — the final
/// freshness gate ([`forge::issue_is_confirmed_closed`]) — reports
/// `"closed"`, simulating an issue that closed (e.g. via a concurrent
/// merge) sometime between this pass's `state=open`-filtered candidate
/// listing and the reclaim write. Distinguished from the `--include`
/// listing call, which still reports `state=open` (mirroring the
/// pre-race candidate list).
fn write_fake_gh_with_closed_race(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    issue_number: u32,
    updated_at: &str,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-closed-race.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  case "$*" in
    *--include*)
      printf 'HTTP/2.0 200 OK\r\n\r\n'
      echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
      exit 0
      ;;
    *"issues/{issue_number} --jq .state"*)
      echo "closed"
      exit 0
      ;;
    */comments*)
      true # no lease comment -- empty stdout
      exit 0
      ;;
  esac
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// Regression test for #7367: issue #7363 was re-labeled `loom:issue`
/// ~46 seconds after its own closing merge, because `reconcile_workspace`
/// wrote the reclaim based on a candidate list built while the issue was
/// still open, with no recheck immediately before the write. This test
/// reproduces the race directly: a dead-PID journal entry makes the
/// DeadPid branch decide to reclaim unconditionally (same fixture as
/// [`reconcile_workspace_reclaims_dead_pid_entry_even_when_label_is_fresh`]),
/// but the fresh, uncached per-issue state check run immediately before
/// the write reports the issue as `closed` — simulating a concurrent
/// merge landing in the gap between listing and reclaim. The reclaim
/// must be skipped: no `gh issue edit` adding `loom:issue` back, and the
/// journal entry is left in place (mirroring the "nothing was reclaimed"
/// convention used by every other veto in this pass) so a later pass can
/// re-evaluate from scratch — a moot point once the issue is closed, but
/// consistent with every other skip branch here.
#[test]
#[serial]
fn reconcile_workspace_skips_reclaim_when_issue_closed_concurrently() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Same dead-PID fixture as the #3975 regression test -- an
    // unconditional, immediate reclaim decision absent the #7367 gate.
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh_with_closed_race(dir.path(), &gh_log, 99, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1, "the issue is still inspected -- only the reclaim ACTION is frozen");
    assert_eq!(
        reclaimed, 0,
        "an issue confirmed closed immediately before the write must not be reclaimed, even \
             though every other evidence source says to (#7367)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--add-label loom:issue"),
        "no gh issue edit must be issued once the issue is confirmed closed; got: {gh_calls:?}"
    );
    assert!(
        gh_calls
            .lines()
            .any(|l| l.contains("issues/99 --jq .state")),
        "expected the final per-issue freshness check to actually run; got: {gh_calls:?}"
    );

    // Nothing was reclaimed, so the journal entry survives untouched --
    // matches the existing "no cleanup on skip" convention used by every
    // other veto branch in this pass.
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_some());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// #6263 regression: a single `gh issue edit --remove-label
/// loom:building --add-label loom:issue` invocation can exit 0 while
/// only *partially* applying the swap — root cause: `gh` implements
/// `--add-label`/`--remove-label` as two independent, concurrently
/// fired GraphQL mutations (see [`forge::reclaim`]'s doc comment), not
/// one atomic operation. This is the plausible mechanism behind #6254
/// carrying both `loom:issue` and `loom:building` simultaneously for
/// ~37 minutes on 2026-08-15. The fix must detect the partial
/// application via a post-mutation re-fetch and repair it with exactly
/// one bounded retry.
#[test]
#[serial]
fn reconcile_workspace_repairs_reclaim_left_partially_applied_by_a_zero_exit_gh() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Same dead-PID fixture as the #3975 regression test above -- an
    // unconditional, immediate reclaim.
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh_with_partial_reclaim(dir.path(), &gh_log, 99, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "the reclaim must succeed once the post-mutation re-fetch confirms the \
             retried edit actually landed both halves (#6263)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    let edit_calls = gh_calls
        .lines()
        .filter(|l| l.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"))
        .count();
    assert_eq!(
        edit_calls, 2,
        "expected exactly one bounded retry (2 total edit calls) after the first \
             invocation reported success but only partially applied the swap; got: {gh_calls:?}"
    );
    let view_calls = gh_calls
        .lines()
        .filter(|l| l.contains("issue view 99 --json labels"))
        .count();
    assert!(
        view_calls >= 2,
        "expected a post-mutation re-fetch after each edit attempt; got: {gh_calls:?}"
    );

    // The reclaim was confirmed to have fully landed, so the journal
    // entry is cleaned up exactly like a straightforward reclaim.
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// #6263 AC3: if the post-mutation re-fetch keeps showing the label
/// swap incomplete even after the one bounded retry, the reclaim must
/// fail (not count toward `reclaimed`) rather than loop indefinitely or
/// silently accept the wrong final state — the exact number of `gh
/// issue edit` invocations is asserted to prove the retry is bounded,
/// not unbounded.
#[test]
#[serial]
fn reconcile_workspace_does_not_loop_forever_when_reclaim_never_repairs() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh_with_persistent_partial_reclaim(dir.path(), &gh_log, 99, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "a reclaim that never repairs after the bounded retry must not be counted as \
             reclaimed (#6263)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    let edit_calls = gh_calls
        .lines()
        .filter(|l| l.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"))
        .count();
    assert_eq!(
        edit_calls, 2,
        "the retry must be bounded to exactly one attempt, never an unbounded loop \
             (#6263 AC3); got {edit_calls} edit calls in: {gh_calls:?}"
    );

    // Nothing was confirmed reclaimed, so the journal entry survives
    // untouched -- matches the existing "no cleanup on failure"
    // convention (see the failed-`gh` branch in `reconcile_workspace`).
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_some());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// Epic #6165 Phase 4 (#6317) regression: the identical dead-PID-in-
/// journal fixture the former Issue #6157 "frozen while degraded" test
/// used to gate on is now reclaimed unconditionally — `reconcile_workspace`
/// no longer accepts (or consults) any peer-coordination-health evidence
/// at all. This replaces
/// `reconcile_workspace_freezes_reclaim_when_peer_coordination_degraded`
/// / `reconcile_workspace_with_coordination_reclaims_normally_when_not_degraded`
/// (both removed — their entire premise, an injectable
/// `coordination_degraded_reason` seam, no longer exists): there is
/// nothing left to freeze reclaim on peer-channel health, by
/// construction, so this test simply confirms the ordinary dead-PID
/// reclaim (mirroring the #3975 fixture) still fires with no such
/// signal available at all — the peer-claim/safehouse channel plays no
/// role whatsoever in this decision now, healthy or not.
#[test]
#[serial]
fn reconcile_workspace_reclaims_dead_pid_with_no_peer_coordination_signal_consulted() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Same dead-PID fixture as #3975's regression test: an
    // unconditional, immediate reclaim, with the peer-coordination
    // global view never registered at all (the default state for every
    // test in this binary — see `peer_claims::GLOBAL_VIEW`'s removal in
    // #6317).
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 99, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "reclaim must fire on dead-PID evidence with no peer-coordination signal of any \
             kind involved in the decision (#6317)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"),
        "expected reclaim to flip labels for #99; got: {gh_calls:?}"
    );

    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

// ------------------------------------------------------------------
// Integration: run-registry evidence via `forge::reconcile_workspace`
// (Issue #4348)
// ------------------------------------------------------------------

/// Write a fake `gh` script (tests only) that logs every invocation to
/// `gh_log` and, for the ETag-cached REST listing (`gh api …/issues?…`,
/// #4428), reports exactly one `loom:building` issue (`issue_number`,
/// `updated_at`) as an `--include`-style HTTP response with **no ETag**
/// (so the process-global cache never carries state across tests). Every
/// other subcommand (e.g. `issue edit`) just logs and exits 0 -- a test
/// asserts on `gh_log`'s contents to see whether a reclaim was actually
/// attempted.
fn write_fake_gh(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    issue_number: u32,
    updated_at: &str,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  # `pr list --head feature/issue-N ...`: no open linked PR by default
  # (the Issue #4462 no-progress path treats empty stdout as "no PR").
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// Write a fake `gh` script (tests only, #6263) that reproduces the
/// non-atomicity of `gh issue edit --remove-label loom:building
/// --add-label loom:issue`: every `issue edit` invocation exits 0
/// (matching real `gh`'s exit status once its GraphQL mutations are
/// accepted), but the *first* subsequent `gh issue view --json labels`
/// re-fetch reports the swap only partially applied (both
/// `loom:building` and `loom:issue` present — mirroring the ~37-minute
/// co-presence observed on #6254). From the second `issue edit`
/// invocation onward, the labels are reported fully corrected
/// (`loom:issue` only) — simulating a retry that succeeds.
fn write_fake_gh_with_partial_reclaim(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    issue_number: u32,
    updated_at: &str,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-partial-reclaim.sh");
    let counter = dir.join("edit-count");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  if [ "$count" -ge 2 ]; then
    echo '{{"labels":[{{"name":"loom:issue"}}]}}'
  else
    echo '{{"labels":[{{"name":"loom:building"}},{{"name":"loom:issue"}}]}}'
  fi
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
        counter = counter.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// Same shape as [`write_fake_gh_with_partial_reclaim`], but the
/// partial application never repairs — every `gh issue view --json
/// labels` re-fetch reports both `loom:building` and `loom:issue`
/// present, regardless of how many `issue edit` retries have run.
/// Exercises the #6263 AC3 "fail safe, never loop forever" path.
fn write_fake_gh_with_persistent_partial_reclaim(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    issue_number: u32,
    updated_at: &str,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-stuck-reclaim.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[{{"name":"loom:building"}},{{"name":"loom:issue"}}]}}'
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

// ------------------------------------------------------------------
// Lease-record freshness (Epic #6165 Phase 2, Issue #6286)
// ------------------------------------------------------------------

/// Pure unit coverage for [`lease_is_fresh`]: within the TTL is fresh,
/// past it is not, and the boundary itself (age == ttl) is NOT fresh
/// (strict `<`, matching every other age-gate in this module).
#[test]
fn lease_is_fresh_within_ttl_stale_past_it_boundary_exclusive() {
    let now = Utc::now();
    assert!(
        lease_is_fresh(now - Duration::minutes(5), now, 15.0),
        "a lease renewed 5 minutes ago is fresh under a 15-minute TTL"
    );
    assert!(
        !lease_is_fresh(now - Duration::minutes(16), now, 15.0),
        "a lease last renewed 16 minutes ago has genuinely expired under a 15-minute TTL"
    );
    assert!(
        !lease_is_fresh(now - Duration::minutes(15), now, 15.0),
        "age exactly equal to the TTL must NOT be treated as fresh (strict <, not <=)"
    );
}

/// Issue #6320: a reclaim decision must record WHY it was reclaimable —
/// specifically whether the lease had expired or was never published at
/// all. [`classify_lease_evidence`] is the pure classifier that feeds
/// that log line; absence must classify as `Absent`, never collapse into
/// "stale".
#[test]
fn classify_lease_evidence_separates_absent_from_stale_and_fresh() {
    let now = Utc::now();
    assert_eq!(
        classify_lease_evidence(None, now, 15.0),
        LeaseEvidence::Absent,
        "no lease comment is ABSENT evidence, never 'stale' — per lease-record.md's \
             reader contract, absence is not evidence of abandonment"
    );
    match classify_lease_evidence(Some(now - Duration::minutes(5)), now, 15.0) {
        LeaseEvidence::Fresh { age_minutes } => {
            assert!(
                (age_minutes - 5.0).abs() < 0.5,
                "fresh lease reports its own age (got {age_minutes})"
            );
        }
        other => panic!("a 5-minute-old lease under a 15m TTL must be Fresh, got {other:?}"),
    }
    match classify_lease_evidence(Some(now - Duration::minutes(40)), now, 15.0) {
        LeaseEvidence::Stale { age_minutes } => {
            assert!(
                (age_minutes - 40.0).abs() < 0.5,
                "stale lease reports its own age (got {age_minutes})"
            );
        }
        other => panic!("a 40-minute-old lease under a 15m TTL must be Stale, got {other:?}"),
    }
}

/// The classification only earns its keep if it reaches the operator's
/// log in a legible, greppable form — the reclaim log line interpolates
/// `Display`, so assert on that rendering directly (#6320).
#[test]
fn lease_evidence_display_is_greppable_and_distinguishes_the_three_cases() {
    let absent = LeaseEvidence::Absent.to_string();
    let fresh = LeaseEvidence::Fresh { age_minutes: 3.25 }.to_string();
    let stale = LeaseEvidence::Stale { age_minutes: 42.0 }.to_string();

    assert!(absent.starts_with("lease_evidence=absent"), "got: {absent}");
    assert!(fresh.starts_with("lease_evidence=fresh"), "got: {fresh}");
    assert!(stale.starts_with("lease_evidence=stale"), "got: {stale}");
    assert!(
        fresh.contains("3.2") || fresh.contains("3.3"),
        "fresh rendering carries the age: {fresh}"
    );
    assert!(stale.contains("42.0"), "stale rendering carries the age: {stale}");
    assert!(
        absent.contains("not evidence of abandonment"),
        "absent rendering states the reader contract so a log reader is not misled: {absent}"
    );
}

/// Write a fake `gh` script (tests only) that, in addition to
/// [`write_fake_gh`]'s ETag-cached REST listing response, answers the
/// lease-comments fetch (`gh api .../issues/<N>/comments --paginate
/// --jq ...`, [`forge::fetch_freshest_lease_updated_at`]) with a single
/// pre-filtered timestamp value — emulating what `gh`'s own `--jq`
/// filtering would have produced from a real API response containing one
/// lease-record comment. `lease_updated_at: None` emulates "no lease
/// comment found at all" (`// empty` in the real jq filter -> empty
/// stdout).
fn write_fake_gh_with_lease(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    issue_number: u32,
    label_updated_at: &str,
    lease_updated_at: Option<&str>,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-lease.sh");
    let lease_stdout = match lease_updated_at {
        Some(ts) => format!("echo '\"{ts}\"'"),
        None => "true # no lease comment -- empty stdout".to_string(),
    };
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  case "$*" in
    *--include*)
      printf 'HTTP/2.0 200 OK\r\n\r\n'
      echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{label_updated_at}"}}]'
      exit 0
      ;;
    */comments*)
      {lease_stdout}
      exit 0
      ;;
  esac
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// Issue #6286 acceptance criterion — the core regression test: a live
/// peer sweep whose lease is being renewed, but whose local liveness
/// evidence (dead-PID journal entry -- the #3975 fixture that normally
/// fires an IMMEDIATE, unconditional reclaim) looks dead, with the
/// peer-claim/safehouse channel simply absent (no `PeerClaimView` ever
/// registered — the reading a host with no safehouse configured
/// produces, and per Epic #6165 Phase 4/#6317 now the ONLY reading that
/// exists, since the peer-claim channel is no longer consulted by this
/// decision at all). Reclamation must NOT fire while the lease is
/// fresh, regardless of what the host-scoped evidence says.
#[test]
#[serial]
fn reconcile_workspace_keeps_claim_when_lease_is_fresh_even_with_channel_absent() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Same dead-PID fixture as the #3975/#6157 regression tests: local
    // evidence alone says "reclaim immediately, no grace period".
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let label_updated_at = Utc::now().to_rfc3339();
    // The lease was renewed 2 minutes ago -- comfortably within the
    // 15-minute default TTL.
    let lease_updated_at = (Utc::now() - Duration::minutes(2)).to_rfc3339();
    let fake_gh = write_fake_gh_with_lease(
        dir.path(),
        &gh_log,
        99,
        &label_updated_at,
        Some(&lease_updated_at),
    );

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1, "the issue is still inspected — only the reclaim ACTION is frozen");
    assert_eq!(
        reclaimed, 0,
        "a fresh lease record must block reclaim even though the dead-PID evidence alone \
             would normally reclaim immediately, and even with no peer-claim/safehouse signal \
             at all (#6286)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--add-label loom:issue"),
        "no gh issue edit must be issued while the lease is fresh; got: {gh_calls:?}"
    );
    assert!(
        gh_calls.contains("/comments"),
        "the lease-comments endpoint must actually have been consulted; got: {gh_calls:?}"
    );

    // Nothing was reclaimed, so the journal entry must survive untouched.
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_some());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// The fail-safe must not become "never reclaim": a claim whose lease has
/// genuinely expired (last renewed well past the TTL) must still be
/// reclaimed once the pre-existing host-scoped evidence says so.
#[test]
#[serial]
fn reconcile_workspace_reclaims_when_lease_has_expired() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let label_updated_at = Utc::now().to_rfc3339();
    // Last renewed 30 minutes ago -- well past the 15-minute default TTL.
    let lease_updated_at = (Utc::now() - Duration::minutes(30)).to_rfc3339();
    let fake_gh = write_fake_gh_with_lease(
        dir.path(),
        &gh_log,
        99,
        &label_updated_at,
        Some(&lease_updated_at),
    );

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "a genuinely expired lease must not block reclamation -- the fail-safe must not \
             become 'never reclaim' (#6286)"
    );

    let after = sweep_journal::load(&journal_path);
    assert!(
        sweep_journal::find(&after, &repo_str, 99).is_none(),
        "a genuine reclaim still cleans up its journal entry"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// A `loom:building` claim with NO lease comment at all (a claim
/// predating this feature) must reclaim exactly as it did before this
/// phase existed -- absence of lease evidence is not itself a reason to
/// refuse.
#[test]
#[serial]
fn reconcile_workspace_reclaims_normally_when_no_lease_comment_exists() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let label_updated_at = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh_with_lease(dir.path(), &gh_log, 99, &label_updated_at, None);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "no lease comment at all must not itself block reclamation (#6286)"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// Fake `gh` that answers the `--include` listing normally but makes the
/// lease-comments probe itself FAIL (non-zero exit) — modeling a
/// transient `gh api` error (rate limit, timeout, a forge hiccup) DURING
/// reconciliation, as opposed to [`write_fake_gh_with_lease`]'s `None`
/// case, which models a successful read that legitimately found nothing.
fn write_fake_gh_with_failing_lease_probe(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    issue_number: u32,
    label_updated_at: &str,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-lease-probe-failure.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  case "$*" in
    *--include*)
      printf 'HTTP/2.0 200 OK\r\n\r\n'
      echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{label_updated_at}"}}]'
      exit 0
      ;;
    */comments*)
      echo "simulated transient gh api failure (rate limit / timeout)" >&2
      exit 1
      ;;
  esac
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// Issue #7591 regression: the incident this issue root-caused showed a
/// 36-hour, 4+-host claim/yield thrash on a single issue, with the
/// dispatch-time tie-break itself already well-hardened (#6951/#6994).
/// The likelier driver was this reconciliation pass repeatedly
/// reopening a LIVE claim to fresh contention. One concrete way that
/// happens: a transient lease-probe READ FAILURE (rate limit, timeout —
/// exactly what a burst of forge calls under multi-host contention
/// produces) was, pre-fix, indistinguishable from "no lease exists at
/// all" — both collapsed to `None` / [`LeaseEvidence::Absent`], which
/// does not block a reclaim. That let a single unlucky `gh` failure
/// evict a claim whose lease was, in fact, still being renewed.
///
/// This test pairs the SAME dead-PID fixture used by
/// `reconcile_workspace_keeps_claim_when_lease_is_fresh_even_with_channel_absent`
/// (host-scoped evidence alone says "reclaim immediately, no grace
/// period") with a lease probe that FAILS outright rather than
/// succeeding-and-finding-nothing. The reclaim must be refused exactly
/// like it would be for a genuinely fresh lease — an unverifiable read
/// must never be treated as a green light to evict.
#[test]
#[serial]
fn reconcile_workspace_keeps_claim_when_lease_probe_read_fails() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Same dead-PID fixture as the #3975/#6157 regression tests: local
    // evidence alone says "reclaim immediately, no grace period".
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry(&repo_str, 99, 0));
    sweep_journal::save(&journal_path, &journal).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let label_updated_at = Utc::now().to_rfc3339();
    let fake_gh =
        write_fake_gh_with_failing_lease_probe(dir.path(), &gh_log, 99, &label_updated_at);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1, "the issue is still inspected — only the reclaim ACTION is frozen");
    assert_eq!(
        reclaimed, 0,
        "a FAILED lease probe must refuse the reclaim exactly like a fresh lease would — an \
             unverifiable read is not evidence the lease is absent, and treating it as such is \
             the #7591 root cause: a transient forge error silently evicting a still-live claim"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--add-label loom:issue"),
        "no gh issue edit must be issued while the lease probe is unverifiable; got: \
             {gh_calls:?}"
    );
    assert!(
        gh_calls.contains("/comments"),
        "the lease-comments endpoint must actually have been consulted (and observed to \
             fail); got: {gh_calls:?}"
    );

    // Nothing was reclaimed, so the journal entry must survive untouched.
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_some());

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// Issue #3651's fail-safe, re-verified against the lease-only
/// reclamation path (Epic #6165 Phase 4, #6317): "absent liveness
/// evidence means every claim is treated as ALIVE, never as orphaned."
///
/// This is the total-absence-of-evidence case, on EVERY axis this
/// module and Epic #6165 collectively consult: no journal entry, no
/// run-registry/checkpoint join, no lease comment, AND (implicitly,
/// since the peer-coordination global view is never registered in this
/// test binary — see `peer_claims::GLOBAL_VIEW`) no peer-claim
/// advertisement either. With the `loom:building` label itself freshly
/// applied (well under [`DEFAULT_STALE_BUILDING_HOURS`]), the claim
/// must be `Keep`, not reclaimed — a total absence of information is
/// never, by itself, proof of death; only *aged* absence is (the
/// `NoRecordStale` branch [`decide`] falls to below, gated on
/// `stale_hours`, is deliberately NOT exercised by this test's fresh
/// label).
#[test]
#[serial]
fn reconcile_workspace_keeps_claim_with_zero_evidence_on_every_axis_fresh_label() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    // No journal entry anywhere for this repo -- point the journal seam
    // at an empty file so the daemon's real `~/.loom/sweeps.json` (if
    // any exists on the test host) is never touched, and so there is
    // genuinely zero journal evidence for issue #99.
    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // No checkpoint file at all -- `read_checkpoint_phase` returns
    // `None`, so both the run-registry join and the no-progress
    // evidence short-circuit to `None` without even attempting a `gh`
    // call for either.

    let gh_log = dir.path().join("gh-invocations.log");
    // Freshly applied label -- comfortably under the default 4-hour
    // staleness threshold.
    let label_updated_at = Utc::now().to_rfc3339();
    // No lease comment either (`None`).
    let fake_gh = write_fake_gh_with_lease(dir.path(), &gh_log, 99, &label_updated_at, None);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "zero evidence on every axis (journal, run-registry, lease, peer-claim) must fail \
             safe to Keep for a freshly-labeled claim -- absence is never itself proof of \
             death (#3651, re-verified post-#6317)"
    );

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--add-label loom:issue"),
        "no gh issue edit must be issued when every evidence source is absent; got: \
             {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// Fabricated-workspace integration test (Issue #4348 acceptance
/// criterion): a `loom:building` issue with NO journal entry at all (the
/// manually/externally spawned sweep's signature -- only
/// `SweepRegistry::dispatch` writes the journal), but a checkpoint +
/// run-registry entry recording a now-dead pid, must be reclaimed within
/// one pass even though the label is fresh (no age-rule grace applies to
/// this provable-death evidence source, exactly like the journal's
/// `DeadPid` branch).
#[test]
#[serial]
fn reconcile_workspace_reclaims_via_dead_run_registry_pid_when_no_journal_entry() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    // No journal entry anywhere for this repo -- point the journal seam
    // at an empty file so the daemon's real `~/.loom/sweeps.json` (if
    // any exists on the test host) is never touched.
    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    seed_checkpoint_task_id(&repo_root, 77, "sweep-dead-1");
    seed_run_registry(&repo_root, "sweep-dead-1", 0); // pid 0 is always dead

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 77, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "a dead run-registry pid must be reclaimed even with a fresh label and no \
             journal entry (#4348)"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 77 --remove-label loom:building --add-label loom:issue"),
        "expected reclaim to flip labels for #77; got: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// A `loom:building` issue whose manual sweep is still ALIVE (per the
/// run-registry join) must be kept -- the periodic/startup pass never
/// reclaims a live claim, even with no journal entry at all.
#[test]
#[serial]
fn reconcile_workspace_keeps_when_run_registry_pid_alive_and_no_journal_entry() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    seed_checkpoint_task_id(&repo_root, 78, "sweep-alive-1");
    // This test process's own pid is, by definition, alive.
    seed_run_registry(&repo_root, "sweep-alive-1", std::process::id());

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 78, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(reclaimed, 0, "a live run-registry pid must never be reclaimed");

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// A malformed checkpoint must degrade to the existing age rule, never a
/// spurious reclaim -- with a fresh label, that means `Keep`.
#[test]
#[serial]
fn reconcile_workspace_falls_back_to_age_rule_on_malformed_checkpoint() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    let checkpoint_dir = repo_root.join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::write(checkpoint_dir.join("issue-79.json"), "not json").unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 79, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "a malformed checkpoint must never be treated as proof of death (fail-safe)"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

// ------------------------------------------------------------------
// Integration: startup-only immediate reclaim on total evidence absence
// (Issue #6615)
// ------------------------------------------------------------------

/// End-to-end repro of the #6615 gap: a `loom:building` claim with a
/// FRESH label (well within `stale_hours`), no journal entry, and no
/// checkpoint at all (so no run-registry join and no no-progress
/// evidence either) -- exactly what a daemon crash between
/// `begin_issue_dispatch`'s label flip and `finish_issue_dispatch`'s
/// journal write leaves behind. `reconcile_workspace(is_startup = true)`
/// must reclaim it in the very first post-restart pass.
#[test]
#[serial]
fn reconcile_workspace_reclaims_zero_evidence_immediately_when_is_startup_true() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
    // No journal entry, no checkpoint written at all for issue 90.

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339(); // fresh label -- would Keep under the age gate
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 90, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, true);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "zero evidence at all must reclaim immediately on the startup pass, even with a \
             fresh label (#6615)"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 90 --remove-label loom:building --add-label loom:issue"),
        "expected reclaim to flip labels for #90; got: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// The steady-state counterpart (curator's Test Plan edge case 1): the
/// IDENTICAL zero-evidence, fresh-label shape must NOT be reclaimed when
/// `is_startup = false` (the periodic pass) -- this is exactly what
/// protects a manually/externally spawned `/loom:sweep` that has not yet
/// written a journal entry.
#[test]
#[serial]
fn reconcile_workspace_does_not_reclaim_zero_evidence_when_not_startup() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 91, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "the periodic pass must keep protecting a manually-spawned /loom:sweep with no \
             journal entry yet (#6615 must not weaken the existing steady-state age gate)"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

// ------------------------------------------------------------------
// Integration: exit-0/no-progress fast reclaim (Issue #4462)
// ------------------------------------------------------------------

/// The #4462 incident, end to end: an in-session sweep reached
/// `curator-done`, then died to a transport-failure backoff and exited 0
/// (its run-registry entry cleaned up at exit — so there is a checkpoint
/// but NO run-registry join). The label is FRESH (well within the age
/// grace), and no open PR exists. `reconcile_workspace` must reclaim it
/// within one pass via the fast no-progress path, not wait out the
/// (hours-long) age gate.
#[test]
#[serial]
fn reconcile_workspace_reclaims_exited_no_progress_curator_done_no_pr() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Checkpoint stalled at curator-done, and DELIBERATELY no run-registry
    // entry (the in-session sweep's entry was cleaned up at exit).
    seed_checkpoint_phase(&repo_root, 80, "curator-done");

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339(); // fresh label
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 80, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 1,
        "a curator-done checkpoint with no run-registry pid and no open PR must be reclaimed \
             fast even with a fresh label (#4462)"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 80 --remove-label loom:building --add-label loom:issue"),
        "expected reclaim to flip labels for #80; got: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// The Issue #4616 regression, end to end: a resumed Builder retry looks
/// byte-for-byte identical to the #4462 orphan (checkpoint at
/// `curator-done`, no run-registry join, no open PR) for the first few
/// minutes after it resumes — the checkpoint's `task_id` is only rewritten
/// on Builder *completion*, not on resume, so the join legitimately
/// resolves to nothing. `reconcile_workspace` must NOT reclaim while the
/// checkpoint's own timestamp is still within the no-progress grace
/// window, even though the `loom:building` label itself may be old (from
/// the ORIGINAL, now-superseded claim).
#[test]
#[serial]
fn reconcile_workspace_keeps_exited_no_progress_within_grace_window() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    // Checkpoint stalled at curator-done, no run-registry join, but its
    // own timestamp is only 2 minutes old -- well within the default
    // 10-minute grace period (a fresh resume, not a proven orphan).
    let fresh_timestamp = Utc::now() - chrono::Duration::minutes(2);
    seed_checkpoint_phase_with_timestamp(
        &repo_root,
        83,
        "curator-done",
        &fresh_timestamp.to_rfc3339(),
    );

    // The `loom:building` label itself is OLD (the original claim, long
    // before this resumed attempt) -- deliberately outside the age rule's
    // own grace, so a Keep here can only be explained by the no-progress
    // grace window, not a fall-through to a still-fresh label.
    let gh_log = dir.path().join("gh-invocations.log");
    let old_label = (Utc::now() - chrono::Duration::hours(5)).to_rfc3339();
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 83, &old_label);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "a curator-done checkpoint whose OWN timestamp is within the no-progress grace \
             window must be kept -- it is indistinguishable from a legitimately-resumed \
             Builder retry (#4616)"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// A `builder-done` checkpoint (past the pre-Builder phase — a PR is
/// expected to exist) must NOT trip the fast no-progress reclaim; with a
/// fresh label it falls through to the age gate and is kept.
#[test]
#[serial]
fn reconcile_workspace_no_fast_reclaim_when_checkpoint_past_curator_done() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    seed_checkpoint_phase(&repo_root, 81, "builder-done");

    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339(); // fresh label
    let fake_gh = write_fake_gh(dir.path(), &gh_log, 81, &now);

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "only the pre-Builder curator-done phase may fast-reclaim; builder-done and later \
             defer to the resume/age machinery (#4462)"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

/// A `curator-done` checkpoint but an OPEN linked PR exists — the sweep did
/// produce something, so the fast no-progress reclaim must NOT fire.
#[test]
#[serial]
fn reconcile_workspace_no_fast_reclaim_when_open_pr_exists() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

    seed_checkpoint_phase(&repo_root, 82, "curator-done");

    // Fake gh that reports an OPEN PR for `pr list` (so no_progress=false).
    let gh_log = dir.path().join("gh-invocations.log");
    let now = Utc::now().to_rfc3339();
    let fake_gh = dir.path().join("fake-gh-with-pr.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":82,"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{now}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  echo 4242
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }

    let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

    assert_eq!(checked, 1);
    assert_eq!(
        reclaimed, 0,
        "an open linked PR means the sweep produced progress -- no fast reclaim (#4462)"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
}

// ------------------------------------------------------------------
// PR-side claim labels: decide_pr / plan_pr (Issue #4367)
// ------------------------------------------------------------------

fn claimed_pr(
    number: u32,
    updated_at: Option<DateTime<Utc>>,
    head_ref_name: Option<&str>,
) -> ClaimedPr {
    // claim_labeled_at/most_recent_claim_activity_at intentionally
    // left unset here so every existing caller of this helper keeps
    // exercising the pre-#4618 updated_at-only fallback path unchanged;
    // the #4618/#4638 regression tests below construct `ClaimedPr`
    // directly to set them.
    ClaimedPr {
        number,
        updated_at,
        claim_labeled_at: None,
        most_recent_claim_activity_at: None,
        head_ref_name: head_ref_name.map(ToString::to_string),
    }
}

#[test]
fn decide_pr_keeps_when_fresh_and_no_join() {
    // fresh-kept: no journal entry, no run-registry pid, and the PR was
    // updated recently -- well within the staleness window.
    let now = Utc::now();
    let fresh = now - Duration::minutes(1);
    let pr = claimed_pr(100, Some(fresh), None);
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    assert_eq!(action, PrReconcileAction::Keep);
}

#[test]
fn decide_pr_reclaims_when_stale_and_no_join() {
    // stale-reclaimed: no journal entry, no run-registry pid, and the PR
    // has aged well past the staleness threshold.
    let now = Utc::now();
    let old = now - Duration::minutes(60);
    let pr = claimed_pr(101, Some(old), None);
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    match action {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
            assert!(age_minutes >= 30.0);
        }
        other => panic!("expected Aged reclaim, got {other:?}"),
    }
}

#[test]
fn decide_pr_keeps_when_journal_pid_alive_even_if_stale() {
    // live-pid-kept: a live joined pid short-circuits to Keep
    // unconditionally, regardless of how stale the label is.
    let now = Utc::now();
    let old = now - Duration::minutes(120);
    let entry = journal_entry("/repo/a", 42, 111);
    let pr = claimed_pr(102, Some(old), Some("feature/issue-42"));
    let action = decide_pr(&pr, Some(&entry), None, &|_| true, 30.0, now);
    assert_eq!(action, PrReconcileAction::Keep);
}

#[test]
fn decide_pr_keeps_when_journal_pid_dead_but_fresh() {
    // dead-pid-but-fresh-kept: the age gate applies unconditionally, even
    // to a dead joined pid -- a fresh label is kept regardless.
    let now = Utc::now();
    let fresh = now - Duration::minutes(1);
    let entry = journal_entry("/repo/a", 42, 111);
    let pr = claimed_pr(103, Some(fresh), Some("feature/issue-42"));
    let action = decide_pr(&pr, Some(&entry), None, &|_| false, 30.0, now);
    assert_eq!(action, PrReconcileAction::Keep);
}

#[test]
fn decide_pr_reclaims_when_journal_pid_dead_and_stale() {
    // A dead joined pid AND an aged label together -- reclaims, carrying
    // the DeadPid reason through (not a generic Aged).
    let now = Utc::now();
    let old = now - Duration::minutes(45);
    let entry = journal_entry("/repo/a", 42, 111);
    let pr = claimed_pr(104, Some(old), Some("feature/issue-42"));
    let action = decide_pr(&pr, Some(&entry), None, &|_| false, 30.0, now);
    assert_eq!(action, PrReconcileAction::Reclaim(PrReclaimReason::DeadPid { pid: 111 }));
}

#[test]
fn decide_pr_keeps_when_no_updated_at() {
    // no-updatedAt-kept: missing/unparseable `updatedAt` fails safe to
    // Keep, even with no join evidence at all.
    let now = Utc::now();
    let pr = claimed_pr(105, None, None);
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    assert_eq!(action, PrReconcileAction::Keep, "fail-safe: no updatedAt => Keep");
}

#[test]
fn decide_pr_falls_through_to_age_rule_on_non_joinable_branch() {
    // non-joinable-branch: a head ref that doesn't match
    // `feature/issue-<N>` has no join key at all -- decide_pr still
    // reaches the age rule and reclaims once stale.
    let now = Utc::now();
    let old = now - Duration::minutes(90);
    let pr = claimed_pr(106, Some(old), Some("some-other-branch-name"));
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    match action {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. }) => {}
        other => panic!("expected Aged reclaim, got {other:?}"),
    }
}

#[test]
fn decide_pr_reclaims_when_run_registry_pid_dead_and_stale() {
    let now = Utc::now();
    let old = now - Duration::minutes(90);
    let pr = claimed_pr(107, Some(old), Some("feature/issue-42"));
    let action = decide_pr(&pr, None, Some(999), &|_| false, 30.0, now);
    assert_eq!(
        action,
        PrReconcileAction::Reclaim(PrReclaimReason::DeadRunRegistry { pid: 999 })
    );
}

#[test]
fn decide_pr_journal_entry_takes_priority_over_run_registry_pid() {
    let now = Utc::now();
    let entry = journal_entry("/repo/a", 42, 111);
    let pr = claimed_pr(108, None, Some("feature/issue-42"));
    let action = decide_pr(&pr, Some(&entry), Some(999), &|pid| pid == 111, 30.0, now);
    assert_eq!(action, PrReconcileAction::Keep);
}

// ------------------------------------------------------------------
// decide_pr: claim_labeled_at freshness signal (Issue #4618 — PR #4614
// stand-down-comment livelock regression coverage)
// ------------------------------------------------------------------

#[test]
fn decide_pr_reclaims_via_claim_labeled_at_despite_standdown_inflated_updated_at() {
    // Reproduces the exact PR #4614 shape: the claim label itself was
    // applied 35 minutes ago (well past the 30-minute reviewing
    // threshold) and never re-applied since, but 2+ "standing down, not
    // stomping" comments posted by later Judge passes bumped the PR's
    // aggregate `updatedAt` to a few seconds ago -- each stand-down
    // comment self-refreshing the very signal the pre-#4618 code used to
    // decide freshness. `claim_labeled_at` is immune to that: it only
    // moves when the label is genuinely re-applied, so the reclaim now
    // fires correctly despite the inflated `updated_at`.
    let now = Utc::now();
    let claimed_at = now - Duration::minutes(35);
    let standdown_inflated = now - Duration::seconds(5);
    let pr = ClaimedPr {
        number: 4614,
        updated_at: Some(standdown_inflated),
        claim_labeled_at: Some(claimed_at),
        most_recent_claim_activity_at: None,
        head_ref_name: Some("some-doctor-branch".to_string()),
    };
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    match action {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
            assert!(
                age_minutes >= 30.0,
                "expected age derived from claim_labeled_at (~35m), got {age_minutes}"
            );
        }
        other => panic!(
            "expected an Aged reclaim driven by claim_labeled_at, got {other:?} \
                 (stand-down-comment-inflated updated_at must not mask staleness)"
        ),
    }
}

#[test]
fn decide_pr_keeps_when_claim_labeled_at_is_fresh_even_if_updated_at_is_old() {
    // The inverse of the case above, for completeness: a fresh
    // claim_labeled_at (recent reclaim) must read as fresh even when
    // updated_at happens to be stale (e.g. a partial/lagging API field),
    // confirming claim_labeled_at is genuinely primary, not just an
    // additional condition.
    let now = Utc::now();
    let recent_claim = now - Duration::minutes(1);
    let stale_updated_at = now - Duration::minutes(90);
    let pr = ClaimedPr {
        number: 4615,
        updated_at: Some(stale_updated_at),
        claim_labeled_at: Some(recent_claim),
        most_recent_claim_activity_at: None,
        head_ref_name: None,
    };
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    assert_eq!(action, PrReconcileAction::Keep);
}

#[test]
fn decide_pr_falls_back_to_updated_at_when_claim_labeled_at_unresolvable() {
    // When the timeline fetch failed/returned nothing (claim_labeled_at
    // is None), decide_pr must fall back to updated_at exactly like the
    // pre-#4618 behavior -- this is the fail-open case, not a second
    // route to the bug: a caller-side fetch failure should never be
    // amplified into either a spurious reclaim or a permanently-fresh
    // claim.
    let now = Utc::now();
    let old = now - Duration::minutes(60);
    let pr = claimed_pr(4616, Some(old), None);
    assert!(pr.claim_labeled_at.is_none());
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    match action {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. }) => {}
        other => panic!("expected fallback-to-updated_at Aged reclaim, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// decide_pr: most_recent_claim_activity_at anchor (Issue #4638 —
// restoring protection for a genuinely live, non-pid-joinable claimant
// after #4618 anchored solely on claim_labeled_at)
// ------------------------------------------------------------------

#[test]
fn decide_pr_keeps_when_old_claim_labeled_at_but_recent_genuine_comment() {
    // The exact #4638 shape: claim_labeled_at is 35 minutes old (past the
    // 30-minute threshold) and the PR is not pid-joinable (no journal
    // entry, no run-registry pid), but a claimant heartbeat carrying this
    // claim's activity marker was posted 1 minute ago (#6523) --
    // most_recent_claim_activity_at must refresh the anchor and the claim
    // must be kept, not reclaimed out from under a still-working claimant.
    let now = Utc::now();
    let claimed_at = now - Duration::minutes(35);
    let recent_genuine_comment = now - Duration::minutes(1);
    let pr = ClaimedPr {
        number: 4638,
        updated_at: Some(claimed_at),
        claim_labeled_at: Some(claimed_at),
        most_recent_claim_activity_at: Some(recent_genuine_comment),
        head_ref_name: Some("pr-worktree-review-branch".to_string()),
    };
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    assert_eq!(
        action,
        PrReconcileAction::Keep,
        "a genuine recent comment must refresh the anchor and prevent reclaim"
    );
}

#[test]
fn decide_pr_reclaims_when_old_claim_labeled_at_and_only_standdown_comments_since() {
    // Regression guard for #4618: the caller-side comment fetch excludes
    // marker-tagged stand-down comments, so a claim whose only comments
    // since the claim are stand-down notes surfaces
    // most_recent_claim_activity_at == None here (exactly like no
    // comments at all) -- the anchor must fall back to claim_labeled_at
    // alone and the stale claim must still be reclaimed. This is the
    // #4618 livelock this fix must not reopen.
    let now = Utc::now();
    let claimed_at = now - Duration::minutes(35);
    let pr = ClaimedPr {
        number: 4618,
        updated_at: Some(now - Duration::seconds(5)),
        claim_labeled_at: Some(claimed_at),
        most_recent_claim_activity_at: None,
        head_ref_name: Some("some-doctor-branch".to_string()),
    };
    let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
    match action {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(age_minutes >= 30.0);
            }
            other => panic!(
                "expected Aged reclaim -- marker-only comments must not refresh the anchor, got {other:?}"
            ),
        }
}

// ------------------------------------------------------------------
// Claim-activity marker: only the CLAIMANT's own heartbeat is liveness
// (Issue #6523 — bringing the daemon side in line with #6514's
// defaults/scripts/claim-staleness.sh)
// ------------------------------------------------------------------

/// The claim timestamp used by the marker fixtures below, rendered exactly
/// as the forge emits a `labeled` event's `created_at`.
fn fixture_claimed_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-19T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn comment(created_at: DateTime<Utc>, body: &str) -> PrComment {
    PrComment {
        created_at,
        body: body.to_string(),
    }
}

#[test]
fn claim_activity_marker_matches_claim_staleness_sh() {
    // The marker MUST be byte-identical to what
    // `defaults/scripts/claim-staleness.sh marker` prints:
    //   ACTIVITY_PREFIX='<!-- loom:claim-activity claim=' + CLAIMED_AT + ' -->'
    // with CLAIMED_AT the timeline `created_at` verbatim (which that
    // script validates as ^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$).
    assert_eq!(CLAIM_ACTIVITY_MARKER_PREFIX, "<!-- loom:claim-activity claim=");
    assert_eq!(
        claim_activity_marker(fixture_claimed_at()),
        "<!-- loom:claim-activity claim=2026-08-19T08:00:00Z -->"
    );
}

#[test]
fn most_recent_claim_activity_at_ignores_an_unrelated_comment() {
    // AC (a) / the #6513 shape reconstructed daemon-side: a routine
    // Builder post-push status note is not claimant liveness. Before
    // #6523 this comment WAS counted (it is not a stand-down note), which
    // is exactly the conflation #6514 removed on the agent side.
    let claimed_at = fixture_claimed_at();
    let comments = vec![
        comment(claimed_at + Duration::minutes(2), "Pushed the fix, CI running."),
        comment(claimed_at + Duration::minutes(9), "Champion: capped-PR notice."),
    ];
    assert_eq!(
        most_recent_claim_activity_at(&comments, claimed_at),
        None,
        "an unrelated comment must not count as claimant activity"
    );
}

#[test]
fn most_recent_claim_activity_at_counts_a_marked_claimant_heartbeat() {
    // AC (b): a comment carrying THIS claim's marker is claimant liveness.
    let claimed_at = fixture_claimed_at();
    let heartbeat_at = claimed_at + Duration::minutes(12);
    let comments = vec![
        comment(claimed_at + Duration::minutes(2), "Pushed the fix, CI running."),
        comment(
            heartbeat_at,
            &format!(
                "Doctor: still working the failing test.\n{}",
                claim_activity_marker(claimed_at)
            ),
        ),
    ];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), Some(heartbeat_at));
}

#[test]
fn most_recent_claim_activity_at_takes_the_newest_marked_heartbeat() {
    let claimed_at = fixture_claimed_at();
    let marker = claim_activity_marker(claimed_at);
    let newest = claimed_at + Duration::minutes(20);
    let comments = vec![
        comment(claimed_at + Duration::minutes(5), &marker),
        comment(newest, &marker),
        comment(claimed_at + Duration::minutes(12), &marker),
    ];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), Some(newest));
}

#[test]
fn most_recent_claim_activity_at_ignores_a_marker_for_a_different_claim() {
    // Mirrors claim-staleness.sh: the marker is matched against the
    // claim's OWN labeled-at timestamp, so a heartbeat left behind by an
    // earlier claim generation (before a reclaim + re-claim) cannot keep
    // the new claim alive.
    let claimed_at = fixture_claimed_at();
    let older_claim = claimed_at - Duration::minutes(45);
    let comments = vec![comment(
        claimed_at + Duration::minutes(3),
        &format!("Judge: reviewing.\n{}", claim_activity_marker(older_claim)),
    )];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
}

#[test]
fn most_recent_claim_activity_at_ignores_comments_at_or_before_the_claim() {
    let claimed_at = fixture_claimed_at();
    let marker = claim_activity_marker(claimed_at);
    let comments = vec![
        comment(claimed_at - Duration::minutes(1), &marker),
        comment(claimed_at, &marker),
    ];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
}

#[test]
fn most_recent_claim_activity_at_still_excludes_standdown_comments() {
    // #4618 regression guard, preserved: a stand-down comment is evidence
    // a LATER pass declined to reclaim, never claimant activity — even in
    // the pathological case where its body quotes an activity marker.
    let claimed_at = fixture_claimed_at();
    let comments = vec![comment(
            claimed_at + Duration::minutes(7),
            &format!(
                "Judge pass: standing down, not stomping.\n{}\n{STANDDOWN_MARKER_PREFIX}2026-08-19T08:00:00Z seq=2 -->",
                claim_activity_marker(claimed_at)
            ),
        )];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
}

#[test]
fn decide_pr_reclaims_when_only_unrelated_comments_since_the_claim() {
    // End-to-end AC (a): the #6513 livelock shape, daemon-side. A 35m-old
    // `loom:reviewing` claim on a chatty PR whose only comments since are
    // a Builder status note and a Champion notice. The claim-activity scan
    // yields None, so the anchor stays at claim_labeled_at and the stale
    // claim is reclaimed. Before #6523 that Builder note refreshed the
    // anchor and bought the dead claim another full 30-minute window.
    let now = Utc::now();
    let claimed_at = now - Duration::minutes(35);
    let scanned = most_recent_claim_activity_at(
        &[
            comment(now - Duration::minutes(20), "Pushed the fix, CI running."),
            comment(now - Duration::minutes(2), "Champion: merge-risk hold."),
        ],
        claimed_at,
    );
    assert_eq!(scanned, None, "neither comment is claimant activity");
    let pr = ClaimedPr {
        number: 6523,
        updated_at: Some(now - Duration::minutes(2)),
        claim_labeled_at: Some(claimed_at),
        most_recent_claim_activity_at: scanned,
        head_ref_name: Some("some-judge-branch".to_string()),
    };
    match decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, now) {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
            assert!(age_minutes >= DEFAULT_STALE_REVIEWING_MINUTES);
        }
        other => panic!(
            "expected an Aged reclaim -- an unrelated comment must not postpone it, got {other:?}"
        ),
    }
}

#[test]
fn decide_pr_keeps_when_a_marked_claimant_heartbeat_is_recent() {
    // End-to-end AC (b): same shape as above, except the claimant itself
    // posted a marked heartbeat 2 minutes ago -- that IS liveness, so the
    // claim is kept.
    let now = Utc::now();
    let claimed_at = now - Duration::minutes(35);
    let heartbeat_at = now - Duration::minutes(2);
    let scanned = most_recent_claim_activity_at(
        &[
            comment(now - Duration::minutes(20), "Pushed the fix, CI running."),
            comment(heartbeat_at, &claim_activity_marker(claimed_at)),
        ],
        claimed_at,
    );
    assert_eq!(scanned, Some(heartbeat_at));
    let pr = ClaimedPr {
        number: 6523,
        updated_at: Some(heartbeat_at),
        claim_labeled_at: Some(claimed_at),
        most_recent_claim_activity_at: scanned,
        head_ref_name: Some("some-judge-branch".to_string()),
    };
    assert_eq!(
        decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, now),
        PrReconcileAction::Keep
    );
}

#[test]
fn decide_pr_marked_heartbeat_extends_by_exactly_one_window_not_indefinitely() {
    // AC (c): the anchor is a max() of timestamps, not a boolean pin, so a
    // heartbeat buys exactly one more staleness window measured from the
    // HEARTBEAT's own timestamp -- matching claim-staleness.sh's "activity
    // resets the idle clock" rule. Same claim, same single heartbeat,
    // evaluated at two moments either side of that window.
    let claimed_at = Utc::now() - Duration::minutes(180);
    let heartbeat_at = claimed_at + Duration::minutes(5);
    let scanned = most_recent_claim_activity_at(
        &[comment(heartbeat_at, &claim_activity_marker(claimed_at))],
        claimed_at,
    );
    assert_eq!(scanned, Some(heartbeat_at));
    let pr = ClaimedPr {
        number: 6523,
        updated_at: Some(heartbeat_at),
        claim_labeled_at: Some(claimed_at),
        most_recent_claim_activity_at: scanned,
        head_ref_name: None,
    };

    // Just inside the window from the heartbeat: kept.
    let inside = heartbeat_at + Duration::minutes(29);
    assert_eq!(
        decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, inside),
        PrReconcileAction::Keep,
        "within one threshold window of the heartbeat the claim is still fresh"
    );

    // One minute past it: reclaimed. A single heartbeat cannot pin the
    // claim, however old the claim itself gets.
    let outside = heartbeat_at + Duration::minutes(31);
    match decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, outside) {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
            assert!(
                (age_minutes - 31.0).abs() < 0.5,
                "age must be measured from the heartbeat (~31m), got {age_minutes}"
            );
        }
        other => {
            panic!("expected an Aged reclaim one window past the heartbeat, got {other:?}")
        }
    }

    // Treating's longer floor behaves identically, just later.
    assert_eq!(
        decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_TREATING_MINUTES, outside),
        PrReconcileAction::Keep,
        "60m treating window is not yet exhausted 31m after the heartbeat"
    );
    match decide_pr(
        &pr,
        None,
        None,
        &|_| true,
        DEFAULT_STALE_TREATING_MINUTES,
        heartbeat_at + Duration::minutes(61),
    ) {
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. }) => {}
        other => panic!("expected an Aged reclaim 61m past the heartbeat, got {other:?}"),
    }
}

#[test]
fn decide_pr_age_floor_vetoes_reclaim_regardless_of_comment_activity() {
    // SAFETY (#4790/#4618 double-claim race): #6523 narrows what counts as
    // activity, which can only make a reclaim happen SOONER -- never
    // sooner than the 30m/60m age floor, which stays the veto no
    // comment-activity outcome can bypass. A claim younger than its floor
    // with NO claimant activity at all (the most reclaim-favourable
    // evidence this pass can see) must still be kept -- including when a
    // dead joined pid is also on the table.
    assert!((DEFAULT_STALE_REVIEWING_MINUTES - 30.0).abs() < f64::EPSILON);
    assert!((DEFAULT_STALE_TREATING_MINUTES - 60.0).abs() < f64::EPSILON);
    let now = Utc::now();
    let dead = journal_entry("/repo/a", 6523, 4618);

    for (label, floor) in [
        ("loom:reviewing", DEFAULT_STALE_REVIEWING_MINUTES),
        ("loom:treating", DEFAULT_STALE_TREATING_MINUTES),
    ] {
        // One minute short of the floor.
        let claimed_at = now - Duration::seconds(((floor - 1.0) * 60.0) as i64);
        let pr = ClaimedPr {
            number: 6523,
            updated_at: Some(now - Duration::seconds(5)),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: None,
            head_ref_name: Some("feature/issue-6523".to_string()),
        };
        assert_eq!(
            decide_pr(&pr, None, None, &|_| true, floor, now),
            PrReconcileAction::Keep,
            "{label}: under the age floor, no-activity must still Keep"
        );
        assert_eq!(
            decide_pr(&pr, Some(&dead), None, &|_| false, floor, now),
            PrReconcileAction::Keep,
            "{label}: the age floor vetoes even a dead joined pid"
        );
        assert_eq!(
            decide_pr(&pr, None, Some(4618), &|_| false, floor, now),
            PrReconcileAction::Keep,
            "{label}: the age floor vetoes even a dead run-registry pid"
        );

        // One minute past it: the same evidence now reclaims, confirming
        // the floor -- not the comment scan -- is what moved.
        let aged = ClaimedPr {
            claim_labeled_at: Some(now - Duration::seconds(((floor + 1.0) * 60.0) as i64)),
            ..pr.clone()
        };
        match decide_pr(&aged, None, None, &|_| true, floor, now) {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(age_minutes >= floor, "{label}: {age_minutes} < {floor}");
            }
            other => panic!("{label}: expected an Aged reclaim past the floor, got {other:?}"),
        }
    }
}

#[test]
fn parse_issue_from_branch_matches_convention() {
    assert_eq!(parse_issue_from_branch("feature/issue-42"), Some(42));
    assert_eq!(parse_issue_from_branch("feature/issue-4367"), Some(4367));
}

#[test]
fn parse_issue_from_branch_rejects_non_matching_shapes() {
    assert_eq!(parse_issue_from_branch("main"), None);
    assert_eq!(parse_issue_from_branch("fix/something"), None);
    assert_eq!(parse_issue_from_branch("feature/issue-"), None);
    assert_eq!(parse_issue_from_branch("feature/issue-abc"), None);
}

#[test]
fn plan_pr_joins_branch_to_journal_entry() {
    let now = Utc::now();
    let mut journal = SweepJournal::default();
    journal.entries.push(journal_entry("/repo/a", 42, 111)); // will be dead

    let prs = vec![
        claimed_pr(200, Some(now - Duration::minutes(45)), Some("feature/issue-42")),
        claimed_pr(201, Some(now - Duration::minutes(1)), None),
    ];

    let decisions = plan_pr("/repo/a", &prs, &journal, &|_| None, &|_| false, 30.0, now);

    assert_eq!(
        decisions[0],
        (200, PrReconcileAction::Reclaim(PrReclaimReason::DeadPid { pid: 111 }))
    );
    assert_eq!(decisions[1], (201, PrReconcileAction::Keep));
}

#[test]
fn plan_pr_consults_run_registry_only_when_journal_entry_absent() {
    let now = Utc::now();
    let journal = SweepJournal::default();

    let prs = vec![
        claimed_pr(300, Some(now - Duration::minutes(60)), Some("feature/issue-1")),
        claimed_pr(301, Some(now - Duration::minutes(1)), Some("feature/issue-2")),
    ];

    let run_registry_pid_for = |issue_num: u32| -> Option<u32> {
        match issue_num {
            1 => Some(555),
            2 => Some(777),
            _ => None,
        }
    };
    let is_alive = |pid: u32| pid == 777;

    let decisions = plan_pr("/repo/a", &prs, &journal, &run_registry_pid_for, &is_alive, 30.0, now);

    assert_eq!(
        decisions[0],
        (300, PrReconcileAction::Reclaim(PrReclaimReason::DeadRunRegistry { pid: 555 }))
    );
    assert_eq!(decisions[1], (301, PrReconcileAction::Keep));
}

#[test]
#[serial]
fn resolve_stale_reviewing_minutes_defaults_and_overrides() {
    std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
    assert!(
        (resolve_stale_reviewing_minutes() - DEFAULT_STALE_REVIEWING_MINUTES).abs() < f64::EPSILON
    );

    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "15");
    assert!((resolve_stale_reviewing_minutes() - 15.0).abs() < f64::EPSILON);

    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "0");
    assert!(
        (resolve_stale_reviewing_minutes() - DEFAULT_STALE_REVIEWING_MINUTES).abs() < f64::EPSILON
    );
    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "garbage");
    assert!(
        (resolve_stale_reviewing_minutes() - DEFAULT_STALE_REVIEWING_MINUTES).abs() < f64::EPSILON
    );

    std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
}

#[test]
#[serial]
fn resolve_stale_treating_minutes_defaults_and_overrides() {
    std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    assert!(
        (resolve_stale_treating_minutes() - DEFAULT_STALE_TREATING_MINUTES).abs() < f64::EPSILON
    );

    std::env::set_var(STALE_TREATING_MINUTES_ENV, "90");
    assert!((resolve_stale_treating_minutes() - 90.0).abs() < f64::EPSILON);

    std::env::remove_var(STALE_TREATING_MINUTES_ENV);
}

// ------------------------------------------------------------------
// Integration: forge::reconcile_pr_claims (Issue #4367)
// ------------------------------------------------------------------

/// Write a fake `gh` script (tests only) that logs every invocation to
/// `gh_log`, reports exactly one PR carrying the requested claim label
/// for `pr list`, and reports `extra_labels` (plus nothing else) for
/// `pr view --json labels,isDraft` -- letting a test control whether the
/// safety-net `loom:review-requested` backfill should fire.
fn write_fake_gh_pr(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    pr_number: u32,
    updated_at: &str,
    head_ref_name: &str,
    extra_labels: &[&str],
) -> std::path::PathBuf {
    write_fake_gh_pr_with_draft(
        dir,
        gh_log,
        pr_number,
        updated_at,
        head_ref_name,
        extra_labels,
        false,
    )
}

/// Same as [`write_fake_gh_pr`], but also lets a test control the PR's
/// `isDraft` value in the `pr view --json labels,isDraft` response — used
/// to reproduce #8250 (a stale claim reclaimed on a draft PR must not be
/// backfilled `loom:review-requested`).
fn write_fake_gh_pr_with_draft(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    pr_number: u32,
    updated_at: &str,
    head_ref_name: &str,
    extra_labels: &[&str],
    is_draft: bool,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-pr.sh");
    let labels_json = extra_labels
        .iter()
        .map(|l| format!(r#"{{"name":"{l}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":{pr_number},"updatedAt":"{updated_at}","headRefName":"{head_ref_name}"}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[{labels_json}],"isDraft":{is_draft}}}'
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

#[test]
#[serial]
fn reconcile_pr_claims_reclaims_stale_reviewing_and_backfills_state_label() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
    std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

    // No journal entry, no checkpoint -- non-joinable branch, well past
    // the 30-minute threshold, and no state label at all.
    let gh_log = dir.path().join("gh-invocations.log");
    let old = (Utc::now() - Duration::minutes(90)).to_rfc3339();
    let fake_gh = write_fake_gh_pr(dir.path(), &gh_log, 500, &old, "some-random-branch", &[]);

    let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

    // Only `loom:reviewing` is queried with results here (the fake `gh`
    // returns the same single PR for every `pr list` call, so both the
    // reviewing and treating passes see it) -- assert on the label-flip
    // evidence instead of the exact checked count to avoid overfitting
    // to that fixture quirk.
    assert!(checked >= 1);
    assert!(reclaimed >= 1);

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("pr edit 500 --remove-label loom:reviewing"),
        "expected loom:reviewing to be removed from #500; got: {gh_calls:?}"
    );
    assert!(
        gh_calls.contains("pr edit 500 --add-label loom:review-requested"),
        "expected the safety net to add loom:review-requested to #500; got: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
    std::env::remove_var(STALE_TREATING_MINUTES_ENV);
}

#[test]
#[serial]
fn reconcile_pr_claims_keeps_fresh_pr() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
    std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

    let gh_log = dir.path().join("gh-invocations.log");
    let fresh = Utc::now().to_rfc3339();
    let fake_gh = write_fake_gh_pr(
        dir.path(),
        &gh_log,
        501,
        &fresh,
        "some-random-branch",
        &["loom:review-requested"],
    );

    let (_checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

    assert_eq!(reclaimed, 0, "a fresh PR-side claim must never be reclaimed");

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--remove-label loom:reviewing")
            && !gh_calls.contains("--remove-label loom:treating"),
        "no claim label should have been removed; got: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
    std::env::remove_var(STALE_TREATING_MINUTES_ENV);
}

/// #8250: a stale `loom:reviewing`/`loom:treating` claim reclaimed off a
/// **draft** PR (`isDraft: true`, no state label) must NOT be backfilled
/// `loom:review-requested` -- Judge is not ready to look at a draft, so
/// the backfill would just waste a review pass. The claim label itself
/// must still be removed (the reclaim happens; only the backfill is
/// skipped).
#[test]
#[serial]
fn reconcile_pr_claims_reclaims_stale_claim_but_skips_backfill_on_draft_pr() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
    std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

    let gh_log = dir.path().join("gh-invocations.log");
    let old = (Utc::now() - Duration::minutes(90)).to_rfc3339();
    let fake_gh = write_fake_gh_pr_with_draft(
        dir.path(),
        &gh_log,
        502,
        &old,
        "some-random-branch",
        &[],
        true,
    );

    let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

    assert!(checked >= 1);
    assert!(reclaimed >= 1, "the stale claim label must still be reclaimed");

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("pr edit 502 --remove-label loom:reviewing"),
        "expected loom:reviewing to be removed from #502; got: {gh_calls:?}"
    );
    assert!(
        !gh_calls.contains("--add-label loom:review-requested"),
        "a draft PR must never be backfilled loom:review-requested; got: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
    std::env::remove_var(STALE_TREATING_MINUTES_ENV);
}

/// #8250 edge case: if `isDraft` is absent from the `pr view` response
/// (e.g. an older `gh` or a partial API response), the fail-safe default
/// is non-draft -- matching the existing accepted failure class for a
/// wholesale `gh pr view` failure (see `pr_label_names`'s doc comment)
/// -- so the backfill still fires rather than silently getting skipped.
#[test]
#[serial]
fn reconcile_pr_claims_backfills_when_is_draft_field_is_absent() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
    std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
    std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

    let gh_log = dir.path().join("gh-invocations.log");
    let old = (Utc::now() - Duration::minutes(90)).to_rfc3339();
    let fake_gh = dir.path().join("fake-gh-pr-no-draft-field.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":503,"updatedAt":"{old}","headRefName":"some-random-branch"}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[]}}'
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }

    let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

    assert!(checked >= 1);
    assert!(reclaimed >= 1);

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("pr edit 503 --add-label loom:review-requested"),
        "a missing isDraft field must default to non-draft, not panic/skip the backfill: {gh_calls:?}"
    );

    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
    std::env::remove_var(STALE_TREATING_MINUTES_ENV);
}

// Issue #4637: `gh api --paginate --jq` re-invokes the `--jq` filter once
// per page and concatenates the per-page results, so a `max // empty`
// filter against a timeline spanning more than one page (>100 events)
// yields one line per page rather than a single overall max.
// `parse_max_timestamp` must resolve the true max across every line.

#[test]
fn parse_max_timestamp_single_line_bare() {
    let stdout = b"2026-01-01T00:00:00Z\n";
    let parsed = forge::parse_max_timestamp(stdout).unwrap();
    assert_eq!(parsed.to_rfc3339(), "2026-01-01T00:00:00+00:00");
}

#[test]
fn parse_max_timestamp_multi_page_picks_max_out_of_order() {
    // Three pages' worth of per-page `max` lines, deliberately not in
    // chronological order, mirroring what `--paginate` concatenation
    // actually produces.
    let stdout = b"2026-01-01T00:00:00Z\n2026-03-15T12:30:00Z\n2026-02-01T00:00:00Z\n";
    let parsed = forge::parse_max_timestamp(stdout).unwrap();
    assert_eq!(parsed.to_rfc3339(), "2026-03-15T12:30:00+00:00");
}

#[test]
fn parse_max_timestamp_multi_page_skips_empty_and_null_lines() {
    // A page with no matching event emits an empty line (the `// empty`
    // fallback) or a literal `null`; both must be ignored, not treated
    // as "no timestamp anywhere".
    let stdout = b"\n2026-05-05T05:05:05Z\nnull\n";
    let parsed = forge::parse_max_timestamp(stdout).unwrap();
    assert_eq!(parsed.to_rfc3339(), "2026-05-05T05:05:05+00:00");
}

#[test]
fn parse_max_timestamp_returns_none_for_empty_output() {
    assert!(forge::parse_max_timestamp(b"").is_none());
    assert!(forge::parse_max_timestamp(b"\n\n").is_none());
    assert!(forge::parse_max_timestamp(b"null\n").is_none());
    assert!(forge::parse_max_timestamp(b"null\nnull\n").is_none());
}

#[test]
fn parse_max_timestamp_returns_none_for_garbage() {
    assert!(forge::parse_max_timestamp(b"not-a-timestamp\n").is_none());
    assert!(forge::parse_max_timestamp(b"not-a-timestamp\nalso-not-one\n").is_none());
}

#[test]
fn parse_max_timestamp_handles_json_quoted_lines() {
    let stdout = b"\"2026-01-01T00:00:00Z\"\n\"2026-06-06T06:06:06Z\"\n";
    let parsed = forge::parse_max_timestamp(stdout).unwrap();
    assert_eq!(parsed.to_rfc3339(), "2026-06-06T06:06:06+00:00");
}

/// A minimal fake `gh` that answers ONLY the `.../comments` lease probe
/// [`forge::fetch_freshest_lease_updated_at`] issues, for direct unit
/// coverage of the three-way [`forge::LeaseProbe`] outcome (Issue #7591)
/// without going through the full `reconcile_workspace` pass.
fn write_fake_gh_lease_probe_only(
    dir: &std::path::Path,
    outcome: &str, // "found:<rfc3339>" | "not-found" | "fail"
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-lease-probe-only.sh");
    let body = match outcome.strip_prefix("found:") {
        Some(ts) => format!("echo '\"{ts}\"'\nexit 0\n"),
        None if outcome == "not-found" => "exit 0\n".to_string(),
        None if outcome == "fail" => {
            "echo 'simulated transient gh api failure' >&2\nexit 1\n".to_string()
        }
        None => panic!("unknown outcome fixture: {outcome}"),
    };
    std::fs::write(&fake_gh, format!("#!/usr/bin/env bash\n{body}")).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

#[test]
fn fetch_freshest_lease_updated_at_distinguishes_found_not_found_and_read_failed() {
    let dir = tempdir().unwrap();

    let ts = "2026-01-01T00:00:00Z";
    let found_gh = write_fake_gh_lease_probe_only(dir.path(), &format!("found:{ts}"));
    assert_eq!(
        forge::fetch_freshest_lease_updated_at(&found_gh, dir.path(), 1),
        forge::LeaseProbe::Found(
            DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .with_timezone(&Utc)
        ),
        "a successful read that finds a lease comment must report Found(ts)"
    );

    let not_found_gh = write_fake_gh_lease_probe_only(dir.path(), "not-found");
    assert_eq!(
        forge::fetch_freshest_lease_updated_at(&not_found_gh, dir.path(), 1),
        forge::LeaseProbe::NotFound,
        "a successful read that finds nothing must report NotFound, never ReadFailed"
    );

    let fail_gh = write_fake_gh_lease_probe_only(dir.path(), "fail");
    assert_eq!(
        forge::fetch_freshest_lease_updated_at(&fail_gh, dir.path(), 1),
        forge::LeaseProbe::ReadFailed,
        "Issue #7591: a FAILED gh invocation (non-zero exit) must report ReadFailed, and must \
             NOT be collapsed into the same outcome as NotFound -- conflating the two is exactly \
             what let a transient forge read failure evict a still-live claim"
    );
}

// --- #5686 stale verdicts (loom:pr / loom:changes-requested) ---

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";

fn marker(sha: &str, token: &str) -> String {
    format!("Reviewed.\n\n<!-- loom:verdict-sha sha={sha} verdict={token} -->")
}

fn verdict_pr(kind: VerdictKind, head: Option<&str>, marker_sha: Option<&str>) -> VerdictPr {
    VerdictPr {
        number: 192,
        kind,
        head_sha: head.map(str::to_string),
        marker_sha: marker_sha.map(str::to_string),
        // The ordinary case: the PR's comments WERE read, so a `None`
        // marker means "confirmed unmarked" rather than "not looked at".
        marker_scan_ok: true,
        on_hold: false,
    }
}

#[test]
fn verdict_kind_label_and_marker_token_match_the_prompt_convention() {
    assert_eq!(VerdictKind::Approved.label(), "loom:pr");
    assert_eq!(VerdictKind::Approved.marker_token(), "approved");
    assert_eq!(VerdictKind::ChangesRequested.label(), "loom:changes-requested");
    assert_eq!(VerdictKind::ChangesRequested.marker_token(), "changes-requested");
}

#[test]
fn extract_verdict_sha_takes_the_newest_marker_of_the_matching_kind() {
    let bodies = vec![
        marker(SHA_A, "changes-requested"),
        "Doctor pushed a fix.".to_string(),
        marker(SHA_B, "changes-requested"),
    ];
    assert_eq!(
        extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested),
        Some(SHA_B.to_string())
    );
}

#[test]
fn extract_verdict_sha_filters_on_verdict_kind() {
    // A PR rejected at SHA_A and later approved at SHA_B carries markers
    // for BOTH. Only the one matching the currently-held label says
    // anything about the current verdict -- taking "newest marker of any
    // kind" would let the approval marker vouch for the rejection.
    let bodies = vec![
        marker(SHA_A, "changes-requested"),
        marker(SHA_B, "approved"),
    ];
    assert_eq!(
        extract_latest_verdict_sha(&bodies, VerdictKind::Approved),
        Some(SHA_B.to_string())
    );
    assert_eq!(
        extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested),
        Some(SHA_A.to_string())
    );
}

#[test]
fn extract_verdict_sha_is_none_without_a_marker_of_that_kind() {
    let bodies = vec!["LGTM, approving.".to_string(), marker(SHA_A, "approved")];
    assert_eq!(extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested), None);
    assert_eq!(extract_latest_verdict_sha(&[], VerdictKind::Approved), None);
}

#[test]
fn extract_verdict_sha_ignores_a_malformed_marker() {
    // Non-hex / too-short SHAs and a missing verdict= token must not
    // produce a bogus marker_sha that could invalidate a live verdict.
    let bodies = vec![
        "<!-- loom:verdict-sha sha=zzzz verdict=approved -->".to_string(),
        "<!-- loom:verdict-sha sha=1111 verdict=approved -->".to_string(),
        format!("<!-- loom:verdict-sha sha={SHA_A} -->"),
    ];
    assert_eq!(extract_latest_verdict_sha(&bodies, VerdictKind::Approved), None);
}

#[test]
fn decide_verdict_keeps_when_the_marker_matches_the_current_head() {
    assert_eq!(
        decide_verdict(&verdict_pr(VerdictKind::ChangesRequested, Some(SHA_A), Some(SHA_A))),
        VerdictAction::Keep(VerdictKeepReason::Fresh)
    );
}

#[test]
fn decide_verdict_invalidates_a_rejection_after_a_force_push() {
    // The rjwalters/repo#192 incident: verdict rendered at SHA_A, branch
    // rebased+force-pushed to SHA_B, label never moved.
    assert_eq!(
        decide_verdict(&verdict_pr(VerdictKind::ChangesRequested, Some(SHA_B), Some(SHA_A))),
        VerdictAction::Invalidate {
            marker_sha: SHA_A.to_string(),
            head_sha: SHA_B.to_string(),
        }
    );
}

#[test]
fn decide_verdict_invalidates_a_stale_approval_the_dangerous_direction() {
    assert_eq!(
        decide_verdict(&verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(SHA_A))),
        VerdictAction::Invalidate {
            marker_sha: SHA_A.to_string(),
            head_sha: SHA_B.to_string(),
        }
    );
}

#[test]
fn decide_verdict_fails_safe_without_a_marker() {
    // Every verdict written before #5686 shipped is in this state --
    // clearing them all on rollout is exactly what must NOT happen.
    assert_eq!(
        decide_verdict(&verdict_pr(VerdictKind::Approved, Some(SHA_B), None)),
        VerdictAction::Keep(VerdictKeepReason::Unverifiable)
    );
}

#[test]
fn decide_verdict_fails_safe_without_a_head_sha() {
    assert_eq!(
        decide_verdict(&verdict_pr(VerdictKind::Approved, None, Some(SHA_A))),
        VerdictAction::Keep(VerdictKeepReason::NoHeadSha)
    );
}

#[test]
fn decide_verdict_respects_an_explicit_hold() {
    // Stale, but clearing it would un-park a PR an operator (or
    // Champion's capped-PR recovery pass) deliberately held.
    let mut pr = verdict_pr(VerdictKind::ChangesRequested, Some(SHA_B), Some(SHA_A));
    pr.on_hold = true;
    assert_eq!(decide_verdict(&pr), VerdictAction::Keep(VerdictKeepReason::Held));
}

#[test]
fn decide_verdict_accepts_an_abbreviated_marker_sha_that_prefixes_the_head() {
    let pr = verdict_pr(VerdictKind::Approved, Some(SHA_A), Some(&SHA_A[..8]));
    assert_eq!(decide_verdict(&pr), VerdictAction::Keep(VerdictKeepReason::Fresh));
}

#[test]
fn verdict_hold_labels_cover_every_parking_label() {
    assert!(VERDICT_HOLD_LABELS.contains(&"loom:blocked"));
    assert!(VERDICT_HOLD_LABELS.contains(&"loom:operator"));
    assert!(VERDICT_HOLD_LABELS.contains(&"loom:operator-only"));
}

// --- #6319 anchoring an unmarked verdict --------------------------------
//
// The gap this closes: the verdict-sha marker exists only because
// judge.md asks the model to append it, and in production it is dropped
// roughly one verdict in four. Every dropped marker silently reinstates
// the pre-#5686 hazard, and until now that state had no counter, no log
// line, and no remediation anywhere in the daemon.

#[test]
fn decide_anchor_stamps_the_current_head_for_a_confirmed_unmarked_verdict() {
    // The observed production case: an approving verdict with no marker.
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_B), None)),
        AnchorAction::Anchor {
            head_sha: SHA_B.to_string()
        }
    );
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::ChangesRequested, Some(SHA_A), None)),
        AnchorAction::Anchor {
            head_sha: SHA_A.to_string()
        }
    );
}

#[test]
fn decide_anchor_treats_an_empty_marker_as_unmarked() {
    // decide_verdict folds `Some("")` into Unverifiable; the anchoring
    // pass must agree, or an empty marker would be permanently stuck.
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(""))),
        AnchorAction::Anchor {
            head_sha: SHA_B.to_string()
        }
    );
}

#[test]
fn decide_anchor_never_touches_a_verdict_that_already_carries_a_marker() {
    // The AC that matters most: an already-marked verdict must behave
    // byte-for-byte as it did before #6319, fresh or stale.
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_A), Some(SHA_A))),
        AnchorAction::Skip(AnchorSkipReason::AlreadyAnchored)
    );
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(SHA_A))),
        AnchorAction::Skip(AnchorSkipReason::AlreadyAnchored)
    );
}

#[test]
fn decide_anchor_skips_a_held_pr() {
    // A held PR's comments are never fetched (list_verdict_prs skips the
    // call), so its marker state is unknown -- and a PR a human parked
    // should not collect automated comments either.
    let mut pr = verdict_pr(VerdictKind::Approved, Some(SHA_B), None);
    pr.on_hold = true;
    pr.marker_scan_ok = false;
    assert_eq!(decide_anchor(&pr), AnchorAction::Skip(AnchorSkipReason::Held));
}

#[test]
fn decide_anchor_skips_when_the_comment_scan_failed() {
    // A failed comment fetch is indistinguishable from "no marker".
    // Anchoring on it would post one duplicate marker comment per tick
    // for the whole duration of a GitHub API outage.
    let mut pr = verdict_pr(VerdictKind::Approved, Some(SHA_B), None);
    pr.marker_scan_ok = false;
    assert_eq!(decide_anchor(&pr), AnchorAction::Skip(AnchorSkipReason::MarkerScanFailed));
}

#[test]
fn decide_anchor_skips_without_a_resolvable_head_sha() {
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::Approved, None, None)),
        AnchorAction::Skip(AnchorSkipReason::NoHeadSha)
    );
    assert_eq!(
        decide_anchor(&verdict_pr(VerdictKind::Approved, Some(""), None)),
        AnchorAction::Skip(AnchorSkipReason::NoHeadSha)
    );
}

#[test]
fn decide_anchor_only_ever_fires_where_decide_verdict_said_unverifiable() {
    // Structural invariant: anchoring is a remediation for exactly one
    // decide_verdict outcome. If it ever fired on a Fresh/Invalidate/Held
    // PR it would be writing a marker over a live verdict decision.
    for pr in [
        verdict_pr(VerdictKind::Approved, Some(SHA_A), Some(SHA_A)), // Fresh
        verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(SHA_A)), // Invalidate
        verdict_pr(VerdictKind::Approved, None, Some(SHA_A)),        // NoHeadSha
    ] {
        assert_ne!(decide_verdict(&pr), VerdictAction::Keep(VerdictKeepReason::Unverifiable));
        assert!(matches!(decide_anchor(&pr), AnchorAction::Skip(_)));
    }
}

#[test]
fn verdict_reconcile_stats_report_the_residual_unanchored_exposure() {
    let mut stats = VerdictReconcileStats {
        checked: 4,
        invalidated: 1,
        unverifiable: 3,
        anchored: 2,
    };
    assert_eq!(stats.residual_unverifiable(), 1);
    stats.merge(VerdictReconcileStats {
        checked: 2,
        invalidated: 0,
        unverifiable: 1,
        anchored: 0,
    });
    assert_eq!(stats.checked, 6);
    assert_eq!(stats.invalidated, 1);
    assert_eq!(stats.unverifiable, 4);
    assert_eq!(stats.anchored, 2);
    assert_eq!(stats.residual_unverifiable(), 2);
    // Never underflows if a future caller anchors without counting.
    let odd = VerdictReconcileStats {
        unverifiable: 0,
        anchored: 1,
        ..VerdictReconcileStats::default()
    };
    assert_eq!(odd.residual_unverifiable(), 0);
}

/// Write a fake `gh` (tests only, #7018) for exercising
/// `forge::reconcile_pr_verdicts` end-to-end. PR #192 carries BOTH
/// terminal verdict labels simultaneously (`loom:pr` +
/// `loom:changes-requested` — the shape of the PR #6817 incident this
/// issue traces) and its only marker comment is a stale `approved` one
/// (recorded for `SHA_A`) while the reported head is `SHA_B`. `gh pr
/// list` returns the SAME PR for both label queries (`--label loom:pr`
/// and `--label loom:changes-requested`), matching how a real PR
/// carrying both labels shows up in both listings.
fn write_fake_gh_for_verdict_reconcile(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-verdict.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":192,"headRefOid":"{sha_b}","labels":[{{"name":"loom:pr"}},{{"name":"loom:changes-requested"}}]}}]'
  exit 0
fi
if [ "$1" = "api" ]; then
  echo '[{{"created_at":"2026-08-23T06:00:00Z","body":"Reviewed.\n\n<!-- loom:verdict-sha sha={sha_a} verdict=approved -->"}}]'
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
        sha_a = SHA_A,
        sha_b = SHA_B,
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// #7018: PR #6817 sat for five days carrying `loom:pr` AND
/// `loom:changes-requested` simultaneously because the clear step only
/// ever removed the ONE verdict label it detected as stale. Reproduce
/// the shape end-to-end: `reconcile_pr_verdicts` finds the PR under BOTH
/// verdict kinds (it is returned by both `gh pr list --label ...`
/// queries), the `Approved` kind is STALE (marker at `SHA_A`, head is
/// `SHA_B`) while the `ChangesRequested` kind has no marker of its own
/// (UNVERIFIABLE, kept) — so exactly ONE `invalidate_verdict` call
/// fires, and it must strip BOTH terminal verdict labels, not just
/// `loom:pr`.
#[test]
fn invalidate_verdict_strips_both_terminal_labels_not_just_the_detected_one() {
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = write_fake_gh_for_verdict_reconcile(dir.path(), &gh_log);

    let stats = forge::reconcile_pr_verdicts(&fake_gh, &repo_root);

    assert_eq!(stats.invalidated, 1, "exactly one invalidate_verdict call (Approved kind)");
    assert_eq!(stats.unverifiable, 1, "ChangesRequested kind has no marker of its own");

    let log = std::fs::read_to_string(&gh_log).unwrap();
    let edit_line = log
        .lines()
        .find(|l| l.starts_with("pr edit 192"))
        .unwrap_or_else(|| panic!("no `gh pr edit 192 ...` call recorded in:\n{log}"));
    assert!(
        edit_line.contains("--remove-label loom:pr"),
        "detected stale loom:pr must be removed: {edit_line}"
    );
    assert!(
        edit_line.contains("--remove-label loom:changes-requested"),
        "stray loom:changes-requested must ALSO be removed, not left behind (#7018): {edit_line}"
    );
    assert!(
        edit_line.contains("--add-label loom:review-requested"),
        "PR must be returned to the review queue: {edit_line}"
    );
}

#[test]
#[serial]
fn verdict_anchoring_is_enabled_by_default_and_killable_by_env() {
    let prev = std::env::var(VERDICT_ANCHOR_ENABLED_ENV).ok();
    std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV);
    assert!(verdict_anchoring_enabled(), "must default to ON");
    for off in ["0", "false", "no", "off", "OFF"] {
        std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, off);
        assert!(!verdict_anchoring_enabled(), "{off} must disable anchoring");
    }
    std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, "1");
    assert!(verdict_anchoring_enabled());
    match prev {
        Some(v) => std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, v),
        None => std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV),
    }
}

#[test]
#[serial]
fn verdict_staleness_is_enabled_by_default_and_killable_by_env() {
    let prev = std::env::var(VERDICT_STALENESS_ENABLED_ENV).ok();
    std::env::remove_var(VERDICT_STALENESS_ENABLED_ENV);
    assert!(verdict_staleness_enabled(), "must default to ON");
    for off in ["0", "false", "no", "off", "OFF"] {
        std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, off);
        assert!(!verdict_staleness_enabled(), "{off} must disable the pass");
    }
    std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, "1");
    assert!(verdict_staleness_enabled());
    match prev {
        Some(v) => std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, v),
        None => std::env::remove_var(VERDICT_STALENESS_ENABLED_ENV),
    }
}
