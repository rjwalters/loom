//! More tests for the workspace resync (#10718): what a pass costs, how it
//! is bounded, the loop bound, and the guards. Same fixtures as the parent.

use super::super::git::{classify_rejection, Push};
use super::*;

// ----------------------------------------------------------------------------
// What a pass costs
// ----------------------------------------------------------------------------

fn current() -> Seed {
    Seed {
        version: RUNNING,
        current: true,
        ..STALE
    }
}

#[test]
fn an_unchanged_workspace_costs_its_line_in_the_head_query_and_nothing_else() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");

    // Startup: nothing is cached. The forge names the head, the clone already
    // has that commit, so nothing is fetched.
    let first = host.pass();
    assert_eq!(only(&first).state, WState::W0);
    assert_eq!(network(&first), (1, 0, 0));

    // Every tick after that asks again (#10987: there is no recheck window),
    // and the answer is the commit already evaluated: one request, no git
    // child on the network, no fetch, no second diff.
    for _ in 0..20 {
        host.advance(INTERVAL);
        let tick = host.pass();
        assert_eq!(only(&tick).state, WState::W0);
        assert_eq!(network(&tick), (1, 0, 0));
    }
    assert_eq!(host.heads.requests.get(), 21, "one head query per tick");
    assert_eq!(host.unpacked.get(), 1, "and the payload was diffed once in all");
    assert!(fx.forge.calls.borrow().is_empty());
}

#[test]
fn a_change_made_outside_this_daemon_is_classified_on_the_next_tick() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    assert_eq!(only(&host.pass()).state, WState::W0);

    // Someone undoes part of the install on the default branch.
    fx.push_from_seed("old script back", |seed| {
        write(&seed.join(".loom/scripts/a.sh"), "#!/bin/sh\necho old\n");
    });
    host.advance(INTERVAL);
    // A host that may not write asks nobody, so it still sees the old head.
    let seen = host.pass_with(Mode::Check, &|| Ok(()), None);
    assert_eq!((only(&seen).state, network(&seen)), (WState::W0, (0, 0, 0)), "check mode");
    let seen = host.pass_with(Mode::Write, &|| Err(NotCurrent::Draining), None);
    assert_eq!((only(&seen).state, network(&seen)), (WState::W0, (0, 0, 0)), "paused");
    // The very next pass that may write sees the new head in its one query,
    // fetches it once, and resyncs in the same pass.
    let seen = host.pass();
    assert_eq!(network(&seen), (1, 0, 1));
    assert_eq!(only(&seen).state, WState::W0, "resynced in the same pass: {seen:?}");
    assert_eq!(fx.origin_file(".loom/scripts/a.sh"), "#!/bin/sh\necho new");
}

#[test]
fn a_host_that_may_not_write_asks_no_remote_at_all() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // Any network call would fail: the clone's origin now points nowhere.
    git(&host.root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    // `fleet.autoApply` off.
    let check = host.pass_with(Mode::Check, &|| Ok(()), None);
    assert_eq!(only(&check).state, WState::W1, "classified from the clone's own ref");
    assert_eq!(network(&check), (0, 0, 0));
    // `paused` (the drain flag), and every other reason a host is not in H0.
    for why in [
        NotCurrent::Draining,
        NotCurrent::RollPending,
        NotCurrent::FloorBelow,
    ] {
        let pass = host.pass_with(Mode::Write, &|| Err(why), None);
        assert_eq!(only(&pass).state, WState::W1);
        assert_eq!(network(&pass), (0, 0, 0), "{why}");
        assert!(pass.alerts.is_empty());
    }
    assert_eq!(host.heads.requests.get(), 0, "the batched head query included");
    assert!(host.memory.borrow().backoff.is_empty(), "nothing failed: nothing was asked");
    assert!(fx.forge.calls.borrow().is_empty());
}

