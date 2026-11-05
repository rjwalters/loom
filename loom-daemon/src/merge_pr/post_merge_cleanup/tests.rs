use super::*;
use std::collections::HashSet;

const REPO: &str = "/repo";
const WT: &str = "/repo/.loom/worktrees/issue-42";

#[derive(Default)]
struct Fake {
    dirs: HashSet<String>,
    sentinels: HashSet<String>,
    porcelain: String,
    status: String,
    /// `None` = the ignored listing read fails.
    ignored: Option<String>,
    links: Vec<String>,
    cwd: Option<String>,
    teardown_fails: bool,
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn healthy() -> Self {
        let mut f = Fake {
            ignored: Some(String::new()),
            ..Fake::default()
        };
        f.dirs.insert(WT.to_string());
        f.sentinels.insert(WT.to_string());
        f.porcelain = format!(
            "worktree {REPO}\nHEAD aaa\nbranch refs/heads/main\n\nworktree {WT}\nHEAD bbb\nbranch refs/heads/feature/issue-42\n"
        );
        f
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
    fn called(&self, prefix: &str) -> bool {
        self.calls.borrow().iter().any(|c| c.starts_with(prefix))
    }
}

impl Host for Fake {
    fn is_dir(&self, p: &str) -> bool {
        self.dirs.contains(p)
    }
    fn worktree_list(&self, _: &str) -> String {
        self.porcelain.clone()
    }
    fn git_status(&self, _: &str) -> String {
        self.status.clone()
    }
    fn git_ignored(&self, _: &str) -> Option<String> {
        self.calls.borrow_mut().push("git-ignored".into());
        self.ignored.clone()
    }
    fn link_paths(&self, _: &str) -> Vec<String> {
        self.links.clone()
    }
    fn size_kib(&self, _: &str, entries: &[String]) -> u64 {
        entries.len() as u64 * 10
    }
    fn sentinel_present(&self, p: &str) -> bool {
        self.sentinels.contains(p)
    }
    fn canonical(&self, p: &str) -> Option<String> {
        Some(p.to_string())
    }
    fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }
    fn chdir(&self, p: &str) {
        self.calls.borrow_mut().push(format!("chdir {p}"));
    }
    fn cleanup_targets(&self, _: &str, branch: &str, pr: &str) -> Option<(String, String, String)> {
        Some(match cleanup_paths::issue_number_of(branch) {
            Some(n) => (
                format!("{REPO}/.loom/worktrees/issue-{n}"),
                n.to_string(),
                format!("{REPO}/.loom/worktrees/pr-{pr}"),
            ),
            None => (format!("{REPO}/.loom/worktrees/pr-{pr}"), String::new(), String::new()),
        })
    }
    fn resolve_cargo_target(&self, w: &str) -> String {
        format!("{w}/target")
    }
    fn teardown(&self, _: &str, path: &str) -> String {
        self.calls.borrow_mut().push(format!("teardown {path}"));
        if self.teardown_fails {
            "LOOM-WORKTREE-TEARDOWN FAILED\nWARNING\tstuck\n".into()
        } else {
            "LOOM-WORKTREE-TEARDOWN REMOVED\nSUCCESS\tRemoved worktree\n".into()
        }
    }
    fn reclaim_cargo_target(&self, _: &str, w: &str, r: &str) -> Option<Record> {
        self.calls.borrow_mut().push(format!("reclaim {w} {r}"));
        None
    }
    fn record_removal(&self, _: &str, w: &str, b: Option<&str>) {
        self.calls
            .borrow_mut()
            .push(format!("ledger {w} {}", b.unwrap_or("-")));
    }
    fn delete_branch(&self, _: &Inputs, branch: &str, sha: &str, out: &mut Vec<Record>) {
        self.calls
            .borrow_mut()
            .push(format!("delete-branch {branch} [{sha}]"));
        out.push(("INFO".into(), format!("branch-step {branch}")));
    }
}

