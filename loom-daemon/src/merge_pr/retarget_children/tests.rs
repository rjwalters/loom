//! Unit tests for the pre-delete stacked-child retarget (#9372). The
//! end-to-end wiring through `merge-pr.sh` is pinned by
//! `defaults/scripts/tests/test-merge-pr-stacked-child-branch-delete.sh`.

use std::cell::RefCell;

use super::*;

/// A scripted forge: one answer per `open_children` call, in order, and a set
/// of PR numbers whose retarget fails.
struct Fake {
    lists: RefCell<Vec<Result<Vec<Child>, String>>>,
    fail: Vec<i64>,
    edits: RefCell<Vec<(i64, String)>>,
}

impl Fake {
    fn new(lists: Vec<Result<Vec<Child>, String>>, fail: Vec<i64>) -> Self {
        Self {
            lists: RefCell::new(lists),
            fail,
            edits: RefCell::new(Vec::new()),
        }
    }
}

impl Forge for Fake {
    fn open_children(&self, _repo: &str, _branch: &str) -> Result<Vec<Child>, String> {
        let mut lists = self.lists.borrow_mut();
        assert!(!lists.is_empty(), "unexpected extra child listing");
        lists.remove(0)
    }
    fn retarget(&self, _repo: &str, number: i64, base: &str) -> Result<(), String> {
        self.edits.borrow_mut().push((number, base.to_string()));
        if self.fail.contains(&number) {
            Err("boom".to_string())
        } else {
            Ok(())
        }
    }
}

fn child(n: i64) -> Child {
    Child {
        number: n,
        head_ref_name: format!("feature/issue-{n}"),
    }
}

const R: &str = "o/r";
const B: &str = "feature/issue-1";

#[test]
fn no_children_deletes_silently_after_one_query() {
    let f = Fake::new(vec![Ok(vec![])], vec![]);
    let r = prepare_delete(&f, R, B, "main");
    assert_eq!(r.verdict, Verdict::Delete);
    assert!(r.lines.is_empty());
    assert!(f.edits.borrow().is_empty());
    assert!(f.lists.borrow().is_empty());
}

#[test]
fn no_children_deletes_even_with_no_base() {
    let f = Fake::new(vec![Ok(vec![])], vec![]);
    assert_eq!(prepare_delete(&f, R, B, "").verdict, Verdict::Delete);
}

#[test]
fn every_open_child_is_retargeted_then_rechecked_before_delete() {
    let f = Fake::new(vec![Ok(vec![child(2), child(3)]), Ok(vec![])], vec![]);
    let r = prepare_delete(&f, R, B, "main");
    assert_eq!(r.verdict, Verdict::Delete);
    assert_eq!(*f.edits.borrow(), vec![(2, "main".to_string()), (3, "main".to_string())]);
    assert_eq!(r.lines.len(), 2);
    assert!(r.lines.iter().all(|(l, _)| *l == Level::Info));
    assert!(r.lines[0]
        .1
        .contains("reconcile-stack.sh 2 feature/issue-1"));
}

#[test]
fn a_failed_retarget_keeps_the_branch_and_names_the_child() {
    let f = Fake::new(vec![Ok(vec![child(2), child(3)])], vec![3]);
    let r = prepare_delete(&f, R, B, "main");
    assert_eq!(r.verdict, Verdict::Keep);
    let (lvl, msg) = r.lines.last().unwrap();
    assert_eq!(*lvl, Level::Warning);
    assert!(msg.contains("#3 (boom)"), "{msg}");
    assert!(msg.contains("Keeping remote branch 'feature/issue-1'"), "{msg}");
    // The successful one is still reported.
    assert!(r.lines[0].1.contains("#2"));
}

#[test]
fn an_unanswered_query_keeps_the_branch_and_mutates_nothing() {
    let f = Fake::new(vec![Err("HTTP 502".to_string())], vec![]);
    let r = prepare_delete(&f, R, B, "main");
    assert_eq!(r.verdict, Verdict::Keep);
    assert!(f.edits.borrow().is_empty());
    assert!(r.lines[0].1.contains("HTTP 502"));
}

#[test]
fn a_child_still_present_on_recheck_keeps_the_branch() {
    let f = Fake::new(vec![Ok(vec![child(2)]), Ok(vec![child(9)])], vec![]);
    let r = prepare_delete(&f, R, B, "main");
    assert_eq!(r.verdict, Verdict::Keep);
    assert!(r.lines.last().unwrap().1.contains("#9 still target it"));
}

#[test]
fn an_unanswered_recheck_keeps_the_branch() {
    let f = Fake::new(vec![Ok(vec![child(2)]), Err("timeout".to_string())], vec![]);
    assert_eq!(prepare_delete(&f, R, B, "main").verdict, Verdict::Keep);
}

#[test]
fn children_with_no_known_base_keep_the_branch_without_editing() {
    for base in ["", B] {
        let f = Fake::new(vec![Ok(vec![child(2)])], vec![]);
        let r = prepare_delete(&f, R, B, base);
        assert_eq!(r.verdict, Verdict::Keep, "base={base:?}");
        assert!(f.edits.borrow().is_empty());
    }
}

#[test]
fn strict_parse_accepts_rows_and_rejects_anything_unreadable() {
    assert_eq!(parse_children_strict("[]").unwrap(), vec![]);
    assert_eq!(
        parse_children_strict(r#"[{"number":5,"headRefName":"x"}]"#).unwrap(),
        vec![Child {
            number: 5,
            head_ref_name: "x".into()
        }]
    );
    for bad in [
        "",
        "null",
        "{}",
        "not json",
        r#"[{"headRefName":"x"}]"#,
        r#"[{"number":"5"}]"#,
    ] {
        assert!(parse_children_strict(bad).is_err(), "{bad:?}");
    }
}

/// Judge P2 (#10406): both live calls carry the typed `owner/repo` target, so
/// the facade's owner credential selection and reader routing see the repo
/// (`--repo` in argv populates neither). `GH_REPO` in the env plan is derived
/// from that same typed target.
#[test]
fn live_invocations_carry_the_typed_repo_target() {
    let list = list_invocation("acme/w", B).unwrap();
    let edit = edit_invocation("acme/w", 7, "main").unwrap();
    for (inv, intent) in [(&list, AccessIntent::Read), (&edit, AccessIntent::Write)] {
        assert_eq!(inv.target().slug().as_deref(), Some("acme/w"));
        assert_eq!(inv.intent(), intent);
        let plan = inv.env_plan(None);
        assert!(
            plan.iter()
                .any(|e| e.key == "GH_REPO" && e.value.as_deref() == Some("acme/w".as_ref())),
            "{plan:?}"
        );
    }
}

/// An invalid repo slug is an error before any `gh` runs, and the decision
/// then keeps the branch: the delete stays denied.
#[test]
fn an_invalid_repo_slug_fails_closed_without_running_gh() {
    for bad in ["", "acme", "acme/w/x", "acme/ w", "/w"] {
        assert!(list_invocation(bad, B).is_err(), "{bad:?}");
        assert!(edit_invocation(bad, 7, "main").is_err(), "{bad:?}");
        assert!(GhForge.open_children(bad, B).is_err(), "{bad:?}");
        assert!(GhForge.retarget(bad, 7, "main").is_err(), "{bad:?}");
        let r = prepare_delete(&GhForge, bad, B, "main");
        assert_eq!(r.verdict, Verdict::Keep, "{bad:?}");
    }
}
