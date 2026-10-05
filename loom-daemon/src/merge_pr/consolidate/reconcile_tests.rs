//! End-to-end tests for `consolidate-reconcile` (#9689): the real
//! orchestration against a stub `gh` (state kept in files, every call logged)
//! and a real git repository for the ancestry proof.
//!
//! The stub is installed as `LOOM_GH_BIN`: `reconcile` spawns `gh` only
//! through the `gh_invocation` facade (#9985), which resolves the program
//! itself and takes no per-call override. Every test that runs it is therefore
//! `#[serial(loom_config_env)]` — the crate's one writer group for
//! `LOOM_GH_BIN` (#8465/#8480) — and holds the variable only while the stub
//! actually runs ([`StubGh`]).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::gh_invocation::ParentContext;
use crate::merge_pr::consolidate::{
    construct, mapping_body, remove_worktree, reservation_comment_body, reservation_marker,
    ComponentState,
};
use crate::proc_exec::ExecError;
use serial_test::serial;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

/// `LOOM_GH_BIN` pointed at the stub while alive; the prior value is restored
/// on drop, so a panicking assertion cannot leak the stub into a later test.
struct StubGh(Option<OsString>);

impl StubGh {
    fn install(gh: &Path) -> Self {
        let prior = std::env::var_os("LOOM_GH_BIN");
        std::env::set_var("LOOM_GH_BIN", gh);
        Self(prior)
    }
}

impl Drop for StubGh {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("LOOM_GH_BIN", v),
            None => std::env::remove_var("LOOM_GH_BIN"),
        }
    }
}

const CANDIDATE: u32 = 99;
const ATTEMPT: &str = "cons-ab12cd34";
const MERGE_SHA: &str = "c0ffee0000000000000000000000000000000001";

/// A stub `gh` whose forge state lives in `state/`:
/// `comments-<n>/*.json` (one comment array per file, concatenated on read —
/// the paginated listing shape), `pr-<n>.state`, `pr-<n>.body`,
/// `pr-<n>.head` (live head), `pr-<n>.labels` (one per line),
/// `issue-<n>.state`, `cand.json`, `branch-gone`; `fail-pr-<n>-<field>` /
/// `fail-issue-<n>` make that read exit non-zero. Every invocation is
/// appended to `state/log`.
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
    [ -e "$S/fail-pr-$3-$5" ] && exit 1
    case "$5" in
      state) cat "$S/pr-$3.state" ;;
      body) cat "$S/pr-$3.body" ;;
      headRefOid) cat "$S/pr-$3.head" ;;
      labels) cat "$S/pr-$3.labels" ;;
    esac
    exit 0 ;;
  "pr comment") record "$3" "$5"; exit 0 ;;
  "pr edit") exit 0 ;;
  "pr close") echo CLOSED > "$S/pr-$3.state"; record "$3" "$5"; exit 0 ;;
  "issue view") [ -e "$S/fail-issue-$3" ] && exit 1; cat "$S/issue-$3.state"; exit 0 ;;
  "issue close") echo CLOSED > "$S/issue-$3.state"; exit 0 ;;
  "api -X")
    if [ -e "$S/branch-gone" ]; then
      echo "gh: Reference does not exist (HTTP 422)" >&2; exit 1
    fi
    touch "$S/branch-gone"; exit 0 ;;
