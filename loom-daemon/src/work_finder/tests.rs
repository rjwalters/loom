use super::*;
use serial_test::serial;

// ===================================================================
// Healthy-account transition tracking (#4344)
// ===================================================================

fn snap(total: usize, available: usize) -> capacity::RankingSnapshot {
    capacity::RankingSnapshot {
        total,
        available,
        exhausted: total - available,
        ..capacity::RankingSnapshot::default()
    }
}

#[test]
fn healthy_token_transition_dedups_by_state() {
    // The tracker only advances `prev` when the healthy count changes —
    // repeated identical ticks are no-ops (the once-per-transition contract
    // the AC requires). We assert on the carried state, since the log line
    // itself is a side effect.
    let mut prev: Option<usize> = None;

    // First observation seeds silently.
    log_healthy_token_transition(&mut prev, 6, Some(&snap(7, 6)));
    assert_eq!(prev, Some(6));

    // Stable ticks: no change.
    log_healthy_token_transition(&mut prev, 6, Some(&snap(7, 6)));
    assert_eq!(prev, Some(6));

    // Drop to token-starved (0 healthy) — a transition.
    log_healthy_token_transition(&mut prev, 0, Some(&snap(7, 0)));
    assert_eq!(prev, Some(0));

    // Recovery to a new count — another transition.
    log_healthy_token_transition(&mut prev, 4, Some(&snap(7, 4)));
    assert_eq!(prev, Some(4));

    // No-ranking fallback path (raw pool size) still tracks the count.
    log_healthy_token_transition(&mut prev, 3, None);
    assert_eq!(prev, Some(3));
}

// ===================================================================
// min_available_tokens_across_roots (issue #7527)
// ===================================================================

/// A repo-local pool exhausted by its own `.ranking` must dominate the
/// minimum even though another registered root's pool is fully healthy —
/// the exact divergence the single `fallback_root` probe (`token_limit`)
/// cannot see, since it only ever probes one directory.
#[test]
#[serial]
fn min_available_tokens_across_roots_takes_the_minimum_of_each_roots_own_pool() {
    std::env::set_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV, "");

    let healthy_root = tempfile::tempdir().unwrap();
    let exhausted_root = tempfile::tempdir().unwrap();

    let healthy_pool = healthy_root.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&healthy_pool).unwrap();
    std::fs::write(healthy_pool.join("a.token"), "key-a").unwrap();
    std::fs::write(healthy_pool.join("b.token"), "key-b").unwrap();
    std::fs::write(healthy_pool.join(".ranking"), "a|available\nb|available\n").unwrap();

    let exhausted_pool = exhausted_root.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&exhausted_pool).unwrap();
    std::fs::write(exhausted_pool.join("c.token"), "key-c").unwrap();
    std::fs::write(exhausted_pool.join(".ranking"), "c|exhausted\n").unwrap();

    let roots = vec![
        healthy_root.path().to_path_buf(),
        exhausted_root.path().to_path_buf(),
    ];
    let min = min_available_tokens_across_roots(&roots);

    std::env::remove_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV);

    assert_eq!(min, Some(0), "the exhausted root's 0-healthy pool must dominate the minimum");
}

/// Two healthy roots with different account counts: the minimum is the
/// smaller of the two, not a sum or an average.
#[test]
#[serial]
fn min_available_tokens_across_roots_picks_the_smaller_healthy_count() {
    std::env::set_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV, "");

    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();

    let pool_a = root_a.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&pool_a).unwrap();
    std::fs::write(pool_a.join("a1.token"), "key").unwrap();
    std::fs::write(pool_a.join("a2.token"), "key").unwrap();
    std::fs::write(pool_a.join(".ranking"), "a1|available\na2|available\n").unwrap();

    let pool_b = root_b.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&pool_b).unwrap();
    std::fs::write(pool_b.join("b1.token"), "key").unwrap();
    std::fs::write(pool_b.join(".ranking"), "b1|available\n").unwrap();

    let roots = vec![root_a.path().to_path_buf(), root_b.path().to_path_buf()];
    let min = min_available_tokens_across_roots(&roots);

    std::env::remove_var(crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV);

    assert_eq!(min, Some(1));
}

/// No roots to probe -> `None`, so the caller falls back to `token_limit`
/// rather than a synthetic zero that would look like total exhaustion.
#[test]
fn min_available_tokens_across_roots_is_none_for_empty_roots() {
    assert_eq!(min_available_tokens_across_roots(&[]), None);
}

// ===================================================================
// Mock source + dispatcher
// ===================================================================

fn issue(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".to_string()])
}

// ===================================================================
// Repo-sharding slice preference (Issue #6243) — retired from dispatch
// (Issue #11682). The no-slice regression tests live in `tests/no_slice.rs`.
// ===================================================================

// ===================================================================
// RegistryDispatcher — production dispatch-path regression (Issue #3967)
// ===================================================================

// ===================================================================
// Restart survivorship — journal-seeded capacity (Issue #6262)
// ===================================================================

// ===================================================================
// tick — dispatch scheduling
// ===================================================================

// ===================================================================
// tick_with_admission_cap — per-tick ramp cap (#4234, Gap 3 of #4231)
// ===================================================================

// ========================================================================
// Saturation admission brake (#4903)
// ========================================================================

#[test]
fn test_saturation_deferrals_reach_the_published_tick_summary() {
    // AC4's plumbing: the counter must survive into the process-global
    // summary `loom-daemon health` / `status` read back, and the summary
    // line must NAME it rather than hide it among the zeros.
    let report = TickReport {
        seen: 4,
        deferred_saturation: 4,
        saturation_held: true,
        ..TickReport::default()
    };
    publish_tick_summary(&report, 12);
    let summary = last_tick_summary().expect("a tick was just published");
    assert_eq!(summary.deferred_saturation, 4);
    assert!(summary.saturation_held);
    let line = summary.reason_summary();
    assert!(line.contains("4 deferred-saturation"), "got: {line}");
    assert!(line.contains("SATURATION-HELD"), "got: {line}");
    reset_last_tick_summary();
}

// ===================================================================
// No-op re-dispatch cooldown (Issue #6670)
// ===================================================================

// ===================================================================
// Hard-exclusion labels + decline cooldown (Issue #7528)
// ===================================================================

#[test]
fn test_park_labels_are_the_non_building_subset_of_skip_labels() {
    // #4444: the dispatch-time guard keys on PARK_LABELS, so the two
    // constants must stay in lockstep — SKIP_LABELS is exactly
    // BUILDING_LABEL + PARK_LABELS (loom:operator among them since #11795) +
    // the bounced decision + `loom:epic` (#11399), and PARK_LABELS
    // must never contain `loom:building` (a guard that refused it would break
    // the watchdogs' and the reaper's re-dispatch of the daemon's OWN claim).
    assert!(
        !PARK_LABELS.contains(&BUILDING_LABEL),
        "PARK_LABELS must exclude {BUILDING_LABEL}: it is legitimately present on a \
             watchdog / checkpoint-resume re-dispatch of the daemon's own claim"
    );
    for park in PARK_LABELS.iter() {
        assert!(
            SKIP_LABELS.contains(park),
            "{park} is a park label, so the work-finder query must skip it too"
        );
    }
    let mut expected = [&[BUILDING_LABEL], PARK_LABELS.as_slice()].concat();
    expected.extend([
        "loom:decision-malformed", // #11480
        labels::EPIC_LABEL,
    ]);
    assert_eq!(
        *SKIP_LABELS, expected,
        "SKIP_LABELS is the claim + the parks + the bounced decision + the epic container"
    );
}

// The vibesql#6664 operator-hold skip-not-park test lives in its own
// sibling file rather than being appended here: this module is over the
// 1000-line ratchet threshold and frozen at its current size
// (`.loom/docs/file-size-policy.md`), the same pattern `roster_fence`
// already uses in this file. `epic_container` (#11399) follows it.
mod epic_container;
mod operator_hold;
mod restore_veto;

