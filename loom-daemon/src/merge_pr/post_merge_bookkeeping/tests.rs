use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Default)]
struct Fake {
    issues: HashMap<String, String>,
    kinds: HashMap<String, String>,
    commits: String,
    fail_helpers: Vec<&'static str>,
    close_fails: bool,
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Host for Fake {
    fn read_issue(&self, _: &str, i: &str) -> String {
        self.calls.borrow_mut().push(format!("read {i}"));
        self.issues.get(i).cloned().unwrap_or_else(|| "{}".into())
    }
    fn commit_messages(&self, _: &str, _: &str) -> String {
        self.commits.clone()
    }
    fn issue_kind(&self, _: &str, i: &str) -> String {
        self.kinds.get(i).cloned().unwrap_or_default()
    }
    fn helper(&self, func: &str, args: &[&str]) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("{func} {}", args[..args.len().min(4)].join(" ")));
        !self.fail_helpers.contains(&func)
    }
    fn close_issue(&self, _: &str, i: &str, _: &str) -> bool {
        self.calls.borrow_mut().push(format!("close {i}"));
        !self.close_fails
    }
    fn timestamp(&self) -> String {
        "T".into()
    }
}

fn inputs(body: &str) -> Inputs {
    Inputs {
        repo: "o/r".into(),
        pr_number: "9".into(),
        pr_branch: "feature/issue-42".into(),
        pr_body: body.into(),
        ..Inputs::default()
    }
}

fn done(o: Outcome) -> Vec<Record> {
    match o {
        Outcome::Done(r) => r,
        Outcome::Need(n) => panic!("unexpected need {n:?}"),
    }
}

fn levels(r: &[Record]) -> Vec<&str> {
    r.iter().map(|(l, _)| l.as_str()).collect()
}

const OPEN_BUILDING: &str = r#"{"state":"open","labels":[{"name":"loom:building"}]}"#;
const CLOSED_BUILDING: &str = r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#;

#[test]
fn partial_reset_swaps_and_comments() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), OPEN_BUILDING.into());
    let r = done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f));
    assert_eq!(levels(&r), ["INFO", "SUCCESS"]);
    let c = f.calls();
    assert_eq!(c[0], "read 5");
    assert_eq!(c[1], "forge_gh_swap_label_rl_safe o/r 5 loom:building loom:issue");
    assert!(c[2].starts_with("forge_gh_comment_rl_safe o/r 5 ## Partial Increment Merged"));
    assert!(!c[2].contains("**Reopened**"));
}

#[test]
fn partial_reset_empty_body_does_nothing() {
    let f = Fake::default();
    assert!(done(run(Pass::PartialReset, &inputs(""), &Facts::default(), &f)).is_empty());
    assert!(f.calls().is_empty());
}

#[test]
fn partial_reset_reverts_a_premature_close_before_swapping() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), CLOSED_BUILDING.into());
    let mut i = inputs("Part of #5");
    i.conflicted = "5".into();
    let r = done(run(Pass::PartialReset, &i, &Facts::default(), &f));
    assert_eq!(levels(&r), ["WARNING", "SUCCESS", "INFO", "SUCCESS"]);
    let c = f.calls();
    assert!(c[1].starts_with("forge_gh_reopen_issue_rl_safe o/r 5"));
    assert!(c[2].starts_with("forge_gh_comment_rl_safe o/r 5 ## Premature Auto-Close Reverted"));
    assert!(c[3].starts_with("forge_gh_swap_label_rl_safe"));
    assert!(c[4].contains("**Reopened**"));
}

#[test]
fn partial_reset_stops_when_the_reopen_fails() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), CLOSED_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_reopen_issue_rl_safe"];
    let mut i = inputs("Part of #5");
    i.conflicted = "5".into();
    let r = done(run(Pass::PartialReset, &i, &Facts::default(), &f));
    assert!(r.last().unwrap().1.starts_with("Could not reopen issue #5"));
    assert!(!f.calls().iter().any(|c| c.contains("swap_label")));
}

#[test]
fn partial_reset_never_touches_a_pull_request() {
    let mut f = Fake::default();
    f.issues
        .insert("5".into(), r#"{"pull_request":{},"state":"open"}"#.into());
    assert!(done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f)).is_empty());
    assert_eq!(f.calls(), ["read 5"]);
}

#[test]
fn partial_reset_failed_swap_warns_and_posts_no_comment() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), OPEN_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_swap_label_rl_safe"];
    let r = done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f));
    assert_eq!(levels(&r), ["INFO", "WARNING"]);
    assert!(!f.calls().iter().any(|c| c.contains("comment")));
}

#[test]
fn partial_reset_comment_failure_is_a_warning_after_the_swap() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), OPEN_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_comment_rl_safe"];
    let r = done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f));
    assert_eq!(levels(&r), ["INFO", "SUCCESS", "WARNING"]);
    assert!(r[2].1.contains("label swap still applied"));
}

