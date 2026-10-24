//! The production [`Transport`]: `gh api --include`, under the daemon's own
//! forge credentials.
//!
//! No new credential is introduced. For a store owned by `OWNER`:
//!
//! - **Reads run on a reader App, never the writer** (operator ruling #12394,
//!   #12584) — the reader the workspace's roster (`forge.identities`,
//!   [`crate::forge_identity`]) routes `OWNER/REPO` to
//!   ([`crate::forge_identity::read_credential_in`]). A rate limit or a
//!   refused token withdraws that reader ([`crate::forge_identity::withdraw_after`])
//!   and the next one serves the read; with none left the read fails as
//!   **shed** and the caller keeps its last copy. A `404` from a reader that
//!   can see the repository (it served the repository recently, or answers
//!   `GET repos/OWNER/REPO`) is the item's own answer — "this ref does not
//!   exist" — not a coverage gap (#10718's concern).
//! - **On a host with no reader pool at all** the writer is the only
//!   credential there is, and reads use it: the writer App (`forge.githubApp`)
//!   installation token for the store, minted through the workspace's
//!   `github-app-token.sh` ([`crate::credential_preflight::RealGithubAppMinter`],
//!   which reuses its own on-disk token cache) and published to the same
//!   per-owner `GH_CONFIG_DIR` the daemon uses for cross-owner repos
//!   ([`crate::credential_preflight::github_app_gh_config_dir_for_owner`]),
//!   else **ambient `gh` auth** when no GitHub App is configured at all (a
//!   standalone install). That is the one place this file pins the writer on
//!   a read (`tests/writer_read_guard.rs`).
//! - **Writes** always run on the writer.
//!
//! Every call runs through [`crate::gh_invocation::GhInvocation`] (#10089),
//! which records it in [`crate::forge_call_stats`] under `fleet_store` /
//! `fleet_store_write`, like the daemon's other conditional reads.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::Value;

use super::fetch::{Reply, Transport};
use crate::credential_preflight::{self as cp, GithubAppMinter, GithubAppOutcome};
use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_identity::{served, Failure, IdentityRole};
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

/// Deadline for one store request (#10089: they were unbounded).
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Readers one read may try before it is shed.
const MAX_READERS_TRIED: usize = 8;

/// The next usable reader for the transport's repo: `(GH_CONFIG_DIR, app id)`.
type NextReader = dyn Fn() -> Option<(PathBuf, String)>;

/// `(app id, owner/repo, failure, why)` → withdraw that reader.
type WithdrawReader = dyn Fn(&str, &str, Failure, &str);

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

/// Whether `reader_root` has a reader pool, and the lookup of the next usable
/// reader for `repo` there (`roster_of` is [`crate::forge_identity::resolve`]
/// in production).
fn reader_pool(
    reader_root: PathBuf,
    repo: &str,
    roster_of: fn(&Path) -> crate::forge_identity::Roster,
) -> (bool, Box<NextReader>) {
    let pooled = !roster_of(&reader_root).readers.is_empty();
    let nwo = repo.to_string();
    let next = move || {
        let roster = roster_of(&reader_root);
        crate::forge_identity::read_credential_in(&reader_root, &roster, &nwo, SystemTime::now())
    };
    (pooled, Box::new(next))
}

/// `gh api` transport for one store, resolving credentials against the
/// daemon workspace `workspace_root`.
pub struct GhTransport {
    gh_bin: String,
    workspace_root: PathBuf,
    repo: String,
    /// The workspace has a reader pool (#12584): its reads never run on the
    /// writer.
    pooled: bool,
    /// The reader the next read runs on.
    reader: RefCell<Option<(PathBuf, String)>>,
    next_reader: Box<NextReader>,
    withdraw: Box<WithdrawReader>,
    writer: RefCell<Option<Credential>>,
    last_used: RefCell<Option<Credential>>,
}