#[test]
fn test_hard_exclusion_labels_are_disjoint_from_skip_labels() {
    // #7528: the hard-exclusion list is deliberately NOT folded into
    // SKIP_LABELS. Two reasons, both load-bearing:
    //
    //  1. Accounting — a SKIP_LABELS hit lands in `labeled-skip`, and
    //     conflating "a human parked this" with "this issue is not Loom's
    //     to work on yet" hides an intake backlog inside the park tally.
    //  2. `is_skipped_with_capabilities`'s #6893 exemption reasons about
    //     SKIP_LABELS membership to decide what the mechanical lane may
    //     relax; a hard exclusion must never be relaxable by anything.
    //
    // So assert the two sets stay disjoint. If a future label genuinely
    // belongs in both, that is a deliberate decision that should have to
    // edit this test.
    //
    // #11743: `loom:frozen` is that deliberate decision. It must refuse every
    // route: as a park (the dispatch-time guard, so also a skip by #4444) AND
    // as a hard exclusion (every role and daemon pass that reads the
    // `hard_exclusion` property). The skip filter runs first, so it counts
    // once, as `labeled-skip`, a human park. It cannot be relaxed, because it
    // is a capability `LANE_VETO_LABELS` entry.
    for excluded in crate::hard_exclusion::HARD_EXCLUSION_LABELS
        .iter()
        .filter(|l| **l != "loom:frozen")
    {
        assert!(
            !SKIP_LABELS.contains(excluded),
            "{excluded} is a hard exclusion (#7528), counted as `declined-skip`; it must \
                 not also be a SKIP_LABELS entry (`labeled-skip`)"
        );
        // And the work item's own park predicate must stay blind to it,
        // so the two filters cannot double-count one candidate.
        let item = WorkItem::new(1, vec!["loom:issue".into(), (*excluded).into()]);
        assert!(
            !item.is_skipped(),
            "the park predicate must not claim {excluded}: the dedicated #7528 filter owns it"
        );
        assert_eq!(crate::hard_exclusion::declining_label(&item.labels), Some(*excluded));
    }
}

// ===================================================================
// tick — reactive main-health halt (Phase C, #3812)
// ===================================================================

// ===================================================================
// tick_multi — multi-workspace fan-out (#3928)
// ===================================================================

// Cross-repo priority ordering (#3946, #11103) and the #9244 red-main-fix
// lane live in sibling files (this one is size-frozen).
mod main_red_fix;
mod ordering;

// ===================================================================
// WorkItem
// ===================================================================

#[test]
fn test_work_item_is_skipped() {
    assert!(!issue(1).is_skipped());
    assert!(WorkItem::new(1, vec!["loom:building".into()]).is_skipped());
    assert!(WorkItem::new(1, vec!["loom:blocked".into()]).is_skipped());
    assert!(WorkItem::new(1, vec!["loom:operator-only".into()]).is_skipped());
    assert!(!WorkItem::new(1, vec!["loom:curated".into()]).is_skipped());
}

// ===================================================================
// Capability-aware `loom:operator-mechanical` lane (#6893)
//
// `is_skipped()` / `is_skipped_with_extra()` are deliberately untouched
// by this lane — every case below asserts the pre-#6893 behavior still
// holds through those two, and only `is_skipped_with_capabilities()`
// with a NON-EMPTY held set ever differs.
// ===================================================================

/// The capability set a host declares it holds.
fn caps(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).to_string()).collect()
}

/// A ready capability-only park (#11861): `loom:issue` + `loom:blocked`, no
/// named blocker, whose body carries the given capability markers.
fn mechanical_item(number: u32, markers: &[&str]) -> WorkItem {
    let body = markers
        .iter()
        .map(|m| format!("<!-- loom:capability={m} -->"))
        .collect::<Vec<_>>()
        .join("\n");
    WorkItem::new(number, vec!["loom:issue".into(), "loom:blocked".into()])
        .with_body(Some(format!("## Task\n\nDo the thing.\n\n{body}\n")))
}

#[test]
fn test_mechanical_item_with_declared_capability_held_is_dispatchable() {
    // AC1: the whole point — a mechanical item whose declared capability
    // this worker holds is no longer unconditionally parked.
    let item = mechanical_item(1, &["host-sudo"]);
    assert!(item.is_skipped(), "the base label still parks it for every other caller");
    assert!(item.is_skipped_with_extra(&[]), "the #6685 path is unchanged");
    assert!(
        !item.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])),
        "a capability-declaring mechanical item is dispatchable to a worker that holds it"
    );
    assert_eq!(
        item.mechanical_routing(&caps(&["host-sudo"])),
        crate::capability::MechanicalRouting::ProposeDispatch,
        "AC4: the only lane it may enter is propose/dry-run mode"
    );
}

#[test]
fn test_mechanical_item_stays_parked_when_the_capability_is_not_held() {
    let item = mechanical_item(1, &["host-sudo"]);
    // No declaration at all (the default on every host).
    assert!(item.is_skipped_with_capabilities(&[], &BTreeSet::new()));
    // A different capability.
    assert!(item.is_skipped_with_capabilities(&[], &caps(&["tailnet-access"])));
    assert_eq!(
        item.mechanical_routing(&caps(&["tailnet-access"])),
        crate::capability::MechanicalRouting::MissingCapabilities {
            missing: vec!["host-sudo".to_string()]
        },
        "the gap is named, so the caller can turn the park into a capability request"
    );
}

#[test]
fn test_mechanical_capabilities_are_anded_not_ored() {
    let item = mechanical_item(1, &["host-sudo", "cloud-profile:prod-aws"]);
    assert!(
        item.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])),
        "holding one of two declared capabilities is not enough"
    );
    assert!(
        !item.is_skipped_with_capabilities(&[], &caps(&["host-sudo", "cloud-profile:prod-aws"]))
    );
}

#[test]
fn test_mechanical_item_without_a_marker_stays_parked_exactly_as_today() {
    // Fail-closed: "no capability declared" is not "no capability needed".
    let bare = WorkItem::new(1, vec!["loom:issue".into(), "loom:blocked".into()])
        .with_body(Some("## Task\n\nRotate the deploy key.\n".to_string()));
    assert!(bare.is_skipped_with_capabilities(&[], &caps(&["host-sudo", "tailnet-access"])));
    // ...and so does one whose body was never fetched.
    let no_body = WorkItem::new(2, vec!["loom:blocked".into()]);
    assert!(no_body.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])));
}

#[test]
fn test_mechanical_item_with_an_unknown_capability_fails_closed() {
    // An out-of-vocabulary value is exactly as undispatchable as none, even
    // when the worker "holds" the same typo'd string.
    let item = mechanical_item(1, &["host-sudo", "root"]);
    assert!(item.is_skipped_with_capabilities(&[], &caps(&["host-sudo"])));
    assert!(item.is_skipped_with_capabilities(&[], &caps(&["host-sudo", "root"])));
}

#[test]
fn test_operator_holds_remain_hard_skipped_for_a_capable_worker() {
    // #11861: no role acts on `loom:operator` (under any name), so a marker in
    // its body is ignored entirely, even for a maximally-capable worker.
    let all_caps = caps(&["host-sudo", "forge-admin-token", "tailnet-access"]);
    for hold in [
        "loom:operator",
        "loom:operator-only",
        "loom:operator-mechanical",
        "loom:operator-decision",
        "loom:operator-blocked",
    ] {
        let item = WorkItem::new(1, vec!["loom:issue".into(), hold.into()])
            .with_body(Some("<!-- loom:capability=host-sudo -->".to_string()));
        assert!(
            item.is_skipped_with_capabilities(&[], &all_caps),
            "{hold} must stay hard-skipped"
        );
        assert_eq!(
            item.mechanical_routing(&all_caps),
            crate::capability::MechanicalRouting::NotApplicable,
            "{hold} is not a capability-lane item at all"
        );
    }
}

