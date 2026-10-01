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
///   - `gh api repos/.../issues/<n>/comments --paginate --jq <filter>`
///     (idempotency check) — prints a comment id for every marker line in
///     `posted_marker` that appears in `<filter>` (`$5`), so a hit is keyed
///     on the SWEEP id embedded in the marker, exactly like the real
///     server-side `startswith` filter; exits `comments_rc` (non-zero models
///     an unreadable check). Checked FIRST since its argv also matches the
///     plainer issue-body pattern below.
///   - `gh api repos/.../issues/<n> --jq .body` (the complexity AND points
///     fetches — same endpoint, same response) — echoes `body`.
///   - `gh api repos/.../issues/<n>/comments --method POST -f body=<text>`
///     (the post, via the #9772 comment chokepoint — this replaced
///     `gh issue comment <n> --body <text>`) — logs `issue comment <n>` plus
///     whether the `sweep.outcome` telemetry journal was ALREADY non-empty
///     at post time to `gh_log`, appends the full body to
///     `<gh_log>.bodies`, appends its first (marker) line to `posted_marker`
///     so a SECOND idempotency check observes a real prior post, then exits
///     `post_rc` (or hangs when `post_rc` is `-1`). Discriminated from the
///     idempotency READ above by `--method POST` — both share the same
///     endpoint — and matched BEFORE it for the same reason. The `gh_log`
///     line keeps its pre-#9772 `issue comment <n> <durable>` wording so
///     every assertion in this file still reads the same.
fn fake_gh_script_with(
    body: &str,
    gh_log: &Path,
    posted_marker: &Path,
    comments_rc: i32,
    post_rc: i32,
) -> String {
    let telemetry = gh_log
        .parent()
        .unwrap()
        .join("test-sweep-outcome-telemetry.jsonl");
    let post_exit = if post_rc < 0 {
        "exec sleep 30".to_string()
    } else {
        format!("exit {post_rc}")
    };
    format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/*/comments && \"$*\" == *\"--method POST\"* ]]; then\n\
         n=\"${{2%/comments}}\"; n=\"${{n##*/}}\"\n\
         b=\"${{@: -1}}\"; b=\"${{b#body=}}\"\n\
         if [[ -s \"{telemetry}\" ]]; then durable=telemetry-durable-before-post; \
         else durable=telemetry-missing-at-post; fi\n\
         printf 'issue comment %s %s\\n' \"$n\" \"$durable\" >> \"{gh_log}\"\n\
         printf '%s\\n' \"$b\" >> \"{gh_log}.bodies\"\n\
         printf '%s\\n' \"${{b%%$'\\n'*}}\" >> \"{posted_marker}\"\n\
         {post_exit}\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/*/comments ]]; then\n\
         if [[ -f \"{posted_marker}\" ]]; then\n\
         while IFS= read -r m; do [[ -n \"$m\" && \"$5\" == *\"$m\"* ]] && printf '111\\n'; \
         done < \"{posted_marker}\"\n\
         fi\n\
         exit {comments_rc}\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* ]]; then\n\
         if [[ \"$4\" == \".body\" ]]; then printf '%s' '{body}'; \
         else printf '%s' '{projection}'; fi\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        telemetry = telemetry.display(),
        gh_log = gh_log.display(),
        posted_marker = posted_marker.display(),
        body = body.replace('\'', "'\\''"),
        // #9441: `complexity_signal` now asks for a `{body, state, closed_at,
        // labels}` projection while `points_signal` still asks for a bare
        // `.body`, so the fake discriminates on the `--jq` argument exactly
        // as the real `gh` would.
        projection = serde_json::json!({
            "body": body,
            "state": "open",
            "closed_at": serde_json::Value::Null,
            "labels": Vec::<String>::new(),
        })
        .to_string()
        .replace('\'', "'\\''"),
    )
}

/// [`fake_gh_script_with`] with every call succeeding.
fn fake_gh_script(body: &str, gh_log: &Path, posted_marker: &Path) -> String {
    fake_gh_script_with(body, gh_log, posted_marker, 0, 0)
}

fn posted_bodies(gh_log: &Path) -> String {
    std::fs::read_to_string(format!("{}.bodies", gh_log.display())).unwrap_or_default()
}

fn telemetry_journal(ws: &Path) -> String {
    std::fs::read_to_string(ws.join("test-sweep-outcome-telemetry.jsonl")).unwrap_or_default()
}