esac
if [ "$1" = "api" ]; then
  n=${2#*issues/}; n=${n%%\?*}; n=${n%/comments}
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
    /// A commit ON TOP of the recorded candidate head (a base merged in at
    /// landing): a landed head that still contains the recorded one.
    landed_descendant: String,
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
        let landed_descendant = commit("landed", &candidate_head, "landed.txt");

        let gh = tmp.path().join("fake-gh.sh");
        let script = FAKE_GH
            .replace("@STATE@", &state.display().to_string())
            .replace("@CAND@", &CANDIDATE.to_string());
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        // ETXTBSY guard (the same harness race worktree_ops/clean/tests.rs
        // retries): a concurrent test thread that forked while this script's
        // write fd was open holds it until its exec, and Linux refuses to
        // exec the script meanwhile. Wait that out here, so the run under test
        // never sees "Text file busy". The probe's log line is cleared. It
        // spawns through the facade too (#9985), which keeps the spawn error's
        // `io::ErrorKind`.
        {
            let _stub = StubGh::install(&gh);
            for _ in 0..100 {
                let probe = GhInvocation::new(
                    Operation::new("test.probe"),
                    AccessIntent::Read,
                    GhTarget::None,
                    std::time::Duration::from_secs(10),
                )
                .parent(ParentContext::Missing)
                .args(["--probe"])
                .execute();
                match probe {
                    Err(ExecError::Spawn(e))
                        if e.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    _ => break,
                }
            }
        }
        let _ = std::fs::remove_file(state.join("log"));
        Self {
            _tmp: tmp,
            repo,
            state,
            gh,
            a,
            b,
            late,
            candidate_head,
            landed_descendant,
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
            "headRefOid": self.candidate_head,
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
            self.put(&format!("pr-{n}.head"), &format!("{head}\n"));
            self.put(&format!("pr-{n}.labels"), &format!("{SEQUENCE_LABEL}\n"));
            self.put(&format!("pr-{n}.body"), &format!("Closes #{}\n", 500 + n));
            self.put(&format!("issue-{}.state", 500 + n), "OPEN\n");
        }
    }

    fn try_run(&self) -> Result<ReconcileReport> {
        let _stub = StubGh::install(&self.gh);
        reconcile("git", &self.repo, CANDIDATE)
    }

    fn run(&self) -> ReconcileReport {
        self.try_run().unwrap()
    }

    /// Rewrite the seeded candidate's landed head (`headRefOid`).
    fn set_landed_head(&self, head: &str) {
        let mut cand: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(self.state.join("cand.json")).unwrap())
                .unwrap();
        cand["headRefOid"] = serde_json::Value::String(head.to_string());
        self.put("cand.json", &cand.to_string());
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
#[serial(loom_config_env)]
fn a_full_run_reconciles_every_step_and_a_restart_writes_nothing() {
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);

    let first = f.run();
    assert_eq!((first.statuses, first.closed_prs, first.closed_issues), (2, 2, 2), "{first:?}");
    // Pin the request shape the stub parses: a query-string change to the
    // comments read must update FAKE_GH's PR-number extraction too (#10089).
    assert!(f.log().iter().any(|l| l.contains("/comments?per_page=100")), "{:?}", f.log());
    assert_eq!(first.branch, BranchCleanup::Deleted);
    assert!(first.complete(), "{first:?}");
    assert_eq!(first.holds_pending_release, vec![10, 12], "observed, not released");
    for n in [10, 12] {
        assert_eq!(f.state_of(&format!("pr-{n}.state")), "CLOSED");
        assert_eq!(f.state_of(&format!("issue-{}.state", 500 + n)), "CLOSED");
        let c = f.comments(n);
        assert!(c.contains(MERGE_SHA), "closure must carry the merge SHA: {c}");
    }

    // Restart after a completed run: finishes bookkeeping that is already
    // finished — zero forge writes beyond the (already-gone) branch probe.
    let writes_before = f.writes().len();
    let second = f.run();
    assert_eq!(
        (second.statuses, second.closed_prs, second.closed_issues),
        (0, 0, 0),
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
#[serial(loom_config_env)]
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
#[serial(loom_config_env)]
fn reconciliation_never_releases_a_reservation() {
    // ADR-0023 §4 (revised 2026-10-01): the ordering pass is the ONE releaser
    // on landing. Reconciliation removes no label and posts no release
    // tombstone, whether or not the pass has released yet.
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    // The pass already released #10; #12 still carries its hold.
    f.put("pr-10.labels", "");

    let report = f.run();
    assert!(report.complete(), "a pending release is not a failure: {report:?}");
    assert_eq!(report.holds_pending_release, vec![12]);
    assert!(
        !f.writes().iter().any(|w| w.starts_with("pr edit")),
        "reconciliation must not touch labels: {:?}",
        f.writes()
    );
    for n in [10, 12] {
        assert!(
            !f.comments(n).contains("loom:sequence released"),
            "reconciliation must not post a release: {}",
            f.comments(n)
        );
        // Closed regardless of the label state (the label on a closed PR is
        // inert; reconciliation does not wait for the pass).
        assert_eq!(f.state_of(&format!("pr-{n}.state")), "CLOSED");
    }
}

#[test]
#[serial(loom_config_env)]
fn a_source_pushed_after_landing_is_left_untouched_open() {
    // ADR-0023 §6.2: the landed pin is on main, the newer commits are not.
    // Past the abort point, so not an abort: status `untouched-open`, the PR
    // and its declared issue stay open.
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.put("pr-12.head", &format!("{}\n", f.late));

    let report = f.run();
    assert_eq!(report.untouched_open, vec![12], "{report:?}");
    assert!(report.complete(), "untouched-open is an end state: {report:?}");
    assert_eq!(f.state_of("pr-12.state"), "OPEN");
    assert_eq!(f.state_of("issue-512.state"), "OPEN");
    let c = f.comments(12);
    assert!(c.contains("status=untouched-open"), "{c}");
    assert!(c.contains(&f.b), "the status names the pinned head that landed: {c}");
    assert!(!c.contains("merged-into #"), "never recorded as merged-into: {c}");
    // The sibling at its pin is reconciled normally.
    assert_eq!(f.state_of("pr-10.state"), "CLOSED");

    // Idempotent: a re-run posts no second untouched-open status.
    let again = f.run();
    assert_eq!(again.statuses, 0, "{again:?}");
    assert_eq!(f.comments(12).matches("status=untouched-open").count(), 1);
}

#[test]
#[serial(loom_config_env)]
fn an_unmerged_candidate_is_refused_without_any_write() {
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.put(
        "cand.json",
        &serde_json::json!({"state": "OPEN", "body": "", "headRefName": "x", "mergeCommit": null})
            .to_string(),
    );
    let err = f.try_run().unwrap_err();
    assert!(err.to_string().contains("land it first"), "{err}");
    assert!(f.writes().is_empty(), "{:?}", f.writes());
}

#[test]
#[serial(loom_config_env)]
fn a_candidate_that_landed_without_its_recorded_head_is_refused_without_any_write() {
    // Every component IS an ancestor of the recorded head, so the per-
    // component proof alone would pass. But the candidate landed at a head
    // that does not contain the recorded one (rewritten after preparation):
    // the proof says nothing about what landed, so nothing may be closed.
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.set_landed_head(&f.late);

    let err = f.try_run().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("does not contain the head recorded"), "{msg}");
    assert!(msg.contains(&f.late) && msg.contains(&f.candidate_head), "{msg}");
    assert!(f.writes().is_empty(), "a refused landing wrote: {:?}", f.writes());
    for n in [10, 12] {
        assert_eq!(f.state_of(&format!("pr-{n}.state")), "OPEN");
        assert_eq!(f.state_of(&format!("issue-{}.state", 500 + n)), "OPEN");
    }
}

