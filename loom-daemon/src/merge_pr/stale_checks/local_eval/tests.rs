//! Tests for the #10388 local merge-tree evaluation of cheap stale components.

use super::*;
use crate::merge_pr::stale_checks::inputs::{BaseMove, FileSet};
use crate::merge_pr::stale_checks::repo_specs::RepoSpecs;
use crate::merge_pr::stale_checks::workflow_scope::CiScope;
use std::process::Command;

// --- Which verdicts may be satisfied locally (pure) -------------------------

fn tip() -> DateTime<Utc> {
    "2026-10-05T07:00:00Z".parse().unwrap()
}

fn green(name: &str) -> CheckRun {
    CheckRun {
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some("success".to_string()),
        started_at: Some("2026-10-05T06:00:00Z".parse().unwrap()),
        actions_run_id: None,
        actions_job_id: None,
    }
}

fn fset(paths: &[&str]) -> FileSet {
    inputs::file_set(paths.iter().map(|p| (*p, false)))
}

fn mv(files: &[&str]) -> BaseMove {
    BaseMove {
        tested_base: "b0b0b0b".to_string(),
        files: fset(files),
        ci_scope: CiScope::Unscoped,
    }
}

fn evidence(pr: &[&str], moves: Vec<(&str, BaseMove)>) -> ScopedEvidence {
    ScopedEvidence {
        pr_delta: fset(pr),
        pr_ci_scope: CiScope::Unscoped,
        base_moves: moves.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        fallbacks: std::collections::BTreeMap::new(),
        repo_specs: RepoSpecs::Absent,
    }
}

const STRUCT: &str = "Structural Checks";
const DAEMON: &str = "Daemon Checks";

