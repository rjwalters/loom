//! Differential test: the Rust port of `merge-pr.sh`'s post-merge
//! worktree-cleanup TARGET PLAN (the `WT_ROOT_DIR` / `ISSUE_NUM` /
//! `DEFAULT_WT_PATH` / `JUDGE_PR_WT_PATH` block, #6264/#3530), against the
//! shell it replaced.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** Each case here is one (branch, PR number,
//! worktree-root configuration) triple, materialised as a real temp repo root
//! on disk; both sides then observe that same root — the shell through the real
//! `defaults/scripts/lib/worktree-root.sh` (`loom_worktree_root`, the helper
//! this port retires from `merge-pr.sh`), the Rust through
//! `worktree_root::worktree_root_readable`.
//!
//! The shell side runs the retired block WHOLE, sourced from
//! `tests/fixtures/merge-pr-cleanup-paths-retired.sh` — a frozen verbatim copy,
//! wrapped in a function only so it can be called, with one added `printf` that
//! publishes the four values it left in scope.
//!
//! # What it proves
//!
//! 1. The `^feature/issue-([0-9]+)$` classification agrees on every branch
//!    shape, including the ones where a looser pattern would misfire
//!    (`release-1`, `fix-bug-42`, `feature/issue-42-extra`) and the ones where
//!    a regex engine's defaults differ from bash's: `$` is end-of-STRING in
//!    `[[ =~ ]]`, and `[0-9]` is an ASCII range rather than a Unicode digit
//!    class.
//! 2. The captured issue number is interpolated AS WRITTEN — `feature/issue-007`
//!    names `issue-007`, which a parse-to-integer port would silently move to
//!    `issue-7`.
//! 3. #6264's asymmetry survives: an issue branch names a `pr-<N>` review
//!    worktree alongside `issue-<N>`, a non-issue branch names none (not a
//!    duplicate of its own default path).
//! 4. The worktree root resolves identically for all five configurations the
//!    bash helper distinguishes — default, absolute env override, RELATIVE env
//!    override (warn + fall back), absolute `worktree.root` config override,
//!    relative config override — plus the unreadable-override fallback
//!    (`_loom_root_unreadable`) that `worktree_root_readable` exists to keep.
//!
//! There are no known divergences: the port changes no decision.
//!
//! # What it deliberately does NOT prove
//!
//! The **framing/parse round-trip**. Both sides here emit a rendered line and
//! the comparison is between those two strings, so a field order bash's `read`
//! cannot actually parse back passes this test — the retired shell never
//! serialised anything (it assigned four variables in scope), so the wire format
//! is new surface the port introduced and has no pre-port counterpart to
//! differ from. That gap is covered by
//! `merge_pr::cleanup_paths::tests::the_rendered_line_round_trips_through_the_shells_read`
//! (real `bash`, the verbatim `read` from `merge-pr.sh`) and by Test 10 in
//! `defaults/scripts/tests/test-merge-pr-worktree-path.sh` (the real cleanup
//! block, a non-issue branch, the real daemon). This file only adds the cheap
//! half of the guard: the leading field is asserted non-empty on every case.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::cleanup_paths::{plan, render};
use loom_daemon::worktree_root::worktree_root_readable;

/// Branch names, spanning the classification's whole boundary.
const BRANCHES: &[&str] = &[
    // --- issue-style: the form the strict pattern admits ---
    "feature/issue-1",
    "feature/issue-42",
    "feature/issue-12345",
    "feature/issue-007",
    // --- the trailing-number shapes merge-pr.sh's own comment names ---
    "release-1",
    "fix-bug-42",
    "feature/issue-42-extra",
    "feature/issue-42/sub",
    // --- anchor boundaries ---
    "x/feature/issue-1",
    "Feature/issue-1",
    "feature/issue--1",
    "feature/issue-",
    "feature/issue-1a",
    "feature/issue-1 ",
    "feature/issue- 1",
    "feature/ISSUE-1",
    // --- non-ASCII digits: `[0-9]` is a range, not a digit class ---
    "feature/issue-\u{661}\u{662}",
    "feature/issue-\u{ff11}",
    // --- ordinary non-issue branches ---
    "main",
    "docs/guide-update",
    "dependabot/cargo/serde-1.0.0",
    "",
];

