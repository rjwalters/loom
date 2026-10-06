//! Issue #10453: the operator-hold / last-start sidecars.

use super::*;

#[test]
fn a_hold_is_lifted_only_by_a_newer_operator_start() {
    let dir = tempfile::tempdir().unwrap();
    let p = vec![dir.path().to_path_buf()];
    assert!(!held_across(&p));
    write_hold(dir.path(), 1_000).unwrap();
    assert!(held_across(&p));
    let raw: Hold =
        serde_json::from_slice(&std::fs::read(dir.path().join(HOLD_FILE)).unwrap()).unwrap();
    assert_eq!(raw.reason, "operator stop");
    record_operator_start(dir.path(), Path::new("/home/u/GitHub"), "img:1", 2_000).unwrap();
    assert!(!held_across(&p));
    assert!(!dir.path().join(HOLD_FILE).exists());
    assert_eq!(latest_start(&p).unwrap().workspace, Path::new("/home/u/GitHub"));
    // stop again: held despite the older start record.
    write_hold(dir.path(), 3_000).unwrap();
    assert!(held_across(&p));
}

#[test]
fn a_hold_in_one_profile_dir_holds_the_account_until_a_newer_start_anywhere() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let both = vec![a.path().to_path_buf(), b.path().to_path_buf()];
    write_hold(b.path(), 1_000).unwrap();
    assert!(held_across(&both));
    record_operator_start(a.path(), Path::new("/w"), "img", 2_000).unwrap();
    assert!(!held_across(&both), "a newer start in another root lifts it");
    write_hold(b.path(), 3_000).unwrap();
    assert!(held_across(&both));
    assert_eq!(latest_start(&both).unwrap().image, "img");
}

#[test]
fn an_unreadable_hold_file_fails_safe_to_held() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(HOLD_FILE), b"{not json").unwrap();
    record_operator_start(dir.path(), Path::new("/w"), "img", 5).unwrap();
    // record_operator_start removed it; re-corrupt after the start.
    std::fs::write(dir.path().join(HOLD_FILE), b"{not json").unwrap();
    assert!(held_across(&[dir.path().to_path_buf()]));
}