#[test]
fn a_pass_that_runs_out_of_time_stops_and_the_next_one_starts_where_it_stopped() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    let roots = [
        host.root.clone(),
        fx.clone_as("second"),
        fx.clone_as("third"),
    ];
    // The budget is spent after one workspace.
    host.budget.set(1);
    let first = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let states: Vec<WState> = first.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::Unknown, WState::Unknown]);
    assert_eq!(network(&first), (1, 0, 0), "one query names every head");
    assert_eq!(reason(&first.workspaces[1]), "not checked yet: the pass ran out of time");
    assert_eq!(first.workspaces[1].repo.as_deref(), Some(REPO), "still listed, in order");
    assert_eq!(host.memory.borrow().cursor, 1);

    // The next pass starts at the second workspace, so the third is not
    // starved by the first being rechecked.
    let second = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let states: Vec<WState> = second.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::W0, WState::Unknown]);
    assert_eq!(network(&second), (1, 0, 0));
    assert_eq!(host.memory.borrow().cursor, 2);
    let third = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let states: Vec<WState> = third.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::W0, WState::W0]);
    // A workspace the budget cut off keeps the verdict it already had.
    let fourth = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let states: Vec<WState> = fourth.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::W0, WState::W0]);
    assert_eq!(network(&fourth), (1, 0, 0));
}

#[test]
fn unreachable_remotes_are_one_alert_for_the_host_not_one_per_repo() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let roots = [
        host.root.clone(),
        fx.clone_as("second"),
        fx.clone_as("third"),
        fx.clone_as("fourth"),
    ];
    for root in &roots {
        git(root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    }
    let mut alerts = Vec::new();
    let mut probes = Vec::new();
    for _ in 0..4 {
        let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
        // Still reported from what each clone holds.
        assert!(pass.workspaces.iter().all(|w| w.state == WState::W1), "{pass:?}");
        probes.push(pass.probes);
        alerts.extend(pass.alerts.iter().map(|a| (a.kind, a.repo.clone())));
        // Past every hold and backoff an outage can set.
        host.advance(Duration::from_secs(7 * 60 * 60));
    }
    // The head query got no answer, so the remotes were asked directly, all
    // four at once. Three in a row that do not answer end the pass's asking.
    assert_eq!(probes, vec![4, 4, 4, 4]);
    assert_eq!(
        alerts,
        vec![("network", None)],
        "one alert, for the host, on the third pass; the failed query is part of the outage"
    );
    assert!(fx.forge.calls.borrow().is_empty(), "no claim during an outage");

    // While no remote answers, the host stays off the network for a while.
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    git(&host.root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    assert_eq!(network(&host.pass()), (1, 1, 0));
    host.advance(Duration::from_secs(30));
    assert_eq!(network(&host.pass()), (0, 0, 0), "inside the hold");
}

// ----------------------------------------------------------------------------
// The loop bound
// ----------------------------------------------------------------------------

/// A payload that differs from the fixture's in one file: what a host built
/// from another tree would embed under the SAME version.
fn other_defaults(fx: &Fixture) -> PathBuf {
    let d = defaults(&fx.tmp.path().canonicalize().unwrap().join("other"), true);
    write(&d.join("scripts/a.sh"), "#!/bin/sh\necho from a feature branch\n");
    d
}

#[test]
fn two_hosts_on_one_version_with_different_payloads_cannot_ping_pong() {
    let fx = Fixture::new(STALE);
    let a = Host::new(&fx, "host-a");
    let b = Host::running(&fx, "host-b", RUNNING, other_defaults(&fx));
    let commits = fx.origin_commits();

    // host-a resyncs the repo to v0.19.880.
    assert_eq!(only(&a.pass()).state, WState::W0);
    assert_eq!(fx.origin_commits(), commits + 1);
    let calls = fx.forge.calls.borrow().len();

    // host-b runs the same version with other files, so to it the repo is
    // stale. Without a bound it would push its files, host-a would push its
    // own back, and so on: one commit per host per tick.
    for tick in 0..3 {
        let pass = b.pass();
        let report = only(&pass);
        assert_eq!(report.state, WState::W1, "{report:?}");
        assert!(
            reason(report).starts_with("resync-loop: host-a already resynced it to v0.19.880 in "),
            "{report:?}"
        );
        // Alerted once, then only reported.
        let kinds: Vec<&str> = pass.alerts.iter().map(|x| x.kind).collect();
        assert_eq!(
            kinds,
            if tick == 0 {
                vec!["resync-loop"]
            } else {
                vec![]
            }
        );
        b.advance(INTERVAL);
    }
    assert_eq!(fx.origin_commits(), commits + 1, "host-b pushed nothing");
    assert_eq!(fx.origin_file(".loom/scripts/a.sh"), "#!/bin/sh\necho new");
    assert_eq!(fx.forge.calls.borrow().len(), calls, "and never asked for the claim");
    assert!(b.memory.borrow().backoff.is_empty(), "a refusal is not a failure");
    assert!(b.resync_worktrees().is_empty());

    // The other direction: whatever makes the repo stale again for host-a
    // (here a person putting host-b's file in place), host-a does not resync
    // it a second time at this version.
    fx.push_from_seed("the other payload, by hand", |seed| {
        write(&seed.join(".loom/scripts/a.sh"), "#!/bin/sh\necho from a feature branch\n");
    });
    // Seen on host-a's very next tick: there is no recheck window.
    a.advance(INTERVAL);
    let pass = a.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert!(reason(report).starts_with("resync-loop: "), "{report:?}");
    assert_eq!(pass.alerts.len(), 1);
    assert_eq!(pass.alerts[0].kind, "resync-loop");
    assert_eq!(fx.origin_commits(), commits + 2, "the person's commit and no other");
    // To host-b the repo now matches its own payload: nothing to do.
    b.advance(INTERVAL);
    assert_eq!(only(&b.pass()).state, WState::W0);
    assert_eq!(fx.origin_commits(), commits + 2);
    assert_eq!(fx.forge.calls.borrow().len(), calls);
}

#[test]
fn a_second_resync_at_one_version_is_refused_under_the_claim_too() {
    // host-b classifies the repo as stale before host-a's resync lands, so
    // the bound cannot stop it before the claim. It stops it before the push.
    let fx = Fixture::new(STALE);
    let a = Host::new(&fx, "host-a");
    let b = Host::running(&fx, "host-b", RUNNING, other_defaults(&fx));
    let commits = fx.origin_commits();
    *b.before_claim.borrow_mut() = Some(Box::new(|| {
        assert_eq!(only(&a.pass()).state, WState::W0);
    }));
    let pass = b.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert!(
        reason(report).starts_with("resync-loop: host-a already resynced it"),
        "{report:?}"
    );
    assert_eq!(pass.alerts.len(), 1);
    assert_eq!(fx.origin_commits(), commits + 1, "host-a's commit only");
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "the claim is released");
    assert!(b.resync_worktrees().is_empty());
}

