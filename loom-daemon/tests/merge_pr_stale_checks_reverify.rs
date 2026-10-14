//! `loom-daemon merge-pr stale-checks` with the #10388 local merge-tree
//! re-verification, end to end through the REAL binary.
//!
//! The library tests (`local_eval/tests.rs`) pin `evaluate()`; this pins the
//! CLI wiring around it: the enablement resolution (env > config > default
//! on, #10465),
//! the verdict promotion (STALE -> the CLEAN sentinel, exit 0), the merge-log
//! line, the evidence comment's marker, and the fail-closed paths keeping the
//! exact refusal. It drives `--from-stdin` with `"reverify": true`, the
//! fixture as the repository (cwd) and a bare fixture as `origin`, so it never
//! touches a forge.
//!
//! Also covered here, against the real process: (g) the primary repository's
//! refs, index, `git worktree list` and FETCH_HEAD are byte-identical after a
//! run, and (h) the run leaves nothing in its temp dir.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const CLEAN: &str = "LOOM-STALE-CHECKS-CLEAN";

/// The fixture's ci.yml: one allowlisted component, run as CI would.
const CI_YML: &str = "jobs:
  structural-checks:
    name: Structural Checks
    steps:
      # component: Conflict Marker Check
      - name: Check no tracked file carries git conflict markers
        if: ${{ !cancelled() }}
        run: bash defaults/scripts/check-conflict-markers.sh
";

/// A conflict-marker scan in miniature (needs a real git work tree, as the
/// real one does).
const CHECKER: &str = "set -e
git rev-parse --is-inside-work-tree >/dev/null
if git grep -n -e '^<<<<<<< ' -- . ; then
  echo 'conflict markers found' >&2
  exit 1
fi
";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

struct Fixture {
    _t: tempfile::TempDir,
    work: PathBuf,
    tmp: PathBuf,
    root: String,
    base: String,
    head: String,
}

