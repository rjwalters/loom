//! Tests for the orphaned-stacked-child pass (#12760).
//!
//! The pure predicates are tested on values. The pass itself runs against
//! real repositories (a bare "origin" plus a clone, under `tempfile`) with an
//! in-memory forge, so the replay, the lease and the push are the real git
//! operations and only the forge is simulated.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

// --- pure predicates ---------------------------------------------------------

fn pr(number: u32, head: &str, base: &str) -> RestPull {
    RestPull {
        number,
        state: "open".to_string(),
        head_ref: Some(head.to_string()),
        head_sha: Some(format!("{number:040}")),
        base_ref: Some(base.to_string()),
        ..RestPull::default()
    }
}

fn closed(number: u32, merged: bool) -> ParentPr {
    ParentPr {
        number,
        merged,
        head_sha: Some("a".repeat(40)),
    }
}

#[test]
fn orphan_predicate_distinguishes_open_merged_closed_and_no_pr() {
    let open: BTreeSet<&str> = ["feature/issue-1"].into_iter().collect();
    assert_eq!(parent_state(&open, "feature/issue-1", None), ParentState::Open);
    // An open head wins even over a stale closed record for the same branch
    // (branch reuse across issue slices).
    assert_eq!(
        parent_state(&open, "feature/issue-1", Some(&closed(9, false))),
        ParentState::Open
    );
    let none = BTreeSet::new();
    assert_eq!(parent_state(&none, "feature/issue-2", None), ParentState::NoPr);
    assert_eq!(
        parent_state(&none, "feature/issue-2", Some(&closed(2, true))),
        ParentState::Merged
    );
    assert_eq!(
        parent_state(&none, "feature/issue-2", Some(&closed(2, false))),
        ParentState::ClosedUnmerged(closed(2, false))
    );
}

#[test]
fn candidates_are_open_prs_on_a_dead_looking_loom_base() {
    let rows = [
        pr(10, "feature/issue-10", "main"),
        pr(11, "feature/issue-11", "feature/issue-10"),
        pr(12, "feature/issue-12", "feature/issue-99"),
        pr(13, "feature/issue-13", "release/1.0"),
        pr(14, "feature/issue-14", "feature/issue-99-extra"),
    ];
    let heads: BTreeSet<&str> = rows.iter().filter_map(|p| p.head_ref.as_deref()).collect();
    let picked: Vec<u32> = rows
        .iter()
        .filter(|p| is_candidate(p, "main", &heads))
        .map(|p| p.number)
        .collect();
    // 10 targets main; 11's base is an open PR's head; 13/14 are not Loom
    // builder branches. Only 12 needs a parent lookup.
    assert_eq!(picked, vec![12]);
    let mut closed_row = pr(15, "feature/issue-15", "feature/issue-98");
    closed_row.state = "closed".to_string();
    assert!(!is_candidate(&closed_row, "main", &heads));
}

#[test]
fn gate_skips_holds_agents_in_flight_and_unusable_heads() {
    let mut p = pr(1, "feature/issue-1", "feature/issue-2");
    assert_eq!(gate(&p), None);
    p.labels = vec!["loom:frozen".to_string()];
    assert_eq!(gate(&p), Some(Skip::Held));
    p.labels = vec!["loom:blocked".to_string()];
    assert_eq!(gate(&p), Some(Skip::Held));
    p.labels = vec!["loom:treating".to_string()];
    assert_eq!(gate(&p), Some(Skip::InFlight));
    p.labels.clear();
    p.head_sha = Some("abc".to_string());
    assert_eq!(gate(&p), Some(Skip::NoHead));
    p.head_sha = Some("b".repeat(40));
    p.head_ref = Some("--upload-pack=/tmp/x".to_string());
    assert_eq!(gate(&p), Some(Skip::NoHead));
}

