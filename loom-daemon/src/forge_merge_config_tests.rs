//! Tests for `loom-daemon forge merge-config` (#9287). Split into a sibling
//! file (same `#[path]` pattern as `forge_listing_tests.rs`) so the module
//! itself stays small.

#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn flags(squash: bool, merge: bool, rebase: bool) -> RepoMergeFlags {
    RepoMergeFlags {
        allow_squash_merge: squash,
        allow_merge_commit: merge,
        allow_rebase_merge: rebase,
    }
}

fn pr_rule(id: u64, methods: &[&str]) -> BranchRule {
    BranchRule {
        kind: "pull_request".into(),
        ruleset_id: Some(id),
        parameters: Some(
            json!({ "allowed_merge_methods": methods, "required_approving_review_count": 0 }),
        ),
    }
}

fn rule(id: u64, kind: &str) -> BranchRule {
    BranchRule {
        kind: kind.into(),
        ruleset_id: Some(id),
        parameters: None,
    }
}

/// Evaluate with merge-pr.sh's own auto-detected method, as the CLI does.
fn eval_auto(f: RepoMergeFlags, rules: &[BranchRule]) -> Evaluation {
    let method = resolve_merge_method(None, f).unwrap();
    evaluate("main", f, &fold_rules(rules), &method, &BTreeMap::new())
}

fn codes(e: &Evaluation) -> Vec<&str> {
    e.findings
        .iter()
        .map(|f| &f[1..f.find(']').unwrap()])
        .collect()
}

// --- the six cases the issue's test plan names ------------------------------

#[test]
fn empty_effective_set_is_reported() {
    // The incident: repo merge-commit-only, ruleset allows only squash.
    let e = eval_auto(
        flags(false, true, false),
        &[
            pr_rule(8_809_610, &["squash"]),
            rule(8_809_610, "required_linear_history"),
        ],
    );
    assert!(e.effective.is_empty());
    assert_eq!(codes(&e), vec!["EMPTY_EFFECTIVE_SET"]);
    assert!(e.findings[0].contains("never edits"), "{:?}", e.findings);
}

#[test]
fn linear_history_with_merge_only_effective_set_is_reported() {
    let e = eval_auto(
        flags(false, true, false),
        &[pr_rule(1, &["merge"]), rule(1, "required_linear_history")],
    );
    assert_eq!(e.effective, MethodSet::from(["merge"]));
    assert_eq!(codes(&e), vec!["LINEAR_HISTORY_REJECTS_MERGE"]);
}

#[test]
fn linear_history_with_squash_is_silent() {
    // Squash produces linear history — the common, deliberate pairing.
    let e = eval_auto(
        flags(true, false, false),
        &[pr_rule(1, &["squash"]), rule(1, "required_linear_history")],
    );
    assert!(e.findings.is_empty(), "{:?}", e.findings);
    // Also silent when the ruleset names no methods at all.
    let e = eval_auto(flags(true, false, false), &[rule(1, "required_linear_history")]);
    assert!(e.findings.is_empty(), "{:?}", e.findings);
    // And when merge is also allowed but merge-pr.sh is told to squash.
    let f = flags(true, true, false);
    let c = fold_rules(&[rule(1, "required_linear_history")]);
    let e = evaluate("main", f, &c, "squash", &BTreeMap::new());
    assert!(e.findings.is_empty(), "{:?}", e.findings);
}

#[test]
fn no_ruleset_is_silent() {
    for f in [
        flags(true, true, true),
        flags(false, true, false),
        flags(true, false, false),
        flags(false, false, true),
    ] {
        let e = eval_auto(f, &[]);
        assert!(e.findings.is_empty(), "{f:?}: {:?}", e.findings);
    }
    // Rules that do not touch merging (deletion, non_fast_forward, checks).
    let e = eval_auto(
        flags(false, true, false),
        &[
            rule(1, "deletion"),
            rule(1, "non_fast_forward"),
            rule(1, "required_status_checks"),
        ],
    );
    assert!(e.findings.is_empty(), "{:?}", e.findings);
}

