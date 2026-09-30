//! Issue #9106: a forge-derived ref that git would parse as a **switch** must
//! be refused by `reconcile_stack::plan` *before any git process starts* — not
//! merely survived by the argv.
//!
//! # Why an argv is not the mitigation
//!
//! [`std::process::Command`] blocks *shell* injection: no word splitting, no
//! globbing, no `;`. It does **not** block *git-option* injection. `git rebase
//! --onto <commit> <upstream> <branch>` receives `<branch>` as `argv[n]`, and
//! if that string begins with `-`, git's own option parser consumes it as a
//! switch. The name never has to survive a shell to do damage.
//!
//! The three tests below are, in order: the premise (git really does accept
//! such a ref), the vector (it really does execute code on a path origin), and
//! the guard (`plan` refuses with a named blocker, having run no git at all).
//!
//! Everything is hermetic: `tempfile::tempdir()`, a path-based bare origin, a
//! payload that only `touch`es a marker inside that same temp dir. Nothing
//! touches the network, a forge, `gh`, or any path outside the fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::reconcile_stack::{self, PlanRequest, Prerequisite};
use loom_daemon::refname::check_refname;

/// The audit's payload name: `--upload-pack=<path>` makes `git fetch` exec
/// `<path>` as the remote's upload-pack when the origin is a local path.
fn upload_pack_payload_ref(payload: &Path) -> String {
    format!("--upload-pack={}", payload.display())
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git must spawn")
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A bare origin reachable by **path** (the high-severity case: local mirrors,
/// CI checkouts, self-hosted repos on a shared filesystem), a clone of it, and
/// an executable payload that records having run.
struct PathOriginFixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    origin: PathBuf,
    payload: PathBuf,
    marker: PathBuf,
}

impl PathOriginFixture {
    fn build() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let repo = tmp.path().join("repo");
        let payload = tmp.path().join("payload.sh");
        let marker = tmp.path().join("PAYLOAD-EXECUTED");

        git(tmp.path(), &["init", "-q", "--bare", origin.to_str().unwrap()]);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "loom@example.com"]);
        git(&repo, &["config", "user.name", "Loom Test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "-q", "-u", "origin", "main"]);

        std::fs::write(&payload, format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        Self {
            _tmp: tmp,
            repo,
            origin,
            payload,
            marker,
        }
    }

    fn payload_ran(&self) -> bool {
        self.marker.exists()
    }
}

/// **Premise.** `git check-ref-format` — the validator a ref actually has to
/// pass to exist — ACCEPTS a leading-dash name. Only the `--branch`
/// convenience form rejects it, and nothing in this codebase called that.
///
/// If this ever stops holding, the vector below closes upstream and this
/// module's urgency drops; the guard stays either way, because it is also the
/// thing that keeps the two halves (shell + Rust) agreeing.
#[test]
fn git_accepts_a_switch_shaped_ref_name() {
    let fx = PathOriginFixture::build();
    let dangerous = "refs/heads/--upload-pack=/tmp/x";

    let plain = git_out(&fx.repo, &["check-ref-format", dangerous]);
    assert!(
        plain.status.success(),
        "premise of #9106: `git check-ref-format {dangerous}` is expected to ACCEPT \
         (that is why such a branch can exist on a forge at all)"
    );

    // ...and the local ref really can be created under that name.
    let head = String::from_utf8_lossy(&git_out(&fx.repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let created = git_out(&fx.repo, &["update-ref", dangerous, &head]);
    assert!(
        created.status.success(),
        "premise of #9106: a switch-shaped branch ref can be created: {}",
        String::from_utf8_lossy(&created.stderr)
    );
}

/// **Vector.** Unguarded, the name is not data — it is a switch, and on a
/// path origin `--upload-pack=` is arbitrary code execution as the merging
/// user. This runs the real exploit against the fixture (the "payload" only
/// touches a file inside the same tempdir) so the regression test asserts an
/// observed fact rather than a remembered one, then shows the two independent
/// mitigations closing it.
#[test]
fn upload_pack_payload_executes_without_the_guard_and_not_with_it() {
    let fx = PathOriginFixture::build();
    let evil = upload_pack_payload_ref(&fx.payload);
    let origin = fx.origin.to_str().unwrap().to_string();

    // 1. Unguarded: the payload executes.
    let _ = git_out(&fx.repo, &["fetch", &origin, &evil, "main"]);
    assert!(
        fx.payload_ran(),
        "the #9106 vector must reproduce here, or this test is proving nothing: \
         `git fetch <path-origin> '{evil}' main` should have executed the payload"
    );

    // 2. Mitigation A — the validator refuses the name outright.
    assert!(check_refname(&evil).is_err(), "check_refname must reject the payload ref");

    // 3. Mitigation B — `--` ends option parsing, so the same fetch treats the
    //    name as a ref and fails harmlessly. Independent of A on purpose: the
    //    guard is defence in depth, not a single point of failure.
    std::fs::remove_file(&fx.marker).unwrap();
    let guarded = git_out(&fx.repo, &["fetch", &origin, "--", &evil, "main"]);
    assert!(
        !guarded.status.success(),
        "the guarded fetch should fail with 'couldn't find remote ref', not succeed"
    );
    assert!(
        !fx.payload_ran(),
        "`--` must stop git parsing the ref as a switch: payload executed anyway"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Acceptance criterion 5: `reconcile_stack` refuses with a NAMED blocker
// instead of invoking rebase / rev-parse / merge-base on an invalid ref.
// ───────────────────────────────────────────────────────────────────────────

/// A directory that is not a git repository. Any git command `plan` runs there
/// fails, and the FIRST thing `plan` does is `git fetch` — so the baseline
/// verdict for a well-formed request is `Prerequisite::Fetch`.
///
/// That is what makes the invalid-ref assertions below proof rather than
/// inference: if a single git process had run, the verdict would be `Fetch`.
/// An `INVALID-REF` verdict can only mean the guard fired first.
fn non_repo_request<'a>(
    dir: &'a Path,
    remote: &'a str,
    default_branch: &'a str,
    child_branch: &'a str,
    parent_branch: &'a str,
) -> PlanRequest<'a> {
    PlanRequest {
        repo_dir: dir,
        remote,
        default_branch,
        child_branch,
        parent_branch,
    }
}

#[test]
fn baseline_a_well_formed_request_reaches_the_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let err = reconcile_stack::plan(&non_repo_request(
        tmp.path(),
        "origin",
        "main",
        "feature/issue-2",
        "feature/issue-1",
    ))
    .expect_err("a non-repo directory cannot be fetched from");
    assert_eq!(
        err.prerequisite,
        Prerequisite::Fetch,
        "baseline: with every ref valid, the first git call (fetch) is what refuses — \
         so any OTHER verdict below means no git ran. Got: {err}"
    );
}

#[test]
fn refuses_an_invalid_ref_before_any_git_command_runs() {
    let tmp = tempfile::tempdir().unwrap();
    // One case per field that reaches a git argv, and one payload shape per
    // documented consequence: RCE (`--upload-pack`), silent clone corruption
    // (`--depth`), external merge-driver exec (`--strategy`), bare switch.
    let cases: [(&str, &str, &str, &str, &str); 5] = [
        (
            "child_branch / --upload-pack (RCE on a path origin)",
            "origin",
            "main",
            "--upload-pack=/tmp/payload",
            "feature/issue-1",
        ),
        (
            "parent_branch / --strategy (external merge-driver exec)",
            "origin",
            "main",
            "feature/issue-2",
            "--strategy=evil",
        ),
        (
            "default_branch / --depth (silent shallow-ification)",
            "origin",
            "--depth=1",
            "feature/issue-2",
            "feature/issue-1",
        ),
        ("remote / bare switch", "-d", "main", "feature/issue-2", "feature/issue-1"),
        (
            "child_branch / '=' without a leading dash",
            "origin",
            "main",
            "a/=b",
            "feature/issue-1",
        ),
    ];

    for (what, remote, default_branch, child_branch, parent_branch) in cases {
        let err = reconcile_stack::plan(&non_repo_request(
            tmp.path(),
            remote,
            default_branch,
            child_branch,
            parent_branch,
        ))
        .expect_err(&format!("plan must refuse: {what}"));

        assert_eq!(
            err.prerequisite,
            Prerequisite::InvalidRef,
            "{what}: expected the INVALID-REF blocker (which proves the refusal \
             happened before the fetch). Got: {err}"
        );
        assert_eq!(err.prerequisite.token(), "INVALID-REF");

        // The refusal must name the offending ref so it is greppable in a
        // sweep log and quotable in a forge comment (#9106 AC4/AC5).
        let offending = [remote, default_branch, child_branch, parent_branch]
            .into_iter()
            .find(|r| check_refname(r).is_err())
            .expect("each case carries exactly one unsafe ref");
        let text = err.to_string();
        assert!(
            text.contains(offending),
            "{what}: refusal must name the invalid ref {offending:?}, got: {text}"
        );
        assert!(
            text.contains("nothing was fetched, rebased, or pushed"),
            "{what}: refusal must state that nothing was mutated, got: {text}"
        );
    }
}

/// The same guard on a REAL repository, where an unguarded `plan` would have
/// got past the fetch and reached `rev-parse` / `merge-base` on the bad name.
/// Here the verdict is still `INVALID-REF`, and the payload never runs.
#[test]
fn refuses_on_a_real_path_origin_without_executing_the_payload() {
    let fx = PathOriginFixture::build();
    let evil = upload_pack_payload_ref(&fx.payload);

    let err = reconcile_stack::plan(&PlanRequest {
        repo_dir: &fx.repo,
        remote: "origin",
        default_branch: "main",
        child_branch: &evil,
        parent_branch: "feature/issue-1",
    })
    .expect_err("plan must refuse a switch-shaped child branch");

    assert_eq!(err.prerequisite, Prerequisite::InvalidRef, "{err}");
    assert!(
        !fx.payload_ran(),
        "plan() must refuse BEFORE any git process could exec the payload"
    );
}
