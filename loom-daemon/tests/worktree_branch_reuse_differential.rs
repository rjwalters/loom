//! Differential test: `loom-daemon worktree-branch-reuse` against the shell it
//! replaced (#8195 slice 14, epic #7810), on one shared fixture per side.
//!
//! Compared for every (branch state, forge answer, `--json`) case: the exit
//! code and the message lines (ANSI-free, prefix-stripped, in order). One
//! DELIBERATE divergence is asserted as such: the `--json` refusal document,
//! which the retired shell spliced by hand and which is not valid JSON for a
//! refname containing a quote — a name `git check-ref-format` permits and the
//! custom-branch argument makes reachable.
//!
//! Both sides run with the SAME PATH — a fake `gh` and nothing else that can
//! answer a forge query — so the two implementations pick the same probe
//! command by the same rule, and the comparison is of the arm, not of forge
//! plumbing.
//!
//! **Every fixture repo lives under a directory named `re po`.** That space is
//! the port's own #7858 regression case at the process boundary: an argument
//! that was ever word-split would make `git` resolve nothing here and collapse
//! every verdict to `unknown`.
//!
//! Then the live `worktree.sh` is driven end to end with the port pinned via
//! `LOOM_DAEMON_SELF_BIN`, to prove the one-line stub wires it correctly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}
fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts")
}
fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-branch-reuse-retired.sh")
}

fn hermetic(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
}

