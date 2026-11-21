use super::*;

fn classify(
    enabled: Option<bool>,
    halted: bool,
    not_evaluated: bool,
    reason: Option<&str>,
    verdict_at: Option<DateTime<Utc>>,
) -> GateVerdict {
    // The 5-arg helper keeps the pre-#4259 test call sites unchanged: no
    // deferral, no tier label. Dedicated tests below exercise those paths
    // by calling `classify_gate_verdict` directly.
    classify_gate_verdict(enabled, halted, not_evaluated, false, reason, None, None, verdict_at)
}

#[test]
fn format_gate_status_names_the_actual_not_evaluated_cause() {
    // Pre-#3974 this line asserted "workspace tree is dirty" for EVERY
    // skip, so a `git fetch` failure on a completely clean tree was
    // reported as a dirty tree. The cause is now passed through verbatim.
    let v = classify(
        Some(true),
        false,
        true,
        Some("git-failure: `git -C /repo fetch origin main` failed (exit 128)"),
        None,
    );
    let s = format_gate_status(&v);
    assert!(s.contains("git-failure"), "got: {s}");
    assert!(s.contains("exit 128"), "got: {s}");
    assert!(!s.contains("dirty"), "must not assume a dirty tree: {s}");
    assert!(s.contains("NOT evidence about"), "got: {s}");
    assert!(s.contains("NOT halted"), "an unevaluated gate does not halt: {s}");

    // A dirty tree still reads as a dirty tree — because the gate said so.
    let v = classify(Some(true), false, true, Some("dirty-tree: [ M src/main.rs]"), None);
    let s = format_gate_status(&v);
    assert!(s.contains("dirty-tree"), "got: {s}");
    assert!(s.contains("src/main.rs"), "got: {s}");
}

#[test]
fn format_gate_status_covers_all_halted_and_not_evaluated_states() {
    let clear = classify(Some(true), false, false, None, Some(Utc::now()));
    assert!(format_gate_status(&clear).starts_with("clear (dispatch allowed"));

    let halted = classify(Some(true), true, false, None, None);
    let s = format_gate_status(&halted);
    assert!(s.starts_with("HALTED"), "got: {s}");
    assert!(s.contains("verified red"), "got: {s}");

    // Both at once: a prior verified-red halt persists while the next tick
    // cannot evaluate.
    let both = classify(Some(true), true, true, Some("timeout: gate command timed out"), None);
    let s = format_gate_status(&both);
    assert!(s.contains("HALTED"), "got: {s}");
    assert!(s.contains("NOT EVALUATED"), "got: {s}");
    assert!(s.contains("timeout"), "got: {s}");

    // A missing cause degrades gracefully rather than inventing one.
    let no_cause = classify(Some(true), false, true, None, None);
    let s = format_gate_status(&no_cause);
    assert!(s.contains("cause unrecorded"), "got: {s}");
}

/// #4012: the core regression this issue fixes — a fresh, enabled gate
/// that has never completed an evaluation must render distinctly from a
/// verified-green gate, even though both allow dispatch (`halted: false`,
/// `not_evaluated: false` in both cases pre-#4012).
#[test]
fn format_gate_status_distinguishes_pending_from_clear() {
    let pending = classify(Some(true), false, false, None, None);
    assert_eq!(pending, GateVerdict::Pending);
    let s = format_gate_status(&pending);
    assert!(s.starts_with("pending"), "got: {s}");
    assert!(s.contains("dispatch allowed"), "got: {s}");
    assert!(!s.contains("clear"), "must not read as verified-green: {s}");

    let now = Utc::now();
    let clear = classify(Some(true), false, false, None, Some(now));
    assert_eq!(
        clear,
        GateVerdict::Clear {
            since: Some(now),
            tier: None
        }
    );
    let s = format_gate_status(&clear);
    assert!(s.starts_with("clear"), "got: {s}");
    assert!(s.contains(&now.to_rfc3339()), "clear must carry its own recency evidence: {s}");
    assert_ne!(
        format_gate_status(&pending),
        format_gate_status(&clear),
        "pending and clear must never render identically"
    );
}

