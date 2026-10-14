//! Refuse a PR whose commits carry a GitHub skip-CI marker (Issue #11389).
//!
//! GitHub skips every `push` and `pull_request` workflow for a head commit
//! whose message contains `[skip ci]`, `[ci skip]`, `[no ci]`,
//! `[skip actions]` or `[actions skip]`, or the trailer `skip-checks: true`.
//! An agent that writes one of those literally (say, while describing a
//! skip-CI feature) gets a PR with no CI run at all, and every later
//! `forge wait-checks` just waits until it times out.
//!
//! [`check`] scans every commit in `<base>..HEAD`, subject and body. GitHub
//! honours only the head commit, but a marker lower in the range becomes the
//! head after a rebase or squash, and reading the range costs one `git log`.
//!
//! Unlike the rest of pre-flight this does NOT depend on `buildGate`: it runs
//! in every repository, from `preflight --check` (which `create-pr.sh` calls).
//! Opt out with `LOOM_ALLOW_SKIP_CI=1`. The installer's own
//! `--skip-target-ci` path does not go through `create-pr.sh`, so it is not
//! affected.

use std::path::Path;
use std::process::Command;

/// Exit code from `preflight --check`: a commit carries a skip-CI marker.
pub const EXIT_SKIP_CI: i32 = 8;

/// Env var that turns the check off (`1` only).
pub const ALLOW_ENV: &str = "LOOM_ALLOW_SKIP_CI";

/// Env var `create-pr.sh` sets to the PR's base branch (empty = the
/// repository default).
pub const BASE_ENV: &str = "LOOM_PR_BASE";

/// GitHub's documented skip markers, lowercase.
pub const MARKERS: [&str; 5] = [
    "[skip ci]",
    "[ci skip]",
    "[no ci]",
    "[skip actions]",
    "[actions skip]",
];

/// One marker found in one commit message line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub sha: String,
    /// 1-based line in the commit message (1 = subject).
    pub line_no: usize,
    pub line: String,
    pub marker: String,
}

impl Finding {
    /// `<sha12> line <n>: <line>` for the refusal message.
    #[must_use]
    pub fn render(&self) -> String {
        let short: String = self.sha.chars().take(12).collect();
        format!("  commit {short} line {}: {}", self.line_no, self.line.trim())
    }
}

/// The marker on `line`, if any (case-insensitive).
#[must_use]
pub fn line_marker(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if let Some(m) = MARKERS.iter().find(|m| lower.contains(*m)) {
        return Some((*m).to_string());
    }
    let t = lower.trim();
    let value = t.strip_prefix("skip-checks:")?.trim();
    (value == "true").then(|| "skip-checks: true".to_string())
}

/// Every marker line in one commit message.
#[must_use]
pub fn scan_message(sha: &str, message: &str) -> Vec<Finding> {
    message
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            line_marker(line).map(|marker| Finding {
                sha: sha.to_string(),
                line_no: i + 1,
                line: line.to_string(),
                marker,
            })
        })
        .collect()
}

/// Parse `git log --format=%H%x00%B%x1e` output and scan every message.
#[must_use]
pub fn scan_log(log: &str) -> Vec<Finding> {
    log.split('\x1e')
        .filter_map(|rec| rec.trim_start_matches('\n').split_once('\0'))
        .flat_map(|(sha, msg)| scan_message(sha.trim(), msg))
        .collect()
}

/// The ref the range starts at: `origin/$LOOM_PR_BASE`, else `origin/HEAD`'s
/// target, else `origin/main`.
#[must_use]
pub fn base_ref(worktree: &Path, base: Option<&str>) -> String {
    if let Some(b) = base.map(str::trim).filter(|b| !b.is_empty()) {
        return format!("origin/{}", b.trim_start_matches("origin/"));
    }
    git(worktree, &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "origin/main".to_string())
}

/// The findings in `<base>..HEAD`. `Err` when the range cannot be read
/// (no such base ref, not a repo); callers fail open on that.
///
/// # Errors
/// `git log` failed.
pub fn scan_range(worktree: &Path, base: &str) -> Result<Vec<Finding>, String> {
    let range = format!("{base}..HEAD");
    git(worktree, &["log", "--format=%H%x00%B%x1e", &range])
        .map(|log| scan_log(&log))
        .ok_or_else(|| format!("could not read {range}"))
}

/// The `create-pr.sh` check: `Ok(None)` = nothing to refuse (clean range,
/// or `allow`); `Ok(Some(msg))` = refuse with `msg`.
///
/// # Errors
/// The range could not be read. Callers print it as a warning and do not
/// refuse.
pub fn check(worktree: &Path, allow: bool, base: Option<&str>) -> Result<Option<String>, String> {
    if allow {
        return Ok(None);
    }
    let base = base_ref(worktree, base);
    let found = scan_range(worktree, &base)?;
    if found.is_empty() {
        return Ok(None);
    }
    let mut msg = format!(
        "preflight: REFUSING the PR — a commit in {base}..HEAD carries a GitHub skip-CI marker, \
         so no CI would run on it (#11389):"
    );
    for f in &found {
        msg.push('\n');
        msg.push_str(&f.render());
    }
    msg.push_str(
        "\nReword the commit (`git commit --amend`, or re-commit the range) so the marker \
         is not written literally: write \"skip-CI marker\", or put a space inside the \
         brackets. Deliberate? Set LOOM_ALLOW_SKIP_CI=1.",
    );
    Ok(Some(msg))
}

