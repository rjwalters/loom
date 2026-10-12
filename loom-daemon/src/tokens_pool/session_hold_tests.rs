//! Issue #10453: the operator-hold / last-start sidecars.

use super::*;

#[test]
fn a_hold_is_lifted_only_by_deleting_it() {
    let dir = tempfile::tempdir().unwrap();
    let p = vec![dir.path().to_path_buf()];
    assert!(!held_across(&p));
    write_hold(dir.path(), 1_000).unwrap();
    assert!(held_across(&p));
    let raw: Hold =
        serde_json::from_slice(&std::fs::read(dir.path().join(HOLD_FILE)).unwrap()).unwrap();
    assert_eq!(raw.reason, "operator stop");
    // A start record alone does not lift it, however new.
    record_operator_start(dir.path(), Path::new("/home/u/GitHub"), "img:1", u64::MAX - 1).unwrap();
    assert!(held_across(&p));
    lift_holds(&p).unwrap();
    assert!(!held_across(&p));
    assert_eq!(latest_start(&p).unwrap().workspace, Path::new("/home/u/GitHub"));
    lift_holds(&p).unwrap(); // idempotent
}

#[test]
fn a_hold_written_after_a_future_dated_start_still_holds_and_sorts_after_it() {
    // Wall clock stepped back between start and stop (verdict r2, blocker 1).
    let dir = tempfile::tempdir().unwrap();
    record_operator_start(dir.path(), Path::new("/w"), "img", 10_000).unwrap();
    write_hold(dir.path(), 5_000).unwrap();
    assert!(held_across(&[dir.path().to_path_buf()]));
    let raw: Hold =
        serde_json::from_slice(&std::fs::read(dir.path().join(HOLD_FILE)).unwrap()).unwrap();
    assert_eq!(raw.held_at_unix_ms, 10_001);
}

#[test]
fn a_hold_in_any_profile_dir_holds_the_account_until_each_is_lifted() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let both = vec![a.path().to_path_buf(), b.path().to_path_buf()];
    write_hold(b.path(), 1_000).unwrap();
    assert!(held_across(&both));
    record_operator_start(a.path(), Path::new("/w"), "img", 2_000).unwrap();
    assert!(held_across(&both), "a start record elsewhere does not out-date it");
    lift_holds(&both).unwrap();
    assert!(!held_across(&both));
    assert_eq!(latest_start(&both).unwrap().image, "img");
}

#[test]
fn an_unreadable_or_empty_hold_file_fails_safe_to_held() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(HOLD_FILE), b"").unwrap();
    assert!(held_across(&[dir.path().to_path_buf()]));
    std::fs::write(dir.path().join(HOLD_FILE), b"{not json").unwrap();
    assert!(held_across(&[dir.path().to_path_buf()]));
}
