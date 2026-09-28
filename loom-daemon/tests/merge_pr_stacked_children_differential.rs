//! Differential test: the Rust port of `merge-pr.sh`'s pre-merge merge-ordering
//! guard (#3747 item 2 / #7982), against the shell it replaced.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** [`CORPUS`] is a `&[(&str, &str)]` in this
//! file, written to disk one entry at a time, and the shell reads the same file —
//! so the harness cannot lie about which side moved.
//!
//! The shell side runs the real retired predicates and message templates, sourced
//! from `tests/fixtures/merge-pr-stacked-children-retired.sh`, a frozen
//! byte-for-byte copy of them as they stood immediately before the port.
//!
//! # What it proves, in two separate layers
//!
//! 1. **Discovery.** The branch-shape gate must agree everywhere, with no
//!    divergence table at all — it is an anchored regex whose whole job is
//!    rejecting `release-1`, and a port that widened it would silently start
//!    pinning refs for release branches. The child count and the two rendered
//!    lists agree everywhere except [`KNOWN_DIVERGENCES`].
//!
//! 2. **Messages.** All four operator-facing messages are compared on a
//!    **shared input** — the port's own `count`/`child_list`/`cmds`, handed to
//!    both sides — so a discovery divergence cannot leak into the message
//!    comparison and be mistaken for a template change. They must agree
//!    everywhere, with no divergence table: this slice is licensed to change
//!    where the logic lives, not what the operator reads. Two of those messages
//!    are the only place an operator learns that a stacked child needs
//!    reconciling and exactly how, and one of them is a refusal.
//!
//! # Why the direction of the one divergence matters here
//!
//! The retired guard turned a row with no usable `number` into the literal
//! string `null`: `#null` in the child list, and `./.loom/scripts/reconcile-stack.sh
//! null feature/issue-100` as the operator's paste-ready command. That command
//! does nothing, so the child was already lost — the retired shell just pinned a
//! ref and printed unusable advice on the way. The port drops the row, which
//! loses the same child with honest output, and (when it is the only row) also
//! skips the pin. `gh pr list --json number` cannot produce that shape, so the
//! trigger is a response the guard cannot read — the same class as the retired
//! `|| echo '[]'` forge-read failure, which already skipped.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::stacked_children as sc;

