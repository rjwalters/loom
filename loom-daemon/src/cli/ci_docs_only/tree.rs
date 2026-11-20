use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const ALLOWLIST: &str = ".loom/scripts/ci-docs-only-paths.txt";
pub(super) const QUEUE_PROOF: &str = ".loom/ci-docs-only-queue.json";
const DISTRIBUTED_PATHS: &str = include_str!("../../../../defaults/scripts/ci-docs-only-paths.txt");
const MAX_DIFF_BYTES: usize = 1024 * 1024;

pub(super) fn is_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && value != "0000000000000000000000000000000000000000"
}

pub(super) struct Tree<'a>(&'a Path);

impl<'a> Tree<'a> {
    pub(super) fn new(root: &'a Path, head: &str) -> Option<Self> {
        let tree = Self(root);
        let top = tree.text(&["rev-parse", "--show-toplevel"])?;
        (std::fs::canonicalize(top).ok()? == root.canonicalize().ok()?
            && tree.text(&["rev-parse", "--is-shallow-repository"])? == "false"
            && tree.text(&["rev-parse", "--verify", "HEAD^{commit}"])? == head)
            .then_some(tree)
    }

    /// Verify the staged tree and every tracked path. An index that hides
    /// paths from Git's normal worktree comparison cannot prove the checkout.
    pub(super) fn matches_checkout(&self, head: &str) -> bool {
        let Some(index) = self.git(&["ls-files", "-v", "-z"]) else {
            return false;
        };
        if !index.strip_suffix(b"\0").is_some_and(|files| {
            files
                .split(|byte| *byte == 0)
                .all(|entry| entry.starts_with(b"H "))
        }) {
            return false;
        }
        let quiet_diff = [
            "diff",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
        ];
        self.git(&[&quiet_diff[..], &["--cached", head, "--"]].concat())
            .as_deref()
            == Some(b"")
            && self.git(&[&quiet_diff[..], &["--"]].concat()).as_deref() == Some(b"")
    }

    fn git(&self, args: &[&str]) -> Option<Vec<u8>> {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.0)
            .args([
                "--no-replace-objects",
                "--no-pager",
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.trustctime=true",
                "-c",
                "core.checkStat=default",
                "-c",
                "core.filemode=true",
            ])
            .args(args)
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::null());
        let result = crate::cmd_out::run_command(cmd, Duration::from_secs(15));
        match result {
            crate::cmd_out::CmdOutcome::Ran(output) if output.status.success() => {
                Some(output.stdout)
            }
            _ => None,
        }
    }

    fn text(&self, args: &[&str]) -> Option<String> {
        Some(String::from_utf8(self.git(args)?).ok()?.trim().to_string())
    }

    pub(super) fn parents(&self, commit: &str) -> Option<Vec<String>> {
        let line = self.text(&["rev-list", "--parents", "-n", "1", commit, "--"])?;
        let mut words = line.split_ascii_whitespace();
        if words.next()? != commit {
            return None;
        }
        words.map(|s| is_sha(s).then(|| s.to_string())).collect()
    }

    pub(super) fn merge_base(&self, target: &str, branch: &str) -> Option<String> {
        let base = self.text(&["merge-base", "--all", target, branch])?;
        is_sha(&base).then_some(base)
    }

    pub(super) fn ancestor(&self, base: &str, head: &str) -> bool {
        self.git(&["merge-base", "--is-ancestor", base, head])
            .is_some()
    }

    pub(super) fn first_parent_contains(&self, base: &str, head: &str) -> bool {
        base == head
            || self
                .text(&["rev-list", "--first-parent", "--max-count=512", head, "--"])
                .is_some_and(|history| history.lines().any(|commit| commit == base))
    }

    pub(super) fn blob(&self, commit: &str, path: &str) -> Option<Vec<u8>> {
        let object = format!("{commit}:{path}");
        let entries = self.text(&["ls-tree", commit, "--", path])?;
        let (meta, name) = entries.split_once('\t')?;
        if name != path || !meta.starts_with("100644 blob ") {
            return None;
        }
        self.git(&["cat-file", "blob", &object])
    }

    pub(super) fn allowlist(&self, protected: &str) -> Option<Vec<String>> {
        let default = policy_lines(DISTRIBUTED_PATHS).collect::<Vec<_>>();
        let bytes = self.blob(protected, ALLOWLIST)?;
        let mut allowed = Vec::new();
        for line in policy_lines(std::str::from_utf8(&bytes).ok()?) {
            if !default.contains(&line) || allowed.iter().any(|p| p == line) {
                return None;
            }
            allowed.push(line.to_string());
        }
        Some(allowed)
    }

    pub(super) fn docs_only(
        &self,
        base: &str,
        protected: &str,
        head: &str,
        allowed: &[String],
    ) -> Result<(), &'static str> {
        // A preceding speculative group must not change the classifier or the
        // workflow whose success is used to skip work in this one.
        for commit in [base, head] {
            if self
                .git(&[
                    "diff-tree",
                    "--no-commit-id",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=none",
                    "--name-only",
                    "-r",
                    protected,
                    commit,
                    "--",
                    ".github/workflows",
                    ".github/actions",
                    ".loom/scripts",
                    ".loom/config.json",
                    QUEUE_PROOF,
                ])
                .as_deref()
                != Some(b"")
            {
                return Err("ci_input_changed");
            }
        }
        let diff = self
            .git(&[
                "diff-tree",
                "--no-commit-id",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "--raw",
                "-z",
                "--no-abbrev",
                "-r",
                base,
                head,
                "--",
            ])
            .ok_or("diff_unavailable")?;
        check_diff(&diff, allowed)
    }
}

fn policy_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

pub(super) fn check_diff(diff: &[u8], allowed: &[String]) -> Result<(), &'static str> {
    if diff.is_empty() {
        return Err("empty_diff");
    }
    let raw = diff.strip_suffix(b"\0").ok_or("diff_unavailable")?;
    if raw.len() >= MAX_DIFF_BYTES {
        return Err("diff_unavailable");
    }
    let mut fields = raw.split(|byte| *byte == 0);
    while let Some(metadata) = fields.next() {
        let path = fields.next().ok_or("diff_unavailable")?;
        let header = std::str::from_utf8(metadata).map_err(|_| "diff_unavailable")?;
        let parts = header.split(' ').collect::<Vec<_>>();
        match parts.as_slice() {
            [":100644", "100644", old, new, "M"] if is_sha(old) && is_sha(new) && old != new => {}
            _ => return Err("nonregular_or_nonmodify"),
        }
        if !allowed.iter().any(|item| item.as_bytes() == path) {
            return Err("path_outside_policy");
        }
    }
    Ok(())
}