#[test]
fn test_capability_exemption_never_overrides_another_skip_reason() {
    let markers = "<!-- loom:capability=host-sudo -->";
    let held = caps(&["host-sudo"]);
    // loom:operator beside the capability park: a human must act (#11861).
    let operator = WorkItem::new(1, vec!["loom:blocked".into(), "loom:operator".into()])
        .with_body(Some(markers.to_string()));
    assert!(operator.is_skipped_with_capabilities(&[], &held));
    // A park record naming a blocker: it waits on another change too.
    let named = WorkItem::new(2, vec!["loom:blocked".into()])
        .with_body(Some(format!("{markers}\n<!-- loom:park Blocked by: #9 by=curator -->")));
    assert!(named.is_skipped_with_capabilities(&[], &held));
    // loom:building — the daemon's own in-flight claim.
    let building = WorkItem::new(3, vec!["loom:blocked".into(), "loom:building".into()])
        .with_body(Some(markers.to_string()));
    assert!(building.is_skipped_with_capabilities(&[], &held));
    // A repo-configured extra skip label (#6685) still wins too.
    let extra = mechanical_item(4, &["host-sudo"]);
    let extra = WorkItem::new(
        extra.number,
        [extra.labels.clone(), vec!["blocked-upstream".into()]].concat(),
    )
    .with_body(extra.body.clone());
    assert!(extra.is_skipped_with_capabilities(&["blocked-upstream".to_string()], &held));
    assert!(
        !extra.is_skipped_with_capabilities(&[], &held),
        "...and without that configured label it is dispatchable again"
    );
}

#[test]
fn test_empty_held_set_is_byte_for_byte_is_skipped_with_extra() {
    // The default on every host: the lane is completely inert.
    let extra = ["blocked-upstream".to_string()];
    for item in [
        issue(1),
        WorkItem::new(2, vec!["loom:blocked".into()]),
        WorkItem::new(3, vec!["loom:operator-only".into()]),
        WorkItem::new(4, vec!["loom:issue".into(), "blocked-upstream".into()]),
        mechanical_item(5, &["host-sudo"]),
        mechanical_item(6, &[]),
    ] {
        assert_eq!(
            item.is_skipped_with_capabilities(&[], &BTreeSet::new()),
            item.is_skipped_with_extra(&[]),
            "#{} with no extra labels",
            item.number
        );
        assert_eq!(
            item.is_skipped_with_capabilities(&extra, &BTreeSet::new()),
            item.is_skipped_with_extra(&extra),
            "#{} with an extra skip label",
            item.number
        );
    }
}

#[test]
fn test_capability_lane_does_not_touch_ordinary_items() {
    // A plain `loom:issue` row is dispatchable with or without the lane, and
    // an operator hold with no capability marker is parked with or without it.
    let held = caps(&["host-sudo", "forge-admin-token", "tailnet-access"]);
    assert!(!issue(1).is_skipped_with_capabilities(&[], &held));
    let operator = WorkItem::new(2, vec!["loom:issue".into(), "loom:operator".into()])
        .with_body(Some("an alert: no capability marker".to_string()));
    assert!(
        operator.is_skipped_with_capabilities(&[], &held),
        "#11795: the lane keys on the marker; without one nothing changes"
    );
}

// ===================================================================
// Host-affinity constraint (#7456)
// ===================================================================

/// A ready `loom:issue` item pinned to `host_id` via the `loom:host:<id>`
/// label convention.
fn host_pinned_item(number: u32, host_id: &str) -> WorkItem {
    WorkItem::new(number, vec!["loom:issue".into(), format!("loom:host:{host_id}")])
}

/// A ready `loom:issue` item pinned to `host_id` via the body-marker
/// convention.
fn host_pinned_item_via_marker(number: u32, host_id: &str) -> WorkItem {
    issue(number).with_body(Some(format!("## Task\n\n<!-- loom:requires-host={host_id} -->\n")))
}

#[test]
fn work_item_host_constraint_reads_the_label_convention() {
    let item = host_pinned_item(1, "loom-worker-2");
    assert_eq!(item.host_constraint().required, hosts_set(&["loom-worker-2"]));
}

#[test]
fn work_item_host_constraint_reads_the_marker_convention() {
    let item = host_pinned_item_via_marker(1, "loom-worker-2");
    assert_eq!(item.host_constraint().required, hosts_set(&["loom-worker-2"]));
}

#[test]
fn work_item_with_no_declaration_has_an_empty_host_constraint() {
    assert!(issue(1).host_constraint().is_empty());
}

fn hosts_set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).to_string()).collect()
}

// ===================================================================
// Additional configurable skip-label list (Issue #6685)
// ===================================================================

#[test]
fn test_is_skipped_with_extra_matches_configured_label() {
    let item = WorkItem::new(1, vec!["loom:issue".into(), "blocked-upstream".into()]);
    assert!(!item.is_skipped(), "a repo-local label alone is not a SKIP_LABELS entry");
    assert!(
        item.is_skipped_with_extra(&["blocked-upstream".to_string()]),
        "the same label IS a skip once configured as an extra skip label"
    );
    assert!(
        !item.is_skipped_with_extra(&["some-other-label".to_string()]),
        "an unrelated extra label must not false-positive"
    );
}

#[test]
fn test_is_skipped_with_extra_empty_list_is_byte_for_byte_is_skipped() {
    assert!(!issue(1).is_skipped_with_extra(&[]));
    assert!(WorkItem::new(1, vec!["loom:blocked".into()]).is_skipped_with_extra(&[]));
}

#[test]
fn test_is_skipped_with_extra_never_needs_building_label_to_skip() {
    // Regression guard (#6685 AC3): SKIP_LABELS already includes
    // BUILDING_LABEL — is_skipped_with_extra must still report a
    // loom:building item as skipped (it's the pre-existing, intentional
    // in-flight-claim skip), and an EXTRA list containing it changes
    // nothing (still skipped, for the same original reason).
    let building = WorkItem::new(1, vec!["loom:building".into()]);
    assert!(building.is_skipped_with_extra(&[]));
    assert!(building.is_skipped_with_extra(&["loom:building".to_string()]));
}

#[test]
fn test_resolve_extra_skip_labels_precedence_and_parsing() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);

    // Default: no config, no env ⇒ empty.
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(resolve_extra_skip_labels_with_config(&cfg), Vec::<String>::new());

    // Config supplies the list.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"extraSkipLabels": ["blocked-upstream", " needs-vendor-fix ", ""]}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec![
            "blocked-upstream".to_string(),
            "needs-vendor-fix".to_string()
        ],
        "whitespace is trimmed and empty entries dropped"
    );

    // Env overrides config entirely (comma-separated).
    std::env::set_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV, "env-label-a, env-label-b ,,");
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec!["env-label-a".to_string(), "env-label-b".to_string()]
    );
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);
}

#[test]
fn test_resolve_extra_skip_labels_never_includes_building_label() {
    // Regression guard (#6685 AC3): even a misconfigured operator
    // naming `loom:building` explicitly must never come back out of
    // resolution — SKIP_LABELS' own "loom:building is never a park"
    // invariant must survive a bad config/env value.
    let cfg = WorkFinderConfig {
        extra_skip_labels: Some(vec!["loom:building".to_string(), "blocked-upstream".to_string()]),
        ..Default::default()
    };
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec!["blocked-upstream".to_string()],
        "loom:building must be filtered out of the resolved extra list"
    );

    std::env::set_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV, "loom:building,blocked-upstream");
    assert_eq!(
        resolve_extra_skip_labels_with_config(&cfg),
        vec!["blocked-upstream".to_string()],
        "the same guard applies to the env-var source"
    );
    std::env::remove_var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV);
}

// ===================================================================
// Self-declared re-check interval marker (Issue #6685)
// ===================================================================

/// A ready issue whose body carries the `<!-- loom:recheck-interval=... -->`
/// marker and a synthetic `updatedAt`, built on `issue`.
fn issue_with_recheck_interval(n: u32, value: &str, updated_at: &str) -> WorkItem {
    issue(n)
        .with_body(Some(format!(
            "## Context\n\nSome body text.\n\n<!-- loom:recheck-interval={value} -->\n"
        )))
        .with_updated_at(Some(updated_at.to_string()))
}

