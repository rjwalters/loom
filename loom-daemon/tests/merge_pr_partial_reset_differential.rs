//! Differential test: the Rust port of `merge-pr.sh`'s partial-increment
//! reset decision (`_reset_one_partial_issue`, #3667/#4569), against the shell
//! it replaced.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** Each corpus entry is written to disk and
//! served, byte for byte, as the stubbed `gh api` response the retired
//! function reads; the Rust side is fed exactly the `issue_json` the live
//! wrapper would capture from that same response.
//!
//! The shell side runs the retired function WHOLE, sourced from
//! `tests/fixtures/merge-pr-partial-reset-retired.sh` — a frozen verbatim
//! copy. Its forge mutations and logging are replaced by recording stubs, so
//! the transcript is the ordered sequence of log lines and REOPEN / SWAP
//! actions it actually performed.
//!
//! # What it proves
//!
//! That the three `jq` filters the port models (`has("pull_request")`,
//! `.state // ""`, `.labels[]?.name`) and the branch ladder built on them
//! agree with the retired shell on every input here — including the shapes a
//! failed `gh api` really produces (an error body, the `|| echo '{}'`
//! fallback appended to it, a truncated read) and jq's per-document error
//! recovery, which nobody would think to unit-test. There are no known
//! divergences: the port changes no decision and no log text.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::partial_reset::{plan, IssueView, PreMerge, Step};

/// `gh api repos/<nwo>/issues/<n>` response bodies.
const CORPUS: &[&str] = &[
    // --- 0-6: the shapes GitHub actually returns ---
    r#"{"number":123,"state":"open","labels":[{"name":"loom:building"},{"name":"loom:epic"}]}"#,
    r#"{"state":"open","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"open","labels":[{"name":"loom:issue"}]}"#,
    r#"{"state":"closed","labels":[]}"#,
    r#"{"state":"open","pull_request":{"url":"x"},"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"closed","state_reason":"completed","labels":[{"id":1,"name":"loom:building","color":"ededed"}]}"#,
    // --- 7-12: failed or degenerate reads ---
    "{}",
    "",
    r#"{"message":"Not Found","documentation_url":"https://docs.github.com","status":"404"}"#,
    "not json at all",
    r#"{"state":"open","labels":[{"name":"loom:bui"#,
    "null",
    // --- 13-17: key presence and falsy values ---
    r#"{"pull_request":null,"state":"open","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":null,"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":false,"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"OPEN","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":3,"labels":[{"name":"loom:building"}]}"#,
    // --- 18-24: label shapes ---
    r#"{"state":"open","labels":[{"name":"loom:building"},"x"]}"#,
    r#"{"state":"open","labels":["x",{"name":"loom:building"}]}"#,
    r#"{"state":"open","labels":[null,{"name":"loom:building"}]}"#,
    r#"{"state":"open","labels":{"k":{"name":"loom:building"}}}"#,
    r#"{"state":"open","labels":[{"name":"loom:building-x"},{"name":"Loom:Building"}]}"#,
    r#"{"state":"open","labels":[{}]}"#,
    r#"{"state":"open","labels":"loom:building"}"#,
    // --- 25-29: multi-document streams ---
    r#"{"state":"open"}{"state":"open","labels":[{"name":"loom:building"}]}"#,
    r#"[1]{"state":"open","labels":[{"name":"loom:building"}]}"#,
    r#"[1]{"pull_request":1,"state":"open"}"#,
    r#"{"state":"closed","labels":[{"name":"loom:building"}]} garbage {"state":"open"}"#,
    "\"s\"\n{\"state\":\"closed\",\"labels\":[{\"name\":\"loom:building\"}]}",
    // --- 30-31: whitespace framing ---
    "  \n{\"state\":\"open\",\"labels\":[{\"name\":\"loom:building\"}]}\n\n",
    "\n\n",
];

/// Every pre-merge fact combination, and whether `gh api` exits non-zero
/// (which makes the shell append `{}` to whatever the response body was).
const FLAGS: &[(bool, bool)] = &[(false, false), (true, false), (false, true), (true, true)];
const GH_RCS: &[i32] = &[0, 1];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-partial-reset-retired.sh")
}