fn req(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

/// The 2026-10-04/05 shape: a fleet merge and this PR both touched the role
/// prompt surface (a shared doc). Only prompt/doc gates go stale.
fn prompt_surface_evidence() -> ScopedEvidence {
    evidence(&["defaults/docs/eta.md"], vec![(STRUCT, mv(&["defaults/docs/eta.md"]))])
}

#[test]
fn only_cheap_stale_components_are_locally_evaluable() {
    let ev = prompt_surface_evidence();
    let runs = vec![green(STRUCT)];
    // The guard itself refuses this (input-scoped), so the test is meaningful.
    let (v, _) = assess_scoped(tip(), &req(&[STRUCT]), &runs, Some(&ev));
    assert!(matches!(v, Verdict::StaleInputs { .. }), "{v:?}");

    let got = locally_evaluable(tip(), &req(&[STRUCT]), &runs, Some(&ev))
        .expect("every stale component is cheap");
    assert!(got.contains(&"Role Prompt Prefix Ratchet"), "{got:?}");
    assert!(got.contains(&"Conflict Marker Check"), "same file both sides: {got:?}");
    assert!(got.iter().all(|c| cheap(c).is_some()), "{got:?}");
}

#[test]
fn a_stale_expensive_component_in_the_same_context_still_blocks() {
    let ev = prompt_surface_evidence();
    let runs = vec![green(STRUCT)];
    let got = locally_evaluable_with(tip(), &req(&[STRUCT]), &runs, Some(&ev), |c| {
        c != "Role Prompt Prefix Ratchet"
    });
    assert_eq!(got, None, "one non-allowlisted stale component refuses the whole context");
}

#[test]
fn mixed_cheap_and_expensive_contexts_still_block() {
    // Structural Checks stale only on cheap components; Daemon Checks stale
    // because the base moved Cargo.lock (a global input of the Rust gates).
    let ev = evidence(
        &["defaults/docs/eta.md", "loom-daemon/src/secret_scan.rs"],
        vec![
            (STRUCT, mv(&["defaults/docs/eta.md"])),
            (DAEMON, mv(&["Cargo.lock"])),
        ],
    );
    let runs = vec![green(STRUCT), green(DAEMON)];
    let (v, _) = assess_scoped(tip(), &req(&[DAEMON]), &runs, Some(&ev));
    assert!(matches!(v, Verdict::StaleInputs { .. }), "fixture: Daemon Checks stale: {v:?}");
    assert_eq!(locally_evaluable(tip(), &req(&[STRUCT, DAEMON]), &runs, Some(&ev)), None);
    // ...and the cheap context alone would have qualified.
    assert!(locally_evaluable(tip(), &req(&[STRUCT]), &runs, Some(&ev)).is_some());
}

#[test]
fn time_rule_and_no_evidence_are_never_locally_evaluable() {
    let runs = vec![green(STRUCT)];
    assert_eq!(locally_evaluable(tip(), &req(&[STRUCT]), &runs, None), None);
    // Evidence gathered, but none for this context: the time rule decides.
    let ev = evidence(&["a.md"], vec![]);
    assert_eq!(locally_evaluable(tip(), &req(&[STRUCT]), &runs, Some(&ev)), None);
}

#[test]
fn nothing_stale_is_nothing_to_evaluate() {
    let ev = evidence(&["loom-daemon/src/x.rs"], vec![(STRUCT, mv(&["README-unrelated.txt"]))]);
    let runs = vec![green(STRUCT)];
    let (v, _) = assess_scoped(tip(), &req(&[STRUCT]), &runs, Some(&ev));
    assert_eq!(v, Verdict::Fresh);
    assert_eq!(locally_evaluable(tip(), &req(&[STRUCT]), &runs, Some(&ev)), None);
}

#[test]
fn a_pr_that_edits_check_code_is_never_evaluated_locally() {
    // Same stale shape as the qualifying case, but the PR also edits a script
    // (or ci.yml): running it locally would execute the PR's code on the
    // merging host, so the CI path keeps it.
    for code in [
        "scripts/check-role-prompt-budget.sh",
        ".github/workflows/ci.yml",
    ] {
        let ev = evidence(
            &["defaults/docs/eta.md", code],
            vec![(STRUCT, mv(&["defaults/docs/eta.md"]))],
        );
        let runs = vec![green(STRUCT)];
        assert_eq!(locally_evaluable(tip(), &req(&[STRUCT]), &runs, Some(&ev)), None, "{code}");
    }
    assert!(changes_check_code("a/b.sh") && !changes_check_code("a/b.md"));
}

#[test]
fn an_unmapped_context_is_not_vouched_for() {
    let ev = evidence(&["x"], vec![("Some Repo Check", mv(&["x"]))]);
    let runs = vec![green("Some Repo Check")];
    assert_eq!(locally_evaluable(tip(), &req(&["Some Repo Check"]), &runs, Some(&ev)), None);
}

// --- The allowlist --------------------------------------------------------

#[test]
fn expensive_components_are_not_on_the_allowlist() {
    for c in [
        "Shell Budget Ratchet",
        "Secret Scan",
        "MCP Guard Wiring Contract",
        ".gitignore Convergence Check",
        "Shell Syntax (ubuntu-latest)",
    ] {
        assert!(cheap(c).is_none(), "{c} needs a toolchain/CI; it must not be local");
    }
}

// --- Fixture repository: the evaluation itself -------------------------------

fn git_ok(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", r])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A ratchet in miniature: `items` may have at most `limit` lines. `ls-files`
/// proves the checkout is a real git repository, as CI's is.
const RATCHET: &str = "set -e\ngit ls-files --error-unmatch items limit >/dev/null\n\
test \"$(wc -l < items)\" -le \"$(cat limit)\"\n";

/// The fixture's ci.yml: the merge tree's own steps are what runs.
const FIXTURE_CI: &str = "jobs:
  structural-checks:
    name: Structural Checks
    steps:
      # component: Conflict Marker Check
      - name: Check the ratchet
        if: ${{ !cancelled() }}
        run: sh check.sh
      # component: Slow
      - name: Hang
        run: |
          sleep 30
";

/// Fixture allowlist entries, mimicking [`CHEAP_CHECKS`].
const FIXTURE: &[CheapCheck] = &[
    CheapCheck {
        component: "Conflict Marker Check",
        script: "check.sh",
        requires: &["sh", "git"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Slow",
        script: "check.sh",
        requires: &["sleep"],
        skip_steps: &[],
        pins: &[],
    },
];

struct Fixture {
    _t: tempfile::TempDir,
    work: std::path::PathBuf,
    head: String,
    base: String,
}

/// origin (bare) with `main` + `refs/pull/7/head` and a clone to run from.
/// The PR grows `items` to 3 lines (limit 3). `main` then moves with
/// `base_move` — e.g. tightening `limit` to 2, the #8248 incident in small.
fn fixture(base_move: &dyn Fn(&Path)) -> Fixture {
    fixture_pr(base_move, &|_: &Path| {})
}

/// [`fixture`] whose PR commit also applies `pr_extra`.
fn fixture_pr(base_move: &dyn Fn(&Path), pr_extra: &dyn Fn(&Path)) -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let origin = t.path().join("origin.git");
    let work = t.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git_ok(
        t.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git_ok(&work, &["init", "-q", "-b", "main"]);
    git_ok(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
    std::fs::write(work.join("check.sh"), RATCHET).unwrap();
    std::fs::create_dir_all(work.join(".github/workflows")).unwrap();
    std::fs::write(work.join(".github/workflows/ci.yml"), FIXTURE_CI).unwrap();
    std::fs::write(work.join("limit"), "3\n").unwrap();
    std::fs::write(work.join("items"), "a\nb\n").unwrap();
    git_ok(&work, &["add", "."]);
    git_ok(&work, &["commit", "-qm", "base"]);
    git_ok(&work, &["checkout", "-qb", "pr"]);
    std::fs::write(work.join("items"), "a\nb\nc\n").unwrap();
    pr_extra(&work);
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-qm", "pr"]);
    let head = rev(&work, "HEAD");
    git_ok(&work, &["checkout", "-q", "main"]);
    base_move(&work);
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-qm", "sibling", "--allow-empty"]);
    let base = rev(&work, "HEAD");
    git_ok(
        &work,
        &[
            "push",
            "-q",
            "origin",
            "main",
            &format!("{head}:refs/pull/7/head"),
        ],
    );
    Fixture {
        _t: t,
        work,
        head,
        base,
    }
}

fn unrelated(w: &Path) {
    std::fs::write(w.join("other.txt"), "x\n").unwrap();
}

fn tighten(w: &Path) {
    std::fs::write(w.join("limit"), "2\n").unwrap();
}

fn eval(f: &Fixture, expected_base: &str, checks: &[CheapCheck], secs: u64) -> Outcome {
    eval_components(f, expected_base, &["Conflict Marker Check"], checks, secs)
}

fn eval_components(
    f: &Fixture,
    expected_base: &str,
    components: &[&str],
    checks: &[CheapCheck],
    secs: u64,
) -> Outcome {
    evaluate(
        &f.work,
        "origin",
        "7",
        "main",
        &f.head,
        expected_base,
        components,
        checks,
        Duration::from_secs(secs),
    )
}

#[test]
fn a_cheap_check_passing_on_the_merge_tree_satisfies_the_guard() {
    let f = fixture(&unrelated);
    match eval(&f, &f.base, FIXTURE, 30) {
        Outcome::Passed(r) => {
            assert_eq!(r.base_sha, f.base);
            assert_eq!(r.head_sha, f.head);
            assert_eq!(r.tree_sha.len(), 40);
            let got: Vec<(&str, bool)> = r
                .results
                .iter()
                .map(|c| (c.component.as_str(), c.passed))
                .collect();
            assert_eq!(got, vec![("Conflict Marker Check", true)]);
            assert_eq!(r.results[0].steps.len(), 1);
            assert_eq!(r.results[0].steps[0].command, "sh check.sh");
            assert_eq!(r.results[0].steps[0].exit, Some(0));
            let line = log_line("7", "pass", &r);
            for needle in [
                f.base.as_str(),
                f.head.as_str(),
                r.tree_sha.as_str(),
                "=pass",
            ] {
                assert!(line.contains(needle), "{line}");
            }
            let c = comment_body(&r, None);
            let m = format!(
                "<!-- loom:merge-tree-reverify base={} head={} tree={} -->",
                f.base, f.head, r.tree_sha
            );
            assert!(c.starts_with(&m), "{c}");
            assert!(c.contains("`sh check.sh` | 0 |") && c.contains("| pass |"), "{c}");
        }
        o => panic!("{o:?}"),
    }
    // The primary checkout was never touched.
    assert_eq!(std::fs::read_to_string(f.work.join("items")).unwrap(), "a\nb\n");
}

#[test]
fn a_cheap_check_failing_on_the_merge_tree_keeps_the_merge_blocked() {
    // Each side passes alone (PR: 3 <= 3; main: 2 <= 2); merged, 3 > 2.
    let f = fixture(&tighten);
    match eval(&f, &f.base, FIXTURE, 30) {
        Outcome::Failed {
            record,
            component,
            step,
            ..
        } => {
            assert_eq!(component, "Conflict Marker Check");
            assert_eq!(step, "sh check.sh\n");
            assert_eq!(record.results.last().map(|r| r.passed), Some(false));
            let c = comment_body(&record, Some((&step, "boom")));
            assert!(c.contains("FAIL") && c.contains("boom"), "{c}");
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn a_base_that_moved_since_the_assessment_is_unknown() {
    let f = fixture(&unrelated);
    let o = eval(&f, "0000000000000000000000000000000000000000", FIXTURE, 30);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("base moved")), "{o:?}");
}

#[test]
fn a_missing_script_is_unknown_not_a_pass() {
    let f = fixture(&unrelated);
    let missing = [CheapCheck {
        script: "nope.sh",
        ..FIXTURE[0]
    }];
    let o = eval(&f, &f.base, &missing, 30);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("nope.sh")), "{o:?}");
}

#[test]
fn a_missing_tool_is_unknown_not_a_pass() {
    let f = fixture(&unrelated);
    let needs = [CheapCheck {
        requires: &["loom-no-such-tool-10388"],
        ..FIXTURE[0]
    }];
    let o = eval(&f, &f.base, &needs, 30);
    assert!(
        matches!(o, Outcome::Unknown(ref w) if w.contains("loom-no-such-tool-10388")),
        "{o:?}"
    );
}

#[test]
fn a_timeout_is_unknown_not_a_failure() {
    let f = fixture(&unrelated);
    let o = eval_components(&f, &f.base, &["Slow"], FIXTURE, 1);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("timed out")), "{o:?}");
}

#[test]
fn a_component_without_runnable_ci_steps_is_unknown() {
    let f = fixture(&unrelated);
    let ghost = [CheapCheck {
        component: "Not In CI",
        ..FIXTURE[0]
    }];
    let o = eval_components(&f, &f.base, &["Not In CI"], &ghost, 30);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("marker")), "{o:?}");
}