/// #4259: a load-deferral is a distinct verdict — it must render as
/// `deferred (…)`, never as `not evaluated (timeout …)` nor as a stale
/// `clear`, and it must never halt dispatch.
#[test]
fn format_gate_status_deferred_is_distinct_and_never_halts() {
    let deferred = classify_gate_verdict(
        Some(true),
        false, // not halted
        false, // not unevaluated
        true,  // deferred
        None,
        Some("load 1.05/core for 14m — fast tier runs at the 30m bound"),
        None,
        None,
    );
    assert!(matches!(deferred, GateVerdict::Deferred { .. }));
    let s = format_gate_status(&deferred);
    assert!(s.starts_with("deferred"), "got: {s}");
    assert!(s.contains("load 1.05/core"), "carries the load reason: {s}");
    assert!(s.contains("NOT evidence about main"), "got: {s}");
    assert!(s.contains("NOT halted"), "a deferred gate does not halt: {s}");
    // Distinct from a timeout not-evaluated line for the same host stress.
    let timeout = classify(
        Some(true),
        false,
        true,
        Some("timeout: gate command timed out after 1200s"),
        None,
    );
    assert_ne!(
        format_gate_status(&deferred),
        format_gate_status(&timeout),
        "deferred (load) must never render identically to not-evaluated (timeout)"
    );
    assert_eq!(gate_status_short_label(&deferred), "deferred");
}

/// #4259: a fast-tier green must be labeled so it is never mistaken for a
/// full-suite green.
#[test]
fn format_gate_status_fast_tier_clear_is_labeled() {
    let now = Utc::now();
    let full =
        classify_gate_verdict(Some(true), false, false, false, None, None, Some("full"), Some(now));
    let fast =
        classify_gate_verdict(Some(true), false, false, false, None, None, Some("fast"), Some(now));
    let full_s = format_gate_status(&full);
    let fast_s = format_gate_status(&fast);
    assert!(full_s.starts_with("clear"), "got: {full_s}");
    assert!(!full_s.contains("fast tier"), "full tier is unlabeled: {full_s}");
    assert!(fast_s.contains("fast tier"), "fast tier is labeled: {fast_s}");
    assert!(
        fast_s.contains("NOT a full-suite green"),
        "the fast-tier caveat is explicit: {fast_s}"
    );
    assert_eq!(gate_status_short_label(&full), "clear");
    assert_eq!(gate_status_short_label(&fast), "clear(fast)");
    // The short label still fits the 13-char table column.
    assert!(gate_status_short_label(&fast).len() <= 13);
}

/// #4012 AC2: the gate-disabled case must be distinguishable from both
/// `pending` and `clear`.
#[test]
fn format_gate_status_distinguishes_disabled() {
    let disabled = classify(Some(false), false, false, None, None);
    assert_eq!(disabled, GateVerdict::Disabled);
    let s = format_gate_status(&disabled);
    assert!(s.starts_with("disabled"), "got: {s}");
    assert!(s.contains("dispatch allowed"), "got: {s}");

    let pending = classify(Some(true), false, false, None, None);
    assert_ne!(
        format_gate_status(&disabled),
        format_gate_status(&pending),
        "disabled and pending must never render identically"
    );

    // A disabled root that (implausibly) still carries a stale verdict
    // timestamp from before it was turned off still reports `Disabled` —
    // the enabled flag takes priority over verdict presence.
    let disabled_with_stale_verdict = classify(Some(false), false, false, None, Some(Utc::now()));
    assert_eq!(disabled_with_stale_verdict, GateVerdict::Disabled);
}

/// #4012 AC3: `pending` and `disabled` both still allow dispatch —
/// observability-only, no new halt path.
#[test]
fn pending_and_disabled_never_halt() {
    for verdict in [
        classify(Some(true), false, false, None, None),
        classify(Some(false), false, false, None, None),
    ] {
        assert!(
            !matches!(verdict, GateVerdict::Halted { .. }),
            "{verdict:?} must never be classified as halted"
        );
        let s = format_gate_status(&verdict);
        assert!(s.contains("dispatch allowed"), "got: {s}");
    }
}

