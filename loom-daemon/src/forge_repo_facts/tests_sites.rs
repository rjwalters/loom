//! Site-level coverage for the repo-facts migration (W3a): owner equivalence
//! with the legacy call, the owner-confirmed `NoPr` / `NoneOpen` guards, the
//! per-pass confirm memo, row validation, decision parity with the legacy
//! probe, and the kill switch / call ledger.
//!
//! Every test here drives sites that resolve `gh` through the crate's
//! resolver, so each holds `loom_config_env` (the `LOOM_GH_BIN` writers'
//! key); the ones that also read `LOOM_REPO` nest a default-group `#[serial]`
//! body inside it, in that lock order (see `role_collision`'s tests).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::path::Path;

use chrono::Utc;
use serial_test::serial;

use super::record::{load, record_key};
use super::test_support::{pull_row, Env, Forge};
use super::*;
use crate::primary_checkout_reaper::{
    classify_primary_checkout, PrimaryCheckoutDecision, PrimaryCheckoutOptions,
    PrimaryCheckoutProbes,
};
use crate::types::ForgeCallCounts;
use crate::worktree_ops::clean::{self, CleanOptions, PrStatus, WorktreeDecision, WorktreeProbes};
use crate::worktree_ops::clean_owner::{pr_status_confirmed, pr_status_validated, repo_owner};
use crate::worktree_ops::gh::{probe_open_linked_pr, resolve_owner_repo, OpenPrProbe};

const BRANCH: &str = "feature/issue-1";
const LONG_AGO: &str = "2020-01-01T00:00:00Z";

/// Point the crate's `gh` resolver at `forge` for the closure.
fn with_gh<R>(forge: &Forge, f: impl FnOnce() -> R) -> R {
    std::env::set_var("LOOM_GH_BIN", &forge.gh);
    let out = f();
    std::env::remove_var("LOOM_GH_BIN");
    out
}

fn rows_after(body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = crate::forge_call_stats::status_report(Utc::now(), None);
    crate::forge_call_stats::set_test_sink_dir(None);
    report.host_window.unwrap_or_default()
}

fn calls(rows: &[ForgeCallCounts], caller: &str) -> u64 {
    rows.iter()
        .filter(|r| r.caller == caller)
        .map(|r| r.ok + r.error + r.not_modified)
        .sum()
}

fn suspect(nwo: &str) -> bool {
    load(&record_key("github.com", nwo)).unwrap().suspect
}

/// `classify_worktree` for a closed, pushed, managed, idle worktree whose
/// only open question is the PR status.
fn decide(path: &Path, pr_status: &dyn Fn(u32) -> PrStatus) -> WorktreeDecision {
    let active = HashSet::new();
    let probes = WorktreeProbes {
        active_issues: &active,
        in_use_marker: &|_| None,
        processes_using: &|_| Vec::new(),
        editable_installs: &|_| Vec::new(),
        is_managed: &|_| true,
        is_registered_worktree: &|_| true,
        issue_state: &|_| "CLOSED".to_string(),
        issue_closed_at: &|_| Some(LONG_AGO.to_string()),
        pr_status,
        branch_reachable_from_remotes: &|_| true,
        uncommitted: &|_| false,
        now: Utc::now(),
    };
    let opts = CleanOptions {
        safe: true,
        require_managed_sentinel: true,
        ..CleanOptions::default()
    };
    clean::classify_worktree(path, 1, &opts, &probes)
}

fn decide_primary(pr_status: &dyn Fn(&str) -> PrStatus) -> PrimaryCheckoutDecision {
    let probes = PrimaryCheckoutProbes {
        current_branch: &|| Some(BRANCH.to_string()),
        default_branch: &|| Some("main".to_string()),
        dirty: &|| false,
        pr_status,
        unpushed_commits: &|_| Some(0),
        now: Utc::now(),
    };
    classify_primary_checkout(
        &PrimaryCheckoutOptions {
            grace_period_secs: 0,
        },
        &probes,
    )
}

/// The reaper's own wiring: one owner per pass, confirmed `NoPr`.
fn reaper_status(root: &Path) -> PrStatus {
    let _pass = PassScope::enter();
    let owner = repo_owner(root).expect("an owner");
    pr_status_confirmed(root, &owner, BRANCH)
}