#[test]
fn newest_closed_pr_is_refiltered_on_head_and_picks_the_newest() {
    let body = serde_json::json!([
        {"number": 3, "state": "closed", "merged_at": "2026-10-01T00:00:00Z",
         "head": {"ref": "feature/issue-7", "sha": "c".repeat(40)}},
        {"number": 8, "state": "closed", "merged_at": null,
         "head": {"ref": "feature/issue-7", "sha": "d".repeat(40)}},
        // GitHub ignores an unresolvable `head=` and lists everything.
        {"number": 20, "state": "closed", "merged_at": null,
         "head": {"ref": "feature/issue-70", "sha": "e".repeat(40)}},
    ])
    .to_string();
    let got = parse_newest_closed(body.as_bytes(), "feature/issue-7").unwrap();
    assert_eq!(
        got,
        Some(ParentPr {
            number: 8,
            merged: false,
            head_sha: Some("d".repeat(40)),
        })
    );
    assert_eq!(parse_newest_closed(b"[]", "feature/issue-7").unwrap(), None);
    assert!(parse_newest_closed(b"{}", "feature/issue-7").is_err());
    let no_merged = r#"[{"number":1,"state":"closed","head":{"ref":"b","sha":"x"}}]"#;
    assert!(parse_newest_closed(no_merged.as_bytes(), "b").is_err());
}

#[test]
fn flag_marker_is_keyed_by_parent_and_head() {
    let head = "f".repeat(40);
    let ctx = FlagContext {
        pr: 5,
        branch: "feature/issue-5",
        head: &head,
        cause_pr: 4,
        orphan: true,
        onto: "origin/main",
        upstream: "abc",
        retarget_to: Some("main"),
    };
    let body = flag_comment_body(&ctx, &FlagReason::Conflict(vec!["a.txt".to_string()]));
    assert!(body.starts_with(&flagged_marker(4, &head)));
    assert!(body.contains("`a.txt`"));
    assert!(body.contains("git rebase --onto origin/main abc"));
    assert!(body.contains("gh pr edit 5 --base main"));
    assert!(body.contains(MERGE_CONFLICT));
    let bodies = vec![body];
    assert!(already_flagged(&bodies, 4, &head));
    assert!(!already_flagged(&bodies, 4, &"0".repeat(40)));
    assert!(!already_flagged(&bodies, 3, &head));
    assert!(ever_flagged(&bodies));
    let merge = flag_comment_body(&ctx, &FlagReason::MergeMode);
    assert!(merge.contains("did not retarget"));
}

#[test]
fn the_merge_mode_flag_names_the_policy_conflict_and_never_says_force_push() {
    let head = "f".repeat(40);
    let ctx = FlagContext {
        pr: 5,
        branch: "feature/issue-5",
        head: &head,
        cause_pr: 4,
        orphan: true,
        onto: "origin/main",
        upstream: "abc",
        retarget_to: Some("main"),
    };
    let merge = flag_comment_body(&ctx, &FlagReason::MergeMode);
    assert!(merge.contains("#10771"), "{merge}");
    assert!(merge.contains("conflicts with this repository's policy"), "{merge}");
    // The flag names its consumer (#12767): Doctor's replacement recipe, not
    // a human decision and not a tracking issue.
    assert!(merge.contains("Doctor picks this PR up"), "{merge}");
    assert!(merge.contains("replacement PR"), "{merge}");
    assert!(!merge.contains("tracked in #12767"), "{merge}");
    assert!(!merge.contains("a human decides"), "{merge}");
    for forbidden in ["--force", "git push", "git rebase", "gh pr edit"] {
        assert!(
            !merge.contains(forbidden),
            "merge-mode flag must not say `{forbidden}`:\n{merge}"
        );
    }
    // A foreign head gets no command recipe either (it may be a fork, where
    // `origin` is not its remote).
    let foreign = flag_comment_body(&ctx, &FlagReason::NotLoomBranch);
    assert!(foreign.contains("not a Loom branch"), "{foreign}");
    assert!(!foreign.contains("--force") && !foreign.contains("git push"), "{foreign}");
    // A rewritten parent: the recipe must not name the parent head as the
    // upstream, since that is exactly the wrong range.
    let rewritten = flag_comment_body(&ctx, &FlagReason::ParentRewritten);
    assert!(rewritten.contains("is not an ancestor"), "{rewritten}");
    assert!(!rewritten.contains("git rebase --onto origin/main abc"), "{rewritten}");
    assert!(
        rewritten.contains("<the parent commit this branch was cut from>"),
        "{rewritten}"
    );
}