/// PR numbers, as written. `merge-pr.sh` validated `^[0-9]+$` at parse time, so
/// only digit strings can reach here — but zero-padding is preserved by the
/// shell's interpolation and must be by the port too.
const PR_NUMBERS: &[&str] = &["1", "1234", "0012"];

/// How the worktree root is configured for a case.
#[derive(Clone, Copy, Debug)]
enum RootCfg {
    /// No override — `$REPO_ROOT/.loom/worktrees`.
    Default,
    /// `LOOM_WORKTREE_ROOT` set to an absolute path.
    EnvAbsolute,
    /// `LOOM_WORKTREE_ROOT` set to a relative path: warn, fall back.
    EnvRelative,
    /// `.loom/config.json` → `worktree.root`, absolute.
    ConfigAbsolute,
    /// `.loom/config.json` → `worktree.root`, relative: warn, fall back.
    ConfigRelative,
    /// `LOOM_WORKTREE_ROOT` absolute, and the resolved target exists but
    /// `readdir` fails — `_loom_root_unreadable`'s fallback to the default.
    EnvUnreadable,
}

const ROOT_CFGS: &[RootCfg] = &[
    RootCfg::Default,
    RootCfg::EnvAbsolute,
    RootCfg::EnvRelative,
    RootCfg::ConfigAbsolute,
    RootCfg::ConfigRelative,
    RootCfg::EnvUnreadable,
];

fn repo_dir() -> PathBuf {
    // <repo>/loom-daemon → <repo>
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent")
        .to_path_buf()
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-cleanup-paths-retired.sh")
}

fn worktree_root_lib() -> PathBuf {
    repo_dir().join("defaults/scripts/lib/worktree-root.sh")
}

/// One materialised case environment: the repo root the two sides resolve
/// against, and the `LOOM_WORKTREE_ROOT` value (if any) to expose.
struct Env {
    _tmp: tempfile::TempDir,
    repo_root: PathBuf,
    env_root: Option<String>,
}

fn build_env(cfg: RootCfg) -> Option<Env> {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    // A repo basename that is not "loom" or "tmp", so the override branches'
    // repo-basename namespacing is observable rather than coincidental.
    let repo_root = tmp.path().join("my-repo");
    std::fs::create_dir_all(repo_root.join(".loom")).expect("mkdir .loom");
    let external = tmp.path().join("external wt");
    let mut env_root = None;
    match cfg {
        RootCfg::Default => {}
        RootCfg::EnvAbsolute => {
            env_root = Some(external.to_string_lossy().into_owned());
        }
        RootCfg::EnvRelative => env_root = Some("relative/wt".to_string()),
        RootCfg::ConfigAbsolute => {
            write_config(&repo_root, &external.to_string_lossy());
        }
        RootCfg::ConfigRelative => write_config(&repo_root, "relative/wt"),
        RootCfg::EnvUnreadable => {
            // The override target must EXIST (so `[[ -d ]]` passes) and refuse
            // readdir. That is only observable as a non-root user; when the
            // chmod does not actually deny listing, skip the case rather than
            // assert a property the platform does not have.
            let target = external.join("my-repo");
            std::fs::create_dir_all(&target).expect("mkdir override target");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000))
                    .expect("chmod 000");
            }
            if std::fs::read_dir(&target).is_ok() {
                return None;
            }
            env_root = Some(external.to_string_lossy().into_owned());
        }
    }
    Some(Env {
        _tmp: tmp,
        repo_root,
        env_root,
    })
}

fn write_config(repo_root: &Path, root: &str) {
    let json = format!(
        "{{\n  \"worktree\": {{\n    \"root\": {}\n  }}\n}}\n",
        serde_json::to_string(root).expect("encode")
    );
    std::fs::write(repo_root.join(".loom/config.json"), json).expect("write config");
}

/// The frozen block, under the same options `merge-pr.sh` runs with, sourcing
/// the REAL `worktree-root.sh` so `loom_worktree_root` is the helper itself.
///
/// `$1` = fixture, `$2` = worktree-root lib, `$3` = repo root, `$4` = branch,
/// `$5` = PR number. `ISSUE_NUM` is explicitly unset: the live script only ever
/// ran this once per process, and each case here gets its own `bash` too, but
/// saying so keeps the fixture's `${ISSUE_NUM:-}` honest rather than incidental.
const HARNESS: &str = r#"set -uo pipefail
source "$2"
REPO_ROOT="$3"; PR_BRANCH="$4"; PR_NUMBER="$5"
unset ISSUE_NUM
source "$1"
_cleanup_paths_retired
"#;

