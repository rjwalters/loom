//! Tests for the re-date remedy and its bounded escalation (#8508).
//!
//! The pure decision logic (`decide`, `commit_message`, the marker/idempotency
//! predicates, the two comment bodies) is exercised directly. [`remedy_with`]
//! additionally gets integration-style coverage against a stub `gh` script —
//! unlike `stale_checks::fetch` / `head_sync::fetch`, this module WRITES to
//! the forge (creates a commit, moves a ref, comments, labels), so getting its
//! call sequence and outcome mapping wrong is a real-commit-producing bug, not
//! just a stale-read bug; the extra coverage is worth the stub. Injected as a
//! plain function argument (`remedy_with(gh_path, ...)`), never a
//! `LOOM_GH_BIN` env var — that would race across `cargo test`'s parallel
//! threads in one process.

use super::attribution::{recompute_with, sanitize, TIME_RULE_CLAUSE};
use super::*;
use crate::merge_pr::stale_checks::inputs::StaleReason;
use crate::merge_pr::stale_checks::Verdict;
use std::fs;
use std::io::Write;

// --- Pure logic ---------------------------------------------------------

#[test]
fn decide_proceeds_when_current_matches_expected() {
    assert_eq!(decide("abc123", "abc123"), PushDecision::Proceed);
}

#[test]
fn decide_refuses_when_the_branch_already_moved() {
    assert_eq!(
        decide("abc123", "def456"),
        PushDecision::HeadMoved {
            current: "def456".to_string()
        }
    );
}

#[test]
fn decide_is_case_sensitive_on_sha() {
    // A SHA compare must never be lossy: different case is a different
    // string, and there is no forge API where that arises legitimately, but
    // the gate must not silently treat it as a match either.
    assert_eq!(
        decide("ABC123", "abc123"),
        PushDecision::HeadMoved {
            current: "abc123".to_string()
        }
    );
}

#[test]
fn commit_message_names_the_pr_and_the_guard() {
    let msg = commit_message("8493");
    for needle in ["#8493", "#8248", "actions:write", "#8508"] {
        assert!(msg.contains(needle), "commit message should mention {needle:?}, got: {msg}");
    }
}

#[test]
fn commit_message_is_deterministic() {
    assert_eq!(commit_message("1"), commit_message("1"));
    assert_ne!(commit_message("1"), commit_message("2"));
}

#[test]
fn the_redate_subject_classifier_recognizes_exactly_what_commit_message_writes() {
    // Producer and classifier (#9337's `stale_main_bump`) must not drift.
    for pr in ["123", "8493", "1"] {
        let msg = commit_message(pr);
        assert!(is_redate_commit_subject(msg.lines().next().unwrap()), "{msg}");
    }
    for near_miss in [
        "chore: re-date required checks for PR #123",
        "chore: re-date required checks for PR # (#8248 guard, automated by #8508)",
        "chore: re-date required checks for PR #12a (#8248 guard, automated by #8508)",
        "chore: re-date required checks for PR #123 (#8248 guard, automated by #8508) ",
        " chore: re-date required checks for PR #123 (#8248 guard, automated by #8508)",
        "fix: something else (#123)",
        "",
    ] {
        assert!(!is_redate_commit_subject(near_miss), "{near_miss:?}");
    }
    // Only the first line is a subject; a whole message is not.
    assert!(!is_redate_commit_subject(&commit_message("123")));
}

#[test]
fn remedy_outcome_variants_are_distinct() {
    // Guards against a match-arm regression collapsing the outcomes — callers
    // branch on all four (HeadMoved = re-queue silently, Pushed = report and
    // wait for CI, Escalated = a human now owns it, Failed = keep the original
    // refusal), the same shape as merge-pr.sh's #5579 exit-3 vs exit-1 split.
    let moved = RemedyOutcome::HeadMoved {
        current: "abc".to_string(),
    };
    let pushed = RemedyOutcome::Pushed {
        new_sha: "abc".to_string(),
    };
    assert_ne!(moved, pushed);
    assert_ne!(
        RemedyOutcome::Escalated {
            notice_posted: true,
            spent: 1,
            budget: 1
        },
        RemedyOutcome::Escalated {
            notice_posted: false,
            spent: 1,
            budget: 1
        }
    );
}

// --- The one-remedy-per-head bound, as a pure predicate ------------------

