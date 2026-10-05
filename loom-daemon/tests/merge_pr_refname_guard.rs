//! Issue #9479: the two `merge_pr` verbs `merge-pr.sh` calls with a PR's
//! `headRefName` must refuse a switch-shaped ref name **before any git process
//! starts**, exactly as `reconcile_stack::plan` already does (#9106/#9474).
//!
//! # Why the daemon repeats a check the shell already makes
//!
//! Since #9474, `merge-pr.sh` runs `check_branch_name "$PR_BRANCH"` before it
//! shells out to `loom-daemon merge-pr stacked-children` / `version-policy`.
//! That is a *version-coupled* guarantee: an installed `.loom/scripts/` that
//! predates #9474 paired with a newer `loom-daemon` — the ordinary state of a
//! host between a `git pull` and a `resync-installed.sh` — hands the
//! unvalidated `.head.ref` straight into these argvs. The daemon side must not
//! depend on its shell caller having been resynced, so it fails closed on its
//! own.
//!
//! # How "no git process ran" is proved
//!
//! Two independent ways, because each alone is weaker than it looks:
//!
//! 1. **By verdict.** On a directory that is not a git repository, every git
//!    command fails, so a well-formed request has exactly one possible
//!    outcome (`Pass` / `ObjectUnreadable`). A *different*, named outcome can
//!    only mean the guard fired before the first git call. This is
//!    `reconcile_stack_refname_guard.rs`'s baseline trick.
//! 2. **By payload.** On a real repository with a path origin, the unguarded
//!    `git fetch origin '--upload-pack=<path>'` EXECUTES `<path>`. The
//!    payload only `touch`es a marker inside the same tempdir; if the marker
//!    appears, a git process ran with the attacker's name.
//!
//! Everything is hermetic: `tempfile::tempdir()`, a path-based bare origin,
//! nothing touching the network, a forge, `gh`, or any path outside the
//! fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::stacked_children::{
    establish_pin, invalid_ref_message as pin_invalid_ref_message, try_establish_pin, PinRefusal,
};
use loom_daemon::merge_pr::version_policy::{evaluate, invalid_ref_message, Inputs, Verdict};
use loom_daemon::refname::check_refname;

/// The audit's payload name: `--upload-pack=<path>` makes `git fetch` exec
/// `<path>` as the remote's upload-pack when the origin is a local path.
fn payload_ref(payload: &Path) -> String {
    format!("--upload-pack={}", payload.display())
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git must spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A clone whose `origin` is a bare repo reachable by PATH — the high-severity
/// `--upload-pack` case — plus an executable payload that records having run.
struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    payload: PathBuf,
    marker: PathBuf,
}

impl Fixture {
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
            payload,
            marker,
        }
    }

    fn payload_ran(&self) -> bool {
        self.marker.exists()
    }
}

/// The names worth driving: one per documented consequence of #9106.
const UNSAFE_NAMES: [(&str, &str); 4] = [
    ("--upload-pack (RCE on a path origin)", "--upload-pack=/tmp/payload"),
    ("--depth (silent shallow-ification)", "--depth=1"),
    ("a bare switch", "-d"),
    ("'=' without a leading dash", "a/=b"),
];

// ───────────────────────────────────────────────────────────────────────────
// merge_pr::stacked_children::establish_pin
// ───────────────────────────────────────────────────────────────────────────

/// Baseline: with a well-formed branch name, a non-repo directory gets as far
/// as git and fails there — `ObjectUnreadable`. Any OTHER verdict below is
/// therefore proof that no git command ran.
#[test]
fn establish_pin_baseline_reaches_git_with_a_valid_name() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        try_establish_pin(tmp.path(), "feature/issue-42", "deadbeef").unwrap_err(),
        PinRefusal::ObjectUnreadable,
        "baseline: a valid name must reach the cat-file/fetch and fail there"
    );
}