/// A checkout whose remote names `oldco/app`, with a record seeded while the
/// forge still said so.
fn seeded(env: &Env) -> (std::path::PathBuf, Forge) {
    let root = env.repo("r", &[("origin", "https://github.com/oldco/app.git")]);
    let forge = Forge::new(env.tmp.path(), "oldco/app");
    let owner = with_gh(&forge, || repo_owner(&root)).unwrap();
    assert_eq!(owner.owner, "oldco");
    assert!(owner.fact.is_some());
    (root, forge)
}

#[test]
#[serial(loom_config_env)]
fn repo_owner_equals_the_legacy_owner_login_including_after_a_redirect() {
    for canonical in ["acme/app", "newco/app"] {
        let env = Env::new(&[]);
        let root = env.repo("r", &[("origin", "https://github.com/acme/app.git")]);
        let forge = Forge::new(env.tmp.path(), canonical);
        let (legacy, facts) =
            with_gh(&forge, || (clean::repo_owner_rest(&root), repo_owner(&root).map(|o| o.owner)));
        assert_eq!(facts, legacy, "{canonical}");
        assert_eq!(facts.as_deref(), Some(canonical.split('/').next().unwrap()));
    }
}

#[test]
#[serial(loom_config_env)]
fn an_empty_answer_under_a_transferred_owner_is_unknown_and_never_removes() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    forge.set("canonical", "newco/app");
    let status = with_gh(&forge, || reaper_status(&root));
    assert_eq!(status, PrStatus::Unknown);
    assert_eq!(decide(&root, &|_| status.clone()), WorktreeDecision::SkipUnknownPrStatus);
    assert_eq!(
        decide_primary(&|_| status.clone()),
        PrimaryCheckoutDecision::SkipUnknownPrStatus
    );
    assert!(suspect("oldco/app"), "the record is suspect after the owner moved");
}

#[test]
#[serial(loom_config_env)]
fn an_empty_answer_under_a_confirmed_owner_is_nopr_exactly_as_before() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    let facts = with_gh(&forge, || reaper_status(&root));
    assert_eq!(facts, PrStatus::NoPr);
    set_test_env(Some(&[("LOOM_REPO_FACTS", "0")]));
    let legacy = with_gh(&forge, || reaper_status(&root));
    assert_eq!(legacy, PrStatus::NoPr);
    assert_eq!(decide(&root, &|_| facts.clone()), decide(&root, &|_| legacy.clone()));
    assert_eq!(decide(&root, &|_| facts.clone()), WorktreeDecision::Remove);
}

#[test]
#[serial(loom_config_env)]
fn a_failed_confirm_is_unknown_never_nopr() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    forge.set("mode", "fail");
    assert_eq!(with_gh(&forge, || reaper_status(&root)), PrStatus::Unknown);
}

#[test]
#[serial(loom_config_env)]
fn ten_nopr_worktrees_in_one_pass_cost_one_confirm() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    let reads = forge.repo_reads();
    let mut statuses = Vec::new();
    let rows = rows_after(|| {
        with_gh(&forge, || {
            let _pass = PassScope::enter();
            let owner = repo_owner(&root).unwrap();
            for n in 0..10 {
                statuses.push(pr_status_confirmed(&root, &owner, &format!("feature/issue-{n}")));
            }
        });
    });
    assert!(statuses.iter().all(|s| *s == PrStatus::NoPr), "{statuses:?}");
    assert_eq!(forge.repo_reads(), reads + 1);
    assert_eq!(calls(&rows, "repo_facts.confirm"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "clean.pr_status_rest"), 10, "{rows:?}");
}

#[test]
#[serial(loom_config_env)]
fn a_pulls_row_for_a_foreign_base_repo_is_unknown() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    let merged = Some(LONG_AGO);
    forge.set("pulls", &format!("[{}]", pull_row("closed", merged, "other/app")));
    assert_eq!(with_gh(&forge, || reaper_status(&root)), PrStatus::Unknown);
    assert!(suspect("oldco/app"));

    // A matching row is exactly the legacy classification.
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    forge.set("pulls", &format!("[{}]", pull_row("closed", merged, "oldco/app")));
    let owner = with_gh(&forge, || repo_owner(&root)).unwrap();
    let facts = with_gh(&forge, || pr_status_validated(&root, &owner, BRANCH));
    let legacy = with_gh(&forge, || clean::check_pr_status_for_branch_rest(&root, "oldco", BRANCH));
    assert_eq!(facts, legacy);
    assert!(matches!(facts, PrStatus::Merged { .. }));
}

