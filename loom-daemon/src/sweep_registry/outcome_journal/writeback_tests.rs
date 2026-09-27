//! End-to-end tests for the #9056 issue write-back: posting on `Success`,
//! staying silent on every other result, respecting the opt-in flag, and —
//! the one defensive property this journal's other callers do not otherwise
//! need — never double-posting if a terminal transition is somehow observed
//! twice.
//!
//! Mirrors [`super::complexity_tests`]'s shape: a fake `gh` on `PATH` answers
//! the issue-body fetch (shared by both the complexity and points markers),
//! the `/comments` idempotency read, and the `issue comment` post itself,
//! logging every invocation to a file this module inspects. The pure
//! formatter/config-resolution logic is unit-tested inline in
//! [`super::writeback`] instead; what is tested HERE is the
//! record-construction + gating path in
//! [`SweepRegistry::append_outcome_telemetry_journal`].

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// A registry whose `gh` is `script` and whose forge probes are NOT skipped.
/// Journals are confined to `ws`. Identical shape to `complexity_tests`'
/// `complexity_registry`, duplicated here rather than shared so this sibling
/// test file stays self-contained.
fn writeback_registry(ws: &Path, script: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-writeback.sh");
    std::fs::write(&fake_gh, script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    config.outcomes_journal_path = Some(ws.join("test-sweep-outcomes.jsonl"));
    config.outcome_telemetry_path = Some(ws.join("test-sweep-outcome-telemetry.jsonl"));
    SweepRegistry::new(config)
}

/// Enable the write-back for `ws` via committed config, mirroring how a real
/// workspace opts in (`autonomous.sweepOutcomeWriteback.enabled`).
fn enable_writeback(ws: &Path) {
    std::fs::create_dir_all(ws.join(".loom")).unwrap();
    std::fs::write(
        ws.join(".loom/config.json"),
        r#"{"autonomous":{"sweepOutcomeWriteback":{"enabled":true}}}"#,
    )
    .unwrap();
}

/// A fake `gh` covering every call this module makes:
///   - `gh api repos/.../issues/<n>/comments --paginate --jq ...` (idempotency
///     check) — non-empty output (`already-existing`) when `POSTED_MARKER`
///     exists on disk, empty otherwise. Checked FIRST since its argv also
///     matches the plainer issue-body pattern below.
///   - `gh api repos/.../issues/<n> --jq .body` (the complexity AND points
///     fetches — same endpoint, same response) — echoes `body`.
///   - `gh issue comment <n> --body <text>` — appends one line to `gh_log`
///     and touches `posted_marker`, so a SECOND idempotency check in the same
///     test run observes a real prior post.
fn fake_gh_script(body: &str, gh_log: &Path, posted_marker: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"issue\" && \"$2\" == \"comment\" ]]; then\n\
         printf 'issue comment %s\\n' \"$3\" >> \"{gh_log}\"\n\
         touch \"{posted_marker}\"\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/*/comments ]]; then\n\
         if [[ -f \"{posted_marker}\" ]]; then\n\
         printf '111\\n'\n\
         fi\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* ]]; then\n\
         printf '%s' '{body}'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        gh_log = gh_log.display(),
        posted_marker = posted_marker.display(),
        body = body.replace('\'', "'\\''"),
    )
}

fn count_comment_posts(gh_log: &Path) -> usize {
    std::fs::read_to_string(gh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("issue comment "))
        .count()
}

#[test]
#[serial]
fn success_with_writeback_enabled_posts_one_marked_comment() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let body = "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";
    let mut registry = writeback_registry(ws, &fake_gh_script(body, &gh_log, &posted_marker));
    let issue = 90561;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-1", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        4000,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    assert_eq!(count_comment_posts(&gh_log), 1, "exactly one comment posted");
    let log_contents = std::fs::read_to_string(&gh_log).unwrap();
    assert!(
        log_contents.contains(&issue.to_string()),
        "the comment targets the right issue: {log_contents}"
    );
}

#[test]
#[serial]
fn writeback_disabled_by_default_never_posts() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    // No `.loom/config.json` at all -- default false, matching a workspace
    // that has never opted in.
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let body = "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";
    let mut registry = writeback_registry(ws, &fake_gh_script(body, &gh_log, &posted_marker));
    let issue = 90562;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-2", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        4000,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    assert_eq!(
        count_comment_posts(&gh_log),
        0,
        "the flag defaults off -- no comment, no forge write"
    );
}

#[test]
#[serial]
fn failure_result_never_posts_even_when_enabled() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let body = "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";
    let mut registry = writeback_registry(ws, &fake_gh_script(body, &gh_log, &posted_marker));
    let issue = 90563;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-3", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        60,
        telemetry::SweepResult::Failure,
        Some("preflight-token-selection-failed".to_string()),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    assert_eq!(
        count_comment_posts(&gh_log),
        0,
        "opt-in write-back never fires on a non-Success terminal result"
    );
}

#[test]
#[serial]
fn a_prior_comment_prevents_a_duplicate_post() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    // Pre-seed the marker file, simulating a write-back that already landed
    // from an earlier terminal observation (or a prior daemon run).
    std::fs::write(&posted_marker, "already posted").unwrap();
    let body = "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";
    let mut registry = writeback_registry(ws, &fake_gh_script(body, &gh_log, &posted_marker));
    let issue = 90564;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-4", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        4000,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    assert_eq!(
        count_comment_posts(&gh_log),
        0,
        "an existing write-back comment must suppress a second post"
    );
}

/// AC: the write-back must be safe to call from a terminal transition that is
/// somehow observed twice — the defensive property `append_outcome_journal`'s
/// own contract does not otherwise require of its callers, since it normally
/// fires exactly once per terminal transition.
#[test]
#[serial]
fn a_terminal_transition_observed_twice_posts_only_once() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let body = "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";
    let mut registry = writeback_registry(ws, &fake_gh_script(body, &gh_log, &posted_marker));
    let issue = 90565;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-5", "log\n");

    // First observation: no prior comment, so this one posts and the fake
    // `gh` records the marker file.
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        4000,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
    // Second observation of the SAME terminal transition (e.g. a bug in a
    // caller, or a defensive re-invocation) must find the marker and skip.
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        4000,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    assert_eq!(
        count_comment_posts(&gh_log),
        1,
        "a repeated terminal observation must not double-post"
    );
}
