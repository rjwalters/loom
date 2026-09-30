//! Differential test: `loom-daemon worktree-base` against the shell it replaced
//! (#8195 slice 13, epic #7810), on one shared fixture per side.
//!
//! Compared for every (base branch, `--json`) case: exit code, the resolved
//! `BASE_REF` / `BASE_DISPLAY`, and the non-error message lines (ANSI-free,
//! prefix-stripped). Two DELIBERATE divergences are asserted as such: the
//! unsafe-name refusal text (the port reuses `refname`'s message) and the
//! `--json` refusal document, which the retired shell spliced by hand and which
//! is invalid JSON for a name containing a quote.
//!
//! Then the live `worktree.sh` is run end to end with the port pinned via
//! `LOOM_DAEMON_SELF_BIN`, to prove the stub replays the records correctly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}
fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts")
}
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worktree-base-retired.sh")
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

/// A repo with a bare origin: `main`, a pushed `feature/issue-1`, a local-only
/// `feature/issue-2`. Built identically per call.
fn side(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("loom-wt-base-diff-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("re po");
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
    git(&repo, &["branch", "feature/issue-1"]);
    git(&repo, &["push", "-q", "origin", "feature/issue-1"]);
    git(&repo, &["branch", "feature/issue-2"]);
    repo
}

const CASES: &[&str] = &[
    "",
    "feature/issue-1",
    "feature/issue-2",
    "feature/issue-9",
    "--upload-pack=/tmp/x",
    "a\"b",
];

fn shell(repo: &Path, base: &str, json: bool) -> Output {
    hermetic(
        Command::new("bash")
            .arg(fixture())
            .args(["main", base, if json { "true" } else { "false" }])
            .env("LOOM_BASE_RETIRED_LIB", scripts().join("lib/default-branch.sh"))
            .current_dir(repo),
    )
    .output()
    .unwrap()
}

fn port(repo: &Path, base: &str, json: bool) -> Output {
    let mut c = Command::new(bin());
    c.arg("worktree-base").args([
        "--default-branch=main".to_string(),
        format!("--base-branch={base}"),
    ]);
    if json {
        c.arg("--quiet");
    }
    hermetic(c.current_dir(repo)).output().unwrap()
}

/// Records from the port, as (token, text).
fn records(o: &Output) -> Vec<(String, String)> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn rec(rs: &[(String, String)], tok: &str) -> Option<String> {
    rs.iter().find(|(t, _)| t == tok).map(|(_, v)| v.clone())
}

fn shell_kv(o: &Output, key: &str) -> Option<String> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Non-error message lines of the shell, prefix-stripped.
fn shell_msgs(o: &Output) -> Vec<String> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            ["ℹ ", "✓ ", "⚠ "]
                .iter()
                .find_map(|p| l.strip_prefix(p))
                .map(str::to_string)
        })
        .collect()
}

#[test]
fn port_agrees_with_the_retired_shell_on_every_case() {
    for base in CASES {
        for json in [false, true] {
            let (a, b) = (side("sh"), side("rs"));
            let (s, p) = (shell(&a, base, json), port(&b, base, json));
            assert_eq!(s.status.code(), p.status.code(), "exit for {base:?} json={json}");
            let rs = records(&p);
            let unsafe_name = base.starts_with('-') || base.contains('"');
            if s.status.code() == Some(0) {
                assert_eq!(shell_kv(&s, "BASE_REF"), rec(&rs, "BASE_REF"), "{base:?}");
                assert_eq!(shell_kv(&s, "BASE_DISPLAY"), rec(&rs, "BASE_DISPLAY"), "{base:?}");
            } else {
                assert!(rec(&rs, "BASE_REF").is_none(), "refusal must not name a base");
            }
            if !unsafe_name {
                let port_msgs: Vec<String> = rs
                    .iter()
                    .filter(|(t, _)| matches!(t.as_str(), "INFO" | "SUCCESS" | "WARNING"))
                    .map(|(_, m)| m.clone())
                    .collect();
                assert_eq!(shell_msgs(&s), port_msgs, "messages for {base:?} json={json}");
                if json && s.status.code() != Some(0) {
                    let sj = String::from_utf8_lossy(&s.stdout);
                    // Compared as values: the shell spaced its hand-built document
                    // (`"a": b`), serde emits it compact; consumers parse it.
                    let parse = |t: &str| serde_json::from_str::<serde_json::Value>(t).unwrap();
                    assert_eq!(
                        parse(sj.trim()),
                        parse(&rec(&rs, "JSON").unwrap()),
                        "json doc for {base:?}"
                    );
                }
            }
            let _ = fs::remove_dir_all(a.parent().unwrap());
            let _ = fs::remove_dir_all(b.parent().unwrap());
        }
    }
}

#[test]
fn hand_spliced_json_was_invalid_and_the_port_escapes_it() {
    // DELIBERATE divergence: the retired shell's refusal document for a name
    // holding a quote is not JSON; the port's always parses and round-trips.
    let (a, b) = (side("shj"), side("rsj"));
    let s = shell(&a, "a\"b", true);
    let doc = String::from_utf8_lossy(&s.stdout);
    assert!(serde_json::from_str::<serde_json::Value>(doc.lines().next().unwrap()).is_err());
    let p = port(&b, "a\"b", true);
    let v: serde_json::Value = serde_json::from_str(&rec(&records(&p), "JSON").unwrap()).unwrap();
    assert_eq!(v["baseBranch"], "a\"b");
    let _ = fs::remove_dir_all(a.parent().unwrap());
    let _ = fs::remove_dir_all(b.parent().unwrap());
}

#[test]
fn live_worktree_sh_replays_the_records_end_to_end() {
    // --base on a pushed parent: the new worktree's branch must start from it.
    let repo = side("live");
    let out = hermetic(
        Command::new("bash")
            .arg(scripts().join("worktree.sh"))
            .args(["77", "--base", "feature/issue-1"])
            .env("LOOM_DAEMON_SELF_BIN", bin())
            .current_dir(&repo),
    )
    .output()
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Stacked worktree base: origin/feature/issue-1"), "{text}");
    assert!(
        repo.join(".loom/worktrees/issue-77").exists(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A missing base refuses with exit 1 and creates nothing.
    let out = hermetic(
        Command::new("bash")
            .arg(scripts().join("worktree.sh"))
            .args(["78", "--base", "feature/issue-404"])
            .env("LOOM_DAEMON_SELF_BIN", bin())
            .current_dir(&repo),
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!repo.join(".loom/worktrees/issue-78").exists());
    let _ = fs::remove_dir_all(repo.parent().unwrap());
}