#[test]
fn the_label_delete_path_percent_encodes_the_colon() {
    assert_eq!(
        label_delete_path(5, MERGE_CONFLICT),
        "repos/{owner}/{repo}/issues/5/labels/loom%3Amerge-conflict"
    );
}

#[test]
fn head_owner_admits_only_loom_branches_in_this_repository() {
    let mut p = pr(1, "feature/issue-1", "feature/issue-2");
    p.head_repo = Some("Acme/Widgets".to_string());
    assert_eq!(head_owner(&p, "main", Some("acme/widgets")), HeadOwner::Loom);
    assert_eq!(head_owner(&p, "main", None), HeadOwner::Unknown);
    p.head_repo = Some("someone/widgets".to_string());
    assert_eq!(head_owner(&p, "main", Some("acme/widgets")), HeadOwner::Foreign);
    p.head_repo = None; // a deleted fork
    assert_eq!(head_owner(&p, "main", Some("acme/widgets")), HeadOwner::Foreign);
    p.head_repo = Some("acme/widgets".to_string());
    for human in ["jdoe/fix", "feature/issue-1-extra", "feature/other-1"] {
        p.head_ref = Some(human.to_string());
        assert_eq!(head_owner(&p, "main", Some("acme/widgets")), HeadOwner::Foreign, "{human}");
    }
    p.head_ref = Some("main".to_string());
    assert_eq!(head_owner(&p, "main", Some("acme/widgets")), HeadOwner::DefaultBranch);
}

// --- the pass, on real repositories -----------------------------------------

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .output()
        .expect("git must spawn")
}

fn run(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    run(dir, &["add", "-A"]);
    run(dir, &["commit", "-q", "-m", msg]);
    run(dir, &["rev-parse", "HEAD"])
}

fn configure(dir: &Path) {
    run(dir, &["config", "user.email", "loom@example.com"]);
    run(dir, &["config", "user.name", "Loom Test"]);
    run(dir, &["config", "commit.gpgsign", "false"]);
}

const A: &str = "feature/issue-901";
const B: &str = "feature/issue-902";
const C: &str = "feature/issue-903";

/// origin + an "author" clone that builds the stack + the clone the pass
/// runs in (which never checked out any of the stack's branches).
struct Fixture {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    author: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let author = tmp.path().join("author");
        let repo = tmp.path().join("repo");
        run(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(&author).unwrap();
        run(&author, &["init", "-q", "--initial-branch=main"]);
        configure(&author);
        commit_file(&author, "base.txt", "base\n", "base");
        run(&author, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run(&author, &["push", "-q", "origin", "main"]);
        Self {
            _tmp: tmp,
            origin,
            author,
            repo,
        }
    }

    /// Commit `file` on a new branch `name` off `from` in the author clone,
    /// push it, and record it as PR `pr`'s head (`refs/pull/<pr>/head`).
    fn branch(&self, name: &str, from: &str, pr: u32, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "-B", name, from]);
        let sha = commit_file(&self.author, file, body, &format!("{name}: {file}"));
        run(&self.author, &["push", "-q", "-f", "origin", &format!("{name}:{name}")]);
        run(
            &self.author,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("{name}:refs/pull/{pr}/head"),
            ],
        );
        sha
    }

    /// Move origin's main forward with `file`.
    fn advance_main(&self, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "main"]);
        let sha = commit_file(&self.author, file, body, &format!("main: {file}"));
        run(&self.author, &["push", "-q", "origin", "main"]);
        sha
    }

    /// The clone the pass runs in, made after the stack exists.
    fn clone_repo(&self) {
        run(
            self.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                self.origin.to_str().unwrap(),
                self.repo.to_str().unwrap(),
            ],
        );
        configure(&self.repo);
    }

    fn origin_rev(&self, rev: &str) -> String {
        run(&self.origin, &["rev-parse", rev])
    }

    fn origin_files(&self, rev: &str) -> Vec<String> {
        run(&self.origin, &["ls-tree", "--name-only", rev])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn origin_count(&self, range: &str) -> usize {
        run(&self.origin, &["rev-list", "--count", range])
            .parse()
            .unwrap()
    }
}

/// The repository the fake forge says it is; [`open_pr`] heads live in it.
const REPO: &str = "acme/widgets";

