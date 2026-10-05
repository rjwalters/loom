//! The GitHub REST surface the CI poller needs, behind a trait so the cycle
//! runs against recorded fixtures in tests (no live network in CI).
//!
//! Production ([`GhCliApi`]) keeps the daemon's zero-HTTP-client house style:
//! one `gh api --include` subprocess per request, parsed with the same
//! [`crate::forge_listing::parse_http_response`] the ETag-cached issue
//! listing uses, plus the three header families this poller additionally
//! needs — `Link` (pagination), and `Retry-After` / `X-RateLimit-*` (org-wide
//! backoff).

use std::fmt;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use crate::forge_call_stats::{ops, ForgeOp};
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

/// Deadline for one `gh api` read (#10089: they were unbounded `.output()`s).
/// Generous because a job-log document can be several MB.
const API_TIMEOUT: Duration = Duration::from_secs(120);

/// Deadline for one `gh run download` (redirect + blob fetch + unzip).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// A successful (2xx) or not-modified (304) response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub etag: Option<String>,
    /// The `rel="next"` pagination target, normalised to an API path.
    pub next: Option<String>,
    pub retry_after_secs: Option<u64>,
    pub ratelimit_remaining: Option<u64>,
    pub ratelimit_reset_epoch: Option<i64>,
    pub body: String,
}

/// Why a request failed. Every variant names its cause; the cycle surfaces
/// the `Display` text as the named `--once` failure reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// A primary (403 + exhausted budget) or secondary (403/429) rate limit.
    /// Backs off the **whole org**, never one repo.
    RateLimited {
        retry_after_secs: Option<u64>,
        reset_epoch: Option<i64>,
        detail: String,
    },
    /// Any other non-2xx/304 status.
    Http {
        status: u16,
        path: String,
        detail: String,
    },
    /// `gh` could not be run, or produced no HTTP response.
    Transport(String),
    /// A 2xx body that did not parse as the expected shape.
    Parse { path: String, detail: String },
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::RateLimited {
                retry_after_secs,
                reset_epoch,
                detail,
            } => write!(
                f,
                "rate-limited (retry_after={retry_after_secs:?}s, reset_epoch={reset_epoch:?}): {detail}"
            ),
            ApiError::Http {
                status,
                path,
                detail,
            } => write!(f, "HTTP {status} for {path}: {detail}"),
            ApiError::Transport(detail) => write!(f, "transport failure: {detail}"),
            ApiError::Parse { path, detail } => write!(f, "unparseable response for {path}: {detail}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// One conditional `GET`. `path` is relative to the API root (e.g.
/// `orgs/2amlogic/repos?per_page=100`).
pub trait GithubApi: Send + Sync {
    fn get(&self, path: &str, etag: Option<&str>) -> Result<ApiResponse, ApiError>;

    /// `GET` a plain-text document rather than a JSON body — the completed-job
    /// log endpoint (Issue #8825), which answers `302` to a signed blob URL.
    ///
    /// Two differences from [`get`](Self::get), both verified live on
    /// 2026-09-25: the final response comes from blob storage, so it carries
    /// **no** `ETag`/`X-RateLimit-*` headers (a rate limit can only surface on
    /// the pre-redirect GitHub response, which `classify` still catches); and
    /// the body routinely contains terminal escape sequences, which `gh api`
    /// refuses to emit unless told otherwise.
    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError>;

    /// One GraphQL `query` (Issue #9088): a PR's closing-issue references
    /// have no REST endpoint. The default refuses, so a client that never
    /// needs it (every test fake predating story stitching) is unchanged and
    /// a caller treats the refusal as "could not resolve", never as an answer.
    fn graphql(&self, _query: &str) -> Result<ApiResponse, ApiError> {
        Err(ApiError::Transport("this GitHub client does not support GraphQL".to_string()))
    }

    /// Download one run artifact by **name**, unpacked into `dest` (Issue
    /// #9089 — the suite-timings record `run-ci-suites.sh` uploads).
    ///
    /// Deliberately not expressed as a [`get`](Self::get) / [`get_document`]
    /// call: `/actions/artifacts/{id}/zip` answers a **zip**, and every other
    /// method here funnels its body through `String::from_utf8_lossy`, which
    /// would corrupt it. Rather than add a zip reader (and a dependency) for
    /// one small JSON file, this delegates the transfer *and* the unzip to
    /// `gh run download`, staying inside the daemon's zero-HTTP-client house
    /// style. The default refuses, so every pre-#9089 test fake compiles
    /// unchanged and a caller reads the refusal as "no artifact data", never
    /// as an empty artifact.
    fn download_artifact(
        &self,
        _repo: &str,
        _run_id: u64,
        _name: &str,
        _dest: &std::path::Path,
    ) -> Result<(), ApiError> {
        Err(ApiError::Transport(
            "this GitHub client does not support artifact download".to_string(),
        ))
    }
}

/// Bound a detail string so an HTML error page never floods a status file.
fn bounded(text: &str) -> String {
    let trimmed = text.trim();
    let mut out: String = trimmed.chars().take(200).collect();
    if trimmed.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// Classify a parsed response: 2xx/304 pass through, rate limits become
/// [`ApiError::RateLimited`], everything else [`ApiError::Http`].
pub fn classify(response: ApiResponse, path: &str) -> Result<ApiResponse, ApiError> {
    if (200..300).contains(&response.status) || response.status == 304 {
        return Ok(response);
    }
    let rate_limited = response.status == 429
        || (response.status == 403
            && (crate::rate_limit_breaker::indicates_rate_limit(&response.body)
                || response.ratelimit_remaining == Some(0)
                || response.retry_after_secs.is_some()));
    if rate_limited {
        return Err(ApiError::RateLimited {
            retry_after_secs: response.retry_after_secs,
            reset_epoch: response.ratelimit_reset_epoch,
            detail: format!("HTTP {} for {path}: {}", response.status, bounded(&response.body)),
        });
    }
    Err(ApiError::Http {
        status: response.status,
        path: path.to_string(),
        detail: bounded(&response.body),
    })
}

/// Extract the `rel="next"` target from a `Link` header value.
#[must_use]
pub fn parse_next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let mut pieces = part.split(';');
        let url = pieces.next()?.trim();
        let is_next = pieces.any(|p| p.trim() == "rel=\"next\"");
        if !is_next {
            return None;
        }
        let url = url.strip_prefix('<')?.strip_suffix('>')?;
        Some(normalise_api_path(url))
    })
}

