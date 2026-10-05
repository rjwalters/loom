//! #10050: forge-egress-governed provisioning (git credential separation,
//! pinned upstream gh, trailing doctor). Split from `tests.rs` (size ratchet).

use super::super::tests::base_config;
use super::super::*;
use super::EgressProvisioning;
use crate::fleet::PlanEntry;
use serde_json::json;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn policy(api: &str, rollout: &str, sha: Option<&str>) -> serde_json::Value {
    json!({
        "enforcement": {"api": api},
        "github": {"gitOrigin": {"rollout": rollout}},
        "toolchain": {
            "ghPinnedVersion": "2.101.0",
            "upstreamGhPath": "/opt/loom/gh-upstream/bin/gh",
            "upstreamGhSha256": sha,
        }
    })
}

fn names(plan: &Plan) -> Vec<String> {
    plan.entries
        .iter()
        .map(|e| match e {
            PlanEntry::Step(s) => s.name.clone(),
            PlanEntry::Skip { name, .. } => name.clone(),
        })
        .collect()
}

fn step<'a>(plan: &'a Plan, name: &str) -> &'a Step {
    plan.entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == name => Some(s),
            _ => None,
        })
        .unwrap()
}

fn secrets() -> Secrets {
    Secrets {
        pat: Some("tok".into()),
        ..Secrets::default()
    }
}

#[test]
fn observe_or_absent_policy_leaves_plan_unchanged() {
    assert!(EgressProvisioning::from_policy(&policy("observe", "unqualified", None)).is_none());
    let base = build_plan(&base_config(), &secrets());
    let none = build_plan_with_policy(&base_config(), &secrets(), None);
    assert_eq!(names(&base), names(&none));
    assert!(step(&base, "forge-auth")
        .apply
        .contains("gh auth setup-git"));
}

#[test]
fn required_policy_drops_setup_git_and_follows_rollout() {
    for (rollout, proto) in [
        ("unqualified", "https"),
        ("qualified", "ssh"),
        ("enforced", "ssh"),
    ] {
        let p = EgressProvisioning::from_policy(&policy("required", rollout, Some(SHA))).unwrap();
        let plan = build_plan_with_policy(&base_config(), &secrets(), Some(&p));
        let auth = step(&plan, "forge-auth");
        // No gh credential at all on a governed host: no login, no setup-git,
        // and the PAT is never consumed on stdin.
        assert!(
            !auth
                .apply
                .lines()
                .any(|l| l.trim_start().starts_with("gh auth")),
            "{}",
            auth.apply
        );
        assert!(auth.stdin.is_none());
        assert!(auth
            .apply
            .contains(&format!("gh config set git_protocol {proto}")));
    }
}

#[test]
fn required_policy_adds_pinned_gh_then_trailing_doctor() {
    let p = EgressProvisioning::from_policy(&policy("required", "qualified", Some(SHA))).unwrap();
    let plan = build_plan_with_policy(&base_config(), &secrets(), Some(&p));
    let n = names(&plan);
    let pos = |x: &str| n.iter().position(|s| s == x).unwrap();
    assert!(pos("pinned-gh") > pos("forge-auth") && pos("pinned-gh") < pos("workspace-clone"));
    assert_eq!(n.last().unwrap(), "forge-egress-doctor");
    let gh = step(&plan, "pinned-gh");
    assert!(gh.apply.contains("/opt/loom/gh-upstream/bin/gh"));
    assert!(gh.apply.contains(SHA) && gh.apply.contains("sha256sum -c"));
    assert!(gh.check.as_ref().unwrap().contains("gh version 2.101.0"));
    assert!(step(&plan, "forge-egress-doctor")
        .apply
        .contains("forge egress doctor"));
    // Dry-run output carries the ordering and no secret.
    let out = plan.render_dry_run("fleet add-worker", "worker-1");
    assert!(out.find("pinned-gh").unwrap() < out.find("forge-egress-doctor").unwrap());
    assert!(!out.contains("tok\n"));
}

#[test]
fn null_checksum_skips_verification_and_bad_values_skip_step() {
    let p = EgressProvisioning::from_policy(&policy("required", "qualified", None)).unwrap();
    let plan = build_plan_with_policy(&base_config(), &secrets(), Some(&p));
    assert!(step(&plan, "pinned-gh")
        .apply
        .contains("skipping checksum verification"));

    let mut bad = policy("required", "qualified", None);
    bad["toolchain"]["upstreamGhPath"] = json!("/x/$(evil)");
    let p = EgressProvisioning::from_policy(&bad).unwrap();
    let plan = build_plan_with_policy(&base_config(), &secrets(), Some(&p));
    assert!(plan
        .entries
        .iter()
        .any(|e| matches!(e, PlanEntry::Skip { name, .. } if name == "pinned-gh")));
}

#[test]
fn governed_forge_auth_runs_without_a_pat() {
    let p = EgressProvisioning::from_policy(&policy("required", "unqualified", None)).unwrap();
    let plan = build_plan_with_policy(&base_config(), &Secrets::default(), Some(&p));
    assert!(step(&plan, "forge-auth")
        .apply
        .contains("git_protocol https"));
}

#[test]
fn load_for_operator_reads_policy_and_fails_closed_when_unreadable() {
    use super::load_for_operator;
    use crate::forge_egress::policy::PolicySources;
    let dir = tempfile::tempdir().unwrap();
    let env = |name: &str, body: Option<String>| {
        let path = dir.path().join(name);
        if let Some(body) = body {
            std::fs::write(&path, body).unwrap();
        }
        PolicySources {
            env_path: Some(path),
            machine_path: None,
            repo_path: None,
            ..PolicySources::default()
        }
    };
    // Absent policy -> legacy plan (the only "no policy" case).
    assert!(load_for_operator(&PolicySources::default())
        .unwrap()
        .is_none());
    let req = env("req.json", Some(policy("required", "qualified", None).to_string()));
    assert!(load_for_operator(&req).unwrap().unwrap().git_ssh);
    let obs = env("obs.json", Some(policy("observe", "qualified", None).to_string()));
    assert!(load_for_operator(&obs).unwrap().is_none());
    // Corrupt or missing-but-named policy -> refuse, never the legacy plan.
    for sources in [
        env("bad.json", Some("{not json".into())),
        env("missing.json", None),
    ] {
        let err = load_for_operator(&sources).unwrap_err().to_string();
        assert!(err.contains("unreadable") && err.contains(".json"), "{err}");
    }
}
