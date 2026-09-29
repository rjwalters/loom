//! Dispatch-time `loom.story_points` resolution tests (Issue #9432, epic
//! #9429). Sibling file rather than appended to `tests.rs`: that module is
//! over the 1000-line ratchet threshold and frozen at its current size
//! (`.loom/docs/file-size-policy.md`), so this one hangs off
//! `prompt_shape_tests`'s foot — the same `#[path]` chain shape `guards.rs`
//! uses for `guards_union_tests.rs`.
//!
//! These pin the wiring the pure guard
//! (`crate::story_points::story_points_from_labels`, unit-tested in its own
//! module) hangs off: the issue's label snapshot is read by the SAME single
//! REST call the #4444 park guard already makes (no second fetch), the
//! resolved estimate is retained on the registry keyed by sweep id for the
//! durable `sweep.outcome` record, it rides the `sweep.global.dispatch`
//! event for `sweep.started`, and every guard-declined shape — multiple
//! labels above all — is **surfaced loudly**, never silently resolved.
//!
//! The fake-`gh` fixture lives HERE rather than in `test_support.rs` — that
//! file is also ratchet-frozen, and this is the fixture's only consumer.

use crate::sweep_registry::{SweepKind, SweepRegistry, SweepRegistryConfig};
use crate::test_log_capture as capture;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// The #4504 state probe's post-`--jq` payload for an open, non-PR issue.
fn state_probe_json() -> &'static str {
    r#"{"state":"open","is_pr":false}"#
}

