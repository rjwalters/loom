//! Differential test: the Rust port of `merge-pr.sh`'s post-merge stacked-child
//! reconcile decisions must agree with the retired shell on a corpus generated
//! from the grammar, not from memory (#8191 slice, #3747 item 1).
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6, four rules in particular:
//!
//! - **Generate the corpus ONCE and feed both sides the same bytes.** Each
//!   generator below enumerates deterministically — no PRNG, nothing to keep
//!   synchronised — and every case is handed to `bash` as the same `String` the
//!   Rust receives.
//! - **Generate the ALPHABET, not a sample.** The branch corpus walks every
//!   structural way a ref name can fail `^feature/issue-([0-9]+)$` (prefix,
//!   suffix, empty capture, non-ASCII digit, embedded newline), not a handful of
//!   plausible names.
//! - **Say WHICH implementation a copied pattern models.** The patterns live in
//!   `tests/fixtures/merge-pr-reconcile-retired.sh`, whose header states
//!   verbatim-vs-reconstructed line by line; this file transcribes nothing.
//! - **Pin `LC_ALL=C`.** `[0-9]` is a *range*, and `grep -x` compares under the
//!   locale's collation. The port is ASCII-only, which is what `C` means; an
//!   unpinned harness would compare against the developer's ambient locale.
//!
//! # What it proves
//!
//! Not "the Rust looks right" — [`loom_daemon::merge_pr::reconcile`]'s own unit
//! tests do that, and `defaults/scripts/tests/test-merge-pr-auto-reconcile.sh`
//! proves the live script still reaches `reconcile-stack.sh` and the comment
//! post. This proves the port did not change **which of two irreversible things
//! happens to a stacked child**: `reconcile` hands it to a
//! `git rebase --onto` + `push --force-with-lease`, `defer` deliberately does
//! not. Getting that backwards on a branch a Builder still has checked out is
//! the one wrong answer in this family that destroys uncommitted work, and it
//! turns on a single `grep -qx` over a label list.
//!
//! # The divergence classes, named up front
//!
//! 1. **NUL bytes.** The retired implementation took the label list and the
//!    branch name through shell *variables*, which cannot hold a NUL at all, so
//!    no differential evidence about NUL is obtainable here. The port's
//!    behaviour on NUL and on invalid UTF-8 is pinned by unit test
//!    (`reconcile::tests::a_non_utf8_label_list_is_still_searched`) against
//!    `grep`'s documented byte-orientation instead.
//! 2. **Deliberate, asserted divergences.** Two exist, both tightenings, and
//!    both are proved *as divergences* by [`the_two_deliberate_divergences_are_real`]
//!    rather than excluded silently: a non-array rollup, and an element whose
//!    `number`/`headRefName` is unusable. See that test for the argument.
//!
//! Note that a trailing `\r` is NOT a divergence — it is an agreement worth
//! testing on purpose, which [`a_trailing_cr_is_data_on_both_sides`] does. An
//! earlier draft of the port trimmed CR before comparing, on the theory that a
//! CRLF list "should still match"; `grep -x` disagrees, because CR is data. The
//! one `grep` that behaves the other way is `ugrep`, and that test's failure
//! message says so — if it ever fires, suspect the host's `grep` before
//! suspecting the port.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::reconcile::{child_route, defer_comment, plan, Plan, Route, Row};

/// The frozen copy of the retired decisions.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-reconcile-retired.sh")
}