#[test]
fn establish_pin_refuses_an_unsafe_branch_before_any_git_runs() {
    let tmp = tempfile::tempdir().unwrap();
    for (what, name) in UNSAFE_NAMES {
        let err = try_establish_pin(tmp.path(), name, "deadbeef")
            .expect_err(&format!("establish_pin must refuse: {what}"));
        match &err {
            PinRefusal::InvalidRef(e) => {
                assert_eq!(e.name, name, "{what}: the refusal must name the offending ref");
                assert!(
                    err.to_string().contains(name),
                    "{what}: the refusal text must quote the ref: {err}"
                );
                assert!(
                    err.to_string().contains("Nothing was fetched"),
                    "{what}: the refusal must state that nothing was mutated: {err}"
                );
            }
            other => panic!(
                "{what}: expected INVALID-REF (which proves the refusal preceded the fetch), \
                 got {other:?}"
            ),
        }
        assert_eq!(err.token(), "INVALID-REF");
        assert!(
            !establish_pin(tmp.path(), name, "deadbeef"),
            "{what}: the boolean form must agree with the named one"
        );
    }
}

/// Both refusals block the merge, so this is about the INSTRUCTION, not the
/// gate: the pre-existing `blocked_message` blames "a detached or unreadable
/// parent" and offers `--allow-stacked-children`, and following either of
/// those on an unsafe ref operand wastes an operator's time — the name has to
/// be changed. The INVALID-REF text must therefore say so and must NOT point
/// at the bypass.
#[test]
fn the_pin_refusal_text_does_not_send_the_operator_to_the_bypass() {
    let name = "--upload-pack=/tmp/payload";
    let err = check_refname(name).expect_err("premise: the validator rejects this");
    let text = pin_invalid_ref_message("7", &err);

    assert!(text.contains(name), "must quote the offending ref: {text}");
    assert!(text.contains("#9106"), "must cite the issue: {text}");
    assert!(
        text.contains("Nothing was fetched") && text.contains("no pin ref was written"),
        "must state that nothing was mutated: {text}"
    );
    assert!(
        text.contains("--allow-stacked-children will not help"),
        "must say the bypass is not the fix, not merely omit it: {text}"
    );
    assert!(
        text.contains("renamed"),
        "must name the actual remedy (rename the branch): {text}"
    );
}