#[test]
fn already_redated_matches_only_the_exact_head() {
    let blob = format!("some review chatter\n{}\nmore text", redate_marker("aaa111"));
    assert!(already_redated(&blob, "aaa111"), "the marker for THIS head must be found");
    assert!(
        !already_redated(&blob, "bbb222"),
        "a marker for a different head must not bound a fresh head"
    );
}

#[test]
fn already_redated_is_false_on_an_empty_thread() {
    assert!(!already_redated("", "aaa111"));
}

#[test]
fn already_redated_does_not_match_a_sha_prefix() {
    // `to=aaa111 -->` must not be satisfied by a longer sha that merely starts
    // with it: the marker carries the closing delimiter for exactly this
    // reason, and a prefix match would silently bound an unrelated head.
    let blob = redate_marker("aaa1112222");
    assert!(!already_redated(&blob, "aaa111"));
    assert!(already_redated(&blob, "aaa1112222"));
}

#[test]
fn hold_notice_idempotency_is_keyed_on_the_head() {
    let blob = hold_marker("aaa111");
    assert!(hold_already_posted(&blob, "aaa111"));
    assert!(
        !hold_already_posted(&blob, "bbb222"),
        "a later push must re-open the question with a fresh notice"
    );
}

#[test]
fn redate_comment_body_records_the_marker_and_explains_the_re_review() {
    let body = redate_comment_body("8493", "abc1234def", 2, 3);
    assert!(body.starts_with(&redate_marker("abc1234def")));
    assert!(body.contains(&budget::attempt_marker("abc1234def", 2)), "{body}");
    assert!(body.contains("2 of 3"), "{body}");
    for needle in ["#8248", "#8508", "tree-identical", "#5686", "abc1234"] {
        assert!(body.contains(needle), "body should mention {needle:?}: {body}");
    }
}

#[test]
fn hold_comment_body_names_the_label_and_both_human_exits() {
    let body = hold_comment_body("8493", "abc1234def", "because reasons");
    assert!(body.starts_with(&hold_marker("abc1234def")));
    for needle in [
        "loom:operator",
        "actions:write",
        "push any commit",
        "because reasons",
        "#8248",
    ] {
        assert!(body.contains(needle), "body should mention {needle:?}: {body}");
    }
}

#[test]
fn comment_bodies_tolerate_a_short_sha() {
    // The short-sha slice must never panic on a sha shorter than 7 chars —
    // `--expected-head-sha` is caller-supplied, and a truncated value is a
    // bad-input case, not a crash case.
    assert!(redate_comment_body("1", "ab", 1, 1).contains("ab"));
    assert!(hold_comment_body("1", "ab", "r").contains("ab"));
}

// --- Stubbed forge I/O ---------------------------------------------------

/// A budget of one with no backoff: #8508's original "one re-date per head,
/// then escalate" bound, which the pre-#9590 tests below pin.
const ONE: BudgetConfig = BudgetConfig {
    budget: 1,
    backoff_secs: 0,
};

/// A fixed "now" for the stubbed runs, so backoff decisions are deterministic.
pub(super) fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z")
        .expect("valid timestamp")
        .with_timezone(&chrono::Utc)
}

pub(super) fn run(gh: &std::path::Path, cfg: BudgetConfig) -> RemedyOutcome {
    remedy_generic_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42", cfg, now())
}

