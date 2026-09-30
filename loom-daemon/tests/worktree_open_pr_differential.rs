//! Differential test: `loom-daemon worktree-open-pr` against the shell
//! function it now runs ahead of, `_worktree_open_pr_for_branch` in
//! `defaults/scripts/lib/worktree-forge-pr-check.sh` (#8195 slice 15, epic
//! #7810).
//!
//! # Shape
//!
//! Unlike most sibling harnesses in this family, the shell side here is NOT a
//! frozen fixture: slice 15 kept the shell body as a live fallback (the same
//! "optional, always-taken-path" shape slice 7's `acquire_worktree_lock`
//! chose), so the reference implementation this test diffs against is the
//! actual file in the working tree, invoked by sourcing it directly with
//! `_WT_DAEMON_BIN` left unset — the same state it is in before this slice's
//! delegation clause can fire.
//!
//! Both sides are driven through the SAME `PATH`, containing nothing but a
//! scripted `gh` (or, for the Gitea case, a scripted `loom-daemon`) — so a
//! divergence in the comparison is about the decision, not about which forge
//! plumbing either side happened to find.
//!
//! # What is compared
//!
//! `STATUS`, `NUMBER`, `CROSS_REPO`, `HEAD_REPO`, `HEAD_REF` and `URL` — the
//! full set of `_WT_OPEN_PR_*` globals the shell function documents, read back
//! from each side's own record stream. `URL` is compared too, even though no
//! caller refuses or reuses on it, because it is the one field the #9109
//! adversarial-value test (`test-worktree-forge-pr-check.sh` Test 9) does not
//! cover for this exact function — the JSON-escaping test drives the CALLER
//! (`_worktree_guard_fresh_branch_against_open_pr`) with `_worktree_open_pr_for_branch`
//! stubbed out, never this function's own forge-parsing path.
//!
//! # Scenarios
//!
//! Mirrors `test-worktree-forge-pr-check.sh`'s own `install_fake_gh` modes
//! (cross_repo / same_repo / none / no_host / unauth / unavailable), plus the
//! Test 8 Gitea-decline shape and the two no-forge-remote/no-remote-at-all
//! cases that skip the forge round-trip entirely.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

fn lib_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/lib/worktree-forge-pr-check.sh")
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

/// A repo with a local-only `origin` and, when `with_forge_remote`, a second
/// remote carrying a real GitHub URL (never fetched — hermetic) so the
/// `has_forge_remote` classification has something to say yes to. Mirrors
/// `setup_repo` in `test-worktree-forge-pr-check.sh`.
fn fixture(tag: &str, with_forge_remote: bool) -> Fx {
    let root =
        std::env::temp_dir().join(format!("loom-wt-openpr-diff-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("re po"); // a space: #7858's class, exercised at the process boundary
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
    if with_forge_remote {
        git(
            &repo,
            &[
                "remote",
                "add",
                "forge",
                "https://github.com/rjwalters/loom.git",
            ],
        );
    }
    let fakebin = root.join("fakebin");
    fs::create_dir_all(&fakebin).unwrap();
    Fx {
        root,
        repo,
        fakebin,
    }
}

fn install_fake_gh(fx: &Fx, mode: &str, pr_number: u32) {
    let script = format!(
        r#"#!/bin/bash
if [[ "$1" == "pr" && "$2" == "list" ]]; then
    shift 2
    branch=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --head) branch="$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    case "{mode}" in
        cross_repo)
            echo '[{{"number": {pr_number}, "isCrossRepository": true, "headRepository": {{"nameWithOwner": "forkuser/loom"}}, "headRefName": "'"$branch"'", "url": "https://github.com/rjwalters/loom/pull/{pr_number}"}}]'
            ;;
        same_repo)
            echo '[{{"number": {pr_number}, "isCrossRepository": false, "headRepository": {{"nameWithOwner": "rjwalters/loom"}}, "headRefName": "'"$branch"'", "url": "https://github.com/rjwalters/loom/pull/{pr_number}"}}]'
            ;;
        none)
            echo "[]"
            ;;
        no_host)
            echo "none of the git remotes configured for this repository point to a known GitHub host. To tell gh about a new GitHub host, please use \`gh auth login\`" >&2
            exit 1
            ;;
        unauth)
            echo "To get started with GitHub CLI, please run:  gh auth login" >&2
            exit 4
            ;;
        unavailable)
            echo "gh: API rate limit exceeded for this token" >&2
            exit 1
            ;;
    esac
    exit 0
