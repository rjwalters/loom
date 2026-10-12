//! Differential test: `loom-daemon merge-pr worktree-teardown` against the
//! retired shell it replaced — the `git worktree remove --force` / one
//! `git worktree prune` + retry ladder and its success / failure report at the
//! end of `merge-pr.sh`'s `_remove_loom_worktree` (#6372).
//!
//! One once-generated corpus feeds both sides (verification-recipes §6). Each
//! case is a REAL repository with a REAL linked worktree, rebuilt from scratch
//! at the same path before each side runs, plus a scripted `git` wrapper on
//! `PATH` that can fail the `remove` calls (with chosen output on chosen
//! streams) or the `prune`, and that logs every `remove`/`prune` it sees. The
//! shell side runs the frozen block from
//! `tests/fixtures/merge-pr-worktree-teardown-retired.sh`; the Rust side runs
//! the real CLI. Both are reduced to the same trace — the verdict, the
//! `LEVEL<TAB>message` records, the git call sequence, and whether the
//! worktree directory survived — and must agree exactly.
//!
//! The one normalization is the documented rendering difference: the retired
//! `warning "$remove_err"` printed a multi-line git error in ONE call, the port
//! emits one record per line. The shell side's `warning` stub therefore splits
//! its argument on newlines before recording, so a divergence in the TEXT of
//! any line still fails here.
//!
//! The last two tests drive the LIVE `_remove_loom_worktree`, extracted from
//! `merge-pr.sh`, to pin the wrapper's fail-closed direction: a binary that
//! cannot answer `worktree-teardown` removes nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path() -> PathBuf {
    manifest().join("tests/fixtures/merge-pr-worktree-teardown-retired.sh")
}

fn merge_pr_path() -> PathBuf {
    manifest().join("../defaults/scripts/merge-pr.sh")
}

fn real_git() -> &'static str {
    static GIT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GIT.get_or_init(|| {
        let out = Command::new("bash")
            .args(["-c", "command -v git"])
            .output()
            .expect("bash ran");
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(!p.is_empty(), "git must be on PATH for this test");
        p
    })
}

/// A `git` wrapper, configured per run through `TD_*` env vars:
/// `TD_FAIL_REMOVES` (how many `worktree remove --force` calls fail),
/// `TD_ERR` / `TD_OUT` (what a failing call writes to stderr / stdout; the
/// attempt number replaces `@N@`), `TD_FAIL_PRUNE` (make `prune` fail, after
/// writing noise to both streams that must be discarded), `TD_STATE` (a dir
/// for the call counter and log).
fn write_wrapper(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let script = format!(
        r#"#!/usr/bin/env bash
REAL={real}
if [[ "$*" == *"worktree remove"* && "$*" == *"--force"* ]]; then
  n=$(( $(cat "$TD_STATE/n" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$TD_STATE/n"
  echo remove >> "$TD_STATE/log"
  if [[ "$n" -le "${{TD_FAIL_REMOVES:-0}}" ]]; then
    [[ -z "${{TD_OUT:-}}" ]] || printf '%s\n' "${{TD_OUT//@N@/$n}}"
    [[ -z "${{TD_ERR:-}}" ]] || printf '%s\n' "${{TD_ERR//@N@/$n}}" >&2
    [[ -z "${{TD_OUT2:-}}" ]] || printf '%s\n' "${{TD_OUT2//@N@/$n}}"
    exit 128
  fi
  exec "$REAL" "$@"
fi
if [[ "$*" == *"worktree prune"* ]]; then
  echo prune >> "$TD_STATE/log"
  if [[ -n "${{TD_FAIL_PRUNE:-}}" ]]; then echo "prune noise out"; echo "prune noise err" >&2; exit 1; fi
  exec "$REAL" "$@"
fi
exec "$REAL" "$@"
"#,
        real = real_git()
    );
    let p = bin.join("git");
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[derive(Clone, Default)]
struct Case {
    name: &'static str,
    fail_removes: u32,
    err: &'static str,
    out: &'static str,
    out2: &'static str,
    fail_prune: bool,
    /// Lock the worktree with REAL git, so real git refuses both attempts.
    lock: bool,
    /// Target a path that was never a worktree.
    bogus: bool,
    /// Directory name of the worktree (exercise spaces).
    wt_name: &'static str,
}

/// Build `<dir>/repo` with a linked worktree at `<dir>/<wt_name>`, fresh.
fn build_fixture(dir: &Path, case: &Case) -> (PathBuf, PathBuf) {
    if dir.exists() {
        // The worktree may have been removed or not; start over either way.
        std::fs::remove_dir_all(dir).unwrap();
    }
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = Command::new(real_git())
            .arg("-C")
            .arg(&repo)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "fixture git {args:?}");
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=t@e",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    let wt = dir.join(case.wt_name);
    if case.bogus {
        std::fs::create_dir_all(&wt).unwrap();
    } else {
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/issue-1",
            wt.to_str().unwrap(),
        ]);
        // Untracked user work: `--force` must remove it anyway (the dirty guard
        // already ran upstream) — both sides see the same tree.
        std::fs::write(wt.join("scratch.txt"), "x").unwrap();
        if case.lock {
            git(&["worktree", "lock", wt.to_str().unwrap()]);
        }
    }
    (repo, wt)
}

fn env_for(cmd: &mut Command, bin: &Path, state: &Path, case: &Case) {
    std::fs::create_dir_all(state).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    cmd.env("PATH", path)
        .env("TD_STATE", state)
        .env("TD_FAIL_REMOVES", case.fail_removes.to_string())
        .env("TD_ERR", case.err)
        .env("TD_OUT", case.out)
        .env("TD_OUT2", case.out2);
    if case.fail_prune {
        cmd.env("TD_FAIL_PRUNE", "1");
    } else {
        cmd.env_remove("TD_FAIL_PRUNE");
    }
}

fn call_log(state: &Path) -> String {
    std::fs::read_to_string(state.join("log")).unwrap_or_default()
}

fn run_shell(dir: &Path, bin: &Path, case: &Case) -> String {
    let (repo, wt) = build_fixture(dir, case);
    let state = dir.join("state-shell");
    let driver = r#"
_rec() { local l; while IFS= read -r l; do printf '%s\t%s\n' "$1" "$l"; done <<<"$2"; }
success() { _rec SUCCESS "$*"; }
warning() { _rec WARNING "$*"; }
REPO_ROOT="$2"
source "$1"
_retired_worktree_teardown "$3"
"#;
    let mut cmd = Command::new("bash");
    cmd.args(["-c", driver, "driver"])
        .arg(fixture_path())
        .arg(&repo)
        .arg(&wt);
    env_for(&mut cmd, bin, &state, case);
    let out = cmd.output().expect("bash ran the frozen shell side");
    assert!(
        out.status.success(),
        "frozen shell exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut removed = false;
    let mut records = String::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line == "LOOM-RETIRED-REMOVED" {
            removed = true;
        } else {
            records.push_str(line);
            records.push('\n');
        }
    }
    let verdict = if removed {
        "LOOM-WORKTREE-TEARDOWN REMOVED"
    } else {
        "LOOM-WORKTREE-TEARDOWN FAILED"
    };
    format!(
        "{verdict}\n{records}--calls--\n{}--exists-- {}\n",
        call_log(&state),
        wt.exists()
    )
}

fn run_rust(dir: &Path, bin: &Path, case: &Case) -> String {
    let (repo, wt) = build_fixture(dir, case);
    let state = dir.join("state-rust");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["merge-pr", "worktree-teardown", "--repo-root"])
        .arg(&repo)
        .arg("--path")
        .arg(&wt);
    env_for(&mut cmd, bin, &state, case);
    let out = cmd.output().expect("loom-daemon ran");
    assert!(
        out.status.success(),
        "worktree-teardown exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    format!(
        "{}--calls--\n{}--exists-- {}\n",
        String::from_utf8_lossy(&out.stdout),
        call_log(&state),
        wt.exists()
    )
}

