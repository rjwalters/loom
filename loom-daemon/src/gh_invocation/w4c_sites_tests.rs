//! W4-C per-site guarantees: every Hygiene / Observability site maps a shed
//! to its "no answer" path, the decision-gating modules never opt out of
//! `Gate`, and the AC carry-over sites route (or stay on the writer) as
//! the audit table says.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::cwd_route::{derive_with, CwdAnswer, Derivation, DeriveEnv};
use super::test_routing::{install, seen, sheds, TestRouting};
use super::{AccessIntent, GhInvocation, GhTarget, Operation};
use crate::forge_identity::{ReadClass, RouteDecision};
use crate::forge_repo_facts::GhRepoEnv;
use crate::worktree_ops::clean::PrStatus;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Every reader for every owner withdrawn; the checkout resolves to
/// `acme/shed`.
fn exhausted() -> TestRouting {
    TestRouting {
        decision: RouteDecision::Exhausted {
            until: SystemTime::now() + Duration::from_secs(900),
        },
        env: DeriveEnv::default(),
        checkout: CwdAnswer::Sole("acme/shed".into()),
        shed: true,
    }
}

/// The Hygiene / Observability sites, driven through their own helpers with
/// every reader withdrawn: each is shed (no request) and each consumer reads
/// the shed as "no answer" — UNKNOWN, `PrStatus::Unknown`, `None`, a skipped
/// pass — never as a negative (`NoPr`, closed, not found, safe to delete).
#[test]
fn every_deferrable_site_maps_a_shed_to_no_answer() {
    use crate::worktree_ops::{clean, gh};
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let _g = install(exhausted());
    let mut prev = 0;
    let mut step = |name: &str, ok: bool| {
        assert!(ok, "{name}: a shed must read as no answer");
        assert!(sheds() > prev, "{name}: was not shed");
        prev = sheds();
    };

    step("worktree.issue_state", gh::issue_state(root, 7) == "UNKNOWN");
    step("worktree.issue_state_rest", gh::issue_state_rest(root, 7) == "UNKNOWN");
    step("worktree.issue_closed_at", gh::issue_closed_at_rest(root, 7).is_none());
    step("worktree.has_open_pr", gh::has_open_pr(root, "feature/x") == (false, false));
    step("clean.pr_list", clean::check_pr_merged(root, 7) == PrStatus::Unknown);
    step(
        "clean.pr_by_number_rest",
        clean::check_pr_by_number_rest(root, 9).status == PrStatus::Unknown,
    );
    step(
        "clean.pr_status_rest",
        clean::check_pr_status_for_branch_rest(root, "acme", "feature/x") == PrStatus::Unknown,
    );
    step(
        "intake.list_open",
        crate::intake_reconcile::run_once(Path::new("gh"), root, chrono::Utc::now(), 5) == 0,
    );
    {
        use crate::observability::ops::stage_dwell::{GhStageFetcher, StageFetcher};
        step(
            "stage_dwell.label_times",
            GhStageFetcher.label_times(root, "acme/shed", 3).is_none(),
        );
        step(
            "stage_dwell.merged_at",
            GhStageFetcher.merged_at(root, "acme/shed", 3).is_none(),
        );
    }
    step(
        "telemetry.repo_identity",
        crate::telemetry::repo_identity::resolve("acme/w4c-shed-identity").is_none(),
    );
    crate::telemetry::visibility::refresh_visibility_cache("acme/w4c-shed-visibility");
    step(
        "visibility.repo",
        crate::telemetry::visibility::cached_visibility("acme/w4c-shed-visibility").is_none(),
    );

    // Every shed read was classified and routed to the derived / typed repo.
    for s in seen().iter().filter(|s| s.shed) {
        assert_ne!(s.class, ReadClass::Gate, "{s:?}");
    }
}

#[test]
fn the_landed_ladder_never_reads_a_shed_as_landed() {
    let root = tempfile::tempdir().unwrap();
    let _g = install(exhausted());
    let landed = crate::worktree_ops::landed::probe(root.path(), Some("abc123"), Some(7));
    assert!(!landed.is_landed(), "{landed:?}");
}

/// The decision-gating modules never set a class other than `Gate`.
#[test]
fn decision_gating_modules_never_opt_out_of_gate() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let gated = rel.starts_with("sweep_registry")
                || rel.starts_with("claim_reconciliation")
                || rel.starts_with("merge_")
                || rel.contains("/merge_")
                || rel.starts_with("verdict")
                || rel.starts_with("quarantine")
                || rel.starts_with("reclaim");
            if !gated || path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                if code.contains("ReadClass::Hygiene") || code.contains("ReadClass::Observability")
                {
                    offenders.push(format!("{rel}:{}", n + 1));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "a decision-gating read was made sheddable: {offenders:?}");
}