#[test]
fn test_recheck_interval_marker_parsing() {
    assert_eq!(
        issue_with_recheck_interval(1, "6h", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(6 * 3600))
    );
    assert_eq!(
        issue_with_recheck_interval(1, "45m", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(45 * 60))
    );
    assert_eq!(
        issue_with_recheck_interval(1, "2d", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(2 * 86_400))
    );
    assert_eq!(
        issue_with_recheck_interval(1, "3600", "2026-01-01T00:00:00Z").recheck_interval(),
        Some(Duration::from_secs(3600)),
        "a bare integer defaults to seconds"
    );
    // No marker at all.
    assert_eq!(issue(1).recheck_interval(), None);
    // Malformed / zero / unrecognized unit — all degrade to None, never
    // an error.
    assert_eq!(
        issue_with_recheck_interval(1, "0h", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
    assert_eq!(
        issue_with_recheck_interval(1, "abc", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
    assert_eq!(
        issue_with_recheck_interval(1, "5w", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
    assert_eq!(
        issue_with_recheck_interval(1, "", "2026-01-01T00:00:00Z").recheck_interval(),
        None
    );
}

#[test]
fn test_is_within_recheck_interval() {
    let now = chrono::Utc::now();
    let five_minutes_ago = (now - chrono::Duration::minutes(5)).to_rfc3339();
    let two_hours_ago = (now - chrono::Duration::hours(2)).to_rfc3339();

    let fresh = issue_with_recheck_interval(1, "1h", &five_minutes_ago);
    assert!(
        fresh.is_within_recheck_interval(now),
        "updated 5m ago, 1h interval ⇒ still within the window"
    );

    let stale = issue_with_recheck_interval(1, "1h", &two_hours_ago);
    assert!(
        !stale.is_within_recheck_interval(now),
        "updated 2h ago, 1h interval ⇒ elapsed, due for a recheck"
    );

    // No marker ⇒ never within the (nonexistent) interval.
    assert!(!issue(1).is_within_recheck_interval(now));

    // Marker present but no updated_at ⇒ never within the interval
    // (degrades to "always due", never silently suppresses dispatch).
    let no_timestamp = issue(1).with_body(Some("<!-- loom:recheck-interval=6h -->".to_string()));
    assert!(!no_timestamp.is_within_recheck_interval(now));
}

/// `recheck_interval_until` (Issue #9311) computes `updated_at + interval`
/// regardless of whether the window has already elapsed — it names the
/// instant a hold clears, not whether it currently holds — and falls
/// through to `None` on exactly the same missing-half cases as
/// [`WorkItem::is_within_recheck_interval`].
#[test]
fn test_recheck_interval_until() {
    let now = chrono::Utc::now();
    let five_minutes_ago = now - chrono::Duration::minutes(5);

    let fresh = issue_with_recheck_interval(1, "1h", &five_minutes_ago.to_rfc3339());
    assert_eq!(
        fresh.recheck_interval_until(),
        Some(five_minutes_ago + chrono::Duration::hours(1))
    );

    // Elapsed windows still report their (past) expiry — this is "when did
    // it clear", not "is it still held".
    let two_hours_ago = now - chrono::Duration::hours(2);
    let stale = issue_with_recheck_interval(1, "1h", &two_hours_ago.to_rfc3339());
    assert_eq!(stale.recheck_interval_until(), Some(two_hours_ago + chrono::Duration::hours(1)));

    assert_eq!(issue(1).recheck_interval_until(), None, "no marker");
    let no_timestamp = issue(1).with_body(Some("<!-- loom:recheck-interval=6h -->".to_string()));
    assert_eq!(no_timestamp.recheck_interval_until(), None, "marker but no updated_at");
}

// ===================================================================
// Env-var configuration
// ===================================================================

#[test]
#[serial]
fn test_enabled_off_by_default() {
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    assert!(!enabled(), "unset ⇒ disabled (zero behavior change)");
}

#[test]
#[serial]
fn test_enabled_truthy_values() {
    for v in ["1", "true", "yes", "on", "TRUE", "On", " Yes "] {
        std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
        assert!(enabled(), "{v:?} should enable");
    }
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_enabled_falsy_values() {
    for v in ["0", "false", "no", "off", "", "maybe"] {
        std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
        assert!(!enabled(), "{v:?} should not enable");
    }
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_interval_default_and_override() {
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));

    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "120");
    assert_eq!(resolve_interval(), Duration::from_secs(120));

    // Zero and unparseable fall back to the default.
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "garbage");
    assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
}

#[test]
#[serial]
fn test_resolve_max_concurrent_default_and_override() {
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);

    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "7");
    assert_eq!(resolve_max_concurrent(), 7);

    // Zero and unparseable fall back to the default.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
    assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
}

// ===================================================================
// resolve_dynamic_max_concurrent — Phase B work-driven policy (#3811),
// per-token concurrency factor (#3947), CPU term REMOVED (#4512)
// ===================================================================

/// #11911 (2am#4463): the budget is the one configured number; headroom only
/// halts at exhaustion.
#[test]
fn test_dynamic_cap_is_the_configured_number_or_a_halt() {
    assert_eq!(resolve_dynamic_max_concurrent(3, usize::MAX, 20), 20, "disk does not size it");
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 2, 20), 20, "nor does RAM");
    assert_eq!(resolve_dynamic_max_concurrent(0, usize::MAX, 20), 0, "a full disk halts");
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 0, 20), 0, "exhausted RAM halts");
}

// ===================================================================
// Eager reclaim trigger (#7512) — the coupling between this loop's
// 60s disk read and the worktree reaper's 15-minute reclaim cadence.
// These mirror the dynamic-cap tests directly above: same inputs, but
// asserting *whether reclaim is attempted* rather than the cap value.
// ===================================================================

#[test]
fn test_eager_reclaim_fires_when_disk_is_about_to_zero_the_cap() {
    // The #7512 headline case: disk 0, RAM and ceiling both healthy. The
    // cap is about to be finalized as 0, so the eager pass must be
    // attempted first.
    assert_eq!(resolve_dynamic_max_concurrent(0, 8, 10), 0);
    assert!(crate::eager_reclaim::should_trigger(false, 0, 8, 10));
}

#[test]
fn test_eager_reclaim_fires_when_disk_merely_binds_the_cap_down() {
    // Disk has not reached 0 yet but is already costing dispatch slots
    // (cap 1 instead of 8). Reclaiming here is what keeps it off 0.
    assert_eq!(resolve_dynamic_max_concurrent(1, 8, 10), 1);
    assert!(crate::eager_reclaim::should_trigger(false, 1, 8, 10));
}

#[test]
fn test_eager_reclaim_does_not_fire_on_an_ordinary_tick() {
    // Healthy disk: the ceiling binds, not disk. No reclaim — this is the
    // overwhelmingly common tick and it must stay side-effect-free.
    assert_eq!(resolve_dynamic_max_concurrent(36, usize::MAX, 10), 10);
    assert!(!crate::eager_reclaim::should_trigger(false, 36, usize::MAX, 10));
}

#[test]
fn test_eager_reclaim_does_not_fire_when_ram_is_the_binding_axis() {
    // Cap is 1, but disk is not why — freeing disk would not buy a single
    // slot, so the forge/docker round-trips would be pure waste.
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 1, 8), 1);
    assert!(!crate::eager_reclaim::should_trigger(false, usize::MAX, 1, 8));
}

#[test]
fn test_eager_reclaim_does_not_fire_on_an_unmeasurable_disk_probe() {
    // `disk_headroom_limit` returns usize::MAX when `df` is unmeasurable
    // (#4164, unknown != zero) and the cap is left unclamped by disk — so
    // the eager pass must not fire either.
    assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, usize::MAX, 10), 10);
    assert!(!crate::eager_reclaim::should_trigger(false, usize::MAX, usize::MAX, 10));
}

