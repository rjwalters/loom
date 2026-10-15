//! `loom-daemon branch-update strategy` end to end, and the Doctor prompt's
//! merge-mode update run against the real binary (#10771).
//!
//! The policy's pure decision is unit-tested next to it
//! (`src/branch_update_tests.rs`). This file pins the parts a unit test cannot:
//! the CLI's stdout contract, the forge read through a fake `gh`, the cache,
//! and — the incident itself — that the Doctor's own "Pin the lease" block, in
//! a merge-commit-only repository, updates a conflicting PR branch with a merge
//! commit and a plain push, keeping every commit other agents pushed. The bare
//! origin refuses non-fast-forward pushes, so a force push cannot pass.
//!
//! It also scans the role prompts: a `git rebase origin/...` in a fenced block
//! must sit behind the strategy answer, and a push may never carry a bare
//! `--force-with-lease` (#10069) or `--force`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_loom-daemon");

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A scratch dir holding a fake `gh` (answers `api repos/...` with the
/// contents of `repo.json`, or fails when it is absent; logs every call) and a
/// `loom-daemon` symlink to the binary under test.
struct Tools {
    dir: tempfile::TempDir,
}

impl Tools {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("gh-calls.log");
        let answer = dir.path().join("repo.json");
        write_exec(
            &dir.path().join("gh"),
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n\
                 if [ \"$1\" = api ] && [ -f '{}' ]; then cat '{}'; exit 0; fi\n\
                 echo 'HTTP 403: Resource not accessible' >&2; exit 1\n",
                log.display(),
                answer.display(),
                answer.display()
            ),
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(BIN, dir.path().join("loom-daemon")).expect("symlink");
        Tools { dir }
    }

    fn answer(&self, json: Option<&str>) {
        let p = self.dir.path().join("repo.json");
        match json {
            Some(j) => std::fs::write(p, j).expect("write"),
            None => {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn gh_calls(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("gh-calls.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn path_env(&self) -> String {
        format!("{}:/usr/bin:/bin", self.dir.path().display())
    }

    fn apply(&self, cmd: &mut Command) {
        cmd.env("PATH", self.path_env())
            .env("LOOM_GH_BIN", self.dir.path().join("gh"))
            .env("LOOM_FORGE_TYPE", "github")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env_remove("LOOM_BRANCH_UPDATE_STRATEGY");
    }
}

/// A checkout whose origin names a GitHub repo (never contacted: `gh` is fake).
fn checkout() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    ok(dir.path(), &["init", "-q", "-b", "main"]);
    ok(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widget.git",
        ],
    );
    dir
}

fn strategy(tools: &Tools, repo: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String) {
    let mut cmd = Command::new(BIN);
    cmd.args(["branch-update", "strategy"])
        .args(args)
        .current_dir(repo);
    tools.apply(&mut cmd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("runs");
    assert!(out.status.success(), "strategy must always exit 0: {out:?}");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn auto_reads_the_forge_settings_and_fails_toward_merge() {
    let cases = [
        (Some(r#"{"allow_merge_commit":true,"allow_rebase_merge":true}"#), "rebase\n"),
        (
            Some(
                r#"{"allow_merge_commit":true,"allow_squash_merge":false,"allow_rebase_merge":false}"#,
            ),
            "merge\n",
        ),
        (Some(r#"{"allow_squash_merge":true}"#), "merge\n"),
        (Some("not json"), "merge\n"),
        (None, "merge\n"),
    ];
    for (answer, want) in cases {
        let tools = Tools::new();
        let repo = checkout();
        tools.answer(answer);
        let (out, err) = strategy(&tools, repo.path(), &[], &[]);
        assert_eq!(out, want, "answer {answer:?}: stderr {err}");
        assert!(err.starts_with("branch-update: "), "reason on stderr: {err}");
    }
}

#[test]
fn a_forge_answer_is_cached_per_run_and_no_cache_rereads() {
    let tools = Tools::new();
    let repo = checkout();
    tools.answer(Some(r#"{"allow_rebase_merge":true}"#));
    assert_eq!(strategy(&tools, repo.path(), &[], &[]).0, "rebase\n");
    tools.answer(Some(r#"{"allow_rebase_merge":false}"#));
    assert_eq!(strategy(&tools, repo.path(), &["--quiet"], &[]).0, "rebase\n");
    assert_eq!(tools.gh_calls(), 1, "the second call was answered from the cache");
    assert_eq!(strategy(&tools, repo.path(), &["--no-cache"], &[]).0, "merge\n");
    assert_eq!(tools.gh_calls(), 2);
}

#[test]
fn an_unknown_answer_is_not_cached() {
    let tools = Tools::new();
    let repo = checkout();
    tools.answer(None);
    assert_eq!(strategy(&tools, repo.path(), &[], &[]).0, "merge\n");
    tools.answer(Some(r#"{"allow_rebase_merge":true}"#));
    assert_eq!(strategy(&tools, repo.path(), &[], &[]).0, "rebase\n");
}

#[test]
fn overrides_beat_the_forge_and_an_unreadable_one_merges() {
    let tools = Tools::new();
    let repo = checkout();
    tools.answer(Some(r#"{"allow_rebase_merge":true}"#));
    let env_merge = [("LOOM_BRANCH_UPDATE_STRATEGY", "merge")];
    assert_eq!(strategy(&tools, repo.path(), &[], &env_merge).0, "merge\n");
    let env_bad = [("LOOM_BRANCH_UPDATE_STRATEGY", "squash")];
    let (out, err) = strategy(&tools, repo.path(), &[], &env_bad);
    assert_eq!(out, "merge\n");
    assert!(err.contains("failing toward merge"), "{err}");
    assert_eq!(tools.gh_calls(), 0, "an explicit override never reads the forge");

    tools.answer(Some(r#"{"allow_rebase_merge":false}"#));
    std::fs::create_dir_all(repo.path().join(".loom")).expect("mkdir");
    std::fs::write(repo.path().join(".loom/config.json"), r#"{"branchUpdateStrategy":"rebase"}"#)
        .expect("write");
    let (out, err) = strategy(&tools, repo.path(), &[], &[]);
    assert_eq!(out, "rebase\n", "{err}");
    assert!(err.contains("branchUpdateStrategy=rebase"), "{err}");
}

#[test]
fn gitea_is_not_probed_and_merges() {
    let tools = Tools::new();
    let repo = checkout();
    tools.answer(Some(r#"{"allow_rebase_merge":true}"#));
    let (out, err) = strategy(&tools, repo.path(), &[], &[("LOOM_FORGE_TYPE", "gitea")]);
    assert_eq!(out, "merge\n", "{err}");
    assert_eq!(tools.gh_calls(), 0);
}

// ---------------------------------------------------------------------------
// The Doctor prompt's merge-mode update, against the real binary.
// ---------------------------------------------------------------------------

fn doctor_prompt() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../defaults/.claude/commands/loom/doctor.md");
    std::fs::read_to_string(p).expect("doctor.md")
}

/// The fenced bash block under `heading` in the Doctor prompt, verbatim.
fn prompt_block(heading: &str) -> String {
    let doctor = doctor_prompt();
    let from = doctor.find(heading).expect("heading");
    let rest = &doctor[from..];
    let open = rest.find("```bash\n").expect("fence") + "```bash\n".len();
    let close = rest[open..].find("```").expect("close");
    rest[open..open + close].to_string()
}

/// A merge-commit-only origin (non-fast-forward pushes refused, the forge
/// answering `allow_rebase_merge=false`) where another agent already merged
/// `main` into `feature/x`, and `main` then moved with a change that conflicts
/// with the PR. Returns (tmp, origin, doctor's checkout, the other agent's
/// merge commit).
fn merge_only_fixture(tools: &Tools) -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    tools.answer(Some(
        r#"{"allow_merge_commit":true,"allow_squash_merge":false,"allow_rebase_merge":false}"#,
    ));
    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin.git");
    ok(
        tmp.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    ok(&origin, &["config", "receive.denyNonFastForwards", "true"]);
    ok(&origin, &["config", "receive.denyDeletes", "true"]);

    let agent = tmp.path().join("agent");
    ok(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            agent.to_str().unwrap(),
        ],
    );
    let commit = |dir: &Path, file: &str, body: &str, msg: &str| {
        std::fs::write(dir.join(file), body).expect("write");
        ok(dir, &["add", file]);
        ok(dir, &["commit", "-q", "-m", msg]);
    };
    commit(&agent, "a.txt", "base\n", "base");
    ok(&agent, &["push", "-q", "origin", "main"]);
    ok(&agent, &["checkout", "-q", "-b", "feature/x"]);
    commit(&agent, "f.txt", "the PR's version\n", "feature");
    ok(&agent, &["push", "-q", "-u", "origin", "feature/x"]);
    // main moves; the other agent merges it in (the commit #10771 lost).
    ok(&agent, &["checkout", "-q", "main"]);
    commit(&agent, "b.txt", "b\n", "main moves");
    ok(&agent, &["push", "-q", "origin", "main"]);
    ok(&agent, &["checkout", "-q", "feature/x"]);
    ok(
        &agent,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "other agent: merge main",
            "main",
        ],
    );
    ok(&agent, &["push", "-q", "origin", "feature/x"]);
    let their_merge = ok(&agent, &["rev-parse", "HEAD"]);
    // main moves again, conflicting with the PR.
    ok(&agent, &["checkout", "-q", "main"]);
    commit(&agent, "f.txt", "main's version\n", "main conflicts");
    ok(&agent, &["push", "-q", "origin", "main"]);

    // The Doctor's checkout, on the PR head. Its origin is a local path; the
    // fake gh answers merge-only for whatever owner/repo that parses to.
    let work = tmp.path().join("work");
    ok(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    ok(&work, &["checkout", "-q", "feature/x"]);
    (tmp, origin, work, their_merge)
}

fn bash(tools: &Tools, dir: &Path, script: &str) -> Output {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(script).current_dir(dir);
    tools.apply(&mut cmd);
    cmd.env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("bash runs")
}

fn doctor_push_script(fix: &str) -> String {
    format!(
        "set -e\nN=1\nCLAIM_HEAD_SHA=$(git rev-parse HEAD)\n{}\n{fix}\n{}",
        prompt_block("### Verify the pin"),
        prompt_block("### Pin the lease")
    )
}

#[test]
fn the_doctor_merges_a_conflicting_branch_and_pushes_without_force() {
    let tools = Tools::new();
    let (_tmp, _origin, work, their_merge) = merge_only_fixture(&tools);
    let claim = ok(&work, &["rev-parse", "HEAD"]);

    // First pass: the Doctor's fix, then the block. The merge stops on the
    // conflict, before anything is pushed.
    let fix = "echo fix > g.txt && git add g.txt && git commit -q -m 'doctor: fix'";
    let first = bash(&tools, &work, &doctor_push_script(fix));
    assert!(!first.status.success(), "a conflict must stop the block");
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(stderr.contains("branch-update: merge"), "the strategy was asked: {stderr}");
    let live = ok(&work, &["ls-remote", "origin", "refs/heads/feature/x"]);
    assert!(live.starts_with(&their_merge), "nothing pushed mid-conflict: {live}");

    // Resolve in place, finish the merge, and resume at the block.
    std::fs::write(work.join("f.txt"), "resolved\n").expect("write");
    ok(&work, &["add", "f.txt"]);
    ok(&work, &["commit", "-q", "--no-edit"]);
    let resume = format!(
        "set -e\nN=1\nCLAIM_HEAD_SHA={claim}\n{}\n{}",
        prompt_block("### Verify the pin"),
        prompt_block("### Pin the lease")
    );
    let second = bash(&tools, &work, &resume);
    assert!(
        second.status.success(),
        "resume must push: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // The PR-lease fence ran before the push (#9401). This fixture is no
    // Loom checkout, so it finds no lease evidence and lets the push through.
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("lease pr: "), "the lease fence ran: {err}");
    let head = ok(&work, &["rev-parse", "HEAD"]);
    let live = ok(&work, &["ls-remote", "origin", "refs/heads/feature/x"]);
    assert!(live.starts_with(&head), "the remote is at the Doctor's head");
    let parents = ok(&work, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3, "HEAD is a merge commit");
    let msg = ok(&work, &["log", "-1", "--format=%s"]);
    assert!(
        msg.starts_with("Merge origin/main into feature/x") && msg.contains("Doctor"),
        "the merge names its reason: {msg}"
    );
    for kept in [&their_merge, &claim] {
        assert!(
            git(&work, &["merge-base", "--is-ancestor", kept, "HEAD"])
                .status
                .success(),
            "{kept} (pushed before the Doctor ran) is still in the branch's history"
        );
    }
    assert!(
        git(&work, &["merge-base", "--is-ancestor", "origin/main", "HEAD"])
            .status
            .success(),
        "the branch now contains main"
    );
}

#[test]
fn a_sibling_push_rejects_the_plain_push_and_survives() {
    let tools = Tools::new();
    let (tmp, origin, work, _their_merge) = merge_only_fixture(&tools);
    // Make main non-conflicting for this case: the block's merge succeeds.
    let agent = tmp.path().join("agent");
    ok(&agent, &["checkout", "-q", "main"]);
    ok(&agent, &["revert", "--no-edit", "HEAD"]);
    ok(&agent, &["push", "-q", "origin", "main"]);
    // A sibling pushes to the PR after the Doctor's claim.
    let sib = tmp.path().join("sibling");
    ok(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            sib.to_str().unwrap(),
        ],
    );
    ok(&sib, &["checkout", "-q", "feature/x"]);
    std::fs::write(sib.join("s.txt"), "sibling\n").expect("write");
    ok(&sib, &["add", "s.txt"]);
    ok(&sib, &["commit", "-q", "-m", "sibling"]);
    let sibling = ok(&sib, &["rev-parse", "HEAD"]);

    let fix = format!(
        "echo fix > g.txt && git add g.txt && git commit -q -m 'doctor: fix' && \
         git -C '{}' push -q origin feature/x",
        sib.display()
    );
    let out = bash(&tools, &work, &doctor_push_script(&fix));
    assert!(!out.status.success(), "a plain push over a sibling's commit is rejected");
    let live = ok(&work, &["ls-remote", "origin", "refs/heads/feature/x"]);
    assert!(live.starts_with(&sibling), "the sibling's commit survives: {live}");
}

// ---------------------------------------------------------------------------
// Prompt scan
// ---------------------------------------------------------------------------

/// Every role prompt: inside fenced blocks, a `git rebase origin/` must be on
/// a line guarded by the strategy answer in a block that asks for it, and no
/// push may be a bare `--force-with-lease` or a `--force`/`-f` (#10771,
/// #10069). Rebase mode keeps working; an unconditional rebase cannot return.
#[test]
fn role_prompts_never_rebase_or_force_unconditionally() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../defaults/.claude/commands/loom");
    let mut problems = Vec::new();
    let mut guarded = 0;
    for entry in std::fs::read_dir(&dir).expect("prompts") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for block in text.split("```").skip(1).step_by(2) {
            for line in block.lines() {
                let code = line.trim_start().trim_start_matches('>').trim_start();
                if code.starts_with('#') {
                    continue;
                }
                if code.contains("git rebase origin/") {
                    if code.contains("\"$BRANCH_UPDATE\" = rebase")
                        && block.contains("branch-update strategy")
                    {
                        guarded += 1;
                    } else {
                        problems.push(format!("{name}: unguarded rebase: {line}"));
                    }
                }
                if code.contains("git push") {
                    let bare_lease = code.split_whitespace().any(|w| w == "--force-with-lease");
                    let force = code.split_whitespace().any(|w| w == "--force" || w == "-f");
                    if bare_lease || force {
                        problems.push(format!("{name}: unpinned force push: {line}"));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(guarded >= 6, "expected the Doctor/Judge/Builder update sites, found {guarded}");
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