#[test]
#[serial(loom_config_env)]
fn a_candidate_whose_landed_head_is_withheld_is_refused_without_any_write() {
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.set_landed_head("");

    let err = f.try_run().unwrap_err();
    assert!(err.to_string().contains("headRefOid"), "{err}");
    assert!(f.writes().is_empty(), "{:?}", f.writes());
}

#[test]
#[serial(loom_config_env)]
fn a_landed_head_that_descends_from_the_recorded_one_reconciles() {
    // Ancestry, not equality: a base merged into the candidate at landing
    // keeps the recorded head (and so every included pin) in the landed tree.
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.set_landed_head(&f.landed_descendant);

    let report = f.run();
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.closed_prs, 2, "{report:?}");
}

#[test]
#[serial(loom_config_env)]
fn a_candidate_outside_the_namespace_is_refused_without_any_write() {
    // A mapping marker in an (editable) body does not make a contributor's
    // branch a candidate: step 7 would otherwise delete it (#9372).
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    let mut cand: serde_json::Value = serde_json::from_str(&f.state_of("cand.json")).unwrap();
    cand["headRefName"] = serde_json::Value::String("feature/issue-10".into());
    f.put("cand.json", &cand.to_string());

    let err = f.try_run().unwrap_err();
    assert!(err.to_string().contains("not a consolidation candidate"), "{err}");
    assert!(f.writes().is_empty(), "{:?}", f.writes());
}

#[test]
#[serial(loom_config_env)]
fn an_unanswered_step5_state_read_is_unread_not_complete() {
    // #9972: a failed `pr view --json state` must not read as "already closed".
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.put("fail-pr-10-state", "");

    let first = f.run();
    assert_eq!(first.unread, vec![10], "{first:?}");
    assert!(!first.complete(), "{first:?}");
    assert_eq!(f.state_of("pr-10.state"), "OPEN");
    assert_eq!(f.state_of("issue-510.state"), "OPEN");
    assert_eq!(f.state_of("pr-12.state"), "CLOSED");

    // Once the forge answers, a re-run finishes the job.
    std::fs::remove_file(f.state.join("fail-pr-10-state")).unwrap();
    let second = f.run();
    assert!(second.complete(), "{second:?}");
    assert_eq!(f.state_of("pr-10.state"), "CLOSED");
    assert_eq!(f.state_of("issue-510.state"), "CLOSED");
}

#[test]
#[serial(loom_config_env)]
fn an_unanswered_step6_read_is_unread_not_complete() {
    // #9972: a failed body read, or a failed issue-state read, leaves the run
    // incomplete instead of silently skipping the issue closure.
    let f = Fixture::new();
    f.seed(&[(10, &f.a), (12, &f.b)]);
    f.put("fail-pr-10-body", "");
    f.put("fail-issue-512", "");

    let report = f.run();
    let mut unread = report.unread.clone();
    unread.sort_unstable();
    assert_eq!(unread, vec![10, 12], "{report:?}");
    assert!(!report.complete(), "{report:?}");
    assert_eq!(f.state_of("issue-510.state"), "OPEN");
    assert_eq!(f.state_of("issue-512.state"), "OPEN");
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