/// The same guard on a REAL repository with a path origin, where an unguarded
/// `establish_pin` would have reached `git fetch origin <payload-ref>`.
#[test]
fn establish_pin_does_not_execute_the_payload() {
    let fx = Fixture::build();
    let evil = payload_ref(&fx.payload);

    // Control: the vector reproduces on this host, so the assertion below is
    // not vacuous.
    // LOOM-REF-SCAN: vector-control — unseparated ON PURPOSE.
    let _ = Command::new("git")
        .arg("-C")
        .arg(&fx.repo)
        .args(["fetch", "origin", evil.as_str()])
        .output()
        .expect("git must spawn");
    assert!(
        fx.payload_ran(),
        "control failed: `git fetch origin {evil}` did not execute the payload, so this \
         environment cannot prove the guard does anything"
    );
    std::fs::remove_file(&fx.marker).unwrap();

    // `deadbeef` is absent, so an unguarded call would take the fetch arm.
    let err = try_establish_pin(&fx.repo, &evil, "deadbeef").expect_err("must refuse");
    assert!(matches!(err, PinRefusal::InvalidRef(_)), "{err}");
    assert!(
        !fx.payload_ran(),
        "establish_pin must refuse BEFORE any git process could exec the payload"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// merge_pr::version_policy::evaluate
// ───────────────────────────────────────────────────────────────────────────

/// `evaluate` skips outright unless an executable checker exists at
/// `<repo_root>/defaults/scripts/check-defaults-version-bump.sh`, so the
/// fixture plants one. It is never invoked by these tests: the fetch (or the
/// guard) decides first.
fn repo_root_with_checker(dir: &Path) {
    let checker = dir.join("defaults/scripts/check-defaults-version-bump.sh");
    std::fs::create_dir_all(checker.parent().unwrap()).unwrap();
    std::fs::write(&checker, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&checker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn inputs<'a>(repo_root: &'a Path, default_branch: &'a str, branch: &'a str) -> Inputs<'a> {
    Inputs {
        repo_root,
        default_branch,
        branch,
        head_sha: "deadbeef",
        pr_number: "1",
        dry_run: false,
    }
}

/// Baseline: with both names well formed, the fetch against a non-repo
/// directory fails and the guard's best-effort contract yields `Pass`. So a
/// non-`Pass` verdict below can only come from the pre-fetch gate.
#[test]
fn version_policy_baseline_reaches_the_fetch_and_passes() {
    let tmp = tempfile::tempdir().unwrap();
    repo_root_with_checker(tmp.path());
    let report = evaluate(&inputs(tmp.path(), "main", "feature/issue-42"));
    assert_eq!(
        report.verdict,
        Verdict::Pass,
        "baseline: a failed fetch skips (the guard is best-effort) — so any other verdict \
         means no git ran"
    );
}

#[test]
fn version_policy_refuses_an_unsafe_name_before_any_git_runs() {
    let tmp = tempfile::tempdir().unwrap();
    repo_root_with_checker(tmp.path());

    for (what, name) in UNSAFE_NAMES {
        // Once per field that reaches the fetch argv.
        for (field, default_branch, branch) in [
            ("branch ($PR_BRANCH)", "main", name),
            ("default_branch ($DEFAULT_BRANCH_NAME)", name, "feature/issue-42"),
        ] {
            let report = evaluate(&inputs(tmp.path(), default_branch, branch));
            let Verdict::InvalidRef(e) = &report.verdict else {
                panic!("{what} / {field}: expected INVALID-REF, got {:?}", report.verdict);
            };
            assert_eq!(e.name, name, "{what} / {field}: the refusal must name the ref");
            assert!(
                report.warnings.is_empty(),
                "{what} / {field}: a refusal is not a warning-and-continue"
            );

            let text = invalid_ref_message("1", e);
            assert!(text.contains(name), "the message must quote the ref: {text}");
            assert!(text.contains("#9106"), "the message must cite the issue: {text}");
            assert!(
                text.contains("Nothing was fetched"),
                "the message must state that nothing was mutated: {text}"
            );
        }
    }
}

/// `--dry-run` previews a would-be version-policy block. It does NOT soften
/// this refusal: nothing was compared, so there is no preview to give, and a
/// dry run that reported "would pass" on an unmergeable name would be lying.
#[test]
fn version_policy_refuses_under_dry_run_too() {
    let tmp = tempfile::tempdir().unwrap();
    repo_root_with_checker(tmp.path());
    let mut i = inputs(tmp.path(), "main", "--upload-pack=/tmp/payload");
    i.dry_run = true;
    assert!(
        matches!(evaluate(&i).verdict, Verdict::InvalidRef(_)),
        "--dry-run must not downgrade an INVALID-REF refusal"
    );
}

/// An empty name is still a SKIP, not a refusal — `merge-pr.sh` passes empty
/// strings for "unknown" and the guard has always skipped on them. The
/// validator rejects `""`, so the ordering of the two gates is load-bearing.
#[test]
fn version_policy_still_skips_on_an_empty_name() {
    let tmp = tempfile::tempdir().unwrap();
    repo_root_with_checker(tmp.path());
    assert!(check_refname("").is_err(), "premise: the validator rejects an empty name");
    for (default_branch, branch) in [("", "feature/issue-42"), ("main", "")] {
        assert_eq!(
            evaluate(&inputs(tmp.path(), default_branch, branch)).verdict,
            Verdict::Pass,
            "an empty name must skip (Pass), not refuse"
        );
    }
}

/// And the end-to-end proof on a real path origin: no payload, ever.
#[test]
fn version_policy_does_not_execute_the_payload() {
    let fx = Fixture::build();
    repo_root_with_checker(&fx.repo);
    let evil = payload_ref(&fx.payload);

    let report = evaluate(&inputs(&fx.repo, "main", &evil));
    assert!(matches!(report.verdict, Verdict::InvalidRef(_)), "{:?}", report.verdict);
    assert!(
        !fx.payload_ran(),
        "evaluate() must refuse BEFORE any git process could exec the payload"
    );
}