#[test]
fn a_newer_version_is_installed_at_once_and_each_version_lands_once() {
    // #10987: there is no wait between two resyncs. The bound is one commit
    // per version, and a host never installs a version older than the repo's.
    let fx = Fixture::new(STALE);
    let a = Host::new(&fx, "host-a");
    assert_eq!(only(&a.pass()).state, WState::W0);
    let commits = fx.origin_commits();

    // A newer release, with a file changed, lands on host-b a moment later.
    let newer = defaults(&fx.tmp.path().canonicalize().unwrap().join("newer"), true);
    write(&newer.join("docs/d.md"), "doc, revised\n");
    let b = Host::running(&fx, "host-b", "0.19.881", newer);
    let pass = b.pass();
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
    assert!(pass.alerts.is_empty());
    assert_eq!(fx.origin_commits(), commits + 1, "installed on its first tick");
    assert_eq!(fx.origin_file(".loom/docs/d.md"), "doc, revised");

    // host-a, still on the older version, sees that on its next tick and
    // leaves it alone: it never downgrades, so the two cannot alternate.
    for _ in 0..3 {
        a.advance(INTERVAL);
        b.advance(INTERVAL);
        let pass = a.pass();
        let report = only(&pass);
        assert_eq!(report.state, WState::RepoAhead, "{report:?}");
        assert_eq!(reason(report), "installed 0.19.881 > running 0.19.880");
        assert_eq!(only(&b.pass()).state, WState::W0);
        assert!(pass.alerts.is_empty());
    }
    assert_eq!(fx.origin_commits(), commits + 1, "one commit per version, and no more");
}

// ----------------------------------------------------------------------------
// Gates and guards
// ----------------------------------------------------------------------------

