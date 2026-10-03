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
use std::time::{Duration, Instant};

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

/// One issue-search hit and who filed it (for [`super::trust`]: an outsider
/// quoting a refusal phrase must not be taken for the incident).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub issue: RestIssue,
    /// `author_association` of the issue's author.
    pub author_association: Option<String>,
}

/// First retry delay after a failed own-login lookup; doubles per failure.
pub const LOGIN_RETRY_BASE: Duration = Duration::from_secs(60);
/// Longest delay between own-login retries.
pub const LOGIN_RETRY_MAX: Duration = Duration::from_secs(3600);

/// The daemon's own forge login, looked up lazily: a success is kept for the
/// process, a failure (transient, or an App token the forge cannot name) is
/// retried with exponential backoff instead of being cached forever.
#[derive(Debug, Default, Clone)]
pub struct LoginLookup {
    known: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl LoginLookup {
    /// The login, calling `fetch` only when none is known and no backoff is
    /// pending.
    pub fn get(&mut self, now: Instant, fetch: impl FnOnce() -> Option<String>) -> Option<String> {
        if self.known.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return self.known.clone();
        }
        match fetch()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(login) => {
                self.known = Some(login);
                self.failures = 0;
                self.retry_at = None;
            }
            None => {
                let delay = LOGIN_RETRY_BASE
                    .saturating_mul(1 << self.failures.min(10))
                    .min(LOGIN_RETRY_MAX);
                self.failures = self.failures.saturating_add(1);
                self.retry_at = Some(now + delay);
            }
        }
        self.known.clone()
    }
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
    /// finding the incident behind a merge refusal, with each author's
    /// association. Callers re-check the phrase and the author: a forge
    /// search is fuzzy, and anyone can file an issue.
    ///
    /// # Errors
    /// The search failed.
    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<SearchHit>>;

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
        if crate::rate_limit_breaker::global_skip_pass("star_liveness") {
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
        crate::forge_listing::list_issues_cached_as(
            "star_liveness",
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

    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<SearchHit>> {
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
        let assoc: HashMap<u64, String> = serde_json::from_str::<Vec<serde_json::Value>>(&out)
            .unwrap_or_default()
            .iter()
            .filter_map(|v| {
                let n = v.get("number")?.as_u64()?;
                Some((n, v.get("author_association")?.as_str()?.to_string()))
            })
            .collect();
        Ok(crate::forge_listing::parse_rest_issues(&out)?
            .into_iter()
            .filter(|i| !i.is_pull_request && i.state.eq_ignore_ascii_case("open"))
            .map(|issue| SearchHit {
                author_association: assoc.get(&u64::from(issue.number)).cloned(),
                issue,
            })
            .collect())
    }

    fn self_login(&mut self) -> Option<String> {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, LoginLookup>>> = OnceLock::new();
        // Registers the configured fleet App with `trust` for this process.
        let app = super::trust::configured_app_slug(&self.root);
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lookup = cache.entry(self.root.clone()).or_default();
        // An App installation token cannot name itself (`GET /user` is a
        // 403): the configured App slug is then this daemon's identity.
        lookup
            .get(Instant::now(), || self.api(&["user", "--jq", ".login"], "user").ok())
            .or_else(|| app.map(|a| format!("{a}[bot]")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_login_lookup_is_retried_with_backoff_and_a_success_is_kept() {
        let mut l = LoginLookup::default();
        let t0 = Instant::now();
        let mut calls = 0;
        let mut fetch = |v: Option<&str>| {
            calls += 1;
            v.map(str::to_string)
        };
        assert_eq!(l.get(t0, || fetch(None)), None);
        assert_eq!(l.get(t0 + Duration::from_secs(30), || fetch(None)), None);
        l.get(t0 + LOGIN_RETRY_BASE, || fetch(None));
        // The second failure doubled the delay: no call at +2 base.
        l.get(t0 + LOGIN_RETRY_BASE * 2, || fetch(Some("early")));
        let got = l.get(t0 + LOGIN_RETRY_BASE * 3, || fetch(Some(" robb-bot ")));
        assert_eq!(got.as_deref(), Some("robb-bot"));
        let got = l.get(t0 + LOGIN_RETRY_MAX * 9, || fetch(None));
        assert_eq!(got.as_deref(), Some("robb-bot"), "a success is kept");
        assert_eq!(calls, 3, "one call per elapsed backoff, never a permanent failure");
        let mut capped = LoginLookup {
            failures: 40,
            ..LoginLookup::default()
        };
        capped.get(t0, || None);
        assert_eq!(capped.retry_at, Some(t0 + LOGIN_RETRY_MAX));
    }
}