#[test]
fn test_eager_reclaim_fires_once_per_crossing_not_once_per_tick() {
    // The exact property that keeps the 60s dispatch loop from becoming a
    // forge-polling loop while a disk stays full for a reason the daemon
    // cannot reclaim (a dataset, another tenant): ten consecutive
    // zero-disk ticks trigger exactly ONE eager pass.
    let mut was_binding = false;
    let mut fired = 0usize;
    for _ in 0..10 {
        if crate::eager_reclaim::should_trigger(was_binding, 0, 8, 10) {
            fired += 1;
        }
        // Post-reclaim re-probe still reads 0 — nothing was reclaimable.
        was_binding = crate::eager_reclaim::disk_axis_binds_cap_down(0, 8, 10);
        assert_eq!(resolve_dynamic_max_concurrent(0, 8, 10), 0, "cap stays 0 either way");
    }
    assert_eq!(fired, 1, "edge-triggered, not level-triggered");
}

#[test]
fn test_eager_reclaim_rearms_after_a_successful_reclaim() {
    // A pass that actually frees space takes the disk term back above the
    // binding threshold; a LATER drop is a genuine new crossing and must
    // fire again rather than being suppressed forever.
    let mut was_binding = false;
    assert!(crate::eager_reclaim::should_trigger(was_binding, 0, 8, 10));
    // Post-reclaim re-probe: 12 GB-worth of headroom recovered.
    was_binding = crate::eager_reclaim::disk_axis_binds_cap_down(12, 8, 10);
    assert!(!was_binding);
    assert_eq!(resolve_dynamic_max_concurrent(12, 8, 10), 8, "cap recovered to the ram term");
    // Hours later it fills up again.
    assert!(crate::eager_reclaim::should_trigger(was_binding, 0, 8, 10));
}

#[test]
fn test_dynamic_cap_unbounded_by_max_concurrent_default() {
    // A machine whose operator has NOT tuned the knob rides the shipped
    // default rather than an estimate of its cores or its token pool.
    assert_eq!(
        resolve_dynamic_max_concurrent(100, usize::MAX, DEFAULT_WORK_FINDER_MAX_CONCURRENT),
        DEFAULT_WORK_FINDER_MAX_CONCURRENT
    );
}

// ===================================================================
// Dynamic cap composed with tick — scale-up / scale-to-zero (#3811)
// ===================================================================

// ===================================================================
// Config-file surface — read_work_finder_config soft-fail (#3813)
// ===================================================================

fn write_config(dir: &std::path::Path, body: &str) {
    let loom_dir = dir.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    std::fs::write(loom_dir.join("config.json"), body).unwrap();
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_file_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_malformed_json_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "{not valid json");
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_autonomous_block_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"terminals": []}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_work_finder_block_is_all_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
fn test_config_full_block_is_parsed() {
    let tmp = tempfile::tempdir().unwrap();
    // `perTokenConcurrency` is deliberately included here even though it is
    // retired (#5743) and no longer has a corresponding `WorkFinderConfig`
    // field — this is exactly the "a fleet host still has the old key in its
    // committed config" scenario, and parsing must silently ignore it (a
    // `serde_json::Value` walk never errors on an unread key) rather than
    // failing to start.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"perTokenConcurrency": 4, "cpuUtilizationTarget": 0.6, "estCoresPerSweep": 3.5, "workFinder": {"enabled": true, "intervalSecs": 90, "maxConcurrent": 5, "maxAdmissionsPerTick": 4}}}"#,
    );
    assert_eq!(
        read_work_finder_config(tmp.path()),
        WorkFinderConfig {
            enabled: Some(true),
            interval_secs: Some(90),
            max_concurrent: Some(5),
            max_admissions_per_tick: Some(4),
            // Retired keys are recorded (accepted-but-ignored), not parsed.
            deprecated_cpu_keys: vec!["cpuUtilizationTarget", "estCoresPerSweep"],
            // Keys this body does not set (`extraSkipLabels`,
            // `maxConcurrentPerRepo`) parse to their `Default`, `None` — for
            // the #9090 per-repo cap that means *uncapped*. Still an exact
            // equality assertion over the whole struct.
            ..WorkFinderConfig::default()
        }
    );
}

// ===================================================================
// Retired cpuUtilizationTarget / estCoresPerSweep knobs: accepted but
// IGNORED, never a config error (#4512, replacing the #4032 parsing tests)
// ===================================================================

#[test]
fn test_deprecated_cpu_knobs_are_accepted_and_recorded_not_parsed() {
    // A fleet upgrades the daemon binary before it edits every repo's
    // committed config, so a stale key must parse fine and simply do nothing
    // — recorded only so the deprecation warning can name it.
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"cpuUtilizationTarget": 0.5, "estCoresPerSweep": 1.5,
                "workFinder": {"enabled": true, "maxConcurrent": 10}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.deprecated_cpu_keys, vec!["cpuUtilizationTarget", "estCoresPerSweep"]);
    // The live knobs in the same block still parse normally: a deprecated
    // sibling must never poison the rest of the config.
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.max_concurrent, Some(10));
}

#[test]
#[serial(loom_config_env)]
fn test_deprecated_cpu_knobs_accepted_at_any_value_including_nonsense() {
    // Pre-#4512 these were range-filtered/type-checked because a value was
    // consumed. Nothing consumes them now, so out-of-range, wrong-type, and
    // even absurd values are all equally inert — and equally non-fatal.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    for body in [
        r#"{"autonomous": {"cpuUtilizationTarget": 0}}"#,
        r#"{"autonomous": {"cpuUtilizationTarget": 1.5}}"#,
        r#"{"autonomous": {"estCoresPerSweep": -2}}"#,
        r#"{"autonomous": {"estCoresPerSweep": "many"}}"#,
        r#"{"autonomous": {"estCoresPerSweep": true}}"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), body);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.deprecated_cpu_keys.len(), 1, "must be accepted-but-noted: {body}");
        // And it must not disturb any live knob.
        assert_eq!(cfg.max_concurrent, None);
        assert_eq!(cfg.enabled, None);
    }
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

#[test]
fn test_deprecated_cpu_knobs_absent_or_null_are_not_reported() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 4}}}"#);
    assert!(read_work_finder_config(tmp.path())
        .deprecated_cpu_keys
        .is_empty());

    // An explicit `null` is "not set" — warning about it would be noise.
    let tmp2 = tempfile::tempdir().unwrap();
    write_config(tmp2.path(), r#"{"autonomous": {"estCoresPerSweep": null}}"#);
    assert!(read_work_finder_config(tmp2.path())
        .deprecated_cpu_keys
        .is_empty());
}

#[test]
fn test_warn_deprecated_cpu_knobs_is_a_noop_without_any_retired_setting() {
    // No config keys, no env vars — nothing to warn about, and (crucially)
    // no panic and no config error. The one-shot `Once` inside means this
    // test cannot assert the log line itself; the observable contract is
    // that it is safe and side-effect-free on the clean path.
    warn_deprecated_cpu_knobs(&WorkFinderConfig::default());
}

#[test]
#[serial]
fn test_deprecated_cpu_knob_notice_is_none_when_nothing_is_set() {
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
    assert!(deprecated_cpu_knob_notice(&WorkFinderConfig::default()).is_none());
}

#[test]
#[serial]
fn test_deprecated_cpu_knob_notice_names_the_config_keys_that_are_set() {
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
    let cfg = WorkFinderConfig {
        deprecated_cpu_keys: vec!["estCoresPerSweep"],
        ..Default::default()
    };
    let notice = deprecated_cpu_knob_notice(&cfg).expect("a set key must produce a notice");
    assert!(notice.contains("estCoresPerSweep"), "must name the key: {notice}");
    assert!(notice.contains("IGNORED"), "must say it is ignored: {notice}");
    // Actionable: it must point at the knob that replaced it.
    assert!(
        notice.contains("autonomous.workFinder.maxConcurrent"),
        "must name the replacement knob: {notice}"
    );
    // A config-only notice must not fabricate an env source.
    assert!(!notice.contains("env LOOM_"), "no env source was set: {notice}");
}

