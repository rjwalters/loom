//! Unit tests for [`super`] (`docker_image_clean`). A sibling file so the
//! parent stays under the file-size threshold (#11195).

#![allow(clippy::unwrap_used)]

use super::*;
use chrono::TimeZone;
use serial_test::serial;

fn t(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
}

fn image(id: &str, tags: &[&str], created_secs: i64, size_gb: f64) -> DockerImageRecord {
    DockerImageRecord {
        id: id.to_string(),
        repo_tags: tags.iter().map(|s| (*s).to_string()).collect(),
        created_at: t(created_secs),
        size_bytes: (size_gb * 1024.0 * 1024.0 * 1024.0) as u64,
        in_use: false,
    }
}

fn tracked() -> Vec<String> {
    DEFAULT_TRACKED_REPOS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// A [`WithBuildSlot`] seam whose slot is free: runs the removal work.
fn slot_free(work: &mut dyn FnMut()) -> Option<String> {
    work();
    None
}

/// A [`WithBuildSlot`] seam whose slot is held by someone else: defers
/// **without** running the removal work at all.
fn slot_busy(_work: &mut dyn FnMut()) -> Option<String> {
    Some("held by another build".to_string())
}

// ===================================================================
// repo_of / is_dangling
// ===================================================================

#[test]
fn repo_of_splits_on_the_last_colon() {
    assert_eq!(repo_of("loom-worker:ci-smoke"), "loom-worker");
    assert_eq!(
        repo_of("ghcr.io/rjwalters/loom-worker:ci-smoke"),
        "ghcr.io/rjwalters/loom-worker"
    );
    assert_eq!(repo_of("untagged"), "untagged");
}

#[test]
fn empty_repo_tags_is_dangling() {
    assert!(image("sha256:a", &[], 0, 1.0).is_dangling());
}

#[test]
fn none_none_repo_tag_is_dangling() {
    assert!(image("sha256:a", &["<none>:<none>"], 0, 1.0).is_dangling());
}

#[test]
fn a_real_tag_is_not_dangling() {
    assert!(!image("sha256:a", &["loom-worker:ci-smoke"], 0, 1.0).is_dangling());
}

#[test]
fn digest_only_repo_tags_are_untagged() {
    let sha = "openroad/orfs@sha256:ebc8";
    assert!(image("sha256:a", &[sha], 0, 6.5).is_dangling());
    assert!(image("sha256:a", &[sha, "<none>:<none>"], 0, 6.5).is_dangling());
    assert!(!image("sha256:a", &["openroad/orfs:latest", sha], 0, 6.5).is_dangling());
}

#[test]
fn digest_only_image_is_planned_for_removal_but_mixed_is_not() {
    let images = vec![
        image("sha256:digest", &["openroad/orfs@sha256:ebc8"], 0, 6.5),
        image("sha256:mixed", &["openroad/orfs:latest", "openroad/orfs@sha256:ebc8"], 0, 6.5),
    ];
    let plan = plan_retention(&images, &tracked(), &[], 2);
    assert_eq!(plan.remove_dangling.len(), 1);
    assert_eq!(plan.remove_dangling[0].id, "sha256:digest");
    assert_eq!(plan.kept.len(), 1);
    assert_eq!(plan.kept[0].id, "sha256:mixed");
}

#[test]
fn container_referenced_untagged_image_is_never_planned() {
    let mut img = image("sha256:a", &["postgres@sha256:abc"], 0, 1.0);
    img.in_use = true;
    let plan = plan_retention(&[img], &tracked(), &[], 2);
    assert!(plan.to_remove().is_empty());
    assert_eq!(plan.kept.len(), 1);
}

// ===================================================================
// unused-image pressure rule (#11195)
// ===================================================================

const DAY: i64 = 86_400;

fn pressure(free_gb: u64) -> PressurePolicy {
    PressurePolicy {
        free_gb,
        floor_gb: 20,
        unused_max_age_days: 7,
        now: t(30 * DAY),
    }
}

#[test]
fn below_floor_an_old_unused_third_party_image_is_planned() {
    let images = vec![image("sha256:k", &["kicad/kicad:9.0"], 0, 5.0)];
    let plan = plan_retention_with_pressure(&images, &tracked(), &[], 2, Some(&pressure(10)));
    assert_eq!(plan.remove_unused_aged.len(), 1);
    assert_eq!(plan.to_remove().len(), 1);
}

#[test]
fn above_floor_an_old_unused_third_party_image_is_kept() {
    let images = vec![image("sha256:k", &["kicad/kicad:9.0"], 0, 5.0)];
    let plan = plan_retention_with_pressure(&images, &tracked(), &[], 2, Some(&pressure(25)));
    assert!(plan.to_remove().is_empty());
    assert_eq!(plan.reclaimable_left_bytes, images[0].size_bytes);
}

#[test]
fn below_floor_a_young_unused_image_is_kept() {
    let images = vec![image("sha256:k", &["kicad/kicad:9.0"], 28 * DAY, 5.0)];
    let plan = plan_retention_with_pressure(&images, &tracked(), &[], 2, Some(&pressure(10)));
    assert!(plan.to_remove().is_empty());
}

#[test]
fn below_floor_a_container_referenced_image_is_never_planned() {
    let mut img = image("sha256:k", &["kicad/kicad:9.0"], 0, 5.0);
    img.in_use = true;
    let plan = plan_retention_with_pressure(&[img], &tracked(), &[], 2, Some(&pressure(1)));
    assert!(plan.to_remove().is_empty());
    assert_eq!(plan.reclaimable_left_bytes, 0);
}

#[test]
fn below_floor_an_allowlisted_image_is_never_planned() {
    let images = vec![image("sha256:k", &["kicad/kicad:9.0"], 0, 5.0)];
    let plan = plan_retention_with_pressure(
        &images,
        &tracked(),
        &["kicad".to_string()],
        2,
        Some(&pressure(1)),
    );
    assert!(plan.to_remove().is_empty());
    assert_eq!(plan.allowlisted.len(), 1);
}

#[test]
fn pressure_removal_goes_largest_first_and_stops_at_the_deficit() {
    // Deficit is 10 GB (free 10, floor 20): the 8 GB image alone does not
    // cover it, the 6 GB one then does, so the 1 GB one is left alone.
    let images = vec![
        image("sha256:s", &["a/small:1"], 0, 1.0),
        image("sha256:l", &["a/large:1"], 0, 8.0),
        image("sha256:m", &["a/medium:1"], 0, 6.0),
    ];
    let plan = plan_retention_with_pressure(&images, &tracked(), &[], 2, Some(&pressure(10)));
    let ids: Vec<&str> = plan
        .remove_unused_aged
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(ids, vec!["sha256:l", "sha256:m"]);
    assert_eq!(plan.kept.len(), 1);
}

#[test]
fn in_use_tracked_image_beyond_keep_n_is_kept() {
    let mut oldest = image("sha256:a", &["loom-worker:old"], 0, 1.0);
    oldest.in_use = true;
    let images = vec![oldest, image("sha256:b", &["loom-worker:new"], 10, 1.0)];
    let plan = plan_retention(&images, &tracked(), &[], 1);
    assert!(plan.remove_stale_tracked.is_empty());
    assert_eq!(plan.kept.len(), 2);
}

// ===================================================================
// plan_retention — the pure core
// ===================================================================

#[test]
fn dangling_images_are_always_planned_for_removal() {
    let images = vec![
        image("sha256:a", &[], 0, 1.0),
        image("sha256:b", &["<none>:<none>"], 0, 2.0),
    ];
    let plan = plan_retention(&images, &tracked(), &[], 2);
    assert_eq!(plan.remove_dangling.len(), 2);
    assert!(plan.remove_stale_tracked.is_empty());
    assert!(plan.kept.is_empty());
}

#[test]
fn a_tracked_repo_keeps_only_the_newest_n() {
    // 4 tagged images in loom-worker, newest-first by construction:
    // sha256:d (t=30) > sha256:c (t=20) > sha256:b (t=10) > sha256:a (t=0)
    let images = vec![
        image("sha256:a", &["loom-worker:old-1"], 0, 1.0),
        image("sha256:b", &["loom-worker:old-2"], 10, 1.0),
        image("sha256:c", &["loom-worker:ci-smoke-prev"], 20, 1.0),
        image("sha256:d", &["loom-worker:ci-smoke"], 30, 1.0),
    ];
    let plan = plan_retention(&images, &tracked(), &[], 2);
    let kept_ids: Vec<&str> = plan.kept.iter().map(|i| i.id.as_str()).collect();
    let removed_ids: Vec<&str> = plan
        .remove_stale_tracked
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(kept_ids, vec!["sha256:d", "sha256:c"], "newest 2 survive");
    assert_eq!(
        removed_ids,
        vec!["sha256:b", "sha256:a"],
        "the older 2 are removed, newest-first"
    );
}

#[test]
fn fewer_than_n_tracked_images_are_all_kept() {
    let images = vec![image("sha256:a", &["loom-worker:ci-smoke"], 0, 1.0)];
    let plan = plan_retention(&images, &tracked(), &[], 2);
    assert_eq!(plan.kept.len(), 1);
    assert!(plan.remove_stale_tracked.is_empty());
}

#[test]
fn multi_tag_aliasing_is_one_unit_not_double_counted() {
    // The exact scenario from the issue: a local tag and a ghcr.io mirror
    // of the identical digest — one DockerImageRecord, one decision.
    let images = vec![
        image(
            "sha256:newest",
            &[
                "loom-worker:ci-smoke",
                "ghcr.io/rjwalters/loom-worker:ci-smoke",
            ],
            20,
            1.0,
        ),
        image("sha256:older", &["loom-worker:ci-smoke-old"], 0, 1.0),
    ];
    let plan = plan_retention(&images, &tracked(), &[], 1);
    assert_eq!(plan.kept.len(), 1);
    assert_eq!(plan.kept[0].repo_tags.len(), 2, "both aliases travel together");
    assert_eq!(plan.remove_stale_tracked.len(), 1);
    assert_eq!(plan.remove_stale_tracked[0].id, "sha256:older");
}

#[test]
fn two_tracked_repos_are_retained_independently() {
    // loom-worker and loom-worker-session each get their own keepLastN=1
    // budget rather than sharing one global budget.
    let images = vec![
        image("sha256:w1", &["loom-worker:ci-smoke"], 20, 1.0),
        image("sha256:w0", &["loom-worker:old"], 0, 1.0),
        image("sha256:s1", &["loom-worker-session:ci-smoke"], 20, 1.0),
        image("sha256:s0", &["loom-worker-session:old"], 0, 1.0),
    ];
    let plan = plan_retention(&images, &tracked(), &[], 1);
    let kept_ids: Vec<&str> = plan.kept.iter().map(|i| i.id.as_str()).collect();
    assert!(kept_ids.contains(&"sha256:w1"));
    assert!(kept_ids.contains(&"sha256:s1"));
    assert_eq!(plan.remove_stale_tracked.len(), 2);
}

#[test]
fn allowlisted_long_lived_base_image_is_never_swept() {
    let images = vec![image(
        "sha256:eda",
        &["shared/eda-toolchain:2026.1"],
        0,
        6.5,
    )];
    let plan = plan_retention(&images, &tracked(), &["eda".to_string()], 1);
    assert_eq!(plan.allowlisted.len(), 1);
    assert!(plan.remove_dangling.is_empty());
    assert!(plan.remove_stale_tracked.is_empty());
}

#[test]
fn allowlisted_image_survives_even_when_it_would_be_stale_tracked() {
    let images = vec![
        image("sha256:new", &["loom-worker:ci-smoke"], 20, 1.0),
        image("sha256:shared", &["loom-worker:shared-base"], 0, 6.5),
    ];
    // Without the allowlist, keepLastN=1 would remove sha256:shared.
    let plan = plan_retention(&images, &tracked(), &["shared-base".to_string()], 1);
    assert_eq!(plan.allowlisted.len(), 1);
    assert_eq!(plan.allowlisted[0].id, "sha256:shared");
    assert!(plan.remove_stale_tracked.is_empty());
}

#[test]
fn untracked_non_dangling_images_are_never_touched() {
    let images = vec![image("sha256:other", &["ubuntu:22.04"], 0, 0.1)];
    let plan = plan_retention(&images, &tracked(), &[], 2);
    assert_eq!(plan.kept.len(), 1);
    assert!(plan.remove_dangling.is_empty());
    assert!(plan.remove_stale_tracked.is_empty());
}

#[test]
fn plan_summary_reports_counts_and_size() {
    let images = vec![
        image("sha256:a", &[], 0, 2.0),
        image("sha256:b", &["loom-worker:old"], 0, 1.0),
        image("sha256:c", &["loom-worker:ci-smoke"], 20, 1.0),
    ];
    let plan = plan_retention(&images, &tracked(), &[], 1);
    assert!(plan.summary().contains("1 dangling"));
    assert!(plan.summary().contains("1 stale tracked"));
}

#[test]
fn empty_plan_summarizes_as_nothing() {
    assert_eq!(RetentionPlan::default().summary(), "nothing");
}

// ===================================================================
// run_pass — the injected I/O shell
// ===================================================================

#[test]
fn disabled_never_lists_or_removes() {
    let inputs = DockerRetentionInputs {
        enabled: false,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let listed = std::sync::atomic::AtomicBool::new(false);
    let report = run_pass(
        &inputs,
        &|| {
            listed.store(true, std::sync::atomic::Ordering::SeqCst);
            Some(Vec::new())
        },
        &|_| panic!("must never remove while disabled"),
        &slot_free,
    );
    assert!(!listed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(report.removed.is_empty());
    assert_eq!(report.plan, None);
}

#[test]
fn docker_unavailable_removes_nothing_and_is_distinguishable_from_empty() {
    let inputs = DockerRetentionInputs {
        enabled: true,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let report = run_pass(&inputs, &|| None, &|_| panic!("must never remove"), &slot_free);
    assert!(report.plan.is_none());
    assert!(report.reason().contains("unqueryable"));
}

#[test]
fn a_build_in_progress_holding_the_slot_defers_removal_entirely() {
    let images = vec![image("sha256:a", &[], 0, 1.0)];
    let inputs = DockerRetentionInputs {
        enabled: true,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let report = run_pass(
        &inputs,
        &move || Some(images.clone()),
        &|_| panic!("must never remove while the build slot is held elsewhere"),
        &slot_busy,
    );
    assert!(report.removed.is_empty());
    assert_eq!(report.deferred.as_deref(), Some("held by another build"));
    // The plan was still computed (for observability) even though
    // nothing was actually removed.
    assert!(report.plan.is_some());
}

#[test]
#[serial]
fn every_removal_runs_while_the_build_slot_is_still_held() {
    // Regression test for the PR #7334 review finding: the seam used to be
    // a stateless `-> Option<String>` probe, so the `BuildSlotLease` it
    // took to answer "is a build running?" was dropped when the probe
    // returned — *before* `run_pass` called `remover` even once. The slot
    // was therefore provably free during the very `docker rmi` calls it
    // exists to protect. Passing the removal work *into* the seam makes
    // that shape inexpressible; this pins the guarantee.
    let images = vec![
        image("sha256:a", &[], 0, 1.0),
        image("sha256:b", &[], 0, 1.0),
    ];
    let inputs = DockerRetentionInputs {
        enabled: true,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let slot_held = std::cell::Cell::new(false);
    let removals_while_held = std::cell::Cell::new(0usize);
    let report = run_pass(
        &inputs,
        &move || Some(images.clone()),
        &|_| {
            assert!(slot_held.get(), "a docker rmi ran outside the machine build slot");
            removals_while_held.set(removals_while_held.get() + 1);
            true
        },
        &|work: &mut dyn FnMut()| {
            slot_held.set(true);
            work();
            slot_held.set(false);
            None
        },
    );
    assert_eq!(report.removed.len(), 2);
    assert_eq!(removals_while_held.get(), 2, "both removals must run inside the slot");
    assert!(!slot_held.get(), "the slot is released only after removal finishes");
}

#[test]
#[serial]
fn the_production_seam_holds_a_real_slot_across_the_work_and_releases_after() {
    // The unit tests above inject a mock seam, so they can only pin
    // `run_pass`'s side of the contract. This exercises the *production*
    // seam against a real slot directory — the half that actually
    // regressed in PR #7334 — by asking whether a competing acquirer (a
    // `docker build` gate on this host) can steal the only slot mid-work.
    // #11014: a per-test slot dir, restored (not unset) when the guard drops.
    let slots = crate::build_slot::test_support::BuildSlotEnvGuard::isolated();
    let dir = slots.dir().to_path_buf();
    std::env::set_var(crate::build_slot::BUILD_SLOTS_ENV, "1");

    let competitor = |label: &str| {
        crate::build_slot::acquire_in(
            &dir,
            1,
            Duration::from_millis(0), // never wait: we want a snapshot, not a queue
            Duration::from_millis(10),
            Duration::from_secs(3_600),
            label,
        )
    };

    let mut competitor_won_mid_work = None;
    let deferred = production_with_build_slot(&mut || {
        competitor_won_mid_work = Some(competitor("competing-docker-build").holds_slot());
    });

    assert_eq!(deferred, None, "the slot was free, so the pass must not defer");
    assert_eq!(
        competitor_won_mid_work,
        Some(false),
        "the build slot must still be held while the removal work runs — an early \
         drop(lease) (the PR #7334 review finding) lets a build start mid-removal"
    );
    assert!(
        competitor("after-the-pass").holds_slot(),
        "the lease must be released once the removal work has completed"
    );
}

#[test]
fn nothing_to_remove_never_takes_the_build_slot() {
    let images = vec![image("sha256:a", &["loom-worker:ci-smoke"], 0, 1.0)];
    let inputs = DockerRetentionInputs {
        enabled: true,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let took_slot = std::sync::atomic::AtomicBool::new(false);
    let report = run_pass(
        &inputs,
        &move || Some(images.clone()),
        &|_| panic!("nothing planned for removal"),
        &|work: &mut dyn FnMut()| {
            took_slot.store(true, std::sync::atomic::Ordering::SeqCst);
            work();
            None
        },
    );
    assert!(!took_slot.load(std::sync::atomic::Ordering::SeqCst));
    assert!(report.removed.is_empty());
}

#[test]
fn removal_failures_are_soft_and_excluded_from_the_removed_list() {
    let images = vec![
        image("sha256:a", &[], 0, 1.0),
        image("sha256:b", &[], 0, 1.0),
    ];
    let inputs = DockerRetentionInputs {
        enabled: true,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let report = run_pass(
        &inputs,
        &move || Some(images.clone()),
        &|id| id == "sha256:a", // sha256:b "fails" (e.g. backs a running container)
        &slot_free,
    );
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].id, "sha256:a");
    assert!(report.deferred.is_none());
}

#[test]
fn reason_mentions_removed_kept_and_allowlisted_counts() {
    let images = vec![
        image("sha256:a", &[], 0, 1.0),
        image("sha256:b", &["loom-worker:ci-smoke"], 0, 1.0),
    ];
    let inputs = DockerRetentionInputs {
        enabled: true,
        tracked_repos: &tracked(),
        allowlist: &[],
        keep_last_n: 2,
        now: t(0),
        pressure: None,
    };
    let report = run_pass(&inputs, &move || Some(images.clone()), &|_| true, &slot_free);
    assert!(report.reason().contains("removed 1 of planned 1"));
    assert!(report.reason().contains("1 kept"));
}

// ===================================================================
// Cooldown (host-wide, not per-repo)
// ===================================================================

#[test]
#[serial]
fn cooldown_elapsed_is_true_before_any_evaluation() {
    reset_state_for_test();
    assert!(cooldown_elapsed(t(0), 1_800));
    reset_state_for_test();
}

#[test]
#[serial]
fn a_recent_evaluation_holds_the_cooldown() {
    reset_state_for_test();
    record_evaluated(t(0));
    assert!(!cooldown_elapsed(t(60), 1_800));
    reset_state_for_test();
}

#[test]
#[serial]
fn the_cooldown_expires() {
    reset_state_for_test();
    record_evaluated(t(0));
    assert!(cooldown_elapsed(t(1_801), 1_800));
    reset_state_for_test();
}

// ===================================================================
// Config resolution defaults
// ===================================================================

#[test]
fn default_config_resolves_to_documented_defaults() {
    let config = DockerRetentionConfig::default();
    assert!(resolve_enabled(&config));
    assert_eq!(resolve_keep_last_n(&config), DEFAULT_KEEP_LAST_N);
    assert_eq!(resolve_min_interval_secs(&config), DEFAULT_MIN_INTERVAL_SECS);
    assert_eq!(resolve_tracked_repos(&config), tracked());
    assert!(resolve_allowlist(&config).is_empty());
    assert_eq!(resolve_unused_max_age_days(&config), DEFAULT_UNUSED_MAX_AGE_DAYS);
}

#[test]
fn config_values_override_defaults() {
    let config = DockerRetentionConfig {
        enabled: Some(false),
        keep_last_n: Some(5),
        min_interval_secs: Some(3_600),
        tracked_repos: Some(vec!["my-repo".to_string()]),
        allowlist: Some(vec!["shared".to_string()]),
        unused_max_age_days: Some(3),
    };
    assert!(!resolve_enabled(&config));
    assert_eq!(resolve_keep_last_n(&config), 5);
    assert_eq!(resolve_min_interval_secs(&config), 3_600);
    assert_eq!(resolve_tracked_repos(&config), vec!["my-repo".to_string()]);
    assert_eq!(resolve_allowlist(&config), vec!["shared".to_string()]);
    assert_eq!(resolve_unused_max_age_days(&config), 3);
}

#[test]
fn read_config_soft_fails_to_defaults_on_a_repo_with_no_block() {
    let tmp = tempfile::tempdir().unwrap();
    let config = read_docker_retention_config(tmp.path());
    assert_eq!(config, DockerRetentionConfig::default());
}