const NONE_OPEN_GRAPHQL: &str =
    r#"{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}"#;

#[test]
#[serial(loom_config_env)]
fn a_none_open_under_a_moved_owner_is_a_probe_failure() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    forge.set("graphql", NONE_OPEN_GRAPHQL);
    assert_eq!(with_gh(&forge, || probe_open_linked_pr(&root, 5)), OpenPrProbe::NoneOpen);
    forge.set("canonical", "newco/app");
    assert_eq!(with_gh(&forge, || probe_open_linked_pr(&root, 5)), OpenPrProbe::ProbeFailed);
}

#[test]
#[serial(loom_config_env)]
fn an_unresolvable_repository_marks_the_record_suspect_and_fails_the_probe() {
    let env = Env::new(&[]);
    let (root, forge) = seeded(&env);
    forge.set(
        "graphql",
        r#"{"data":{"repository":null},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a Repository with the name 'oldco/app'."}]}"#,
    );
    forge.set("graphql_exit", "1");
    assert_eq!(with_gh(&forge, || probe_open_linked_pr(&root, 5)), OpenPrProbe::ProbeFailed);
    assert!(suspect("oldco/app"));
}

/// The fixture corpus run with the legacy probe and with facts + confirm on
/// an identical fake forge: identical decisions when the owner is stable,
/// and on a transfer every difference is a skip where legacy acted.
#[test]
#[serial(loom_config_env)]
fn decisions_match_the_legacy_probe_and_differ_only_toward_skipping_on_a_transfer() {
    let row = |state: &str, merged: Option<&str>| (state.to_string(), merged.map(str::to_string));
    let corpus: Vec<Vec<(String, Option<String>)>> = vec![
        vec![],
        vec![row("open", None)],
        vec![row("closed", Some(LONG_AGO))],
        vec![row("closed", None)],
        vec![row("closed", None), row("closed", Some(LONG_AGO))],
        vec![row("open", None), row("closed", None)],
    ];
    for transferred in [false, true] {
        for rows in &corpus {
            let env = Env::new(&[]);
            let (root, forge) = seeded(&env);
            let canonical = if transferred {
                "newco/app"
            } else {
                "oldco/app"
            };
            forge.set("canonical", canonical);
            let body: Vec<String> = rows
                .iter()
                .map(|(s, m)| pull_row(s, m.as_deref(), canonical))
                .collect();
            forge.set("pulls", &format!("[{}]", body.join(",")));

            let facts = with_gh(&forge, || reaper_status(&root));
            set_test_env(Some(&[("LOOM_REPO_FACTS", "0")]));
            let legacy = with_gh(&forge, || reaper_status(&root));
            set_test_env(Some(&[]));

            let (wf, wl) = (decide(&root, &|_| facts.clone()), decide(&root, &|_| legacy.clone()));
            let (pf, pl) =
                (decide_primary(&|_| facts.clone()), decide_primary(&|_| legacy.clone()));
            if transferred {
                assert!(
                    wf == wl || wf == WorktreeDecision::SkipUnknownPrStatus,
                    "{rows:?}: {wf:?} vs legacy {wl:?}"
                );
                assert!(
                    pf == pl || pf == PrimaryCheckoutDecision::SkipUnknownPrStatus,
                    "{rows:?}: {pf:?} vs legacy {pl:?}"
                );
            } else {
                assert_eq!(wf, wl, "{rows:?}");
                assert_eq!(pf, pl, "{rows:?}");
            }
        }
    }
}

/// With `LOOM_REPO_FACTS=0` every migrated site issues exactly its pre-facts
/// call; with facts on none of them does, and the reads are `repo_facts.*`.
#[test]
#[serial(loom_config_env)]
fn kill_switch_restores_each_legacy_call_and_facts_replace_them() {
    sites_ledger_body();
}