fn inputs() -> Inputs {
    Inputs {
        repo_root: REPO.into(),
        pr_number: "99".into(),
        pr_branch: "feature/issue-42".into(),
        pr_head_sha: "deadbeef".into(),
        default_branch: Some("main".into()),
        cleanup_worktree: true,
        cleanup_primary: true,
        preserve_env: false,
        worktree_path_override: String::new(),
        remove_ignored: false,
    }
}

fn done(o: Outcome) -> (Vec<Record>, Option<String>) {
    match o {
        Outcome::Done { records, cd } => (records, cd),
        Outcome::Need(n) => panic!("unexpected {n:?}"),
    }
}

fn texts(r: &[Record]) -> Vec<String> {
    r.iter().map(|(l, t)| format!("{l}:{t}")).collect()
}

fn closes_42() -> Facts {
    Facts {
        close_targets: Some("42\n".into()),
        ..Facts::default()
    }
}

#[test]
fn no_cleanup_worktree_does_nothing_but_say_so() {
    let host = Fake::healthy();
    let mut i = inputs();
    i.cleanup_worktree = false;
    let (r, _) = done(run(&i, &Facts::default(), &host));
    assert_eq!(
        texts(&r),
        ["INFO:Worktree cleanup skipped (--no-cleanup-worktree) — local branch left in place"]
    );
    assert!(host.calls().is_empty());
}

#[test]
fn preserve_env_skips_everything_including_the_branch_delete() {
    let host = Fake::healthy();
    let mut i = inputs();
    i.preserve_env = true;
    let (r, _) = done(run(&i, &Facts::default(), &host));
    assert_eq!(
        texts(&r),
        ["INFO:Worktree cleanup skipped (LOOM_PRESERVE_WORKTREE=1) — local branch left in place"]
    );
    assert!(host.calls().is_empty());
}

#[test]
fn a_close_target_removes_the_worktree_then_deletes_the_branch() {
    let host = Fake::healthy();
    let (r, cd) = done(run(&inputs(), &closes_42(), &host));
    assert_eq!(cd, None);
    assert_eq!(
        host.calls(),
        [
            format!("teardown {WT}"),
            format!("ledger {WT} feature/issue-42"),
            format!("reclaim {WT} {WT}/target"),
            "delete-branch feature/issue-42 [deadbeef]".to_string(),
        ]
    );
    let t = texts(&r);
    assert!(t.contains(&format!("INFO:Removing worktree: {WT}")));
    assert!(t.contains(&"SUCCESS:Removed worktree".to_string()));
    assert_eq!(t.last().unwrap(), "INFO:branch-step feature/issue-42");
}

#[test]
fn facts_are_asked_for_lazily_and_in_order_with_nothing_done() {
    let host = Fake::healthy();
    let mut facts = Facts::default();
    assert_eq!(run(&inputs(), &facts, &host), Outcome::Need(Need::CloseTargets));
    facts.close_targets = Some(String::new());
    assert_eq!(run(&inputs(), &facts, &host), Outcome::Need(Need::IssueState("42".into())));
    facts.issue_state = Some("OPEN".into());
    assert_eq!(run(&inputs(), &facts, &host), Outcome::Need(Need::Landed));
    assert!(host.calls().is_empty(), "asking must not act: {:?}", host.calls());
}

#[test]
fn an_open_unlanded_issue_preserves_the_worktree_but_still_deletes_the_branch() {
    let host = Fake::healthy();
    let facts = Facts {
        close_targets: Some(String::new()),
        issue_state: Some("OPEN".into()),
        landed: Some(Landed {
            landed: false,
            verdict: "not-landed".into(),
            evidence: "tree-differs".into(),
        }),
    };
    let (r, _) = done(run(&inputs(), &facts, &host));
    assert!(!host.called("teardown"));
    assert!(host.called("delete-branch feature/issue-42"));
    assert!(texts(&r)
        .iter()
        .any(|t| t.starts_with("INFO:") || t.starts_with("WARNING:")));
}