#[test]
fn a_merge_tree_conflict_is_unknown() {
    let f = fixture(&|w: &Path| std::fs::write(w.join("items"), "a\nb\nZ\n").unwrap());
    let o = eval(&f, &f.base, FIXTURE, 30);
    assert!(matches!(o, Outcome::Unknown(_)), "{o:?}");
}

#[test]
fn a_component_off_the_allowlist_is_unknown() {
    let f = fixture(&unrelated);
    let o = evaluate(
        &f.work,
        "origin",
        "7",
        "main",
        &f.head,
        &f.base,
        &["Secret Scan"],
        FIXTURE,
        Duration::from_secs(30),
    );
    assert!(matches!(o, Outcome::Unknown(_)), "{o:?}");
}

// --- Opt-in: env > config > default (off) ----------------------------------

#[test]
fn reverification_is_off_unless_the_repository_opts_in() {
    use serde_json::json;
    let none = json!({});
    let on = json!({"merge": {"reverifyStaleChecks": true}});
    let off = json!({"merge": {"reverifyStaleChecks": false}});
    // Default: off.
    assert!(!resolve_enabled(None, &none));
    assert!(!resolve_enabled(None, &json!({"merge": {"treeChecks": ["x"]}})));
    // Config opts in or out.
    assert!(resolve_enabled(None, &on));
    assert!(!resolve_enabled(None, &off));
    assert!(resolve_enabled(None, &json!({"merge": {"reverifyStaleChecks": "true"}})));
    // Env beats config, both ways.
    for v in ["1", "true", " on ", "YES"] {
        assert!(resolve_enabled(Some(v), &off), "{v}");
        assert!(resolve_enabled(Some(v), &none), "{v}");
    }
    for v in ["0", "false", "off", "no"] {
        assert!(!resolve_enabled(Some(v), &on), "{v}");
    }
    // An unparseable env value falls through to config, then the default.
    assert!(resolve_enabled(Some("maybe"), &on));
    assert!(!resolve_enabled(Some(""), &none));
    assert_eq!(CONFIG_KEY, "merge.reverifyStaleChecks");
    assert_eq!(ENABLE_ENV, "LOOM_MERGE_REVERIFY_STALE_CHECKS");
}