/// Writes an executable fake `gh` to `dir` that answers the calls
/// [`remedy_with`] makes from canned values — and appends every invocation's
/// argv (sentinel-terminated, see [`split_stub_calls`]) to `dir/argv.log` for
/// assertions. Requests carrying `--input -` also have their stdin body
/// appended, so comment/label payloads are assertable. The ref-heads path is
/// reused for BOTH the read (no `-X`) and the write (`-X PATCH`); the case
/// arms tell them apart by scanning for `-X PATCH` anywhere in argv,
/// mirroring this repo's other stubs.
pub(super) fn write_stub_gh(
    dir: &std::path::Path,
    current_sha: &str,
    tree_sha: &str,
    new_sha: &str,
    comments: &str,
    fail_at: &str,
) -> std::path::PathBuf {
    let path = dir.join("gh");
    // #9548: the listing the remedy reads is REST JSON, filtered by author;
    // a non-empty `comments` is one comment by this fleet's default App.
    let listing = if comments.is_empty() {
        serde_json::json!([])
    } else {
        serde_json::json!([{
            "user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"},
            "author_association": "NONE",
            "body": comments,
        }])
    };
    fs::write(dir.join("comments.json"), listing.to_string()).expect("write comments listing");
    let script = format!(
        r#"#!/usr/bin/env bash
set -uo pipefail
shift # drop leading "api"
# One record per call, terminated by a sentinel line rather than a bare
# newline — the create-commit call's `message=` argument and every `--input -`
# body embed real newlines, so a plain newline-per-record log would miscount.
BODY=""
for arg in "$@"; do
  if [ "$arg" = "--input" ]; then BODY="$(cat)"; break; fi
done
{{ printf '%s\nSTDIN:%s\n<<<REDATE-STUB-CALL-END>>>\n' "$*" "$BODY"; }} >> "{dir}/argv.log"

# The path arg can be ANYWHERE in argv — real `gh api -X PATCH <path> ...`
# puts `-X PATCH` BEFORE it — so find it by shape rather than assuming $1.
PATH_ARG=""
IS_PATCH=0
prev=""
for arg in "$@"; do
  if [ "$prev" = "-X" ] && [ "$arg" = "PATCH" ]; then
    IS_PATCH=1
  fi
  case "$arg" in
    repos/*) PATH_ARG="$arg" ;;
  esac
  prev="$arg"
done

case "$PATH_ARG" in
  */git/refs/heads/*)
    if [ "$IS_PATCH" = 1 ]; then
      [ "{fail_at}" = "patch" ] && exit 1
      exit 0
    else
      [ "{fail_at}" = "ref" ] && exit 1
      echo "{current_sha}"
    fi
    ;;
  */issues/*/comments)
    if [ -n "$BODY" ]; then
      [ "{fail_at}" = "comment" ] && exit 1
      echo '{{"id":1}}'
    else
      [ "{fail_at}" = "read-comments" ] && exit 1
      cat "{dir}/comments.json"
    fi
    ;;
  */issues/*/labels)
    [ "{fail_at}" = "label" ] && exit 1
    echo '[]'
    ;;
  */git/commits/*)
    [ "{fail_at}" = "tree" ] && exit 1
    echo "{tree_sha}"
    ;;
  */git/commits)
    [ "{fail_at}" = "create" ] && exit 1
    echo "{new_sha}"
    ;;
  *)
    echo "stub gh: unexpected path '$PATH_ARG' in args: $*" >&2
    exit 2
    ;;
esac
"#,
        dir = dir.display(),
    );
    let mut f = fs::File::create(&path).expect("write stub gh");
    f.write_all(script.as_bytes())
        .expect("write stub gh contents");
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub gh");
    }
    path
}

