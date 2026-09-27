//! Unit tests for the merge-ordering guard (#3747 item 2 / #7982, #8191 slice).
//!
//! The pure decisions and every rendered message are pinned here; agreement with
//! the retired shell on a shared corpus is
//! `tests/merge_pr_stacked_children_differential.rs`'s job. The pin itself is
//! exercised against a real git repository, because what makes the pin useful is
//! that the ref names an object the repo actually has — a mock cannot fail that
//! way.

use super::*;
use std::process::Command;

fn kid(number: i64, branch: &str) -> Child {
    Child {
        number,
        head_ref_name: branch.to_string(),
    }
}

fn inputs<'a>(branch: &'a str, children: &'a [Child]) -> Inputs<'a> {
    Inputs {
        pr_number: "999",
        branch,
        head_sha: "abc123",
        children,
        allow_stacked_children: false,
        dry_run: false,
    }
}

// --- the branch-shape gate -------------------------------------------------

#[test]
fn only_a_strict_feature_issue_branch_is_a_stackable_parent() {
    assert!(is_stackable_parent_branch("feature/issue-100"));
    assert!(is_stackable_parent_branch("feature/issue-7"));
}

/// `feature/harness-ops-<N>` (2AMLogic/harness-ops's Builder convention) is a
/// stackable parent too (2AMLogic/2am#1298, #1396) — the same two-name
/// allow-list the bash-side `_stacked_branch_issue_num` helper recognizes.
#[test]
fn a_harness_ops_branch_is_also_a_stackable_parent() {
    assert!(is_stackable_parent_branch("feature/harness-ops-350"));
    assert!(is_stackable_parent_branch("feature/harness-ops-7"));
}

/// The retired anchored regex was `^feature/issue-([0-9]+)$`, and the whole
/// point of the anchors is that `release-1` / `fix-bug-42` classify as PR-style
/// rather than issue-style. A `strip_prefix` port is only equivalent if the
/// trailing digit requirement is enforced too, which these pin.
#[test]
fn a_near_miss_branch_name_is_not_a_stackable_parent() {
    for branch in [
        "release-1",
        "fix-bug-42",
        "feature/issue-",
        "feature/issue-100-extra",
        "feature/issue-100/sub",
        "feature/issue-abc",
        "feature/issue-1a",
        "xfeature/issue-1",
        "feature/issue-1 ",
        "",
    ] {
        assert!(
            !is_stackable_parent_branch(branch),
            "{branch} must not be treated as a stackable parent"
        );
    }
}

/// The generalized match stays just as strict/anchored on the harness-ops
/// shape — these near-misses must not slip through the two-name allow-list
/// (2AMLogic/2am#1298, #1396's T12).
#[test]
fn a_harness_ops_near_miss_branch_name_is_not_a_stackable_parent() {
    for branch in [
        "feature/harness-ops-",
        "feature/harness-ops-350-extra",
        "feature/harness-ops-350/sub",
        "feature/harness-ops-abc",
        "feature/other-350",
    ] {
        assert!(
            !is_stackable_parent_branch(branch),
            "{branch} must not be treated as a stackable parent"
        );
    }
}

// --- parsing and re-serialization -----------------------------------------

#[test]
fn children_are_parsed_from_the_gh_json_shape() {
    let parsed = parse_children(
        r#"[{"number":501,"headRefName":"feature/issue-201"},{"number":502,"headRefName":"feature/issue-202"}]"#,
    );
    assert_eq!(parsed, vec![kid(501, "feature/issue-201"), kid(502, "feature/issue-202")]);
}

/// Every read in the retired shell was `jq … 2>/dev/null || echo 0`, so garbage
/// meant "no children" and the guard skipped. The port must skip the same way —
/// a parse error that surfaced as a refusal would stop merges on malformed forge
/// output, which is the opposite of the fail-open this guard argues for.
#[test]
fn unparseable_or_non_array_output_yields_no_children() {
    for raw in [
        "",
        "not json",
        "{}",
        "null",
        "[",
        r#"{"number":1}"#,
        r#"{"a":1,"b":2}"#,
    ] {
        assert!(parse_children(raw).is_empty(), "{raw:?} must yield no children");
    }
}