#[test]
fn enabled_for_root_reads_the_repository_config() {
    if std::env::var_os(ENABLE_ENV).is_some() {
        return; // the env tier would decide; covered by resolve_enabled above
    }
    let t = tempfile::tempdir().unwrap();
    assert!(!enabled_for_root(t.path()), "no config: off");
    std::fs::create_dir_all(t.path().join(".loom")).unwrap();
    std::fs::write(
        t.path().join(".loom/config.json"),
        r#"{"merge": {"reverifyStaleChecks": true}}"#,
    )
    .unwrap();
    assert!(enabled_for_root(t.path()));
}

// --- Trust boundary: the merge tree, not the forge's file list --------------

#[test]
fn a_pr_script_edit_missing_from_the_api_delta_is_still_never_run() {
    // The forge's file list caps at 3000 entries without saying so: model a
    // PR whose API delta shows docs only while it actually rewrites the
    // checker. evaluate() is never given the delta; it diffs the trees.
    let t = tempfile::tempdir().unwrap();
    let pwned = t.path().join("pwned");
    let evil = format!("touch '{}'\n", pwned.display());
    let f = fixture_pr(&unrelated, &|w: &Path| std::fs::write(w.join("check.sh"), &evil).unwrap());
    let ev = evidence(&["defaults/docs/eta.md"], vec![(STRUCT, mv(&["defaults/docs/eta.md"]))]);
    assert!(
        locally_evaluable(tip(), &req(&[STRUCT]), &[green(STRUCT)], Some(&ev)).is_some(),
        "fixture: the truncated API delta passes the fast-path screen"
    );
    let o = eval(&f, &f.base, FIXTURE, 30);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("check.sh")), "{o:?}");
    assert!(!pwned.exists(), "the PR's checker must never execute");

    // ci.yml is check code too.
    let f = fixture_pr(&unrelated, &|w: &Path| {
        let ci = w.join(".github/workflows/ci.yml");
        let text = std::fs::read_to_string(&ci).unwrap() + "# edited by the PR\n";
        std::fs::write(ci, text).unwrap();
    });
    let o = eval(&f, &f.base, FIXTURE, 30);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("ci.yml")), "{o:?}");

    // A base-side-only script change is the base's own reviewed code: fine.
    let f = fixture(&|w: &Path| {
        std::fs::write(w.join("check.sh"), format!("{RATCHET}true\n")).unwrap();
    });
    assert!(matches!(eval(&f, &f.base, FIXTURE, 30), Outcome::Passed(_)));
}