pub(super) fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-redate-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Split `argv.log` into one entry per recorded call. NOT `.lines()`: the
/// create-commit call's `message=` argument and the comment bodies embed real
/// newlines, so the stub terminates each record with an explicit sentinel
/// instead of relying on one call == one line.
pub(super) fn split_stub_calls(argv_log: &str) -> Vec<&str> {
    argv_log
        .split("<<<REDATE-STUB-CALL-END>>>\n")
        .map(str::trim_end)
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn remedy_pushes_a_tree_identical_commit_when_head_matches() {
    let dir = tmp_dir("happy-path");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");

    let outcome = run(&gh, ONE);
    assert_eq!(
        outcome,
        RemedyOutcome::Pushed {
            new_sha: "newsha22".to_string()
        }
    );

    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let calls = split_stub_calls(&argv);
    assert_eq!(calls.len(), 6, "expected exactly 6 gh api calls, got: {argv}");
    assert!(
        calls[0].starts_with("repos/o/r/git/refs/heads/feature/x"),
        "call 1 reads the ref: {}",
        calls[0]
    );
    assert!(
        calls[1].starts_with("repos/o/r/issues/42/comments"),
        "call 2 reads the attempt state BEFORE writing anything: {}",
        calls[1]
    );
    assert!(
        calls[2].starts_with("repos/o/r/git/commits/abc0000"),
        "call 3 reads the tree: {}",
        calls[2]
    );
    assert!(
        calls[3].starts_with("repos/o/r/git/commits "),
        "call 4 creates the commit: {}",
        calls[3]
    );
    assert!(
        calls[3].contains("parents[]=abc0000"),
        "new commit's parent is the current head: {}",
        calls[3]
    );
    assert!(
        calls[3].contains("tree=tree1111"),
        "new commit reuses the CURRENT tree — the diff must not change: {}",
        calls[3]
    );
    assert!(calls[4].contains("-X PATCH"), "call 5 patches the ref: {}", calls[4]);
    assert!(
        calls[4].contains("sha=newsha22"),
        "ref is patched to the new commit: {}",
        calls[4]
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn remedy_records_the_attempt_marker_after_the_push_lands() {
    let dir = tmp_dir("marker-recorded");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    let _ = run(&gh, ONE);

    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let calls = split_stub_calls(&argv);
    // The final call is the POST recording the marker — and it must come
    // AFTER the ref PATCH, so a marker can never outlive a push that failed.
    let last = calls.last().expect("at least one call");
    assert!(
        last.starts_with("repos/o/r/issues/42/comments"),
        "last call records the attempt: {last}"
    );
    assert!(
        last.contains(&redate_marker("newsha22")),
        "recorded comment carries the per-head marker: {last}"
    );
    let patch_at = calls
        .iter()
        .position(|c| c.contains("-X PATCH"))
        .expect("ref patch happened");
    assert!(patch_at < calls.len() - 1, "the marker is recorded LAST");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn remedy_refuses_to_push_when_the_branch_already_moved() {
    let dir = tmp_dir("head-moved");
    // Stub reports "def9999" as the live ref; caller expected "abc0000".
    let gh = write_stub_gh(&dir, "def9999", "unused", "unused", "", "");

    let outcome = run(&gh, ONE);
    assert_eq!(
        outcome,
        RemedyOutcome::HeadMoved {
            current: "def9999".to_string()
        }
    );

    // Only the ref read happened — no comment/tree/create/patch calls once the
    // head mismatch is detected.
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert_eq!(split_stub_calls(&argv).len(), 1, "must stop after the ref read: {argv}");

    let _ = fs::remove_dir_all(&dir);
}

// --- The bound: a second block on an already-re-dated head escalates ------

#[test]
fn remedy_escalates_instead_of_pushing_a_second_commit_for_the_same_head() {
    let dir = tmp_dir("escalate");
    let prior = redate_marker("abc0000");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", &prior, "");

    let outcome = run(&gh, ONE);
    assert_eq!(
        outcome,
        RemedyOutcome::Escalated {
            notice_posted: true,
            spent: 1,
            budget: 1
        }
    );

    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert!(
        !argv.contains("-X PATCH"),
        "no second no-op commit may be pushed once the bound is reached: {argv}"
    );
    assert!(
        argv.contains(&hold_marker("abc0000")),
        "the hold notice is posted, keyed on the blocked head: {argv}"
    );
    assert!(
        argv.contains("repos/o/r/issues/42/labels") && argv.contains(HOLD_LABEL),
        "the durable {HOLD_LABEL} hold is applied: {argv}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn escalation_does_not_repost_an_existing_notice_but_re_asserts_the_label() {
    let dir = tmp_dir("escalate-idempotent");
    let prior = format!("{}\n{}", redate_marker("abc0000"), hold_marker("abc0000"));
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", &prior, "");

    let outcome = run(&gh, ONE);
    assert_eq!(
        outcome,
        RemedyOutcome::Escalated {
            notice_posted: false,
            spent: 1,
            budget: 1
        }
    );

    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let calls = split_stub_calls(&argv);
    assert_eq!(
        calls.len(),
        3,
        "ref read + comment read + label apply only — no duplicate notice: {argv}"
    );
    assert!(
        calls[2].starts_with("repos/o/r/issues/42/labels"),
        "the label is still re-asserted so a hand-removal cannot strand the PR: {}",
        calls[2]
    );

    let _ = fs::remove_dir_all(&dir);
}

// --- Failure paths -------------------------------------------------------

#[test]
fn remedy_fails_when_the_ref_update_errors() {
    let dir = tmp_dir("patch-fails");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "patch");

    let outcome = run(&gh, ONE);
    match outcome {
        RemedyOutcome::Failed(msg) => {
            assert!(msg.contains("newsha22"), "failure should name the dangling commit: {msg}");
            assert!(
                msg.contains("dangling"),
                "failure should explain the commit is orphaned: {msg}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn remedy_fails_when_the_commit_cannot_be_created() {
    let dir = tmp_dir("create-fails");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "create");

    let outcome = run(&gh, ONE);
    assert!(matches!(outcome, RemedyOutcome::Failed(_)), "expected Failed, got {outcome:?}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn remedy_fails_closed_when_the_attempt_state_cannot_be_read() {
    // Not knowing whether the remedy already ran is exactly the state that
    // must NOT produce a push: an unreadable thread would otherwise defeat the
    // bound and re-push every tick.
    let dir = tmp_dir("comments-unreadable");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "read-comments");

    let outcome = run(&gh, ONE);
    assert!(matches!(outcome, RemedyOutcome::Failed(_)), "expected Failed, got {outcome:?}");
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert!(
        !argv.contains("-X PATCH"),
        "nothing may be pushed when the bound cannot be evaluated: {argv}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_landed_push_whose_marker_cannot_be_recorded_reports_the_exposure() {
    let dir = tmp_dir("marker-unwritable");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "comment");

    match run(&gh, ONE) {
        RemedyOutcome::Failed(msg) => {
            assert!(
                msg.contains("landed") && msg.contains(&redate_marker("newsha22")),
                "the caller must be told the push landed but is unrecorded: {msg}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

/// #9548 (H13): a re-date marker for this head written by an outsider is not
/// attempt state, so it neither escalates the PR to a hold nor stops the one
/// re-date the remedy is allowed; an unparseable listing fails the remedy.
#[test]
fn an_untrusted_redate_marker_is_not_attempt_state() {
    let dir = tmp_dir("untrusted-marker");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    let outsider = serde_json::json!([{
        "user": {"login": "drive-by", "type": "User"},
        "author_association": "NONE",
        "body": redate_marker("abc0000"),
    }]);
    fs::write(dir.join("comments.json"), outsider.to_string()).expect("write listing");
    let outcome = run(&gh, ONE);
    assert_eq!(
        outcome,
        RemedyOutcome::Pushed {
            new_sha: "newsha22".to_string()
        }
    );

    fs::write(dir.join("comments.json"), "not json").expect("write listing");
    let outcome = run(&gh, ONE);
    assert!(matches!(outcome, RemedyOutcome::Failed(_)), "{outcome:?}");
    let _ = fs::remove_dir_all(&dir);
}

// --- Attribution trailers (#9746) -----------------------------------------

/// #9743's pair: the seed expectation for the narrowing follow-up (#9748).
fn clause4_verdict() -> Verdict {
    Verdict::StaleInputs {
        check: "Structural Checks (Role Prompt Prefix Ratchet)".to_string(),
        tested_base: "803f0c7d".to_string(),
        reason: StaleReason {
            clause: "the base move and this PR both touch this check's coupled inputs",
            base_path: Some("CLAUDE.md".to_string()),
            pr_path: Some("defaults/docs/eta.md".to_string()),
        },
    }
}

/// The message's trailer block as git itself parses it — the contract the
/// report depends on, checked by the real parser rather than by eye.
fn git_trailers(message: &str) -> Vec<String> {
    let mut child = std::process::Command::new("git")
        .args(["interpret-trailers", "--parse"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git is on PATH for the test suite");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(message.as_bytes())
        .expect("write message");
    let out = child.wait_with_output().expect("git interpret-trailers");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn a_clause_4_verdict_renders_all_four_trailers() {
    let a = Attribution::from_verdict(&clause4_verdict()).expect("stale verdict attributes");
    let msg = commit_message_with("9743", Some(&a));
    assert_eq!(
        git_trailers(&msg),
        vec![
            "Stale-Check: Structural Checks (Role Prompt Prefix Ratchet)",
            "Stale-Clause: the base move and this PR both touch this check's coupled inputs",
            "Coupled-Base-Path: CLAUDE.md",
            "Coupled-PR-Path: defaults/docs/eta.md",
        ],
        "{msg}"
    );
    // The subject is byte-identical to the unattributed one, and the generic
    // body is kept intact above the trailer paragraph.
    assert_eq!(msg.lines().next(), commit_message("9743").lines().next());
    assert!(is_redate_commit_subject(msg.lines().next().unwrap()));
    assert!(msg.starts_with(&commit_message("9743")), "{msg}");
}

#[test]
fn a_time_rule_verdict_names_the_check_and_no_paths() {
    let t = now();
    let v = Verdict::Stale {
        check: "Rust Tests".to_string(),
        started_at: t,
        base_tip: t,
    };
    let a = Attribution::from_verdict(&v).expect("time-rule verdict attributes");
    assert_eq!(
        git_trailers(&commit_message_with("1", Some(&a))),
        vec![
            "Stale-Check: Rust Tests".to_string(),
            format!("Stale-Clause: {TIME_RULE_CLAUSE}"),
            "Coupled-Base-Path: none".to_string(),
            "Coupled-PR-Path: none".to_string(),
        ]
    );
}

#[test]
fn fresh_and_unknown_verdicts_carry_no_attribution() {
    assert_eq!(Attribution::from_verdict(&Verdict::Fresh), None);
    assert_eq!(Attribution::from_verdict(&Verdict::Unknown("x".into())), None);
}

#[test]
fn a_crafted_path_cannot_inject_a_trailer_or_a_line() {
    let a = Attribution {
        check: "Lint\nStale-Check: forged".to_string(),
        clause: "c\r\n\r\nSigned-off-by: mallory".to_string(),
        base_path: Some("a.md\nCoupled-PR-Path: forged\u{0}\t".to_string()),
        pr_path: Some("\n\n".to_string()),
    };
    let msg = commit_message_with("7", Some(&a));
    let trailers = git_trailers(&msg);
    assert_eq!(trailers.len(), 4, "exactly the four trailers, nothing forged: {trailers:?}");
    assert_eq!(trailers[0], "Stale-Check: Lint Stale-Check: forged");
    assert_eq!(trailers[1], "Stale-Clause: c Signed-off-by: mallory");
    assert_eq!(trailers[2], "Coupled-Base-Path: a.md Coupled-PR-Path: forged");
    assert_eq!(trailers[3], "Coupled-PR-Path: none", "an all-whitespace value reads as none");
    // The trailer paragraph is exactly four lines: nothing broke out of one.
    assert_eq!(msg.rsplit("\n\n").next().unwrap().lines().count(), 4, "{msg}");
}

#[test]
fn sanitize_caps_length_and_keeps_ordinary_values() {
    assert_eq!(sanitize("defaults/docs/eta.md"), "defaults/docs/eta.md");
    assert_eq!(sanitize("  spaced   out  "), "spaced out");
    assert_eq!(sanitize(""), "none");
    assert_eq!(sanitize(&"x".repeat(5000)).chars().count(), 300);
}

#[test]
fn no_attribution_is_exactly_the_generic_body() {
    assert_eq!(commit_message_with("42", None), commit_message("42"));
}

#[test]
fn the_remedy_writes_the_trailers_into_the_created_commit() {
    let dir = tmp_dir("attributed-push");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    let a = Attribution::from_verdict(&clause4_verdict());
    let outcome =
        remedy_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42", ONE, now(), || a);
    assert_eq!(
        outcome,
        RemedyOutcome::Pushed {
            new_sha: "newsha22".to_string()
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let create = split_stub_calls(&argv)[3].to_string();
    assert!(create.starts_with("repos/o/r/git/commits "), "{create}");
    assert!(create.contains("Coupled-Base-Path: CLAUDE.md"), "{create}");
    assert!(create.contains("Coupled-PR-Path: defaults/docs/eta.md"), "{create}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_unrecomputable_verdict_falls_back_to_the_generic_body_and_still_pushes() {
    let dir = tmp_dir("attribution-fallback");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    let gh = gh.to_str().unwrap();
    // The stub answers no `pulls/` path (exit 2), so the recompute fails at its
    // very first read — the remedy must not care.
    let recomputed = recompute_with(gh, "o/r", "42", "abc0000");
    assert!(recomputed.is_err(), "{recomputed:?}");
    let outcome = remedy_with(gh, "o/r", "feature/x", "abc0000", "42", ONE, now(), || {
        recompute_with(gh, "o/r", "42", "abc0000").ok()
    });
    assert_eq!(
        outcome,
        RemedyOutcome::Pushed {
            new_sha: "newsha22".to_string()
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let create = split_stub_calls(&argv)
        .into_iter()
        .find(|c| c.starts_with("repos/o/r/git/commits "))
        .expect("the commit was still created")
        .to_string();
    assert!(create.contains(&format!("message={}", commit_message("42"))), "{create}");
    assert!(!create.contains("Stale-Check:"), "{create}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn attribution_is_never_computed_when_nothing_is_pushed() {
    let dir = tmp_dir("attribution-lazy");
    // Head moved: no push, so the (possibly expensive) recompute must not run.
    let gh = write_stub_gh(&dir, "def9999", "unused", "unused", "", "");
    let outcome =
        remedy_with(gh.to_str().unwrap(), "o/r", "feature/x", "abc0000", "42", ONE, now(), || {
            panic!("attribution must not be computed for a head move")
        });
    assert!(matches!(outcome, RemedyOutcome::HeadMoved { .. }), "{outcome:?}");
    let _ = fs::remove_dir_all(&dir);
}