/// Shapes of the AC carry-over sites, as each builds its argv.
fn shape(op: &'static str, args: &[&str]) -> GhInvocation {
    GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .current_dir("/tmp/w4c-ac")
    .args(args.iter().copied())
}

#[test]
fn the_carry_over_reads_route_to_the_reader() {
    let lone = |_: &Path, _: GhRepoEnv| CwdAnswer::Sole("acme/widget".to_string());
    let env = DeriveEnv::default();
    for inv in [
        shape(
            "worktree.issue_state_rest",
            &["api", "repos/{owner}/{repo}/issues/7", "--jq", ".state"],
        ),
        shape(
            "clean.pr_list",
            &[
                "pr", "list", "--head", "b", "--state", "all", "--json", "state",
            ],
        ),
        shape(
            "clean.pr_status_rest",
            &["api", "repos/{owner}/{repo}/pulls?state=all&head=acme:b"],
        ),
        shape("clean.pr_by_number_rest", &["api", "repos/{owner}/{repo}/pulls/9"]),
        shape(
            "intake.list_open",
            &[
                "api",
                "--paginate",
                "repos/{owner}/{repo}/issues?state=open&per_page=100",
                "--jq",
                ".",
            ],
        ),
        shape(
            "guard.lease_comments",
            &["api", "repos/{owner}/{repo}/issues/7/comments?per_page=100"],
        ),
        shape("guard.open_pr_timeline", &["api", "repos/acme/widget/issues/7/timeline"]),
        shape(
            "verdict.pr_comments",
            &[
                "api",
                "--paginate",
                "repos/{owner}/{repo}/issues/7/comments",
            ],
        ),
        shape("sequence.predecessor", &["api", "repos/{owner}/{repo}/pulls/8"]),
        shape("sequence.trusted_bodies", &["pr", "view", "8", "--json", "body"]),
    ] {
        let d = derive_with(&inv, &env, &lone);
        assert!(
            matches!(&d, Derivation::Route { slug, .. } if slug == "acme/widget"),
            "{}: {d:?}",
            inv.operation().as_str()
        );
    }
}

#[test]
fn the_asker_dependent_reads_stay_on_the_writer() {
    // Each site pins the writer in source, and the derivation refuses the
    // shapes on its own as a second line.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (file, op) in [
        ("write_scope/probe.rs", "write_scope.probe"),
        ("observability/ops/ratelimit.rs", "api.user"),
        ("forge_merge_method.rs", "merge_method.repo"),
        ("forge_merge_config.rs", "merge_config.api"),
        ("credential_preflight/pool.rs", "credential_preflight.user"),
    ] {
        let text = std::fs::read_to_string(src.join(file)).unwrap();
        let at = text
            .find(&format!("\"{op}\""))
            .unwrap_or_else(|| panic!("{op} in {file}"));
        let window: String = text[at..].chars().take(900).collect();
        assert!(window.contains(".writer_identity()"), "{op} ({file}) must pin the writer");
    }
    let env = DeriveEnv {
        loom_repo: Some("acme/widget".into()),
        ..DeriveEnv::default()
    };
    let lone = |_: &Path, _: GhRepoEnv| CwdAnswer::Sole("acme/widget".to_string());
    for inv in [
        shape("api.user", &["api", "user", "--jq", ".login"]),
        shape("merge_method.repo", &["api", "repos/acme/widget"]).writer_identity(),
        shape("merge_config.api", &["api", "repos/acme/widget/branches/main/protection"]),
        shape("write_scope.probe", &["api", "repos/acme/widget/collaborators/bot/permission"]),
    ] {
        assert_eq!(
            derive_with(&inv, &env, &lone),
            Derivation::Writer,
            "{}",
            inv.operation().as_str()
        );
    }
}

#[test]
fn the_seam_routes_a_gate_read_to_the_writer_not_a_shed() {
    // A Gate site under the same exhaustion is NOT shed: it reaches the
    // (non-existent) writer program and reports a spawn failure.
    let root = tempfile::tempdir().unwrap();
    let _g = install(exhausted());
    let out = crate::worktree_ops::gh::resolve_owner_repo(root.path());
    assert!(out.is_none());
    assert_eq!(sheds(), 0);
}
