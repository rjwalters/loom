//! Contracts for `loom-daemon lease renewer` (#10229).

use super::*;
use std::cell::RefCell;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir: dir.path().join("lease-renew"),
    };
    (dir, store)
}

fn key(repo: &str, host: &str, sweep: &str, issue: u64) -> Key {
    Key {
        repo: repo.into(),
        host: host.into(),
        sweep: sweep.into(),
        issue,
    }
}

fn ident(pid: u32) -> String {
    format!("id-{pid}")
}

/// Every recorded owner is alive and still itself.
fn all_live(_: u32, _: &str) -> bool {
    true
}

fn none_live(_: u32, _: &str) -> bool {
    false
}

#[test]
fn first_claim_owns_and_records_identity() {
    let (_d, s) = store();
    let k = key("acme/widget", "h", "s", 7);
    assert_eq!(claim(&s, &k, 100, "t1", &ident, &all_live).unwrap(), 100);
    let rec = s.read(&k).unwrap().unwrap();
    assert_eq!((rec.pid, rec.ident.as_str(), rec.token.as_str()), (100, "id-100", "t1"));
    assert!(!rec.released);
}

#[test]
fn a_live_owner_wins_against_a_duplicate_start() {
    let (_d, s) = store();
    let k = key("acme/widget", "h", "s", 7);
    claim(&s, &k, 100, "t1", &ident, &all_live).unwrap();
    assert_eq!(claim(&s, &k, 200, "t2", &ident, &all_live).unwrap(), 100);
    assert_eq!(s.read(&k).unwrap().unwrap().token, "t1", "the live owner is never evicted");
}

#[test]
fn a_dead_or_recycled_owner_is_taken_over() {
    let (_d, s) = store();
    let k = key("acme/widget", "h", "s", 7);
    claim(&s, &k, 100, "t1", &ident, &all_live).unwrap();
    // pid 100 is alive but no longer the process recorded (pid reuse).
    let recycled = |pid: u32, id: &str| pid != 100 || id != "id-100";
    assert_eq!(claim(&s, &k, 200, "t2", &ident, &recycled).unwrap(), 200);
    assert_eq!(s.read(&k).unwrap().unwrap().token, "t2");
}

#[test]
fn keys_are_independent_per_repo_host_sweep_and_issue() {
    let (_d, s) = store();
    let base = key("acme/widget", "h", "s", 7);
    claim(&s, &base, 100, "t1", &ident, &all_live).unwrap();
    for (i, other) in [
        key("acme/other", "h", "s", 7),
        key("acme/widget", "h2", "s", 7),
        key("acme/widget", "h", "s2", 7),
        key("acme/widget", "h", "s", 8),
    ]
    .iter()
    .enumerate()
    {
        let pid = 200 + i as u32;
        assert_eq!(claim(&s, other, pid, "tx", &ident, &all_live).unwrap(), pid);
    }
}

#[test]
fn claim_sweeps_dead_owners_records() {
    let (_d, s) = store();
    let gone = key("acme/widget", "h", "s", 1);
    claim(&s, &gone, 100, "t1", &ident, &all_live).unwrap();
    let only_new = |pid: u32, _: &str| pid != 100;
    claim(&s, &key("acme/widget", "h", "s", 2), 200, "t2", &ident, &only_new).unwrap();
    assert!(s.read(&gone).unwrap().is_none(), "dead owner's record removed");
}

#[test]
fn check_honours_ownership_before_state() {
    let (_d, s) = store();
    let k = key("acme/widget", "h", "s", 7);
    // Never recorded: fail open (renew), state still decides.
    assert_eq!(check(&s, &k, "t1", Some("open")).0, EXIT_RENEW);
    claim(&s, &k, 100, "t1", &ident, &all_live).unwrap();
    assert_eq!(check(&s, &k, "t1", None).0, EXIT_RENEW);
    let (code, why) = check(&s, &k, "t-other", Some("open"));
    assert_eq!(code, EXIT_STOP);
    assert!(why.unwrap().contains("pid 100"));
}