/// An older daemon that never populated `main_health_gate_enabled` (wire
/// field absent ⇒ `None`, #4012) must not be misread as "disabled" — that
/// is exactly the `bool::default() == false` trap the `Option<bool>` wire
/// type exists to avoid.
#[test]
fn format_gate_status_legacy_none_enabled_is_not_disabled() {
    let v = classify(None, false, false, None, None);
    assert_ne!(v, GateVerdict::Disabled);
    // With no verdict either, it reads as pending (dispatch allowed) —
    // the conservative reading, never a fabricated "clear".
    assert_eq!(v, GateVerdict::Pending);
}

/// `halted`/`not_evaluated` always win over disabled/pending, matching the
/// gate loop's own soft-fail contract (its disabled path always clears
/// `halted` first) — this combination should only ever arise from a test
/// poking the raw state directly, and the renderer must still surface it
/// as halted rather than silently downgrading to "disabled".
#[test]
fn format_gate_status_halted_beats_disabled_and_pending() {
    let v = classify(Some(false), true, false, None, None);
    assert!(matches!(
        v,
        GateVerdict::Halted {
            not_evaluated: false,
            ..
        }
    ));
}

#[test]
fn gate_status_short_label_fits_table_width_and_matches_long_form() {
    let cases = [
        classify(Some(false), false, false, None, None),
        classify(Some(true), false, false, None, None),
        classify(Some(true), false, false, None, Some(Utc::now())),
        classify(Some(true), false, true, Some("timeout"), None),
        classify(Some(true), true, false, None, None),
        classify(Some(true), true, true, Some("timeout"), None),
    ];
    for v in cases {
        let short = gate_status_short_label(&v);
        assert!(short.len() <= 13, "{short:?} exceeds the 13-char HEALTH-GATE column");
    }
    // The short label and long form must agree on the halted/not distinction.
    let halted = classify(Some(true), true, false, None, None);
    assert_eq!(gate_status_short_label(&halted), "HALTED");
    assert!(format_gate_status(&halted).starts_with("HALTED"));
}

/// Issue #8288: the off-state short label must not be the word
/// "disabled". In the `HEALTH-GATE` column that read as "dispatch is
/// disabled for this workspace" (a real operator misread, 2026-09-18),
/// when it only ever meant the main-health gate feature is off — the
/// default — with dispatch entirely unaffected.
#[test]
fn gate_status_short_label_off_state_does_not_read_as_dispatch_disabled() {
    let off = classify(Some(false), false, false, None, None);
    assert_eq!(off, GateVerdict::Disabled);
    assert_eq!(gate_status_short_label(&off), "off");

    // The three gate-feature states an operator has to tell apart at a
    // glance must stay mutually distinct: off (feature not enabled,
    // dispatch allowed), pending (enabled, no verdict yet, dispatch
    // allowed), HALTED (enabled, main verified red, dispatch paused).
    let pending = classify(Some(true), false, false, None, None);
    let halted = classify(Some(true), true, false, None, None);
    let labels = [
        gate_status_short_label(&off),
        gate_status_short_label(&pending),
        gate_status_short_label(&halted),
    ];
    assert_eq!(labels, ["off", "pending", "HALTED"]);

    // The long form is unchanged and still spells out the consequence —
    // only the width-constrained column label was shortened.
    assert!(
        format_gate_status(&off).contains("dispatch allowed"),
        "got: {}",
        format_gate_status(&off)
    );
}