fn open_pr(number: u32, head: &str, sha: &str, base: &str) -> RestPull {
    RestPull {
        number,
        state: "open".to_string(),
        head_ref: Some(head.to_string()),
        head_sha: Some(sha.to_string()),
        base_ref: Some(base.to_string()),
        head_repo: Some(REPO.to_string()),
        ..RestPull::default()
    }
}

#[derive(Default)]
struct FakeForge {
    parents: BTreeMap<String, ParentPr>,
    strategy_merge: bool,
    repo_unknown: bool,
    lease: Option<Option<bool>>,
    lookups: RefCell<Vec<String>>,
    comments: RefCell<BTreeMap<u32, Vec<String>>>,
    added: RefCell<Vec<(u32, String)>>,
    removed: RefCell<Vec<(u32, String)>>,
    retargets: RefCell<Vec<(u32, String)>>,
}

impl OrphanForge for FakeForge {
    fn newest_closed_pr_for_head(&self, branch: &str) -> Result<Option<ParentPr>, String> {
        self.lookups.borrow_mut().push(branch.to_string());
        Ok(self.parents.get(branch).cloned())
    }
    fn trusted_comments(&self, pr: u32) -> Option<Vec<String>> {
        Some(self.comments.borrow().get(&pr).cloned().unwrap_or_default())
    }
    fn issue_lease_fresh(&self, _issue: u32) -> Option<bool> {
        self.lease.unwrap_or(Some(false))
    }
    fn strategy(&self) -> Strategy {
        if self.strategy_merge {
            Strategy::Merge
        } else {
            Strategy::Rebase
        }
    }
    fn repository(&self) -> Option<String> {
        (!self.repo_unknown).then(|| REPO.to_string())
    }
    fn retarget(&self, pr: u32, base: &str) -> Result<(), String> {
        self.retargets.borrow_mut().push((pr, base.to_string()));
        Ok(())
    }
    fn add_label(&self, pr: u32, label: &str) -> Result<(), String> {
        self.added.borrow_mut().push((pr, label.to_string()));
        Ok(())
    }
    fn remove_label(&self, pr: u32, label: &str) -> Result<(), String> {
        self.removed.borrow_mut().push((pr, label.to_string()));
        Ok(())
    }
    fn comment(&self, pr: u32, body: &str) -> Result<(), String> {
        self.comments
            .borrow_mut()
            .entry(pr)
            .or_default()
            .push(body.to_string());
        Ok(())
    }
}

fn closed_parent(number: u32, sha: &str) -> ParentPr {
    ParentPr {
        number,
        merged: false,
        head_sha: Some(sha.to_string()),
    }
}

fn worktree_count(repo: &Path) -> usize {
    run(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

#[test]
fn closed_parent_child_is_replayed_onto_main_with_only_its_own_commits() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    // main moves after the stack was built: the destination must be the
    // fetched tip, not whatever the clone had.
    fx.clone_repo();
    let main_tip = fx.advance_main("m.txt", "main moved\n");
    // The parent's branch is gone (deleted after it closed): only its PR
    // record (refs/pull/1/head) and the child's ancestry still name it.
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), closed_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);

    assert_eq!(stats.orphans, 1);
    assert_eq!(stats.retargeted, 1, "{stats:?}");
    assert_eq!(*forge.retargets.borrow(), vec![(2, "main".to_string())]);
    let new_b = fx.origin_rev(B);
    assert_ne!(new_b, b);
    assert_eq!(fx.origin_rev(&format!("{B}^")), main_tip, "B sits on the fetched main tip");
    assert_eq!(fx.origin_count(&format!("main..{B}")), 1, "only B's own commit");
    let files = fx.origin_files(B);
    assert!(files.contains(&"b.txt".to_string()) && files.contains(&"m.txt".to_string()));
    assert!(!files.contains(&"a.txt".to_string()), "the closed parent's change is gone");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RETARGETED_MARKER_PREFIX));
    assert!(forge.added.borrow().is_empty());
    assert_eq!(worktree_count(&fx.repo), 1, "the scratch worktree is removed");
    assert_eq!(
        run(&fx.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "the clone's own checkout is never switched"
    );

    // Idempotent: the retargeted child's base is main, so the next pass does
    // not even look its parent up.
    let after = vec![open_pr(2, B, &new_b, "main")];
    let lookups_before = forge.lookups.borrow().len();
    let again = reconcile_with(&forge, &fx.repo, "main", &after);
    assert_eq!(again, OrphanStats::default());
    assert_eq!(forge.lookups.borrow().len(), lookups_before);
    assert_eq!(fx.origin_rev(B), new_b);
}

