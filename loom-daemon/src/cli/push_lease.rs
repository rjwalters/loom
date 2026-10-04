//! `loom-daemon push-lease pin-flag` — build a **pinned**
//! `--force-with-lease=<branch>:<oid>` argument, or refuse (#9487).
//!
//! # The bug this exists to make unexpressible
//!
//! `git push --force-with-lease` with no `=<ref>:<expect>` value compares the
//! remote head against the LOCAL remote-tracking ref
//! `refs/remotes/<remote>/<branch>`. In a Loom clone that ref is **shared by
//! every linked worktree** (`.loom/worktrees/issue-N` all point at one object
//! store and one set of remote-tracking refs), exactly like `refs/stash` is
//! shared (#4821/#5754).
//!
//! So a sibling agent that pushes and then fetches — or merely fetches —
//! fast-forwards *your* lease value to the commit *they* just published. The
//! bare lease is then satisfied by construction, your push is **accepted**, and
//! their commit is deleted with no error and no conflict signal. It compares
//! against a ref somebody else updated, not against truth. This happened live
//! on PR #9483 (2026-09-29): two Doctors on one PR, the second's bare-lease
//! push overwrote the first's already-Judge-approved commit.
//!
//! # Why the pin must be read HERE, not freshened at push time
//!
//! The expected value has to be the remote head the pushing work is actually
//! **based on**, captured at the point the caller reads the branch state it is
//! about to rewrite — before the rebase/amend. "Fetch immediately before the
//! push, then pin to the fresh value" *reintroduces* the bug: a pin read after
//! the sibling pushed **is** the sibling's commit, so the lease passes and the
//! clobber proceeds, laundered as "fresh".
//!
//! Callers therefore invoke this subcommand where they read the branch, keep
//! the printed argument, and pass it verbatim to the eventual `git push`.
//!
//! # Two independent refusals
//!
//! The pin stops a push that would overwrite a commit published *after* the pin
//! was taken. It does nothing about a commit published *before* it that this
//! checkout never incorporated — there the pin is accurate and the push still
//! deletes commits. `--local-ref` adds that second check:
//! `<oid>` must be `<local-ref>` itself or an ancestor of it.
//!
//! # Exit-code contract
//!
//! | Exit | Meaning | Output |
//! |---|---|---|
//! | `0` | Safe to publish | stdout: `--force-with-lease=<branch>:<oid>` (an empty `<oid>` when the branch does not exist on the remote yet — git reads that as "must not exist", which is the correct lease for a first push) |
//! | [`EX_UNREADABLE`] (3) | The remote could not be queried, so there is no pin. The caller must **not** fall back to the bare flag — that is the bug | stderr: `[FETCH] …` |
//! | [`EX_UNINCORPORATED`] (4) | The remote holds commits this checkout has not incorporated | stderr: `[LEASE-PIN] … has not incorporated …` |
//! | `2` | Usage error (clap) | stderr |
//!
//! There is deliberately **no** "could not tell, here is a bare flag" outcome:
//! every failure mode is a refusal, because the only safe fallback for a
//! missing pin is not pushing.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

/// The remote could not be queried, so no pin could be taken. Distinct from
/// [`EX_UNINCORPORATED`]: the question could not be asked at all.
pub(crate) const EX_UNREADABLE: i32 = 3;

/// The remote head is real, and this checkout has not incorporated it.
pub(crate) const EX_UNINCORPORATED: i32 = 4;

/// `git ls-remote`'s answer for one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveTip {
    /// The remote has the branch, at this object id.
    At(String),
    /// The remote answered, and does not have the branch.
    Absent,
}

