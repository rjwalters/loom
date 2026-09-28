//! Differential test: the Rust port of `merge-pr.sh`'s pre-merge
//! partial-increment close-conflict decision
//! (`_check_partial_increment_close_conflict`, #4569/#4595), against the shell
//! it replaced.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** Each corpus entry is written to disk and
//! served, byte for byte, as the stubbed forge reads the retired function
//! makes (the PR body, the commit messages, `forge_pr_close_targets`, and each
//! issue's `gh api` response and exit code); the Rust side is fed exactly the
//! values the live wrapper would capture from those same responses.
//!
//! The shell side runs the retired function WHOLE, sourced from
//! `tests/fixtures/merge-pr-partial-conflict-retired.sh` — a frozen verbatim
//! copy — on top of `merge-pr-refs-retired.sh`, the frozen pure-shell ref
//! extractors it called. The transcript is every `warning` line it emitted,
//! then the two sets it left behind.
//!
//! # What it proves
//!
//! That the union (`grep -E '^[0-9]+$' | sort -un`), the per-issue `jq`
//! reads, the `grep -qx` membership test and the three-way warning
//! attribution agree with the retired shell on every input here, including a
//! failed `gh api` (its error body plus the `|| echo '{}'` fallback) and both
//! dry-run phrasings. There are no known divergences: the port changes no
//! decision and no log text.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::partial_conflict::{plan, Frame, Step};

struct Case {
    body: &'static str,
    commits: &'static str,
    graphql: &'static str,
    /// `(issue, gh api response body, gh exit code)`.
    issues: &'static [(&'static str, &'static str, i32)],
}

const OPEN: &str = r#"{"state":"open","labels":[{"name":"loom:building"}]}"#;
const CLOSED: &str = r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#;
const PR: &str = r#"{"state":"open","pull_request":{"url":"x"}}"#;
const NOT_FOUND: &str = r#"{"message":"Not Found","status":"404"}"#;

