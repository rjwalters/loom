//! End-to-end tests for `consolidate-reconcile` (#9689): the real
//! orchestration against a stub `gh` (state kept in files, every call logged)
//! and a real git repository for the ancestry proof.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::merge_pr::consolidate::{
    construct, mapping_body, remove_worktree, reservation_comment_body, reservation_marker,
    ComponentState,
};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const CANDIDATE: u32 = 99;
const ATTEMPT: &str = "cons-ab12cd34";
const MERGE_SHA: &str = "c0ffee0000000000000000000000000000000001";

/// A stub `gh` whose forge state lives in `state/`:
/// `comments-<n>/*.json` (one comment array per file, concatenated on read —
/// the paginated listing shape), `pr-<n>.state`, `pr-<n>.body`,
/// `issue-<n>.state`, `cand.json`, `fail-label-<n>` (label edit fails),
/// `branch-gone`. Every invocation is appended to `state/log`.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
S="@STATE@"
printf '%s\n' "$*" >> "$S/log"
record() { # record <pr> <body>
  mkdir -p "$S/comments-$1"
  c=$(cat "$S/seq" 2>/dev/null || echo 100); c=$((c+1)); echo "$c" > "$S/seq"
  jq -n --arg b "$2" '[{body:$b, author_association:"OWNER", user:{login:"fleet", type:"User"}}]' \
    > "$S/comments-$1/$c.json"
}
case "$1 $2" in
  "pr view")
    if [ "$3" = "@CAND@" ]; then cat "$S/cand.json"; exit 0; fi
    case "$5" in
      state) cat "$S/pr-$3.state" ;;
      body) cat "$S/pr-$3.body" ;;
    esac
    exit 0 ;;
  "pr comment") record "$3" "$5"; exit 0 ;;
  "pr edit")
    if [ -e "$S/fail-label-$3" ]; then echo "HTTP 403: Resource not accessible" >&2; exit 1; fi
    exit 0 ;;
  "pr close") echo CLOSED > "$S/pr-$3.state"; record "$3" "$5"; exit 0 ;;
  "issue view") cat "$S/issue-$3.state"; exit 0 ;;
  "issue close") echo CLOSED > "$S/issue-$3.state"; exit 0 ;;
  "api -X")
    if [ -e "$S/branch-gone" ]; then
      echo "gh: Reference does not exist (HTTP 422)" >&2; exit 1
    fi
    touch "$S/branch-gone"; exit 0 ;;