#[test]
fn an_unreadable_merge_tree_diff_fails_closed() {
    let f = fixture(&unrelated);
    let bogus = "1111111111111111111111111111111111111111";
    assert!(tree_check_code_changes(&f.work, &f.base, bogus).is_err());
    assert!(tree_check_code_changes(&f.work, bogus, &f.base).is_err());
    assert_eq!(tree_check_code_changes(&f.work, &f.base, &f.base), Ok(vec![]));
}

// --- (g) the primary repository is untouched; (h) temp dirs are removed -----

/// Everything a run could plausibly disturb in the primary clone: refs
/// (including HEAD and pseudo-refs such as FETCH_HEAD), the index bytes,
/// `git worktree list`, and the work tree's status. Read without optional
/// locks, so taking the snapshot cannot itself refresh the index.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let git_dir = dir.join(".git");
    let mut out = vec![("index".to_string(), std::fs::read(git_dir.join("index")).unwrap())];
    for name in ["HEAD", "FETCH_HEAD", "ORIG_HEAD", "packed-refs", "config"] {
        out.push((name.to_string(), std::fs::read(git_dir.join(name)).unwrap_or_default()));
    }
    for args in [
        &["for-each-ref", "--format=%(refname) %(objectname)"][..],
        &["worktree", "list", "--porcelain"][..],
        &["status", "--porcelain", "--untracked-files=all"][..],
        &["stash", "list"][..],
    ] {
        let o = Command::new("git")
            .current_dir(dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}");
        out.push((args.join(" "), o.stdout));
    }
    out
}

