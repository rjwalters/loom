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
fn release_is_a_local_exact_key_signal_never_a_pre_emptive_stop() {
    let (_d, s) = store();
    let sig = |_: u32| {};
    // Releasing a key nobody owns leaves no tombstone behind...
    assert_eq!(release(&s, "acme/widget", 7, Some("h"), "s", &all_live, &sig).unwrap(), 0);
    assert!(s.all().is_empty(), "no record is created by an unmatched release");
    // ...so a later start of that same key is a normal owner and keeps renewing.
    let k = key("acme/widget", "h", "s", 7);
    assert_eq!(claim(&s, &k, 100, "t", &ident, &all_live).unwrap(), 100);
    assert_eq!(check(&s, &k, "t", Some("open")).0, EXIT_RENEW);
    // A release for another sweep's or issue's key never stops this loop.
    for (issue, sweep) in [(8, "s"), (7, "other")] {
        release(&s, "acme/widget", issue, Some("h"), sweep, &all_live, &sig).unwrap();
        assert_eq!(check(&s, &k, "t", Some("open")).0, EXIT_RENEW);
    }
    // An unverified state after a release still stops: a release is never "skipped".
    release(&s, "acme/widget", 7, Some("h"), "s", &all_live, &sig).unwrap();
    assert_eq!(check(&s, &k, "t", Some("")).0, EXIT_STOP);
    assert_eq!(check(&s, &k, "t", None).0, EXIT_STOP);
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

// --- stop (#11086) ---------------------------------------------------------

fn argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

#[test]
fn renewer_argv_matches_only_this_toolings_renewers() {
    assert!(is_renewer_argv(&argv(
        "bash /w/.loom/scripts/sweep-lease-renew.sh start 7 --watch-pid 1"
    )));
    assert!(is_renewer_argv(&argv("/bin/bash sweep-lease-renew.sh start 7")));
    assert!(is_renewer_argv(&argv("/usr/bin/loom-daemon lease renewer claim 7")));
    assert!(!is_renewer_argv(&argv("sleep 30")));
    assert!(!is_renewer_argv(&argv("claude --resume start")));
    assert!(!is_renewer_argv(&argv("bash sweep-lease-renew.sh renew-once 7")));
    assert!(!is_renewer_argv(&argv("vim notes sweep-lease-renew.sh start")));
    assert!(!is_renewer_argv(&argv("loom-daemon lease ensure 7")));
    assert!(!is_renewer_argv(&argv("grep loom-daemon lease renewer")));
}

#[test]
fn renewer_argv_requires_a_shell_running_the_script_file() {
    for ok in [
        "sh x/sweep-lease-renew.sh start 7",
        "/usr/bin/zsh x/sweep-lease-renew.sh start 7",
        "dash x/sweep-lease-renew.sh start 7",
        "bash -x x/sweep-lease-renew.sh start 7",
        "bash -eu x/sweep-lease-renew.sh start 7",
    ] {
        assert!(is_renewer_argv(&argv(ok)), "{ok}");
    }
    // Non-shell programs whose arguments look like a renewer's.
    for bad in [
        "python3 x/sweep-lease-renew.sh start 7",
        "python3 -I /tmp/x/sweep-lease-renew.sh start",
        "vim x/sweep-lease-renew.sh start",
        "/usr/bin/vim -R x/sweep-lease-renew.sh start",
        "less x/sweep-lease-renew.sh start",
        "x/sweep-lease-renew.sh start 7",
        "fakebash x/sweep-lease-renew.sh start",
        // Command-string / stdin forms and options that take an argument.
        "bash -c sleep x/sweep-lease-renew.sh start",
        "bash -xc sleep x/sweep-lease-renew.sh start",
        "sh -c sleep x/sweep-lease-renew.sh start",
        "bash -s x/sweep-lease-renew.sh start",
        "bash -o x/sweep-lease-renew.sh start",
        "bash -O x/sweep-lease-renew.sh start",
        "bash --rcfile x/sweep-lease-renew.sh start",
        "bash -- x/sweep-lease-renew.sh start",
        "bash - x/sweep-lease-renew.sh start",
    ] {
        assert!(!is_renewer_argv(&argv(bad)), "{bad}");
    }
    assert!(!is_renewer_argv(&[]));
}

#[test]
fn stop_pid_refusal_names_the_program_but_never_its_arguments() {
    let sent = RefCell::new(vec![]);
    let fake = "FAKE-SECRET-SENTINEL-must-not-be-logged";
    let cmd = format!("/opt/tool/deploy --token {fake} x/sweep-lease-renew.sh start");
    let StopOutcome::Refused(why) = run_stop("500", &[Some("a")], Some(&cmd), &sent) else {
        panic!("expected a refusal");
    };
    assert!(why.contains("pid 500") && why.contains("program: deploy"), "{why}");
    assert!(
        !why.contains(fake) && !why.contains("--token") && !why.contains("/opt/tool"),
        "{why}"
    );
    assert!(sent.borrow().is_empty());
}

fn run_stop(
    arg: &str,
    ids: &[Option<&str>],
    args: Option<&str>,
    sent: &RefCell<Vec<u32>>,
) -> StopOutcome {
    let calls = RefCell::new(0usize);
    let ident_of = |_: u32| {
        let i = (*calls.borrow()).min(ids.len() - 1);
        *calls.borrow_mut() += 1;
        ids[i].map(str::to_string)
    };
    stop_pid(arg, 4242, &ident_of, &|_| args.map(argv), &|p| sent.borrow_mut().push(p))
}

#[test]
fn stop_pid_signals_a_verified_renewer_only() {
    let sent = RefCell::new(vec![]);
    let r = run_stop("500", &[Some("a")], Some("bash x/sweep-lease-renew.sh start 9"), &sent);
    assert_eq!(r, StopOutcome::Stopped);
    assert_eq!(*sent.borrow(), vec![500]);
}

#[test]
fn stop_pid_refuses_unrelated_nonnumeric_and_reserved_pids_without_signalling() {
    let sent = RefCell::new(vec![]);
    let renewer = Some("bash sweep-lease-renew.sh start 9");
    for (arg, a) in [
        ("500", Some("sleep 30")),
        ("500", Some("python3 -I /tmp/x/sweep-lease-renew.sh start")),
        ("500", Some("vim x/sweep-lease-renew.sh start")),
        ("11086", Some("node server.js")),
        ("abc", renewer),
        ("-1", renewer),
        ("", renewer),
        ("1", renewer),
        ("4242", renewer),
        ("500", None),
    ] {
        assert!(
            matches!(run_stop(arg, &[Some("a")], a, &sent), StopOutcome::Refused(_)),
            "{arg:?}"
        );
    }
    assert!(sent.borrow().is_empty());
}

#[test]
fn stop_pid_refuses_an_identity_that_changes_mid_check_and_ignores_a_dead_pid() {
    let sent = RefCell::new(vec![]);
    let r =
        run_stop("500", &[Some("a"), Some("b")], Some("bash sweep-lease-renew.sh start 9"), &sent);
    assert!(matches!(r, StopOutcome::Refused(_)));
    assert_eq!(run_stop("500", &[None], None, &sent), StopOutcome::NotRunning);
    assert!(sent.borrow().is_empty());
}

#[test]
fn stop_pid_never_signals_a_real_unrelated_process() {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let sent = RefCell::new(vec![]);
    let r = stop_pid(
        &child.id().to_string(),
        std::process::id(),
        &start_identity,
        &process_argv,
        &|p| sent.borrow_mut().push(p),
    );
    assert!(matches!(r, StopOutcome::Refused(_)), "{r:?}");
    assert!(sent.borrow().is_empty());
    assert!(process_argv(child.id())
        .unwrap()
        .contains(&"sleep".to_string()));
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn end_owners_matches_any_sweep_for_the_issue_but_never_a_peer() {
    let (_d, s) = store();
    claim(&s, &key("acme/widget", "h", "s1", 7), 100, "t1", &ident, &all_live).unwrap();
    claim(&s, &key("acme/widget", "h", "s2", 7), 101, "t2", &ident, &all_live).unwrap();
    claim(&s, &key("acme/widget", "h", "s1", 8), 102, "t3", &ident, &all_live).unwrap();
    claim(&s, &key("acme/other", "h", "s1", 7), 103, "t4", &ident, &all_live).unwrap();
    let sent = RefCell::new(vec![]);
    let n = end_owners(&s, "acme/widget", 7, None, None, &all_live, &|p| sent.borrow_mut().push(p))
        .unwrap();
    assert_eq!(n, 2);
    let mut got = sent.borrow().clone();
    got.sort_unstable();
    assert_eq!(got, vec![100, 101]);
    assert!(
        s.read(&key("acme/widget", "h", "s1", 7))
            .unwrap()
            .unwrap()
            .released
    );
    assert!(
        !s.read(&key("acme/widget", "h", "s1", 8))
            .unwrap()
            .unwrap()
            .released
    );
    assert!(
        !s.read(&key("acme/other", "h", "s1", 7))
            .unwrap()
            .unwrap()
            .released
    );
    // A sweep filter narrows it; a dead owner is tombstoned but not signalled.
    let sent = RefCell::new(vec![]);
    let n = end_owners(&s, "acme/widget", 8, None, Some("nope"), &all_live, &|p| {
        sent.borrow_mut().push(p)
    })
    .unwrap();
    assert_eq!((n, sent.borrow().len()), (0, 0));
    let n =
        end_owners(&s, "acme/widget", 8, None, None, &none_live, &|p| sent.borrow_mut().push(p))
            .unwrap();
    assert_eq!((n, sent.borrow().len()), (1, 0));
}