/// Run one helper from the frozen fixture.
///
/// `None` means `bash` itself failed (missing, unreadable fixture), which every
/// caller counts, so a harness that silently stopped running cannot pass.
fn shell(func: &str, args: &[&str]) -> Option<String> {
    let prog = r#"
set -uo pipefail
source "$1"
func="$2"
shift 2
"$func" "$@"
"#;
    let mut cmd = Command::new("bash");
    cmd
        // `[0-9]` is a range and `grep -x` compares under the locale's
        // collation. The port is ASCII-only, which is what `C` means.
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(prog)
        .arg("bash")
        .arg(fixture())
        .arg(func);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Corpus 1: branch names, against the `^feature/issue-([0-9]+)$` predicate.
// ---------------------------------------------------------------------------

/// Every structural way a ref name can relate to the predicate.
///
/// Enumerated from the *regex's* parts — prefix, capture, anchors — so a port
/// that dropped an anchor, widened the digit class, or lost the empty-capture
/// rejection is caught by construction rather than by having thought of the
/// name.
fn branch_corpus() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();

    // Matching shapes, including the ones that must NOT be renormalised.
    for digits in ["1", "42", "8191", "007", "0", &"9".repeat(25)] {
        v.push(format!("feature/issue-{digits}"));
    }

    // Empty capture: `+` requires at least one digit.
    v.push("feature/issue-".to_string());

    // Non-digit capture bodies.
    for body in [
        "abc",
        "12a",
        "a12",
        "1-2",
        "1.2",
        "1 2",
        "+12",
        "-12",
        "1_2",
        "١٢",
        "１２",
        "12\u{00a0}",
    ] {
        v.push(format!("feature/issue-{body}"));
    }

    // Anchor probes: anything before the prefix, or after the digits, breaks it.
    for prefix in ["", " ", "x", "\n", "refs/heads/", "\t"] {
        for suffix in ["", " ", "x", "/y", "\n", "\r", "\r\n", "\t", "-"] {
            if prefix.is_empty() && suffix.is_empty() {
                continue; // already covered above
            }
            v.push(format!("{prefix}feature/issue-12{suffix}"));
        }
    }

    // Wholly unrelated names the parent gate met in production.
    for name in [
        "main",
        "master",
        "release-1",
        "chore/resync-installed",
        "feature/harness-ops-9",
        "dependabot/cargo/all-dependencies-80ab654de6",
        "feature/issue",
        "featureissue-12",
        "",
    ] {
        v.push(name.to_string());
    }

    v
}

#[test]
fn the_issue_number_derivation_agrees_with_the_retired_regex() {
    let cases = branch_corpus();
    let mut compared = 0usize;
    let mut matched = 0usize;
    for branch in &cases {
        let Some(want) = shell("retired_child_issue", &[branch]) else {
            continue;
        };
        let got = loom_daemon::merge_pr::reconcile::issue_from_branch(branch).unwrap_or_default();
        assert_eq!(
            want, got,
            "issue_from_branch({branch:?}): retired shell said {want:?}, port said {got:?}"
        );
        if !got.is_empty() {
            matched += 1;
        }
        compared += 1;
    }
    assert_eq!(
        compared,
        cases.len(),
        "every case must have been compared; a skipped case means bash or the fixture failed"
    );
    assert!(
        matched >= 6,
        "the corpus must still contain matching branches, else it proves only that everything fails: {matched}"
    );
}

/// The parent gate and the child derivation were the SAME regex written twice,
/// so the port's single definition must answer the gate identically too. This is
/// what makes deleting one of the two copies safe.
#[test]
fn the_parent_gate_is_the_same_predicate_as_the_child_derivation() {
    let cases = branch_corpus();
    let mut compared = 0usize;
    for branch in &cases {
        // `retired_is_stacked` communicates through its EXIT status, so drive it
        // through `retired_plan`, whose NOT-STACKED record is the gate's only
        // observable effect.
        let Some(want) = shell("retired_plan", &[branch, "[]"]) else {
            continue;
        };
        let want_stacked = !want.starts_with("NOT-STACKED");
        let got_stacked = plan(branch, b"[]") != Plan::NotStacked;
        assert_eq!(
            want_stacked, got_stacked,
            "parent gate for {branch:?}: retired shell said stacked={want_stacked}, port said {got_stacked}"
        );
        compared += 1;
    }
    assert_eq!(compared, cases.len());
}

// ---------------------------------------------------------------------------
// Corpus 2: label lists, against `grep -qx 'loom:building'`.
// ---------------------------------------------------------------------------