esac
if [ "$1" = "api" ]; then
  n=${2#*issues/}; n=${n%/comments}
  if ls "$S/comments-$n"/*.json >/dev/null 2>&1; then cat "$S/comments-$n"/*.json; else echo '[]'; fi
  exit 0
fi
echo "fake gh: unhandled: $*" >&2
exit 1
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    gh: PathBuf,
    /// Pinned heads of the two included components (#10, #12).
    a: String,
    b: String,
    /// A head created after construction — never an ancestor of the candidate.
    late: String,
    candidate_head: String,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        let commit = |branch: &str, from: &str, file: &str| {
            git(&["checkout", "-qb", branch, from]);
            std::fs::write(repo.join(file), format!("{file}\n")).unwrap();
            git(&["add", "."]);
            git(&["commit", "-qm", file]);
            git(&["rev-parse", "HEAD"])
        };
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        let a = commit("a", &base, "a.txt");
        let b = commit("b", &base, "b.txt");
        let worktree = tmp.path().join("scratch");
        let candidate_head =
            construct("git", &repo, &worktree, &base, &[(10, &a), (12, &b)]).unwrap();
        remove_worktree("git", &repo, &worktree);
        let late = commit("late", &base, "late.txt");

        let gh = tmp.path().join("fake-gh.sh");
        let script = FAKE_GH
            .replace("@STATE@", &state.display().to_string())
            .replace("@CAND@", &CANDIDATE.to_string());
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _tmp: tmp,
            repo,
            state,
            gh,
            a,
            b,
            late,
            candidate_head,
        }
    }

    fn put(&self, name: &str, content: &str) {
        std::fs::write(self.state.join(name), content).unwrap();
    }

    /// A MERGED candidate mapping `components`, each reserved by this
    /// attempt, OPEN, and declaring `Closes #<500 + n>`.
    fn seed(&self, components: &[(u32, &str)]) {
        let body = mapping_body(ATTEMPT, "main", &self.candidate_head, components, "overlap");
        let cand = serde_json::json!({
            "state": "MERGED",
            "body": body,
            "headRefName": format!("loom/consolidated/{ATTEMPT}"),
            "mergeCommit": {"oid": MERGE_SHA},
        });
        self.put("cand.json", &cand.to_string());
        for (n, head) in components {
            let source = ComponentState {
                number: *n,
                state: "OPEN".into(),
                draft: false,
                head_sha: Some((*head).to_string()),
                base_ref: "main".into(),
                labels: vec![SEQUENCE_LABEL.into()],
                files: std::collections::BTreeSet::new(),
                additions: 1,
                deletions: 0,
            };
            let marker = reservation_marker(CANDIDATE, &self.candidate_head, &source, ATTEMPT);
            let reservation = reservation_comment_body(&marker, ATTEMPT);
            std::fs::create_dir_all(self.state.join(format!("comments-{n}"))).unwrap();
            let listing = serde_json::json!([{
                "body": reservation,
                "author_association": "OWNER",
                "user": {"login": "fleet", "type": "User"},
            }]);
            self.put(&format!("comments-{n}/000.json"), &listing.to_string());
            self.put(&format!("pr-{n}.state"), "OPEN\n");
            self.put(&format!("pr-{n}.body"), &format!("Closes #{}\n", 500 + n));
            self.put(&format!("issue-{}.state", 500 + n), "OPEN\n");
        }
    }

    fn run(&self) -> ReconcileReport {
        reconcile(&self.gh, "git", &self.repo, CANDIDATE).unwrap()
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn writes(&self) -> Vec<String> {
        const WRITES: [&str; 5] = ["pr comment", "pr edit", "pr close", "issue close", "api -X"];
        self.log()
            .into_iter()
            .filter(|l| WRITES.iter().any(|w| l.starts_with(w)))
            .collect()
    }

    fn comments(&self, n: u32) -> String {
        let dir = self.state.join(format!("comments-{n}"));
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .iter()
            .map(|f| std::fs::read_to_string(f).unwrap())
            .collect()
    }

    fn state_of(&self, name: &str) -> String {
        std::fs::read_to_string(self.state.join(name))
            .unwrap()
            .trim()
            .to_string()
    }
}

#[test]
fn a_full_run_reconciles_every_step_and_a_restart_writes_nothing() {
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);

    let first = f.run();
    assert_eq!(
        (first.statuses, first.released, first.closed_prs, first.closed_issues),
        (2, 2, 2, 2),
        "{first:?}"
    );
    assert_eq!(first.branch, BranchCleanup::Deleted);
    assert!(first.complete(), "{first:?}");
    for n in [10, 12] {
        assert_eq!(f.state_of(&format!("pr-{n}.state")), "CLOSED");
        assert_eq!(f.state_of(&format!("issue-{}.state", 500 + n)), "CLOSED");
        let c = f.comments(n);
        assert!(c.contains(MERGE_SHA), "closure must carry the merge SHA: {c}");
        assert!(c.contains(&format!("released plan={ATTEMPT}")), "{c}");
    }

    // Restart after a completed run: finishes bookkeeping that is already
    // finished — zero forge writes beyond the (already-gone) branch probe.
    let writes_before = f.writes().len();
    let second = f.run();
    assert_eq!(
        (second.statuses, second.released, second.closed_prs, second.closed_issues),
        (0, 0, 0, 0),
        "a restart must not repeat any step: {second:?}"
    );
    assert_eq!(second.branch, BranchCleanup::AlreadyGone, "already-deleted is not a failure");
    assert!(second.complete(), "{second:?}");
    let new_writes: Vec<_> = f.writes().into_iter().skip(writes_before).collect();
    assert!(
        new_writes.iter().all(|w| w.starts_with("api -X DELETE")),
        "a restart re-wrote forge state: {new_writes:?}"
    );
}

#[test]
fn an_unverified_component_stays_open_and_untouched() {
    let f = Fixture::new();
    // #14's pinned head is NOT an ancestor of the recorded candidate head.
    f.seed(&[(10, &f.a), (14, &f.late)]);

    let report = f.run();
    assert_eq!(report.unverified, vec![14]);
    assert!(!report.complete(), "an unverified component must fail the run's exit status");
    assert_eq!(f.state_of("pr-14.state"), "OPEN");
    assert_eq!(f.state_of("issue-514.state"), "OPEN", "its declared issue stays open");
    assert!(
        !f.writes()
            .iter()
            .any(|w| w.split_whitespace().nth(2) == Some("14")),
        "nothing may be written to an unverified component: {:?}",
        f.writes()
    );
    // The verified sibling is reconciled normally.
    assert_eq!(f.state_of("pr-10.state"), "CLOSED");
}

#[test]
fn a_failed_label_removal_posts_no_release_and_a_rerun_retries() {
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.put("fail-label-10", "");

    let first = f.run();
    assert_eq!(first.release_failed, vec![10]);
    assert_eq!(first.released, 1, "#12's release is unaffected");
    assert!(!first.complete());
    assert!(
        !f.comments(10).contains("released plan="),
        "no release may be claimed while the label is still on: {}",
        f.comments(10)
    );

    std::fs::remove_file(f.state.join("fail-label-10")).unwrap();
    let retry = f.run();
    assert_eq!(retry.released, 1, "the retry releases #10 only: {retry:?}");
    assert!(retry.release_failed.is_empty());
    assert!(f.comments(10).contains(&format!("released plan={ATTEMPT}")));
    assert_eq!(f.comments(12).matches("released plan=").count(), 1, "#12 not released twice");
}

#[test]
fn an_unmerged_candidate_is_refused_without_any_write() {
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.put(
        "cand.json",
        &serde_json::json!({"state": "OPEN", "body": "", "headRefName": "x", "mergeCommit": null})
            .to_string(),
    );
    let err = reconcile(&f.gh, "git", &f.repo, CANDIDATE).unwrap_err();
    assert!(err.to_string().contains("land it first"), "{err}");
    assert!(f.writes().is_empty(), "{:?}", f.writes());
}

#[test]
fn branch_cleanup_classifies_an_already_deleted_ref_as_done() {
    assert_eq!(branch_cleanup_outcome(true, ""), BranchCleanup::Deleted);
    assert_eq!(
        branch_cleanup_outcome(false, "gh: Reference does not exist (HTTP 422)"),
        BranchCleanup::AlreadyGone
    );
    assert_eq!(
        branch_cleanup_outcome(false, "gh: Not Found (HTTP 404)"),
        BranchCleanup::AlreadyGone
    );
    assert!(matches!(
        branch_cleanup_outcome(false, "gh: Bad credentials (HTTP 401)"),
        BranchCleanup::Failed(_)
    ));
}