#[test]
fn the_primary_repository_is_byte_identical_after_an_evaluation() {
    for (base_move, label) in [
        (&unrelated as &dyn Fn(&Path), "pass"),
        (&tighten as &dyn Fn(&Path), "fail"),
    ] {
        let f = fixture(base_move);
        let before = snapshot(&f.work);
        let tmp = tempfile::tempdir().unwrap();
        let o = evaluate_in(
            tmp.path(),
            &target(&f, &f.base),
            &["Conflict Marker Check"],
            FIXTURE,
            Duration::from_secs(30),
        );
        match (label, &o) {
            ("pass", Outcome::Passed(_)) | ("fail", Outcome::Failed { .. }) => {}
            _ => panic!("{label}: {o:?}"),
        }
        let after = snapshot(&f.work);
        for ((name, a), (_, b)) in before.iter().zip(&after) {
            assert_eq!(
                String::from_utf8_lossy(a),
                String::from_utf8_lossy(b),
                "{label}: primary repo's `{name}` changed"
            );
        }
    }
}

fn target<'a>(f: &'a Fixture, expected_base: &'a str) -> Target<'a> {
    Target {
        repo_root: &f.work,
        remote: "origin",
        pr: "7",
        base_ref: "main",
        head_sha: &f.head,
        expected_base,
    }
}

#[test]
fn every_temp_dir_is_removed_on_every_path() {
    let evil = |w: &Path| std::fs::write(w.join("check.sh"), "exit 0\n").unwrap();
    let cases: Vec<(Fixture, &[&str], &str)> = vec![
        (fixture(&unrelated), &["Conflict Marker Check"][..], "pass"),
        (fixture(&tighten), &["Conflict Marker Check"][..], "fail"),
        (fixture(&unrelated), &["Slow"][..], "timeout"),
        (fixture_pr(&unrelated, &evil), &["Conflict Marker Check"][..], "check code"),
    ];
    for (f, components, label) in &cases {
        let tmp = tempfile::tempdir().unwrap();
        let o = evaluate_in(
            tmp.path(),
            &target(f, &f.base),
            components,
            FIXTURE,
            Duration::from_secs(2),
        );
        let ok = match *label {
            "pass" => matches!(o, Outcome::Passed(_)),
            "fail" => matches!(o, Outcome::Failed { .. }),
            _ => matches!(o, Outcome::Unknown(_)),
        };
        assert!(ok, "{label}: {o:?}");
        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(left.is_empty(), "{label}: left behind {left:?}");
    }
}

