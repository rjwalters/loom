//! Repo-configured pre-merge MERGE-TREE checks (#10026).
//!
//! PR CI tests "base + PR", never "base + PR + every sibling PR that merged
//! since". Two PRs that each touch a shared surface (two migrations taking the
//! same numeric prefix, an exhaustive switch a sibling extended) are green
//! alone and red together, and the collision is invisible until both have
//! merged. `merge-pr.sh` is the one point before a merge that can see the real
//! merge tree, so a repo may declare cheap checks to run against it:
//!
//! ```json
//! { "merge": { "treeChecks": ["scripts/check-migration-prefixes.sh", "npm run typecheck"],
//!              "treeChecksTimeoutSecs": 120 } }
//! ```
//!
//! # Contract
//!
//! * `merge.treeChecks` unset, `[]` or not an array of non-empty strings with
//!   at least one entry means NO checks: [`Outcome::Clean`] before any git or
//!   network call is made.
//! * Otherwise the merge tree is built WITHOUT touching the primary checkout
//!   or any issue worktree: `git fetch` the base and the PR head, verify the
//!   fetched head is the head being merged, `git merge-tree --write-tree`, and
//!   `git archive` the resulting tree into a temporary directory that is
//!   removed on every exit path (it is a [`tempfile::TempDir`]).
//! * Each check runs, in order, through `sh -c` with the temp tree as cwd, a
//!   scrubbed minimal environment (no forge tokens) and a timeout. The first
//!   failure stops the run ([`Outcome::Failed`]); a timeout is a failure.
//! * A tree that cannot be built, or a check that cannot be started, is
//!   [`Outcome::Unknown`] and the caller must refuse (fail closed).
//!
//! `node_modules` is untracked, so it is absent from an archived tree. If the
//! repo root that runs `merge-pr.sh` has one, it is symlinked into the temp
//! tree; a typecheck-style step therefore needs dependencies installed in the
//! checkout that runs the merge (loom-ui#1042).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The sentinel printed on stdout when every check passed (or none declared).
pub const CLEAN: &str = "LOOM-TREE-CHECKS-CLEAN";
/// Prefix of the stdout line printed when `--allow-red-tree` overrode a failure.
pub const BYPASSED: &str = "LOOM-TREE-CHECKS-BYPASSED";
/// Default per-check timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Ends the `Failed` output of a check that was killed on timeout; see
/// [`is_timeout`].
const TIMED_OUT_SUFFIX: &str = " and was killed)";
/// Output kept (tail) for the refusal and the PR comment.
const OUTPUT_TAIL_BYTES: usize = 6000;

/// The declared checks.
#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    pub checks: Vec<String>,
    pub timeout: Duration,
}

/// Parse `.loom/config.json` text. Unreadable/unparseable JSON is an error
/// (fail closed): a config that cannot be read must not silently disable a
/// guard the repo asked for.
pub fn parse_config(json: &str) -> Result<Config, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("config is not valid JSON: {e}"))?;
    let merge = v.get("merge");
    let checks = match merge.and_then(|m| m.get("treeChecks")) {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(a)) => {
            let mut out = Vec::new();
            for item in a {
                match item.as_str().map(str::trim) {
                    Some(s) if !s.is_empty() => out.push(s.to_string()),
                    _ => {
                        return Err("merge.treeChecks must be an array of non-empty strings".into())
                    }
                }
            }
            out
        }
        Some(_) => return Err("merge.treeChecks must be an array of non-empty strings".into()),
    };
    let secs = merge
        .and_then(|m| m.get("treeChecksTimeoutSecs"))
        .and_then(serde_json::Value::as_u64)
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Ok(Config {
        checks,
        timeout: Duration::from_secs(secs),
    })
}

/// Result of the gate.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Clean,
    Failed { check: String, output: String },
    Unknown(String),
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("could not exec git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The merge of the fetched base tip and the PR head, as git objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTree {
    /// The base tip that was actually fetched (and merged onto).
    pub base_sha: String,
    /// `git merge-tree --write-tree`'s result.
    pub tree_sha: String,
}