/// Recording stubs + the frozen function, under the options merge-pr.sh runs
/// with — INCLUDING the `|| true` its only call site
/// (`_reset_partial_increment_labels || true`) wraps the pass in. That is not
/// cosmetic: bash disables `errexit` for the whole body of a function called
/// in an `||` list, so in production a `jq` that exits non-zero on an
/// unparseable body left `issue_state` empty and the function carried on to
/// its "not open" skip. Called bare under `-e`, the same input aborts the
/// function instead — a behaviour production never had. `$1` = response body file, `$2` = fixture, `$3` = gh exit code,
/// `$4`/`$5` = the two pre-merge facts as 0/1.
const HARNESS: &str = r#"set -euo pipefail
BODY="$1"; GH_RC="$3"; CONFLICTED="$4"; OPEN_BEFORE="$5"
gh() { cat "$BODY"; return "$GH_RC"; }
info() { printf 'INFO\t%s\n' "$*"; }
warning() { printf 'WARNING\t%s\n' "$*"; }
success() { :; }
forge_gh_reopen_issue_rl_safe() { printf 'REOPEN\n'; }
forge_gh_swap_label_rl_safe() { printf 'SWAP\n'; }
forge_gh_comment_rl_safe() { :; }
_post_premature_close_comment() { :; }
_partial_ref_is_conflicted() { [[ "$CONFLICTED" == 1 ]]; }
_partial_ref_was_open_before_merge() { [[ "$OPEN_BEFORE" == 1 ]]; }
REPO_NWO="owner/repo"; PR_NUMBER="999"
source "$2"
_reset_one_partial_issue 123 || true
"#;

fn shell_transcript(body_path: &Path, gh_rc: i32, pre: PreMerge) -> String {
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(HARNESS)
        .arg("bash")
        .arg(body_path)
        .arg(fixture())
        .arg(gh_rc.to_string())
        .arg(if pre.conflicted { "1" } else { "0" })
        .arg(if pre.open_before_merge { "1" } else { "0" })
        .output()
        .expect("run the frozen retired function");
    assert!(
        out.status.success(),
        "the retired function must not fail under `set -euo pipefail`; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What `issue_json="$(gh api … 2>/dev/null || echo '{}')"` holds, followed
/// by the newline the live wrapper's `printf '%s\n'` adds.
fn captured_issue_json(body: &str, gh_rc: i32) -> String {
    let mut s = body.to_string();
    if gh_rc != 0 {
        s.push_str("{}\n");
    }
    while s.ends_with('\n') {
        s.pop();
    }
    s.push('\n');
    s
}

/// The port's steps in the SHELL's transcript shape — one line per log call,
/// whatever its content. (The live wire protocol additionally splits a
/// multi-line message so every line carries its level; that is framing, and
/// `partial_reset::tests` pins it separately.)
fn rust_transcript(body: &str, gh_rc: i32, pre: PreMerge) -> String {
    let view = IssueView::from_json(&captured_issue_json(body, gh_rc));
    let mut out = String::new();
    for step in plan("123", "999", "owner/repo", &view, pre) {
        match step {
            Step::Info(m) => out.push_str(&format!("INFO\t{m}\n")),
            Step::Warning(m) => out.push_str(&format!("WARNING\t{m}\n")),
            Step::Reopen => out.push_str("REOPEN\n"),
            Step::Swap => out.push_str("SWAP\n"),
        }
    }
    out
}

fn has_jq() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn plan_agrees_with_the_retired_shell_on_every_input() {
    assert!(fixture().is_file(), "the frozen fixture must exist");
    assert!(has_jq(), "the retired function needs jq, which merge-pr.sh hard-requires too");

    let mut compared = 0usize;
    let mut outcomes: Vec<String> = Vec::new();
    for (i, body) in CORPUS.iter().enumerate() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(body.as_bytes()).expect("write corpus entry");
        for &(conflicted, open_before_merge) in FLAGS {
            let pre = PreMerge {
                conflicted,
                open_before_merge,
            };
            for &gh_rc in GH_RCS {
                let shell = shell_transcript(tmp.path(), gh_rc, pre);
                let rust = rust_transcript(body, gh_rc, pre);
                assert_eq!(
                    rust, shell,
                    "corpus[{i}] gh_rc={gh_rc} {pre:?}: the port disagrees with the retired \
shell. Input: {body:?}"
                );
                let shape: String = shell
                    .lines()
                    .map(|l| l.split('\t').next().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(",");
                if !outcomes.contains(&shape) {
                    outcomes.push(shape);
                }
                compared += 1;
            }
        }
    }
    assert_eq!(compared, CORPUS.len() * FLAGS.len() * GH_RCS.len());

    // Size says nothing about reach: every branch of the ladder must have been
    // hit, or the comparison above could be "empty == empty" 256 times.
    for want in [
        "",                         // a PR: silent
        "INFO",                     // not open / not building
        "WARNING",                  // open-before, unattributed close
        "INFO,SWAP",                // the normal reset
        "WARNING,REOPEN,INFO,SWAP", // #4569 revert then reset
        "WARNING,REOPEN,INFO",      // #4569 revert, nothing to swap
    ] {
        assert!(
            outcomes.iter().any(|o| o == want),
            "the corpus never produced the outcome {want:?}; saw {outcomes:?}"
        );
    }
}
