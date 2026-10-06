//! W4-C: the reader route the facade derives for an untargeted read.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::forge_identity::IdentityRole;
use crate::forge_repo_facts::test_support::Env;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation, ParentContext};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn read_in(cwd: &Path, args: &[&str]) -> GhInvocation {
    GhInvocation::new(
        Operation::new("claim.issue_state"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .current_dir(cwd)
    .args(args.iter().copied())
}

fn env() -> DeriveEnv {
    DeriveEnv::default()
}

fn sole(slug: &'static str) -> impl Fn(&Path, GhRepoEnv) -> CwdAnswer {
    move |_, _| CwdAnswer::Sole(slug.to_string())
}

fn route(inv: &GhInvocation, env: &DeriveEnv, answer: CwdAnswer) -> Derivation {
    derive_with(inv, env, &move |_: &Path, _: GhRepoEnv| answer.clone())
}

fn slug_of(d: &Derivation) -> Option<&str> {
    match d {
        Derivation::Route { slug, .. } => Some(slug),
        _ => None,
    }
}

const PLACEHOLDER: &[&str] = &["api", "repos/{owner}/{repo}/issues/7", "--jq", ".state"];

// ===== resolution order =====

#[test]
fn an_explicit_path_a_repo_flag_and_a_lone_origin_placeholder_each_route() {
    let cwd = PathBuf::from("/tmp/w4c-checkout");
    let unresolved = CwdAnswer::Unresolved;
    let path = read_in(&cwd, &["api", "repos/acme/widget/pulls/3", "--jq", ".state"]);
    assert_eq!(slug_of(&route(&path, &env(), unresolved.clone())), Some("acme/widget"));
    let flag = read_in(&cwd, &["issue", "view", "7", "-R", "acme/widget", "--json", "state"]);
    assert_eq!(slug_of(&route(&flag, &env(), unresolved.clone())), Some("acme/widget"));
    let eq = read_in(&cwd, &["pr", "list", "--repo=acme/widget"]);
    assert_eq!(slug_of(&route(&eq, &env(), unresolved)), Some("acme/widget"));
    let placeholder = read_in(&cwd, PLACEHOLDER);
    assert_eq!(
        slug_of(&route(&placeholder, &env(), CwdAnswer::Sole("acme/widget".into()))),
        Some("acme/widget")
    );
}

#[test]
fn loom_repo_wins_over_the_checkout_for_a_placeholder_read() {
    // The claim_reconciliation `issue_is_confirmed_closed` shape: gh_call::read
    // (GhTarget::None, cwd = root) of a placeholder path, on a host whose
    // LOOM_REPO names a repo other than the root's origin.
    let cwd = PathBuf::from("/tmp/w4c-root");
    let inv = read_in(&cwd, PLACEHOLDER);
    let e = DeriveEnv {
        loom_repo: Some("acme/scoped".into()),
        ..env()
    };
    let d = derive_with(&inv, &e, &sole("acme/origin"));
    assert_eq!(
        d,
        Derivation::Route {
            slug: "acme/scoped".into(),
            via: Via::LoomRepo
        }
    );
    // An inherited GH_REPO, with LOOM_REPO unset, is what gh would use too.
    let e = DeriveEnv {
        gh_repo: Some("acme/inherited".into()),
        ..env()
    };
    let d = derive_with(&inv, &e, &sole("acme/origin"));
    assert_eq!(
        d,
        Derivation::Route {
            slug: "acme/inherited".into(),
            via: Via::GhRepo
        }
    );
    // A set-but-empty LOOM_REPO is exported as an empty GH_REPO, which gh
    // ignores: the checkout decides, and an inherited GH_REPO does not.
    let e = DeriveEnv {
        loom_repo: Some("".into()),
        gh_repo: Some("acme/inherited".into()),
        ..env()
    };
    assert_eq!(slug_of(&derive_with(&inv, &e, &sole("acme/origin"))), Some("acme/origin"));
}

#[test]
fn the_scope_repo_cli_shape_routes_to_the_scoped_repo_never_the_cwd_origin() {
    // `merge-pr consolidate --repo` sets LOOM_REPO (and GH_REPO) for the
    // process because the cwd is NOT the target repo.
    let cwd = PathBuf::from("/tmp/w4c-elsewhere");
    let e = DeriveEnv {
        loom_repo: Some("acme/target".into()),
        gh_repo: Some("acme/target".into()),
        ..env()
    };
    for args in [
        &["api", "repos/{owner}/{repo}/pulls/9/files"][..],
        &["pr", "view", "9", "--json", "files"],
        &["issue", "list", "--label", "x"],
    ] {
        let d = derive_with(&read_in(&cwd, args), &e, &sole("acme/cwd-origin"));
        assert_eq!(slug_of(&d), Some("acme/target"), "{args:?}");
    }
}

#[test]
fn the_reader_env_names_the_route_and_the_writer_env_is_unchanged() {
    let cwd = PathBuf::from("/tmp/w4c-root-env");
    let base = read_in(&cwd, PLACEHOLDER);
    let e = DeriveEnv {
        loom_repo: Some("acme/scoped".into()),
        ..env()
    };
    let derived = base.clone().with_derived_route_in(&e, &sole("acme/origin"));
    assert_eq!(derived.reader_slug().as_deref(), Some("acme/scoped"));
    assert_eq!(derived.target(), &GhTarget::None, "target is never changed");
    let gh_repo = |inv: &GhInvocation, loom: Option<&str>| {
        inv.env_plan_with(loom.map(OsString::from), None)
            .into_iter()
            .find(|e| e.key == "GH_REPO")
            .and_then(|e| e.value)
    };
    let reader = derived
        .clone()
        .identity_role(IdentityRole::Reader)
        .gh_config_dir(Some(Path::new("/tmp/reader-dir")))
        .without_token_env();
    assert_eq!(gh_repo(&reader, Some("acme/scoped")), Some("acme/scoped".into()));
    // Even with no LOOM_REPO in the child's env (a checkout route), the reader
    // attempt names its repo.
    let from_checkout = base
        .clone()
        .with_derived_route_in(&env(), &sole("acme/origin"));
    let reader = from_checkout
        .identity_role(IdentityRole::Reader)
        .gh_config_dir(Some(Path::new("/tmp/reader-dir")));
    assert_eq!(gh_repo(&reader, None), Some("acme/origin".into()));
}

#[test]
fn a_writer_fallback_env_plan_equals_the_pre_change_plan_with_derivation_on_and_off() {
    let cwd = PathBuf::from("/tmp/w4c-writer-snapshot");
    for loom_repo in [None, Some("acme/scoped")] {
        let e = DeriveEnv {
            loom_repo: loom_repo.map(OsString::from),
            ..env()
        };
        let before = read_in(&cwd, PLACEHOLDER);
        let derived = before
            .clone()
            .with_derived_route_in(&e, &sole("acme/origin"));
        assert!(derived.reader_slug().is_some());
        let off = DeriveEnv {
            cwd_routing: Some("0".into()),
            ..e.clone()
        };
        let not_derived = before
            .clone()
            .with_derived_route_in(&off, &sole("acme/origin"));
        assert!(not_derived.reader_slug().is_none());
        for role in [
            None,
            Some(IdentityRole::Writer),
            Some(IdentityRole::WriterFallback),
        ] {
            let attempt = |inv: &GhInvocation| {
                let inv = match role {
                    Some(r) => inv.clone().identity_role(r),
                    None => inv.clone(),
                };
                inv.env_plan_with(loom_repo.map(OsString::from), None)
            };
            assert_eq!(attempt(&derived), attempt(&before), "{loom_repo:?} {role:?}");
            assert_eq!(attempt(&not_derived), attempt(&before), "{loom_repo:?} {role:?}");
        }
    }
}

// ===== what never derives =====

#[test]
fn unroutable_shapes_stay_on_the_writer() {
    let cwd = PathBuf::from("/tmp/w4c-unroutable");
    let e = DeriveEnv {
        loom_repo: Some("acme/scoped".into()),
        ..env()
    };
    for args in [
        &["api", "graphql", "-f", "query=query{viewer{login}}"][..],
        &["api", "user"],
        &["api", "/user", "--jq", ".login"],
        &["api", "rate_limit"],
        &["api", "search/issues?q=x"],
        &["api", "orgs/acme/repos"],
        &["api", "installation/repositories"],
        &["api", "repos/acme/widget/collaborators/bob/permission"],
        &["api", "repos/acme/widget/branches/main/protection"],
        &["api", "repos/acme/widget/rulesets"],
        &[
            "api",
            "--hostname",
            "ghe.example.com",
            "repos/acme/widget/issues",
        ],
        &["search", "issues", "--repo", "acme/widget"],
        &["run", "list", "-R", "acme/widget"],
        &["release", "view", "v1"],
        &["repo", "list", "acme"],
        &["pr", "view", "https://github.com/acme/other/pull/3"],
        &["issue", "view", "3", "-R", "ghe.example.com/acme/widget"],
        &["pr", "checks", "4"],
        // Mutations sent through a read-intent helper never derive.
        &[
            "api",
            "-X",
            "POST",
            "repos/acme/widget/issues/3/reactions",
            "-f",
            "content=+1",
        ],
        &[
            "api",
            "repos/acme/widget/issues/3/reactions",
            "-f",
            "content=+1",
        ],
        &[
            "api",
            "--method",
            "DELETE",
            "repos/acme/widget/issues/3/reactions/9",
        ],
        &["api", "-XPATCH", "repos/{owner}/{repo}/issues/3"],
        &["api", "repos/{owner}/{repo}/issues", "--input", "body.json"],
    ] {
        let d = derive_with(&read_in(&cwd, args), &e, &sole("acme/origin"));
        assert_eq!(d, Derivation::Writer, "{args:?}");
    }
}

#[test]
fn ineligible_invocations_are_never_derived() {
    let cwd = PathBuf::from("/tmp/w4c-ineligible");
    let base = read_in(&cwd, PLACEHOLDER);
    let write = GhInvocation::new(
        Operation::new("claim.issue_state"),
        AccessIntent::Write,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .current_dir(&cwd)
    .args(PLACEHOLDER.iter().copied());
    let no_cwd = GhInvocation::new(
        Operation::new("claim.issue_state"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .args(PLACEHOLDER.iter().copied());
    for (name, inv) in [
        ("write", write),
        ("no cwd", no_cwd),
        ("passthrough", base.clone().passthrough()),
        ("writer_identity", base.clone().writer_identity()),
        ("explicit dir", base.clone().gh_config_dir(Some(Path::new("/tmp/x")))),
        ("token stripped", base.clone().without_token_env()),
        ("role", base.clone().identity_role(IdentityRole::Reader)),
        (
            "typed target",
            GhInvocation {
                target: GhTarget::repo("acme/t").unwrap(),
                ..base.clone()
            },
        ),
    ] {
        assert_eq!(
            route(&inv, &env(), CwdAnswer::Sole("acme/origin".into())),
            Derivation::Writer,
            "{name}"
        );
    }
}

#[test]
fn the_kill_switches_disable_derivation() {
    let cwd = PathBuf::from("/tmp/w4c-killed");
    let inv = read_in(&cwd, &["api", "repos/acme/widget/issues/1"]);
    let off = DeriveEnv {
        cwd_routing: Some(" 0 ".into()),
        ..env()
    };
    assert_eq!(route(&inv, &off, CwdAnswer::Unresolved), Derivation::Writer);
    let legacy = DeriveEnv {
        legacy: true,
        ..env()
    };
    assert_eq!(route(&inv, &legacy, CwdAnswer::Unresolved), Derivation::Writer);
    let on = DeriveEnv {
        cwd_routing: Some("1".into()),
        ..env()
    };
    assert!(slug_of(&route(&inv, &on, CwdAnswer::Unresolved)).is_some());
}

#[test]
fn a_stripped_gh_repo_and_repo_view_resolve_from_the_checkout_only() {
    let cwd = PathBuf::from("/tmp/w4c-repo-view");
    let e = DeriveEnv {
        loom_repo: Some("acme/scoped".into()),
        ..env()
    };
    let seen = std::cell::RefCell::new(Vec::new());
    let checkout = |_: &Path, g: GhRepoEnv| {
        seen.borrow_mut().push(g);
        CwdAnswer::Sole("acme/origin".to_string())
    };
    let view = read_in(&cwd, &["repo", "view", "--json", "owner"]);
    assert_eq!(slug_of(&derive_with(&view, &e, &checkout)), Some("acme/origin"));
    let stripped = read_in(&cwd, PLACEHOLDER).strip_env("GH_REPO");
    assert_eq!(slug_of(&derive_with(&stripped, &e, &checkout)), Some("acme/origin"));
    assert_eq!(*seen.borrow(), vec![GhRepoEnv::Ignore, GhRepoEnv::Ignore]);
    let named = read_in(&cwd, &["repo", "view", "acme/named", "--json", "owner"]);
    assert_eq!(slug_of(&derive_with(&named, &e, &checkout)), Some("acme/named"));
}

// ===== the checkout rule against real git config =====

#[test]
fn a_lone_github_origin_is_the_sole_answer() {
    let env = Env::new(&[]);
    let root = env.repo("lone", &[("origin", "https://github.com/acme/widget.git")]);
    assert_eq!(checkout_answer(&root, GhRepoEnv::Honour), CwdAnswer::Sole("acme/widget".into()));
}

#[test]
fn ambiguous_checkouts_keep_the_writer_and_count_a_disagreement() {
    let env = Env::new(&[]);
    let pinned = env.repo("pinned", &[("origin", "https://github.com/acme/widget.git")]);
    env.git(&pinned, &["config", "remote.origin.gh-resolved", "base"]);
    let upstream = env.repo(
        "upstream",
        &[
            ("origin", "https://github.com/me/widget.git"),
            ("upstream", "https://github.com/acme/widget.git"),
        ],
    );
    let rewritten = env.repo("rewritten", &[("origin", "https://github.com/acme/widget.git")]);
    env.git(
        &rewritten,
        &[
            "config",
            "url.https://github.com/acme/fork.git.insteadOf",
            "https://github.com/acme/widget.git",
        ],
    );
    for root in [&pinned, &upstream, &rewritten] {
        assert_eq!(
            checkout_answer(root, GhRepoEnv::Honour),
            CwdAnswer::Disagree,
            "{}",
            root.display()
        );
        let before = crate::forge_call_stats::counters::get(DISAGREE_COUNTER);
        let inv = read_in(root, PLACEHOLDER)
            .with_derived_route_in(&DeriveEnv::default(), &checkout_answer);
        assert!(inv.reader_slug().is_none(), "{}", root.display());
        assert!(crate::forge_call_stats::counters::get(DISAGREE_COUNTER) > before);
    }
}

#[test]
fn a_remote_identity_that_disagrees_with_git_config_keeps_the_writer() {
    // The resolver's answer and remote_identity disagree (a stale memo, or
    // a rewrite only one of them applies): modelled by the checkout answer.
    let cwd = PathBuf::from("/tmp/w4c-stale");
    let before = crate::forge_call_stats::counters::get(DISAGREE_COUNTER);
    let inv = read_in(&cwd, PLACEHOLDER)
        .with_derived_route_in(&DeriveEnv::default(), &|_: &Path, _: GhRepoEnv| {
            CwdAnswer::Disagree
        });
    assert!(inv.reader_slug().is_none());
    assert!(crate::forge_call_stats::counters::get(DISAGREE_COUNTER) > before);
}

#[test]
fn a_moved_origin_is_seen_without_a_restart() {
    let env = Env::new(&[]);
    let root = env.repo("moved", &[("origin", "https://github.com/acme/old.git")]);
    assert_eq!(checkout_answer(&root, GhRepoEnv::Honour), CwdAnswer::Sole("acme/old".into()));
    env.git(
        &root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/new.git",
        ],
    );
    assert_eq!(checkout_answer(&root, GhRepoEnv::Honour), CwdAnswer::Sole("acme/new".into()));
}

#[test]
fn facts_off_or_no_checkout_is_unresolved_not_a_disagreement() {
    // Facts are off by default on a test thread.
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(checkout_answer(tmp.path(), GhRepoEnv::Honour), CwdAnswer::Unresolved);
    let env = Env::new(&[]);
    let bare = env.tmp.path().join("not-a-checkout");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(checkout_answer(&bare, GhRepoEnv::Honour), CwdAnswer::Unresolved);
}

// ===== shapes =====

#[test]
fn asker_dependent_paths_are_recognised() {
    for p in [
        "user",
        "/user/repos",
        "installation/repositories",
        "app",
        "repos/acme/w/collaborators/bob/permission",
        "repos/acme/w/branches/main/protection/required_status_checks",
        "repos/acme/w/rulesets/7",
        "repos/acme/w/rules/branches/main",
        "repos/acme/w/installation",
    ] {
        assert!(asker_dependent_path(p), "{p}");
    }
    for p in [
        "repos/acme/w/issues/1",
        "repos/acme/w/collaborators",
        "repos/acme/w/branches/main",
    ] {
        assert!(!asker_dependent_path(p), "{p}");
    }
}

#[test]
fn a_get_with_query_fields_still_derives() {
    let cwd = PathBuf::from("/tmp/w4c-get-fields");
    let inv = read_in(
        &cwd,
        &[
            "api",
            "-X",
            "GET",
            "repos/acme/widget/issues",
            "-f",
            "state=open",
        ],
    );
    assert_eq!(
        derive_with(&inv, &env(), &sole("acme/origin")),
        Derivation::Route {
            slug: "acme/widget".to_string(),
            via: Via::Path
        }
    );
}