/// Every structural way a label list can relate to `grep -qx 'loom:building'`.
///
/// `-x` (whole line) and case-sensitivity are the two properties that decide
/// whether a *different* label authorises the force-push, so the corpus crosses
/// each near-miss with every position in a multi-line list.
fn label_corpus() -> Vec<String> {
    // Near-misses: each is one edit from the claim, targeting a specific thing
    // the retired flags contributed.
    let singles = [
        "loom:building",
        "loom:building-paused",
        "loom:buildings",
        "xloom:building",
        "loom:build",
        "loom:building ",
        " loom:building",
        "\tloom:building",
        "loom:building\t",
        "LOOM:BUILDING",
        "Loom:Building",
        "loom:BUILDING",
        "loom:bui*lding",
        "loom:bui.lding",
        "loom.building",
        "loom:buildinG",
        "",
    ];
    let others = ["loom:issue", "loom:curated", "bug", "loom:pr"];

    let mut v: Vec<String> = Vec::new();
    // Empty / whitespace-only lists: the shape a FAILED lookup produced, and
    // the one the retired `|| true` layers made indistinguishable from "no
    // claim".
    for s in ["", "\n", "\n\n", " ", "\t\n"] {
        v.push(s.to_string());
    }
    for s in &singles {
        // Alone, with and without the trailing newline `jq -r` emits.
        v.push((*s).to_string());
        v.push(format!("{s}\n"));
        // At each of three positions in a list, so a port that only inspected
        // the first or last line is caught.
        v.push(format!("{s}\n{}\n{}\n", others[0], others[1]));
        v.push(format!("{}\n{s}\n{}\n", others[0], others[1]));
        v.push(format!("{}\n{}\n{s}\n", others[0], others[1]));
        // …and beside the real claim, which must still defer.
        v.push(format!("{s}\nloom:building\n"));
    }
    v.push(others.join("\n"));
    v
}

#[test]
fn the_claim_gate_agrees_with_the_retired_grep_qx() {
    let cases = label_corpus();
    let mut compared = 0usize;
    let mut defers = 0usize;
    let mut reconciles = 0usize;
    for labels in &cases {
        // A non-empty child issue: the branch of the retired code that actually
        // consulted the labels.
        let Some(want) = shell("retired_route", &["202", labels]) else {
            continue;
        };
        let want = want.trim_end_matches('\n').to_string();
        let got = child_route(Some("202"), labels.as_bytes()).route;
        assert_eq!(
            want,
            got.token(),
            "child_route(Some(\"202\"), {labels:?}): retired shell said {want:?}, port said {:?}",
            got.token()
        );
        match got {
            Route::Defer => defers += 1,
            Route::Reconcile => reconciles += 1,
        }
        compared += 1;
    }
    assert_eq!(
        compared,
        cases.len(),
        "every case must have been compared; a skipped case means bash or the fixture failed"
    );
    // Floors, so a corpus that drifted into testing only one answer fails
    // instead of passing vacuously.
    assert!(defers >= 20, "corpus must exercise the defer route: {defers}");
    assert!(reconciles >= 20, "corpus must exercise the reconcile route: {reconciles}");
}

/// The short-circuit: an empty `$child_issue` never consulted the labels at all,
/// so even a list that plainly carries the claim reconciled.
#[test]
fn an_absent_child_issue_short_circuits_identically() {
    let mut compared = 0usize;
    for labels in &label_corpus() {
        let Some(want) = shell("retired_route", &["", labels]) else {
            continue;
        };
        let want = want.trim_end_matches('\n').to_string();
        let got = child_route(None, labels.as_bytes()).route;
        assert_eq!(want, got.token(), "child_route(None, {labels:?}): retired shell said {want:?}");
        assert_eq!(want, "reconcile", "the short-circuit is always reconcile");
        compared += 1;
    }
    assert!(compared > 100, "too few cases compared: {compared}");
}

/// A trailing `\r` is data on both sides.
///
/// Kept out of [`label_corpus`] and given its own test only so this failure
/// message can exist: the port originally trimmed CR before comparing, which
/// flips `loom:building\r` from `reconcile` to `defer`, and that trim is a very
/// natural-looking "fix" for someone to reapply. `grep -x` compares whole lines
/// and CR is an ordinary byte to GNU and BSD grep — which is what `merge-pr.sh`,
/// being a script, resolves from PATH.
#[test]
fn a_trailing_cr_is_data_on_both_sides() {
    for labels in [
        "loom:building\r\n",
        "loom:building\r",
        "loom:issue\r\nloom:building\r\nbug\r\n",
    ] {
        let want = shell("retired_route", &["202", labels])
            .expect("fixture must run")
            .trim_end_matches('\n')
            .to_string();
        let got = child_route(Some("202"), labels.as_bytes()).route;
        assert_eq!(
            want,
            got.token(),
            "CR handling for {labels:?}: retired shell said {want:?}, port said {:?}.\n\
             If the shell side said \"defer\", this host's `grep` is probably ugrep, which \
             strips CR as line-ending handling; GNU and BSD grep do not, and merge-pr.sh \
             resolves whichever `grep` is on PATH. Check `grep --version` before changing \
             the port — trimming CR in child_route would flip a real force-push decision.",
            got.token()
        );
        assert_eq!(want, "reconcile", "a CR-terminated line is not the claim");
    }
}