#[test]
fn a_childs_own_children_are_replayed_onto_its_new_head() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    let c = fx.branch(C, B, 3, "c.txt", "grandchild\n");
    fx.clone_repo();
    let forge = FakeForge {
        parents: [(A.to_string(), closed_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A), open_pr(3, C, &c, B)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);

    assert_eq!((stats.retargeted, stats.cascaded, stats.flagged), (1, 1, 0), "{stats:?}");
    // Only the orphan is retargeted; the grandchild stays stacked on B.
    assert_eq!(*forge.retargets.borrow(), vec![(2, "main".to_string())]);
    assert_eq!(fx.origin_rev(&format!("{C}^")), fx.origin_rev(B));
    assert_eq!(fx.origin_count(&format!("{B}..{C}")), 1);
    assert!(!fx.origin_files(C).contains(&"a.txt".to_string()));
    assert!(fx.origin_files(C).contains(&"b.txt".to_string()));
}

#[test]
fn a_conflicting_replay_is_flagged_once_and_nothing_is_pushed() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "base.txt", "child edit\n");
    fx.clone_repo();
    fx.advance_main("base.txt", "main edit\n");
    let forge = FakeForge {
        parents: [(A.to_string(), closed_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);

    assert_eq!((stats.flagged, stats.retargeted), (1, 0), "{stats:?}");
    assert_eq!(*forge.added.borrow(), vec![(2, MERGE_CONFLICT.to_string())]);
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(&flagged_marker(1, &b)));
    assert!(posted[0].contains("`base.txt`"));
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    assert!(forge.retargets.borrow().is_empty());
    assert_eq!(worktree_count(&fx.repo), 1, "the scratch worktree is removed");

    // Second pass at the same head: the marker makes it a no-op.
    let again = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!(again.flagged, 0);
    assert_eq!(forge.added.borrow().len(), 1);
    assert_eq!(forge.comments.borrow().get(&2).map(Vec::len), Some(1));
}

#[test]
fn merge_mode_flags_without_rewriting_or_retargeting() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let forge = FakeForge {
        parents: [(A.to_string(), closed_parent(1, &a))]
            .into_iter()
            .collect(),
        strategy_merge: true,
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
    assert_eq!(stats.flagged, 1);
    assert_eq!(fx.origin_rev(B), b);
    assert!(forge.retargets.borrow().is_empty());
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert!(posted[0].contains("#10771"));
    assert!(posted[0].contains("#12767"));
    assert!(!posted[0].contains("--force"), "{}", posted[0]);
}

#[test]
fn a_fresh_or_unreadable_issue_lease_leaves_the_child_alone() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    for lease in [Some(true), None] {
        let forge = FakeForge {
            parents: [(A.to_string(), closed_parent(1, &a))]
                .into_iter()
                .collect(),
            lease: Some(lease),
            ..FakeForge::default()
        };
        let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
        assert_eq!((stats.skipped, stats.retargeted, stats.flagged), (1, 0, 0), "{lease:?}");
        assert!(forge.comments.borrow().is_empty() && forge.added.borrow().is_empty());
        assert_eq!(fx.origin_rev(B), b);
    }
}

#[test]
fn a_merged_or_open_parent_is_not_this_passs_business() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let merged = FakeForge {
        parents: [(
            A.to_string(),
            ParentPr {
                merged: true,
                ..closed_parent(1, &a)
            },
        )]
        .into_iter()
        .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&merged, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
    assert_eq!(stats, OrphanStats::default());

    // Parent open: no lookup at all.
    let open_parent = FakeForge::default();
    let open = vec![open_pr(1, A, &a, "main"), open_pr(2, B, &b, A)];
    assert_eq!(reconcile_with(&open_parent, &fx.repo, "main", &open), OrphanStats::default());
    assert!(open_parent.lookups.borrow().is_empty());
    assert_eq!(fx.origin_rev(B), b);
}