#[test]
fn a_failed_issue_lookup_preserves() {
    let host = Fake::healthy();
    let facts = Facts {
        close_targets: Some(String::new()),
        issue_state: Some(String::new()),
        landed: Some(Landed::default()),
    };
    done(run(&inputs(), &facts, &host));
    assert!(!host.called("teardown"));
}

#[test]
fn an_open_issue_whose_branch_landed_is_removed() {
    let host = Fake::healthy();
    let facts = Facts {
        close_targets: Some(String::new()),
        issue_state: Some("OPEN".into()),
        landed: Some(Landed {
            landed: true,
            verdict: "landed".into(),
            evidence: "tree-equal".into(),
        }),
    };
    done(run(&inputs(), &facts, &host));
    assert!(host.called("teardown"));
}

#[test]
fn uncommitted_work_refuses_the_removal_and_names_the_remediation() {
    let mut host = Fake::healthy();
    host.status = " M src/lib.rs".into();
    let (r, _) = done(run(&inputs(), &closes_42(), &host));
    assert!(!host.called("teardown"));
    assert!(!host.called("ledger"));
    let t = texts(&r);
    assert!(t
        .iter()
        .any(|l| l.starts_with("WARNING:Refusing to remove worktree at")));
    assert!(t.iter().any(|l| l.starts_with("PLAIN:")));
    // The branch delete is independent of the worktree and still runs.
    assert!(host.called("delete-branch"));
}

#[test]
fn loom_runtime_markers_alone_do_not_block_removal() {
    let mut host = Fake::healthy();
    host.status = "?? .loom-managed\n?? .loom-in-use".into();
    done(run(&inputs(), &closes_42(), &host));
    assert!(host.called("teardown"));
}

#[test]
fn a_missing_sentinel_is_user_owned_and_left_alone() {
    let mut host = Fake::healthy();
    host.sentinels.clear();
    let (r, _) = done(run(&inputs(), &closes_42(), &host));
    assert!(!host.called("teardown"));
    assert!(texts(&r)
        .iter()
        .any(|t| t.contains("lacks .loom-managed sentinel")));
}

#[test]
fn the_primary_checkout_is_never_removed_even_with_a_sentinel() {
    let mut host = Fake::healthy();
    let mut i = inputs();
    host.dirs.insert(REPO.into());
    host.sentinels.insert(REPO.into());
    i.worktree_path_override = REPO.into();
    let (r, _) = done(run(&i, &Facts::default(), &host));
    assert!(!host.called("teardown"));
    assert!(texts(&r)
        .iter()
        .any(|t| t.starts_with("WARNING:Refusing to remove the primary/main worktree")));
}

#[test]
fn a_failed_teardown_skips_the_success_only_steps() {
    let mut host = Fake::healthy();
    host.teardown_fails = true;
    let (r, _) = done(run(&inputs(), &closes_42(), &host));
    assert!(host.called("teardown"));
    assert!(!host.called("ledger"));
    assert!(!host.called("reclaim"));
    assert!(texts(&r).contains(&"WARNING:stuck".to_string()));
    assert!(host.called("delete-branch"));
}

#[test]
fn removing_the_worktree_we_stand_in_asks_the_shell_to_leave_it() {
    let mut host = Fake::healthy();
    host.cwd = Some(format!("{WT}/src"));
    let (r, cd) = done(run(&inputs(), &closes_42(), &host));
    assert_eq!(cd.as_deref(), Some(REPO));
    assert!(host.called(&format!("chdir {REPO}")));
    assert!(texts(&r).contains(&format!("PLAIN:  cd {REPO}")));
}