#[test]
fn branch_issue_is_exact() {
    assert_eq!(branch_issue("feature/issue-42"), Some("42"));
    for b in [
        "feature/issue-",
        "feature/issue-4x",
        "x/feature/issue-4",
        "feature/issue-4-slice",
        "main",
        "",
    ] {
        assert_eq!(branch_issue(b), None, "{b}");
    }
}

#[test]
fn branch_close_asks_for_close_targets_before_anything() {
    let f = Fake::default();
    assert_eq!(
        run(Pass::BranchClose, &inputs("body"), &Facts::default(), &f),
        Outcome::Need(Need::CloseTargets)
    );
    assert!(f.calls().is_empty());
}

fn facts(t: &str) -> Facts {
    Facts {
        close_targets: Some(t.into()),
    }
}

#[test]
fn branch_close_closes_an_open_undeclared_issue() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    let r = done(run(Pass::BranchClose, &inputs("no keyword"), &facts(""), &f));
    assert_eq!(levels(&r), ["SUCCESS"]);
    assert_eq!(f.calls(), ["close 42"]);
}

#[test]
fn branch_close_skips_each_reason_to_leave_it_open() {
    let open = |f: &mut Fake| {
        f.kinds.insert("42".into(), "open".into());
    };
    // partial-increment declaration: decided before the close targets are asked for
    let mut f = Fake::default();
    open(&mut f);
    assert!(done(run(Pass::BranchClose, &inputs("Part of #42"), &Facts::default(), &f)).is_empty());
    // already a close target
    assert!(done(run(Pass::BranchClose, &inputs("x"), &facts("7\n42\n"), &f)).is_empty());
    // closing keyword in the body, then in a commit message
    assert!(done(run(Pass::BranchClose, &inputs("Fixes #42"), &facts(""), &f)).is_empty());
    f.commits = "wip\n\nresolved  #42".into();
    assert!(done(run(Pass::BranchClose, &inputs("x"), &facts(""), &f)).is_empty());
    // not open / a PR / unreadable
    for kind in ["closed", "pr", ""] {
        let mut g = Fake::default();
        g.kinds.insert("42".into(), kind.into());
        assert!(done(run(Pass::BranchClose, &inputs("x"), &facts(""), &g)).is_empty(), "{kind}");
    }
    assert!(!f.calls().iter().any(|c| c.starts_with("close")));
}

#[test]
fn branch_close_failure_is_a_warning() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    f.close_fails = true;
    let r = done(run(Pass::BranchClose, &inputs("x"), &facts(""), &f));
    assert_eq!(levels(&r), ["WARNING"]);
}

#[test]
fn branch_close_ignores_other_branches() {
    let f = Fake::default();
    let mut i = inputs("x");
    i.pr_branch = "docs/guide".into();
    assert!(done(run(Pass::BranchClose, &i, &Facts::default(), &f)).is_empty());
}

#[test]
fn closing_keyword_matching_is_line_local_and_word_bounded() {
    assert!(has_closing_keyword("CLOSES #42", "42"));
    assert!(has_closing_keyword("fix\t#42.", "42"));
    assert!(!has_closing_keyword("fix\n#42", "42"), "grep never spans lines");
    assert!(!has_closing_keyword("closes #421", "42"));
    assert!(!has_closing_keyword("disclose #42", "42"));
    assert!(!has_closing_keyword("fixes #4", "42"));
}

#[test]
fn strip_asks_first_then_strips_only_closed_claims() {
    let mut f = Fake::default();
    assert_eq!(
        run(Pass::Strip, &inputs(""), &Facts::default(), &f),
        Outcome::Need(Need::CloseTargets)
    );
    f.issues.insert("1".into(), CLOSED_BUILDING.into());
    f.issues.insert("2".into(), OPEN_BUILDING.into());
    f.issues.insert(
        "3".into(),
        r#"{"state":"closed","labels":[{"name":"loom:building"},{"name":"loom:issue"}]}"#.into(),
    );
    let r = done(run(Pass::Strip, &inputs(""), &facts("1\n2\n3\n"), &f));
    assert_eq!(levels(&r), ["SUCCESS", "SUCCESS", "SUCCESS"]);
    assert_eq!(
        f.calls()
            .iter()
            .filter(|c| c.contains("remove_label"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            "forge_gh_remove_label_rl_safe o/r 1 loom:building",
            "forge_gh_remove_label_rl_safe o/r 3 loom:building",
            "forge_gh_remove_label_rl_safe o/r 3 loom:issue",
        ]
    );
}

#[test]
fn strip_with_no_close_targets_is_a_quiet_no_op() {
    let f = Fake::default();
    assert!(done(run(Pass::Strip, &inputs(""), &facts(""), &f)).is_empty());
    assert!(f.calls().is_empty());
}

#[test]
fn strip_failed_removal_warns_with_the_manual_command() {
    let mut f = Fake::default();
    f.issues.insert("1".into(), CLOSED_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_remove_label_rl_safe"];
    let r = done(run(Pass::Strip, &inputs(""), &facts("1"), &f));
    assert_eq!(levels(&r), ["WARNING"]);
    assert!(r[0]
        .1
        .contains("gh issue edit 1 --repo o/r --remove-label loom:building"));
}