#[test]
fn check_maps_issue_state() {
    let (_d, s) = store();
    let k = key("acme/widget", "h", "s", 7);
    claim(&s, &k, 100, "t", &ident, &all_live).unwrap();
    for (state, want) in [
        ("open", EXIT_RENEW),
        ("OPEN\n", EXIT_RENEW),
        (r#"{"state":"open","title":"x"}"#, EXIT_RENEW),
        ("closed", EXIT_STOP),
        (r#"{"state":"closed"}"#, EXIT_STOP),
        ("", EXIT_SKIP),
        ("not json", EXIT_SKIP),
        (r#"{"message":"API rate limit exceeded"}"#, EXIT_SKIP),
    ] {
        assert_eq!(check(&s, &k, "t", Some(state)).0, want, "state {state:?}");
    }
}

#[test]
fn release_tombstones_exactly_its_key_and_signals_live_owners() {
    let (_d, s) = store();
    let mine = key("acme/widget", "h", "s", 7);
    let peer_sweep = key("acme/widget", "h", "s2", 7);
    let peer_repo = key("acme/other", "h", "s", 7);
    claim(&s, &mine, 100, "t1", &ident, &all_live).unwrap();
    claim(&s, &peer_sweep, 200, "t2", &ident, &all_live).unwrap();
    claim(&s, &peer_repo, 300, "t3", &ident, &all_live).unwrap();

    let signalled = RefCell::new(Vec::new());
    let sig = |pid: u32| signalled.borrow_mut().push(pid);
    let n = release(&s, "acme/widget", 7, None, "s", &all_live, &sig).unwrap();
    assert_eq!(n, 1);
    assert_eq!(*signalled.borrow(), vec![100]);
    assert_eq!(check(&s, &mine, "t1", Some("open")).0, EXIT_STOP, "tombstone stops the loop");
    assert_eq!(check(&s, &peer_sweep, "t2", Some("open")).0, EXIT_RENEW);
    assert_eq!(check(&s, &peer_repo, "t3", Some("open")).0, EXIT_RENEW);

    // A later start after release is a fresh owner.
    assert_eq!(claim(&s, &mine, 400, "t4", &ident, &all_live).unwrap(), 400);
}

#[test]
fn release_is_idempotent_and_never_signals_a_dead_or_recycled_pid() {
    let (_d, s) = store();
    let sig = |_: u32| panic!("must not signal");
    assert_eq!(release(&s, "acme/widget", 7, None, "s", &all_live, &sig).unwrap(), 0);
    claim(&s, &key("acme/widget", "h", "s", 7), 100, "t", &ident, &all_live).unwrap();
    assert_eq!(release(&s, "acme/widget", 7, None, "s", &none_live, &sig).unwrap(), 1);
}

#[test]
fn release_host_filter_is_exact_when_given() {
    let (_d, s) = store();
    claim(&s, &key("acme/widget", "h1", "s", 7), 100, "t", &ident, &all_live).unwrap();
    let sig = |_: u32| {};
    assert_eq!(release(&s, "acme/widget", 7, Some("h2"), "s", &all_live, &sig).unwrap(), 0);
    assert_eq!(release(&s, "acme/widget", 7, Some("h1"), "s", &all_live, &sig).unwrap(), 1);
}

#[test]
fn parse_state_rejects_anything_but_open_or_closed() {
    assert_eq!(parse_state("\"closed\""), Some("closed"));
    assert_eq!(parse_state("all"), None);
    assert_eq!(parse_state("{"), None);
}

#[test]
fn repo_identity_prefers_loom_repo_then_origin_slug() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(repo_identity(Some(" Acme/Widget "), dir.path()), "acme/widget");
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "git@github.com:Acme/Widget.git"]);
    assert_eq!(repo_identity(None, dir.path()), "acme/widget");
    assert_eq!(repo_identity(Some(""), dir.path()), "acme/widget");
}

#[test]
fn start_identity_tracks_a_real_process_and_its_death() {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    let first = start_identity(pid).expect("identity of a live process");
    assert_eq!(start_identity(pid).as_deref(), Some(first.as_str()), "stable");
    assert!(owner_is_live(pid, &first));
    assert!(
        !owner_is_live(pid, "lstart:Thu Jan  1 00:00:00 1970"),
        "a mismatched identity reads as dead"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!owner_is_live(pid, &first));
}