#[test]
fn the_override_path_bypasses_the_sentinel_and_deletes_its_attached_branch_with_no_sha() {
    let mut host = Fake::healthy();
    host.sentinels.clear();
    let mut i = inputs();
    i.worktree_path_override = WT.into();
    let (r, _) = done(run(&i, &Facts::default(), &host));
    assert!(texts(&r)
        .iter()
        .any(|t| t.starts_with("INFO:Cleanup target overridden by --worktree-path")));
    assert!(host.called(&format!("teardown {WT}")));
    assert!(host.called(&format!("ledger {WT} feature/issue-42")));
    assert!(host.called("delete-branch feature/issue-42 []"));
    // The override arm does not also run the convention path.
    assert!(!host.called("delete-branch feature/issue-42 [deadbeef]"));
}

#[test]
fn a_pr_only_branch_needs_no_forge_fact() {
    let mut host = Fake::healthy();
    let pr_wt = format!("{REPO}/.loom/worktrees/pr-99");
    host.dirs.insert(pr_wt.clone());
    host.sentinels.insert(pr_wt.clone());
    let mut i = inputs();
    i.pr_branch = "docs/thing".into();
    let (_, _) = done(run(&i, &Facts::default(), &host));
    assert!(host.called(&format!("teardown {pr_wt}")));
}

#[test]
fn a_missing_default_path_with_nothing_tracking_the_branch_says_so() {
    let mut host = Fake::healthy();
    host.dirs.clear();
    host.porcelain = format!("worktree {REPO}\nHEAD aaa\nbranch refs/heads/main\n");
    let (r, _) = done(run(&inputs(), &Facts::default(), &host));
    assert!(texts(&r).contains(&format!(
        "INFO:No worktree found at {WT} (and none tracking 'feature/issue-42' in 'git worktree list')"
    )));
    assert!(host.called("delete-branch"));
}

#[test]
fn a_discovered_user_owned_worktree_is_advice_only() {
    let mut host = Fake::healthy();
    let odd = "/elsewhere/mine";
    host.dirs.remove(WT);
    host.porcelain = format!(
        "worktree {REPO}\nHEAD aaa\nbranch refs/heads/main\n\nworktree {odd}\nHEAD bbb\nbranch refs/heads/feature/issue-42\n"
    );
    host.dirs.insert(odd.into());
    let (r, _) = done(run(&inputs(), &closes_42(), &host));
    assert!(!host.called("teardown"));
    assert!(!texts(&r).is_empty());
}

#[test]
fn a_fact_demanded_after_a_mutation_fails_safe_instead_of_asking() {
    // Default removal runs on a close target; the judge worktree then needs
    // the issue state only if the close-target answer were missing. Model the
    // late demand directly.
    let host = Fake::healthy();
    let run = Run {
        inputs: &inputs(),
        facts: &Facts::default(),
        host: &host,
        out: RefCell::new(Vec::new()),
        cd: RefCell::new(None),
        mutated: RefCell::new(true),
    };
    assert_eq!(run.issue_is_closed("42"), Ok(false));
    let early = Run {
        mutated: RefCell::new(false),
        ..run
    };
    assert_eq!(early.issue_is_closed("42"), Err(Need::CloseTargets));
}

