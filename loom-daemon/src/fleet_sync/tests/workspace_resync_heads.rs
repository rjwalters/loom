//! Tests for the workspace resync's head check (#10987): one batched query
//! per tick, the `git ls-remote` fallback, the rate-limit breaker, one repo's
//! failure kept apart from the host's network, and a pass that does not end.
//!
//! The forge's side of the query is [`ForgeHeads`], in memory: it answers
//! from the temp `origin` each clone points at. No network, no `gh`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;

use super::super::git::is_refusal;
use super::super::heads::{
    batches, gather, parse, query, Answer, Fault as HeadFault, HeadAsk, Heads, CHUNK,
};
use super::super::host::{begin, note_refused, supervise, Ended};
use super::*;

// ----------------------------------------------------------------------------
// The in-memory forge
// ----------------------------------------------------------------------------

/// The forge's batched head query, as one host sees it.
#[derive(Default)]
pub(super) struct ForgeHeads {
    /// Requests this host has sent.
    pub(super) requests: Cell<u32>,
    /// How many repos each request asked about.
    pub(super) asked: RefCell<Vec<usize>>,
    /// The query fails as a whole (the API is down, or `gh` is missing).
    pub(super) down: Cell<bool>,
    /// The rate-limit breaker is open: nothing is sent.
    pub(super) breaker: Cell<bool>,
    /// The breaker opens on this very query (it was rate limited).
    pub(super) trip_after_query: Cell<bool>,
    /// Repos the forge has no head for, by root.
    pub(super) faults: RefCell<HashMap<PathBuf, HeadFault>>,
    /// Repos whose default branch the forge calls something else, by root.
    pub(super) renamed: RefCell<HashMap<PathBuf, String>>,
}

impl ForgeHeads {
    /// Answer one pass's asks: every head in one request.
    pub(super) fn answer(&self, asks: &[HeadAsk]) -> Heads {
        let mut heads = Heads::default();
        if self.breaker.get() {
            heads.breaker_open = true;
            return heads;
        }
        self.requests.set(self.requests.get() + 1);
        self.asked.borrow_mut().push(asks.len());
        heads.requests = 1;
        heads.breaker_open = self.trip_after_query.get();
        if self.down.get() {
            heads.failures.push("acme: HTTP 502".to_string());
            return heads;
        }
        for ask in asks {
            assert_eq!(ask.nwo, REPO);
            if let Some(fault) = self.faults.borrow().get(&ask.root) {
                heads
                    .answers
                    .insert(ask.root.clone(), Answer::Fault(fault.clone()));
                continue;
            }
            // The forge is the bare repo the clone's origin names. An origin
            // that is not there stands for a forge that cannot be reached.
            let origin = PathBuf::from(git(&ask.root, &["remote", "get-url", "origin"]));
            if !origin.is_dir() {
                heads.failures.push("acme: no route to host".to_string());
                continue;
            }
            let branch = self
                .renamed
                .borrow()
                .get(&ask.root)
                .cloned()
                .unwrap_or_else(|| "main".to_string());
            let commit = git(&origin, &["rev-parse", "refs/heads/main"]);
            heads
                .answers
                .insert(ask.root.clone(), Answer::At { branch, commit });
        }
        heads
    }
}

// ----------------------------------------------------------------------------
// One request per tick
// ----------------------------------------------------------------------------

#[test]
fn a_settled_fleet_costs_one_request_per_tick_and_no_fetch() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    let roots = [
        host.root.clone(),
        fx.clone_as("second"),
        fx.clone_as("third"),
        fx.clone_as("fourth"),
    ];
    for tick in 0..10 {
        let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
        assert!(pass.workspaces.iter().all(|w| w.state == WState::W0), "{pass:?}");
        assert_eq!(network(&pass), (1, 0, 0), "tick {tick}");
        host.advance(INTERVAL);
    }
    assert_eq!(*host.heads.asked.borrow(), vec![4; 10], "every repo, in the one request");
    assert!(fx.forge.calls.borrow().is_empty());
}

fn current() -> Seed {
    Seed {
        version: RUNNING,
        current: true,
        ..STALE
    }
}

