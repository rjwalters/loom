//! A newer resync closes the older daemon resync pull requests (#13488).

use serde_json::json;

use super::*;

/// Put an open PR in the fake forge, as if a host had opened it.
fn open_pr(fx: &Fixture, number: u64, title: &str, head: &str, body: &str) {
    fx.forge.pulls.borrow_mut().push((
        head.to_string(),
        json!({"number": number, "title": title, "body": body, "head": {"ref": head}}),
    ));
    fx.forge
        .set_ref(&format!("refs/heads/{head}"), &"a".repeat(40));
}

/// A resync PR exactly as the daemon opens one for `version`.
fn resync_pr(fx: &Fixture, number: u64, version: &str) {
    let message = git::resync_message("host-z", version);
    let (title, body) = message.split_once("\n\n").unwrap();
    open_pr(fx, number, title, &git::resync_branch(version), body);
}

fn comments(fx: &Fixture) -> Vec<(u64, String)> {
    fx.forge.comments.borrow().clone()
}

#[test]
fn opening_the_newer_resync_pull_request_closes_the_older_one() {
    let fx = Fixture::new(STALE);
    refuse_main_only(&fx);
    resync_pr(&fx, 7, "0.19.879");
    let host = Host::new(&fx, "host-a");

    let pass = host.pass();
    assert!(reason(only(&pass)).contains("resync pull request open: #2 "), "{pass:?}");
    assert_eq!(*fx.forge.closed.borrow(), vec![7]);
    let said = comments(&fx);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, 7);
    assert!(said[0].1.contains(&format!("#2 (v{RUNNING})")), "{said:?}");
    assert_eq!(fx.forge.ref_sha("refs/heads/chore/loom-resync-v0.19.879"), None);
    assert_eq!(fx.forge.pulls.borrow().len(), 1, "only the newest stays open");
}

#[test]
fn an_older_pull_request_is_closed_when_a_later_pass_finds_the_newest_open() {
    let fx = Fixture::new(STALE);
    refuse_main_only(&fx);
    let host = Host::new(&fx, "host-a");
    assert_eq!(only(&host.pass()).state, WState::W1);
    resync_pr(&fx, 9, "0.19.700");
    host.now.set(t0() + ChronoDuration::hours(7));
    host.pass();
    assert_eq!(*fx.forge.closed.borrow(), vec![9]);
}

#[test]
fn a_checkout_already_at_the_version_closes_the_resync_pull_request() {
    let fx = Fixture::new(STALE);
    resync_pr(&fx, 4, RUNNING);
    let host = Host::new(&fx, "host-a");
    // The resync lands between classification and the re-read under the claim.
    *host.before_claim.borrow_mut() = Some(Box::new(|| fx.resync_from_seed()));
    let pass = host.pass();
    assert_eq!(reason(only(&pass)), "already current once the claim was held; no commit");
    assert_eq!(*fx.forge.closed.borrow(), vec![4]);
    let said = comments(&fx);
    assert!(said[0].1.contains(&format!("already v{RUNNING}")), "{said:?}");
    assert_eq!(
        fx.forge
            .ref_sha(&format!("refs/heads/chore/loom-resync-v{RUNNING}")),
        None
    );
}

#[test]
fn a_pull_request_for_a_newer_version_than_the_running_one_is_left_alone() {
    let fx = Fixture::new(STALE);
    refuse_main_only(&fx);
    resync_pr(&fx, 5, "0.19.881");
    let host = Host::new(&fx, "host-a");
    host.pass();
    assert!(fx.forge.closed.borrow().is_empty());
    assert!(comments(&fx).is_empty());
    assert!(fx
        .forge
        .ref_sha("refs/heads/chore/loom-resync-v0.19.881")
        .is_some());
}

#[test]
fn pull_requests_that_are_not_this_daemons_resync_are_left_alone() {
    let fx = Fixture::new(STALE);
    refuse_main_only(&fx);
    let trailer = "Loom-Resync-Version: 0.19.870\n";
    let title = "chore(loom): resync installed Loom to v0.19.870";
    // A person's PR with the title but not the branch, the branch but not the
    // trailer, and an unrelated one.
    open_pr(&fx, 11, title, "somebody/resync", trailer);
    open_pr(&fx, 12, title, "chore/loom-resync-v0.19.870", "no trailer here");
    open_pr(&fx, 13, "docs: unrelated", "chore/loom-resync-v0.19.870x", trailer);
    open_pr(
        &fx,
        14,
        "chore(loom): resync installed Loom to vnext",
        "chore/loom-resync-vnext",
        trailer,
    );
    let host = Host::new(&fx, "host-a");
    host.pass();
    assert!(fx.forge.closed.borrow().is_empty(), "{:?}", fx.forge.closed);
    assert!(comments(&fx).is_empty());
}

#[test]
fn a_failed_close_is_logged_and_does_not_change_the_pass() {
    let fx = Fixture::new(STALE);
    refuse_main_only(&fx);
    resync_pr(&fx, 7, "0.19.879");
    fx.forge
        .fault("PATCH repos/acme/app/pulls/7", Fault::Status(500));
    let host = Host::new(&fx, "host-a");
    let mut pass = None;
    let logs = crate::test_log_capture::capture_logs(|| pass = Some(host.pass()));
    let pass = pass.unwrap();
    assert_eq!(only(&pass).state, WState::W1, "{pass:?}");
    assert!(reason(only(&pass)).contains("resync pull request open: #2 "));
    assert!(fx.forge.closed.borrow().is_empty());
    assert!(
        logs.iter()
            .any(|l| l.0 == log::Level::Warn && l.1.contains("could not close superseded")),
        "{logs:?}"
    );
    // The branch stays with its still-open PR.
    assert!(fx
        .forge
        .ref_sha("refs/heads/chore/loom-resync-v0.19.879")
        .is_some());
}
