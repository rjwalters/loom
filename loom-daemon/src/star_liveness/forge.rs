//! The forge surface the liveness pass and the intent applier use, as a
//! trait so every decision is testable against a fake ([`StarForge`]), and
//! its production implementation over REST `gh api` ([`GhStarForge`]).
//!
//! Reads go through the ETag-cached listing ([`crate::forge_listing`]) where
//! a label listing will do; single-issue and comment reads are plain REST.
//! Every call honors the rate-limit breaker and the per-owner credential
//! (`credential_preflight`), like the work finder's own reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Result};

use crate::forge_listing::RestIssue;

/// One issue comment: its body, when it was posted and last edited, and who
/// wrote it (for [`super::trust`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ForgeComment {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The forge-assigned edit time (a lease renewal PATCHes its comment).
    #[serde(default)]
    pub updated_at: Option<String>,
    /// `user.login`.
    #[serde(default)]
    pub author: Option<String>,
    /// `author_association` (`OWNER`, `MEMBER`, `COLLABORATOR`, …).
    #[serde(default)]
    pub author_association: Option<String>,
}

/// What the liveness pass and the intent applier need from one repo.
pub trait StarForge {
    /// Open issues **and PRs** carrying `label`.
    ///
    /// # Errors
    /// The listing failed.
    fn list_open(&mut self, label: &str) -> Result<Vec<RestIssue>>;

    /// One issue or PR by number (`None` when it does not exist).
    ///
    /// # Errors
    /// The read failed.
    fn issue(&mut self, number: u32) -> Result<Option<RestIssue>>;

    /// Every comment on an issue or PR, oldest first.
    ///
    /// # Errors
    /// The read failed.
    fn comments(&mut self, number: u32) -> Result<Vec<ForgeComment>>;

    /// Open **issues** (not PRs) whose title or body contains `phrase`, for
    /// finding the incident behind a merge refusal. Callers re-check the
    /// phrase: a forge search is fuzzy.
    ///
    /// # Errors
    /// The search failed.
    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<RestIssue>>;

    /// This daemon's own forge login, when the forge can name it (an App
    /// installation token cannot). Used only to trust its own markers.
    fn self_login(&mut self) -> Option<String> {
        None
    }

    /// Add `label` (a no-op when already present).
    ///
    /// # Errors
    /// The write failed.
    fn add_label(&mut self, number: u32, label: &str) -> Result<()>;

    /// Remove `label` (a no-op when absent).
    ///
    /// # Errors
    /// The write failed.
    fn remove_label(&mut self, number: u32, label: &str) -> Result<()>;

    /// Post a comment.
    ///
    /// # Errors
    /// The write failed.
    fn post_comment(&mut self, number: u32, body: &str) -> Result<()>;
}

/// [`StarForge`] over `gh api`, for the repo checked out at `root` whose
/// forge slug is `slug`.
pub struct GhStarForge {
    pub gh_bin: PathBuf,
    pub root: PathBuf,
    pub slug: String,
}

impl GhStarForge {
    #[must_use]
    pub fn new(root: &Path, slug: &str) -> Self {
        Self {
            gh_bin: PathBuf::from("gh"),
            root: root.to_path_buf(),
            slug: slug.to_string(),
        }
    }

    fn api(&self, args: &[&str], context: &str) -> Result<String> {
        if crate::rate_limit_breaker::global_is_suppressed() {
            return Err(anyhow!("rate-limit breaker is suppressing forge calls"));
        }
        let mut cmd = Command::new(&self.gh_bin);
        cmd.arg("api").args(args).current_dir(&self.root);
        crate::credential_preflight::apply_gh_config_for_cwd(&mut cmd, Some(&self.root));
        let out = cmd.output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "star_liveness");
            return Err(anyhow!("gh api {context} failed: {stderr}"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn issue_path(&self, number: u32) -> String {
        format!("repos/{}/issues/{number}", self.slug)
    }
}

impl StarForge for GhStarForge {
    fn list_open(&mut self, label: &str) -> Result<Vec<RestIssue>> {
        crate::forge_listing::list_issues_cached(
            &self.gh_bin,
            Some(&self.root),
            Some(&self.slug),
            label,
            "open",
        )
    }

    fn issue(&mut self, number: u32) -> Result<Option<RestIssue>> {
        let path = self.issue_path(number);
        match self.api(&[&path], &path) {
            Ok(body) => Ok(crate::forge_listing::parse_rest_issues(&format!("[{body}]"))?
                .into_iter()
                .next()),
            Err(e) if e.to_string().contains("HTTP 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn comments(&mut self, number: u32) -> Result<Vec<ForgeComment>> {
        let path = format!("{}/comments?per_page=100", self.issue_path(number));
        let jq = ".[] | {body, created_at, updated_at, author: .user.login, author_association}";
        let out = self.api(&[&path, "--paginate", "--jq", jq], &path)?;
        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<ForgeComment>(l).ok())
            .collect())
    }

    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<RestIssue>> {
        let clean: String = phrase.chars().filter(|c| *c != '"').collect();
        let q = format!("q=\"{clean}\" repo:{} is:issue is:open in:title,body", self.slug);
        let out = self.api(
            &[
                "-X",
                "GET",
                "search/issues",
                "-f",
                &q,
                "-f",
                "per_page=20",
                "--jq",
                ".items",
            ],
            "search/issues",
        )?;
        let wanted = phrase.to_ascii_lowercase();
        Ok(crate::forge_listing::parse_rest_issues(&out)?
            .into_iter()
            .filter(|i| !i.is_pull_request && i.state.eq_ignore_ascii_case("open"))
            .filter(|i| {
                let text = format!(
                    "{}\n{}",
                    i.title.as_deref().unwrap_or_default(),
                    i.body.as_deref().unwrap_or_default()
                );
                text.to_ascii_lowercase().contains(&wanted)
            })
            .collect())
    }

    fn self_login(&mut self) -> Option<String> {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(hit) = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.root)
        {
            return hit.clone();
        }
        let login = self
            .api(&["user", "--jq", ".login"], "user")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.root.clone(), login.clone());
        login
    }

    fn add_label(&mut self, number: u32, label: &str) -> Result<()> {
        let path = format!("{}/labels", self.issue_path(number));
        let field = format!("labels[]={label}");
        self.api(&["-X", "POST", &path, "-f", &field], &path)
            .map(|_| ())
    }

    fn remove_label(&mut self, number: u32, label: &str) -> Result<()> {
        let path = format!("{}/labels/{label}", self.issue_path(number));
        match self.api(&["-X", "DELETE", &path], &path) {
            Err(e) if e.to_string().contains("HTTP 404") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    fn post_comment(&mut self, number: u32, body: &str) -> Result<()> {
        let path = format!("{}/comments", self.issue_path(number));
        let field = format!("body={body}");
        self.api(&["-X", "POST", &path, "-f", &field], &path)
            .map(|_| ())
    }
}
