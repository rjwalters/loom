//! The mirror's HTTP client: typed answers, the watermark check, and a
//! blocking bridge for the synchronous discovery callers.
//!
//! Every request is a `GET` with `Authorization: Bearer <key>`, a bounded
//! timeout and **redirects disabled** (a redirect would re-send the key
//! somewhere the operator did not configure). The key never appears in a URL,
//! an error or a log line.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::Active;

/// One request's deadline.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a mirror request did not produce an answer. Never carries the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorError {
    /// Connect, TLS, timeout or body read.
    Transport(String),
    /// `401` / `403`: the key is wrong or not scoped to the mirror.
    Auth(u16),
    /// Any other non-2xx.
    Status(u16),
    /// A 2xx whose body is not the expected shape.
    Malformed(String),
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Auth(s) => write!(f, "refused the key (HTTP {s})"),
            Self::Status(s) => write!(f, "HTTP {s}"),
            Self::Malformed(e) => write!(f, "malformed answer: {e}"),
        }
    }
}

/// The freshness watermark every answer carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Watermark {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub known: bool,
    #[serde(default)]
    pub last_event_at: Option<String>,
    #[serde(default)]
    pub last_reconciled_at: Option<String>,
    #[serde(default)]
    pub latest_seq: u64,
    #[serde(default)]
    pub served_at: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub gone: Option<String>,
}

/// Whether an answer at a watermark may be used for discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Within the bound.
    Fresh,
    /// Known, reconciled, but its newest evidence is older than the bound
    /// (or the answer itself is older than the bound by our clock).
    Stale,
    /// The mirror cannot vouch for the repo at all: never heard of it, it
    /// moved or was deleted, it has never completed a reconcile (its holdings
    /// may be only what webhooks happened to name), or the answer names a
    /// different repo (or names none).
    Unknown,
}

fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

impl Watermark {
    /// The discovery verdict for an answer about `repo` (lowercase
    /// `owner/name`) at this watermark, against `bound`, at local time `now`.
    ///
    /// Fresh needs all of: the mirror knows the repo, it has not moved, it has
    /// completed at least one reconcile (the backfill that makes its listing
    /// whole), and the newest of `last_event_at` / `last_reconciled_at` is
    /// within `bound` of `served_at`. The answer's own `served_at` must also
    /// be within `bound` of `now`, so a replayed or long-delayed answer, or a
    /// skewed clock, reads stale rather than fresh.
    #[must_use]
    pub fn freshness(&self, repo: &str, bound: Duration, now: DateTime<Utc>) -> Freshness {
        let named = self.repo.as_deref().map(str::to_ascii_lowercase);
        if !self.known || self.gone.is_some() || named.as_deref() != Some(repo) {
            return Freshness::Unknown;
        }
        let Some(reconciled) = parse_time(self.last_reconciled_at.as_deref()) else {
            return Freshness::Unknown;
        };
        let Some(served) = parse_time(self.served_at.as_deref()) else {
            return Freshness::Stale;
        };
        let bound = chrono::Duration::from_std(bound).unwrap_or(chrono::Duration::MAX);
        if (now - served).abs() > bound {
            return Freshness::Stale;
        }
        let newest =
            parse_time(self.last_event_at.as_deref()).map_or(reconciled, |e| e.max(reconciled));
        if served - newest > bound {
            Freshness::Stale
        } else {
            Freshness::Fresh
        }
    }
}

/// One issue or PR as the mirror holds it (`/v1/mirror/items`). Fields the
/// mirror has not learned yet are absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MirrorItem {
    pub number: u64,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub author_association: Option<String>,
    #[serde(default)]
    pub comments: Option<u64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub sub_issues_total: Option<u64>,
    #[serde(default)]
    pub sub_issues_completed: Option<u64>,
}

impl MirrorItem {
    /// The REST listing row this item stands in for, or `None` when it
    /// cannot be one (a number past `u32`, no state).
    #[must_use]
    pub fn to_rest(&self) -> Option<crate::forge_listing::RestIssue> {
        let number = u32::try_from(self.number).ok().filter(|n| *n > 0)?;
        let state = self.state.clone()?;
        let open_sub_issues = match (self.sub_issues_total, self.sub_issues_completed) {
            (Some(t), Some(c)) if c <= t => u32::try_from(t - c).ok(),
            _ => None,
        };
        Some(crate::forge_listing::RestIssue {
            number,
            title: self.title.clone(),
            labels: self.labels.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            closed_at: self.closed_at.clone(),
            state,
            body: self.body.clone(),
            author: self.author.clone(),
            author_association: self.author_association.clone(),
            is_pull_request: self.kind.as_deref() == Some("pr"),
            comments: self
                .comments
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(0),
            open_sub_issues,
        })
    }
}

