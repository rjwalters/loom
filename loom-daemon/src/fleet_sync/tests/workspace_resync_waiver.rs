//! The resync waiver proof and the daemon's landing on it (#13487), against
//! a fake forge: eligible-merge, ineligible and one-byte fallbacks, red or
//! pending checks, an unreadable input, a moved head, and a merge refusal
//! (which is what a merge-queue repo without a Judge verdict gives).

use std::cell::{Cell, RefCell};

use super::*;

const HEAD: &str = "aaaa";
const TITLE: &str = "chore(loom): resync installed Loom to v1.2.3";

struct Fake {
    title: String,
    heads: RefCell<Vec<String>>,
    meta: Result<String, String>,
    tag: Result<bool, String>,
    files: Result<Vec<File>, String>,
    changed: usize,
    upstream: Vec<(String, String)>,
}

fn file(name: &str, sha: &str, status: &str) -> File {
    File {
        filename: name.to_string(),
        previous_filename: None,
        sha: sha.to_string(),
        status: status.to_string(),
    }
}

impl Fake {
    /// A resync with one kept-set file (`.loom/hooks/guard-x.sh`) and one
    /// ordinary file, both byte-identical to the tag.
    fn ok() -> Self {
        Self {
            title: TITLE.to_string(),
            heads: RefCell::new(vec![HEAD.to_string(), HEAD.to_string()]),
            meta: Ok(r#"{"loom_version":"1.2.3"}"#.to_string()),
            tag: Ok(true),
            files: Ok(vec![
                file(".loom/hooks/guard-x.sh", "b1", "modified"),
                file(".loom/docs/readme.md", "b2", "modified"),
            ]),
            changed: 2,
            upstream: vec![("defaults/hooks/guard-x.sh".to_string(), "b1".to_string())],
        }
    }
}

impl Source for Fake {
    fn pr(&self, _: u32) -> Result<Pr, String> {
        let mut heads = self.heads.borrow_mut();
        let head_sha = if heads.len() > 1 {
            heads.remove(0)
        } else {
            heads[0].clone()
        };
        Ok(Pr {
            title: self.title.clone(),
            head_sha,
            changed_files: self.changed,
        })
    }
    fn files(&self, _: u32) -> Result<Vec<File>, String> {
        self.files.clone()
    }
    fn metadata(&self, _: &str) -> Result<String, String> {
        self.meta.clone()
    }
    fn tag_exists(&self, _: &str) -> Result<bool, String> {
        self.tag.clone()
    }
    fn upstream_blob(&self, path: &str, _: &str) -> Result<Option<String>, String> {
        Ok(self
            .upstream
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| s.clone()))
    }
}

fn why(v: &Verdict) -> &str {
    match v {
        Verdict::NotEligible(w) => w,
        Verdict::Eligible { .. } => "ELIGIBLE",
    }
}

#[test]
fn a_byte_identical_resync_is_eligible_and_records_the_kept_set_file() {
    let v = evaluate(&Fake::ok(), 1);
    assert_eq!(
        v,
        Verdict::Eligible {
            tag: "v1.2.3".to_string(),
            head: HEAD.to_string(),
            record: vec![(".loom/hooks/guard-x.sh".to_string(), "b1".to_string())],
        }
    );
}

#[test]
fn one_byte_of_difference_is_not_eligible() {
    let mut f = Fake::ok();
    f.upstream = vec![("defaults/hooks/guard-x.sh".to_string(), "b1-different".to_string())];
    assert!(why(&evaluate(&f, 1)).contains("differs from v1.2.3:defaults/hooks/guard-x.sh"));
}

#[test]
fn every_unreadable_input_fails_closed() {
    let mut f = Fake::ok();
    f.files = Err("HTTP 502".to_string());
    assert_eq!(why(&evaluate(&f, 1)), "files unreadable");
    let mut f = Fake::ok();
    f.meta = Err("HTTP 404".to_string());
    assert!(why(&evaluate(&f, 1)).starts_with("install-metadata names 'none'"));
    let mut f = Fake::ok();
    f.tag = Err("HTTP 500".to_string());
    assert!(why(&evaluate(&f, 1)).contains("tag v1.2.3 not found"));
    let mut f = Fake::ok();
    f.tag = Ok(false);
    assert!(why(&evaluate(&f, 1)).contains("tag v1.2.3 not found"));
    let mut f = Fake::ok();
    f.changed = 3;
    assert_eq!(why(&evaluate(&f, 1)), "files read incomplete");
}