/// origin (bare) with `main` and `refs/pull/7/head`; `work` is the primary
/// clone the CLI runs in. Both sides edit `notes/plan.txt` (different lines,
/// so the merge is clean), which makes exactly `Conflict Marker Check` stale.
fn fixture(pr_line: &str) -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let origin = t.path().join("origin.git");
    let work = t.path().join("work");
    let tmp = t.path().join("tmp");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    git(
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
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
    std::fs::create_dir_all(work.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(work.join("defaults/scripts")).unwrap();
    std::fs::create_dir_all(work.join("notes")).unwrap();
    std::fs::write(work.join(".github/workflows/ci.yml"), CI_YML).unwrap();
    std::fs::write(work.join("defaults/scripts/check-conflict-markers.sh"), CHECKER).unwrap();
    std::fs::write(work.join("notes/plan.txt"), "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "base"]);
    let root = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-qb", "pr"]);
    std::fs::write(
        work.join("notes/plan.txt"),
        format!("{pr_line}\ntwo\nthree\nfour\nfive\nsix\n"),
    )
    .unwrap();
    git(&work, &["commit", "-qam", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("notes/plan.txt"), "one\ntwo\nthree\nfour\nfive\nSIX\n").unwrap();
    git(&work, &["commit", "-qam", "sibling"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    git(
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
        tmp,
        root,
        base,
        head,
    }
}

impl Fixture {
    /// `merge.reverifyStaleChecks` in the fixture repo's `.loom/config.json`
    /// (untracked; `None` = no config file at all, but `.loom/` exists).
    fn config(&self, flag: Option<bool>) {
        let dir = self.work.join(".loom");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        match flag {
            Some(b) => {
                std::fs::write(&path, format!(r#"{{"merge":{{"reverifyStaleChecks":{b}}}}}"#))
                    .unwrap()
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    fn payload(&self, reverify: Option<bool>) -> String {
        let mut v = serde_json::json!({
            "tip_sha": self.base,
            "base_tip": "2026-10-05T07:00:00Z",
            "required": ["Structural Checks"],
            "check_runs": [{
                "name": "Structural Checks",
                "status": "completed",
                "conclusion": "success",
                "started_at": "2026-10-05T06:00:00Z"
            }],
            "pr_files": [{"filename": "notes/plan.txt", "status": "modified"}],
            "base_moves": {"Structural Checks": {
                "tested_base": self.root,
                "files": [{"filename": "notes/plan.txt", "status": "modified"}]
            }}
        });
        if let Some(r) = reverify {
            v["reverify"] = serde_json::Value::Bool(r);
        }
        v.to_string()
    }

    fn run(&self, payload: &str, env_flag: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        cmd.args([
            "merge-pr",
            "stale-checks",
            "--pr",
            "7",
            "--repo",
            "acme/widgets",
            "--head-sha",
            &self.head,
            "--base-ref",
            "main",
            "--from-stdin",
        ])
        .current_dir(&self.work)
        .env("TMPDIR", &self.tmp)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_GH_BIN", self.tmp.join("no-gh-here"))
        .env_remove("LOOM_STALE_CHECKS_DRY_RUN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        match env_flag {
            Some(v) => cmd.env("LOOM_MERGE_REVERIFY_STALE_CHECKS", v),
            None => cmd.env_remove("LOOM_MERGE_REVERIFY_STALE_CHECKS"),
        };
        let mut child = cmd.spawn().unwrap();
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    /// (g): refs (incl. HEAD/FETCH_HEAD), index bytes, worktree list, status.
    fn snapshot(&self) -> Vec<(String, String)> {
        let g = self.work.join(".git");
        let mut out =
            vec![("index".to_string(), format!("{:?}", std::fs::read(g.join("index")).unwrap()))];
        for f in ["HEAD", "FETCH_HEAD", "ORIG_HEAD", "packed-refs", "config"] {
            out.push((f.to_string(), std::fs::read_to_string(g.join(f)).unwrap_or_default()));
        }
        for args in [
            &["for-each-ref", "--format=%(refname) %(objectname)"][..],
            &["worktree", "list", "--porcelain"][..],
            &["status", "--porcelain"][..],
        ] {
            let o = Command::new("git")
                .current_dir(&self.work)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .args(args)
                .output()
                .unwrap();
            out.push((args.join(" "), String::from_utf8_lossy(&o.stdout).into_owned()));
        }
        out
    }

    /// (h): nothing left in the run's TMPDIR.
    fn assert_tmp_empty(&self, label: &str) {
        let left: Vec<_> = std::fs::read_dir(&self.tmp)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(left.is_empty(), "{label}: temp dir not cleaned: {left:?}");
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The checker needs bash, git and jq (the real Conflict Marker Check's
/// `requires`); a host without them gets no verdict, which other tests pin.
fn prerequisites() -> bool {
    let ok = ["bash", "git", "jq"].iter().all(|t| on_path(t));
    if !ok {
        eprintln!("skipping: bash/git/jq not all on PATH");
    }
    ok
}

/// (a) at the CLI: opted in, every stale component allowlisted and passing on
/// the merge tree ⇒ the CLEAN sentinel alone on stdout, exit 0, the merge-log
/// line and the evidence marker on stderr; (g) and (h) hold.
#[test]
fn opted_in_a_passing_merge_tree_turns_the_refusal_clean() {
    if !prerequisites() {
        return;
    }
    let f = fixture("ONE");
    f.config(Some(true));
    let before = f.snapshot();
    let out = f.run(&f.payload(Some(true)), None);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(stdout, format!("{CLEAN}\n"), "stdout carries the sentinel and nothing else");
    let line = stderr
        .lines()
        .find(|l| l.starts_with("LOOM-MERGE-TREE-REVERIFY "))
        .unwrap_or_else(|| panic!("no merge-log line: {stderr}"));
    for needle in [
        "pr=7",
        "verdict=pass",
        &format!("base={}", f.base),
        &format!("head={}", f.head),
        "tree=",
        "Conflict Marker Check=pass",
    ] {
        assert!(line.contains(needle), "{needle} missing from {line}");
    }
    let marker = format!("<!-- loom:merge-tree-reverify base={} head={} tree=", f.base, f.head);
    assert!(stderr.contains(&marker), "evidence marker: {stderr}");
    assert!(stderr.contains("posts no PR comment"), "--from-stdin never posts: {stderr}");
    assert_eq!(before, f.snapshot(), "(g) the primary repository changed");
    f.assert_tmp_empty("pass");
}

/// Default-on (#10465): with no config, or an unparseable env value and no
/// config, a passing merge tree turns the refusal clean exactly as an
/// explicit opt-in does.
#[test]
fn by_default_a_passing_merge_tree_turns_the_refusal_clean() {
    if !prerequisites() {
        return;
    }
    let f = fixture("ONE");
    for (env, label) in [
        (None, "no config"),
        (Some("garbage"), "unparseable env, no config"),
    ] {
        f.config(None);
        let before = f.snapshot();
        let out = f.run(&f.payload(Some(true)), env);
        let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
        assert_eq!(out.status.code(), Some(0), "{label}: stdout: {stdout}\nstderr: {stderr}");
        assert_eq!(stdout, format!("{CLEAN}\n"), "{label}: the sentinel and nothing else");
        assert!(stderr.contains("verdict=pass"), "{label}: {stderr}");
        assert_eq!(before, f.snapshot(), "{label}: (g) the primary repository changed");
        f.assert_tmp_empty(label);
    }
}

/// Off (explicitly, by config or env), behavior is byte-identical to a
/// payload that never asked for re-verification: same exit, stdout and
/// stderr, no fetch, no temp files.
#[test]
fn off_the_output_is_byte_identical_to_today() {
    let f = fixture("ONE");
    f.config(None);
    let before = f.snapshot();
    let today = f.run(&f.payload(None), None);
    assert_eq!(
        today.status.code(),
        Some(1),
        "fixture: the guard refuses: {}",
        text(&today.stdout)
    );
    assert!(text(&today.stdout).contains("Merge blocked"));
    for (cfg, env, label) in [
        (Some(false), None, "config false"),
        (Some(true), Some("0"), "env 0 beats config true"),
        (None, Some("0"), "env 0, no config"),
        (Some(false), Some("garbage"), "unparseable env falls through to config false"),
    ] {
        f.config(cfg);
        let got = f.run(&f.payload(Some(true)), env);
        assert_eq!(got.status.code(), today.status.code(), "{label}");
        assert_eq!(text(&got.stdout), text(&today.stdout), "{label}: stdout");
        assert_eq!(text(&got.stderr), text(&today.stderr), "{label}: stderr");
    }
    f.config(None);
    assert_eq!(before, f.snapshot(), "flag off must not fetch or touch the repository");
    f.assert_tmp_empty("off");
}

/// The env tier opts in even when the config says no.
#[test]
fn the_env_opt_in_beats_a_config_opt_out() {
    if !prerequisites() {
        return;
    }
    let f = fixture("ONE");
    f.config(Some(false));
    let out = f.run(&f.payload(Some(true)), Some("1"));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), format!("{CLEAN}\n"));
}

/// (b) at the CLI: a check failing on the merge tree keeps exactly today's
/// refusal on stdout (exit 1); the failure is logged and would be posted.
#[test]
fn a_failing_merge_tree_check_keeps_the_exact_refusal() {
    if !prerequisites() {
        return;
    }
    let f = fixture("<<<<<<< ours");
    f.config(None);
    let today = f.run(&f.payload(None), None);
    f.config(Some(true));
    let before = f.snapshot();
    let out = f.run(&f.payload(Some(true)), None);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(text(&out.stdout), text(&today.stdout), "the refusal is unchanged");
    assert!(stderr.contains("verdict=fail"), "{stderr}");
    assert!(stderr.contains("Merge blocked: a stale cheap check FAILS"), "{stderr}");
    assert_eq!(before, f.snapshot(), "(g) the primary repository changed");
    f.assert_tmp_empty("fail");
}

/// No verdict (here: the assessment judged a base the remote no longer has
/// at its tip) keeps the refusal and posts nothing.
#[test]
fn no_verdict_keeps_the_refusal_without_a_comment() {
    let f = fixture("ONE");
    f.config(Some(true));
    let mut v: serde_json::Value = serde_json::from_str(&f.payload(Some(true))).unwrap();
    v["tip_sha"] = serde_json::Value::String(f.root.clone());
    let out = f.run(&v.to_string(), None);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(!text(&out.stdout).contains(CLEAN));
    assert!(stderr.contains("gave no verdict"), "{stderr}");
    assert!(!stderr.contains("loom:merge-tree-reverify"), "no evidence comment: {stderr}");
    f.assert_tmp_empty("unknown");
}