/// Reduce an absolute API URL to a root-relative path `gh api` accepts on
/// both github.com (`https://api.github.com/x`) and GHES
/// (`https://host/api/v3/x`).
#[must_use]
pub fn normalise_api_path(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return url.trim_start_matches('/').to_string();
    };
    let path = rest.split_once('/').map_or("", |(_, p)| p);
    path.strip_prefix("api/v3/").unwrap_or(path).to_string()
}

/// Parse raw `gh api --include` output into an [`ApiResponse`] (any status).
#[must_use]
pub fn parse_raw(raw: &str) -> Option<ApiResponse> {
    let base = crate::forge_listing::parse_http_response(raw)?;
    let mut response = ApiResponse {
        status: base.status,
        etag: base.etag,
        body: base.body,
        ..ApiResponse::default()
    };
    for line in raw.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "link" => response.next = parse_next_link(value),
            "retry-after" => response.retry_after_secs = value.parse().ok(),
            "x-ratelimit-remaining" => response.ratelimit_remaining = value.parse().ok(),
            "x-ratelimit-reset" => response.ratelimit_reset_epoch = value.parse().ok(),
            _ => {}
        }
    }
    Some(response)
}

/// `owner/repo` from a `repos/<owner>/<repo>/…` API path (leading `/`
/// allowed), for picking the repo's reader. Anything else (`graphql`,
/// `orgs/…`) is `None` and runs on the writer.
fn repo_of_path(path: &str) -> Option<String> {
    let mut parts = path.trim_start_matches('/').split('/');
    if parts.next()? != "repos" {
        return None;
    }
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    let repo = repo.split(['?', '#']).next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// The inventoried forge operation one poller request serves (#9831).
///
/// [`ApiClient`] is a path-level trait, so the operation is read off the route
/// the request names — the same route strings the inventory rows list under
/// `github.routes` (`defaults/forge/operations/*.toml`), not a guess about the
/// caller. A route the inventory has no row for records `unknown`, with the
/// reason next to it.
fn ci_operation(path: &str) -> ForgeOp {
    let path = path.trim_start_matches('/');
    let route = path.split(['?', '#']).next().unwrap_or_default();
    let segs: Vec<&str> = route.split('/').collect();
    match segs.as_slice() {
        // The poller's only GraphQL query is `story.rs`'s batched
        // closing-issue-references lookup.
        ["graphql"] => ops::PR_CLOSING_ISSUE_REFERENCES,
        ["orgs" | "users", _, "repos"] => ops::REPO_LIST_FOR_OWNER,
        // The run listing and the jobs of one run: the run-state reads the
        // `ci.workflow-runs-for-sha` row covers (its callers list names this
        // file).
        ["repos", _, _, "actions", "runs"] | ["repos", _, _, "actions", "runs", _, "jobs"] => {
            ops::CI_WORKFLOW_RUNS_FOR_SHA
        }
        ["repos", _, _, "actions", "jobs", _, "logs"]
        | ["repos", _, _, "actions", "runs", _, "artifacts"]
        | ["repos", _, _, "actions", "artifacts", ..] => ops::CI_RUN_LOGS_AND_ARTIFACTS,
        ["users", _] => ForgeOp::uninventoried(
            "owner-kind probe (org vs user) has no inventory row; repo.list-for-owner names the listing only",
        ),
        ["repos", _, _, "check-runs", _, "annotations"] => {
            ForgeOp::uninventoried("check-run annotations have no inventory row")
        }
        _ => ForgeOp::uninventoried("route not mapped to an inventory row"),
    }
}

/// Production client: one `gh api --include` subprocess per request.
pub struct GhCliApi {
    gh_bin: PathBuf,
}

impl GhCliApi {
    /// `$LOOM_GH_BIN` (the crate-wide gh override) or `gh`.
    #[must_use]
    pub fn from_env() -> Self {
        GhCliApi {
            gh_bin: PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into())),
        }
    }
}