#[test]
fn a_stale_repo_this_host_cannot_write_costs_only_its_line_in_the_query() {
    // It stays stale for as long as nobody else resyncs it, and is looked at
    // every tick. That is one more alias in the one request, nothing else.
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    write(&host.root.join(".read-only-credential"), "");
    for _ in 0..10 {
        let pass = host.pass();
        assert_eq!(only(&pass).state, WState::W1);
        assert_eq!(network(&pass), (1, 0, 0));
        host.advance(INTERVAL);
    }
    assert!(fx.forge.calls.borrow().is_empty());
    assert!(host.memory.borrow().backoff.is_empty());
}

#[test]
fn a_default_branch_the_forge_names_differently_is_asked_about_directly() {
    // The clone follows `main`; the forge's default branch has been renamed.
    // The forge's head is another branch's, so it is not used.
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    host.heads
        .renamed
        .borrow_mut()
        .insert(host.root.clone(), "trunk".to_string());
    let pass = host.pass();
    assert_eq!(only(&pass).state, WState::W0);
    assert_eq!(network(&pass), (1, 1, 0), "one ls-remote of the branch the clone follows");
}

// ----------------------------------------------------------------------------
// The fallback and the breaker
// ----------------------------------------------------------------------------

#[test]
fn a_failed_head_query_falls_back_to_ls_remote_and_alerts_once() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    let roots = [host.root.clone(), fx.clone_as("second")];
    host.heads.down.set(true);
    let mut alerts = Vec::new();
    for tick in 0..5 {
        let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
        assert!(pass.workspaces.iter().all(|w| w.state == WState::W0), "{pass:?}");
        assert_eq!(network(&pass), (1, 2, 0), "tick {tick}: each remote is asked instead");
        alerts.push(pass.alerts.iter().map(|a| a.kind).collect::<Vec<_>>());
        host.advance(INTERVAL);
    }
    // Not an outage: the remotes answer. One alert for the query, on the
    // third failing pass, and no hold off the network.
    assert_eq!(alerts, vec![vec![], vec![], vec!["head-query"], vec![], vec![]]);
    assert!(host.memory.borrow().outage_hold(host.now.get()).is_none());
    assert!(host.memory.borrow().backoff.is_empty());

    // A change is still seen on the next tick, through the fallback.
    fx.push_from_seed("old script back", |seed| {
        write(&seed.join(".loom/scripts/a.sh"), "#!/bin/sh\necho old\n");
    });
    let pass = host.pass();
    assert_eq!(network(&pass), (1, 1, 1));
    assert_eq!(only(&pass).state, WState::W0, "resynced: {pass:?}");

    // The query works again: back to one request, and a later failure is a
    // new run with its own alert.
    host.heads.down.set(false);
    host.advance(INTERVAL);
    assert_eq!(network(&host.pass()), (1, 0, 0));
    host.heads.down.set(true);
    let kinds: Vec<usize> = (0..3).map(|_| host.pass().alerts.len()).collect();
    assert_eq!(kinds, vec![0, 0, 1]);
}

#[test]
fn an_open_breaker_skips_the_check_that_tick_and_says_so() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    host.heads.breaker.set(true);
    for _ in 0..3 {
        let pass = host.pass();
        assert_eq!(
            pass.host.as_deref(),
            Some(
                "the forge rate-limit breaker is open, so no default-branch head was checked \
                 this tick"
            )
        );
        // Nothing is sent, nothing falls back to git, nothing is claimed.
        assert_eq!(network(&pass), (0, 0, 0));
        assert_eq!(only(&pass).state, WState::W1, "still reported, from the clone");
        assert!(pass.alerts.is_empty());
        host.advance(INTERVAL);
    }
    assert_eq!(host.heads.requests.get(), 0);
    assert!(fx.forge.calls.borrow().is_empty());
    assert_eq!(fx.origin_head(), before);
    assert!(host.memory.borrow().backoff.is_empty(), "waiting is not a failure");

    // It closes: the next tick checks and resyncs.
    host.heads.breaker.set(false);
    let pass = host.pass();
    assert_eq!(pass.host, None);
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
}