#[test]
#[serial]
fn test_deprecated_cpu_knob_notice_names_env_vars_and_combines_both_sources() {
    std::env::set_var(DEPRECATED_CPU_ENV_VARS[0], "0.85");
    let env_only = deprecated_cpu_knob_notice(&WorkFinderConfig::default())
        .expect("a set env var must produce a notice");
    assert!(env_only.contains(DEPRECATED_CPU_ENV_VARS[0]), "{env_only}");

    let both = deprecated_cpu_knob_notice(&WorkFinderConfig {
        deprecated_cpu_keys: vec!["estCoresPerSweep"],
        ..Default::default()
    })
    .expect("notice");
    assert!(both.contains("estCoresPerSweep"), "{both}");
    assert!(both.contains(DEPRECATED_CPU_ENV_VARS[0]), "{both}");
    assert!(both.contains(" and "), "both sources must be joined: {both}");

    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
}

#[test]
fn test_deprecated_knob_name_tables_stay_in_sync() {
    // The config keys and env vars are two halves of one deprecation; a
    // future edit that adds one must add the other (the warning names both).
    assert_eq!(DEPRECATED_CPU_CONFIG_KEYS.len(), DEPRECATED_CPU_ENV_VARS.len());
    assert!(DEPRECATED_CPU_ENV_VARS
        .iter()
        .all(|v| v.starts_with("LOOM_")));
}

#[test]
#[serial(loom_config_env)]
fn test_config_retired_per_token_concurrency_key_is_ignored() {
    // #5743: `perTokenConcurrency` fed a disclaimed, causally-irrelevant
    // status number and has been fully retired — `WorkFinderConfig` no
    // longer has a field for it. A host whose committed
    // `.loom/config.json` still sets the key (this repo's own included, at
    // the time this issue was filed) must start cleanly and simply ignore
    // it, not fail to parse.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"perTokenConcurrency": 3}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, WorkFinderConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_enabled_false_is_disabled_flag() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": false}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.enabled, Some(false));
    assert_eq!(cfg.interval_secs, None);
    assert_eq!(cfg.max_concurrent, None);
}

#[test]
fn test_config_zero_interval_and_max_drop_to_none() {
    // A zero interval/max in config is treated as absent so it falls through
    // to the built-in default rather than a useless value.
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"enabled": true, "intervalSecs": 0, "maxConcurrent": 0}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.interval_secs, None);
    assert_eq!(cfg.max_concurrent, None);
}

// ===================================================================
// config_resolver migration (#4058) — tier precedence
// ===================================================================

fn write_project_config(dir: &std::path::Path, body: &str) {
    let full = dir.join(crate::config_resolver::PROJECT_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

fn write_local_config(dir: &std::path::Path, body: &str) {
    let full = dir.join(crate::config_resolver::LOCAL_CONFIG_REL);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_only_is_honored_like_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 5}}}"#,
    );
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.max_concurrent, Some(5));
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 5, "intervalSecs": 60}}}"#,
    );
    write_project_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 9}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    // Overlapping `maxConcurrent` -> project tier wins.
    assert_eq!(cfg.max_concurrent, Some(9));
    // Non-overlapping keys still supplied by the legacy tier.
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.interval_secs, Some(60));
}

#[test]
#[serial(loom_config_env)]
fn test_config_local_tier_overrides_legacy_and_project() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 5}}}"#);
    write_project_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 9}}}"#);
    write_local_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 2}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.max_concurrent, Some(2));
}

// ===================================================================
// Config-file surface — resolve_* precedence env > config > default (#3813)
// ===================================================================

#[test]
#[serial]
fn test_resolve_enabled_precedence() {
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);

    // Absent config + unset env ⇒ default off (zero behavior change).
    assert!(!resolve_enabled(&WorkFinderConfig::default()));

    // Config alone enables when env is unset.
    let on = WorkFinderConfig {
        enabled: Some(true),
        ..Default::default()
    };
    assert!(resolve_enabled(&on));
    let off = WorkFinderConfig {
        enabled: Some(false),
        ..Default::default()
    };
    assert!(!resolve_enabled(&off));

    // Env overrides config in both directions.
    std::env::set_var(WORK_FINDER_ENABLE_ENV, "1");
    assert!(resolve_enabled(&off), "env truthy overrides config=false");
    std::env::set_var(WORK_FINDER_ENABLE_ENV, "0");
    assert!(!resolve_enabled(&on), "env falsy overrides config=true");
    std::env::remove_var(WORK_FINDER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_interval_with_config_precedence() {
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_interval_with_config(&WorkFinderConfig::default()),
        Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS)
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        interval_secs: Some(120),
        ..Default::default()
    };
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));

    // Env overrides config.
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "45");
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(45));

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));
    std::env::set_var(WORK_FINDER_INTERVAL_ENV, "nope");
    assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));
    std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
}

#[test]
#[serial]
fn test_resolve_max_concurrent_with_config_precedence() {
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_max_concurrent_with_config(&WorkFinderConfig::default()),
        DEFAULT_WORK_FINDER_MAX_CONCURRENT
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        max_concurrent: Some(8),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);

    // Env overrides config.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 2);

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
}

/// #6203: [`resolve_max_concurrent_with_source`] must report the same
/// numeric precedence as [`resolve_max_concurrent_with_config`] while
/// additionally naming which layer supplied the value, so the startup log
/// line can tell an operator whether a config edit was actually picked
/// up.
#[test]
#[serial]
fn test_resolve_max_concurrent_with_source_precedence() {
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_max_concurrent_with_source(&WorkFinderConfig::default()),
        (DEFAULT_WORK_FINDER_MAX_CONCURRENT, ConfigSource::Default)
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        max_concurrent: Some(8),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (8, ConfigSource::Config));

    // Env overrides config.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (2, ConfigSource::Env));

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (8, ConfigSource::Config));
    std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
    assert_eq!(resolve_max_concurrent_with_source(&cfg), (8, ConfigSource::Config));
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

    // `.0` of the source-aware resolver must always match the plain
    // resolver — they share the same underlying precedence.
    assert_eq!(
        resolve_max_concurrent_with_source(&cfg).0,
        resolve_max_concurrent_with_config(&cfg)
    );
}

// ===================================================================
// resolve_max_admissions_per_tick_with_config — env > config > default
// (#4234)
// ===================================================================

#[test]
#[serial]
fn test_resolve_max_admissions_per_tick_with_config_precedence() {
    std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);

    // Default when neither env nor config set.
    assert_eq!(
        resolve_max_admissions_per_tick_with_config(&WorkFinderConfig::default()),
        DEFAULT_MAX_ADMISSIONS_PER_TICK
    );

    // Config used when env unset.
    let cfg = WorkFinderConfig {
        max_admissions_per_tick: Some(7),
        ..Default::default()
    };
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);

    // Env overrides config.
    std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "1");
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 1);

    // A zero/garbage env value is ignored; config still wins over default.
    std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "0");
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);
    std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "nope");
    assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);
    std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);
}

#[test]
fn test_read_work_finder_config_parses_max_admissions_per_tick() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxAdmissionsPerTick": 5}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.max_admissions_per_tick, Some(5));
}

#[test]
fn test_read_work_finder_config_drops_zero_max_admissions_per_tick() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxAdmissionsPerTick": 0}}}"#);
    let cfg = read_work_finder_config(tmp.path());
    assert_eq!(cfg.max_admissions_per_tick, None, "a zero cap is treated as absent");
}