const CORPUS: &[Case] = &[
    // --- 0-4: no declaration, or no body -> the shell returns early ---
    Case { body: "", commits: "", graphql: "", issues: &[] },
    Case { body: "Closes #123", commits: "close #123", graphql: "123", issues: &[] },
    Case { body: "`Part of #123`", commits: "", graphql: "", issues: &[("123", OPEN, 0)] },
    Case { body: "```\nPart of #123\n```\nclose #123", commits: "", graphql: "", issues: &[] },
    Case { body: "See Part of #123 in prose", commits: "", graphql: "", issues: &[] },
    // --- 5-9: the retained suite's shapes ---
    Case {
        body: "## Operator follow-up (after merge)\n\n1. npm publish\n2. Verify, then close #123.\n\nContributes to #123\n",
        commits: "",
        graphql: "",
        issues: &[("123", OPEN, 0)],
    },
    Case { body: "Implements a slice.\n\nContributes to #123", commits: "", graphql: "", issues: &[("123", OPEN, 0)] },
    Case { body: "Closes #888\n\nPart of #123", commits: "", graphql: "", issues: &[("123", OPEN, 0)] },
    Case { body: "Part of #777\n\nclose #777", commits: "", graphql: "", issues: &[("777", CLOSED, 0)] },
    Case { body: "Contributes to #123", commits: "", graphql: "123", issues: &[("123", OPEN, 0)] },
    // --- 10-14: attribution ladder and the commit signal ---
    Case { body: "Part of #321\n\nclose #321", commits: "", graphql: "", issues: &[("321", PR, 0)] },
    Case {
        body: "Implements a slice.\n\nContributes to #123",
        commits: "feat: implement the slice\n\nclose #123",
        graphql: "",
        issues: &[("123", OPEN, 0)],
    },
    Case {
        body: "Part of #123",
        commits: "feat: part one\nfixup: resolves #123 while here\nchore: part three",
        graphql: "123",
        issues: &[("123", OPEN, 0)],
    },
    Case {
        body: "Part of #123\n\nfixes #123 and Fixes #123",
        commits: "close #123",
        graphql: "123",
        issues: &[("123", OPEN, 0)],
    },
    Case {
        body: "Part of #123",
        commits: "chore: follow-up will close issue #123 later",
        graphql: "",
        issues: &[("123", OPEN, 0)],
    },
    // --- 15-19: several declarations, list markers, the ordinal trap ---
    Case {
        body: "- Part of #456\n* Contributes to #123\n\nfixes #456",
        commits: "",
        graphql: "",
        issues: &[("123", OPEN, 0), ("456", OPEN, 0)],
    },
    Case {
        body: "3. Part of #789\n\nCloses #3",
        commits: "",
        graphql: "",
        issues: &[("789", OPEN, 0)],
    },
    Case {
        body: "Part of #123\nPart of #123\n> Part of #456\n\nclose #123",
        commits: "",
        graphql: "456\n123",
        issues: &[("123", OPEN, 0), ("456", CLOSED, 0)],
    },
    Case {
        body: "Part of #5\n\n`close #5`",
        commits: "",
        graphql: "",
        issues: &[("5", OPEN, 0)],
    },
    Case {
        body: "Part of #10\n\nDiscloses #10, prefixes #10",
        commits: "",
        graphql: "",
        issues: &[("10", OPEN, 0)],
    },
    // --- 20-24: failed and degenerate forge reads ---
    Case { body: "Part of #123\n\nclose #123", commits: "", graphql: "", issues: &[("123", NOT_FOUND, 1)] },
    Case { body: "Part of #123\n\nclose #123", commits: "", graphql: "", issues: &[("123", "", 1)] },
    Case { body: "Part of #123\n\nclose #123", commits: "", graphql: "", issues: &[("123", "not json", 0)] },
    Case {
        body: "Part of #123\n\nclose #123",
        commits: "",
        graphql: "",
        issues: &[("123", r#"{"state":"open","pull_request":null}"#, 0)],
    },
    Case {
        body: "Part of #123\n\nclose #123",
        commits: "",
        graphql: "",
        issues: &[("123", r#"{"state":"OPEN"}"#, 0)],
    },
    // --- 25-28: sidebar-line hygiene ---
    Case { body: "Part of #123", commits: "", graphql: "#123\n 123\n123x\n\n", issues: &[("123", OPEN, 0)] },
    Case { body: "Part of #7", commits: "", graphql: "70\n17\n", issues: &[("7", OPEN, 0)] },
    Case { body: "Part of #123", commits: "", graphql: "\n\n123\n\n", issues: &[("123", OPEN, 0)] },
    Case {
        body: "Part of #123\r\n\r\nclose #123\r\n",
        commits: "",
        graphql: "",
        issues: &[("123", OPEN, 0)],
    },
];

fn fixtures() -> (PathBuf, PathBuf) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    (
        dir.join("merge-pr-refs-retired.sh"),
        dir.join("merge-pr-partial-conflict-retired.sh"),
    )
}

/// Recording stubs + the frozen function, under the options merge-pr.sh runs
/// with — INCLUDING the `|| true` its only call site wraps the guard in, which
/// disables `errexit` for its whole body. `$1` = refs fixture, `$2` = guard
/// fixture, `$3` = case directory, `$4` = DRY_RUN.
const HARNESS: &str = r#"set -euo pipefail
REFS="$1"; GUARD="$2"; DIR="$3"; DRY_RUN="$4"
gh() { local n="${2##*/}"; cat "$DIR/issue-$n.json"; return "$(cat "$DIR/issue-$n.rc")"; }
warning() { printf 'WARNING\t%s\n' "$*"; }
_mp_refs() { cat >/dev/null; }
_pr_commit_messages() { cat "$DIR/commits"; }
forge_pr_close_targets() { cat "$DIR/graphql"; }
FORGE_TYPE=github; PR_NUMBER=999; REPO_NWO=owner/repo; GH=gh
PARTIAL_OPEN_BEFORE_MERGE=""; PARTIAL_CONFLICT_ISSUES=""
PR_JSON="$(jq -n --rawfile body "$DIR/body" '{body:$body}')"
source "$REFS"
source "$GUARD"
_check_partial_increment_close_conflict || true
printf 'OPEN-SET\t%s\nCONFLICT-SET\t%s\n' "$PARTIAL_OPEN_BEFORE_MERGE" "$PARTIAL_CONFLICT_ISSUES"
"#;

fn shell_transcript(dir: &Path, dry_run: bool) -> String {
    let (refs, guard) = fixtures();
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(HARNESS)
        .arg("bash")
        .arg(refs)
        .arg(guard)
        .arg(dir)
        .arg(if dry_run { "true" } else { "false" })
        .output()
        .expect("run the frozen retired function");
    assert!(
        out.status.success(),
        "the retired function must not fail under `set -euo pipefail`; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `$(...)`: trailing newlines gone.
fn captured(s: &str) -> String {
    s.trim_end_matches('\n').to_string()
}

/// What the live wrapper hands the daemon, and the port's answer rendered in
/// the shell transcript's shape.
fn rust_transcript(case: &Case, dry_run: bool) -> String {
    let body = captured(case.body);
    let (mut open, mut conflict, mut out) = (Vec::new(), Vec::new(), String::new());
    // The wrapper returns before calling the daemon on an empty body.
    if !body.is_empty() {
        let frame = Frame {
            body,
            commit_messages: captured(case.commits),
            graphql_close_refs: captured(case.graphql),
            issues: case
                .issues
                .iter()
                .map(|(n, json, rc)| {
                    let mut s = (*json).to_string();
                    if *rc != 0 {
                        s.push_str("{}\n");
                    }
                    ((*n).to_string(), captured(&s))
                })
                .collect(),
        };
        for step in plan(&frame, "999", dry_run) {
            match step {
                Step::Open(n) => open.push(n.to_string()),
                Step::Conflict(n) => conflict.push(n.to_string()),
                Step::Warning(m) => out.push_str(&format!("WARNING\t{m}\n")),
            }
        }
    }
    out.push_str(&format!("OPEN-SET\t{}\nCONFLICT-SET\t{}\n", open.join(" "), conflict.join(" ")));
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
    let (refs, guard) = fixtures();
    assert!(refs.is_file() && guard.is_file(), "the frozen fixtures must exist");
    assert!(has_jq(), "the retired function needs jq, which merge-pr.sh hard-requires too");

    let mut compared = 0usize;
    let mut saw = [false; 4]; // body-, commit-, sidebar-attributed conflict; open-only
    for (i, case) in CORPUS.iter().enumerate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        fs::write(dir.join("body"), case.body).expect("write body");
        fs::write(dir.join("commits"), case.commits).expect("write commits");
        fs::write(dir.join("graphql"), case.graphql).expect("write graphql");
        for (n, json, rc) in case.issues {
            fs::write(dir.join(format!("issue-{n}.json")), json).expect("write issue");
            fs::write(dir.join(format!("issue-{n}.rc")), rc.to_string()).expect("write rc");
        }
        for dry_run in [false, true] {
            let shell = shell_transcript(dir, dry_run);
            let rust = rust_transcript(case, dry_run);
            assert_eq!(
                rust, shell,
                "corpus[{i}] dry_run={dry_run}: the port disagrees with the retired shell. \
Body: {:?}",
                case.body
            );
            saw[0] |= shell.contains("its body ALSO carries");
            saw[1] |= shell.contains("a commit message of this PR");
            saw[2] |= shell.contains("Development-sidebar link");
            saw[3] |= !shell.contains("WARNING") && !shell.starts_with("OPEN-SET\t\n");
            compared += 1;
        }
    }
    assert_eq!(compared, CORPUS.len() * 2);
    // Size says nothing about reach: every rung of the ladder must have been
    // hit, or the comparison could be "empty == empty" throughout.
    assert_eq!(saw, [true; 4], "a rung of the ladder was never exercised");
}