impl GhTransport {
    /// A transport for `repo` (`OWNER/REPO`) using the credentials configured
    /// for the daemon workspace at `workspace_root`.
    ///
    /// Callers pass a managed checkout's root (the workspace resync, the head
    /// check), but readers are published under the daemon's workspace root
    /// only, so the pool is resolved there
    /// ([`crate::forge_identity::reader_root`], #12630). The writer credential
    /// still resolves against `workspace_root`.
    #[must_use]
    pub fn new(workspace_root: &Path, repo: &str) -> Self {
        Self::new_with_reader_root(
            workspace_root,
            crate::forge_identity::published_reader_root(),
            repo,
            crate::forge_identity::resolve,
        )
    }

    /// [`GhTransport::new`] with the root readers are published under
    /// (`published`: the daemon's workspace root, else the agent front's;
    /// `None` in a process with neither) and the roster resolver injected.
    /// The reader pool is resolved at `published`, falling back to
    /// `workspace_root`; the writer credential always resolves at
    /// `workspace_root` (#12630).
    fn new_with_reader_root(
        workspace_root: &Path,
        published: Option<&Path>,
        repo: &str,
        roster_of: fn(&Path) -> crate::forge_identity::Roster,
    ) -> Self {
        let (pooled, next_reader) = reader_pool(
            crate::forge_identity::reader_root_with(workspace_root, published),
            repo,
            roster_of,
        );
        Self::with_readers(
            workspace_root,
            repo,
            pooled,
            next_reader,
            Box::new(crate::forge_identity::withdraw_after),
        )
    }