fn shell_line(env: &Env, branch: &str, pr: &str) -> String {
    let mut cmd = Command::new("bash");
    cmd.env("LC_ALL", "C")
        .env_remove("LOOM_WORKTREE_ROOT")
        .arg("-c")
        .arg(HARNESS)
        .arg("bash")
        .arg(fixture())
        .arg(worktree_root_lib())
        .arg(&env.repo_root)
        .arg(branch)
        .arg(pr);
    if let Some(v) = &env.env_root {
        cmd.env("LOOM_WORKTREE_ROOT", v);
    }
    let out = cmd.output().expect("run the frozen retired block");
    assert!(
        out.status.success(),
        "the retired block must not fail; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn rust_line(env: &Env, branch: &str, pr: &str) -> String {
    match &env.env_root {
        Some(v) => std::env::set_var("LOOM_WORKTREE_ROOT", v),
        None => std::env::remove_var("LOOM_WORKTREE_ROOT"),
    }
    let root = worktree_root_readable(&env.repo_root);
    render(&plan(branch, pr, &root)).expect("a temp-dir root has no tab or newline in it")
}

/// One `#[test]`, deliberately: the Rust side reads `LOOM_WORKTREE_ROOT` from
/// the process environment, so splitting the comparison across parallel test
/// functions would race on it.
#[test]
fn the_plan_agrees_with_the_retired_shell_on_every_input() {
    assert!(fixture().is_file(), "the frozen fixture must exist");
    assert!(worktree_root_lib().is_file(), "the shell side needs the real worktree-root.sh");

    let mut compared = 0usize;
    let mut skipped_cfgs = 0usize;
    let mut with_issue = 0usize;
    let mut without_issue = 0usize;
    let mut distinct_roots: Vec<String> = Vec::new();

    for &cfg in ROOT_CFGS {
        let Some(env) = build_env(cfg) else {
            skipped_cfgs += 1;
            continue;
        };
        for branch in BRANCHES {
            for pr in PR_NUMBERS {
                let shell = shell_line(&env, branch, pr);
                let rust = rust_line(&env, branch, pr);
                assert_eq!(
                    rust, shell,
                    "cfg={cfg:?} branch={branch:?} pr={pr:?}: the port disagrees with the \
                     retired shell"
                );
                // Field 2 non-empty iff the branch classified as issue-style;
                // record which side of #6264's asymmetry this case exercised.
                // (Field 1 is the default path — it leads precisely because it
                // is the only one that is never empty; see `render`'s docs.)
                let fields: Vec<&str> = rust.trim_end_matches('\n').split('\t').collect();
                assert_eq!(fields.len(), 4, "framing must stay four fields: {rust:?}");
                assert!(
                    !fields[1].is_empty(),
                    "the leading field must never be empty — bash's `read` cannot preserve an \
                     empty leading field when IFS is a whitespace character: {rust:?}"
                );
                if fields[2].is_empty() {
                    assert!(fields[3].is_empty(), "no issue number ⇒ no judge-pr path");
                    without_issue += 1;
                } else {
                    assert!(!fields[3].is_empty(), "issue number ⇒ a judge-pr path");
                    with_issue += 1;
                }
                let root = fields[1]
                    .rsplit_once('/')
                    .map_or(fields[1], |(head, _)| head)
                    .to_string();
                if !distinct_roots.contains(&root) {
                    distinct_roots.push(root);
                }
                compared += 1;
            }
        }
    }

    assert_eq!(compared, (ROOT_CFGS.len() - skipped_cfgs) * BRANCHES.len() * PR_NUMBERS.len());
    // Size says nothing about reach. Both sides of the classification, and at
    // least the default plus one overridden base, must actually have occurred —
    // otherwise the comparison above could be agreeing on one shape throughout.
    assert!(with_issue > 0, "no case classified as an issue branch");
    assert!(without_issue > 0, "no case classified as a PR-style branch");
    assert!(
        distinct_roots.len() >= 2,
        "every case resolved the same worktree base ({distinct_roots:?}); the override \
         configurations are not being exercised"
    );
    assert!(
        skipped_cfgs <= 1,
        "only the unreadable-override case may be skipped, and only when the platform \
         cannot deny readdir; skipped {skipped_cfgs}"
    );
}