impl GhCliApi {
    /// Run one `gh api --include …` read and classify its output.
    ///
    /// #9537: a `repos/<owner>/<repo>/…` path is a repo-scoped read, so it
    /// runs under that repo's reader App when one is usable (readers carry
    /// `actions: read`). A rate limit or an auth/coverage refusal withdraws
    /// the reader (until the reported reset, when there is one) and the same
    /// call is retried once on the writer's credential.
    fn run(&self, path: &str, extra: &[&str]) -> Result<ApiResponse, ApiError> {
        use crate::forge_identity::Failure;
        let nwo = repo_of_path(path);
        let reader = nwo
            .as_deref()
            .and_then(|r| crate::forge_identity::read_credential(r, None));
        // The shared reader → writer retry (#9872, `reader_then_writer`).
        let attempt = crate::forge_identity::reader_then_writer(
            reader.as_ref().map(|(dir, _)| dir.as_path()),
            |dir, role| Ok::<_, std::convert::Infallible>(self.run_once(path, extra, dir, role)),
            Result::is_ok,
            |r| match r {
                Err(ApiError::RateLimited { .. } | ApiError::Http { status: 401, .. }) => {
                    Some(Failure::App)
                }
                Err(ApiError::Http {
                    status: 403 | 404, ..
                }) => Some(Failure::Coverage),
                _ => None,
            },
            |failure, failed| {
                let (Some((_, app_id)), Some(nwo)) = (&reader, nwo.as_deref()) else {
                    return;
                };
                // A rate limit withdraws the reader until the reported reset.
                let app_until = match (failure, failed) {
                    (Failure::App, Err(ApiError::RateLimited { reset_epoch, .. })) => reset_epoch
                        .and_then(|e| u64::try_from(e).ok())
                        .map(|e| std::time::UNIX_EPOCH + std::time::Duration::from_secs(e)),
                    _ => None,
                };
                let why = format!("ci_telemetry {path}");
                crate::forge_identity::withdraw_after(app_id, nwo, failure, app_until, &why);
            },
        );
        match attempt {
            Ok(result) => result,
            Err(never) => match never {},
        }
    }