// ----------------------------------------------------------------------------
// One repo's failure is not the host's
// ----------------------------------------------------------------------------

fn gone() -> HeadFault {
    HeadFault::NotFound("Could not resolve to a Repository with the name 'acme/app'.".into())
}

#[test]
fn a_dead_repo_is_its_own_failure_and_never_a_network_outage() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    let dead = fx.clone_as("deleted-upstream");
    // The repo was deleted: the forge has none, and its remote gives nothing.
    host.heads.faults.borrow_mut().insert(dead.clone(), gone());
    git(&dead, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    let roots = [dead.clone(), host.root.clone()];

    let mut alerts = Vec::new();
    for _ in 0..4 {
        let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
        let (bad, good) = (&pass.workspaces[0], &pass.workspaces[1]);
        assert_eq!(good.state, WState::W0, "the healthy repo is unaffected: {pass:?}");
        assert!(reason(bad).contains("the forge has no such repository"), "{bad:?}");
        assert_eq!(bad.state, WState::W0, "still reported, from what its clone holds");
        alerts.extend(pass.alerts.iter().map(|a| (a.kind, a.root.clone())));
        // Never the host's outage: no hold, and the next tick asks again.
        assert!(host.memory.borrow().outage_hold(host.now.get()).is_none());
        // Past the repo's own backoff.
        host.advance(Duration::from_secs(60 * 60));
    }
    assert_eq!(
        alerts,
        vec![("repo-access", dead.clone()), ("repo-access", dead.clone())],
        "its own alert, from the third failure, and no `network` alert"
    );

    // While it backs off it is not asked about at all; the other repo is.
    let waiting = fx.clone_as("waiting");
    host.heads
        .faults
        .borrow_mut()
        .insert(waiting.clone(), gone());
    git(&waiting, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    let roots = [waiting, host.root.clone()];
    assert_eq!(network(&host.pass_over(&roots, Mode::Write, &|| Ok(()), None)), (1, 1, 0));
    host.advance(Duration::from_secs(30));
    let next = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    assert_eq!(network(&next), (1, 0, 0));
    assert_eq!(host.heads.asked.borrow().last(), Some(&1));
}

#[test]
fn a_host_whose_only_repo_is_dead_is_not_held_off_the_network() {
    let fx = Fixture::new(current());
    let host = Host::new(&fx, "host-a");
    host.heads
        .faults
        .borrow_mut()
        .insert(host.root.clone(), gone());
    git(&host.root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    let pass = host.pass();
    assert_eq!(network(&pass), (1, 1, 0));
    assert!(pass.alerts.is_empty());
    assert!(host.memory.borrow().outage_hold(host.now.get()).is_none());
    assert_eq!(host.memory.borrow().backoff[&host.root].failures, 1);
}

#[test]
fn a_repo_the_query_cannot_see_is_read_from_its_own_remote() {
    // The query's credential does not cover the repo; the repo's own does.
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    host.heads
        .faults
        .borrow_mut()
        .insert(host.root.clone(), gone());
    let pass = host.pass();
    assert_eq!(network(&pass), (1, 1, 0));
    assert_eq!(only(&pass).state, WState::W0, "resynced as usual: {pass:?}");
    assert!(pass.alerts.is_empty());
    assert!(host.memory.borrow().backoff.is_empty());
}

#[test]
fn git_refusals_are_told_apart_from_a_remote_that_does_not_answer() {
    for refused in [
        "remote: Repository not found.\nfatal: repository 'https://github.com/a/b.git/' not found",
        "ERROR: Repository not found.\nfatal: Could not read from remote repository.",
        "fatal: Authentication failed for 'https://github.com/a/b.git/'",
        "remote: Invalid username or token. Password authentication is not supported",
        "remote: Permission to a/b.git denied to someone.",
        "fatal: unable to access 'https://github.com/a/b/': The requested URL returned error: 403",
    ] {
        assert!(is_refusal(refused), "{refused}");
    }
    for silent in [
        "fatal: unable to access 'https://github.com/a/b/': Could not resolve host: github.com",
        "fatal: unable to access 'https://github.com/a/b/': Failed to connect to github.com",
        "ssh: connect to host github.com port 22: Operation timed out",
        "fatal: '/nonexistent/origin.git' does not appear to be a git repository",
        "",
        // The host's credential helper, not this repo.
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
    ] {
        assert!(!is_refusal(silent), "{silent}");
    }
    assert!(super::super::git::is_credential_failure(
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
    ));
}

// ----------------------------------------------------------------------------
// The query itself
// ----------------------------------------------------------------------------

#[test]
fn the_query_has_one_alias_per_repo_and_quotes_only_safe_names() {
    assert_eq!(
        query(&["acme/app", "acme/lib.rs"]),
        "query { r0: repository(owner: \"acme\", name: \"app\") { isArchived \
         defaultBranchRef { name target { oid } } } r1: repository(owner: \"acme\", \
         name: \"lib.rs\") { isArchived defaultBranchRef { name target { oid } } } }"
    );
    // A slug that could break out of the string literal gets no alias (and
    // so no answer); the aliases of the others keep their positions.
    let odd = query(&["acme/a\") { x }", "no-slash", "acme/ok"]);
    assert!(!odd.contains("r0:") && !odd.contains("r1:"), "{odd}");
    assert!(odd.contains("r2: repository(owner: \"acme\", name: \"ok\")"), "{odd}");
}

#[test]
fn an_answer_is_read_per_alias() {
    let oid = "a".repeat(40);
    let body = format!(
        r#"{{"data":{{
            "r0":{{"defaultBranchRef":{{"name":"main","target":{{"oid":"{oid}"}}}}}},
            "r1":null,
            "r2":null,
            "r3":{{"defaultBranchRef":null}},
            "r4":null,
            "r5":{{"defaultBranchRef":{{"name":"main","target":{{"oid":"not-a-sha"}}}}}}
          }},
          "errors":[
            {{"type":"NOT_FOUND","path":["r1"],"message":"Could not resolve to a Repository"}},
            {{"type":"FORBIDDEN","path":["r2"],"message":"Resource not accessible"}},
            {{"type":"INTERNAL","path":["r4"],"message":"Something went wrong"}}
          ]}}"#
    );
    let found = parse(&body, 7).expect("the answer has data");
    assert_eq!(
        found,
        vec![
            Some(Answer::At {
                branch: "main".to_string(),
                commit: oid,
            }),
            Some(Answer::Fault(HeadFault::NotFound(
                "Could not resolve to a Repository".to_string()
            ))),
            Some(Answer::Fault(HeadFault::Forbidden("Resource not accessible".to_string()))),
            Some(Answer::Fault(HeadFault::NoDefaultBranch)),
            None, // an error that says nothing about the repo: unanswered
            None, // not a commit id: unanswered
            None, // not in the answer at all
        ]
    );
    // No `data`: the whole query failed (a rate limit, bad credentials).
    assert_eq!(parse(r#"{"message":"API rate limit exceeded"}"#, 1), None);
    assert_eq!(parse(r#"{"data":null,"errors":[{"message":"x"}]}"#, 1), None);
    assert_eq!(parse("", 1), None);
}

fn asks(repos: &[&str]) -> Vec<HeadAsk> {
    repos
        .iter()
        .map(|nwo| HeadAsk {
            root: PathBuf::from(format!("/src/{nwo}")),
            nwo: (*nwo).to_string(),
        })
        .collect()
}

/// A response body naming `oid` as the head of every one of `count` repos.
fn all_at(count: usize, oid: &str) -> String {
    let fields: Vec<String> = (0..count)
        .map(|i| {
            format!(r#""r{i}":{{"defaultBranchRef":{{"name":"main","target":{{"oid":"{oid}"}}}}}}"#)
        })
        .collect();
    format!(r#"{{"data":{{{}}}}}"#, fields.join(","))
}

#[test]
fn git_default_branch_heads_is_one_query_per_owner_chunked_past_a_hundred_repos() {
    // Two owners, as on the fleet; one of them spelled two ways, and one repo
    // cloned twice.
    let mut repos: Vec<String> = (0..CHUNK + 30)
        .map(|i| format!("2AMLogic/r{i:03}"))
        .collect();
    repos.extend(["rjwalters/loom", "RJWalters/repo", "rjwalters/loom"].map(String::from));
    let names: Vec<&str> = repos.iter().map(String::as_str).collect();
    let mut asked = asks(&names);
    asked.last_mut().unwrap().root = PathBuf::from("/src/a-second-clone-of-loom");
    let plan = batches(&asked);
    let shape: Vec<(&str, usize)> = plan
        .iter()
        .map(|b| (b.owner.as_str(), b.repos.len()))
        .collect();
    assert_eq!(shape, vec![("2amlogic", CHUNK), ("2amlogic", 30), ("rjwalters", 2)]);
    // Each query runs under a root of its own owner: that picks the credential.
    assert!(plan
        .iter()
        .all(|b| b.under.nwo.to_lowercase().starts_with(&b.owner)));

    // 60 repos of one owner: one request, and every root gets its answer,
    // both clones of the repo that is cloned twice included.
    let oid = "b".repeat(40);
    let sent = Cell::new(0);
    let heads = gather(&asked, &|| false, &|| false, &|batch| {
        sent.set(sent.get() + 1);
        Ok(all_at(batch.repos.len(), &oid))
    });
    assert_eq!((heads.requests, sent.get()), (3, 3));
    assert_eq!(heads.answers.len(), asked.len());
    assert!(heads.failures.is_empty() && !heads.breaker_open);
    let one_owner = asks(&names[..60]);
    assert_eq!(
        gather(&one_owner, &|| false, &|| false, &|b| Ok(all_at(b.repos.len(), &oid))).requests,
        1
    );
}

#[test]
fn one_owners_failed_query_leaves_the_other_owners_answers() {
    let asked = asks(&["2AMLogic/a", "2AMLogic/b", "rjwalters/loom"]);
    let oid = "c".repeat(40);
    let heads = gather(&asked, &|| false, &|| false, &|batch| {
        if batch.owner == "2amlogic" {
            anyhow::bail!("the GraphQL query got no answer: HTTP 502")
        }
        Ok(all_at(batch.repos.len(), &oid))
    });
    assert_eq!(heads.requests, 2);
    assert_eq!(heads.failures, vec!["2amlogic: the GraphQL query got no answer: HTTP 502"]);
    assert_eq!(heads.answers.len(), 1, "the failed owner's repos fall back to ls-remote");
    assert!(heads
        .answers
        .contains_key(&PathBuf::from("/src/rjwalters/loom")));
    // A body with no `data` (a rate-limit message) is a failure too.
    let heads = gather(&asked, &|| false, &|| false, &|_| {
        Ok(r#"{"message":"rate limited"}"#.to_string())
    });
    assert_eq!((heads.answers.len(), heads.failures.len()), (0, 2));
}

#[test]
fn nothing_is_sent_while_the_breaker_is_open() {
    let asked = asks(&["2AMLogic/a", "rjwalters/loom"]);
    let sent = Cell::new(0);
    let send = |batch: &super::super::heads::Batch<'_>| {
        sent.set(sent.get() + 1);
        Ok(all_at(batch.repos.len(), &"d".repeat(40)))
    };
    let heads = gather(&asked, &|| true, &|| false, &send);
    assert!(heads.breaker_open);
    assert_eq!((heads.requests, sent.get()), (0, 0));
    // It opens after the first request (which was refused): the second is
    // not sent.
    let heads = gather(&asked, &|| sent.get() > 0, &|| false, &send);
    assert!(heads.breaker_open);
    assert_eq!((heads.requests, sent.get()), (1, 1));
    // The breaker opens on the last request itself (it was rate limited):
    // the pass reports it, so the caller neither falls back nor claims.
    sent.set(0);
    let one = asks(&["2AMLogic/a"]);
    let heads = gather(&one, &|| sent.get() > 0, &|| false, &send);
    assert!(heads.breaker_open, "re-checked after the last request");
    assert_eq!((heads.requests, sent.get()), (1, 1));
    // No repos, no request.
    assert_eq!(gather(&[], &|| false, &|| false, &send).requests, 0);
}

#[test]
fn the_budget_is_checked_between_owners() {
    let asked = asks(&["2AMLogic/a", "rjwalters/loom", "zed/z"]);
    let sent = Cell::new(0);
    let send = |batch: &super::super::heads::Batch<'_>| {
        sent.set(sent.get() + 1);
        Ok(all_at(batch.repos.len(), &"e".repeat(40)))
    };
    // Spent after the first owner: the others are left to ls-remote.
    let heads = gather(&asked, &|| false, &|| sent.get() >= 1, &send);
    assert!(heads.out_of_budget && !heads.breaker_open);
    assert_eq!((heads.requests, heads.answers.len()), (1, 1));
}

#[test]
fn a_breaker_that_opens_on_the_passes_last_query_means_no_fallback_and_no_claim() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    host.heads.trip_after_query.set(true);
    let pass = host.pass();
    assert_eq!(network(&pass), (1, 0, 0), "one query, no ls-remote, no fetch");
    assert_eq!(only(&pass).state, WState::W1, "reported from the clone, not resynced");
    assert!(pass
        .host
        .as_deref()
        .is_some_and(|h| h.contains("breaker is open")));
    assert!(fx.forge.calls.borrow().is_empty());
    assert_eq!(fx.origin_head(), before);
}

#[test]
fn empty_and_archived_repos_are_skipped_without_a_failure() {
    for fault in [HeadFault::NoDefaultBranch, HeadFault::Archived] {
        let fx = Fixture::new(STALE);
        let host = Host::new(&fx, "host-a");
        host.heads
            .faults
            .borrow_mut()
            .insert(host.root.clone(), fault);
        let pass = host.pass();
        assert_eq!(only(&pass).state, WState::Skipped, "{pass:?}");
        assert_eq!(network(&pass), (1, 0, 0), "no ls-remote for it");
        assert!(pass.alerts.is_empty());
        assert!(host.memory.borrow().backoff.is_empty());
        assert!(host
            .memory
            .borrow()
            .noted
            .iter()
            .all(|n| !n.starts_with("blind:")));
    }
}

#[test]
fn a_broken_credential_helper_raises_one_host_level_alert() {
    use super::super::git::{network_failure, Credential, Refused};
    let stderr = b"fatal: could not read Username for 'https://github.com': terminal prompts \
                   disabled";
    let err = network_failure("git ls-remote", stderr);
    assert!(err.downcast_ref::<Credential>().is_some());
    assert!(err.downcast_ref::<Refused>().is_none(), "not a per-repo refusal");

    // Any number of repos failing that way, over any number of passes, is one
    // alert for the host with an empty root; a clean pass ends the run.
    let mut memory = Memory::default();
    let now = Utc::now();
    let first = memory.note_credential(Some("acme/a: no helper"), INTERVAL, now);
    let first = first.expect("the first failing pass alerts");
    assert_eq!((first.kind, first.root.as_os_str().is_empty()), ("credential-helper", true));
    assert!(memory
        .note_credential(Some("acme/b: no helper"), INTERVAL, now)
        .is_none());
    assert!(memory.note_credential(None, INTERVAL, now).is_none());
    assert!(memory
        .note_credential(Some("acme/a: no helper"), INTERVAL, now)
        .is_some());

    // And a per-repo failure of that kind never alerts as `repo-access`.
    let root = PathBuf::from("/nonexistent/cred");
    for _ in 0..5 {
        let (_, alert) =
            memory.fail(&report_for(&root), FailureKind::Credential, "x", INTERVAL, now);
        assert!(alert.is_none());
    }
}

#[cfg(unix)]
#[test]
fn a_credential_helper_failure_behind_a_forge_fault_stays_the_hosts() {
    // The forge says NotFound/Forbidden (the query's credential cannot see a
    // private repo) and the `git ls-remote` fallback cannot get a credential
    // either. That is the host's credential helper, not the repo: one
    // `credential-helper` alert, never a `repo-access` refusal per repo.
    use std::os::unix::fs::PermissionsExt;
    for fault in [
        gone(),
        HeadFault::Forbidden("Resource not accessible by integration".to_string()),
    ] {
        let fx = Fixture::new(current());
        let host = Host::new(&fx, "host-a");
        let private = fx.clone_as("private");
        host.heads
            .faults
            .borrow_mut()
            .insert(private.clone(), fault.clone());
        // An ssh "transport" that fails the way git does with no credential.
        let ssh = fx.tmp.path().join("no-credential-ssh");
        write(
            &ssh,
            "#!/bin/sh\necho \"fatal: could not read Username for 'https://github.com': \
             terminal prompts disabled\" >&2\nexit 128\n",
        );
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&private, &["config", "core.sshCommand", ssh.to_str().unwrap()]);
        git(
            &private,
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://git@example.invalid/acme/private.git",
            ],
        );
        let roots = [private.clone(), host.root.clone()];

        let mut alerts = Vec::new();
        for _ in 0..4 {
            let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
            let (bad, good) = (&pass.workspaces[0], &pass.workspaces[1]);
            assert_eq!(good.state, WState::W0, "the healthy repo is unaffected: {pass:?}");
            assert!(
                !reason(bad).contains("the forge"),
                "not wrapped as the forge's refusal ({fault}): {bad:?}"
            );
            assert!(reason(bad).contains("could not read Username"), "{bad:?}");
            alerts.extend(pass.alerts.iter().map(|a| (a.kind, a.root.clone())));
            assert!(host.memory.borrow().outage_hold(host.now.get()).is_none());
            // Past the repo's own backoff: every pass retries it.
            host.advance(Duration::from_secs(60 * 60));
        }
        assert_eq!(
            alerts,
            vec![("credential-helper", PathBuf::new())],
            "one host alert across the retries, no `repo-access` ({fault})"
        );
    }
}

// ----------------------------------------------------------------------------
// A pass that does not end
// ----------------------------------------------------------------------------

#[test]
fn no_resync_starts_after_the_pass_deadline() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    host.overdue.set(true);
    let pass = host.pass();
    assert_eq!(only(&pass).state, WState::W1, "classified, and left for the next pass");
    assert!(fx.forge.calls.borrow().is_empty(), "no claim");
    assert_eq!(fx.origin_head(), before);
    assert!(pass.alerts.is_empty());
    host.overdue.set(false);
    assert_eq!(only(&host.pass()).state, WState::W0);
}