/// A row with no usable `number` is dropped rather than defaulted — the one
/// recorded divergence from the retired shell, which rendered it as the literal
/// `null` (see `parse_children`'s docs and the differential's divergence table).
/// A numeric STRING is accepted, because the retired `(.number|tostring)` turned
/// one into a working command and dropping it would lose a real child.
#[test]
fn a_row_without_a_usable_number_is_dropped_but_a_numeric_string_is_kept() {
    let parsed = parse_children(
        r#"[{"headRefName":"feature/issue-9"},{"number":"501"},{"number":502,"headRefName":"b"}]"#,
    );
    assert_eq!(parsed, vec![kid(501, ""), kid(502, "b")]);
}

/// A missing `headRefName` keeps the child (its number is what the guard's
/// messages need); the empty branch only reaches the post-merge reconcile, which
/// skips a row with no branch of its own.
#[test]
fn a_row_without_a_head_ref_name_keeps_the_child_with_an_empty_branch() {
    assert_eq!(parse_children(r#"[{"number":7}]"#), vec![kid(7, "")]);
}

/// `STACKED_CHILDREN_JSON` is consumed by `jq 'length'` and
/// `jq -r '.[] | "\(.number)\t\(.headRefName)"'`, and the retained suite asserts
/// the record contains the literal `"number":501`. Compact, no spaces.
#[test]
fn the_children_record_is_compact_json_the_shell_consumer_can_read() {
    let json = children_json(&[kid(501, "feature/issue-201")]);
    assert_eq!(json, r#"[{"number":501,"headRefName":"feature/issue-201"}]"#);
    assert!(json.contains(r#""number":501"#));
}

#[test]
fn an_empty_children_record_is_an_empty_array_not_an_empty_string() {
    assert_eq!(children_json(&[]), "[]");
}

// --- message building blocks ----------------------------------------------

#[test]
fn the_child_list_is_hash_prefixed_and_comma_separated() {
    assert_eq!(child_list(&[kid(501, "a"), kid(502, "b")]), "#501, #502");
    assert_eq!(child_list(&[kid(501, "a")]), "#501");
}

#[test]
fn each_child_gets_its_own_two_space_indented_reconcile_command() {
    assert_eq!(
        reconcile_commands(&[kid(501, "a"), kid(502, "b")], "feature/issue-100"),
        "  ./.loom/scripts/reconcile-stack.sh 501 feature/issue-100\n  ./.loom/scripts/reconcile-stack.sh 502 feature/issue-100"
    );
}

/// The retired `jq … || echo "  ./.loom/scripts/reconcile-stack.sh <child-pr>
/// $PR_BRANCH"` fallback. Unreachable from `decide` (which skips on an empty
/// list) but preserved so no caller can render an empty command block.
#[test]
fn an_empty_child_list_still_renders_a_usable_placeholder_command() {
    assert_eq!(
        reconcile_commands(&[], "feature/issue-100"),
        "  ./.loom/scripts/reconcile-stack.sh <child-pr> feature/issue-100"
    );
}

#[test]
fn the_pin_ref_lives_under_the_namespace_reconcile_stack_reads() {
    assert_eq!(pin_ref("feature/issue-100"), "refs/loom/parent/feature/issue-100");
    assert!(pin_ref("feature/issue-100").starts_with(PIN_NAMESPACE));
}

// --- the decision ladder --------------------------------------------------

#[test]
fn a_non_stackable_parent_branch_skips_before_anything_else() {
    let children = [kid(503, "feature/issue-201")];
    let mut i = inputs("release-1", &children);
    i.allow_stacked_children = true;
    i.dry_run = true;
    assert_eq!(decide(&i), Outcome::Skip);
}

#[test]
fn no_open_children_skips() {
    assert_eq!(decide(&inputs("feature/issue-100", &[])), Outcome::Skip);
}

#[test]
fn an_open_child_with_no_flags_needs_a_pin() {
    let children = [kid(501, "feature/issue-201")];
    assert_eq!(decide(&inputs("feature/issue-100", &children)), Outcome::NeedsPin);
}

/// The bypass is tested BEFORE dry-run, as the retired shell had it. Reporting
/// the pin-or-block path under `--dry-run` when the operator has already opted
/// out of it would describe a run that will not happen.
#[test]
fn the_bypass_wins_over_dry_run() {
    let children = [kid(501, "feature/issue-201")];
    let mut i = inputs("feature/issue-100", &children);
    i.allow_stacked_children = true;
    i.dry_run = true;
    let Outcome::Bypass(msg) = decide(&i) else {
        panic!("expected a bypass");
    };
    assert!(msg.contains("--allow-stacked-children set"));
    assert!(!msg.contains("[dry-run]"));
}

#[test]
fn the_bypass_warning_names_the_count_the_children_and_the_branch() {
    let children = [kid(501, "feature/issue-201"), kid(502, "feature/issue-202")];
    let mut i = inputs("feature/issue-100", &children);
    i.allow_stacked_children = true;
    assert_eq!(
        decide(&i),
        Outcome::Bypass(
            "Merge-ordering guard: --allow-stacked-children set; proceeding despite 2 open stacked child PR(s) (#501, #502) targeting 'feature/issue-100' (operator asserts they are reconciled)".to_string()
        )
    );
}

#[test]
fn the_dry_run_report_names_the_pin_it_would_write_and_says_it_wrote_nothing() {
    let children = [kid(501, "feature/issue-201")];
    let mut i = inputs("feature/issue-100", &children);
    i.dry_run = true;
    let Outcome::DryRun(msg) = decide(&i) else {
        panic!("expected a dry-run report");
    };
    assert!(msg.starts_with(
        "[dry-run] 1 open stacked child PR(s) (#501) still target 'feature/issue-100'."
    ));
    assert!(msg.contains("refs/loom/parent/feature/issue-100"));
    assert!(msg.ends_with("No ref was written."));
}

// --- the rendered pin / block messages ------------------------------------

#[test]
fn the_pinned_warning_names_the_ref_the_sha_and_every_child_command() {
    let children = [kid(501, "feature/issue-201"), kid(502, "feature/issue-202")];
    let msg = pinned_message(&inputs("feature/issue-100", &children));
    assert!(msg.starts_with("Merge-ordering guard: PR #999's branch 'feature/issue-100' still has 2 open stacked child PR(s) (#501, #502) targeting it."));
    assert!(msg.contains("Pinned the parent tip to refs/loom/parent/feature/issue-100 (abc123)"));
    assert!(msg.ends_with("reconcile each child once this has landed:\n  ./.loom/scripts/reconcile-stack.sh 501 feature/issue-100\n  ./.loom/scripts/reconcile-stack.sh 502 feature/issue-100"));
}

/// The refusal has to carry all three recovery routes the retained suite checks
/// for: what is blocking (the child numbers), how to clear it
/// (`reconcile-stack.sh`), and how to override it deliberately.
#[test]
fn the_block_message_names_the_children_the_fix_and_the_override() {
    let children = [kid(501, "feature/issue-201")];
    let msg = blocked_message(&inputs("feature/issue-100", &children));
    assert!(msg.starts_with("Merge blocked: PR #999's branch 'feature/issue-100'"));
    assert!(msg.contains("(#501)"));
    assert!(
        msg.contains("its tip (abc123) could not be pinned to refs/loom/parent/feature/issue-100")
    );
    assert!(msg.contains("  ./.loom/scripts/reconcile-stack.sh 501 feature/issue-100"));
    assert!(msg.ends_with("re-run with --allow-stacked-children to bypass this guard."));
}

#[test]
fn the_block_message_explains_the_delete_branch_on_merge_race() {
    let children = [kid(501, "a")];
    let msg = blocked_message(&inputs("feature/issue-100", &children));
    assert!(msg.contains("delete_branch_on_merge"));
    assert!(msg.contains("#3747 item 2"));
}

// --- the pin, against a real repository -----------------------------------

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A repo with one commit on `feature/issue-100`, and an `origin` that does
    /// NOT have it — so `establish_pin`'s fetch fallback is exercised as a
    /// no-help-needed path here and as a genuinely-failing one in the
    /// unresolvable case.
    fn new() -> (Self, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for args in [
            vec!["init", "--quiet", "-b", "main"],
            vec!["config", "user.email", "t@loom.test"],
            vec!["config", "user.name", "Loom Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(git_ok(root, &args), "git {args:?} failed");
        }
        std::fs::write(root.join("base.txt"), "base").expect("write");
        assert!(git_ok(root, &["add", "base.txt"]));
        assert!(git_ok(root, &["commit", "-q", "-m", "base"]));
        assert!(git_ok(root, &["checkout", "-q", "-b", "feature/issue-100"]));
        let sha = String::from_utf8_lossy(
            &Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .trim()
        .to_string();
        (Self { dir }, sha)
    }

    fn root(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn pinned(&self, branch: &str) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args(["rev-parse", "--verify", "--quiet", &pin_ref(branch)])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

fn git_ok(root: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn a_resolvable_tip_is_pinned_to_the_ref_reconcile_stack_reads() {
    let (sb, sha) = Sandbox::new();
    assert!(establish_pin(sb.root(), "feature/issue-100", &sha));
    assert_eq!(sb.pinned("feature/issue-100").as_deref(), Some(sha.as_str()));
}

/// The one case the guard still refuses on. Both the local object check and the
/// fetch fallback must fail — and crucially, NO ref may be left behind: a ref
/// naming a missing object would satisfy a naive "did we write a ref" check
/// while being useless to the later rebase, which is the whole reason
/// `establish_pin` verifies before writing.
#[test]
fn an_unresolvable_tip_is_not_pinned_and_leaves_no_ref() {
    let (sb, _sha) = Sandbox::new();
    let nowhere = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    assert!(!establish_pin(sb.root(), "feature/issue-100", nowhere));
    assert_eq!(sb.pinned("feature/issue-100"), None);
}

#[test]
fn establishing_the_pin_twice_is_idempotent() {
    let (sb, sha) = Sandbox::new();
    assert!(establish_pin(sb.root(), "feature/issue-100", &sha));
    assert!(establish_pin(sb.root(), "feature/issue-100", &sha));
    assert_eq!(sb.pinned("feature/issue-100").as_deref(), Some(sha.as_str()));
}

/// A tree object is readable but is not a commit. `^{commit}` is what rejects
/// it, and dropping that peel would pin a ref a rebase cannot use.
#[test]
fn a_non_commit_object_does_not_satisfy_the_pin() {
    let (sb, _sha) = Sandbox::new();
    let tree = String::from_utf8_lossy(
        &Command::new("git")
            .arg("-C")
            .arg(sb.root())
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .expect("rev-parse tree")
            .stdout,
    )
    .trim()
    .to_string();
    assert!(!establish_pin(sb.root(), "feature/issue-100", &tree));
    assert_eq!(sb.pinned("feature/issue-100"), None);
}

/// A guard whose forge read fails must skip, not refuse. `discover_open_children`
/// is the only place that can decide this, so it is pinned here: a `gh` that does
/// not exist yields the same `[]` the retired `|| echo '[]'` produced.
#[test]
fn an_unrunnable_gh_discovers_no_children_rather_than_erroring() {
    let raw =
        discover_open_children("loom-no-such-gh-binary-8191", "owner/repo", "feature/issue-100");
    assert_eq!(raw, "[]");
    assert!(parse_children(&raw).is_empty());
}