/// Fetch `<remote>/<base_ref>` and PR `pr`'s head, verify the head is
/// `head_sha`, and write their merge tree. Shared by [`build_tree`] and
/// [`build_checkout`]. Any conflict is an error (fail closed).
pub fn merge_tree(
    repo_root: &Path,
    remote: &str,
    pr: &str,
    base_ref: &str,
    head_sha: &str,
) -> Result<MergeTree, String> {
    // Fetch both tips into private refs and resolve them from what was actually
    // fetched (no second `ls-remote` round trip that could see a newer base).
    let base_local = format!("refs/loom/tree-checks/{pr}/base");
    let pr_local = format!("refs/loom/tree-checks/{pr}/head");
    let base_spec = format!("+refs/heads/{base_ref}:{base_local}");
    let pr_spec = format!("+refs/pull/{pr}/head:{pr_local}");
    let fetched = git(
        repo_root,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            remote,
            "--",
            &base_spec,
            &pr_spec,
        ],
    )
    .map_err(|e| format!("could not fetch the base and PR head: {e}"));
    let resolve = |r: &str| {
        git(
            repo_root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{r}^{{commit}}"),
            ],
        )
        .ok()
    };
    let tips = fetched.map(|_| (resolve(&base_local), resolve(&pr_local)));
    let _ = git(repo_root, &["update-ref", "-d", &base_local]);
    let _ = git(repo_root, &["update-ref", "-d", &pr_local]);
    let (base_sha, pr_tip) = tips?;
    let base_sha = base_sha.ok_or_else(|| format!("could not resolve {remote}/{base_ref}"))?;
    let pr_tip = pr_tip.ok_or_else(|| format!("could not resolve {remote} PR #{pr} head"))?;
    if pr_tip != head_sha {
        return Err(format!(
            "PR #{pr}'s head on {remote} is {pr_tip}, not the {head_sha} being merged (head moved)"
        ));
    }
    let tree = git(
        repo_root,
        &[
            "merge-tree",
            "--write-tree",
            "--no-messages",
            &base_sha,
            head_sha,
        ],
    )
    .map_err(|e| format!("could not build the merge tree (conflict or old git): {e}"))?;
    let tree = tree.lines().next().unwrap_or("").to_string();
    if tree.len() < 40 {
        return Err("git merge-tree printed no tree id".to_string());
    }
    Ok(MergeTree {
        base_sha,
        tree_sha: tree,
    })
}

/// Build the merge tree of `origin/<base_ref>` and the PR head and extract it
/// into a fresh temp dir. `remote` is the git remote to fetch from.
pub fn build_tree(
    repo_root: &Path,
    remote: &str,
    pr: &str,
    base_ref: &str,
    head_sha: &str,
) -> Result<tempfile::TempDir, String> {
    let tree = merge_tree(repo_root, remote, pr, base_ref, head_sha)?.tree_sha;
    let dir = tempfile::Builder::new()
        .prefix("loom-tree-checks-")
        .tempdir()
        .map_err(|e| format!("could not create a temp dir: {e}"))?;
    let tar = dir.path().join("..").join(format!(
        "{}.tar",
        dir.path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("loom-tree")
    ));
    let tar_file = File::create(&tar).map_err(|e| format!("temp tar: {e}"))?;
    let status = Command::new("git")
        .current_dir(repo_root)
        .args(["archive", "--format=tar", &tree])
        .stdout(Stdio::from(tar_file))
        .status();
    let extracted = match status {
        Ok(s) if s.success() => Command::new("tar")
            .arg("-xf")
            .arg(&tar)
            .arg("-C")
            .arg(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        _ => false,
    };
    let _ = std::fs::remove_file(&tar);
    if !extracted {
        return Err("could not extract the merge tree".to_string());
    }
    #[cfg(unix)]
    {
        let nm = repo_root.join("node_modules");
        if nm.is_dir() && !dir.path().join("node_modules").exists() {
            let _ = std::os::unix::fs::symlink(&nm, dir.path().join("node_modules"));
        }
    }
    Ok(dir)
}

fn tail(s: &str) -> String {
    if s.len() <= OUTPUT_TAIL_BYTES {
        return s.to_string();
    }
    let mut start = s.len() - OUTPUT_TAIL_BYTES;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[... output truncated ...]\n{}", &s[start..])
}

/// Run the checks in order in `tree`; first failure wins.
pub fn run_checks(tree: &Path, cfg: &Config) -> Outcome {
    for check in &cfg.checks {
        let log: PathBuf = tree.join("..").join(format!(
            "{}.log",
            tree.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("loom-tree")
        ));
        let (out, err) = match File::create(&log).and_then(|f| Ok((f.try_clone()?, f))) {
            Ok(p) => p,
            Err(e) => return Outcome::Unknown(format!("could not capture output: {e}")),
        };
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        let home = std::env::var("HOME").unwrap_or_default();
        let mut cmd = Command::new("sh");
        // Own process group, so a timeout kills the check's descendants
        // (e.g. `node` under `npm run`), not just `sh`.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = match cmd
            .arg("-c")
            .arg(check)
            .current_dir(tree)
            .env_clear()
            .env("PATH", path)
            .env("HOME", home)
            .env("CI", "1")
            .env("LOOM_MERGE_TREE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&log);
                return Outcome::Unknown(format!("could not start check `{check}`: {e}"));
            }
        };
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Ok(s),
                Ok(None) if started.elapsed() >= cfg.timeout => {
                    #[cfg(unix)]
                    if let Ok(pid) = i32::try_from(child.id()) {
                        // SAFETY: kill(2) on our own child's process group.
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    let _ = std::fs::remove_file(&log);
                    return Outcome::Unknown(format!("could not wait on check `{check}`: {e}"));
                }
            }
        };
        let text = std::fs::read(&log)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                let code = s
                    .code()
                    .map_or("a signal".to_string(), |c| format!("exit {c}"));
                return Outcome::Failed {
                    check: check.clone(),
                    output: format!("{}\n(terminated by {code})", tail(&text)),
                };
            }
            Err(()) => {
                return Outcome::Failed {
                    check: check.clone(),
                    output: format!(
                        "{}\n(timed out after {}s{TIMED_OUT_SUFFIX}",
                        tail(&text),
                        cfg.timeout.as_secs()
                    ),
                }
            }
        }
    }
    Outcome::Clean
}

