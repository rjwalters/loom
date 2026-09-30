//! "Did the reviewed tree actually change?" — the single implementation of the
//! tree-identical test both verdict-invalidation paths ask (issues #9124,
//! #9576).
//!
//! # What the question is
//!
//! A review verdict is a statement about a **tree**, not about a commit
//! identity. Any head move off the verdict's marker SHA is therefore treated as
//! invalidating by default (`#5686`; `claim_reconciliation::decide_verdict`
//! stays a pure function and guesses nothing from event shape). But a head move
//! that changes **nothing in the tree** — the `#8248` required-check-freshness
//! guard's automated `chore: re-date required checks …` commit (#8508), or any
//! empty commit — invalidates a verdict that is still, provably, about the code
//! in front of it. Clearing it buys a full extra Judge cycle and nothing else.
//!
//! [`tree_unchanged`] answers that one question against GitHub's own
//! `compare/{base}...{head}`. It is evidence, never a heuristic: no "shaped
//! like a rebase" inference is made anywhere.
//!
//! # `files: []` alone is NOT proof — `status` must be `identical`/`ahead`
//!
//! The endpoint is a **three-dot** compare: it diffs `merge-base(base, head)`
//! against `head`, not `base` against `head`. The two coincide only when the
//! merge-base *is* `base`, i.e. `status` is `"identical"` (same commit) or
//! `"ahead"` (`head` descends from `base` — the re-date shape: a tree-identical
//! commit appended on top). When `head` was moved **back** to an ancestor of
//! `base` (a force-push that drops the reviewed commits), the merge-base is
//! `head` itself and `files` is empty even though the trees differ — measured
//! on this repo, `compare/main...main~3` reports `status: "behind"`,
//! `files: 0` against a 25-file real diff (PR #9581 review). A `"diverged"`
//! head whose tree equals the merge-base has the same hole. So
//! [`tree_unchanged`] returns `Some(true)` **only** for an empty `files` array
//! *together with* `status` `identical`/`ahead`; `behind`, `diverged`, or an
//! unknown `status` never prove equality, and a response missing `status` or
//! `files` is no answer at all.
//!
//! # The kill switch lives here too
//!
//! [`VERDICT_TREE_CARVEOUT_ENABLED_ENV`] (`LOOM_VERDICT_TREE_CARVEOUT`) turns
//! the carve-out off. Both callers read it from this one module — the daemon
//! pass through [`verdict_tree_carveout_enabled`], the shell guard because
//! [`handle`] checks it before asking anything — so switching it off cannot
//! leave one path still applying the exemption (PR #9581 review).
//!
//! # Why it lives here and not in the pass that first needed it
//!
//! #9124 landed the test inside
//! `claim_reconciliation::verdict_invalidation`, reachable only from the
//! daemon's periodic `reconcile_pr_verdicts` tick. The agent-side fast path —
//! `defaults/scripts/verdict-staleness-guard.sh`, invoked at step 0 of every
//! Judge pass, by Doctor before claiming, by Champion's merge janitor and by
//! Mode C's pre-merge check — had no tree comparison at all, so it kept
//! clearing verdicts the daemon would have kept. Both PR #9541 and PR #9483
//! lost a verdict that way to the very re-date commit #9124 was written about,
//! on a host already running the #9124 code (#9576).
//!
//! So the test moved out to a module both callers can share, with a thin CLI
//! verb in front of it:
//!
//! - in-process — `claim_reconciliation::verdict_invalidation::handle_invalidate`
//!   calls [`tree_unchanged`] directly, exactly as before;
//! - out-of-process — `loom-daemon forge tree-unchanged <base> <head>` ([`handle`])
//!   is what the shell guard shells out to, the same delegation shape
//!   `forge disable-auto-merge` (#8900) already uses for the auto-merge disarm.
//!
//! There is deliberately **no** second copy of the comparison in shell. Adding
//! one is what `.loom/docs/shell-language-policy.md` forbids and what the
//! shell-budget ratchet refuses, and it is how the two paths came to disagree
//! in the first place.
//!
//! # Fail closed, in both directions
//!
//! Every answer other than a *positive* proof of equality means "invalidate as
//! before":
//!
//! | Situation | [`tree_unchanged`] | CLI |
//! |---|---|---|
//! | `files: []` and `status` `identical`/`ahead` | `Some(true)` | `TREE_UNCHANGED=1`, exit 0 |
//! | non-empty `files`, or `status` `behind`/`diverged`/unknown | `Some(false)` | `TREE_UNCHANGED=0`, exit 0 |
//! | `gh` failed, unparseable JSON, missing `status`/`files`, unknown ref, non-GitHub forge | `None` | exit 1, nothing on stdout |
//! | carve-out switched off (`LOOM_VERDICT_TREE_CARVEOUT=0`) | not asked | exit 1, nothing on stdout, no `gh` call |
//!
//! The `None` arm is the pre-#9124 behavior, so an unavailable comparison can
//! only ever cost a redundant re-review — never a verdict kept on a tree nobody
//! read.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde::Deserialize;