fn corpus() -> Vec<Case> {
    let base = Case {
        wt_name: "wt",
        ..Case::default()
    };
    vec![
        Case {
            name: "healthy first-try removal",
            ..base.clone()
        },
        Case {
            name: "healthy, path with spaces",
            wt_name: "a wt with spaces",
            ..base.clone()
        },
        Case {
            name: "first attempt fails, prune, retry succeeds",
            fail_removes: 1,
            err: "fatal: simulated stale registration (attempt @N@)",
            ..base.clone()
        },
        Case {
            name: "both attempts fail: the RETRY's error is reported",
            fail_removes: 99,
            err: "fatal: simulated persistent failure (attempt @N@)",
            ..base.clone()
        },
        Case {
            name: "prune fails: no retry, the FIRST error is reported",
            fail_removes: 99,
            err: "fatal: first and only (attempt @N@)",
            fail_prune: true,
            ..base.clone()
        },
        Case {
            name: "first fails, prune fails, but retry would have succeeded",
            fail_removes: 1,
            err: "fatal: once (attempt @N@)",
            fail_prune: true,
            ..base.clone()
        },
        Case {
            name: "multi-line error",
            fail_removes: 99,
            err: "fatal: line one (attempt @N@)\nhint: line two\n\nhint: after a blank",
            ..base.clone()
        },
        Case {
            name: "empty error output",
            fail_removes: 99,
            ..base.clone()
        },
        Case {
            name: "stdout and stderr interleave on one stream",
            fail_removes: 99,
            out: "out-before (attempt @N@)",
            err: "err-middle",
            out2: "out-after",
            ..base.clone()
        },
        Case {
            name: "trailing and leading blank lines",
            fail_removes: 99,
            err: "\nfatal: framed (attempt @N@)\n\n\n",
            ..base.clone()
        },
        Case {
            name: "format and escape hazards",
            fail_removes: 99,
            err: r"fatal: 100% $HOME `x` \n \t {} %s (attempt @N@)",
            ..base.clone()
        },
        Case {
            name: "hazards in the path",
            fail_removes: 99,
            err: "fatal: x",
            wt_name: "wt $HOME %s {} 100%",
            ..base.clone()
        },
        Case {
            name: "real git: a locked worktree refuses both attempts",
            lock: true,
            ..base.clone()
        },
        Case {
            name: "real git: a path that is not a worktree",
            bogus: true,
            ..base.clone()
        },
    ]
}