    /// One `gh api --include …` invocation, under `reader_dir`'s credential
    /// when given, else the process's own.
    ///
    /// Built through [`GhInvocation`] (#10089): the facade records the call
    /// under `ci_telemetry` in [`crate::forge_call_stats`] from the
    /// `--include` status line and headers (the same
    /// [`crate::forge_call_stats::classify`] the hand-rolled record call it
    /// replaced used).
    fn run_once(
        &self,
        path: &str,
        extra: &[&str],
        reader_dir: Option<&std::path::Path>,
        role: crate::forge_identity::IdentityRole,
    ) -> Result<ApiResponse, ApiError> {
        let inv = GhInvocation::new(
            Operation::new("ci_telemetry"),
            AccessIntent::Read,
            GhTarget::None,
            API_TIMEOUT,
        )
        .forge_op(ci_operation(path))
        // Accounting only: the repo the route names (never the credential).
        .identity_scope(None, repo_of_path(path).as_deref())
        .program(&self.gh_bin)
        .args(["api", "--include"])
        .args(extra)
        .arg(path)
        .identity_role(role)
        .gh_config_dir(reader_dir);
        // A reader attempt must not be outranked by an env token (#9872).
        let inv = if role == crate::forge_identity::IdentityRole::Reader {
            inv.without_token_env()
        } else {
            inv
        };
        let output = self.captured(inv, &format!("gh api {path}"), API_TIMEOUT)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        match parse_raw(&stdout) {
            Some(response) => classify(response, path),
            None if crate::rate_limit_breaker::indicates_rate_limit(&stderr) => {
                Err(ApiError::RateLimited {
                    retry_after_secs: None,
                    reset_epoch: None,
                    detail: bounded(&stderr),
                })
            }
            None => Err(ApiError::Transport(format!(
                "gh api {path} produced no HTTP response: {}",
                bounded(&stderr)
            ))),
        }
    }

    /// Execute `inv`: its output when `gh` ran to an exit (any status), else
    /// a transport error naming `what` (could not start, or timed out).
    fn captured(
        &self,
        inv: GhInvocation,
        what: &str,
        timeout: Duration,
    ) -> Result<Output, ApiError> {
        match inv.execute() {
            Ok(GhCompletion::Captured(Completion::Exited(output))) => Ok(output),
            Ok(_) => {
                Err(ApiError::Transport(format!("{what} timed out after {}s", timeout.as_secs())))
            }
            Err(e) => {
                Err(ApiError::Transport(format!("could not run {}: {e}", self.gh_bin.display())))
            }
        }
    }
}

/// `gh` refuses to emit a body containing terminal escape sequences without
/// `--allow-escape-sequences`; a `gh` that predates the flag rejects it as
/// unknown. Either way the failure text names the flag, so the fallback is
/// keyed on that rather than on a version probe.
pub(crate) fn mentions_unknown_escape_flag(detail: &str) -> bool {
    let lowered = detail.to_ascii_lowercase();
    lowered.contains("allow-escape-sequences")
        && (lowered.contains("unknown flag") || lowered.contains("unknown command"))
}

