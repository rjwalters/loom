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
    /// Run one `gh api --include …` invocation and classify its output.
    fn run(&self, path: &str, extra: &[&str]) -> Result<ApiResponse, ApiError> {
        let mut cmd = Command::new(&self.gh_bin);
        cmd.arg("api").arg("--include");
        for argument in extra {
            cmd.arg(argument);
        }
        cmd.arg(path).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd.output().map_err(|e| {
            ApiError::Transport(format!("could not run {}: {e}", self.gh_bin.display()))
        })?;
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
}