#[test]
fn a_remote_head_that_moved_since_the_listing_is_retried_not_clobbered() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    // A sibling pushes to B after the listing was taken.
    run(&fx.author, &["checkout", "-q", B]);
    let pushed = commit_file(&fx.author, "b2.txt", "more\n", "B: more");
    run(&fx.author, &["push", "-q", "origin", B]);
    let forge = FakeForge {
        parents: [(A.to_string(), closed_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
    assert_eq!((stats.skipped, stats.retargeted, stats.flagged), (1, 0, 0));
    assert_eq!(fx.origin_rev(B), pushed);
    assert!(forge.retargets.borrow().is_empty());
}

// --- #12764 review: ancestry, ownership, transient fetches ------------------

fn parents_a(a: &str) -> BTreeMap<String, ParentPr> {
    [(A.to_string(), closed_parent(1, a))].into_iter().collect()
}

#[test]
fn a_parent_rewritten_after_the_child_branched_is_flagged_not_replayed() {
    let fx = Fixture::new();
    let _a1 = fx.branch(A, "main", 1, "a.txt", "parent v1\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    // A is amended (rebuilt from main) after B was cut from it, then closed.
    // Its PR record names A2, which B does not contain.
    let a2 = fx.branch(A, "main", 1, "a.txt", "parent v2\n");
    fx.clone_repo();
    let forge = FakeForge {
        parents: parents_a(&a2),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);

    assert_eq!((stats.flagged, stats.retargeted), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b, "nothing pushed: the replay would carry A1 to main");
    assert!(forge.retargets.borrow().is_empty());
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(&flagged_marker(1, &b)));
    assert!(posted[0].contains("is not an ancestor"), "{}", posted[0]);
    assert_eq!(worktree_count(&fx.repo), 1, "refused before any scratch worktree");

    let again = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!(again.flagged, 0, "deduplicated by head");
    assert_eq!(forge.added.borrow().len(), 1);
}

#[test]
fn a_grandchild_cut_from_an_older_child_head_is_flagged_not_replayed() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let _b1 = fx.branch(B, A, 2, "b.txt", "child v1\n");
    let c = fx.branch(C, B, 3, "c.txt", "grandchild\n");
    // B is rebuilt on A after C was cut from B1: B2 is not an ancestor of C.
    let b2 = fx.branch(B, A, 2, "b.txt", "child v2\n");
    fx.clone_repo();
    let forge = FakeForge {
        parents: parents_a(&a),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b2, A), open_pr(3, C, &c, B)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);

    assert_eq!((stats.retargeted, stats.cascaded, stats.flagged), (1, 0, 1), "{stats:?}");
    assert_eq!(fx.origin_rev(C), c, "the grandchild is not rewritten");
    let posted = forge.comments.borrow().get(&3).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(&flagged_marker(2, &c)));
    assert!(posted[0].contains("is not an ancestor"), "{}", posted[0]);
}