    /// [`GhTransport::new`] with the reader pool injected: whether there is
    /// one (`pooled`), the reader lookup, and the withdrawal.
    fn with_readers(
        workspace_root: &Path,
        repo: &str,
        pooled: bool,
        next_reader: Box<NextReader>,
        withdraw: Box<WithdrawReader>,
    ) -> Self {
        let reader = if pooled { next_reader() } else { None };
        Self {
            gh_bin: std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string()),
            workspace_root: workspace_root.to_path_buf(),
            repo: repo.to_string(),
            pooled,
            reader: RefCell::new(reader),
            next_reader,
            withdraw,
            writer: RefCell::new(None),
            last_used: RefCell::new(None),
        }
    }

    /// A transport whose reader pool is a test's (see
    /// [`GhTransport::with_readers`]).
    #[cfg(test)]
    pub(crate) fn with_test_readers(
        workspace_root: &Path,
        repo: &str,
        pooled: bool,
        next_reader: Box<NextReader>,
        withdraw: Box<WithdrawReader>,
    ) -> Self {
        Self::with_readers(workspace_root, repo, pooled, next_reader, withdraw)
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

    /// One GET under `cred`: a reader's (`reader_app` = its app id), else
    /// the writer's — which only a host with no reader pool reaches.
    fn run(
        &self,
        cred: &Credential,
        reader_app: Option<&str>,
        api_path: &str,
        accept: Option<&str>,
        etag: Option<&str>,
    ) -> Result<(Option<crate::forge_listing::HttpResponse>, String, bool)> {
        let inv = self.invocation("fleet_store", ops::GIT_READ_OBJECTS, AccessIntent::Read, cred);
        let inv = match reader_app {
            // A reader the transport routed itself: declared, so the facade
            // keeps it, and no env token can outrank it (#9872).
            Some(app_id) => inv
                .identity_role(IdentityRole::Reader)
                .identity_bucket(&crate::forge_identity::reader_bucket(app_id, &self.repo))
                .without_token_env(),
            // No reader pool on this host: the writer is the only credential
            // there is (#12394's one exception).
            None => inv.writer_identity(),
        };
        let mut inv = inv.args(["api", "--include", "--method", "GET"]);
        if let Some(a) = accept {
            inv = inv.arg("-H").arg(format!("Accept: {a}"));
        }
        if let Some(e) = etag {
            inv = inv.arg("-H").arg(format!("If-None-Match: {e}"));
        }
        let out = self.output(inv.arg(api_path))?;
        let response =
            crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        *self.last_used.borrow_mut() = Some(cred.clone());
        Ok((response, stderr, out.status.success()))
    }

    /// A facade invocation of this transport's `gh` in the workspace, under
    /// `cred` (`Ambient` = the facade's own lookup, which is the process's
    /// credential when no App — hence no per-owner config dir — is set up).
    ///
    /// Accounted under `forge_op` against the store repository (#9831). The
    /// target stays [`GhTarget::None`] — the credential is chosen here, not by
    /// the facade's owner lookup — so the repo is named for accounting only.
    fn invocation(
        &self,
        op: &'static str,
        forge_op: ForgeOp,
        intent: AccessIntent,
        cred: &Credential,
    ) -> GhInvocation {
        self.invocation_within(op, forge_op, intent, cred, GH_TIMEOUT)
    }

    /// [`Self::invocation`] with its own deadline.
    fn invocation_within(
        &self,
        op: &'static str,
        forge_op: ForgeOp,
        intent: AccessIntent,
        cred: &Credential,
        timeout: Duration,
    ) -> GhInvocation {
        let dir = match cred {
            Credential::ConfigDir { dir, .. } => Some(dir.as_path()),
            Credential::Ambient => None,
        };
        GhInvocation::new(Operation::new(op), intent, GhTarget::None, timeout)
            .forge_op(forge_op)
            .identity_scope(None, Some(&self.repo))
            .program(&self.gh_bin)
            .current_dir(&self.workspace_root)
            .gh_config_dir(dir)
            // 2am#4367 P4 break-glass: `fleet.json` is how a broken gateway
            // is unset, so this fetch must never depend on it.
            .gateway_exempt()
    }

    /// Run `inv` to an exit (any status); a spawn failure or timeout is `Err`.
    /// A timed-out write may still have been applied — the error says so.
    fn output(&self, inv: GhInvocation) -> Result<Output> {
        match inv.execute() {
            Ok(GhCompletion::Captured(Completion::Exited(out))) => Ok(out),
            Ok(_) => anyhow::bail!(
                "{} timed out after {}s (a write may still have been applied)",
                self.gh_bin,
                GH_TIMEOUT.as_secs()
            ),
            Err(e) => Err(e).with_context(|| format!("failed to invoke {}", self.gh_bin)),
        }
    }

    /// One read with no cache validator, on the read path: for a ref this
    /// transport itself wrote and is about to write again (the resync claim).
    ///
    /// Reader-only, like every store read (#12394, #12584). #10718 kept this
    /// on the writer because a reader App not installed on the repo answers
    /// `404`, which would read as "the ref is gone": a reader's `404` is now
    /// taken as the answer only when that reader can see the repository
    /// ([`GhTransport::reader_sees_repo`]); otherwise the next reader is
    /// asked, and with none left the read is shed (an error, never "absent").
    ///
    /// # Errors
    /// `gh` could not complete the request at all, or no reader can serve
    /// it; any HTTP status is `Ok`.
    pub(crate) fn get_fresh(&self, api_path: &str) -> Result<Reply> {
        self.read(api_path, None, None)
    }

    /// One store read (see the module docs): a reader's answer, the writer's
    /// on a host with no reader pool, or an error when it was shed.
    fn read(&self, api_path: &str, accept: Option<&str>, etag: Option<&str>) -> Result<Reply> {
        if !self.pooled {
            let (response, stderr, _) = self.run(&self.writer(), None, api_path, accept, etag)?;
            return reply_of(api_path, response, &stderr);
        }
        let family = served::endpoint_family(api_path);
        let mut tried: Vec<String> = Vec::new();
        loop {
            let Some((dir, app_id)) = self.reader.borrow().clone() else {
                break;
            };
            if tried.contains(&app_id) || tried.len() >= MAX_READERS_TRIED {
                break;
            }
            let cred = Credential::ConfigDir {
                dir,
                label: format!("reader app {app_id}"),
            };
            let (response, stderr, _) = self.run(&cred, Some(&app_id), api_path, accept, etag)?;
            let status = response.as_ref().map(|r| r.status);
            if matches!(status, Some(200..=299 | 304)) {
                served::note_reader_served(&app_id, &self.repo, family, SystemTime::now());
                return reply_of(api_path, response, &stderr);
            }
            let headers = response.as_ref().map(|r| &r.ratelimit);
            let resource = crate::forge_bucket_book::Resource::Core;
            let Some(failure) =
                crate::forge_identity::classify_failure(&stderr, status, headers, resource)
            else {
                // Not the credential's fault (a 5xx, a network error):
                // another reader would not help.
                return reply_of(api_path, response, &stderr);
            };
            if failure == Failure::Coverage
                && status == Some(404)
                && !names_blindness(&stderr)
                && self.reader_sees_repo(&cred, &app_id)?
            {
                // The reader sees the repository: the item is missing.
                return reply_of(api_path, response, &stderr);
            }
            (self.withdraw)(&app_id, &self.repo, failure, &format!("fleet_store {api_path}"));
            tried.push(app_id);
            *self.reader.borrow_mut() = (self.next_reader)();
        }
        anyhow::bail!(
            "gh api {api_path} was shed: no reader can serve {} (the writer is never used for \
             reads, #12394)",
            self.repo
        )
    }

    /// Whether reader `app_id` can see this transport's repository: it
    /// answered one of the repository's reads recently, or it answers
    /// `GET repos/OWNER/REPO` now (one extra request, only on a `404` with no
    /// such evidence).
    fn reader_sees_repo(&self, cred: &Credential, app_id: &str) -> Result<bool> {
        if served::reader_recently_served_repo(app_id, &self.repo, SystemTime::now()) {
            return Ok(true);
        }
        let path = format!("repos/{}", self.repo);
        let (response, _, _) = self.run(cred, Some(app_id), &path, None, None)?;
        Ok(response.is_some_and(|r| r.status == 200))
    }

    /// One GraphQL read on the read path (#10987, #12389): the workspace
    /// resync's batched default-branch head query, one request for every repo
    /// of one owner, through the facade's gateway-then-reader chain, targeted
    /// at this transport's repo so the route is the owner's. Booked as
    /// `fleet_store_graphql`. Not gateway-exempt: it is not the `fleet.json`
    /// fetch.
    ///
    /// **Reader-only** (operator ruling #12394): it never falls back to the
    /// writer. With no gateway or reader to serve it, the read is shed and
    /// this returns an error, so the owner's repos go unanswered and the pass
    /// asks their remotes (`git ls-remote`) instead. A reader that is not
    /// installed on one of the repos answers `NOT_FOUND` for it, so the
    /// caller must not take a fault from this answer as final.
    ///
    /// Returns the response body. `gh` exits non-zero when the answer carries
    /// any `errors` entry and still prints the partial `data`, so the caller
    /// parses the body and judges each alias. A failed request is shown to
    /// the rate-limit breaker; one with no body at all is an error.
    ///
    /// # Errors
    /// `gh` could not run, timed out, was shed, or printed no body.
    pub(crate) fn graphql_read(
        &self,
        forge_op: ForgeOp,
        query: &str,
        timeout: Duration,
    ) -> Result<String> {
        let field = format!("query={query}");
        let target = GhTarget::repo(&self.repo).map_err(|e| anyhow::anyhow!(e))?;
        let inv = GhInvocation::new(
            Operation::new("fleet_store_graphql"),
            AccessIntent::Read,
            target,
            timeout,
        )
        .forge_op(forge_op)
        .identity_scope(None, Some(&self.repo))
        .program(&self.gh_bin)
        .current_dir(&self.workspace_root)
        .args(["api", "graphql", "-f", field.as_str()]);
        self.graphql_body(inv, timeout)
    }

    /// Run a GraphQL query invocation and return its body (see
    /// [`Self::graphql_read`]).
    fn graphql_body(&self, inv: GhInvocation, timeout: Duration) -> Result<String> {
        let out = match inv.execute() {
            Ok(GhCompletion::Captured(Completion::Exited(out))) => out,
            Ok(GhCompletion::Shed { .. }) => {
                anyhow::bail!("the GraphQL query was shed: no gateway or reader can serve it")
            }
            Ok(_) => anyhow::bail!("the GraphQL query timed out after {}s", timeout.as_secs()),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to invoke {}", self.gh_bin));
            }
        };
        let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            // A GraphQL rate limit is a `200` whose body says so: the breaker
            // reads both. A per-alias `NOT_FOUND` does not trip it.
            crate::rate_limit_breaker::global_observe_failure(
                &format!("{stderr}\n{body}"),
                "fleet_store",
            );
        }
        if body.is_empty() {
            anyhow::bail!(
                "the GraphQL query got no answer: {}",
                if stderr.is_empty() {
                    "no output"
                } else {
                    &stderr
                }
            );
        }
        Ok(body)
    }

    /// One write call (`gh api --method POST/PUT … --input <file>`), always
    /// under the writer credential — never the reader, which is never granted
    /// write scope on the store (see the module docs). Any HTTP status,
    /// including a 403/404 that means the writer app's installation lacks
    /// `contents: write` / `pull_requests: write`, comes back as `Ok`; only a
    /// request `gh` itself could not complete at all is `Err`.
    pub(crate) fn write_raw(&self, method: &str, api_path: &str, body: &Value) -> Result<Reply> {
        let cred = self.writer();
        // The facade owns the child's stdin, so the body goes through a
        // private (`0600`) temp file rather than `--input -`.
        let mut input = tempfile::NamedTempFile::new().context("creating the gh api body file")?;
        let payload = serde_json::to_string(body).context("encoding the gh api request body")?;
        input
            .write_all(payload.as_bytes())
            .and_then(|()| input.flush())
            .context("writing the gh api request body")?;
        let mut inv = self
            .invocation(
                "fleet_store_write",
                ops::GIT_WRITE_REFS_AND_CONTENTS,
                AccessIntent::Write,
                &cred,
            )
            .args(["api", "--include", "--method", method, api_path]);
        // A DELETE carries no body (#10718: deleting the resync claim ref).
        if method != "DELETE" {
            inv = inv.arg("--input").arg(input.path());
        }
        let out = self.output(inv)?;
        let response =
            crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        *self.last_used.borrow_mut() = Some(cred.clone());
        let response = response.ok_or_else(|| {
            anyhow::anyhow!(
                "gh api {method} {api_path} failed before an HTTP response: {}",
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

impl super::propose::WriteTransport for GhTransport {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> Result<Reply> {
        self.write_raw(method, api_path, body)
    }
}

impl Transport for GhTransport {
    fn get(&self, api_path: &str, accept: Option<&str>, etag: Option<&str>) -> Result<Reply> {
        self.read(api_path, accept, etag)
    }
}

/// A store reply from one `gh api --include` run, or why it has none.
fn reply_of(
    api_path: &str,
    response: Option<crate::forge_listing::HttpResponse>,
    stderr: &str,
) -> Result<Reply> {
    let response = response.ok_or_else(|| {
        anyhow::anyhow!(
            "gh api {api_path} failed before an HTTP response: {}",
            if stderr.is_empty() {
                "no output"
            } else {
                stderr
            }
        )
    })?;
    Ok(Reply {
        status: response.status,
        etag: response.etag,
        body: response.body,
    })
}

/// GitHub's own words for a credential that cannot see the repository at
/// all, as opposed to a missing item (`forge_identity::served`).
fn names_blindness(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("resource not accessible by integration")
        || s.contains("could not resolve to a repository")
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
        ref outcome @ GithubAppOutcome::Minted { .. } => {
            let dir =
                cp::github_app_gh_config_dir_for_owner(workspace_root, cp::owner_of_nwo(repo));
            match cp::publish_outcome(&dir, outcome, repo) {
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

#[cfg(test)]
#[path = "gh_gateway_tests.rs"]
mod gateway_tests;

#[cfg(test)]
#[path = "gh_reader_tests.rs"]
mod reader_tests;
