#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::resolver::{resolve_from, GhBinSource};
use super::*;
use std::os::unix::fs::PermissionsExt;

fn env_of(plan: &[EnvEntry], key: &str) -> Option<Option<String>> {
    plan.iter()
        .find(|e| e.key == key)
        .map(|e| e.value.as_ref().map(|v| v.to_string_lossy().into_owned()))
}

fn read_op(target: GhTarget) -> GhInvocation {
    GhInvocation::new(
        Operation::new("issue.list"),
        AccessIntent::Read,
        target,
        Duration::from_secs(5),
    )
    .parent(ParentContext::Missing)
}

#[test]
fn resolver_precedence_is_policy_then_env_then_path() {
    let r = resolve_from(Some("/opt/launcher".into()), Some("/stub/gh".into()));
    assert_eq!((r.program.as_str(), r.source), ("/opt/launcher", GhBinSource::Policy));

    let r = resolve_from(None, Some("/stub/gh".into()));
    assert_eq!((r.program.as_str(), r.source), ("/stub/gh", GhBinSource::EnvOverride));

    let r = resolve_from(None, None);
    assert_eq!((r.program.as_str(), r.source), ("gh", GhBinSource::Path));
}

#[test]
fn resolver_keeps_the_legacy_set_but_empty_semantics() {
    // Byte-identical to the hand-rolled `std::env::var(..).unwrap_or("gh")`
    // copies: a set-but-empty LOOM_GH_BIN is returned, not skipped.
    let r = resolve_from(None, Some(String::new()));
    assert_eq!((r.program.as_str(), r.source), ("", GhBinSource::EnvOverride));
}

#[test]
fn operation_names_are_dotted_lowercase_literals() {
    for ok in [
        "issue.list",
        "api.rest",
        "api.graphql",
        "pr.merge",
        "rate_limit",
        "v2.x",
    ] {
        assert!(Operation::is_valid(ok), "{ok}");
    }
    for bad in [
        "",
        "Issue.list",
        "issue..list",
        ".issue",
        "issue.",
        "issue list",
        "1st",
        "a-b",
    ] {
        assert!(!Operation::is_valid(bad), "{bad:?}");
    }
}

#[test]
fn target_parses_owner_repo_and_never_echoes_bad_input() {
    let t = GhTarget::repo("rjwalters/loom").unwrap();
    assert_eq!(t.slug().as_deref(), Some("rjwalters/loom"));
    assert_eq!(t.bounded(), "rjwalters/loom");
    assert_eq!(GhTarget::None.bounded(), "none");
    // One fixed message for every rejection: the input is never interpolated.
    for bad in ["", "loom", "a/b/c", "/repo", "owner/", "own er/repo"] {
        let err = GhTarget::repo(bad).unwrap_err();
        assert_eq!(err, "gh target must be an `owner/repo` slug", "{bad:?}");
    }
}

#[test]
fn target_attribute_is_bounded() {
    let long = format!("{}/{}", "o".repeat(80), "r".repeat(80));
    let t = GhTarget::repo(&long).unwrap();
    assert_eq!(t.bounded().chars().count(), TARGET_ATTR_MAX);
}

#[test]
fn env_plan_with_no_child_context_strips_both_traceparents() {
    let plan = read_op(GhTarget::None).env_plan_with(None, None);
    assert_eq!(env_of(&plan, TRACEPARENT_ENV), Some(None));
    assert_eq!(env_of(&plan, W3C_TRACEPARENT_ENV), Some(None));
    assert_eq!(env_of(&plan, "GH_REPO"), None);
    assert_eq!(read_op(GhTarget::None).context_source(), "missing");
}

#[test]
fn env_plan_exports_the_child_context_under_both_names() {
    let ctx = TraceContext::derived("sweep", &["gh-invocation-test"]);
    let inv = read_op(GhTarget::None).parent(ParentContext::Parent(ctx.clone()));
    let plan = inv.env_plan_with(None, Some(&ctx));
    assert_eq!(env_of(&plan, TRACEPARENT_ENV), Some(Some(ctx.traceparent())));
    assert_eq!(env_of(&plan, W3C_TRACEPARENT_ENV), Some(Some(ctx.traceparent())));
    assert_eq!(inv.context_source(), "parent");
}