fn git(repo: Option<&Path>, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    if let Some(dir) = repo {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    cmd
}

/// The LIVE remote head of `branch` — asked of the remote itself, never read
/// from `refs/remotes/<remote>/<branch>`, which is the shared, locally-mutable
/// ref this whole module exists to stop trusting.
///
/// # Errors
/// When `git ls-remote` cannot be run or exits non-zero (no network, no such
/// remote, auth failure). A failed query is never reported as `Absent`:
/// "the branch is not there" and "I could not ask" are different answers and
/// only the first one is safe to push against.
pub(crate) fn live_tip(repo: Option<&Path>, remote: &str, branch: &str) -> Result<LiveTip> {
    let refspec = format!("refs/heads/{branch}");
    let out = git(repo, &["ls-remote", remote, &refspec]).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git ls-remote {remote} {refspec} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
    {
        Some(oid) if !oid.is_empty() => Ok(LiveTip::At(oid.to_string())),
        _ => Ok(LiveTip::Absent),
    }
}

/// The pinned flag for `branch` at `tip`.
///
/// `LiveTip::Absent` renders the empty expected value, which git reads as
/// "this ref must not exist" — the correct lease for publishing a branch the
/// remote does not have yet.
#[must_use]
pub(crate) fn pin_flag(branch: &str, tip: &LiveTip) -> String {
    match tip {
        LiveTip::At(oid) => format!("--force-with-lease={branch}:{oid}"),
        LiveTip::Absent => format!("--force-with-lease={branch}:"),
    }
}

/// Whether `local_ref` resolves in this checkout at all.
fn ref_resolves(repo: Option<&Path>, local_ref: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", local_ref])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether this checkout has INCORPORATED `oid` into `local_ref` — `oid` is
/// `local_ref` itself or an ancestor of it.
///
/// An `oid` this clone does not even have resolves to `false`, which is the
/// correct answer: an object we do not hold is one we have not incorporated.
fn incorporated(repo: Option<&Path>, oid: &str, local_ref: &str) -> bool {
    git(repo, &["merge-base", "--is-ancestor", oid, local_ref])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `loom-daemon push-lease` — the pinned-lease family.
#[derive(clap::Subcommand)]
pub(crate) enum PushLeaseCommand {
    /// Print the pinned `--force-with-lease=<branch>:<oid>` argument for a
    /// branch, or refuse. See the module docs for the exit-code contract.
    PinFlag(PinFlagArgs),
}

impl PushLeaseCommand {
    /// Never returns normally on a refusal: exits with the documented code.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            PushLeaseCommand::PinFlag(args) => args.run(),
        }
    }
}

/// `loom-daemon push-lease pin-flag --branch <B> [--remote origin]
/// [--local-ref <REF>] [--repo <DIR>]`.
#[derive(clap::Args)]
pub(crate) struct PinFlagArgs {
    /// Branch whose remote head the lease is pinned to.
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// Remote to query (`git ls-remote`).
    #[arg(long, default_value = "origin", value_name = "REMOTE")]
    remote: String,

    /// Also require that this checkout has incorporated the remote head —
    /// usually `refs/heads/<BRANCH>`. Skipped when the ref does not resolve
    /// here (the caller's own branch-existence prerequisite reports that far
    /// more precisely).
    #[arg(long, value_name = "REF")]
    local_ref: Option<String>,

    /// Directory to run `git` in (default: the current directory).
    #[arg(long, value_name = "DIR")]
    repo: Option<PathBuf>,
}

impl PinFlagArgs {
    /// Never returns normally on a refusal: exits with the documented code.
    pub(crate) fn run(self) -> Result<()> {
        let repo = self.repo.as_deref();
        let tip = match live_tip(repo, &self.remote, &self.branch) {
            Ok(tip) => tip,
            Err(e) => {
                // `[FETCH]` / `[LEASE-PIN]` are the same greppable
                // prerequisite-token shape `reconcile_stack::Prerequisite`
                // uses, so one caller's diagnostics read alike whichever
                // subcommand refused. Unreadable remote IS the fetch-class
                // prerequisite; an unincorporated head is its own.
                eprintln!(
                    "[FETCH] push-lease: could not read {}'s live head for '{}' ({e}) — \
                     refusing to build a lease. A bare --force-with-lease here would compare \
                     against the SHARED refs/remotes/{}/{} and can silently clobber (#9487).",
                    self.remote, self.branch, self.remote, self.branch
                );
                std::process::exit(EX_UNREADABLE);
            }
        };

        if let (LiveTip::At(oid), Some(local_ref)) = (&tip, self.local_ref.as_deref()) {
            if ref_resolves(repo, local_ref) && !incorporated(repo, oid, local_ref) {
                eprintln!(
                    "[LEASE-PIN] push-lease: {}/{} is at {oid}, which '{local_ref}' has not incorporated \
                     (#9487) — someone pushed commits this checkout does not have. Refusing: \
                     rewriting and pushing from here would delete them. Run 'git fetch {} -- {}', \
                     reconcile by hand, then re-run.",
                    self.remote, self.branch, self.remote, self.branch
                );
                std::process::exit(EX_UNINCORPORATED);
            }
        }

        println!("{}", pin_flag(&self.branch, &tip));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(status.status.success(), "git {args:?} failed");
    }

    /// A bare origin with one commit on `feature/x`, plus a clone of it.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&origin).expect("mkdir");
        run(tmp.path(), &["init", "--bare", "-q", origin.to_str().unwrap()]);
        std::fs::create_dir_all(&work).expect("mkdir");
        run(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("a.txt"), "a\n").expect("write");
        run(&work, &["add", "a.txt"]);
        run(&work, &["commit", "-q", "-m", "a"]);
        run(&work, &["checkout", "-q", "-b", "feature/x"]);
        run(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run(&work, &["push", "-q", "origin", "feature/x"]);
        (tmp, origin, work)
    }

    #[test]
    fn live_tip_reads_the_remote_itself_not_the_tracking_ref() {
        let (_tmp, _origin, work) = fixture();
        let head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(["rev-parse", "feature/x"])
                .output()
                .expect("git")
                .stdout,
        )
        .expect("utf8");
        let tip = live_tip(Some(&work), "origin", "feature/x").expect("ls-remote");
        assert_eq!(tip, LiveTip::At(head.trim().to_string()));
    }

    #[test]
    fn a_branch_the_remote_does_not_have_is_absent_not_an_error() {
        let (_tmp, _origin, work) = fixture();
        let tip = live_tip(Some(&work), "origin", "feature/nope").expect("ls-remote");
        assert_eq!(tip, LiveTip::Absent);
    }

    #[test]
    fn an_unqueryable_remote_is_an_error_never_absent() {
        let (_tmp, _origin, work) = fixture();
        // A remote name that does not exist: ls-remote exits non-zero. The
        // distinction matters — Absent would render a lease that says "the ref
        // must not exist", which is exactly the clobber we refuse to build.
        assert!(live_tip(Some(&work), "no-such-remote", "feature/x").is_err());
    }

    #[test]
    fn pin_flag_renders_the_pinned_and_must_not_exist_forms() {
        assert_eq!(
            pin_flag("feature/x", &LiveTip::At("deadbeef".into())),
            "--force-with-lease=feature/x:deadbeef"
        );
        assert_eq!(pin_flag("feature/x", &LiveTip::Absent), "--force-with-lease=feature/x:");
    }

    #[test]
    fn incorporated_is_true_for_the_tip_and_its_ancestors_false_for_a_sibling() {
        let (_tmp, _origin, work) = fixture();
        let base = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("git")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_string();
        assert!(incorporated(Some(&work), &base, "refs/heads/feature/x"));

        // A commit that exists only on the remote (a sibling's push) is not
        // incorporated here.
        let other = _tmp.path().join("sibling");
        run(
            _tmp.path(),
            &[
                "clone",
                "-q",
                _origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        run(&other, &["checkout", "-q", "feature/x"]);
        std::fs::write(other.join("b.txt"), "b\n").expect("write");
        run(&other, &["add", "b.txt"]);
        run(&other, &["commit", "-q", "-m", "sibling"]);
        run(&other, &["push", "-q", "origin", "feature/x"]);
        let sibling_sha = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&other)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("git")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_string();
        assert!(!incorporated(Some(&work), &sibling_sha, "refs/heads/feature/x"));
    }

    /// `git -C dir args…`: did it succeed? (For the pushes git must REJECT.)
    fn git_ok(dir: &Path, args: &[&str]) -> bool {
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
            .status
            .success()
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", rev])
            .output()
            .expect("git");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    /// A sibling clone of `origin` pushes one commit to `feature/x`; returns its SHA.
    fn sibling_pushes(tmp: &Path, origin: &Path) -> String {
        let other = tmp.join("sibling");
        run(
            tmp,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        run(&other, &["checkout", "-q", "feature/x"]);
        std::fs::write(other.join("s.txt"), "s\n").expect("write");
        run(&other, &["add", "s.txt"]);
        run(&other, &["commit", "-q", "-m", "sibling"]);
        run(&other, &["push", "-q", "origin", "feature/x"]);
        rev_parse(&other, "HEAD")
    }

    /// Negative control for the Doctor's ancestry check (#9487 operator
    /// review): git does NOT fail closed on a pin this clone never fetched. A
    /// full-SHA pin to the sibling's commit — what a pin re-read from the forge
    /// AFTER their push looks like — is accepted and their commit overwritten.
    /// `incorporated` (`git merge-base --is-ancestor <pin> HEAD`) is the only
    /// thing that catches it, so it must run before anything is rewritten.
    #[test]
    fn a_never_fetched_full_sha_pin_is_accepted_so_the_ancestry_check_is_load_bearing() {
        let (tmp, origin, work) = fixture();
        let sibling = sibling_pushes(tmp.path(), &origin);
        assert!(
            !git_ok(&work, &["cat-file", "-e", &sibling]),
            "precondition: this clone never fetched the sibling's commit"
        );
        // The guard refuses it…
        assert!(!incorporated(Some(&work), &sibling, "HEAD"));

        // …and without the guard, git happily clobbers.
        std::fs::write(work.join("m.txt"), "m\n").expect("write");
        run(&work, &["add", "m.txt"]);
        run(&work, &["commit", "-q", "-m", "mine"]);
        let lease = format!("--force-with-lease=feature/x:{sibling}");
        assert!(
            git_ok(&work, &["push", "-q", &lease, "origin", "feature/x"]),
            "git accepts an unfetched full-SHA pin — the pin alone does not fail closed"
        );
        assert_eq!(
            live_tip(Some(&work), "origin", "feature/x").expect("ls-remote"),
            LiveTip::At(rev_parse(&work, "HEAD")),
            "the sibling's commit was overwritten"
        );
    }

    /// The Doctor re-pins to its own pushed `HEAD`. Without that, its second
    /// push in a session is rejected exactly like a foreign push would be.
    #[test]
    fn a_second_push_needs_a_re_pin_to_the_first_pushs_head() {
        let (_tmp, _origin, work) = fixture();
        let claim = rev_parse(&work, "HEAD");
        let commit = |name: &str| {
            std::fs::write(work.join(name), "x\n").expect("write");
            run(&work, &["add", name]);
            run(&work, &["commit", "-q", "--amend", "--no-edit"]);
        };

        // First push: amended (a rewrite), pinned to the claim-time head.
        commit("one.txt");
        let first = format!("--force-with-lease=feature/x:{claim}");
        assert!(git_ok(&work, &["push", "-q", &first, "origin", "feature/x"]));
        let re_pin = rev_parse(&work, "HEAD");

        // Second rewrite: the stale claim pin is now rejected…
        commit("two.txt");
        assert!(
            !git_ok(&work, &["push", "-q", &first, "origin", "feature/x"]),
            "a stale pin after your own push is rejected like a foreign push"
        );
        // …and the re-pin to the first push's HEAD goes through.
        let second = format!("--force-with-lease=feature/x:{re_pin}");
        assert!(git_ok(&work, &["push", "-q", &second, "origin", "feature/x"]));
    }

    /// Structural guard over the Doctor prompt's push recipes (#9487 operator
    /// review): the pin comes from claim time (never a post-work forge read),
    /// is ancestry-checked BEFORE the rebase, and is re-pinned after the
    /// Doctor's own push. A refactor that drops any of these fails here.
    #[test]
    fn the_doctor_prompt_pins_at_claim_time_checks_ancestry_and_re_pins() {
        let doctor = include_str!("../../../defaults/.claude/commands/loom/doctor.md");
        assert!(
            !doctor.contains("PUSH_LEASE_SHA=$(gh pr view"),
            "a pin re-read from the forge after the fix work can be a sibling's push"
        );
        assert!(doctor.contains(r#"PUSH_LEASE_SHA="${PUSH_LEASE_SHA:-${CLAIM_HEAD_SHA:?}}""#));
        let guard = r#"git merge-base --is-ancestor "$PUSH_LEASE_SHA" HEAD"#;
        let re_pin = "{ PUSH_LEASE_SHA=$(git rev-parse HEAD); CLAIM_HEAD_SHA=$PUSH_LEASE_SHA; }";
        assert!(doctor.matches(re_pin).count() >= 2, "both rewrite recipes re-pin");
        // Every fenced block that rebases onto main and pushes a pinned lease
        // checks ancestry first — except the one that pins `git rev-parse HEAD`
        // right before the rebase, where the check is a tautology.
        let mut guarded = 0;
        for block in doctor.split("```").skip(1).step_by(2) {
            let (Some(rebase), true) =
                (block.find("git rebase origin/main"), block.contains("--force-with-lease=\"$"))
            else {
                continue;
            };
            if block.contains("PUSH_LEASE_SHA=$(git rev-parse HEAD)\n") {
                continue;
            }
            let at = block
                .find(guard)
                .expect("pinned rebase recipe lacks the ancestry check");
            assert!(at < rebase, "the ancestry check must run BEFORE the rebase");
            guarded += 1;
        }
        assert!(guarded >= 2, "found {guarded} guarded recipes, expected >= 2");
        assert!(
            !doctor.contains("Re-read the real head"),
            "a missing pin means STOP, not re-reading the head"
        );
        for line in doctor
            .lines()
            .filter(|l| l.contains("git push --force-with-lease"))
        {
            assert!(line.contains("--force-with-lease=\""), "bare lease: {line}");
        }
    }

    /// PR #9966 Judge re-review: step 9's prose told the Doctor to
    /// `git rebase origin/main` BEFORE "Pin the lease". Rebasing onto a moved
    /// `main` drops the claim-time head from `HEAD`'s history, so the block's
    /// ancestry check then STOPs a legitimate push, and a conflict handed the
    /// Doctor to "PR Has Merge Conflicts" mid-rebase, where its check always
    /// fails. Step 9 must route the rebase through the Pin block, resolve a
    /// conflict in place, and the conflicts recipe must exempt a Doctor that
    /// is already mid-rebase after a passed check.
    #[test]
    fn step_9_rebases_only_inside_the_pin_block_after_its_ancestry_check() {
        let doctor = include_str!("../../../defaults/.claude/commands/loom/doctor.md");
        let start = doctor.find("9. **Commit and push**").expect("step 9");
        let len = doctor[start..].find("\n10. ").expect("step 10");
        let step9 = &doctor[start..start + len];
        let pin = step9
            .find("Pin the lease")
            .expect("step 9 names Pin the lease");
        let main = step9
            .find("origin/main")
            .expect("step 9 names the #7668 rebase");
        assert!(pin < main, "step 9 must send the Doctor to Pin the lease before any rebase");
        for line in step9.lines() {
            if line.contains("origin/main") {
                assert!(line.contains("Pin the lease"), "standalone rebase in step 9: {line}");
            }
            if line.contains("PR Has Merge Conflicts") {
                assert!(line.contains("git rebase --continue"), "mid-rebase hand-off: {line}");
            }
        }
        let guard = r#"git merge-base --is-ancestor "$PUSH_LEASE_SHA" HEAD"#;
        let conflicts = doctor.find("### PR Has Merge Conflicts").expect("recipe");
        let recipe = &doctor[conflicts..];
        let at = recipe.find(guard).expect("conflict recipe ancestry check");
        assert!(
            recipe[..at].contains("Mid-rebase") && recipe[..at].contains("git rebase --continue"),
            "the conflict recipe must exempt a Doctor already mid-rebase from its check"
        );
    }

    #[test]
    fn a_ref_that_does_not_resolve_is_reported_as_such() {
        let (_tmp, _origin, work) = fixture();
        assert!(ref_resolves(Some(&work), "refs/heads/feature/x"));
        assert!(!ref_resolves(Some(&work), "refs/heads/feature/missing"));
    }
}