/// Env kill switch for the tree-identical carve-out (Issues #9124, #9576).
/// Defaults to ON: the carve-out can only ever *reduce* exposure relative to
/// invalidating on every head move, because it fires only on a positive proof
/// of equality and fails closed whenever that proof is unavailable.
/// `0`/`false`/`no`/`off` disables it on **both** paths — the daemon's periodic
/// pass (nested inside `LOOM_VERDICT_STALENESS_RECONCILE`) and the shell
/// guard's `forge tree-unchanged` call — restoring invalidate-on-every-move.
pub const VERDICT_TREE_CARVEOUT_ENABLED_ENV: &str = "LOOM_VERDICT_TREE_CARVEOUT";

/// Is the tree-identical carve-out enabled? See
/// [`VERDICT_TREE_CARVEOUT_ENABLED_ENV`].
#[must_use]
pub fn verdict_tree_carveout_enabled() -> bool {
    match std::env::var(VERDICT_TREE_CARVEOUT_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Is `s` a plausible commit SHA — 7-40 lowercase hex digits, the same shape
/// `verdict-staleness-guard.sh`'s marker regex (`sha=[0-9a-f]{7,40}`) and
/// `claim_reconciliation::extract_latest_verdict_sha` accept?
///
/// Both SHAs are interpolated into a `gh api` **path**, and on the shell path
/// the base SHA originates in a PR comment — untrusted external content
/// (`defaults/docs/untrusted-external-content.md`). A value carrying `/` or
/// `..` would address a different endpoint entirely, so anything that is not a
/// bare hex SHA is refused here rather than sent. Refusing reads as `None` /
/// exit 1, i.e. the fail-closed arm, never as an assumed equality.
fn is_sha(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Does `head`'s tree differ from `base`'s at all (Issue #9124)? Backed by
/// GitHub's own `compare/{base}...{head}`: equality is proven only by
/// `files: []` **and** `status` `"identical"`/`"ahead"` — see the module doc
/// for why an empty `files` under `behind`/`diverged` proves nothing.
///
/// `Some(true)` — the codebase is byte-for-byte unchanged; commonest cause
/// measured on this repo is the `#8248` required-check-freshness guard's
/// automated "re-date required checks" commit (#8508), which exists ONLY to
/// give a merge queue's required checks a fresh timestamp and explicitly
/// changes nothing in the tree. `Some(false)` — a real content change, or a
/// `status` under which an empty `files` proves nothing (`behind`, `diverged`,
/// anything unrecognized); invalidate as before. `None` — the comparison could
/// not be made (a `gh api` failure, an unparsable response or one missing its
/// `status`/`files` keys, a SHA the compare endpoint does not
/// recognize, an argument that is not a bare hex SHA): fails open into "proceed
/// with the ordinary invalidation", the behavior both paths have always had,
/// never into an assumed equivalence on missing evidence.
///
/// `cwd` is the repo root to run `gh` in (`None` = inherit the process cwd,
/// which is what the CLI path wants, matching
/// [`crate::forge_disable_auto_merge::disarm_auto_merge`]). It is what resolves
/// the `{owner}/{repo}` placeholders, so a daemon managing several roots must
/// pass the root it is asking about.
///
/// Costs one `gh api` call, and only for a head move a caller has already
/// decided to invalidate on — never on the common `Fresh` path.
#[must_use]
pub fn tree_unchanged(gh_bin: &Path, cwd: Option<&Path>, base: &str, head: &str) -> Option<bool> {
    // Both keys are REQUIRED: a response missing either fails to deserialize,
    // i.e. `None`, never an assumed equality (PR #9581 review — the inherited
    // `#[serde(default)]` on `files` was the one fail-open arm).
    #[derive(Deserialize)]
    struct Compare {
        status: String,
        files: Vec<serde_json::Value>,
    }
    if !is_sha(base) || !is_sha(head) {
        return None;
    }
    let mut cmd = Command::new(gh_bin);
    cmd.arg("api")
        .arg(format!("repos/{{owner}}/{{repo}}/compare/{base}...{head}"));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        // #5401: a cross-owner managed repo needs its own owner's
        // installation-token GH_CONFIG_DIR (no-op for single-owner fleets).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: Compare = serde_json::from_slice(&out.stdout).ok()?;
    Some(proves_identical_trees(&parsed.status, parsed.files.is_empty()))
}

/// The one predicate: does a three-dot compare response prove `base` and
/// `head` carry byte-identical trees? Only when the merge-base is `base`
/// (`identical`/`ahead`) does an empty `files` list describe `base` vs `head`;
/// under `behind` or `diverged` it describes the merge-base instead, and any
/// status this code does not recognize is treated the same way.
fn proves_identical_trees(status: &str, files_empty: bool) -> bool {
    files_empty && matches!(status, "identical" | "ahead")
}

/// What [`handle`] answers, with the kill switch passed in explicitly so the
/// "switched off => no `gh` call, no answer" contract is unit-testable without
/// mutating process env.
fn answer(
    carveout_enabled: bool,
    gh_bin: &Path,
    cwd: Option<&Path>,
    base: &str,
    head: &str,
) -> Option<bool> {
    if !carveout_enabled {
        return None;
    }
    tree_unchanged(gh_bin, cwd, base, head)
}

/// Handle `loom-daemon forge tree-unchanged <base> <head>`. Never returns
/// (exits).
///
/// Prints exactly one machine-readable line and exits:
///
/// - `TREE_UNCHANGED=1`, exit 0 — the two commits' trees are byte-identical, so
///   a verdict rendered against `base` still describes `head`.
/// - `TREE_UNCHANGED=0`, exit 0 — the trees differ; a verdict rendered against
///   `base` says nothing about `head`.
/// - exit 1 with **nothing on stdout** and the reason on stderr — the
///   comparison could not be made.
///
/// Both determinate answers exit 0 on purpose: the *answer* is on stdout, and
/// exit 1 means only "no answer". A caller must therefore key on the stdout
/// line — `TREE_UNCHANGED=1` and nothing else may suppress an invalidation —
/// which makes every failure mode (an absent binary, a clap error from a
/// daemon predating this verb, a `gh` outage, a non-GitHub forge) collapse into
/// the same fail-closed arm as `None` above. `verdict-staleness-guard.sh`
/// reads it exactly that way.
///
/// With [`VERDICT_TREE_CARVEOUT_ENABLED_ENV`] switched off, no comparison is
/// made and the verb exits 1 with nothing on stdout — so the shell guard
/// invalidates exactly as the daemon pass does with the switch off.
pub fn handle(base: &str, head: &str) -> Result<()> {
    let enabled = verdict_tree_carveout_enabled();
    if !enabled {
        eprintln!(
            "loom-daemon forge tree-unchanged: the tree-identical carve-out is disabled \
             ({VERDICT_TREE_CARVEOUT_ENABLED_ENV} is off). No answer — callers invalidate on \
             every head move."
        );
        std::process::exit(1);
    }
    let gh = crate::forge_cmd::gh_bin();
    match answer(enabled, Path::new(&gh), None, base, head) {
        Some(true) => {
            println!("TREE_UNCHANGED=1");
            std::process::exit(0);
        }
        Some(false) => {
            println!("TREE_UNCHANGED=0");
            std::process::exit(0);
        }
        None => {
            eprintln!(
                "loom-daemon forge tree-unchanged: could not compare {base}...{head} (a `gh api \
                 repos/{{owner}}/{{repo}}/compare/…` failure, an unparsable response, a ref this \
                 repo does not carry, or an argument that is not a bare hex SHA). No answer — \
                 callers must treat this as \"the tree may have changed\"."
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Write a fake `gh` that logs its argv to `log` and answers
    /// `api repos/.../compare/...` with `body` and exit status `rc`.
    fn fake_gh(dir: &Path, log: &Path, body: &str, rc: i32) -> std::path::PathBuf {
        let bin = dir.join("fake-gh.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "{rc}" != "0" ]; then
  echo 'gh: Not Found (HTTP 404)' 1>&2
  exit {rc}
fi
printf '%s' '{body}'
exit 0
"#,
            log = log.display(),
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";

    /// The #9576 incident shape: a re-date commit appended on top of the
    /// reviewed head changed nothing, so `compare` reports `status: "ahead"`
    /// with an empty `files` array — the merge-base is `base`, so this really
    /// is `base` vs `head`.
    #[test]
    fn ahead_with_empty_files_is_tree_unchanged() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "ahead", "files": []}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), Some(true));
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(
            argv.contains(&format!("compare/{SHA_A}...{SHA_B}")),
            "the compare endpoint must be addressed base...head, got: {argv}"
        );
    }

    /// Same commit: `status: "identical"`, `files: []` — unchanged.
    #[test]
    fn identical_is_tree_unchanged() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "identical", "files": []}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), Some(true));
    }

    /// An appended commit that DOES change files is a real change.
    #[test]
    fn ahead_with_files_is_tree_changed() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let body = r#"{"status": "ahead", "files": [{"filename": "src/main.rs"}]}"#;
        let gh = fake_gh(dir.path(), &log, body, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), Some(false));
    }

    /// THE PR #9581 REVIEW HOLE: a force-push that rewinds the head to an
    /// ancestor of the reviewed commit. The three-dot compare's merge-base is
    /// then `head` itself, so `files` is empty although the trees differ
    /// (measured: `compare/main...main~3` -> `behind`, 0 files, 25-file real
    /// diff). Must never read as unchanged.
    #[test]
    fn behind_with_empty_files_is_not_unchanged() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "behind", "files": []}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), Some(false));
    }

    /// A diverged head whose tree equals the merge-base: `files: []` again
    /// describes the merge-base, not `base`. Must never read as unchanged.
    #[test]
    fn diverged_with_empty_files_is_not_unchanged() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "diverged", "files": []}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), Some(false));
    }

    /// A status this code does not recognize proves nothing.
    #[test]
    fn unknown_status_with_empty_files_is_not_unchanged() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "mystery", "files": []}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), Some(false));
    }

    /// A response with no `status` is no answer (`None`), even with `files: []`
    /// — the one-key reading #9124 shipped with is exactly the hole above.
    #[test]
    fn missing_status_is_indeterminate() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"files": []}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), None);
    }

    /// Flipped from the inherited #9124 `#[serde(default)]` reading (which
    /// was fail-*open*): a response with no `files` key is no answer.
    #[test]
    fn absent_files_key_is_indeterminate() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "identical"}"#, 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), None);
    }

    /// Kill switch off: no `gh` call, no answer — even for a response that
    /// would otherwise prove equality. This is what `handle` (the shell
    /// guard's path) consults, so the switch now covers both callers.
    #[test]
    fn carveout_disabled_answers_nothing_without_calling_gh() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "identical", "files": []}"#, 0);
        assert_eq!(answer(false, &gh, Some(dir.path()), SHA_A, SHA_B), None);
        assert!(!log.exists(), "a disabled carve-out must not call `gh`");
        assert_eq!(answer(true, &gh, Some(dir.path()), SHA_A, SHA_B), Some(true));
    }

    /// A failed `gh` call is `None` — the fail-closed arm.
    #[test]
    fn gh_failure_is_indeterminate() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, "", 1);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), None);
    }

    /// Unparsable JSON is `None`, not an assumed equality.
    #[test]
    fn unparsable_response_is_indeterminate() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, "not json at all", 0);
        assert_eq!(tree_unchanged(&gh, Some(dir.path()), SHA_A, SHA_B), None);
    }

    /// A non-SHA argument is refused before any `gh` call — it would otherwise
    /// be interpolated into the request path.
    #[test]
    fn non_sha_arguments_are_refused_without_calling_gh() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"status": "ahead", "files": []}"#, 0);
        for (base, head) in [
            ("../../pulls/1", SHA_B),
            (SHA_A, "HEAD"),
            ("", SHA_B),
            (SHA_A, "111111"), // 6 chars — below the marker regex's own floor
            (SHA_A, "AAAAAAAAAA"),
        ] {
            assert_eq!(
                tree_unchanged(&gh, Some(dir.path()), base, head),
                None,
                "{base}...{head} must be refused"
            );
        }
        assert!(!log.exists(), "no `gh` call may be made for a malformed ref");
    }

    #[test]
    fn is_sha_accepts_the_marker_regexs_range() {
        assert!(is_sha("1234567"));
        assert!(is_sha(SHA_A));
        assert!(!is_sha("123456"));
        assert!(!is_sha(&"1".repeat(41)));
        assert!(!is_sha("123456g"));
    }
}