#[test]
fn multiple_active_rulesets_intersect() {
    // Each ruleset alone overlaps the repo; together they permit nothing. A
    // reader that looked only at the first ruleset would say "merge is fine".
    let e = eval_auto(flags(true, true, true), &[pr_rule(1, &["merge"]), pr_rule(2, &["squash"])]);
    assert!(e.effective.is_empty());
    assert_eq!(codes(&e), vec!["EMPTY_EFFECTIVE_SET"]);

    // Intersection narrows to squash; merge-pr.sh auto-detects merge.
    let e = eval_auto(
        flags(true, true, true),
        &[
            pr_rule(1, &["merge", "squash"]),
            pr_rule(2, &["squash", "rebase"]),
        ],
    );
    assert_eq!(e.effective, MethodSet::from(["squash"]));
    assert_eq!(codes(&e), vec!["METHOD_NOT_PERMITTED"]);

    // Intersection still contains the auto-detected method: silent.
    let e = eval_auto(
        flags(true, true, true),
        &[pr_rule(1, &["merge", "squash"]), pr_rule(2, &["merge"])],
    );
    assert_eq!(e.effective, MethodSet::from(["merge"]));
    assert!(e.findings.is_empty(), "{:?}", e.findings);

    // Linear history from a DIFFERENT ruleset still applies to the branch.
    let e = eval_auto(
        flags(false, true, false),
        &[pr_rule(1, &["merge"]), rule(2, "required_linear_history")],
    );
    assert_eq!(codes(&e), vec!["LINEAR_HISTORY_REJECTS_MERGE"]);
}

#[test]
fn github_403_on_rules_is_could_not_determine_never_a_finding() {
    let dir = tempdir().unwrap();
    let gh = mock_gh(
        dir.path(),
        Some(
            r#"{"allow_merge_commit":true,"allow_squash_merge":false,"allow_rebase_merge":false,"default_branch":"main","permissions":{"admin":false}}"#,
        ),
        Err("gh: Resource not accessible by integration (HTTP 403)"),
        None,
    );
    let out = github_merge_config(&gh, "acme/widgets", None, None, false);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("could not determine"), "{out:?}");
    assert!(out[0].contains("HTTP 403"), "{out:?}");
    assert!(out[0].contains("not a repository admin"), "{out:?}");
    assert!(!out[0].contains("WARNING"), "{out:?}");
}

// --- the remaining shapes ---------------------------------------------------

#[test]
fn configured_method_excluded_by_ruleset_is_reported() {
    // Repo allows merge + squash; ruleset only squash; auto-detect picks merge.
    let e = eval_auto(flags(true, true, false), &[pr_rule(1, &["squash"])]);
    assert_eq!(codes(&e), vec!["METHOD_NOT_PERMITTED"]);
    assert!(e.findings[0].contains("--merge-method squash"), "{:?}", e.findings);
    // An explicit --method squash is fine.
    let c = fold_rules(&[pr_rule(1, &["squash"])]);
    assert!(evaluate("main", flags(true, true, false), &c, "squash", &BTreeMap::new())
        .findings
        .is_empty());
}

#[test]
fn linear_history_rejects_auto_detected_merge_even_when_squash_is_permitted() {
    // merge-pr.sh prefers merge (#9105) and has no fallback: under linear
    // history its merges fail although squash would have worked.
    let e = eval_auto(flags(true, true, false), &[rule(1, "required_linear_history")]);
    assert_eq!(e.effective, MethodSet::from(["merge", "squash"]));
    assert_eq!(codes(&e), vec!["METHOD_NOT_PERMITTED"]);
    assert!(e.findings[0].contains("--merge-method squash"), "{:?}", e.findings);
}

#[test]
fn this_repos_post_incident_state_is_silent() {
    // rjwalters/loom after the fix: merge-only repo, ruleset ["merge"], no
    // required_linear_history — a false positive here would sink the check.
    let e = eval_auto(
        flags(false, true, false),
        &[
            rule(8_809_610, "deletion"),
            rule(8_809_610, "non_fast_forward"),
            pr_rule(8_809_610, &["merge"]),
            rule(8_809_610, "required_status_checks"),
        ],
    );
    assert!(e.findings.is_empty(), "{:?}", e.findings);
}

#[test]
fn pull_request_rule_without_methods_does_not_constrain() {
    let r = BranchRule {
        kind: "pull_request".into(),
        ruleset_id: Some(1),
        parameters: Some(json!({ "required_approving_review_count": 1 })),
    };
    let c = fold_rules(&[r]);
    assert!(c.allowed_by_ruleset.is_empty());
    assert!(eval_auto(flags(false, true, false), &[])
        .findings
        .is_empty());
}

