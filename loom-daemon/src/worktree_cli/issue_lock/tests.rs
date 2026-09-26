//! Tests for the per-issue claim-lock cross-check (#8553).

use super::*;

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    // A real repo, so `locks_dir` exercises the `--git-common-dir` path rather
    // than its fallback.
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(d.path())
        .args(["init", "-q"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init must succeed for these tests to mean anything");
    d
}

fn write_owner(repo: &Path, issue: u32, owner_pid: u32, sweep_id: &str, acquired_at: &str) {
    let lock = locks_dir(repo).join(format!("issue-{issue}"));
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(
        lock.join("owner.json"),
        format!(
            r#"{{"issue": {issue}, "owner_pid": {owner_pid}, "acquired_at": "{acquired_at}", "sweep_id": "{sweep_id}"}}"#
        ),
    )
    .unwrap();
}

#[test]
fn no_lock_dir_is_free() {
    let d = repo();
    assert!(check(d.path(), 42).is_none());
}

#[test]
fn a_live_owner_is_reported_live() {
    let d = repo();
    // Our own pid is guaranteed alive for the duration of the test.
    write_owner(d.path(), 42, std::process::id(), "sweep-issue-42-abc", "2026-01-01T00:00:00Z");
    let live = check(d.path(), 42).expect("a live owner must be reported live");
    assert_eq!(live.sweep_id, "sweep-issue-42-abc");
    assert_eq!(live.owner_pid, std::process::id());
    assert_eq!(live.acquired_at, "2026-01-01T00:00:00Z");
}

#[test]
fn a_dead_owner_pid_is_free() {
    let d = repo();
    // Spawn and reap a child so its pid is guaranteed dead.
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    let dead_pid = child.id();
    child.wait().expect("reap child");
    write_owner(d.path(), 42, dead_pid, "sweep-issue-42-dead", "2026-01-01T00:00:00Z");
    assert!(check(d.path(), 42).is_none(), "a dead owner pid must fail open");
}

#[test]
fn unparsable_owner_json_is_free() {
    let d = repo();
    let lock = locks_dir(d.path()).join("issue-42");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("owner.json"), "{ not json").unwrap();
    assert!(
        check(d.path(), 42).is_none(),
        "garbage metadata must never wedge a live verdict"
    );
}

#[test]
fn a_missing_owner_pid_field_defaults_to_zero_and_is_free() {
    let d = repo();
    let lock = locks_dir(d.path()).join("issue-42");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("owner.json"), r#"{"issue": 42}"#).unwrap();
    assert!(check(d.path(), 42).is_none());
}

#[test]
fn a_different_issues_lock_does_not_leak_into_this_ones_check() {
    let d = repo();
    write_owner(d.path(), 99, std::process::id(), "sweep-issue-99-abc", "2026-01-01T00:00:00Z");
    assert!(check(d.path(), 42).is_none(), "issue 42 has no lock of its own");
    assert!(check(d.path(), 99).is_some(), "issue 99's own lock is unaffected");
}

#[test]
fn age_desc_reports_a_bounded_non_empty_string_for_a_live_lock() {
    let d = repo();
    write_owner(d.path(), 42, std::process::id(), "sweep-issue-42-abc", "2026-01-01T00:00:00Z");
    let live = check(d.path(), 42).expect("live");
    assert!(!live.age_desc().is_empty());
}

#[test]
fn age_desc_degrades_gracefully_on_an_unparsable_timestamp() {
    let d = repo();
    write_owner(d.path(), 42, std::process::id(), "sweep-issue-42-abc", "not-a-timestamp");
    let live = check(d.path(), 42).expect("live");
    assert_eq!(live.age_desc(), "unknown age");
}

#[test]
fn owned_by_the_same_sweep_id_is_true() {
    let d = repo();
    write_owner(d.path(), 42, std::process::id(), "sweep-issue-42-abc", "2026-01-01T00:00:00Z");
    let live = check(d.path(), 42).expect("live");
    assert!(
        live.owned_by(Some("sweep-issue-42-abc")),
        "the sweep that holds the lock must recognize itself as the owner (#8702)"
    );
}

#[test]
fn owned_by_a_different_sweep_id_is_false() {
    let d = repo();
    write_owner(d.path(), 42, std::process::id(), "sweep-issue-42-abc", "2026-01-01T00:00:00Z");
    let live = check(d.path(), 42).expect("live");
    assert!(!live.owned_by(Some("sweep-issue-42-other")));
}

#[test]
fn owned_by_no_caller_sweep_id_is_false() {
    let d = repo();
    write_owner(d.path(), 42, std::process::id(), "sweep-issue-42-abc", "2026-01-01T00:00:00Z");
    let live = check(d.path(), 42).expect("live");
    assert!(!live.owned_by(None), "an unset LOOM_SWEEP_ID must still refuse (#8553)");
}