fi
echo "fake gh: unsupported invocation: $*" >&2
exit 1
"#
    );
    let path = fx.fakebin.join("gh");
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Test 8's Gitea shape: a fake `loom-daemon` on `PATH` (ahead of the real
/// one under test — see the Rust-side runner below, which pins the real
/// binary via a positional path rather than `PATH` lookup) that declines the
/// query the way `EX_FORGE_DECLINED` does.
fn install_fake_loom_daemon_gitea_decline(fx: &Fx) {
    let script = r#"#!/bin/bash
if [[ "$1" == "forge" && "$2" == "pr" && "$3" == "list" ]]; then
    echo "loom-daemon forge: gitea is not handled natively; falling back to the caller's shell path" >&2
    exit 3
fi
echo "fake loom-daemon: unsupported invocation: $*" >&2
exit 1
"#;
    let path = fx.fakebin.join("loom-daemon");
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Answer {
    status: String,
    number: String,
    cross_repo: String,
    head_repo: String,
    head_ref: String,
    url: String,
}

fn parse_records(text: &str) -> Answer {
    let mut a = Answer::default();
    for line in text.lines() {
        let Some((token, value)) = line.split_once('\t') else {
            continue;
        };
        match token {
            "STATUS" => a.status = value.to_string(),
            "NUMBER" => a.number = value.to_string(),
            "CROSS_REPO" => a.cross_repo = value.to_string(),
            "HEAD_REPO" => a.head_repo = value.to_string(),
            "HEAD_REF" => a.head_ref = value.to_string(),
            "URL" => a.url = value.to_string(),
            _ => {}
        }
    }
    a
}

/// PATH scoped to `fx.fakebin` plus a bare `/usr/bin:/bin` for `git` itself —
/// deliberately NOT the ambient `$PATH`, which on a dev/fleet host very
/// plausibly has a REAL `loom-daemon` (or `gh`) installed. Command selection
/// inside [`loom_daemon::worktree_cli::open_pr::query`] prefers a
/// `loom-daemon` on `PATH` over `gh`, so leaking the ambient PATH through
/// here would make a "no `loom-daemon` fixture installed" scenario silently
/// shell out to the REAL machine daemon and make a REAL forge call instead of
/// exercising the fixture.
fn path(fx: &Fx) -> String {
    format!("{}:/usr/bin:/bin", fx.fakebin.display())
}

/// Run the real Rust CLI under the scoped `PATH` above.
fn run_rust(fx: &Fx, branch: &str) -> Answer {
    let out = Command::new(bin())
        .args(["worktree-open-pr", "--branch", branch, "--repo"])
        .arg(&fx.repo)
        .env("PATH", path(fx))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "worktree-open-pr exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_records(&String::from_utf8_lossy(&out.stdout))
}

/// Run the shell reference: source the live library (unmodified) and call
/// `_worktree_open_pr_for_branch` directly, with `_WT_DAEMON_BIN` unset so the
/// delegation clause never fires — this is the fallback body itself.
fn run_shell(fx: &Fx, branch: &str) -> Answer {
    let script = format!(
        r#"set -euo pipefail
cd {repo:?}
print_error() {{ :; }}
print_info() {{ :; }}
source {lib:?}
_worktree_open_pr_for_branch {branch:?}
printf 'STATUS\t%s\n' "$_WT_OPEN_PR_STATUS"
printf 'NUMBER\t%s\n' "$_WT_OPEN_PR_NUMBER"
printf 'CROSS_REPO\t%s\n' "$_WT_OPEN_PR_IS_CROSS_REPO"
printf 'HEAD_REPO\t%s\n' "$_WT_OPEN_PR_HEAD_REPO"
printf 'HEAD_REF\t%s\n' "$_WT_OPEN_PR_HEAD_REF"
printf 'URL\t%s\n' "$_WT_OPEN_PR_URL"
"#,
        repo = fx.repo,
        lib = lib_path(),
        branch = branch,
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", path(fx))
        .env_remove("_WT_DAEMON_BIN")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "shell reference failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_records(&String::from_utf8_lossy(&out.stdout))
}

fn assert_agree(fx: &Fx, branch: &str, scenario: &str) {
    let rust = run_rust(fx, branch);
    let shell = run_shell(fx, branch);
    assert_eq!(rust, shell, "scenario '{scenario}' disagreed (rust vs shell)");
}

#[test]
fn cross_repo_open_pr_agrees() {
    let fx = fixture("cross-repo", true);
    install_fake_gh(&fx, "cross_repo", 1234);
    assert_agree(&fx, "feature/issue-77", "cross_repo");
}

#[test]
fn same_repo_open_pr_agrees() {
    let fx = fixture("same-repo", true);
    install_fake_gh(&fx, "same_repo", 999);
    assert_agree(&fx, "feature/issue-77", "same_repo");
}

#[test]
fn no_matching_pr_agrees() {
    let fx = fixture("none", true);
    install_fake_gh(&fx, "none", 1);
    assert_agree(&fx, "feature/issue-77", "none");
}

#[test]
fn no_known_github_host_agrees() {
    let fx = fixture("no-host", true);
    install_fake_gh(&fx, "no_host", 1);
    assert_agree(&fx, "feature/issue-77", "no_host");
}

#[test]
fn unauthenticated_gh_agrees() {
    // The #7863 shape: unauthenticated gh bails out before it can name the
    // host reason, but with NO forge remote at all this must still be
    // no_forge_remote on both sides without ever invoking `gh`.
    let fx = fixture("unauth-no-forge-remote", false);
    install_fake_gh(&fx, "unauth", 1);
    assert_agree(&fx, "feature/issue-77", "unauth-no-forge-remote");
}

#[test]
fn genuinely_unavailable_agrees() {
    let fx = fixture("unavailable", true);
    install_fake_gh(&fx, "unavailable", 1);
    assert_agree(&fx, "feature/issue-77", "unavailable");
}

#[test]
fn local_filesystem_only_origin_agrees_without_a_query() {
    // No forge remote at all -> both sides must classify no_forge_remote
    // without ever invoking the fake gh (which would answer cross_repo if
    // consulted, so a regression that starts querying anyway is visible as a
    // disagreement, not silently skipped).
    let fx = fixture("local-only", false);
    install_fake_gh(&fx, "cross_repo", 4242);
    assert_agree(&fx, "feature/issue-77", "local-filesystem-only");
}

#[test]
fn gitea_decline_agrees() {
    let fx = fixture("gitea", true);
    install_fake_loom_daemon_gitea_decline(&fx);
    assert_agree(&fx, "feature/issue-77", "gitea-decline");
}

#[test]
fn empty_branch_agrees() {
    let fx = fixture("empty-branch", true);
    install_fake_gh(&fx, "cross_repo", 1);
    assert_agree(&fx, "", "empty-branch");
}