#[test]
fn a_pass_still_running_after_n_ticks_alerts_exactly_once() {
    let refused = AtomicU32::new(0);
    let alerts: Vec<bool> = (0..STUCK_AFTER_TICKS + 3)
        .map(|_| note_refused(&refused).1)
        .collect();
    let at = (STUCK_AFTER_TICKS - 1) as usize;
    assert_eq!(alerts.iter().filter(|alert| **alert).count(), 1);
    assert!(alerts[at], "on the {STUCK_AFTER_TICKS}th tick in a row: {alerts:?}");
    assert_eq!(note_refused(&refused).0, STUCK_AFTER_TICKS + 4);
}

#[tokio::test]
async fn a_pass_that_never_ends_gives_the_slot_up() {
    static SLOT: AtomicBool = AtomicBool::new(false);
    // A pass that is stuck until the test lets it go.
    let (release, stuck) = mpsc::channel::<()>();
    let flight = begin(&SLOT).expect("the slot is free");
    assert!(begin(&SLOT).is_none(), "single flight while it runs");
    let ended = supervise(flight, Duration::from_millis(50), move || {
        let _ = stuck.recv();
        7
    })
    .await;
    assert_eq!(ended, Ended::Abandoned);
    assert!(!SLOT.load(Ordering::Acquire), "the slot is free although the pass never ended");
    drop(release);

    // A pass that ends in time, and one that panics, free it too.
    let flight = begin(&SLOT).expect("free again");
    assert_eq!(supervise(flight, Duration::from_secs(30), || 7).await, Ended::Done(7));
    let flight = begin(&SLOT).expect("free again");
    let ended = supervise(flight, Duration::from_secs(30), || -> u32 { panic!("a defect") }).await;
    assert!(matches!(ended, Ended::Panicked(_)), "{ended:?}");
    assert!(begin(&SLOT).is_some());
}