#[test]
fn frozen_shell_and_rust_cli_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let bin = root.join("fake-bin");
    write_wrapper(&bin);
    let mut removed_after_prune = false;
    let mut retry_error_reported = false;
    for (i, case) in corpus().iter().enumerate() {
        let dir = root.join(format!("case-{i}"));
        let shell = run_shell(&dir, &bin, case);
        let rust = run_rust(&dir, &bin, case);
        assert_eq!(shell, rust, "diverged on case {:?}", case.name);
        removed_after_prune |= rust.contains("after pruning a stale worktree registration");
        retry_error_reported |= rust.contains("(attempt 2)");
    }
    // The corpus must actually have exercised both prune arms; a wrapper that
    // silently stopped intercepting would make every case a first-try success.
    assert!(removed_after_prune, "no case reached the prune-then-retry success");
    assert!(retry_error_reported, "no case reported a retry's error");
}

#[test]
fn the_live_script_no_longer_carries_the_ladder() {
    let src = std::fs::read_to_string(merge_pr_path()).unwrap();
    assert!(
        !src.contains(r#"worktree remove "$worktree_path" --force 2>&1"#),
        "merge-pr.sh still runs the retired remove ladder itself"
    );
    assert!(src.contains("_mp_worktree worktree-teardown"));
}

/// Extract the live `_remove_loom_worktree` (and the helpers it reaches) and
/// run it against a fresh worktree with `LOOM_DAEMON_SELF_BIN` pointed at a
/// shim that answers `worktree-teardown` with `teardown_script` and delegates
/// every other verb to the real binary. Returns (stdout, worktree survived).
fn run_live_wrapper(teardown_script: &str) -> (String, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let case = Case {
        wt_name: "wt",
        ..Case::default()
    };
    let (repo, wt) = build_fixture(&root.join("live"), &case);
    std::fs::write(wt.join(".loom-managed"), "").unwrap();
    std::fs::remove_file(wt.join("scratch.txt")).unwrap();
    let real = env!("CARGO_BIN_EXE_loom-daemon");
    let shim = root.join("shim-loom-daemon");
    std::fs::write(
        &shim,
        format!(
            "#!/usr/bin/env bash\nif [[ \"$1\" == merge-pr && \"$2\" == worktree-teardown ]]; then {teardown_script}; fi\nexec \"{real}\" \"$@\"\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let driver = r#"
set -euo pipefail
extract_fn() { awk -v fn="$1" '$0 ~ "^"fn"\\(\\) \\{" { grab=1 } grab { print } grab && /^}/ { exit }' "$2"; }
info() { echo "INFO: $*"; }
warning() { echo "WARN: $*"; }
success() { echo "OK: $*"; }
error() { echo "ERROR: $*" >&2; return 1; }
loom_record_worktree_removal() { echo "LEDGER: $3"; }
for f in _mp_worktree _primary_worktree_path _worktree_branch_for _remove_loom_worktree; do
  eval "$(extract_fn "$f" "$1")"
done
REPO_ROOT="$2"
_remove_loom_worktree "$3"
"#;
    let out = Command::new("bash")
        .args(["-c", driver, "driver"])
        .arg(merge_pr_path())
        .arg(&repo)
        .arg(&wt)
        .env("LOOM_DAEMON_SELF_BIN", &shim)
        .env("LOOM_DAEMON_BIN", real)
        .current_dir(&root)
        .output()
        .expect("bash ran the live wrapper");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "live wrapper exited {:?}: {stdout}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    (stdout, wt.exists())
}

#[test]
fn the_live_wrapper_removes_through_the_verb() {
    let (out, survived) = run_live_wrapper(":");
    assert!(!survived, "the worktree should be gone: {out}");
    assert!(out.contains("OK: Worktree removed"), "{out}");
    assert!(out.contains("LEDGER: "), "the success-only ledger step ran: {out}");
}

#[test]
fn the_live_wrapper_removes_nothing_when_the_verb_cannot_run() {
    // A binary predating the verb (clap usage error, exit 2), and one that
    // answers off-protocol with exit 0: both must leave the worktree in place,
    // warn, and skip the success-only steps.
    for script in [
        "echo 'error: unrecognized subcommand' >&2; exit 2",
        "echo 'LOOM-WORKTREE-TEARDOWN MAYBE'; exit 0",
    ] {
        let (out, survived) = run_live_wrapper(script);
        assert!(survived, "a verb that did not run must remove nothing ({script}): {out}");
        assert!(
            out.contains("WARN: Could not remove worktree at")
                && out.contains("no removal was attempted"),
            "{out}"
        );
        assert!(
            !out.contains("LEDGER: "),
            "no ledger entry for a removal that did not happen: {out}"
        );
        assert!(!out.contains("OK: Worktree removed"), "{out}");
    }
}