#[test]
fn the_title_shape_is_strict() {
    for title in [
        "chore(loom): resync installed Loom to v1.2",
        "chore(loom): resync installed Loom to v1.2.3-rc1",
        "chore(loom): bump Repo Skills to v1.2.3",
        "chore(loom): resync installed Loom to v1.2.3 extra",
    ] {
        let mut f = Fake::ok();
        f.title = title.to_string();
        assert_eq!(why(&evaluate(&f, 1)), "not a resync PR", "{title}");
    }
}

#[test]
fn metadata_must_name_the_title_version() {
    let mut f = Fake::ok();
    f.meta = Ok(r#"{"loom_version":"1.2.4"}"#.to_string());
    assert!(why(&evaluate(&f, 1)).contains("names '1.2.4', title names 1.2.3"));
}

#[test]
fn outside_the_install_set_or_a_gate_path_holds_the_whole_pr() {
    let mut f = Fake::ok();
    f.files = Ok(vec![
        file("src/main.rs", "x", "modified"),
        file(".loom/hooks/guard-x.sh", "b1", "modified"),
    ]);
    assert!(why(&evaluate(&f, 1)).contains("outside the install set"));
    let mut f = Fake::ok();
    f.files = Ok(vec![
        file(".loom/scripts/merge-pr.sh", "b1", "modified"),
        file(".loom/hooks/guard-x.sh", "b1", "modified"),
    ]);
    assert!(why(&evaluate(&f, 1)).contains("merge-gate path"));
}

#[test]
fn a_removed_or_renamed_kept_set_file_holds() {
    let mut f = Fake::ok();
    f.files = Ok(vec![
        file(".loom/hooks/guard-x.sh", "b1", "removed"),
        file(".loom/docs/readme.md", "b2", "modified"),
    ]);
    assert_eq!(why(&evaluate(&f, 1)), ".loom/hooks/guard-x.sh is removed");
}

#[test]
fn a_kept_set_file_without_an_upstream_counterpart_holds() {
    let mut f = Fake::ok();
    f.files = Ok(vec![
        file(".claude/settings.json", "b1", "modified"),
        file(".loom/docs/readme.md", "b2", "modified"),
    ]);
    assert!(why(&evaluate(&f, 1)).contains("has no upstream counterpart"));
}

#[test]
fn a_resync_with_no_kept_set_file_has_nothing_to_waive() {
    let mut f = Fake::ok();
    f.files = Ok(vec![file(".loom/docs/readme.md", "b2", "modified")]);
    f.changed = 1;
    assert_eq!(why(&evaluate(&f, 1)), "no kept-set file to waive");
}

#[test]
fn a_head_that_moves_while_reading_holds() {
    let f = Fake::ok();
    *f.heads.borrow_mut() = vec![HEAD.to_string(), "bbbb".to_string()];
    assert_eq!(why(&evaluate(&f, 1)), "head moved while reading");
}

struct Fx {
    checks: Checks,
    merge: Result<(), String>,
    comments: RefCell<Vec<String>>,
    labels: RefCell<Vec<(Vec<String>, Vec<String>)>>,
    merged: Cell<Option<String>>,
    present: Vec<String>,
}

impl Fx {
    fn new(checks: Checks, merge: Result<(), String>) -> Self {
        Self {
            checks,
            merge,
            comments: RefCell::default(),
            labels: RefCell::default(),
            merged: Cell::new(None),
            present: Vec::new(),
        }
    }
}

impl Landing for Fx {
    fn checks(&self, _: u32, head: &str) -> Checks {
        assert_eq!(head, HEAD, "checks are read on the proven head");
        self.checks.clone()
    }
    fn labels(&self, _: u32) -> Result<Vec<String>, String> {
        Ok(self.present.clone())
    }
    fn relabel(&self, _: u32, add: &[&str], remove: &[&str]) -> Result<(), String> {
        self.labels.borrow_mut().push((
            add.iter().map(ToString::to_string).collect(),
            remove.iter().map(ToString::to_string).collect(),
        ));
        Ok(())
    }
    fn comment(&self, _: u32, body: &str) -> Result<(), String> {
        self.comments.borrow_mut().push(body.to_string());
        Ok(())
    }
    fn merge(&self, _: u32, head: &str) -> Result<(), String> {
        self.merged.set(Some(head.to_string()));
        self.merge.clone()
    }
}

#[test]
fn eligible_and_green_merges_pinned_to_the_head_with_the_record_and_loom_pr() {
    let fx = Fx::new(Checks::Green, Ok(()));
    let out = land(&Fake::ok(), &fx, 1);
    assert_eq!(out.word(), "landed-by-daemon", "{out:?}");
    assert_eq!(fx.merged.take().as_deref(), Some(HEAD));
    let comments = fx.comments.borrow();
    assert!(comments[0].contains(".loom/hooks/guard-x.sh: b1") && comments[0].contains("v1.2.3"));
    // loom:pr only; review-requested is never touched.
    assert_eq!(*fx.labels.borrow(), vec![(vec!["loom:pr".to_string()], vec![])]);
}

#[test]
fn an_ineligible_resync_merges_nothing_and_takes_the_lane() {
    let mut f = Fake::ok();
    f.upstream = vec![("defaults/hooks/guard-x.sh".to_string(), "other".to_string())];
    let fx = Fx::new(Checks::Green, Ok(()));
    let out = land(&f, &fx, 1);
    assert_eq!(out.word(), "handed-to-lane");
    assert!(out.reason().starts_with("waiver proof failed:"), "{out:?}");
    assert!(
        fx.merged.take().is_none()
            && fx.comments.borrow().is_empty()
            && fx.labels.borrow().is_empty()
    );
}

#[test]
fn red_or_pending_checks_take_the_lane_without_a_comment_or_label() {
    for why in [
        "LOOM-CHECKS-RED aaaa build",
        "LOOM-CHECKS-TIMEOUT aaaa build",
    ] {
        let fx = Fx::new(Checks::NotGreen(why.to_string()), Ok(()));
        let out = land(&Fake::ok(), &fx, 1);
        assert_eq!(out.word(), "handed-to-lane");
        assert!(out.reason().contains(why), "{out:?}");
        assert!(
            fx.merged.take().is_none()
                && fx.comments.borrow().is_empty()
                && fx.labels.borrow().is_empty()
        );
    }
}

#[test]
fn a_refused_merge_such_as_a_queue_repo_without_a_verdict_restores_the_lane_label() {
    let fx = Fx::new(Checks::Green, Err("no Judge verdict for the queue".to_string()));
    let out = land(&Fake::ok(), &fx, 1);
    assert_eq!(out.word(), "handed-to-lane");
    assert!(out.reason().contains("no Judge verdict for the queue"));
    assert_eq!(
        *fx.labels.borrow(),
        vec![
            (vec!["loom:pr".to_string()], vec![]),
            (vec![], vec!["loom:pr".to_string()])
        ]
    );
}

#[test]
fn a_refusing_merge_leaves_no_comment_across_passes() {
    let fx = Fx::new(Checks::Green, Err("hold".to_string()));
    for _ in 0..2 {
        assert_eq!(land(&Fake::ok(), &fx, 1).word(), "handed-to-lane");
    }
    assert!(fx.comments.borrow().is_empty());
}

#[test]
fn a_refused_merge_keeps_a_loom_pr_the_daemon_did_not_add() {
    let mut fx = Fx::new(Checks::Green, Err("hold".to_string()));
    fx.present = vec!["loom:pr".to_string()];
    assert_eq!(land(&Fake::ok(), &fx, 1).word(), "handed-to-lane");
    assert!(fx.labels.borrow().is_empty(), "{:?}", fx.labels.borrow());
}

/// `KEPT_SET` is a copy of Champion's hardcoded `KEPT_SET_PATTERNS` (still
/// used by the other kept-set checks in the prompt); this keeps them equal.
#[test]
fn the_kept_set_equals_championss_kept_set_patterns() {
    let md = include_str!("../../../../defaults/.claude/commands/loom/champion-pr-merge.md");
    let block = md
        .split("\nKEPT_SET_PATTERNS=(\n")
        .nth(1)
        .and_then(|rest| rest.split("\n)\n").next())
        .expect("KEPT_SET_PATTERNS block");
    let mut in_md: Vec<&str> = block
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split('"').skip(1).step_by(2))
        .collect();
    let mut in_rust = KEPT_SET.to_vec();
    in_md.sort_unstable();
    in_rust.sort_unstable();
    assert_eq!(in_rust, in_md);
}

/// `GATE` is the merge-gate head of the same array.
#[test]
fn the_gate_list_is_the_head_of_the_kept_set() {
    assert_eq!(GATE, &KEPT_SET[..GATE.len()]);
}