/// Install a fake `gh` fit for a clean issue dispatch whose labels probe
/// answers `labels_stdout` verbatim (newline-separated label names, exactly
/// as the real `--jq .labels[].name` renders them): the #4504 issue-state
/// probe reports an open non-PR issue, `api graphql` answers the
/// closes-graph with no open PRs, and `repo view` resolves the owner/repo.
/// Every invocation is logged to the returned path; `spawn-claude.sh` is a
/// benign echo-and-exit. Same shape as `test_support`'s
/// `open_pr_guard_registry`, distinguished only by the labels arm.
fn story_points_dispatch_registry(
    ws: &std::path::Path,
    labels_stdout: &str,
) -> (SweepRegistry, std::path::PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         if [[ \"$1\" == \"api\" && \"$2\" == \"graphql\" ]]; then\n\
         printf '{{\"data\":{{\"repository\":{{\"issue\":{{\"closedByPullRequestsReferences\":{{\"nodes\":[]}}}}}}}}}}\\n'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
         if printf '%s' \"$*\" | grep -qF '.labels[].name'; then\n\
         printf '%s\\n' '{labels}'\n\
         exit 0\n\
         fi\n\
         printf '%s\\n' '{state}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = gh_log.display(),
        labels = labels_stdout,
        state = state_probe_json(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    // The #4027 workspace-commands marker AND the minimal runtime-admission
    // surface real dispatch fixtures need (this is exactly `test_support::
    // touch_sweep_command`'s pair).
    crate::sweep_registry::test_support::touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

/// Dispatch issue #9432 through the fixture and return `(registry, sweep_id)`.
fn dispatch_with_labels(ws: &std::path::Path, labels: &str) -> (SweepRegistry, String) {
    let (mut registry, _gh_log) = story_points_dispatch_registry(ws, labels);
    let outcome = registry
        .dispatch(&SweepKind::Issue(9432), None, None, None, None)
        .expect("the fixture's clean forge state must let dispatch proceed");
    (registry, outcome.sweep_id)
}

#[test]
#[serial]
fn a_resolved_points_label_is_retained_for_the_outcome_record() {
    let dir = tempdir().unwrap();
    let (registry, sweep_id) =
        dispatch_with_labels(dir.path(), "loom:issue\nloom:curated\npoints:5\n");

    // The estimate the sweep was planned against, keyed by its sweep id —
    // this is what `append_outcome_telemetry_journal` reads at the terminal
    // transition to emit `story_points` on the durable `sweep.outcome`.
    assert_eq!(
        registry.story_points.get(&sweep_id),
        Some(&5),
        "one in-vocabulary `points:5` label must resolve to the estimate 5"
    );
}

#[test]
#[serial]
fn no_points_label_leaves_the_estimate_absent_never_zero() {
    let dir = tempdir().unwrap();
    let (registry, sweep_id) = dispatch_with_labels(dir.path(), "loom:issue\nloom:curated\n");

    assert!(
        !registry.story_points.contains_key(&sweep_id),
        "an issue with no `points:*` label must record NO estimate — the attribute \
         is absent, never a fabricated 0"
    );
}

#[test]
#[serial]
fn multiple_points_labels_are_surfaced_loudly_and_never_guessed() {
    let dir = tempdir().unwrap();
    let labels = "loom:issue\npoints:3\npoints:8\n";
    let (mut registry, _gh_log) = story_points_dispatch_registry(dir.path(), labels);
    let sweep_id = std::cell::RefCell::new(String::new());
    let records = capture::capture_logs(|| {
        let outcome = registry
            .dispatch(&SweepKind::Issue(9432), None, None, None, None)
            .expect(
                "an ambiguous estimate must not block dispatch — the guard only refuses to guess",
            );
        *sweep_id.borrow_mut() = outcome.sweep_id;
    });

    // The contract's loud half: a MULTIPLE-label conflict is WARNED with both
    // offending labels named, so an operator can fix the Curator-side defect.
    // (The guard's verdict itself — `StoryPoints::Multiple` — is unit-tested
    // in `crate::story_points`'s own module tests.)
    assert!(
        records.iter().any(|(level, msg)| {
            *level == log::Level::Warn
                && msg.contains("MULTIPLE points labels")
                && msg.contains("[points:3, points:8]")
        }),
        "the multiple-label conflict must be surfaced as a loud warning naming both \
         labels; got: {records:?}"
    );
    // And the quiet half of the same contract: never a guess — no estimate is
    // retained, so neither record kind will carry the attribute.
    assert!(
        !registry
            .story_points
            .contains_key(sweep_id.borrow().as_str()),
        "an ambiguous estimate must resolve to NO attribute, not to one of the labels"
    );
}

#[test]
#[serial]
fn an_out_of_vocabulary_points_label_is_surfaced_not_coerced() {
    let dir = tempdir().unwrap();
    let labels = "loom:issue\npoints:21\n";
    let (mut registry, _gh_log) = story_points_dispatch_registry(dir.path(), labels);
    let sweep_id = std::cell::RefCell::new(String::new());
    let records = capture::capture_logs(|| {
        let outcome = registry
            .dispatch(&SweepKind::Issue(9432), None, None, None, None)
            .expect("an invalid estimate must not block dispatch — it is surfaced, not enforced");
        *sweep_id.borrow_mut() = outcome.sweep_id;
    });

    assert!(
        records.iter().any(|(level, msg)| {
            *level == log::Level::Warn
                && msg.contains("`points:21`")
                && msg.contains("closed points vocabulary")
        }),
        "an out-of-vocabulary label must be surfaced as a loud warning; got: {records:?}"
    );
    assert!(
        !registry.story_points.contains_key(&*sweep_id.borrow()),
        "an invalid estimate must resolve to NO attribute, never a coerced value"
    );
}

#[test]
#[serial]
fn the_estimate_rides_the_dispatch_event_for_sweep_started() {
    // `sweep.started`'s only transport is the `sweep.global.dispatch` event
    // (the collector's event→record mapping is pure), so the value must be
    // observable there. Driven through a real bus subscription, mirroring how
    // the outcome-journal emit-site tests observe events end-to-end (#4863).
    use std::sync::Arc;
    let dir = tempdir().unwrap();
    let (mut registry, _gh_log) =
        story_points_dispatch_registry(dir.path(), "loom:issue\npoints:8\n");
    let bus = crate::event_bus::EventBus::new();
    let mut sub = bus.subscribe(["sweep.global"]);
    registry.set_event_bus(Arc::new(bus));

    registry
        .dispatch(&SweepKind::Issue(9432), None, None, None, None)
        .expect("dispatch must proceed");

    let mut observed = None;
    while let Ok(event) = sub.try_recv() {
        if let crate::types::Event::SweepGlobalDispatch { story_points, .. } = event {
            observed = Some(story_points);
        }
    }
    assert_eq!(
        observed,
        Some(Some(8)),
        "the resolved estimate must ride the dispatch event for `sweep.started`"
    );
}

#[test]
#[serial]
fn the_label_read_is_the_park_guards_own_not_a_second_fetch() {
    // The #9432 estimate must piggyback the #4444 park guard's existing REST
    // label read — a second fetch per dispatch is exactly the forge-pressure
    // regression the issue's "do not add a second fetch" rule exists to stop.
    let dir = tempdir().unwrap();
    let (mut registry, gh_log) =
        story_points_dispatch_registry(dir.path(), "loom:issue\npoints:5\n");
    registry
        .dispatch(&SweepKind::Issue(9432), None, None, None, None)
        .expect("dispatch must proceed");

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    let label_reads = gh_calls
        .lines()
        .filter(|call| call.contains(".labels[].name"))
        .count();
    assert_eq!(
        label_reads, 1,
        "exactly one labels REST read may happen per dispatch (the park guard's \
         own); a second fetch is the regression #9432 forbids. gh log: {gh_calls}"
    );
}