#[test]
fn env_plan_gh_repo_prefers_the_typed_target_over_loom_repo() {
    let typed = read_op(GhTarget::repo("acme/widgets").unwrap())
        .env_plan_with(Some("other/repo".into()), None);
    assert_eq!(env_of(&typed, "GH_REPO"), Some(Some("acme/widgets".into())));

    let ambient = read_op(GhTarget::None).env_plan_with(Some("other/repo".into()), None);
    assert_eq!(env_of(&ambient, "GH_REPO"), Some(Some("other/repo".into())));
}

#[test]
fn env_plan_scopes_gh_config_dir_by_root_then_target_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("checkout");
    std::fs::create_dir_all(&root).unwrap();
    let root_cfg = tmp.path().join("root-cfg");
    let owner_cfg = tmp.path().join("owner-cfg");
    crate::credential_preflight::register_root_gh_config_dir(&root, &root_cfg);
    crate::credential_preflight::register_owner_gh_config_dir(
        "gh-invocation-test-owner-9985",
        &owner_cfg,
    );
    let target = GhTarget::repo("gh-invocation-test-owner-9985/x").unwrap();

    let by_owner = read_op(target.clone()).env_plan_with(None, None);
    assert_eq!(
        env_of(&by_owner, "GH_CONFIG_DIR"),
        Some(Some(owner_cfg.to_string_lossy().into_owned()))
    );

    let by_root = read_op(target).current_dir(&root).env_plan_with(None, None);
    assert_eq!(
        env_of(&by_root, "GH_CONFIG_DIR"),
        Some(Some(root_cfg.to_string_lossy().into_owned()))
    );

    let unscoped = read_op(GhTarget::None).env_plan_with(None, None);
    assert_eq!(env_of(&unscoped, "GH_CONFIG_DIR"), None);
}

/// A recording stub `gh`: prints its argv and the facade-owned env.
fn stub(dir: &Path) -> String {
    let path = dir.join("gh-stub");
    std::fs::write(
        &path,
        "#!/bin/sh\necho \"argv=$*\"\necho \"tp=${TRACEPARENT:-unset}\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn captured_execution_runs_the_resolved_program_with_the_assembled_env() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = TraceContext::derived("sweep", &["gh-invocation-exec"]);
    let inv = read_op(GhTarget::None)
        .args(["issue", "list"])
        .current_dir(tmp.path())
        .parent(ParentContext::Parent(ctx.clone()));
    let GhCompletion::Captured(Completion::Exited(out)) = inv
        .execute_with(&stub(tmp.path()), GhBinSource::EnvOverride)
        .unwrap()
    else {
        panic!("expected a captured, exited completion");
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("argv=issue list"), "{stdout}");
    assert!(stdout.contains(&format!("tp={}", ctx.traceparent())), "{stdout}");
    assert_eq!(out.status.code(), Some(7), "a non-zero exit is reported, not hidden");
}

#[test]
fn a_missing_program_is_a_spawn_error_not_an_empty_result() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-gh");
    let err = read_op(GhTarget::None)
        .execute_with(&missing.to_string_lossy(), GhBinSource::EnvOverride)
        .unwrap_err();
    assert!(matches!(err, ExecError::Spawn(_)), "{err}");
}

#[test]
fn passthrough_contract_is_preserved_and_returns_the_exit_status() {
    let tmp = tempfile::tempdir().unwrap();
    let inv = read_op(GhTarget::None).args(["--version"]).passthrough();
    assert_eq!(inv.contract(), OutputContract::Passthrough);
    let GhCompletion::Passthrough(status) = inv
        .execute_with(&stub(tmp.path()), GhBinSource::EnvOverride)
        .unwrap()
    else {
        panic!("expected a passthrough completion");
    };
    assert_eq!(status.code(), Some(7));
}
