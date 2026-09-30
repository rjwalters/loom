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
use std::process::{Command, Stdio};

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
        let nwo = repo_of_path(path);
        let reader = nwo
            .as_deref()
            .and_then(|r| crate::forge_identity::read_credential(r, None));
        if let (Some((dir, app_id)), Some(nwo)) = (reader, nwo.as_deref()) {
            let first = self.run_once(path, extra, Some(&dir));
            let (failure, app_until) = match &first {
                Err(ApiError::RateLimited { reset_epoch, .. }) => (
                    Some(crate::forge_identity::Failure::App),
                    reset_epoch
                        .and_then(|e| u64::try_from(e).ok())
                        .map(|e| std::time::UNIX_EPOCH + std::time::Duration::from_secs(e)),
                ),
                Err(ApiError::Http { status: 401, .. }) => {
                    (Some(crate::forge_identity::Failure::App), None)
                }
                Err(ApiError::Http {
                    status: 403 | 404, ..
                }) => (Some(crate::forge_identity::Failure::Coverage), None),
                _ => (None, None),
            };
            let Some(failure) = failure else {
                return first;
            };
            let why = format!("ci_telemetry {path}");
            if failure == crate::forge_identity::Failure::App {
                crate::forge_identity::withdraw_after(&app_id, nwo, failure, app_until, &why);
                return self.run_once(path, extra, None);
            }
            // Coverage only when the writer CAN read it: a real 404 (a deleted
            // run) fails on both and must not withdraw the repo's reader.
            let second = self.run_once(path, extra, None);
            if second.is_ok() {
                crate::forge_identity::withdraw_after(&app_id, nwo, failure, None, &why);
            }
            return second;
        }
        self.run_once(path, extra, None)
    }

    /// One `gh api --include …` invocation, under `reader_dir`'s credential
    /// when given, else the process's own.
    fn run_once(
        &self,
        path: &str,
        extra: &[&str],
        reader_dir: Option<&std::path::Path>,
    ) -> Result<ApiResponse, ApiError> {
        let mut cmd = Command::new(&self.gh_bin);
        cmd.arg("api").arg("--include");
        for argument in extra {
            cmd.arg(argument);
        }
        if let Some(dir) = reader_dir {
            cmd.env("GH_CONFIG_DIR", dir);
        }
        cmd.arg(path).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd.output().map_err(|e| {
            ApiError::Transport(format!("could not run {}: {e}", self.gh_bin.display()))
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // #9251: per-caller accounting (a local write, never a forge call).
        let base = crate::forge_listing::parse_http_response(&stdout);
        let exit_ok = output.status.success();
        crate::forge_call_stats::record_gh_api("ci_telemetry", base.as_ref(), exit_ok, &stderr);
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
        let output = Command::new(&self.gh_bin)
            .arg("run")
            .arg("download")
            .arg(run_id.to_string())
            .arg("--repo")
            .arg(repo)
            .arg("--name")
            .arg(name)
            .arg("--dir")
            .arg(dest)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                ApiError::Transport(format!("could not run {}: {e}", self.gh_bin.display()))
            })?;
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
    use super::repo_of_path;

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