#[test]
fn ruleset_names_label_findings() {
    let names = BTreeMap::from([(7u64, "main".to_string())]);
    let c = fold_rules(&[pr_rule(7, &["squash"])]);
    let e = evaluate("main", flags(false, true, false), &c, "merge", &names);
    assert!(e.findings[0].contains("ruleset 7 ('main') allows squash"), "{:?}", e.findings);
}

// --- the GitHub probe path, against a fake `gh` -----------------------------

/// A fake `gh` answering `api repos/<nwo>`, `api repos/<nwo>/rules/branches/…`
/// and `api repos/<nwo>/rulesets`. `Err(text)` makes that call exit 1 with
/// `text` on stderr; `None` for the repo body fails the repo call.
fn mock_gh(
    dir: &Path,
    repo: Option<&str>,
    rules: Result<&str, &str>,
    rulesets: Option<&str>,
) -> String {
    let arm = |body: Option<&str>, err: &str| match body {
        Some(b) => format!("cat <<'EOF'\n{b}\nEOF\nexit 0"),
        None => format!("echo '{err}' >&2; exit 1"),
    };
    let (rules_ok, rules_err) = match rules {
        Ok(b) => (Some(b), ""),
        Err(e) => (None, e),
    };
    let script = format!(
        "#!/bin/sh\ncase \"$2\" in\n\
         */rules/branches/*) {} ;;\n\
         */rulesets) {} ;;\n\
         *) {} ;;\nesac\n",
        arm(rules_ok, rules_err),
        arm(rulesets.or(Some("[]")), ""),
        arm(repo, "gh: HTTP 404"),
    );
    let path: PathBuf = dir.join("fake-gh.sh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_str().unwrap().to_string()
}

const MERGE_ONLY_REPO: &str = r#"{"allow_merge_commit":true,"allow_squash_merge":false,"allow_rebase_merge":false,"default_branch":"main","permissions":{"admin":true}}"#;

#[test]
fn github_incident_state_reports_empty_effective_set_with_ruleset_name() {
    let dir = tempdir().unwrap();
    let gh = mock_gh(
        dir.path(),
        Some(MERGE_ONLY_REPO),
        Ok(
            r#"[{"type":"pull_request","ruleset_id":8809610,"parameters":{"allowed_merge_methods":["squash"]}},{"type":"required_linear_history","ruleset_id":8809610}]"#,
        ),
        Some(r#"[{"id":8809610,"name":"main","target":"branch","enforcement":"active"}]"#),
    );
    let out = github_merge_config(&gh, "acme/widgets", None, None, false);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("merge-config: WARNING [EMPTY_EFFECTIVE_SET]"), "{out:?}");
    assert!(out[0].contains("ruleset 8809610 ('main')"), "{out:?}");
}

#[test]
fn github_no_ruleset_prints_nothing_unless_verbose() {
    let dir = tempdir().unwrap();
    let gh = mock_gh(dir.path(), Some(MERGE_ONLY_REPO), Ok("[]"), None);
    assert!(github_merge_config(&gh, "acme/widgets", None, None, false).is_empty());
    let out = github_merge_config(&gh, "acme/widgets", None, None, true);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("OK"), "{out:?}");
    assert!(out[0].contains("no active ruleset"), "{out:?}");
}

#[test]
fn github_repo_read_failure_is_could_not_determine() {
    let dir = tempdir().unwrap();
    let gh = mock_gh(dir.path(), None, Ok("[]"), None);
    let out = github_merge_config(&gh, "acme/widgets", None, None, false);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("could not determine"), "{out:?}");
    assert!(!out[0].contains("WARNING"), "{out:?}");
}

#[test]
fn github_invisible_allow_flags_are_could_not_determine_not_an_empty_set() {
    // A credential that cannot see allow_* must not read as "nothing allowed".
    let dir = tempdir().unwrap();
    let gh = mock_gh(
        dir.path(),
        Some(r#"{"default_branch":"main","permissions":{"admin":false}}"#),
        Ok(
            r#"[{"type":"pull_request","ruleset_id":1,"parameters":{"allowed_merge_methods":["squash"]}}]"#,
        ),
        None,
    );
    let out = github_merge_config(&gh, "acme/widgets", None, None, false);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("could not determine"), "{out:?}");
    assert!(!out[0].contains("EMPTY_EFFECTIVE_SET"), "{out:?}");
}