/// Issue #8288: the column header names the feature it reports on, and
/// still fits the 13-wide column the row values are formatted into — so
/// the header cannot silently widen the table past its separator rule.
#[test]
fn health_gate_column_header_names_the_gate_and_fits_the_column() {
    const HEADER: &str = "HEALTH-GATE";
    assert!(HEADER.len() <= 13, "{HEADER:?} exceeds the 13-char HEALTH-GATE column");
    assert_ne!(HEADER, "GATE", "the bare noun is what #8288 was about");
    // Header and separator must describe the same row width: the header
    // row is `2 + 4 + 2 + 9 + 2 + 13 + 2 + 5 + 2` of fixed columns before
    // the variable-width REPO column, and the separator is `2 + 68`.
    let header_row =
        format!("  {:>4}  {:>9}  {:<13}  {:<5}  REPO", "WGHT", "IN-FLIGHT", HEADER, "ROLES");
    let separator = format!("  {:-<68}", "");
    assert!(
        header_row.len() <= separator.len(),
        "header row {:?} ({} chars) outgrew the separator rule ({} chars)",
        header_row,
        header_row.len(),
        separator.len()
    );
}

/// Issue #5682: a workspace missing `.claude/commands/loom/sweep.md`
/// must render a `HEALTH-GATE` value that is visibly distinct from a
/// healthy idle repo's — the exact `HEALTH-GATE off` a fresh/unconfigured
/// repo (no `buildGate` block) already reports, which is what made the bug
/// invisible in `status` in the first place.
#[test]
fn gate_column_label_distinguishes_missing_sweep_command_from_healthy_idle() {
    // A healthy idle repo: gate off (no buildGate block configured),
    // nothing halted, nothing deferred — exactly the state a freshly
    // registered, empty-backlog repo reports.
    let healthy_idle_verdict = classify(Some(false), false, false, None, None);
    let healthy_idle_label = gate_column_label(false, &healthy_idle_verdict);
    assert_eq!(
        healthy_idle_label, "off",
        "sanity check: healthy idle repo's own HEALTH-GATE label passes through \
             gate_status_short_label unchanged (#8288 renamed it from \"disabled\")"
    );

    // Same underlying gate verdict — the bug is that today this is the
    // ONLY input to the column, so a sweep.md-less repo is indistinguishable.
    let missing_sweep_label = gate_column_label(true, &healthy_idle_verdict);
    assert_ne!(
        missing_sweep_label, healthy_idle_label,
        "a workspace missing /loom:sweep must not render the same HEALTH-GATE value \
             as a healthy idle repo"
    );
    assert_eq!(missing_sweep_label, "no-sweep");
    assert!(
        missing_sweep_label.len() <= 13,
        "{missing_sweep_label:?} exceeds the 13-char HEALTH-GATE column"
    );
}

/// The `sweep_command_missing` override must win regardless of the
/// underlying gate verdict — dispatch is refused unconditionally by this
/// condition, so even a `HALTED` gate must still surface as `no-sweep`
/// (the more specific, actionable reason) rather than `HALTED`.
#[test]
fn gate_column_label_missing_sweep_overrides_every_underlying_verdict() {
    let verdicts = [
        classify(Some(false), false, false, None, None),
        classify(Some(true), false, false, None, None),
        classify(Some(true), true, false, None, None),
        classify(Some(true), false, true, Some("timeout"), None),
    ];
    for v in verdicts {
        assert_eq!(gate_column_label(true, &v), "no-sweep");
    }
}

fn status_contract_report() -> DaemonStatusReport {
    let mut report = crate::cli::status::sample_report::sample_report();
    // The shared fixture uses the current time. Keep the compatibility inputs
    // deterministic so the full payload can also be compared before and after.
    report.main_health_gate_verdict_at = None;
    report.credential_preflight = None;
    report.per_repo = vec![super::role_runner_diagnostic_source_render_tests::repo(
        true,
        &["auditor"],
        None,
    )];
    report
}