fn run_success(registry: &mut SweepRegistry, issue: u32, sweep_id: &str) {
    registry.append_outcome_telemetry_journal(
        issue,
        sweep_id,
        4000,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
}

const MARKED_BODY: &str =
    "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";

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
    let bodies = posted_bodies(&gh_log);
    assert!(
        bodies.starts_with(&format!("<!-- loom:sweep-outcome-writeback sweep={sweep_id} -->\n")),
        "the posted body opens with THIS sweep's marker: {bodies}"
    );
    assert!(bodies.contains("points `8`"), "the points read reaches the comment: {bodies}");
    assert!(
        bodies.contains("complexity `complex`"),
        "the complexity read reaches the comment: {bodies}"
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
    let body = "Some issue.\n\n<!-- loom:complexity=complex -->\n<!-- loom:points=8 -->\n";
    let mut registry = writeback_registry(ws, &fake_gh_script(body, &gh_log, &posted_marker));
    let issue = 90564;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-4", "log\n");
    // Pre-seed THIS sweep's marker, simulating a write-back that already
    // landed from an earlier terminal observation (or a prior daemon run).
    std::fs::write(
        &posted_marker,
        format!("{}\n", writeback::sweep_outcome_writeback_marker(&sweep_id)),
    )
    .unwrap();

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

/// Judge #9216 item 2: idempotency is per SWEEP. A write-back already left on
/// the issue by a DIFFERENT sweep (a partial-increment slice reusing the issue
/// number, a re-opened issue, a re-dispatch) must not suppress this sweep's
/// own post — it has its own actuals.
#[test]
#[serial]
fn a_different_sweeps_prior_comment_does_not_suppress_this_sweeps_post() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    std::fs::write(
        &posted_marker,
        format!(
            "{}\n",
            writeback::sweep_outcome_writeback_marker("sweep-issue-90566-earlier-slice")
        ),
    )
    .unwrap();
    let mut registry =
        writeback_registry(ws, &fake_gh_script(MARKED_BODY, &gh_log, &posted_marker));
    let issue = 90566;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-6", "log\n");
    assert_ne!(sweep_id, "sweep-issue-90566-earlier-slice");

    run_success(&mut registry, issue, &sweep_id);

    assert_eq!(
        count_comment_posts(&gh_log),
        1,
        "another sweep's write-back on the same issue must not suppress this one"
    );
    assert!(
        posted_bodies(&gh_log).contains(&format!("sweep={sweep_id} -->")),
        "{}",
        posted_bodies(&gh_log)
    );
}

/// Fail CLOSED: when the idempotency read itself fails (non-zero exit), the
/// write-back skips this pass rather than risk a duplicate.
#[test]
#[serial]
fn an_unreadable_comments_check_fails_closed_and_never_posts() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let mut registry =
        writeback_registry(ws, &fake_gh_script_with(MARKED_BODY, &gh_log, &posted_marker, 1, 0));
    let issue = 90567;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-7", "log\n");

    run_success(&mut registry, issue, &sweep_id);

    assert_eq!(
        count_comment_posts(&gh_log),
        0,
        "an unverifiable comments read must fail closed (no post)"
    );
    assert!(
        telemetry_journal(ws).contains(&sweep_id),
        "the durable sweep.outcome record is written regardless"
    );
}

/// Judge #9216 item 1: the forge post runs strictly AFTER the durable
/// `sweep.outcome` telemetry append, and a failing post never loses it.
#[test]
#[serial]
fn telemetry_is_durable_before_the_post_and_survives_a_failed_post() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let mut registry =
        writeback_registry(ws, &fake_gh_script_with(MARKED_BODY, &gh_log, &posted_marker, 0, 1));
    let issue = 90568;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-8", "log\n");

    run_success(&mut registry, issue, &sweep_id);

    let log_contents = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        log_contents.contains("telemetry-durable-before-post"),
        "the telemetry record must already be on disk when gh posts: {log_contents}"
    );
    assert!(
        telemetry_journal(ws).contains(&sweep_id),
        "a failed post must not lose the durable sweep.outcome record"
    );
}

/// A HUNG post is bounded by `reap_gh_timeout()` and — because it runs after
/// the append — cannot delay or lose the durable record.
#[test]
#[serial]
fn a_hung_post_is_bounded_and_the_record_is_already_durable() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    enable_writeback(ws);
    let gh_log = ws.join("gh-invocations.log");
    let posted_marker = ws.join("posted.marker");
    let mut registry =
        writeback_registry(ws, &fake_gh_script_with(MARKED_BODY, &gh_log, &posted_marker, 0, -1));
    let issue = 90569;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-wb-9", "log\n");

    std::env::set_var(REAP_GH_TIMEOUT_ENV, "1");
    let started = std::time::Instant::now();
    run_success(&mut registry, issue, &sweep_id);
    let elapsed = started.elapsed();
    std::env::remove_var(REAP_GH_TIMEOUT_ENV);

    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the hung post must be killed at the gh timeout, took {elapsed:?}"
    );
    assert!(
        std::fs::read_to_string(&gh_log)
            .unwrap_or_default()
            .contains("telemetry-durable-before-post"),
        "the record was durable before the (hung) post started"
    );
    assert!(telemetry_journal(ws).contains(&sweep_id));
}