/// Was this [`Outcome::Failed`] output produced by a timeout rather than a
/// check's own verdict? The #10388 local evaluation treats a timeout as
/// "no verdict" (fail closed to the CI remedy), not as a failing check.
#[must_use]
pub fn is_timeout(output: &str) -> bool {
    output.trim_end().ends_with(TIMED_OUT_SUFFIX) && output.contains("\n(timed out after ")
}

/// Whole gate: no checks means no git, no network, no temp dir.
pub fn evaluate(
    repo_root: &Path,
    config_json: Option<&str>,
    remote: &str,
    pr: &str,
    base_ref: &str,
    head_sha: &str,
) -> Outcome {
    let Some(json) = config_json else {
        return Outcome::Clean;
    };
    let cfg = match parse_config(json) {
        Ok(c) => c,
        Err(e) => return Outcome::Unknown(e),
    };
    if cfg.checks.is_empty() {
        return Outcome::Clean;
    }
    match build_tree(repo_root, remote, pr, base_ref, head_sha) {
        Ok(dir) => run_checks(dir.path(), &cfg),
        Err(e) => Outcome::Unknown(e),
    }
}

/// The refusal text, naming the failing check and its real output.
pub fn refusal(pr: &str, check: &str, output: &str) -> String {
    format!(
        "Merge blocked: PR #{pr}'s merge tree (base + PR head) fails the repo's pre-merge tree check `{check}` (merge.treeChecks, #10026). Each PR passed CI alone; the combination does not. Rebase/fix the PR against the current base, or bypass with --allow-red-tree.\n\n--- output of `{check}` ---\n{}",
        output.trim_end()
    )
}

/// A code fence longer than any backtick run in `text`, so output cannot escape it.
fn fence_for(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// PR comment for a refusal.
pub fn failure_comment(check: &str, output: &str) -> String {
    let output = output.trim_end();
    let fence = fence_for(output);
    format!(
        "## Merge Blocked: Tree Check Failed\n\nThe merge tree (current base + this PR's head) fails the repo-declared pre-merge check `{check}` (`merge.treeChecks`, #10026).\n\n{fence}\n{output}\n{fence}\n\nBypass (operator only): `merge-pr.sh --allow-red-tree`."
    )
}

/// Audit comment for `--allow-red-tree`.
pub fn bypass_comment(head_sha: &str, check: &str) -> String {
    format!(
        "## Red Merge Tree Override Recorded\n\nThis PR is being merged via `merge-pr.sh --allow-red-tree` (later merge stages may still refuse) although the merge tree fails the pre-merge check `{check}` (`merge.treeChecks`).\n\n- **Head SHA**: `{head_sha}`\n\nThe operator running this merge explicitly asserted responsibility for this override (#10026)."
    )
}

#[cfg(test)]
mod tests;