#[serial]
fn sites_ledger_body() {
    std::env::remove_var("LOOM_REPO");
    let legacy_ops = [
        "clean.repo_owner",
        "worktree.resolve_repo",
        "guard.repo_nwo",
    ];
    let run_sites = |root: &Path, forge: &Forge| {
        let mut config = crate::sweep_registry::SweepRegistryConfig::new(root.to_path_buf());
        config.gh_bin = Some(forge.gh.clone());
        let registry = crate::sweep_registry::SweepRegistry::new(config);
        with_gh(forge, || {
            assert_eq!(repo_owner(root).unwrap().owner, "acme");
            assert_eq!(resolve_owner_repo(root), Some(("acme".into(), "app".into())));
            assert_eq!(registry.resolve_owner_repo(), Some(("acme".into(), "app".into())));
        });
    };

    let env = Env::new(&[("LOOM_REPO_FACTS", "0")]);
    let root = env.repo("r", &[("origin", "https://github.com/acme/app.git")]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    let rows = rows_after(|| run_sites(&root, &forge));
    for op in legacy_ops {
        assert_eq!(calls(&rows, op), 1, "{op}: {rows:?}");
    }
    assert!(rows.iter().all(|r| !r.caller.starts_with("repo_facts.")), "{rows:?}");

    let env = Env::new(&[]);
    let root = env.repo("r", &[("origin", "https://github.com/acme/app.git")]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    let rows = rows_after(|| {
        for _ in 0..3 {
            run_sites(&root, &forge);
        }
    });
    for op in legacy_ops {
        assert_eq!(calls(&rows, op), 0, "{op}: {rows:?}");
    }
    assert_eq!(calls(&rows, "repo_facts.verify"), 1, "{rows:?}");
}

/// The sweep registry's guard resolver never turns `Unavailable` into `None`
/// (its guards fail OPEN on `None`): once a fact has answered, an expired
/// record whose verify fails — and the suspect backoff after it — still
/// resolves to the last-known pair, and a cold start whose verify fails
/// takes the pre-facts `gh repo view` path.
#[test]
#[serial(loom_config_env)]
fn guard_resolver_keeps_an_answer_when_the_record_is_unavailable() {
    guard_resolver_unavailable_body();
}

#[serial]
fn guard_resolver_unavailable_body() {
    std::env::remove_var("LOOM_REPO");
    let acme = Some(("acme".to_string(), "app".to_string()));
    let registry_for = |root: &Path, forge: &Forge| {
        let mut config = crate::sweep_registry::SweepRegistryConfig::new(root.to_path_buf());
        config.gh_bin = Some(forge.gh.clone());
        crate::sweep_registry::SweepRegistry::new(config)
    };

    // Warm: a verified record answers, then expires and its verify fails.
    let env = Env::new(&[]);
    let root = env.repo("r", &[("origin", "https://github.com/acme/app.git")]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    let registry = registry_for(&root, &forge);
    with_gh(&forge, || {
        assert_eq!(registry.resolve_owner_repo(), acme);
        forge.set("mode", "fail");
        advance_test_clock(VERIFY_TTL_DEFAULT_SECS + 1);
        let reads = forge.repo_reads();
        assert_eq!(registry.resolve_owner_repo(), acme, "expired + failed verify");
        assert_eq!(forge.repo_reads(), reads + 1, "the expired record was re-read");
        assert!(suspect("acme/app"));
        // Inside the suspect backoff: still answered, with no new read.
        for _ in 0..3 {
            assert_eq!(registry.resolve_owner_repo(), acme, "during the backoff");
        }
        assert_eq!(forge.repo_reads(), reads + 1, "no read during the backoff");
    });

    // Cold: no record, verify fails — the legacy `gh repo view` answers.
    let env = Env::new(&[]);
    let root = env.repo("r", &[("origin", "https://github.com/acme/app.git")]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    forge.set("mode", "fail");
    let registry = registry_for(&root, &forge);
    with_gh(&forge, || {
        assert_eq!(registry.resolve_owner_repo(), acme, "cold start + failed verify");
        assert!(
            forge
                .calls()
                .iter()
                .any(|c| c.starts_with("repo view --json owner,name")),
            "{:?}",
            forge.calls()
        );
    });
}