// --- Host-environment failures are no verdict, not "Merge blocked" ---------

const ENV_CI: &str = "jobs:
  structural-checks:
    steps:
      # component: Conflict Marker Check
      - name: Self-test
        run: sh check.sh --self-test
      - name: Check
        run: sh check.sh
";

#[test]
fn environment_failures_are_unknown_not_a_failing_check() {
    for (script, why) in [
        ("exit 78\n", "78"),
        ("no-such-command-10388\n", "127"),
        ("[ \"$1\" != --self-test ] || exit 1\n", "self-test"),
    ] {
        // A base-side change: the checker is the base's own code, not the PR's.
        let f = fixture(&|w: &Path| {
            std::fs::write(w.join("check.sh"), script).unwrap();
            std::fs::write(w.join(".github/workflows/ci.yml"), ENV_CI).unwrap();
        });
        let o = eval(&f, &f.base, FIXTURE, 30);
        assert!(matches!(o, Outcome::Unknown(ref w) if w.contains(why)), "{why}: {o:?}");
    }
}

#[test]
fn exit_status_parses_the_run_checks_trailer() {
    assert_eq!(exit_status("x\n(terminated by exit 78)"), Some(Ok(78)));
    assert_eq!(exit_status("x\n(terminated by exit 1)\n"), Some(Ok(1)));
    assert_eq!(exit_status("\n(terminated by a signal)"), Some(Err(())));
    assert_eq!(exit_status("no trailer"), None);
    assert_eq!(environment_failure("sh check.sh", Some(Ok(1))), None);
    assert!(environment_failure("sh check.sh --self-test", Some(Ok(1))).is_some());
}

// --- Pinned tool versions ---------------------------------------------------

#[test]
fn a_pinned_tool_must_match_cis_version_exactly() {
    assert!(version_matches("lychee 0.24.2", "v0.24.2"));
    assert!(version_matches("lychee v0.24.2", "0.24.2"));
    assert!(!version_matches("lychee 0.24.20", "v0.24.2"));
    assert!(!version_matches("lychee 0.23.0", "v0.24.2"));
    assert!(!version_matches("", "v0.24.2"));
    assert!(!version_matches("lychee 0.24.2", ""));
}

#[test]
fn a_pinned_tool_at_another_version_is_unknown() {
    // `git` stands in for the pinned tool: its `--version` never prints
    // "999.0.0", so the pin cannot hold.
    let ci = "jobs:
  s:
    steps:
      # component: Conflict Marker Check
      - name: Install sh
        run: |
          VER=v999.0.0
          echo install
      - name: Check
        run: sh check.sh
";
    let f = fixture(&|w: &Path| std::fs::write(w.join(".github/workflows/ci.yml"), ci).unwrap());
    let pinned = [CheapCheck {
        skip_steps: &["Install sh"],
        pins: &[ToolPin {
            tool: "git",
            step: "Install sh",
            var: "VER",
        }],
        ..FIXTURE[0]
    }];
    let o = eval(&f, &f.base, &pinned, 30);
    assert!(matches!(o, Outcome::Unknown(ref w) if w.contains("999.0.0")), "{o:?}");
}

#[test]
fn only_conflict_markers_go_stale_when_both_sides_edit_a_plain_file() {
    // The shape the CLI-level test (tests/merge_pr_stale_checks_reverify.rs)
    // relies on: one allowlisted component, needing no lychee.
    let ev = evidence(&["notes/plan.txt"], vec![(STRUCT, mv(&["notes/plan.txt"]))]);
    let got = locally_evaluable(tip(), &req(&[STRUCT]), &[green(STRUCT)], Some(&ev));
    assert_eq!(got, Some(vec!["Conflict Marker Check"]));
}