/// `(parent_branch, gh pr list --json number,headRefName output)`.
///
/// Drawn from the shapes this guard actually meets — the everyday zero/one/many
/// child cases, the branch names the anchored regex exists to reject, and the
/// malformed-response shapes the retired `2>/dev/null || echo 0` swallowed. Not
/// random text, which would exercise "no children" on both sides and prove
/// nothing.
const CORPUS: &[(&str, &str)] = &[
    // --- 0-3: the everyday cases on a stackable parent ---
    ("feature/issue-100", "[]"),
    ("feature/issue-100", r#"[{"number":501,"headRefName":"feature/issue-201"}]"#),
    (
        "feature/issue-100",
        r#"[{"number":501,"headRefName":"feature/issue-201"},{"number":502,"headRefName":"feature/issue-202"}]"#,
    ),
    (
        "feature/issue-7",
        r#"[{"number":8,"headRefName":"feature/issue-9"},{"number":10,"headRefName":"feature/issue-11"},{"number":12,"headRefName":"feature/issue-13"}]"#,
    ),
    // --- 4-9: branch names the anchored regex must reject ---
    ("release-1", r#"[{"number":503,"headRefName":"feature/issue-201"}]"#),
    ("fix-bug-42", r#"[{"number":503,"headRefName":"feature/issue-201"}]"#),
    ("feature/issue-100-extra", r#"[{"number":503,"headRefName":"x"}]"#),
    ("feature/issue-", r#"[{"number":503,"headRefName":"x"}]"#),
    ("feature/issue-abc", r#"[{"number":503,"headRefName":"x"}]"#),
    ("main", r#"[{"number":503,"headRefName":"x"}]"#),
    // --- 10-14: malformed / degraded forge responses ---
    ("feature/issue-100", ""),
    ("feature/issue-100", "not json"),
    ("feature/issue-100", "{}"),
    ("feature/issue-100", "null"),
    ("feature/issue-100", r#"{"a":1,"b":2}"#),
    // --- 15-17: rows the schema says cannot happen ---
    ("feature/issue-100", r#"[{"headRefName":"feature/issue-9"}]"#),
    ("feature/issue-100", r#"[{"number":501,"headRefName":"a"},{"headRefName":"b"}]"#),
    ("feature/issue-100", r#"[{"number":"501"}]"#),
    // --- 18-20: shapes that stress the rendered strings ---
    ("feature/issue-100", r#"[{"number":0}]"#),
    ("feature/issue-100", r#"[{"number":123456789}]"#),
    (
        "feature/issue-100",
        r#"[{"number":501,"headRefName":"feature/issue with spaces"}]"#,
    ),
];

/// Corpus indices where the port deliberately reads a different child set than
/// the retired shell, keyed by mechanism.
///
/// Asserted to STILL differ, so a later "cleanup" that silently reconverges (or
/// widens the gap) fails here rather than in production.
const KNOWN_DIVERGENCES: &[(usize, &str)] = &[
    (
        14,
        "an OBJECT response: `jq 'length'` counts its 2 KEYS as 2 children, then `.[].number` errors and \
         `|| echo ''` blanks the list — the retired guard pinned and warned about \"2 open stacked child PR(s) ()\". \
         The port reads a non-array as no children.",
    ),
    (
        15,
        "a row with no `number`: the retired shell counted it and rendered it as the literal `null` \
         (`#null`, `reconcile-stack.sh null <branch>`). The port drops it, so this response yields no children at all.",
    ),
    (
        16,
        "same mechanism as 15, mixed with a usable row: the retired child list was `#501, #null`; the port's is `#501`.",
    ),
];

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("merge-pr-stacked-children-retired.sh")
}

/// Run one frozen shell function, feeding the corpus entry from `input_file`
/// (never a re-generated string) on stdin.
///
/// `set -euo pipefail` and `LC_ALL=C`, matching `merge-pr.sh`'s own shell
/// options, so a pipeline failure in the fixture surfaces as a failure here
/// rather than as silently empty output.
fn shell(func: &str, args: &[&str], input_file: &Path) -> String {
    let fixture = fixture_path();
    let mut script = String::from("set -euo pipefail\n");
    script.push_str(&format!(". {}\n", shell_quote(&fixture)));
    script.push_str(func);
    for a in args {
        script.push(' ');
        script.push_str(&shell_quote_str(a));
    }
    script.push_str(&format!(" < {}\n", shell_quote(input_file)));
    run_bash(&script)
}

/// Run one frozen message template. Messages take positional arguments only —
/// no stdin — so they are compared without any parsing in the way.
fn shell_msg(func: &str, args: &[&str]) -> String {
    let fixture = fixture_path();
    let mut script = String::from("set -euo pipefail\n");
    script.push_str(&format!(". {}\n", shell_quote(&fixture)));
    script.push_str(func);
    for a in args {
        script.push(' ');
        script.push_str(&shell_quote_str(a));
    }
    script.push('\n');
    run_bash(&script)
}

fn run_bash(script: &str) -> String {
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .env("LC_ALL", "C")
        .output()
        .expect("bash");
    assert!(
        out.status.success(),
        "fixture shell failed ({}):\n{script}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // The fixture's helpers all print exactly one trailing newline; strip only
    // that, so an intentionally empty line inside a multi-line message survives.
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.strip_suffix('\n').map_or(s.clone(), str::to_string)
}

fn shell_quote(p: &Path) -> String {
    shell_quote_str(&p.to_string_lossy())
}

fn shell_quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Write one corpus entry to its own file, ONCE, and hand the same path to both
/// sides.
fn write_entry(dir: &Path, index: usize, body: &str) -> PathBuf {
    let path = dir.join(format!("children-{index}.json"));
    let mut f = std::fs::File::create(&path).expect("create corpus entry");
    f.write_all(body.as_bytes()).expect("write corpus entry");
    path
}

/// The anchored branch-shape gate must agree on every entry, with no divergence
/// table: a port that widened `^feature/issue-([0-9]+)$` would start pinning
/// `refs/loom/parent/release-1` and querying the forge for children of branches
/// that cannot have any.
#[test]
fn the_branch_shape_gate_agrees_everywhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, (branch, body)) in CORPUS.iter().enumerate() {
        let path = write_entry(dir.path(), i, body);
        let shell_says = shell("_retired_is_stackable_parent", &[branch], &path);
        let rust_says = sc::is_stackable_parent_branch(branch).to_string();
        assert_eq!(shell_says, rust_says, "entry {i} ({branch:?}): branch-shape gate disagrees");
    }
}

/// Whether the guard FIRES at all — the decision `count` feeds, not the string
/// `count` holds. Agreement everywhere except the recorded divergences.
#[test]
fn the_fire_or_skip_decision_agrees_except_where_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, (branch, body)) in CORPUS.iter().enumerate() {
        if !sc::is_stackable_parent_branch(branch) {
            continue; // the gate returns before any count is taken
        }
        let path = write_entry(dir.path(), i, body);
        let shell_says = shell("_retired_has_open_children", &[], &path);
        let rust_says = (!sc::parse_children(body).is_empty()).to_string();
        if shell_says != rust_says {
            assert!(
                divergence(i).is_some(),
                "entry {i} ({body:?}): UNRECORDED fire/skip divergence — shell {shell_says}, rust {rust_says}"
            );
        }
    }
}

/// The count both sides interpolate into the messages, compared only where both
/// sides agree the guard fires (elsewhere the number is never rendered).
#[test]
fn the_rendered_child_count_agrees_except_where_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, (branch, body)) in CORPUS.iter().enumerate() {
        if !sc::is_stackable_parent_branch(branch) {
            continue;
        }
        let children = sc::parse_children(body);
        let path = write_entry(dir.path(), i, body);
        if children.is_empty() || shell("_retired_has_open_children", &[], &path) != "true" {
            continue;
        }
        let shell_says = shell("_retired_count", &[], &path);
        let rust_says = children.len().to_string();
        if shell_says != rust_says {
            assert!(
                divergence(i).is_some(),
                "entry {i} ({body:?}): UNRECORDED count divergence — shell {shell_says}, rust {rust_says}"
            );
        }
    }
}

/// Every recorded divergence must STILL differ on at least one of the compared
/// surfaces. A silent reconvergence means the table is licensing a disagreement
/// that no longer exists, which is how a real future divergence gets waved
/// through.
#[test]
fn every_recorded_divergence_still_differs_somewhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, why) in KNOWN_DIVERGENCES {
        let (_branch, body) = CORPUS[*i];
        let path = write_entry(dir.path(), *i, body);
        let children = sc::parse_children(body);
        let fire_differs =
            shell("_retired_has_open_children", &[], &path) != (!children.is_empty()).to_string();
        let list_differs = shell("_retired_child_list", &[], &path) != sc::child_list(&children);
        assert!(
            fire_differs || list_differs,
            "entry {i} is recorded as a divergence ({why}) but the two sides now AGREE — \
             if the reconvergence is intended, delete the entry from KNOWN_DIVERGENCES"
        );
    }
}

/// `#501, #502` — what the operator reads as "these are what is blocking".
#[test]
fn the_child_list_agrees_except_where_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, (branch, body)) in CORPUS.iter().enumerate() {
        if !sc::is_stackable_parent_branch(branch) {
            continue;
        }
        let path = write_entry(dir.path(), i, body);
        let shell_says = shell("_retired_child_list", &[], &path);
        let rust_says = sc::child_list(&sc::parse_children(body));
        if shell_says != rust_says {
            assert!(
                divergence(i).is_some(),
                "entry {i} ({body:?}): UNRECORDED child-list divergence — shell {shell_says:?}, rust {rust_says:?}"
            );
        }
    }
}

/// The paste-ready unblock commands. The retired `jq` built them with string
/// concatenation and an `--arg`; the port builds them with `format!`. A drift in
/// the indentation, the script path, or the argument order hands the operator a
/// command that does not run.
#[test]
fn the_reconcile_commands_agree_except_where_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, (branch, body)) in CORPUS.iter().enumerate() {
        if !sc::is_stackable_parent_branch(branch) {
            continue;
        }
        let children = sc::parse_children(body);
        if children.is_empty() {
            continue; // the guard skips before rendering commands
        }
        let path = write_entry(dir.path(), i, body);
        let shell_says = shell("_retired_cmds", &[branch], &path);
        let rust_says = sc::reconcile_commands(&children, branch);
        if shell_says != rust_says {
            assert!(
                divergence(i).is_some(),
                "entry {i} ({body:?}): UNRECORDED command divergence —\nshell: {shell_says:?}\nrust:  {rust_says:?}"
            );
        }
    }
}

/// All four messages, on a SHARED input: the port's own count / list / commands
/// are handed to the frozen templates, so what is compared is the template and
/// nothing else. No divergence table — every entry must agree byte for byte.
#[test]
fn every_operator_facing_message_agrees_byte_for_byte() {
    let pr = "999";
    let sha = "deadbeef1234";
    for (i, (branch, body)) in CORPUS.iter().enumerate() {
        if !sc::is_stackable_parent_branch(branch) {
            continue;
        }
        let children = sc::parse_children(body);
        if children.is_empty() {
            continue;
        }
        let count = children.len().to_string();
        let list = sc::child_list(&children);
        let cmds = sc::reconcile_commands(&children, branch);
        let pin = sc::pin_ref(branch);
        let mk = |allow: bool, dry: bool| sc::Inputs {
            pr_number: pr,
            branch,
            head_sha: sha,
            children: &children,
            allow_stacked_children: allow,
            dry_run: dry,
        };
        let inputs = mk(false, false);

        // Bypass.
        let sc::Outcome::Bypass(rust_bypass) = sc::decide(&mk(true, false)) else {
            panic!("entry {i}: expected a bypass outcome");
        };
        assert_eq!(
            shell_msg("_retired_bypass_msg", &[&count, &list, branch]),
            rust_bypass,
            "entry {i}: bypass warning drifted"
        );

        // Dry run.
        let sc::Outcome::DryRun(rust_dry) = sc::decide(&mk(false, true)) else {
            panic!("entry {i}: expected a dry-run outcome");
        };
        assert_eq!(
            shell_msg("_retired_dry_run_msg", &[&count, &list, branch, &pin]),
            rust_dry,
            "entry {i}: dry-run report drifted"
        );

        // Pin written.
        assert_eq!(
            shell_msg("_retired_pinned_msg", &[pr, branch, &count, &list, &pin, sha, &cmds]),
            sc::pinned_message(&inputs),
            "entry {i}: pin-succeeded warning drifted"
        );

        // Hard block.
        assert_eq!(
            shell_msg("_retired_blocked_msg", &[pr, branch, &count, &list, &pin, sha, &cmds]),
            sc::blocked_message(&inputs),
            "entry {i}: refusal drifted"
        );
    }
}

fn divergence(index: usize) -> Option<&'static str> {
    KNOWN_DIVERGENCES
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, why)| *why)
}

/// Every recorded divergence must name a real corpus entry — a stale index would
/// silently license an unrelated disagreement.
#[test]
fn every_recorded_divergence_names_a_real_corpus_entry() {
    for (i, why) in KNOWN_DIVERGENCES {
        assert!(*i < CORPUS.len(), "divergence index {i} is out of range");
        assert!(why.len() > 40, "divergence {i} must be recorded by mechanism, not by label");
    }
}