#[test]
fn the_gate_is_read_again_immediately_before_the_claim() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    // The host is paused while the pass is classifying.
    let calls = Cell::new(0);
    let pass = host.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() >= AT_CLAIM {
                Err(NotCurrent::Draining)
            } else {
                Ok(())
            }
        },
        None,
    );
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert_eq!(reason(report), "host not H0: draining before the claim");
    assert_eq!(calls.get(), AT_CLAIM);
    assert!(fx.forge.calls.borrow().is_empty(), "no claim was asked for");
    assert_eq!(fx.origin_head(), before);
    assert!(pass.alerts.is_empty());
    assert!(host.memory.borrow().backoff.is_empty());
}

#[test]
fn a_panic_mid_resync_removes_the_worktree_and_releases_the_claim() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    let calls = Cell::new(0);
    let seen = RefCell::new((Vec::new(), None));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host.pass_with(
            Mode::Write,
            &|| {
                calls.set(calls.get() + 1);
                if calls.get() == AT_PUSH {
                    // The resync is in its worktree, under the claim.
                    *seen.borrow_mut() = (host.resync_worktrees(), fx.forge.ref_sha(CLAIM_REF));
                    panic!("a defect in the middle of W2");
                }
                Ok(())
            },
            None,
        )
    }));
    assert!(outcome.is_err(), "the panic reaches the caller");
    let (worktrees, claim) = seen.into_inner();
    assert_eq!(worktrees.len(), 1, "fixture: the panic was inside the worktree");
    assert!(claim.is_some(), "fixture: and under the claim");

    assert!(host.resync_worktrees().is_empty(), "the worktree is removed");
    assert_eq!(git(&host.root, &["worktree", "list"]).lines().count(), 1);
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "the claim is released");
    assert_eq!(fx.origin_head(), before, "nothing was pushed");
    // The next pass is unaffected.
    assert_eq!(only(&host.pass()).state, WState::W0);
}

#[test]
fn a_default_branch_name_git_would_read_as_an_option_is_not_used() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // The remote chooses the default branch's name.
    fs::write(
        host.root.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/--upload-pack=touch${IFS}pwned\n",
    )
    .unwrap();
    assert_eq!(super::super::git::default_branch(&host.root).as_deref(), Some("main"));
    let pass = host.pass();
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
}

#[test]
fn a_failed_push_is_read_as_moved_protected_or_neither() {
    let moved = " ! [rejected]        HEAD -> main (fetch first)\nerror: failed to push some refs";
    assert_eq!(classify_rejection(moved), Some(Push::NonFastForward));
    let stale = " ! [rejected]        HEAD -> main (non-fast-forward)";
    assert_eq!(classify_rejection(stale), Some(Push::NonFastForward));
    let raced = " ! [remote rejected] HEAD -> main (cannot lock ref 'refs/heads/main')";
    assert_eq!(classify_rejection(raced), Some(Push::NonFastForward));
    let ruleset = "remote: error: GH013: Repository rule violations found for refs/heads/main.\n \
                   ! [remote rejected] HEAD -> main (push declined due to repository rule violations)";
    assert_eq!(
        classify_rejection(ruleset),
        Some(Push::Protected(
            "error: GH013: Repository rule violations found for refs/heads/main.".to_string()
        ))
    );
    let hook = " ! [remote rejected] HEAD -> main (pre-receive hook declined)";
    assert_eq!(classify_rejection(hook), Some(Push::Protected(hook.trim().to_string())));
    assert_eq!(
        classify_rejection("fatal: unable to access 'https://…': Could not resolve host"),
        None
    );
}

#[test]
fn only_one_pass_runs_at_a_time() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let slot = AtomicBool::new(false);
    let first = super::super::host::begin(&slot).expect("the slot is free");
    assert!(super::super::host::begin(&slot).is_none(), "a second pass does not start");
    assert!(
        super::super::host::begin(&slot).is_none(),
        "and a refusal does not free the slot"
    );
    drop(first);
    assert!(!slot.load(Ordering::Acquire));
    // A pass that panics frees the slot on the way out.
    let unwound = std::panic::catch_unwind(|| {
        let _flight = super::super::host::begin(&slot).expect("free again");
        panic!("a pass that dies");
    });
    assert!(unwound.is_err());
    assert!(super::super::host::begin(&slot).is_some(), "the next tick's pass starts");
}