fn git(dir: &Path, args: &[&str]) {
    let out = hermetic(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", r])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Which merged-PR answer the fake forge gives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Forge {
    /// A merged PR #999 whose head is the branch's current tip.
    MergedAtTip,
    /// No merged PR for the name.
    None,
    /// Every call fails — the outage case the arm must fail OPEN on.
    Off,
}

struct Fx {
    root: PathBuf,
    repo: PathBuf,
    fakebin: PathBuf,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// `main` pushed to a bare origin, plus a local `feature/issue-42` with one
/// commit of its own. `advance_main` pushes a later commit onto `main`, so the
/// branch stops containing all of the base ref's history.
fn fixture(tag: &str, branch: &str, advance_main: bool, forge: Forge) -> Fx {
    let root =
        std::env::temp_dir().join(format!("loom-wt-reuse-diff-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("re po"); // a space: #7858's class
    fs::create_dir_all(&repo).unwrap();
    git(&root, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            root.join("origin.git").to_str().unwrap(),
        ],
    );
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["checkout", "-q", "-b", branch]);
    fs::write(repo.join("slice.txt"), "slice\n").unwrap();
    git(&repo, &["add", "slice.txt"]);
    git(&repo, &["commit", "-q", "-m", "slice work"]);
    git(&repo, &["checkout", "-q", "main"]);
    if advance_main {
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "later main"]);
        git(&repo, &["push", "-q", "origin", "main"]);
    }
    git(&repo, &["fetch", "-q", "origin"]);

    let fakebin = root.join("fakebin");
    fs::create_dir_all(&fakebin).unwrap();
    let tip = rev(&repo, branch);
    let body = match forge {
        Forge::Off => "echo 'fake forge unavailable' >&2\nexit 1\n".to_string(),
        Forge::None => "echo '[]'\nexit 0\n".to_string(),
        Forge::MergedAtTip => {
            format!("echo '[{{\"headRefOid\": \"{tip}\", \"number\": 999}}]'\nexit 0\n")
        }
    };
    fs::write(fakebin.join("gh"), format!("#!/bin/bash\n{body}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fakebin.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    Fx {
        root,
        repo,
        fakebin,
    }
}

/// The one PATH both sides see: the fake `gh`, plus the system directories that
/// hold `git`, `jq` and `bash`. Deliberately WITHOUT any `loom-daemon`, so the
/// shell's `command -v loom-daemon` and the port's `on_path("loom-daemon")`
/// both fall through to `gh` and the two probe the same thing.
fn path(fx: &Fx) -> String {
    format!("{}:/usr/bin:/bin", fx.fakebin.display())
}

fn shell(fx: &Fx, branch: &str, json: bool) -> Output {
    hermetic(
        Command::new("bash")
            .arg(fixture_script())
            .args([
                branch,
                "42",
                "main",
                "origin/main",
                "main",
                if json { "true" } else { "false" },
            ])
            .env("LOOM_BRANCH_REUSE_RETIRED_LIB", scripts().join("lib/branch-landed.sh"))
            .env("PATH", path(fx))
            .current_dir(&fx.repo),
    )
    .output()
    .unwrap()
}

fn port(fx: &Fx, branch: &str, json: bool) -> Output {
    let mut c = Command::new(bin());
    c.arg("worktree-branch-reuse").args([
        format!("--repo={}", fx.repo.display()),
        format!("--branch={branch}"),
        "--issue=42".to_string(),
        "--default-branch=main".to_string(),
        "--base-ref=origin/main".to_string(),
        "--base-display=main".to_string(),
        format!("--json-output={}", if json { "true" } else { "false" }),
    ]);
    hermetic(c.env("PATH", path(fx)).current_dir(&fx.repo))
        .output()
        .unwrap()
}

/// Message lines from either side: ANSI stripped, icon/`ERROR:` prefix
/// stripped, both streams merged in the order each produced them (stdout then
/// stderr — the two sides never interleave within one stream).
fn msgs(o: &Output) -> Vec<String> {
    let mut all = String::from_utf8_lossy(&o.stdout).into_owned();
    all.push_str(&String::from_utf8_lossy(&o.stderr));
    all.lines()
        .map(strip_ansi)
        .filter_map(|l| {
            for p in ["⚠ ", "ℹ ", "✓ ", "ERROR: "] {
                if let Some(rest) = l.strip_prefix(p) {
                    return Some(rest.to_string());
                }
            }
            None
        })
        .collect()
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The first line of stdout that parses as a JSON object, if any.
fn json_doc(o: &Output) -> Option<String> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .map(str::to_string)
}

const BRANCH: &str = "feature/issue-42";

#[test]
fn port_agrees_with_the_retired_shell_on_every_case() {
    for forge in [Forge::MergedAtTip, Forge::None, Forge::Off] {
        for advance_main in [true, false] {
            for json in [false, true] {
                let a = fixture("sh", BRANCH, advance_main, forge);
                let b = fixture("rs", BRANCH, advance_main, forge);
                let (s, p) = (shell(&a, BRANCH, json), port(&b, BRANCH, json));
                let ctx = format!("forge={forge:?} advance_main={advance_main} json={json}");
                assert_eq!(s.status.code(), p.status.code(), "exit for {ctx}");
                assert_eq!(msgs(&s), msgs(&p), "messages for {ctx}");
                if json && s.status.code() != Some(0) {
                    // Compared as VALUES: the shell spaced its hand-built
                    // document (`"a": b`), serde emits it compact.
                    let parse = |t: &str| serde_json::from_str::<serde_json::Value>(t).unwrap();
                    assert_eq!(
                        parse(&json_doc(&s).expect("shell refusal document")),
                        parse(&json_doc(&p).expect("port refusal document")),
                        "json doc for {ctx}"
                    );
                } else if json {
                    assert!(json_doc(&p).is_none(), "no document on success: {ctx}");
                }
            }
        }
    }
}

#[test]
fn a_degenerate_tip_is_reused_by_both_sides() {
    // A local branch pointing exactly at origin/main is trivially its own
    // ancestor, so the ancestry rung answers `landed`. Refusing it would break
    // every ordinary `worktree.sh <N>` re-run — both sides must reuse.
    let a = fixture("shdeg", BRANCH, false, Forge::MergedAtTip);
    let b = fixture("rsdeg", BRANCH, false, Forge::MergedAtTip);
    for fx in [&a, &b] {
        git(&fx.repo, &["branch", "-f", BRANCH, "origin/main"]);
    }
    let (s, p) = (shell(&a, BRANCH, false), port(&b, BRANCH, false));
    assert_eq!(s.status.code(), Some(0), "shell refused the degenerate tip");
    assert_eq!(p.status.code(), Some(0), "port refused the degenerate tip");
    assert_eq!(msgs(&s), msgs(&p));
}

#[test]
fn hand_spliced_json_was_invalid_and_the_port_escapes_it() {
    // DELIBERATE divergence. `git check-ref-format` permits `"` in a refname,
    // and $BRANCH_NAME is `feature/$CUSTOM_BRANCH` — operator input.
    let branch = "feature/a\"b";
    let a = fixture("shq", branch, true, Forge::MergedAtTip);
    let b = fixture("rsq", branch, true, Forge::MergedAtTip);
    let s = shell(&a, branch, true);
    assert_eq!(s.status.code(), Some(1), "fixture must reach the refusal");
    let sdoc = json_doc(&s).expect("shell emitted a document");
    assert!(
        serde_json::from_str::<serde_json::Value>(&sdoc).is_err(),
        "the retired splice was supposed to be invalid JSON here: {sdoc}"
    );

    let p = port(&b, branch, true);
    assert_eq!(p.status.code(), Some(1));
    let v: serde_json::Value =
        serde_json::from_str(&json_doc(&p).expect("port emitted a document")).unwrap();
    assert_eq!(v["branch"], branch);
    assert_eq!(v["error"], "branch-already-landed");
    assert_eq!(v["issueNumber"], 42);
    assert_eq!(v["prNumber"], 999);
}

#[test]
fn live_worktree_sh_refuses_an_already_landed_local_branch() {
    // End to end through the one-line stub: the refusal must reach the
    // caller's exit code and leave nothing behind.
    let fx = fixture("live", BRANCH, true, Forge::MergedAtTip);
    fs::create_dir_all(fx.repo.join(".loom/scripts")).unwrap();
    let out = hermetic(
        Command::new("bash")
            .arg(scripts().join("worktree.sh"))
            .arg("42")
            .env("LOOM_DAEMON_SELF_BIN", bin())
            .env("PATH", path(&fx))
            .current_dir(&fx.repo),
    )
    .output()
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("already-merged PR #999"), "{text}");
    assert!(
        !fx.repo.join(".loom/worktrees/issue-42").exists(),
        "a worktree was created on an already-landed branch: {text}"
    );
    // The refusal is not destructive.
    assert!(!rev(&fx.repo, BRANCH).is_empty(), "the branch was deleted");
}

#[test]
fn live_worktree_sh_reuses_a_branch_the_forge_cannot_answer_for() {
    // The fail-open direction, end to end: a forge outage still yields a
    // worktree carrying the branch's work.
    let fx = fixture("livefo", BRANCH, true, Forge::Off);
    let out = hermetic(
        Command::new("bash")
            .arg(scripts().join("worktree.sh"))
            .arg("42")
            .env("LOOM_DAEMON_SELF_BIN", bin())
            .env("PATH", path(&fx))
            .current_dir(&fx.repo),
    )
    .output()
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        fx.repo.join(".loom/worktrees/issue-42/slice.txt").exists(),
        "the reused branch's work is missing: {text}"
    );
    assert!(text.contains("already exists - reusing it"), "{text}");
    assert!(text.contains("has diverged from main"), "{text}");
}