#[test]
fn only_loom_branches_in_this_repo_are_rewritten() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    let human = fx.branch("jdoe/fix", A, 5, "h.txt", "human\n");
    fx.clone_repo();

    // A human's branch: flagged once per head, never pushed.
    let forge = FakeForge {
        parents: parents_a(&a),
        ..FakeForge::default()
    };
    let open = vec![open_pr(5, "jdoe/fix", &human, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!((stats.flagged, stats.retargeted), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev("jdoe/fix"), human);
    assert!(forge.retargets.borrow().is_empty());
    let posted = forge.comments.borrow().get(&5).cloned().unwrap_or_default();
    assert!(posted[0].contains("not a Loom branch"), "{}", posted[0]);
    assert!(!posted[0].contains("--force"), "{}", posted[0]);
    let again = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!((again.flagged, again.skipped), (0, 0), "deduplicated by head, no retry loop");
    assert_eq!(forge.added.borrow().len(), 1);

    // A fork whose branch happens to be named like a Loom branch.
    let fork = FakeForge {
        parents: parents_a(&a),
        ..FakeForge::default()
    };
    let mut forked = open_pr(2, B, &b, A);
    forked.head_repo = Some("someone/fork".to_string());
    let stats = reconcile_with(&fork, &fx.repo, "main", &[forked]);
    assert_eq!((stats.flagged, stats.retargeted), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b);

    // This repository's identity unreadable: a skip, not a flag or a push.
    let unknown = FakeForge {
        parents: parents_a(&a),
        repo_unknown: true,
        ..FakeForge::default()
    };
    let stats = reconcile_with(&unknown, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
    assert_eq!((stats.skipped, stats.flagged, stats.retargeted), (1, 0, 0), "{stats:?}");
    assert!(unknown.added.borrow().is_empty() && unknown.comments.borrow().is_empty());
    assert_eq!(fx.origin_rev(B), b);

    // The default branch as head (a sync PR into the stack branch): no read.
    let sync = FakeForge {
        parents: parents_a(&a),
        ..FakeForge::default()
    };
    let main = fx.origin_rev("main");
    let stats = reconcile_with(&sync, &fx.repo, "main", &[open_pr(6, "main", &main, A)]);
    assert_eq!(stats, OrphanStats::default());
    assert!(sync.lookups.borrow().is_empty());
    assert_eq!(fx.origin_rev("main"), main);
}

#[test]
fn a_cascade_never_rewrites_a_grandchild_that_is_not_a_loom_branch() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    let top = fx.branch("jdoe/top", B, 3, "t.txt", "human on top\n");
    fx.clone_repo();
    let forge = FakeForge {
        parents: parents_a(&a),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A), open_pr(3, "jdoe/top", &top, B)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!((stats.retargeted, stats.cascaded, stats.flagged), (1, 0, 1), "{stats:?}");
    assert_eq!(fx.origin_rev("jdoe/top"), top, "the human branch is never pushed");
    let posted = forge.comments.borrow().get(&3).cloned().unwrap_or_default();
    assert!(posted[0].contains("not a Loom branch"), "{}", posted[0]);
}

#[test]
fn a_failed_parent_fetch_is_retried_next_tick_not_flagged() {
    let fx = Fixture::new();
    // The pass's clone exists before the stack, so A's commit is not in it.
    fx.clone_repo();
    run(&fx.author, &["checkout", "-q", "-B", A, "main"]);
    let a = commit_file(&fx.author, "a.txt", "parent\n", "A: a.txt");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    // refs/pull/1/head is not served yet: the parent fetch fails.
    let forge = FakeForge {
        parents: parents_a(&a),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!((stats.skipped, stats.flagged, stats.retargeted), (1, 0, 0), "{stats:?}");
    assert!(forge.added.borrow().is_empty() && forge.comments.borrow().is_empty());
    assert_eq!(fx.origin_rev(B), b);

    // The fetch works on the next tick: the child is restacked, not stuck.
    run(&fx.author, &["push", "-q", "origin", &format!("{a}:refs/pull/1/head")]);
    let stats = reconcile_with(&forge, &fx.repo, "main", &open);
    assert_eq!(stats.retargeted, 1, "{stats:?}");
    assert!(!fx.origin_files(B).contains(&"a.txt".to_string()));
}

#[test]
fn a_parent_head_absent_from_its_record_or_its_fetched_ref_is_flagged() {
    let fx = Fixture::new();
    let _a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    // The record names a head that refs/pull/1/head (which fetches fine)
    // does not contain.
    let missing = FakeForge {
        parents: parents_a(&"1".repeat(40)),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&missing, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
    assert_eq!(stats.flagged, 1, "{stats:?}");
    let posted = missing
        .comments
        .borrow()
        .get(&2)
        .cloned()
        .unwrap_or_default();
    assert!(posted[0].contains("is not in it"), "{}", posted[0]);

    // The record names no head at all.
    let no_head = FakeForge {
        parents: [(
            A.to_string(),
            ParentPr {
                number: 1,
                merged: false,
                head_sha: None,
            },
        )]
        .into_iter()
        .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&no_head, &fx.repo, "main", &[open_pr(2, B, &b, A)]);
    assert_eq!(stats.flagged, 1, "{stats:?}");
    let posted = no_head
        .comments
        .borrow()
        .get(&2)
        .cloned()
        .unwrap_or_default();
    assert!(posted[0].contains("names no head commit"), "{}", posted[0]);
    assert_eq!(fx.origin_rev(B), b);
}