#[test]
#[serial]
fn test_retired_per_token_concurrency_env_var_is_silently_ignored() {
    // #5743: `LOOM_PER_TOKEN_CONCURRENCY` no longer resolves to anything —
    // no function reads it any more. Setting it must not be a startup
    // error and must not affect the dynamic cap, which has had no token
    // term since #5270.
    //
    // #7455: a live `loom-daemon` on the host running this test can leak
    // `LOOM_WORK_FINDER_MAX_CONCURRENT` into the ambient env, which would
    // silently override the `Some(10)` set below (env > config >
    // default) and produce a spurious mismatch unrelated to the retired
    // var under test — clear it first so the test stays hermetic.
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    std::env::set_var("LOOM_PER_TOKEN_CONCURRENCY", "7");
    let cfg = WorkFinderConfig {
        max_concurrent: Some(10),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 10);
    assert_eq!(
        resolve_dynamic_max_concurrent(36, usize::MAX, 10),
        10,
        "a retired env var must not clamp the cap"
    );
    std::env::remove_var("LOOM_PER_TOKEN_CONCURRENCY");
}

#[test]
#[serial]
fn test_retired_cpu_env_vars_are_ignored_not_honored() {
    // #4512: `LOOM_CPU_UTILIZATION_TARGET` / `LOOM_EST_CORES_PER_SWEEP` no
    // longer resolve to anything (the functions that read them are gone).
    // The observable contract is that setting them changes NO cap input and
    // is not an error — only the deprecation warning notices them.
    //
    // #7455: clear `LOOM_WORK_FINDER_MAX_CONCURRENT` first — a live
    // `loom-daemon` on the host running this test can leak it into the
    // ambient env, which would silently override the `Some(10)` set
    // below (env > config > default) and produce a spurious mismatch
    // unrelated to the retired CPU vars under test.
    std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::set_var(var, "0.01");
    }
    let cfg = WorkFinderConfig {
        max_concurrent: Some(10),
        ..Default::default()
    };
    assert_eq!(resolve_max_concurrent_with_config(&cfg), 10);
    assert_eq!(
        resolve_dynamic_max_concurrent(36, usize::MAX, 10),
        10,
        "a retired env knob must not clamp the cap"
    );
    // Safe to call with only env-side deprecation present (no config keys).
    warn_deprecated_cpu_knobs(&cfg);
    for var in DEPRECATED_CPU_ENV_VARS {
        std::env::remove_var(var);
    }
}

// ===================================================================
// Token-capacity advisory transitions (#3902)
// ===================================================================

fn pressured_assessment() -> capacity::PressureAssessment {
    // token_limit 0 (zero healthy accounts); 12 deferred ⇒ genuinely
    // token-starved + pressured.
    let snap = capacity::RankingSnapshot {
        total: 7,
        available: 0,
        exhausted: 7,
        ..capacity::RankingSnapshot::default()
    };
    capacity::assess_pressure(Some(&snap), 7, 0, 12, capacity::DEFAULT_ADVISORY_MIN_QUEUED)
}

fn calm_assessment() -> capacity::PressureAssessment {
    // Nothing deferred ⇒ not pressured (healthy pool).
    let snap = capacity::RankingSnapshot {
        total: 7,
        available: 7,
        ..capacity::RankingSnapshot::default()
    };
    capacity::assess_pressure(Some(&snap), 7, 7, 0, capacity::DEFAULT_ADVISORY_MIN_QUEUED)
}

#[test]
fn transition_enters_pressure_and_publishes_advisory() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let a = pressured_assessment();
    assert!(a.pressured);

    // Not previously pressured ⇒ transition fires, returns true.
    let now = emit_capacity_transition(&bus, false, &a);
    assert!(now, "entered pressured state");

    match sub.try_recv().expect("an advisory event was published") {
        Event::CapacityAdvisory {
            pressured,
            queued,
            healthy_accounts,
            message,
            ..
        } => {
            assert!(pressured);
            assert_eq!(queued, 12);
            assert_eq!(healthy_accounts, 0);
            assert!(message.contains("loom-daemon tokens bootstrap"));
        }
        other => panic!("expected CapacityAdvisory, got {other:?}"),
    }
}

#[test]
fn transition_is_deduplicated_while_pressure_persists() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let a = pressured_assessment();

    // Already pressured ⇒ no new event, state stays true.
    let now = emit_capacity_transition(&bus, true, &a);
    assert!(now);
    assert!(
        matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
        "no duplicate advisory while pressure persists"
    );
}

#[test]
fn transition_recovers_and_publishes_symmetric_event() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let calm = calm_assessment();

    // Was pressured, now calm ⇒ recovery event, state returns to false.
    let now = emit_capacity_transition(&bus, true, &calm);
    assert!(!now, "left pressured state");

    match sub.try_recv().expect("a recovery event was published") {
        Event::CapacityAdvisory {
            pressured, message, ..
        } => {
            assert!(!pressured);
            assert!(message.contains("restored"));
        }
        other => panic!("expected CapacityAdvisory recovery, got {other:?}"),
    }
}

#[test]
fn transition_stays_calm_when_never_pressured() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
    let calm = calm_assessment();

    let now = emit_capacity_transition(&bus, false, &calm);
    assert!(!now);
    assert!(
        matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
        "no event when staying calm"
    );
}

#[test]
fn capacity_advisory_event_topic() {
    let ev = Event::CapacityAdvisory {
        pressured: true,
        queued: 3,
        healthy_accounts: 1,
        exhausted_accounts: 6,
        total_accounts: 7,
        estimated_drain_minutes: Some(90),
        message: "x".to_string(),
    };
    assert_eq!(ev.topic(), "daemon.capacity.advisory");
}

// ===================================================================
// Gate-in-flight dispatch suppressor (#4084)
// ===================================================================

#[test]
fn test_dispatch_held_per_root_gate_in_flight_holds_only_its_own_root() {
    // A root whose gate run is in flight is held; a sibling with no gate in
    // flight is NOT — the #3930 per-repo isolation contract must survive.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let held = dispatch_held_per_root(&states, &[root_a, root_b], true);
    assert_eq!(held, vec![true, false], "only the in-flight root is held");
}

#[test]
fn test_dispatch_held_per_root_suppressor_disabled_is_is_halted_only() {
    // With the suppressor off, the in-flight term drops out entirely — the
    // result is byte-for-byte the pre-#4084 `is_halted`-only vector, even
    // for a root with a gate run in flight.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    states.get_or_create(&root_b).set_halted(true);
    let held = dispatch_held_per_root(&states, &[root_a.clone(), root_b.clone()], false);
    assert_eq!(
        held,
        vec![false, true],
        "suppressor off ⇒ gate-in-flight is ignored; only verified-red holds"
    );
    // Sanity: with the suppressor on, root_a is additionally held.
    let held_on = dispatch_held_per_root(&states, &[root_a, root_b], true);
    assert_eq!(held_on, vec![true, true]);
}

#[test]
fn test_dispatch_held_per_root_with_drain_holds_every_root() {
    // A daemon-global scheduled drain (#4090) holds EVERY root at once,
    // regardless of each root's gate state — the merge with #4084 must not
    // let the per-root gate term shadow the global drain term.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    // Neither root is verified-red nor has a gate in flight.
    let held = dispatch_held_per_root_with_drain(
        &states,
        &[root_a, root_b],
        true, // suppressor on
        true, // draining
    );
    assert_eq!(
        held,
        vec![true, true],
        "a scheduled drain holds every root regardless of gate state"
    );
}

#[test]
fn test_dispatch_held_per_root_with_drain_gate_still_per_root_when_not_draining() {
    // With no drain in progress the gate-in-flight term stays strictly
    // per-root: only the root whose gate run is in flight is held, its
    // sibling keeps dispatching (#3930 isolation contract survives #4090).
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let held = dispatch_held_per_root_with_drain(
        &states,
        &[root_a, root_b],
        true,  // suppressor on
        false, // not draining
    );
    assert_eq!(
        held,
        vec![true, false],
        "gate-in-flight holds only its own root when not draining"
    );
}