fn render_status_contract(
    report: &DaemonStatusReport,
    worktrees: Option<&[WorktreeDiskSummary]>,
) -> serde_json::Value {
    let update = self_update::SelfUpdateStatus {
        built_commit: "abc1234".into(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    };
    // The fleet-store and pending-restart sections consult local daemon files.
    // Keep these contract fixtures independent of the machine running them.
    let sections = SectionSet::only(
        Section::all()
            .iter()
            .copied()
            .filter(|section| !matches!(section, Section::FleetStore | Section::PendingRestart)),
    );
    build_status_json_value_for(report, None, &update, None, None, worktrees, &sections)
}

fn assert_status_subtrees(actual: &serde_json::Value, expected: serde_json::Value) {
    for (key, value) in expected.as_object().unwrap() {
        // get() distinguishes an omitted key from a key whose value is null.
        assert_eq!(actual.get(key), Some(value), "status field {key}");
    }
}

#[test]
fn status_json_shared_types_keep_their_public_fields_and_values() {
    use crate::types::{
        IdleExitStatus, RateLimitBreakerStatus, RoleOnIdlePromotionStatus, RoleRunnerShardPosture,
        RoleRunnerShardStatus, RosterMemberStatus, RosterStatus, SafehouseStatus,
        UnregisteredLockedSweep,
    };

    let mut report = status_contract_report();
    report.unregistered_locked = vec![UnregisteredLockedSweep {
        root: "/repos/locked".into(),
        issue: 42,
        owner_pid: 321,
    }];
    report.role_runner_shard = Some(RoleRunnerShardPosture {
        index: Some(1),
        count: Some(3),
        summary: "sharded by roster".into(),
        configured: true,
        roster: Some(RosterStatus {
            issue: "acme/roster#55".into(),
            live_count: 1,
            seen_count: 2,
            generation: Some("2026-11-20T01:02:03Z".parse().unwrap()),
            settled_secs: Some(120),
            fence: Some("admitted".into()),
            members: vec![
                RosterMemberStatus {
                    host: "host-active".into(),
                    fresh: true,
                    last_beat_secs_ago: 7,
                    serves_count: 20,
                    is_this_host: true,
                },
                RosterMemberStatus {
                    host: "host-expired".into(),
                    fresh: false,
                    last_beat_secs_ago: 300,
                    serves_count: 0,
                    is_this_host: false,
                },
            ],
        }),
    });
    report.per_repo[0].role_runner_on_idle_promotions = vec![
        RoleOnIdlePromotionStatus {
            role: "auditor".into(),
            max_wait_secs: 60,
            age_secs: Some(70),
            promoted: true,
        },
        RoleOnIdlePromotionStatus {
            role: "hermit".into(),
            max_wait_secs: 90,
            age_secs: None,
            promoted: true,
        },
    ];
    report.per_repo[0].role_runner_shard = Some(RoleRunnerShardStatus {
        owned_here: false,
        key: "acme/repo".into(),
        key_source: "origin".into(),
        owning_shard: Some(2),
        host_shard: Some(1),
        shard_count: Some(3),
    });
    report.idle_exit = Some(IdleExitStatus {
        enabled: true,
        eligible: false,
        trigger: None,
        idle_minutes: 11,
        in_flight_sweeps: 2,
        active_role_runs: 3,
        healthy_tokens: 4,
        total_tokens: 5,
        idle_elapsed_secs: 82,
        starved_elapsed_secs: 19,
        starvation_enabled: true,
        observed_at: Some("2026-11-20T05:06:07Z".parse().unwrap()),
    });
    report.rate_limit_breaker = Some(Box::new(RateLimitBreakerStatus {
        enabled: true,
        phase: "cooldown".into(),
        suppressed: true,
        source: Some("work_finder".into()),
        tripped_at: Some("2026-11-20T08:00:00Z".parse().unwrap()),
        cooldown_until: Some("2026-11-20T08:02:00Z".parse().unwrap()),
        trips_total: 2,
        core_remaining: Some(0),
        graphql_remaining: Some(21),
        core_used: Some(5000),
        graphql_used: None,
        budget_probed_at: Some("2026-11-20T08:00:01Z".parse().unwrap()),
    }));
    report.safehouse = Some(SafehouseStatus {
        state: "connected".into(),
        socket: Some("/fixture/safehouse.sock".into()),
        room: Some("fixture-room".into()),
        reason: None,
    });
    let worktrees = [
        WorktreeDiskSummary {
            root: "/repos/unknown".into(),
            total_count: 6,
            issue_count: 1,
            pr_count: 2,
            other_count: 3,
            total_bytes: None,
        },
        WorktreeDiskSummary {
            root: "/repos/empty".into(),
            total_bytes: Some(0),
            ..WorktreeDiskSummary::default()
        },
    ];

    let json = render_status_contract(&report, Some(&worktrees));
    // The expected public schema is independent of the shared Rust serializers.
    assert_status_subtrees(
        &json,
        serde_json::json!({
            "unregistered_locked": [{"root": "/repos/locked", "issue": 42, "owner_pid": 321}],
            "role_runner_shard": {
                "index": 1, "count": 3, "configured": true, "summary": "sharded by roster",
                "roster": {
                    "issue": "acme/roster#55", "live_count": 1, "seen_count": 2,
                    "generation": "2026-11-20T01:02:03Z", "settled_secs": 120, "fence": "admitted",
                    "members": [
                        {"host": "host-active", "fresh": true, "last_beat_secs_ago": 7, "serves_count": 20, "is_this_host": true},
                        {"host": "host-expired", "fresh": false, "last_beat_secs_ago": 300, "serves_count": 0, "is_this_host": false}
                    ]
                }
            },
            "worktrees": [
                {"root": "/repos/unknown", "total_count": 6, "issue_count": 1, "pr_count": 2, "other_count": 3, "total_bytes": null},
                {"root": "/repos/empty", "total_count": 0, "issue_count": 0, "pr_count": 0, "other_count": 0, "total_bytes": 0}
            ],
            "idle_exit": {
                "enabled": true, "eligible": false, "trigger": null, "idle_minutes": 11,
                "in_flight_sweeps": 2, "active_role_runs": 3, "healthy_tokens": 4, "total_tokens": 5,
                "idle_elapsed_secs": 82, "starved_elapsed_secs": 19, "starvation_enabled": true,
                "observed_at": "2026-11-20T05:06:07Z"
            },
            "rate_limit_breaker": {
                "enabled": true, "phase": "cooldown", "suppressed": true, "source": "work_finder",
                "tripped_at": "2026-11-20T08:00:00Z", "cooldown_until": "2026-11-20T08:02:00Z",
                "trips_total": 2, "core_remaining": 0, "graphql_remaining": 21, "core_used": 5000,
                "graphql_used": null, "budget_probed_at": "2026-11-20T08:00:01Z"
            },
            "safehouse": {"state": "connected", "socket": "/fixture/safehouse.sock", "room": "fixture-room", "reason": null}
        }),
    );
    assert_status_subtrees(
        &json["per_repo"][0],
        serde_json::json!({
            "role_runner_on_idle_promotions": [
                {"role": "auditor", "max_wait_secs": 60, "age_secs": 70, "promoted": true},
                {"role": "hermit", "max_wait_secs": 90, "age_secs": null, "promoted": true}
            ],
            "role_runner_shard": {
                "owned_here": false, "key": "acme/repo", "key_source": "origin",
                "owning_shard": 2, "host_shard": 1, "shard_count": 3
            }
        }),
    );
}

#[test]
fn status_json_shared_types_keep_null_and_empty_distinct() {
    let mut report = status_contract_report();
    let uncollected = render_status_contract(&report, None);
    assert_status_subtrees(
        &uncollected,
        serde_json::json!({
            "unregistered_locked": [], "role_runner_shard": null, "worktrees": null,
            "idle_exit": null, "rate_limit_breaker": null, "safehouse": null
        }),
    );
    assert_status_subtrees(
        &uncollected["per_repo"][0],
        serde_json::json!({"role_runner_on_idle_promotions": [], "role_runner_shard": null}),
    );

    report.role_runner_shard = Some(crate::types::RoleRunnerShardPosture {
        index: None,
        count: None,
        summary: "not configured".into(),
        configured: false,
        roster: None,
    });
    let collected_empty = render_status_contract(&report, Some(&[]));
    assert_status_subtrees(
        &collected_empty,
        serde_json::json!({
            "worktrees": [],
            "role_runner_shard": {
                "index": null, "count": null, "configured": false, "summary": "not configured", "roster": null
            }
        }),
    );
}