#[test]
fn an_unframeable_worktree_root_removes_nothing_but_still_runs_the_branch_step() {
    struct NoTargets(Fake);
    impl Host for NoTargets {
        fn is_dir(&self, p: &str) -> bool {
            self.0.is_dir(p)
        }
        fn worktree_list(&self, r: &str) -> String {
            self.0.worktree_list(r)
        }
        fn git_status(&self, p: &str) -> String {
            self.0.git_status(p)
        }
        fn git_ignored(&self, p: &str) -> Option<String> {
            self.0.git_ignored(p)
        }
        fn link_paths(&self, r: &str) -> Vec<String> {
            self.0.link_paths(r)
        }
        fn size_kib(&self, p: &str, e: &[String]) -> u64 {
            self.0.size_kib(p, e)
        }
        fn sentinel_present(&self, p: &str) -> bool {
            self.0.sentinel_present(p)
        }
        fn canonical(&self, p: &str) -> Option<String> {
            self.0.canonical(p)
        }
        fn cwd(&self) -> Option<String> {
            self.0.cwd()
        }
        fn chdir(&self, p: &str) {
            self.0.chdir(p)
        }
        fn cleanup_targets(&self, _: &str, _: &str, _: &str) -> Option<(String, String, String)> {
            None
        }
        fn resolve_cargo_target(&self, w: &str) -> String {
            self.0.resolve_cargo_target(w)
        }
        fn teardown(&self, r: &str, p: &str) -> String {
            self.0.teardown(r, p)
        }
        fn reclaim_cargo_target(&self, r: &str, w: &str, x: &str) -> Option<Record> {
            self.0.reclaim_cargo_target(r, w, x)
        }
        fn record_removal(&self, r: &str, w: &str, b: Option<&str>) {
            self.0.record_removal(r, w, b)
        }
        fn delete_branch(&self, i: &Inputs, b: &str, s: &str, o: &mut Vec<Record>) {
            self.0.delete_branch(i, b, s, o)
        }
    }
    let host = NoTargets(Fake::healthy());
    let (r, _) = done(run(&inputs(), &closes_42(), &host));
    assert!(!host.0.called("teardown"));
    assert!(host.0.called("delete-branch"));
    assert!(texts(&r)[0].starts_with("WARNING:Post-merge worktree cleanup for PR #99 did not run"));
}

// --- #13599: ignored-data guard on the --worktree-path override ------------

fn override_inputs() -> Inputs {
    let mut i = inputs();
    i.worktree_path_override = WT.into();
    i
}

#[test]
fn override_keeps_worktree_with_ignored_user_data() {
    let mut host = Fake::healthy();
    host.sentinels.clear();
    host.ignored = Some("!! wip/results/out.json\n!! node_modules/\n".into());
    let (r, _) = done(run(&override_inputs(), &Facts::default(), &host));
    let t = texts(&r);
    assert!(!host.called("teardown"), "{t:?}");
    assert!(t.iter().any(|l| l.contains("ignored-data guard, #13599")), "{t:?}");
    assert!(t.iter().any(|l| l == "WARNING:  wip/results/out.json"), "{t:?}");
    assert!(!t.iter().any(|l| l.contains("node_modules")), "{t:?}");
    assert!(
        t.iter()
            .any(|l| l.contains("Total size of the above: 10 KiB")),
        "{t:?}"
    );
    assert!(t.iter().any(|l| l.contains("--remove-ignored")), "{t:?}");
}

#[test]
fn override_with_force_flag_removes_despite_ignored_data() {
    let mut host = Fake::healthy();
    host.sentinels.clear();
    host.ignored = Some("!! wip/results/out.json\n".into());
    let mut i = override_inputs();
    i.remove_ignored = true;
    done(run(&i, &Facts::default(), &host));
    assert!(host.called("teardown"));
    assert!(!host.called("git-ignored"));
}

#[test]
fn override_removes_when_only_artifacts_are_ignored() {
    let mut host = Fake::healthy();
    host.sentinels.clear();
    host.ignored =
        Some("!! node_modules/\n!! target/\n!! .venv/\n!! .loom-managed\n!! linked/dir/\n".into());
    host.links = vec!["linked/dir".into()];
    done(run(&override_inputs(), &Facts::default(), &host));
    assert!(host.called("teardown"));
}

#[test]
fn override_fails_closed_when_ignored_listing_unreadable() {
    let mut host = Fake::healthy();
    host.sentinels.clear();
    host.ignored = None;
    let (r, _) = done(run(&override_inputs(), &Facts::default(), &host));
    assert!(!host.called("teardown"));
    assert!(texts(&r).iter().any(|l| l.contains("could not be read")));
}

#[test]
fn managed_default_path_never_reads_ignored_files() {
    let mut host = Fake::healthy();
    host.ignored = None; // would refuse if consulted
    done(run(&inputs(), &closes_42(), &host));
    assert!(host.called("teardown"));
    assert!(!host.called("git-ignored"));
}
