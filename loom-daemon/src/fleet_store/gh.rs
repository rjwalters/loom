//! The production [`Transport`]: `gh api --include`, under the daemon's own
//! forge credentials.
//!
//! No new credential is introduced. For a store owned by `OWNER`, requests
//! run under, in order:
//!
//! 1. **A reader App** — when the workspace's roster (`forge.identities`,
//!    [`crate::forge_identity`]) has readers and the daemon has published a
//!    fresh reader token for `OWNER` ([`crate::forge_identity::read_credential_in`]).
//!    A store read is a read, so it goes to a reader first, exactly like the
//!    daemon's own listings.
//! 2. **The writer App** (`forge.githubApp`) — an installation token for the
//!    store minted through the workspace's `github-app-token.sh`
//!    ([`crate::credential_preflight::RealGithubAppMinter`], which reuses its
//!    own on-disk token cache) and published to the same per-owner
//!    `GH_CONFIG_DIR` the daemon uses for cross-owner repos
//!    ([`crate::credential_preflight::github_app_gh_config_dir_for_owner`]).
//!    Used when no reader is usable, or after a reader's credential failure.
//! 3. **Ambient `gh` auth** — when no GitHub App is configured at all (a
//!    standalone install), the same fallback every daemon `gh` call has.
//!
//! Every call is recorded in [`crate::forge_call_stats`] under
//! `fleet_store`, like the daemon's other conditional reads.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result};

use super::fetch::{Reply, Transport};
use crate::credential_preflight::{self as cp, GithubAppMinter, GithubAppOutcome};

/// Which credential a request ran under, for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A `GH_CONFIG_DIR` carrying an App installation token.
    ConfigDir {
        /// The directory.
        dir: PathBuf,
        /// `reader app <id>` / `writer app`.
        label: String,
    },
    /// Whatever `gh` resolves on its own.
    Ambient,
}

impl Credential {
    /// Human label.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Credential::ConfigDir { label, .. } => label,
            Credential::Ambient => "ambient gh auth",
        }
    }
}

/// `gh api` transport for one store, resolving credentials against the
/// daemon workspace `workspace_root`.
pub struct GhTransport {
    gh_bin: String,
    workspace_root: PathBuf,
    repo: String,
    reader: Option<Credential>,
    writer: RefCell<Option<Credential>>,
    reader_failed: RefCell<bool>,
    last_used: RefCell<Option<Credential>>,
}

impl GhTransport {
    /// A transport for `repo` (`OWNER/REPO`) using the credentials configured
    /// for the daemon workspace at `workspace_root`.
    #[must_use]
    pub fn new(workspace_root: &Path, repo: &str) -> Self {
        let roster = crate::forge_identity::resolve(workspace_root);
        let reader = crate::forge_identity::read_credential_in(
            workspace_root,
            &roster,
            repo,
            SystemTime::now(),
        )
        .map(|(dir, app_id)| Credential::ConfigDir {
            dir,
            label: format!("reader app {app_id}"),
        });
        Self {
            gh_bin: std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string()),
            workspace_root: workspace_root.to_path_buf(),
            repo: repo.to_string(),
            reader,
            writer: RefCell::new(None),
            reader_failed: RefCell::new(false),
            last_used: RefCell::new(None),
        }
    }

    /// The credential the most recent request ran under.
    #[must_use]
    pub fn last_credential(&self) -> Option<Credential> {
        self.last_used.borrow().clone()
    }

    fn writer(&self) -> Credential {
        if let Some(c) = self.writer.borrow().as_ref() {
            return c.clone();
        }
        let c = writer_credential(&self.workspace_root, &self.repo);
        *self.writer.borrow_mut() = Some(c.clone());
        c
    }

    fn run(
        &self,
        cred: &Credential,
        api_path: &str,
        accept: Option<&str>,
        etag: Option<&str>,
    ) -> Result<(Option<crate::forge_listing::HttpResponse>, String, bool)> {
        let mut cmd = Command::new(&self.gh_bin);
        cmd.arg("api").arg("--include").arg("--method").arg("GET");
        if let Some(a) = accept {
            cmd.arg("-H").arg(format!("Accept: {a}"));
        }
        if let Some(e) = etag {
            cmd.arg("-H").arg(format!("If-None-Match: {e}"));
        }
        cmd.arg(api_path);
        if let Credential::ConfigDir { dir, .. } = cred {
            cmd.env("GH_CONFIG_DIR", dir);
        }
        cmd.current_dir(&self.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", self.gh_bin))?;
        let response =
            crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        crate::forge_call_stats::record_gh_api(
            "fleet_store",
            response.as_ref(),
            out.status.success(),
            &stderr,
        );
        *self.last_used.borrow_mut() = Some(cred.clone());
        Ok((response, stderr, out.status.success()))
    }
}

impl Transport for GhTransport {
    fn get(&self, api_path: &str, accept: Option<&str>, etag: Option<&str>) -> Result<Reply> {
        let use_reader = self.reader.is_some() && !*self.reader_failed.borrow();
        let first = if use_reader {
            self.reader.clone().unwrap_or(Credential::Ambient)
        } else {
            self.writer()
        };
        let (mut response, mut stderr, _) = self.run(&first, api_path, accept, etag)?;
        if use_reader {
            let status = response.as_ref().map(|r| r.status);
            let ok = matches!(status, Some(200..=299 | 304));
            if !ok && crate::forge_identity::classify_failure(&stderr, status).is_some() {
                // A reader that cannot serve this store (not installed on the
                // owner, rate-limited) costs one request, then the writer.
                *self.reader_failed.borrow_mut() = true;
                (response, stderr, _) = self.run(&self.writer(), api_path, accept, etag)?;
            }
        }
        let response = response.ok_or_else(|| {
            anyhow::anyhow!(
                "gh api {api_path} failed before an HTTP response: {}",
                if stderr.is_empty() {
                    "no output"
                } else {
                    &stderr
                }
            )
        })?;
        Ok(Reply {
            status: response.status,
            etag: response.etag,
            body: response.body,
        })
    }
}

/// The writer credential for `repo`: an App token minted and published to
/// the per-owner `GH_CONFIG_DIR`, or ambient auth when no App is configured.
fn writer_credential(workspace_root: &Path, repo: &str) -> Credential {
    let Some(script_path) = cp::resolve_github_app_script(workspace_root) else {
        return Credential::Ambient;
    };
    let minter = cp::RealGithubAppMinter {
        script_path,
        cwd: workspace_root.to_path_buf(),
    };
    match minter.mint(repo) {
        GithubAppOutcome::Minted { token, .. } => {
            let dir =
                cp::github_app_gh_config_dir_for_owner(workspace_root, cp::owner_of_nwo(repo));
            match cp::publish_github_app_token(&dir, &token) {
                Ok(()) => Credential::ConfigDir {
                    dir,
                    label: "writer app".to_string(),
                },
                Err(e) => {
                    eprintln!(
                        "warning: minted a GitHub App token for {repo} but could not publish it \
                         to {} ({e}); falling back to ambient gh auth",
                        dir.display()
                    );
                    Credential::Ambient
                }
            }
        }
        GithubAppOutcome::NotConfigured => Credential::Ambient,
        GithubAppOutcome::Error(reason) => {
            eprintln!(
                "warning: GitHub App token mint for {repo} failed ({reason}); falling back to \
                 ambient gh auth"
            );
            Credential::Ambient
        }
    }
}
