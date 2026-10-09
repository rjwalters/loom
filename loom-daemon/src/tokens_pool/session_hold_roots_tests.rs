//! Issue #10661 item 3: one hold root set for the CLI and the reconcile pass.

use super::*;

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

#[test]
fn hold_roots_are_the_registered_roots_then_an_unregistered_fallback() {
    let registered = vec![p("/w/a"), p("/w/b")];
    assert_eq!(
        hold_roots(&registered, Some(Path::new("/srv/daemon"))),
        vec![p("/w/a"), p("/w/b"), p("/srv/daemon")]
    );
    // A registered fallback is not repeated; an empty registry is the fallback alone.
    assert_eq!(hold_roots(&registered, Some(Path::new("/w/b"))), registered);
    assert_eq!(hold_roots(&[], Some(Path::new("/srv/daemon"))), vec![p("/srv/daemon")]);
    assert_eq!(hold_roots(&registered, None), registered);
}

#[test]
fn the_cli_peers_are_the_hold_roots_without_its_own_workspace() {
    let registered = vec![p("/w/a"), p("/w/b")];
    assert_eq!(
        cli_peer_roots(Path::new("/w/a"), &registered, Some(Path::new("/srv/daemon"))),
        vec![p("/w/b"), p("/srv/daemon")]
    );
    // The daemon's fallback root with an empty registry: before #10661 the
    // CLI's peers were empty here.
    assert_eq!(
        cli_peer_roots(Path::new("/w/a"), &[], Some(Path::new("/srv/daemon"))),
        vec![p("/srv/daemon")]
    );
    assert!(
        cli_peer_roots(Path::new("/srv/daemon"), &[], Some(Path::new("/srv/daemon"))).is_empty()
    );
}

#[test]
fn the_fallback_root_record_round_trips_and_fails_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("nested").join("fallback-root.json");
    assert_eq!(read_fallback_root(&file), None);
    record_fallback_root(&file, Path::new("/srv/daemon")).unwrap();
    assert_eq!(read_fallback_root(&file), Some(p("/srv/daemon")));
    record_fallback_root(&file, Path::new("/srv/other")).unwrap();
    assert_eq!(read_fallback_root(&file), Some(p("/srv/other")), "replaced, not appended");
    for bad in [
        &b""[..],
        b"{not json",
        br#"{"schema_version":2,"root":"/srv/daemon"}"#,
        br#"{"schema_version":1,"root":"relative/root"}"#,
    ] {
        std::fs::write(&file, bad).unwrap();
        assert_eq!(read_fallback_root(&file), None, "{}", String::from_utf8_lossy(bad));
    }
}
