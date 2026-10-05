//! Fixtures shared by the repo-facts tests: a scratch checkout whose git
//! config is isolated from the host's, and a fake `gh` that plays a forge
//! whose canonical answer the test can change mid-run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::state;

/// Facts on for this test thread, with an isolated environment; everything
/// is reset when dropped.
pub(crate) struct Env {
    pub(crate) tmp: tempfile::TempDir,
    pub(crate) global: PathBuf,
}

impl Env {
    /// `vars` replaces the environment the resolver reads.
    pub(crate) fn new(vars: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global.gitconfig");
        std::fs::write(&global, "").unwrap();
        state::set_test_enabled(true);
        state::set_test_env(Some(vars));
        state::set_test_git_env(&[
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]);
        Self { tmp, global }
    }

    /// A fresh checkout `name` with the given `(remote, url)` pairs.
    pub(crate) fn repo(&self, name: &str, remotes: &[(&str, &str)]) -> PathBuf {
        let root = self.tmp.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        self.git(&root, &["init", "-q"]);
        for (remote, url) in remotes {
            self.git(&root, &["remote", "add", remote, url]);
        }
        root
    }

    /// Run `git` in `root` under the isolated config.
    pub(crate) fn git(&self, root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", &self.global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        state::set_test_enabled(false);
        state::set_test_env(None);
        state::set_test_git_env(&[]);
    }
}

/// A fake forge behind a `gh` stub. Files in `dir` steer it:
/// `canonical` (the repo's current `owner/name`), `gh_answer` (what gh's own
/// resolver names; defaults to `canonical`), `mode` (`ok` / `404` / `fail`),
/// `pulls` (the rows a `head=<canonical owner>:` filter returns; any other
/// owner gets `[]`), `graphql` / `graphql_exit`, `timeline`.
pub(crate) struct Forge {
    pub(crate) dir: PathBuf,
    pub(crate) gh: PathBuf,
}

impl Forge {
    pub(crate) fn new(parent: &Path, canonical: &str) -> Self {
        let dir = parent.join("forge");
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
D='{d}'
# The stub is reached through the process-wide LOOM_GH_BIN, which tests in
# other modules may read concurrently: answer only for this fixture's own
# checkouts, and fail like an unreachable forge for anyone else.
case "$(pwd -P)" in
  '{scope}'/*) ;;
  *) echo "fake forge: foreign cwd" >&2; exit 1 ;;
esac
echo "$* | GH_REPO=${{GH_REPO-unset}}" >> "$D/log"
canon=$(cat "$D/canonical")
owner=${{canon%%/*}}
name=${{canon#*/}}
mode=$(cat "$D/mode" 2>/dev/null || echo ok)
case "$*" in
  "api graphql"*)
    cat "$D/graphql" 2>/dev/null
    exit $(cat "$D/graphql_exit" 2>/dev/null || echo 0) ;;
  *"/timeline "*)
    cat "$D/timeline" 2>/dev/null
    exit 0 ;;
  "api --include repos/{{owner}}/{{repo}}"*)
    ans=$(cat "$D/gh_answer" 2>/dev/null || echo "$canon")
    printf 'HTTP/2.0 200 OK\r\nEtag: "c"\r\n\r\n{{"full_name":"%s"}}' "$ans"
    exit 0 ;;
  "repo view --json nameWithOwner"*)
    cat "$D/gh_answer" 2>/dev/null || echo "$canon"
    exit 0 ;;
  "repo view --json owner,name"*)
    echo "$canon"
    exit 0 ;;
  "api --include repos/"*)
    if [ "$mode" = 404 ]; then printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'; exit 1; fi
    if [ "$mode" = fail ]; then echo boom >&2; exit 1; fi
    case "$*" in
      *If-None-Match*) if [ -f "$D/notmodified" ]; then printf 'HTTP/2.0 304 Not Modified\r\n\r\n'; exit 1; fi ;;
    esac
    printf 'HTTP/2.0 200 OK\r\nEtag: "v1"\r\n\r\n{{"id":7,"name":"%s","full_name":"%s","owner":{{"login":"%s"}}}}' "$name" "$canon" "$owner"
    exit 0 ;;
  *"--jq .owner.login"*)
    echo "$owner"
    exit 0 ;;
  *"/pulls?"*)
    case "$*" in
      *"head=$owner:"*) cat "$D/pulls" 2>/dev/null || echo '[]' ;;
      *) echo '[]' ;;
    esac
    exit 0 ;;
esac
echo "unexpected: $*" >&2
exit 1
"#,
            d = dir.display(),
            scope = parent.canonicalize().unwrap().display()
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let forge = Self { dir, gh };
        forge.set("canonical", canonical);
        forge
    }

    pub(crate) fn set(&self, file: &str, value: &str) {
        std::fs::write(self.dir.join(file), value).unwrap();
    }

    /// Every call so far (`argv | GH_REPO=…`).
    pub(crate) fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Verify/confirm reads (`api --include repos/<nwo>` with an explicit
    /// repo, never the placeholder).
    pub(crate) fn repo_reads(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with("api --include repos/") && !c.contains("{owner}"))
            .count()
    }
}

/// One pulls row targeting `base_full_name`.
pub(crate) fn pull_row(state: &str, merged_at: Option<&str>, base_full_name: &str) -> String {
    let merged = merged_at.map_or_else(|| "null".to_string(), |m| format!("\"{m}\""));
    format!(
        r#"{{"state":"{state}","merged_at":{merged},"closed_at":"2020-01-01T00:00:00Z","head":{{"sha":"abc","repo":{{"owner":{{"login":"x"}}}}}},"base":{{"repo":{{"full_name":"{base_full_name}"}}}}}}"#
    )
}
