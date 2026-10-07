use std::cell::RefCell;

use super::*;

fn attempt(ok: bool, output: &str) -> Attempt {
    Attempt {
        ok,
        output: output.to_string(),
    }
}

/// Drive [`run_with`] with scripted answers, recording the call order.
fn scripted(removes: &[Attempt], prune_ok: bool) -> (Outcome, Vec<&'static str>) {
    let calls = RefCell::new(Vec::new());
    let mut queue = removes.iter().cloned();
    let outcome = run_with(
        || {
            calls.borrow_mut().push("remove");
            queue
                .next()
                .expect("no more scripted removes than the ladder makes")
        },
        || {
            calls.borrow_mut().push("prune");
            prune_ok
        },
    );
    (outcome, calls.into_inner())
}

#[test]
fn first_try_success_never_prunes() {
    let (outcome, calls) = scripted(&[attempt(true, "")], true);
    assert_eq!(outcome, Outcome::Removed);
    assert_eq!(calls, ["remove"]);
}

#[test]
fn a_failed_first_try_prunes_once_then_retries_once() {
    let (outcome, calls) = scripted(&[attempt(false, "first"), attempt(true, "")], true);
    assert_eq!(outcome, Outcome::RemovedAfterPrune);
    assert_eq!(calls, ["remove", "prune", "remove"]);
}

#[test]
fn a_failed_retry_reports_the_retry_error_not_the_first() {
    let (outcome, calls) = scripted(&[attempt(false, "first"), attempt(false, "second")], true);
    assert_eq!(
        outcome,
        Outcome::Failed {
            error: "second".into()
        }
    );
    assert_eq!(calls, ["remove", "prune", "remove"]);
}

#[test]
fn a_failed_prune_skips_the_retry_and_keeps_the_first_error() {
    let (outcome, calls) = scripted(&[attempt(false, "first")], false);
    assert_eq!(
        outcome,
        Outcome::Failed {
            error: "first".into()
        }
    );
    assert_eq!(calls, ["remove", "prune"]);
}

#[test]
fn success_texts_distinguish_a_pruned_retry() {
    let (v, lines) = report(&Outcome::Removed, "/r", "/r/wt");
    assert_eq!(v, REMOVED);
    assert_eq!(lines, [(Level::Success, "Worktree removed".to_string())]);
    let (v, lines) = report(&Outcome::RemovedAfterPrune, "/r", "/r/wt");
    assert_eq!(v, REMOVED);
    assert_eq!(
        lines,
        [(
            Level::Success,
            "Worktree removed (after pruning a stale worktree registration)".to_string()
        )]
    );
}

#[test]
fn failure_names_the_error_and_both_remediations() {
    let (v, lines) = report(
        &Outcome::Failed {
            error: "fatal: boom".into(),
        },
        "/repo",
        "/repo/wt",
    );
    assert_eq!(v, FAILED);
    let text: Vec<&str> = lines.iter().map(|(_, m)| m.as_str()).collect();
    assert_eq!(
        text,
        [
            "Could not remove worktree at /repo/wt (best-effort cleanup — the merge itself already succeeded and is unaffected):",
            "fatal: boom",
            "Remediation: git worktree prune && git -C \"/repo\" worktree remove \"/repo/wt\" --force",
            "If that still fails: rm -rf \"/repo/wt\" && git -C \"/repo\" worktree prune",
        ]
    );
    assert!(lines.iter().all(|(l, _)| *l == Level::Warning));
}

#[test]
fn a_multi_line_error_becomes_one_record_per_line() {
    let (_, lines) = report(
        &Outcome::Failed {
            error: "fatal: cannot remove a locked working tree;\nuse 'remove -f -f' to override"
                .into(),
        },
        "/r",
        "/r/wt",
    );
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[1].1, "fatal: cannot remove a locked working tree;");
    assert_eq!(lines[2].1, "use 'remove -f -f' to override");
}

#[test]
fn an_empty_error_still_emits_its_one_empty_line() {
    let (_, lines) = report(
        &Outcome::Failed {
            error: String::new(),
        },
        "/r",
        "/r/wt",
    );
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[1], (Level::Warning, String::new()));
}

#[test]
fn render_leads_with_the_verdict_and_tabs_each_record() {
    let s = render(REMOVED, &[(Level::Success, "Worktree removed".to_string())]);
    assert_eq!(s, "LOOM-WORKTREE-TEARDOWN REMOVED\nSUCCESS\tWorktree removed\n");
}

#[test]
fn a_real_worktree_is_removed_and_a_bogus_path_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=t@e",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "i",
    ]);
    let wt = tmp.path().join("wt one");
    git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feature/issue-1",
        wt.to_str().unwrap(),
    ]);

    let out = teardown(repo.to_str().unwrap(), wt.to_str().unwrap());
    assert_eq!(out, format!("{REMOVED}\nSUCCESS\tWorktree removed\n"));
    assert!(!wt.exists());

    let bogus = tmp.path().join("never-a-worktree");
    let out = teardown(repo.to_str().unwrap(), bogus.to_str().unwrap());
    assert!(out.starts_with(&format!("{FAILED}\n")), "{out}");
    assert!(out.contains("is not a working tree"), "{out}");
}
