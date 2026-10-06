//! Issue #10161: the insta-crash quarantine parks through a body park record
//! (`park_record::apply`) before `loom:blocked`.
//!
//! Lives in its own sibling module rather than inside `quarantine.rs`'s
//! `mod tests`: that file is over the file-size ratchet's threshold and is
//! therefore frozen at its current size (see `.loom/docs/file-size-policy.md`).
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// A registry whose fake `gh` logs every invocation and answers the park
/// adapter's `issue view --json body,labels`. `body_edit_exit` is the exit
/// code of the body edit; `hang_on` is a substring of `$*` on which the fake
/// `exec sleep`s instead of answering (a wedged `gh`, killed by the timeout).
fn park_registry(
    dir: &Path,
    body_edit_exit: i32,
    hang_on: Option<&str>,
) -> (SweepRegistry, PathBuf) {
    let gh_log = dir.join("gh.log");
    let fake = dir.join("fake-gh-park.sh");
    let hang = hang_on.map_or_else(String::new, |pat| {
        format!("if [[ \"$*\" == *\"{pat}\"* ]]; then exec sleep 30; fi\n")
    });
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         {hang}\
         {park_view}\
         if [[ \"$1\" == \"issue\" && \"$2\" == \"edit\" && \"$*\" == *--body* ]]; then\n\
         exit {body_exit}\n\
         fi\n\
         exit 0\n",
        log = gh_log.display(),
        park_view = crate::sweep_registry::test_support::fake_gh_park_view_arm(),
        body_exit = body_edit_exit,
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = SweepRegistryConfig::new(dir.to_path_buf());
    config.gh_bin = Some(fake);
    config.skip_label_flip = false;
    (SweepRegistry::new(config), gh_log)
}

/// One entry per `gh` invocation: a multi-line `--body` spills continuation
/// lines into the log, so keep only lines that start an invocation.
fn logged_calls(gh_log: &Path) -> Vec<String> {
    std::fs::read_to_string(gh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("issue ") || l.starts_with("api "))
        .map(String::from)
        .collect()
}

#[test]
#[serial]
fn apply_quarantine_label_writes_body_record_before_the_label() {
    let dir = tempdir().unwrap();
    let (registry, gh_log) = park_registry(dir.path(), 0, None);
    registry.apply_quarantine_label(9601, 3);
    let calls = logged_calls(&gh_log);
    let body = calls
        .iter()
        .position(|c| c.contains("--body <!-- loom:park"))
        .unwrap();
    let label = calls
        .iter()
        .position(|c| c.contains("--add-label loom:blocked"))
        .unwrap();
    assert!(body < label, "body record must precede the label: {calls:?}");
    assert!(calls[body].contains("reason=\"insta-crash quarantine\""), "{calls:?}");
    assert!(calls[body].contains("by=daemon"), "{calls:?}");
    assert!(calls[label].contains("--remove-label loom:issue"), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("issue comment 9601")), "{calls:?}");
}

/// A REJECTED (non-timeout) body write leaves no label edit — no label-only
/// park — while the explanatory comment is still posted.
#[test]
#[serial]
fn apply_quarantine_label_failed_body_write_applies_no_label() {
    let dir = tempdir().unwrap();
    let (registry, gh_log) = park_registry(dir.path(), 1, None);
    registry.apply_quarantine_label(9602, 3);
    let log = std::fs::read_to_string(&gh_log).unwrap();
    assert!(!log.contains("--add-label"), "label-only park: {log}");
    assert!(log.contains("issue comment 9602"), "comment is still posted: {log}");
}

/// Run `apply_quarantine_label` against a fake `gh` that wedges on `hang_on`,
/// with a 1s reap timeout; return the logged calls and the elapsed time.
fn run_wedged(issue: u32, hang_on: &str) -> (Vec<String>, std::time::Duration) {
    let dir = tempdir().unwrap();
    let (registry, gh_log) = park_registry(dir.path(), 0, Some(hang_on));
    std::env::set_var(REAP_GH_TIMEOUT_ENV, "1");
    let started = std::time::Instant::now();
    registry.apply_quarantine_label(issue, 3);
    let elapsed = started.elapsed();
    std::env::remove_var(REAP_GH_TIMEOUT_ENV);
    (logged_calls(&gh_log), elapsed)
}

/// Review of #10578: a TIMED-OUT park must not be followed by the comment —
/// that would spend a second full `reap_gh_timeout()` on the `ListSweeps` /
/// `GetSweepStatus` read path (#3973). A wedge on the first call (`park.view`)
/// spawns exactly one `gh` process for the whole writer.
#[test]
#[serial]
fn apply_quarantine_label_timed_out_view_spawns_no_second_gh() {
    let (calls, elapsed) = run_wedged(9603, "body,labels");
    assert_eq!(calls.len(), 1, "only the wedged view may be spawned: {calls:?}");
    assert!(calls[0].contains("--json body,labels"), "{calls:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the wedged view must be killed at the gh timeout, took {elapsed:?}"
    );
}

/// The same budget when the wedge is the body write (`park.body`): view, then
/// the killed body edit, and nothing after it — no label edit, no comment.
#[test]
#[serial]
fn apply_quarantine_label_timed_out_body_write_skips_label_and_comment() {
    let (calls, elapsed) = run_wedged(9604, "--body <!-- loom:park");
    assert_eq!(calls.len(), 2, "view + the wedged body edit only: {calls:?}");
    assert!(calls[1].contains("--body <!-- loom:park"), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("issue comment")), "{calls:?}");
    assert!(elapsed < std::time::Duration::from_secs(20), "took {elapsed:?}");
}
