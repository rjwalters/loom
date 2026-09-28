//! Differential test: the Rust port of `merge-pr.sh`'s closed-issue
//! `loom:building` cleanup decision (`_strip_one_closed_issue_building_label`,
//! #6199), against the shell it replaced.
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
//! `tests/fixtures/merge-pr-closed-building-retired.sh` — a frozen verbatim
//! copy. Its forge mutation and logging are replaced by recording stubs, so
//! the transcript is `STRIP` when it removed the label and empty when it
//! declined.
//!
//! # What it proves
//!
//! That the three `jq` filters the port models (`has("pull_request")`,
//! `.state // ""`, `.labels[]?.name`) and the three-step ladder built on them
//! agree with the retired shell on every input here — including the shapes a
//! failed `gh api` really produces (an error body, the `|| echo '{}'` fallback
//! appended to it, a truncated read) and jq's per-document error recovery.
//!
//! Every skip in this function is SILENT, which is exactly why the comparison
//! cannot stop at the transcript: three different reasons all render as no
//! output, so "both sides said nothing" is nearly free. The harness therefore
//! also asserts a coverage floor over the port's own `Skip` reasons, so a port
//! that skipped for the wrong reason — a live issue mistaken for a PR, say —
//! still has to be caught by the corpus rather than hidden by it.
//!
//! There are no known divergences: the port changes no decision.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::closed_building::{plan, Plan, Skip};
use loom_daemon::merge_pr::partial_reset::IssueView;

/// `gh api repos/<nwo>/issues/<n>` response bodies.
const CORPUS: &[&str] = &[
    // --- 0-6: the shapes GitHub actually returns ---
    r#"{"number":123,"state":"closed","labels":[{"name":"loom:building"},{"name":"tier:goal-advancing"}]}"#,
    r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"open","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"closed","labels":[{"name":"loom:issue"}]}"#,
    r#"{"state":"closed","labels":[]}"#,
    r#"{"state":"closed","pull_request":{"url":"x"},"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"closed","state_reason":"completed","labels":[{"id":1,"name":"loom:building","color":"ededed"}]}"#,
    // --- 7-12: failed or degenerate reads ---
    "{}",
    "",
    r#"{"message":"Not Found","documentation_url":"https://docs.github.com","status":"404"}"#,
    "not json at all",
    r#"{"state":"closed","labels":[{"name":"loom:bui"#,
    "null",
    // --- 13-18: key presence and falsy values ---
    r#"{"pull_request":null,"state":"closed","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":null,"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":false,"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"CLOSED","labels":[{"name":"loom:building"}]}"#,
    r#"{"state":3,"labels":[{"name":"loom:building"}]}"#,
    r#"{"state":"closed","pull_request":false,"labels":[{"name":"loom:building"}]}"#,
    // --- 19-25: label shapes ---
    r#"{"state":"closed","labels":[{"name":"loom:building"},"x"]}"#,
    r#"{"state":"closed","labels":["x",{"name":"loom:building"}]}"#,
    r#"{"state":"closed","labels":[null,{"name":"loom:building"}]}"#,
    r#"{"state":"closed","labels":{"k":{"name":"loom:building"}}}"#,
    r#"{"state":"closed","labels":[{"name":"loom:building-x"},{"name":"Loom:Building"}]}"#,
    r#"{"state":"closed","labels":[{}]}"#,
    r#"{"state":"closed","labels":"loom:building"}"#,
    // --- 26-30: multi-document streams ---
    r#"{"state":"closed"}{"state":"closed","labels":[{"name":"loom:building"}]}"#,
    r#"[1]{"state":"closed","labels":[{"name":"loom:building"}]}"#,
    r#"[1]{"pull_request":1,"state":"closed"}"#,
    r#"{"state":"closed","labels":[{"name":"loom:building"}]} garbage {"state":"open"}"#,
    "\"s\"\n{\"state\":\"closed\",\"labels\":[{\"name\":\"loom:building\"}]}",
    // --- 31-32: whitespace framing ---
    "  \n{\"state\":\"closed\",\"labels\":[{\"name\":\"loom:building\"}]}\n\n",
    "\n\n",
];

/// Whether `gh api` exits non-zero, which makes the shell append `{}` to
/// whatever the response body was.
const GH_RCS: &[i32] = &[0, 1];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-closed-building-retired.sh")
}

/// Recording stubs + the frozen function, under the options merge-pr.sh runs
/// with — INCLUDING the `|| true` its call chain
/// (`_strip_closed_issue_building_labels || true`) wraps the pass in. That is
/// not cosmetic: bash disables `errexit` for the whole body of a function
/// called in an `||` list, so in production a `jq` that exits non-zero on an
/// unparseable body left `issue_state` empty and the function carried on to
/// its "not closed" skip. Called bare under `-e`, the same input aborts the
/// function instead — a behaviour production never had.
///
/// `$1` = response body file, `$2` = fixture, `$3` = gh exit code.
const HARNESS: &str = r#"set -euo pipefail
BODY="$1"; GH_RC="$3"
gh() { cat "$BODY"; return "$GH_RC"; }
success() { :; }
warning() { :; }
forge_gh_remove_label_rl_safe() { printf 'STRIP\n'; }
REPO_NWO="owner/repo"
source "$2"
_strip_one_closed_issue_building_label 123 || true
"#;

fn shell_transcript(body_path: &Path, gh_rc: i32) -> String {
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(HARNESS)
        .arg("bash")
        .arg(body_path)
        .arg(fixture())
        .arg(gh_rc.to_string())
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
    let mut reasons: Vec<Skip> = Vec::new();
    let mut stripped = 0usize;
    for (i, body) in CORPUS.iter().enumerate() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(body.as_bytes()).expect("write corpus entry");
        for &gh_rc in GH_RCS {
            let shell = shell_transcript(tmp.path(), gh_rc);
            let decision = plan(&IssueView::from_json(&captured_issue_json(body, gh_rc)));
            let rust = match decision {
                Plan::Strip => "STRIP\n",
                Plan::Skip(_) => "",
            };
            assert_eq!(
                rust, shell,
                "corpus[{i}] gh_rc={gh_rc}: the port disagrees with the retired shell \
(port decided {decision:?}). Input: {body:?}"
            );
            match decision {
                Plan::Strip => stripped += 1,
                Plan::Skip(why) => {
                    if !reasons.contains(&why) {
                        reasons.push(why);
                    }
                }
            }
            compared += 1;
        }
    }
    assert_eq!(compared, CORPUS.len() * GH_RCS.len());

    // Size says nothing about reach, and here it says even less than usual:
    // all three skips render as the SAME empty transcript, so without this
    // floor the comparison above could be "empty == empty" almost throughout.
    assert!(stripped > 0, "the corpus never produced a STRIP");
    for want in [Skip::IsPullRequest, Skip::NotClosed, Skip::NotBuilding] {
        assert!(
            reasons.contains(&want),
            "the corpus never produced the skip {want:?}; saw {reasons:?}"
        );
    }
}