/// One page of `/v1/mirror/items`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemsPage {
    #[serde(default)]
    pub items: Vec<MirrorItem>,
    #[serde(default)]
    pub next_after: Option<u64>,
    pub watermark: Watermark,
}

/// One repo's part of a `/v1/changes` answer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepoChanges {
    #[serde(default)]
    pub next_after: u64,
    #[serde(default)]
    pub more: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub watermark: Watermark,
}

/// One change summary. Only the routing fields are read; the rest is never
/// carried into a decision.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Change {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub item_type: Option<String>,
}

/// A `/v1/changes` answer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChangesPage {
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default)]
    pub more: bool,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoChanges>,
}

/// Build the outbound client: bounded timeout, redirects disabled.
///
/// # Errors
///
/// When the client cannot be built.
pub fn build_http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("could not construct the mirror HTTP client: {e}"))
}

/// `GET <base>/<path>?<query>` and parse the JSON body.
///
/// # Errors
///
/// [`MirrorError`]; a `401` also asks the refresher to re-read the key.
pub async fn get_json(
    http: &reqwest::Client,
    active: &Active,
    path: &str,
    query: &[(String, String)],
) -> Result<Value, MirrorError> {
    let mut url = reqwest::Url::parse(&format!("{}{path}", active.base_url))
        .map_err(|e| MirrorError::Transport(format!("bad url: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in query {
            q.append_pair(k, v);
        }
    }
    let res = http
        .get(url)
        .bearer_auth(active.key())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| MirrorError::Transport(classify_transport(&e)))?;
    let status = res.status().as_u16();
    if status == 401 || status == 403 {
        if status == 401 {
            super::note_key_rejected();
        }
        return Err(MirrorError::Auth(status));
    }
    if !(200..300).contains(&status) {
        return Err(MirrorError::Status(status));
    }
    res.json::<Value>()
        .await
        .map_err(|e| MirrorError::Malformed(classify_transport(&e)))
}

/// A transport error's class, never its URL (which carries no key, but may
/// carry repo names an operator did not expect in a log).
fn classify_transport(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connect".into()
    } else if e.is_decode() {
        "decode".into()
    } else if e.is_body() {
        "body".into()
    } else {
        "request".into()
    }
}

/// Parse `value` as `T`.
///
/// # Errors
///
/// [`MirrorError::Malformed`].
pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, MirrorError> {
    serde_json::from_value(value).map_err(|e| MirrorError::Malformed(e.to_string()))
}

// ---------------------------------------------------------------------------
// The blocking bridge
// ---------------------------------------------------------------------------

/// The discovery listings run synchronously, and some of their callers are
/// async tasks (the work finder calls its source from its own task). Their
/// requests run on this small runtime of their own, which is only ever
/// *built* and spawned onto, never entered with `block_on`, so first use is
/// safe from any thread, inside a tokio runtime or not; it is never dropped,
/// and its client's connections live as long as the process.
fn runtime() -> Option<&'static (tokio::runtime::Runtime, reqwest::Client)> {
    static RT: OnceLock<Option<(tokio::runtime::Runtime, reqwest::Client)>> = OnceLock::new();
    RT.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("loom-forge-mirror")
            .enable_all()
            .build()
            .map_err(|e| log::warn!("forge_mirror: no runtime for discovery reads ({e})"))
            .ok()?;
        // Building a reqwest client needs no runtime.
        let http = build_http()
            .map_err(|e| log::warn!("forge_mirror: {e}"))
            .ok()?;
        Some((rt, http))
    })
    .as_ref()
}

/// Wait for `rx`, telling a multi-thread tokio runtime the current worker is
/// about to block (so its other tasks move off it). A current-thread runtime
/// cannot hand its only worker over; there the wait simply blocks, bounded by
/// `deadline`, like the `gh` subprocesses the same callers already run.
fn wait<T>(rx: &std::sync::mpsc::Receiver<T>, deadline: Duration) -> Option<T> {
    let multi = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi {
        tokio::task::block_in_place(|| rx.recv_timeout(deadline).ok())
    } else {
        rx.recv_timeout(deadline).ok()
    }
}

/// [`get_json`] from synchronous code, bounded by `deadline` whatever the
/// network does.
///
/// # Errors
///
/// [`MirrorError`]; a missed deadline is [`MirrorError::Transport`].
pub fn get_json_blocking(
    active: &Active,
    path: &str,
    query: &[(String, String)],
    deadline: Duration,
) -> Result<Value, MirrorError> {
    let Some((rt, http)) = runtime() else {
        return Err(MirrorError::Transport("no runtime".into()));
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (http, active, path, query) =
        (http.clone(), active.clone(), path.to_string(), query.to_vec());
    rt.spawn(async move {
        let _ = tx.send(get_json(&http, &active, &path, &query).await);
    });
    wait(&rx, deadline).unwrap_or_else(|| Err(MirrorError::Transport("deadline".into())))
}