#[test]
fn test_dispatch_held_per_root_with_drain_terms_are_independent() {
    // Both terms compose additively: a drain holds a healthy root, and a
    // gate in flight holds its own root — with the drain on, every root is
    // held whether or not its gate is in flight.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a"); // gate in flight
    let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
    states.get_or_create(&root_a).set_gate_in_flight(true);
    let held = dispatch_held_per_root_with_drain(
        &states,
        &[root_a, root_b],
        true, // suppressor on
        true, // draining
    );
    assert_eq!(held, vec![true, true], "drain OR gate-in-flight: both roots held");
}

#[test]
fn test_dispatch_held_per_root_with_drain_no_drain_matches_per_root() {
    // With `draining = false` the result is byte-for-byte the plain
    // per-root vector — the drain fold is a pure superset, never a
    // regression of the #4084 / #3930 semantics.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    states.get_or_create(&root_b).set_halted(true);
    let roots = [root_a, root_b];
    let plain = dispatch_held_per_root(&states, &roots, true);
    let with_drain = dispatch_held_per_root_with_drain(&states, &roots, true, false);
    assert_eq!(plain, with_drain, "draining=false ⇒ identical to dispatch_held_per_root");
}

/// The admission side of a roll (#6007's test, restated for #10831). The work
/// finder reads exactly one bit (`DrainState::flag`, surfaced here as
/// `draining`). A pause roll holds it for the whole pause, so no new sweep is
/// admitted while agents are being stopped; if the roll ends without a restart
/// (superseded or aborted before it stopped anything) admission resumes, so a
/// roll can never hold dispatch indefinitely.
#[test]
fn test_admission_stays_held_for_a_pause_roll_and_resumes_when_it_ends_without_a_restart() {
    use crate::ipc::{DrainOrigin, DrainState};

    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    let roots = [root_a, root_b];

    let drain = DrainState::new();
    let _ =
        drain.begin_as(std::time::Duration::from_secs(120), false, false, DrainOrigin::PauseRoll);
    assert_eq!(
        dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
        vec![true, true],
        "a pause roll holds every root"
    );

    assert!(drain.abort_pause_roll());
    assert_eq!(
        dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
        vec![false, false],
        "a roll that ended without a restart returns the admission window to the work finder"
    );
}

// ===================================================================
// Pre-flight-advisory dispatch hold (#5030)
// ===================================================================

#[test]
fn test_dispatch_held_per_root_with_preflight_holds_only_the_tripped_root() {
    // A workspace whose pre-flight breaker is holding (broken .mcp.json)
    // holds only its own root; a healthy sibling keeps dispatching — the
    // #3930 per-repo isolation contract must survive the #5030 fold.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a"); // pre-flight held
    let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
    let preflight_held = [true, false];
    let held =
        dispatch_held_per_root_with_preflight(&states, &[root_a, root_b], true, &preflight_held);
    assert_eq!(
        held,
        vec![true, false],
        "only the tripped root is held; its healthy sibling keeps dispatching"
    );
}

#[test]
fn test_dispatch_held_per_root_with_preflight_empty_slice_matches_per_root() {
    // An all-false / missing pre-flight slice is byte-for-byte the plain
    // per-root vector — the fold is a pure superset, never a regression of
    // the #4084 / #3930 semantics.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    states.get_or_create(&root_a).set_gate_in_flight(true);
    states.get_or_create(&root_b).set_halted(true);
    let roots = [root_a, root_b];
    let plain = dispatch_held_per_root(&states, &roots, true);
    let folded = dispatch_held_per_root_with_preflight(&states, &roots, true, &[]);
    assert_eq!(plain, folded, "empty pre-flight slice ⇒ identical to dispatch_held_per_root");
}

#[test]
fn test_dispatch_held_per_root_with_preflight_composes_with_verified_red() {
    // The pre-flight hold and the #3930 verified-red hold compose
    // additively per root: root_a is held by pre-flight, root_b by a red
    // main, root_c is healthy on both axes.
    let states = WorkspaceHealthStates::new();
    let root_a = std::path::PathBuf::from("/tmp/repo-a");
    let root_b = std::path::PathBuf::from("/tmp/repo-b");
    let root_c = std::path::PathBuf::from("/tmp/repo-c");
    states.get_or_create(&root_b).set_halted(true);
    let held = dispatch_held_per_root_with_preflight(
        &states,
        &[root_a, root_b, root_c],
        true,
        &[true, false, false],
    );
    assert_eq!(held, vec![true, true, false]);
}

// ===================================================================
// Last-tick publication (#4761)
// ===================================================================

#[test]
#[serial(work_finder_last_tick)]
fn last_tick_summary_is_none_before_any_tick() {
    reset_last_tick_summary();
    assert!(last_tick_summary().is_none());
}

#[test]
#[serial(work_finder_last_tick)]
fn publishing_a_tick_makes_its_counters_readable_cross_process() {
    reset_last_tick_summary();
    let at = chrono::Utc::now();
    let report = TickReport {
        seen: 12,
        dispatched: 2,
        skipped_in_flight: 9,
        skipped_pr_open: 1,
        errors: 0,
        halted: false,
        ..TickReport::default()
    };
    publish_tick_summary_at(&report, 7, at);

    let summary = last_tick_summary().expect("a published tick must be readable");
    assert_eq!(summary.at, at);
    assert_eq!(summary.max_concurrent, 7);
    assert_eq!(summary.seen, 12);
    assert_eq!(summary.dispatched, 2);
    assert_eq!(summary.skipped_in_flight, 9);
    assert_eq!(summary.skipped_pr_open, 1);
    assert!(!summary.halted);
}

#[test]
#[serial(work_finder_last_tick)]
fn publishing_replaces_the_previous_tick() {
    reset_last_tick_summary();
    publish_tick_summary(
        &TickReport {
            seen: 1,
            ..TickReport::default()
        },
        3,
    );
    publish_tick_summary(
        &TickReport {
            seen: 99,
            ..TickReport::default()
        },
        4,
    );
    let summary = last_tick_summary().unwrap();
    assert_eq!(summary.seen, 99);
    assert_eq!(summary.max_concurrent, 4);
}

/// The rendered summary must show only the *non-zero* skip reasons, so an
/// operator reading one line is not scanning a wall of zeros.
#[test]
fn reason_summary_omits_zero_terms() {
    let summary = crate::types::WorkFinderTickSummary {
        seen: 12,
        dispatched: 2,
        skipped_in_flight: 10,
        ..Default::default()
    };
    assert_eq!(summary.reason_summary(), "12 seen, 2 dispatched, 10 in-flight-skip");
}

#[test]
fn reason_summary_flags_a_halted_tick() {
    let summary = crate::types::WorkFinderTickSummary {
        seen: 5,
        halted: true,
        ..Default::default()
    };
    assert!(summary.reason_summary().ends_with("HALTED"));
}

/// Issue #5302: `TickReport::collisions` was already logged on the
/// per-tick `work_finder: tick — …` line (#4085) but never reached the
/// wire-carried [`crate::types::WorkFinderTickSummary`], so
/// `loom-daemon status` / `GetDaemonStatus` could not see a cross-host
/// collision without scraping the daemon log. Assert the count now
/// survives publication.
#[test]
fn publish_tick_summary_carries_collisions_through() {
    reset_last_tick_summary();
    publish_tick_summary(
        &TickReport {
            seen: 4,
            dispatched: 1,
            collisions: 3,
            ..TickReport::default()
        },
        2,
    );
    let summary = last_tick_summary().unwrap();
    assert_eq!(summary.collisions, 3, "collision total must survive publication");
    assert!(
        summary
            .reason_summary()
            .contains("3 cross-host-collision(s)"),
        "reason_summary must surface a non-zero collision count: {}",
        summary.reason_summary()
    );
}

/// A clean tick (no collisions) must not mention collisions at all — the
/// same "only non-zero terms" discipline every other counter follows.
#[test]
fn reason_summary_omits_zero_collisions() {
    let summary = crate::types::WorkFinderTickSummary {
        seen: 1,
        dispatched: 1,
        ..Default::default()
    };
    assert!(!summary.reason_summary().contains("collision"));
}