impl GithubApi for GhCliApi {
    fn get(&self, path: &str, etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        let header = etag.map(|etag| format!("If-None-Match: {etag}"));
        let mut extra = vec!["-H", "Accept: application/vnd.github+json"];
        if let Some(header) = &header {
            extra.extend_from_slice(&["-H", header.as_str()]);
        }
        self.run(path, &extra)
    }

    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        match self.run(path, &["--allow-escape-sequences"]) {
            Err(ApiError::Transport(detail)) if mentions_unknown_escape_flag(&detail) => {
                log::warn!(
                    "ci_telemetry: this gh has no --allow-escape-sequences; a job log containing \
terminal escapes will be refused by gh rather than captured (upgrade gh to capture it)"
                );
                self.run(path, &[])
            }
            other => other,
        }
    }

    fn graphql(&self, query: &str) -> Result<ApiResponse, ApiError> {
        let field = format!("query={query}");
        self.run("graphql", &["-f", field.as_str()])
    }

    /// One `gh run download` subprocess (#9089). `gh` handles the redirect to
    /// blob storage and the unzip; a non-zero exit is a transport failure
    /// naming `gh`'s own stderr, which [`indicates_credential_failure`] and
    /// [`crate::rate_limit_breaker::indicates_rate_limit`] can still classify
    /// at the call site exactly as they do for a `gh api` failure.
    ///
    /// [`indicates_credential_failure`]: super::poll::indicates_credential_failure
    fn download_artifact(
        &self,
        repo: &str,
        run_id: u64,
        name: &str,
        dest: &std::path::Path,
    ) -> Result<(), ApiError> {
        let inv = GhInvocation::new(
            Operation::new("ci_telemetry.download"),
            AccessIntent::Read,
            // Not `GhTarget::repo`: that would switch the download onto the
            // owner's registered credential; it keeps the ambient one.
            GhTarget::None,
            DOWNLOAD_TIMEOUT,
        )
        .forge_op(ops::CI_RUN_LOGS_AND_ARTIFACTS)
        .identity_scope(None, Some(repo))
        .program(&self.gh_bin)
        .args(["run", "download"])
        .arg(run_id.to_string())
        .arg("--repo")
        .arg(repo)
        .arg("--name")
        .arg(name)
        .arg("--dir")
        .arg(dest);
        let output = self.captured(inv, &format!("gh run download {run_id}"), DOWNLOAD_TIMEOUT)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if crate::rate_limit_breaker::indicates_rate_limit(&stderr) {
            return Err(ApiError::RateLimited {
                retry_after_secs: None,
                reset_epoch: None,
                detail: bounded(&stderr),
            });
        }
        Err(ApiError::Transport(format!(
            "gh run download {run_id} --name {name} failed: {}",
            bounded(&stderr)
        )))
    }
}

#[cfg(test)]
mod repo_of_path_tests {
    use super::{ci_operation, ops, repo_of_path};

    #[test]
    fn poller_routes_map_to_their_inventoried_operation() {
        let cases = [
            ("graphql", ops::PR_CLOSING_ISSUE_REFERENCES),
            ("orgs/acme/repos?per_page=100&type=all", ops::REPO_LIST_FOR_OWNER),
            ("users/me/repos?per_page=100", ops::REPO_LIST_FOR_OWNER),
            (
                "repos/o/r/actions/runs?per_page=100&created=%3E%3D1",
                ops::CI_WORKFLOW_RUNS_FOR_SHA,
            ),
            ("repos/o/r/actions/runs/7/jobs?filter=all", ops::CI_WORKFLOW_RUNS_FOR_SHA),
            ("repos/o/r/actions/jobs/9/logs", ops::CI_RUN_LOGS_AND_ARTIFACTS),
            (
                "/repos/o/r/actions/runs/7/artifacts?per_page=100",
                ops::CI_RUN_LOGS_AND_ARTIFACTS,
            ),
            ("repos/o/r/actions/artifacts/3/zip", ops::CI_RUN_LOGS_AND_ARTIFACTS),
        ];
        for (path, op) in cases {
            assert_eq!(ci_operation(path), op, "{path}");
        }
        // Deliberately unmapped routes stay `unknown`, never a wrong row.
        for path in ["users/me", "repos/o/r/check-runs/5/annotations", "meta"] {
            assert_eq!(ci_operation(path).id(), None, "{path}");
        }
    }

    #[test]
    fn repo_scoped_paths_name_their_repo_and_others_do_not() {
        assert_eq!(
            repo_of_path("repos/2AMLogic/2am/actions/runs?per_page=5").as_deref(),
            Some("2AMLogic/2am")
        );
        assert_eq!(repo_of_path("/repos/o/r").as_deref(), Some("o/r"));
        assert_eq!(repo_of_path("repos/o/r?x=1").as_deref(), Some("o/r"));
        assert_eq!(repo_of_path("graphql"), None);
        assert_eq!(repo_of_path("orgs/o/installations"), None);
        assert_eq!(repo_of_path("repos/o"), None);
    }
}