/// `LOOM_ALLOW_SKIP_CI=1`.
#[must_use]
pub fn allowed_by_env() -> bool {
    std::env::var(ALLOW_ENV).is_ok_and(|v| v.trim() == "1")
}

fn git(worktree: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(worktree)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SPELLINGS: [&str; 5] = [
        "[skip ci]",
        "[ci skip]",
        "[no ci]",
        "[skip actions]",
        "[actions skip]",
    ];

    #[test]
    fn each_spelling_in_subject_and_body_is_found_with_sha_and_line() {
        for m in SPELLINGS {
            for spelled in [m.to_string(), m.to_ascii_uppercase()] {
                let subj = scan_message("abc", &format!("fix: thing {spelled}\n\nbody"));
                assert_eq!(subj.len(), 1, "{spelled}");
                assert_eq!((subj[0].sha.as_str(), subj[0].line_no), ("abc", 1));
                assert_eq!(subj[0].marker, m);
                let body = scan_message("abc", &format!("fix: thing\n\nwhy\n{spelled} here"));
                assert_eq!(body.len(), 1, "{spelled}");
                assert_eq!(body[0].line_no, 4);
                assert!(body[0].render().contains("commit abc line 4: "));
            }
        }
    }

    #[test]
    fn skip_checks_trailer_is_found() {
        for t in ["skip-checks: true", "skip-checks:true", "Skip-Checks: TRUE"] {
            let f = scan_message("s", &format!("feat: x\n\n\n{t}"));
            assert_eq!(f.len(), 1, "{t}");
            assert_eq!(f[0].marker, "skip-checks: true");
            assert_eq!(f[0].line_no, 4);
        }
        assert!(scan_message("s", "feat: x\n\nskip-checks: false").is_empty());
    }

    #[test]
    fn clean_and_near_miss_messages_pass() {
        for msg in [
            "feat: add a skip-CI marker detector",
            "docs: mention [skip ci ] with a space",
            "fix: [skipci] and skip ci without brackets",
            "",
        ] {
            assert!(scan_message("s", msg).is_empty(), "{msg}");
        }
    }

    #[test]
    fn scan_log_reports_a_non_head_commit_body() {
        let log = "aaa\0feat: head\n\nclean\n\x1e\nbbb\0fix: older\n\nnote [ci skip]\n\x1e\n";
        let f = scan_log(log);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].sha.as_str(), f[0].line_no), ("bbb", 3));
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(st.status.success(), "{args:?}: {}", String::from_utf8_lossy(&st.stderr));
    }

    /// A repo with `origin/main` at the root commit and `msgs` on top. No
    /// `.loom/config.json`, i.e. no `buildGate`.
    fn repo(msgs: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        git_ok(p, &["init", "-q"]);
        git_ok(p, &["commit", "-q", "--allow-empty", "-m", "root"]);
        git_ok(p, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        for m in msgs {
            git_ok(p, &["commit", "-q", "--allow-empty", "-m", m]);
        }
        d
    }

    #[test]
    fn check_refuses_with_no_build_gate_configured() {
        let d = repo(&["fix: older\n\nbody [no ci]", "feat: head"]);
        let msg = check(d.path(), false, None).unwrap().unwrap();
        assert!(msg.contains("line 3: body [no ci]"), "{msg}");
        assert!(msg.contains("origin/main..HEAD"), "{msg}");
        // The base named by create-pr.sh wins.
        assert!(check(d.path(), false, Some("main")).unwrap().is_some());
        assert!(check(d.path(), false, Some("nope")).is_err());
    }

    #[test]
    fn check_passes_clean_ranges_and_the_opt_out() {
        let clean = repo(&["feat: a skip-CI marker detector"]);
        assert_eq!(check(clean.path(), false, None).unwrap(), None);
        let marked = repo(&["chore: install [skip ci]"]);
        assert_eq!(check(marked.path(), true, None).unwrap(), None);
    }

    /// The Builder and Doctor prompts tell agents not to write the markers,
    /// and `create-pr.sh` passes the PR base and refuses on exit 8.
    #[test]
    fn prompts_and_create_pr_are_pinned() {
        let defaults = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults");
        for role in ["builder", "doctor"] {
            let p = defaults.join(format!(".claude/commands/loom/{role}.md"));
            let text = std::fs::read_to_string(&p).unwrap();
            assert!(
                text.contains("Never write a skip-CI marker") && text.contains("#11389"),
                "{role}.md lost the skip-CI marker sentence"
            );
        }
        let sh = std::fs::read_to_string(defaults.join("scripts/create-pr.sh")).unwrap();
        assert!(sh.contains("LOOM_PR_BASE=\"$BASE_BRANCH\""), "create-pr.sh base");
        assert!(sh.contains("-ne 8 ]] || exit"), "create-pr.sh exit 8");
    }
}