// ---------------------------------------------------------------------------
// Corpus 3: the children rollup.
// ---------------------------------------------------------------------------

/// Rollups that the two producers (`gh pr list --json number,headRefName` and
/// the pre-merge `STACKED_CHILDREN_JSON` snapshot) actually emit, plus the
/// degenerate shapes the retired `|| echo 0` / `|| true` layers absorbed.
fn rollup_corpus() -> Vec<String> {
    vec![
        "[]".to_string(),
        r#"[{"number":501,"headRefName":"feature/issue-201"}]"#.to_string(),
        r#"[{"number":501,"headRefName":"feature/issue-201"},{"number":502,"headRefName":"feature/issue-202"}]"#.to_string(),
        // Order must be the rollup's, not sorted — the retired `.[]` preserved it.
        r#"[{"number":9,"headRefName":"feature/issue-9"},{"number":3,"headRefName":"feature/issue-3"}]"#.to_string(),
        // A child on an ad-hoc branch: no issue, so no claim to race.
        r#"[{"number":503,"headRefName":"hotfix/xyz"}]"#.to_string(),
        r#"[{"number":503,"headRefName":"feature/harness-ops-9"}]"#.to_string(),
        // Extra keys are ignored by both sides.
        r#"[{"number":504,"headRefName":"feature/issue-204","title":"x","isDraft":false}]"#.to_string(),
        // A digit string where a number is expected — `"\(.number)"` rendered
        // both identically.
        r#"[{"number":"505","headRefName":"feature/issue-205"}]"#.to_string(),
        // Whitespace and formatting variation.
        "[\n  {\"number\": 506,\n   \"headRefName\": \"feature/issue-206\"}\n]".to_string(),
        // A large-but-plausible fan-out.
        format!(
            "[{}]",
            (1..=12)
                .map(|n| format!(r#"{{"number":{},"headRefName":"feature/issue-{}"}}"#, 600 + n, n))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ]
}

#[test]
fn the_rollup_plan_agrees_with_the_retired_jq_pipeline() {
    let cases = rollup_corpus();
    let mut compared = 0usize;
    let mut child_rows = 0usize;
    for rollup in &cases {
        let Some(want) = shell("retired_plan", &["feature/issue-100", rollup]) else {
            continue;
        };
        let got = render_plan(plan("feature/issue-100", rollup.as_bytes()));
        assert_eq!(
            want.trim_end_matches('\n'),
            got.trim_end_matches('\n'),
            "plan for rollup {rollup}"
        );
        child_rows += got.lines().filter(|l| l.starts_with("CHILD ")).count();
        compared += 1;
    }
    assert_eq!(compared, cases.len());
    assert!(
        child_rows >= 20,
        "the corpus must produce real child rows, not just empty plans: {child_rows}"
    );
}

/// Render a [`Plan`] in the frozen fixture's record shape, so the two sides are
/// compared as text rather than through a translation only one side knows.
fn render_plan(p: Plan) -> String {
    match p {
        Plan::NotStacked => "NOT-STACKED\n".to_string(),
        // The fixture has no UNREADABLE record — see
        // `the_two_deliberate_divergences_are_real`, which is why no corpus case
        // reaches this arm.
        Plan::Unreadable(d) => format!("UNREADABLE {d}\n"),
        Plan::Rows(rows) => {
            let mut s = format!("COUNT {}\n", rows.len());
            for r in rows {
                match r {
                    Row::Child { pr, branch, issue } => {
                        s.push_str(&format!(
                            "CHILD {pr}\t{branch}\t{}\n",
                            issue.unwrap_or_default()
                        ));
                    }
                    Row::Malformed { index, detail } => {
                        s.push_str(&format!("MALFORMED {index}\t{detail}\n"));
                    }
                }
            }
            s
        }
    }
}

/// The port tightens the rollup parse in exactly two places. Both are asserted
/// here **as divergences**, so neither can be mistaken for an accident and
/// neither is quietly kept out of the corpus above.
///
/// 1. **A non-array rollup.** `jq 'length'` on an object returns its KEY COUNT
///    and `jq -r '.[]'` iterates its VALUES, so a stray object could produce
///    rows nobody meant to act on — and each of those rows is a candidate for a
///    force-push. Unparseable text took the other road: `|| echo 0` made it
///    indistinguishable from "this parent has no open children". The port says
///    `UNREADABLE`, which the seam reports as a warning and a skip.
/// 2. **An unusable element.** `"\(.number)"` on a `null` rendered the four
///    characters `null`, which then went into `repos/<nwo>/issues/null`. The port
///    names the row `MALFORMED` and keeps the sibling rows, so one bad element
///    costs one child's cleanup rather than silently mis-addressing a lookup.
///
/// Both moves are in the same direction — refuse to act on a row whose meaning
/// is unknown — which is the direction a step that force-pushes should fail in.
#[test]
fn the_two_deliberate_divergences_are_real() {
    // (1) Non-array rollups.
    for rollup in [
        r#"{"number":1,"headRefName":"feature/issue-1"}"#,
        "null",
        "7",
        "",
        "gh: command not found",
    ] {
        let want = shell("retired_plan", &["feature/issue-100", rollup])
            .expect("fixture must run")
            .trim_end_matches('\n')
            .to_string();
        let got = plan("feature/issue-100", rollup.as_bytes());
        assert!(
            matches!(got, Plan::Unreadable(_)),
            "the port must refuse a non-array rollup {rollup:?}, got {got:?}"
        );
        assert_ne!(want, "UNREADABLE", "the retired shell had no such answer, by construction");
    }

    // (2) An unusable element, alongside two good ones.
    let rollup = r#"[{"number":501,"headRefName":"feature/issue-201"},
                     {"number":null,"headRefName":"feature/issue-202"},
                     {"number":503,"headRefName":"feature/issue-203"}]"#;
    let want = shell("retired_plan", &["feature/issue-100", rollup]).expect("fixture must run");
    assert!(
        want.contains("CHILD null\tfeature/issue-202\t202"),
        "the retired shell rendered a null number as the text `null`; it said: {want}"
    );
    let Plan::Rows(rows) = plan("feature/issue-100", rollup.as_bytes()) else {
        panic!("expected rows")
    };
    assert!(
        matches!(&rows[1], Row::Malformed { index: 1, .. }),
        "the port must name the unusable element rather than render it"
    );
    assert!(
        matches!(&rows[0], Row::Child { pr, .. } if pr == "501")
            && matches!(&rows[2], Row::Child { pr, .. } if pr == "503"),
        "the sibling rows must survive"
    );
}

// ---------------------------------------------------------------------------
// Corpus 4: the deferral comment, byte for byte.
// ---------------------------------------------------------------------------

/// The comment is the entire operator-visible output of the defer route and its
/// text is the only instruction a human gets for finishing the reconcile by
/// hand. Compared as raw bytes against the frozen heredoc, across every
/// interpolation slot, including values with shell-significant characters in
/// them — a `$`, a backtick or a `"` reaching the heredoc unescaped is the exact
/// class of bug a byte comparison catches and an "it contains the PR number"
/// assertion does not.
#[test]
fn the_deferral_comment_is_byte_identical_to_the_retired_heredoc() {
    let slots = [
        ("feature/issue-100", "202", "502", "2026-09-27T18:00:00Z"),
        ("feature/issue-1", "1", "2", "1970-01-01T00:00:00Z"),
        (
            "feature/issue-99999999999999999999",
            "99999999999999999999",
            "123456",
            "2026-12-31T23:59:59Z",
        ),
        // Shell-significant characters. A parent branch cannot really contain
        // them, but the retired code interpolated the value unvalidated and so
        // does the port; if the two ever disagree it must be here and not in
        // production.
        ("feature/$USER-`id`-\"x\"", "202", "502", "2026-09-27T18:00:00Z"),
        ("feature/issue-7", "0", "0", ""),
    ];
    for (parent, issue, pr, ts) in slots {
        let want = shell("retired_comment", &[parent, issue, pr, ts]).expect("fixture must run");
        let got = defer_comment(issue, pr, parent, ts);
        assert_eq!(
            want.as_bytes(),
            got.as_bytes(),
            "deferral comment for parent={parent:?} issue={issue:?} pr={pr:?} ts={ts:?}\n\
             --- retired ---\n{want}\n--- port ---\n{got}"
        );
    }
}
