//! Distributing the captain's fit to every host (#10395).
//!
//! With `fleet.captain` declared only the captain refreshes the fleet
//! snapshots (#10329), so only it can fit. Without distribution every other
//! host's `land-2026-10-04-twin-otter` would stop learning and end at
//! `no_model`. The captain therefore **publishes** each fit and every other
//! host **fetches, verifies and installs** the newest one into its `fit_dir`;
//! the registry's `load_latest` / `swap_fit` reload path (#10243) does the rest,
//! so no reload code lives here.
//!
//! # Transport
//!
//! A dedicated branch of the fleet store (`fleet.repo`), `fleet.etaFitRef`
//! (default [`DEFAULT_REF`]), written through the contents API by the
//! captain's writer App and read through the reader/writer App every host
//! already resolves for the store ([`crate::fleet_store::gh::GhTransport`]).
//! No new credential. Not `main`: a daily machine artifact does not belong in
//! reviewed state, and `main`'s ruleset would block the push. The branch is
//! created from `fleet.ref` on first publish. Contract:
//!
//! - `eta/fit/<fit_id>.json`: the coefficient file, byte-for-byte
//!   [`coeffs::to_json`];
//! - `eta/fit/latest.json`: the [`Envelope`] (`eta-fit-pub/v1`), written
//!   **last**, so it never names a file that is not there yet.
//!
//! # Integrity
//!
//! Provenance (captain host, sha) lives in the envelope, not the fit:
//! `FitMeta` excludes the host so the fit stays deterministic (#10245). The
//! sha256 is over the exact published bytes, because `fit::read` does not
//! re-derive `id` (a float may be one ulp off after a round trip). [`verify`]
//! lists the checks, in order; any failure keeps the previous fit.
//!
//! # State
//!
//! `<fit_dir>/../fit-pub/status.json` ([`PubStatus`]): the conditional-fetch
//! validator, the last outcome and what was last published. Outside `fit_dir`
//! on purpose: `load_latest` parses every `*.json` there.

use super::coeffs::{self, CoefficientFile, Fitter};
use super::FEATURES;
use crate::fleet_store::fetch::{write_atomic, Transport};
use crate::fleet_store::propose::WriteTransport;
use crate::fleet_store::StoreLocation;
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The envelope's schema tag.
pub const PUB_SCHEMA: &str = "eta-fit-pub/v1";
/// Branch published to when `fleet.etaFitRef` is unset.
pub const DEFAULT_REF: &str = "eta-fit";
/// Config key of the publication branch.
pub const REF_KEY: &str = "fleet.etaFitRef";
/// Config key of the staleness bound, in days.
pub const MAX_AGE_KEY: &str = "fleet.etaFitMaxAgeDays";
/// A publication older than this (by `as_of`) is stale and not installed.
pub const DEFAULT_MAX_AGE_DAYS: i64 = 3;
/// Store path of the envelope.
pub const LATEST_PATH: &str = "eta/fit/latest.json";
/// Clock skew tolerated on a published `as_of`.
const FUTURE_SLACK_MIN: i64 = 5;

/// The training window as the envelope reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvWindow {
    /// The fit's `window.start`.
    pub start: DateTime<Utc>,
    /// The fit's cutoff (`as_of`).
    pub end: DateTime<Utc>,
}

/// `eta-fit-pub/v1`: provenance and integrity of one published fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// [`PUB_SCHEMA`].
    pub schema: String,
    /// The fit's `id`.
    pub fit_id: String,
    /// The fit's cutoff.
    pub as_of: DateTime<Utc>,
    /// The training window.
    pub window: EnvWindow,
    /// The declared captain that fitted and published.
    pub captain_host: String,
    /// The build that fitted.
    pub fitter: Fitter,
    /// Bare file name, `<fit_id>.json`.
    pub file: String,
    /// Hex sha256 of the file's exact bytes.
    pub sha256: String,
    /// When it was published.
    pub published_at: DateTime<Utc>,
}

/// Hex sha256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The envelope for `file` published as `bytes` by `captain`.
#[must_use]
pub fn envelope_for(
    file: &CoefficientFile,
    bytes: &[u8],
    captain: &str,
    now: DateTime<Utc>,
) -> Envelope {
    Envelope {
        schema: PUB_SCHEMA.to_string(),
        fit_id: file.id.clone(),
        as_of: file.as_of,
        window: EnvWindow {
            start: file.window.start,
            end: file.as_of,
        },
        captain_host: captain.to_string(),
        fitter: file.fitter.clone(),
        file: format!("{}.json", file.id),
        sha256: sha256_hex(bytes),
        published_at: now,
    }
}

/// Why a published fit was not installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The envelope is not valid `eta-fit-pub/v1` JSON.
    BadEnvelope(String),
    /// `file` is not a bare `<16 hex>.json` name.
    BadFileName(String),
    /// The fetched bytes do not hash to the envelope's sha (tampered or partial).
    ShaMismatch,
    /// The bytes are not an `eta-fit/v1` file.
    BadFit,
    /// The file's id, `as_of` or window disagrees with the envelope.
    EnvelopeMismatch(String),
    /// Published by a host that is not the declared captain.
    WrongCaptain {
        /// The envelope's `captain_host`.
        published_by: String,
    },
    /// The file's features are not this build's.
    Incompatible(String),
    /// `as_of` is in the future.
    FutureAsOf,
    /// `as_of` is older than the newest local fit.
    OlderThanLocal,
    /// `as_of` is older than the staleness bound.
    Stale,
}

impl Refusal {
    /// A short stable code, for the status file and `eta doctor`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::BadEnvelope(_) => "bad_envelope",
            Refusal::BadFileName(_) => "bad_file_name",
            Refusal::ShaMismatch => "sha_mismatch",
            Refusal::BadFit => "bad_fit",
            Refusal::EnvelopeMismatch(_) => "envelope_mismatch",
            Refusal::WrongCaptain { .. } => "wrong_captain",
            Refusal::Incompatible(_) => "incompatible",
            Refusal::FutureAsOf => "future_as_of",
            Refusal::OlderThanLocal => "older_than_local",
            Refusal::Stale => "stale",
        }
    }

    /// A one-line explanation.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Refusal::BadEnvelope(e) => format!("envelope is not eta-fit-pub/v1: {e}"),
            Refusal::BadFileName(n) => format!("file name `{n}` is not <16 hex>.json"),
            Refusal::ShaMismatch => "fetched bytes do not match the envelope sha256".to_string(),
            Refusal::BadFit => "file is not a readable eta-fit/v1 coefficient file".to_string(),
            Refusal::EnvelopeMismatch(w) => format!("file disagrees with its envelope: {w}"),
            Refusal::WrongCaptain { published_by } => {
                format!("published by `{published_by}`, not the declared captain")
            }
            Refusal::Incompatible(w) => format!("incompatible fit: {w}"),
            Refusal::FutureAsOf => "as_of is in the future".to_string(),
            Refusal::OlderThanLocal => "as_of is older than the newest local fit".to_string(),
            Refusal::Stale => "publication is older than the staleness bound".to_string(),
        }
    }
}

/// What a fetched publication is checked against.
#[derive(Debug, Clone)]
pub struct VerifyCtx<'a> {
    /// The declared `fleet.captain`.
    pub captain: &'a str,
    /// The clock.
    pub now: DateTime<Utc>,
    /// The newest local fit's `as_of`, if any.
    pub local_as_of: Option<DateTime<Utc>>,
    /// Staleness bound.
    pub max_age: Duration,
}

/// A bare `<16 lowercase hex>.json` name (no separators, no traversal).
#[must_use]
pub fn is_bare_fit_name(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(|stem| {
        stem.len() == 16 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Parse an envelope's bytes and check its schema and file name (checks 1).
///
/// # Errors
///
/// [`Refusal::BadEnvelope`] or [`Refusal::BadFileName`].
pub fn parse_envelope(bytes: &[u8]) -> Result<Envelope, Refusal> {
    let env: Envelope =
        serde_json::from_slice(bytes).map_err(|e| Refusal::BadEnvelope(e.to_string()))?;
    if env.schema != PUB_SCHEMA {
        return Err(Refusal::BadEnvelope(format!("schema `{}`", env.schema)));
    }
    if !is_bare_fit_name(&env.file) {
        return Err(Refusal::BadFileName(env.file));
    }
    Ok(env)
}

/// Verify `env` and the fetched `bytes`, in order: sha256, parse, id and
/// as-of and window against the envelope, captain, compatibility, then the
/// as-of bounds (future, stale, older than local). The file is returned only
/// when every check passes. (The envelope's own schema and file name are
/// [`parse_envelope`]'s.)
///
/// # Errors
///
/// The first failed check.
pub fn verify(
    env: &Envelope,
    bytes: &[u8],
    ctx: &VerifyCtx<'_>,
) -> Result<CoefficientFile, Refusal> {
    if sha256_hex(bytes) != env.sha256 {
        return Err(Refusal::ShaMismatch);
    }
    let file: CoefficientFile = serde_json::from_slice(bytes).map_err(|_| Refusal::BadFit)?;
    if file.schema != super::SCHEMA {
        return Err(Refusal::BadFit);
    }
    if file.id != env.fit_id || env.file != format!("{}.json", file.id) {
        return Err(Refusal::EnvelopeMismatch("id".to_string()));
    }
    if file.as_of != env.as_of {
        return Err(Refusal::EnvelopeMismatch("as_of".to_string()));
    }
    if file.window.start != env.window.start || env.window.end != file.as_of {
        return Err(Refusal::EnvelopeMismatch("window".to_string()));
    }
    if env.captain_host != ctx.captain {
        return Err(Refusal::WrongCaptain {
            published_by: env.captain_host.clone(),
        });
    }
    if file.fitter != env.fitter {
        return Err(Refusal::EnvelopeMismatch("fitter".to_string()));
    }
    // A different build may fit; a different feature set cannot be served.
    if file.features.iter().map(String::as_str).ne(FEATURES) {
        return Err(Refusal::Incompatible("feature set differs from this build".to_string()));
    }
    if file.as_of > ctx.now + Duration::minutes(FUTURE_SLACK_MIN) {
        return Err(Refusal::FutureAsOf);
    }
    if ctx.now - file.as_of > ctx.max_age {
        return Err(Refusal::Stale);
    }
    if ctx.local_as_of.is_some_and(|local| file.as_of < local) {
        return Err(Refusal::OlderThanLocal);
    }
    Ok(file)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// What the last fetch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchKind {
    /// A new fit was verified and installed.
    Installed,
    /// The published fit is already the newest local one.
    Current,
    /// Nothing new since the last good fetch (a 304).
    NotModified,
    /// The store has no publication (no branch, or no `latest.json`).
    Absent,
    /// The store could not be read.
    FetchError,
    /// A publication was fetched and refused; the previous fit stays.
    Refused,
    /// The publication is older than the staleness bound.
    Stale,
}

impl FetchKind {
    /// Whether this host is serving the captain's published fit, so a local
    /// fit would only diverge from it.
    #[must_use]
    pub fn serving_published(self) -> bool {
        matches!(self, Self::Installed | Self::Current | Self::NotModified)
    }
}

/// The persisted state, for the conditional fetch and for `eta doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubStatus {
    /// `latest.json`'s validator, kept only after a good fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// When the last fetch ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
    /// The last fetch's outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FetchKind>,
    /// The refusal code, or the error, when the last fetch did not succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The published fit seen last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_id: Option<String>,
    /// Its captain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captain_host: Option<String>,
    /// Its cutoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    /// When it was published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<Utc>>,
    /// Captain side: what was last published successfully, and where and as
    /// whom. Cleared whenever this host runs as a non-captain, so a host that
    /// regains captaincy re-checks the store instead of trusting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_published: Option<Publication>,
    /// Captain side: the last publish failure, cleared on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_error: Option<String>,
}

/// The identity of one publication: the fit, the captain it was published as,
/// and the destination. A change in any part (a captain failover, a new
/// `fleet.repo` or `fleet.etaFitRef`) is a new publication, never a skip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    /// The fit's `id`.
    pub fit_id: String,
    /// The captain it was published as.
    pub captain_host: String,
    /// The store repo (`fleet.repo`).
    pub repo: String,
    /// The publication branch (`fleet.etaFitRef`).
    pub reference: String,
}

impl Publication {
    /// The identity of publishing `fit_id` to `loc` as `captain`.
    #[must_use]
    pub fn new(fit_id: &str, captain: &str, loc: &StoreLocation) -> Self {
        Self {
            fit_id: fit_id.to_string(),
            captain_host: captain.to_string(),
            repo: loc.repo.clone(),
            reference: loc.reference.clone(),
        }
    }
}

/// `<fit_dir>/../fit-pub/status.json`.
#[must_use]
pub fn status_path(root: &Path) -> PathBuf {
    coeffs::fit_dir(root)
        .with_file_name("fit-pub")
        .join("status.json")
}

/// The persisted status; default when absent or unreadable.
#[must_use]
pub fn read_status(root: &Path) -> PubStatus {
    std::fs::read_to_string(status_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_status(root: &Path, status: &PubStatus) {
    let path = status_path(root);
    let text = serde_json::to_string_pretty(status).unwrap_or_default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = write_atomic(&path, text.as_bytes()) {
        log::warn!("eta fit publish: could not write {}: {e:#}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Fetch (non-captain)
// ---------------------------------------------------------------------------

pub(crate) fn contents_path(loc: &StoreLocation, path: &str) -> String {
    format!("repos/{}/contents/{path}?ref={}", loc.repo, loc.reference)
}

pub(crate) const RAW: &str = "application/vnd.github.raw+json";

/// Fetch the captain's newest publication, verify it and install it into
/// `fit_dir`. Never panics and never leaves a half-written file: an error of
/// any kind is a [`FetchKind`] outcome, recorded in the status file, and the
/// previous fit stays in place. `loc.reference` is the publication branch.
pub fn fetch_and_install(
    transport: &dyn Transport,
    loc: &StoreLocation,
    root: &Path,
    captain: &str,
    now: DateTime<Utc>,
    max_age: Duration,
) -> FetchKind {
    let mut status = read_status(root);
    // Running as a non-captain ends any captaincy this host held: forget what
    // it published, so if it is captain again it re-checks the store (which
    // may now name another captain) rather than skipping on stale state.
    status.last_published = None;
    let (kind, reason) = match fetch_inner(transport, loc, root, captain, now, max_age, &mut status)
    {
        Ok(kind) => (kind, None),
        Err(FetchFail::Refused(r, env)) => {
            if let Some(env) = env {
                status.fit_id = Some(env.fit_id);
                status.captain_host = Some(env.captain_host);
                status.as_of = Some(env.as_of);
                status.published_at = Some(env.published_at);
            }
            let kind = if r == Refusal::Stale {
                FetchKind::Stale
            } else {
                FetchKind::Refused
            };
            log::warn!("eta fit publish: refused the published fit: {}", r.detail());
            (kind, Some(r.code().to_string()))
        }
        Err(FetchFail::Error(e)) => {
            log::warn!("eta fit publish: fetch failed, keeping the local fit: {e:#}");
            (FetchKind::FetchError, Some(format!("{e:#}")))
        }
    };
    if !matches!(kind, FetchKind::Installed | FetchKind::Current | FetchKind::NotModified) {
        status.etag = None;
    }
    status.checked_at = Some(now);
    status.kind = Some(kind);
    status.reason = reason;
    write_status(root, &status);
    kind
}

enum FetchFail {
    Refused(Refusal, Option<Box<Envelope>>),
    Error(anyhow::Error),
}

impl From<anyhow::Error> for FetchFail {
    fn from(e: anyhow::Error) -> Self {
        FetchFail::Error(e)
    }
}

fn fetch_inner(
    transport: &dyn Transport,
    loc: &StoreLocation,
    root: &Path,
    captain: &str,
    now: DateTime<Utc>,
    max_age: Duration,
    status: &mut PubStatus,
) -> Result<FetchKind, FetchFail> {
    let local = coeffs::load_latest(root, now + Duration::days(36_500));
    // A 304 means "still serving the last good fetch", which holds only while
    // that fetch's captain is still the declared one and its fit is still the
    // newest local file. Otherwise fetch in full, so `verify` runs again.
    let still_serving = status.captain_host.as_deref() == Some(captain)
        && status.fit_id.is_some()
        && local.as_ref().map(|f| f.id.as_str()) == status.fit_id.as_deref();
    let etag = status.etag.as_deref().filter(|_| still_serving);
    let reply = transport
        .get(&contents_path(loc, LATEST_PATH), Some(RAW), etag)
        .context("reading the published envelope")?;
    match reply.status {
        304 => {
            // Nothing new; the installed fit may have aged out meanwhile.
            let stale = status.as_of.is_some_and(|a| now - a > max_age);
            return Ok(if stale {
                FetchKind::Stale
            } else {
                FetchKind::NotModified
            });
        }
        404 => return Ok(FetchKind::Absent),
        200 => {}
        s => {
            return Err(anyhow!(
                "forge answered HTTP {s} reading {LATEST_PATH} @ {}",
                loc.reference
            )
            .into())
        }
    }
    let env = parse_envelope(reply.body.as_bytes()).map_err(|r| FetchFail::Refused(r, None))?;
    let file_reply = transport
        .get(&contents_path(loc, &format!("eta/fit/{}", env.file)), Some(RAW), None)
        .context("reading the published fit")?;
    if file_reply.status != 200 {
        return Err(
            anyhow!("forge answered HTTP {} reading {}", file_reply.status, env.file).into()
        );
    }
    let bytes = file_reply.body.into_bytes();
    let ctx = VerifyCtx {
        captain,
        now,
        local_as_of: local.as_ref().map(|f| f.as_of),
        max_age,
    };
    let fit = verify(&env, &bytes, &ctx)
        .map_err(|r| FetchFail::Refused(r, Some(Box::new(env.clone()))))?;
    status.fit_id = Some(env.fit_id.clone());
    status.captain_host = Some(env.captain_host.clone());
    status.as_of = Some(env.as_of);
    status.published_at = Some(env.published_at);
    if local.as_ref().is_some_and(|l| l.id == fit.id) {
        status.etag = reply.etag;
        return Ok(FetchKind::Current);
    }
    let path = coeffs::fit_dir(root).join(coeffs::path_for(fit.as_of));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_atomic(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    // Retention, as for a local fit.
    super::run::prune_dir(&coeffs::fit_dir(root), super::run::RETAIN_FILES);
    status.etag = reply.etag;
    log::info!(
        "eta fit publish: installed the captain's fit {} (as_of {}) into {}",
        fit.id,
        fit.as_of.to_rfc3339(),
        path.display()
    );
    Ok(FetchKind::Installed)
}

// ---------------------------------------------------------------------------
// Publish (captain)
// ---------------------------------------------------------------------------

/// What a publish did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishKind {
    /// The fit and envelope were written.
    Published,
    /// The store already holds this exact publication (same envelope apart
    /// from `published_at`, same captain, fit bytes present); nothing written.
    AlreadyPublished,
}

pub(crate) fn ensure_ok(
    reply: &crate::fleet_store::fetch::Reply,
    what: &str,
    repo: &str,
) -> Result<()> {
    if (200..300).contains(&reply.status) {
        return Ok(());
    }
    let hint = if reply.status == 403 || reply.status == 404 || reply.status == 422 {
        " — check the writer App has contents:write on the store and the eta-fit branch is \
         exempt from the main ruleset"
    } else {
        ""
    };
    bail!("{what} in {repo}: HTTP {}{hint}", reply.status)
}

/// The blob sha of `path` on the branch, or `None` when absent.
pub(crate) fn blob_sha(
    t: &dyn Transport,
    loc: &StoreLocation,
    path: &str,
) -> Result<Option<String>> {
    let r = t.get(&contents_path(loc, path), None, None)?;
    match r.status {
        200 => {
            let v: Value = serde_json::from_str(&r.body).context("malformed contents response")?;
            Ok(v.get("sha").and_then(Value::as_str).map(str::to_string))
        }
        404 => Ok(None),
        s => bail!("HTTP {s} reading {path} in {}", loc.repo),
    }
}

pub(crate) fn put_file(
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    path: &str,
    body: &[u8],
    existing: Option<String>,
    message: &str,
) -> Result<()> {
    let mut payload = json!({
        "message": message,
        "content": general_purpose::STANDARD.encode(body),
        "branch": loc.reference,
    });
    if let Some(sha) = existing {
        payload["sha"] = Value::String(sha);
    }
    let reply = wt.write("PUT", &format!("repos/{}/contents/{path}", loc.repo), &payload)?;
    ensure_ok(&reply, &format!("writing {path}"), &loc.repo)
}

/// Create the publication branch from `base_ref` when it does not exist.
pub(crate) fn ensure_branch(
    t: &dyn Transport,
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    base_ref: &str,
) -> Result<()> {
    let r = t.get(&format!("repos/{}/git/ref/heads/{}", loc.repo, loc.reference), None, None)?;
    match r.status {
        200 => return Ok(()),
        404 => {}
        s => bail!("HTTP {s} checking branch {} in {}", loc.reference, loc.repo),
    }
    let base = t.get(
        &format!("repos/{}/commits/{base_ref}", loc.repo),
        Some("application/vnd.github.sha"),
        None,
    )?;
    if base.status != 200 {
        bail!("HTTP {} resolving {base_ref} in {}", base.status, loc.repo);
    }
    let reply = wt.write(
        "POST",
        &format!("repos/{}/git/refs", loc.repo),
        &json!({"ref": format!("refs/heads/{}", loc.reference), "sha": base.body.trim()}),
    )?;
    ensure_ok(&reply, "creating the publication branch", &loc.repo)
}

/// `fleet.etaFitRef` is spliced into store request paths, a query string and
/// the PUT `branch` field, so it must pass the same check as `fleet.ref`
/// ([`crate::fleet_store::validate_ref`]) and be a bare branch name: no
/// `refs/` or `heads/` prefix and no empty or dot-led segment (so no trailing
/// `/`). A non-canonical spelling would make the written branch ambiguous.
///
/// # Errors
///
/// When `reference` is not a plain, canonical branch name.
pub fn validate_publication_ref(reference: &str) -> Result<()> {
    validate_branch_for(REF_KEY, reference)
}

/// [`validate_publication_ref`] for the publication branch configured under
/// `key` — shared with the captain gauges heartbeat
/// (`observability::captain_gauges::store`), which publishes beside the fit.
///
/// # Errors
///
/// When `reference` is not a plain, canonical branch name.
pub(crate) fn validate_branch_for(key: &str, reference: &str) -> Result<()> {
    crate::fleet_store::validate_ref(reference)
        .with_context(|| format!("`{key}` `{reference}` is invalid"))?;
    if reference.starts_with("refs/")
        || reference.starts_with("heads/")
        || reference
            .split('/')
            .any(|seg| seg.is_empty() || seg.starts_with('.'))
    {
        bail!(
            "`{key}` `{reference}` must be a bare branch name \
             (no `refs/` or `heads/` prefix, no empty or dot-led segment)"
        );
    }
    Ok(())
}

/// The branch `reference` names, for comparison only: trimmed, without a
/// leading `refs/heads/` or `heads/`, without trailing `/`.
fn branch_name(reference: &str) -> &str {
    let r = reference.trim();
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("heads/"))
        .unwrap_or(r)
        .trim_end_matches('/')
}

/// This autonomous write may land only on the dedicated publication branch,
/// never on the store's reviewed branch (`fleet.ref`) or `main`: that is part
/// of how `write_scope::tests::daemon_write_paths_are_scoped` scopes this file
/// (`FleetStore`), so it is enforced here rather than left to the ruleset.
/// The publication branch must be valid and canonical
/// ([`validate_publication_ref`]); both sides are normalized
/// ([`branch_name`]) and compared case-insensitively, so `refs/heads/main`,
/// `Main` or `fleet.ref = refs/heads/stable` against `stable` are refused.
pub(crate) fn refuse_reviewed_branch(loc: &StoreLocation, base_ref: &str) -> Result<()> {
    refuse_reviewed_branch_for(REF_KEY, "the eta fit", loc, base_ref)
}

/// [`refuse_reviewed_branch`] for an artifact `what` whose branch is
/// configured under `key`: the same validation and the same refusal of the
/// store's reviewed branch and `main`.
///
/// # Errors
///
/// When `loc.reference` is invalid or names the reviewed branch or `main`.
pub(crate) fn refuse_reviewed_branch_for(
    key: &str,
    what: &str,
    loc: &StoreLocation,
    base_ref: &str,
) -> Result<()> {
    validate_branch_for(key, &loc.reference)?;
    let r = branch_name(&loc.reference);
    if r.eq_ignore_ascii_case(branch_name(base_ref)) || r.eq_ignore_ascii_case("main") {
        bail!(
            "refusing to publish {what} to `{r}` in {}: `{key}` must name a dedicated \
             branch, not the store's reviewed branch",
            loc.repo
        );
    }
    Ok(())
}

/// Whether `cur` (the store's envelope) is the publication `env` would make:
/// every field equal (schema, fit, window, **captain**, fitter, file, sha)
/// except `published_at`. A former captain's envelope for the same fit is
/// not: the new captain must republish it, or every host would refuse it.
pub(crate) fn same_publication(cur: &Envelope, env: &Envelope) -> bool {
    Envelope {
        published_at: env.published_at,
        ..cur.clone()
    } == *env
}

/// Publish `fit` to `loc` (its `reference` is the publication branch,
/// created from `base_ref` if missing) as `captain`: the file first (unless
/// the store already holds these exact bytes), the envelope last.
/// Idempotent: a store whose envelope is already this publication
/// ([`same_publication`]) and whose fit bytes are present is left alone.
///
/// # Errors
///
/// Any store failure; nothing partial is ever named by the envelope.
pub fn publish(
    t: &dyn Transport,
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    base_ref: &str,
    fit: &CoefficientFile,
    captain: &str,
    now: DateTime<Utc>,
) -> Result<PublishKind> {
    refuse_reviewed_branch(loc, base_ref)?;
    let bytes = coeffs::to_json(fit).into_bytes();
    let env = envelope_for(fit, &bytes, captain, now);
    ensure_branch(t, wt, loc, base_ref)?;

    let latest = t.get(&contents_path(loc, LATEST_PATH), Some(RAW), None)?;
    let current = latest.status == 200
        && parse_envelope(latest.body.as_bytes()).is_ok_and(|cur| same_publication(&cur, &env));
    let file_path = format!("eta/fit/{}", env.file);
    let stored = t.get(&contents_path(loc, &file_path), Some(RAW), None)?;
    let file_ok = match stored.status {
        200 => sha256_hex(stored.body.as_bytes()) == env.sha256,
        404 => false,
        s => bail!("HTTP {s} reading {file_path} in {}", loc.repo),
    };
    if current && file_ok {
        return Ok(PublishKind::AlreadyPublished);
    }
    let msg = format!("eta fit {} (as_of {})", fit.id, fit.as_of.to_rfc3339());
    if !file_ok {
        // Absent, or other bytes under this name: the envelope will name
        // these bytes' sha, so the store must hold exactly them.
        let existing = if stored.status == 404 {
            None
        } else {
            blob_sha(t, loc, &file_path)?
        };
        put_file(wt, loc, &file_path, &bytes, existing, &msg)?;
    }
    let latest_sha = blob_sha(t, loc, LATEST_PATH)?;
    let env_text = serde_json::to_string_pretty(&env).context("encoding the envelope")? + "\n";
    put_file(wt, loc, LATEST_PATH, env_text.as_bytes(), latest_sha, &msg)?;
    Ok(PublishKind::Published)
}

/// Publish the newest local fit unless this host already published it, as
/// this captain, to this destination ([`Publication`]). A failure is logged
/// and recorded, never propagated: it must not fail the fit or the refresh
/// cycle. Returns the kind on success.
pub fn publish_newest(
    t: &dyn Transport,
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    base_ref: &str,
    root: &Path,
    captain: &str,
    now: DateTime<Utc>,
) -> Option<PublishKind> {
    let fit = coeffs::load_latest(root, now + Duration::days(1))?;
    let mut status = read_status(root);
    let publication = Publication::new(&fit.id, captain, loc);
    if status.last_published.as_ref() == Some(&publication) {
        return None;
    }
    match publish(t, wt, loc, base_ref, &fit, captain, now) {
        Ok(kind) => {
            log::info!(
                "eta fit publish: {kind:?} fit {} to {} @ {}",
                fit.id,
                loc.repo,
                loc.reference
            );
            status.last_published = Some(publication);
            status.publish_error = None;
            write_status(root, &status);
            Some(kind)
        }
        Err(e) => {
            log::warn!("eta fit publish: could not publish fit {}: {e:#}", fit.id);
            status.publish_error = Some(format!("{e:#}"));
            write_status(root, &status);
            None
        }
    }
}

/// The publication branch for `effective_config` (`fleet.etaFitRef`).
#[must_use]
pub fn resolve_ref(effective_config: &Value) -> String {
    crate::config_resolver::get_path(effective_config, REF_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| DEFAULT_REF.to_string(), str::to_string)
}

/// The staleness bound (`fleet.etaFitMaxAgeDays`).
#[must_use]
pub fn resolve_max_age(effective_config: &Value) -> Duration {
    let days = crate::config_resolver::get_path(effective_config, MAX_AGE_KEY)
        .and_then(Value::as_i64)
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_MAX_AGE_DAYS);
    Duration::days(days)
}

/// The store location and publication branch for the workspace at `root`, or
/// `None` when `fleet.repo` is unset (the feature is off) or invalid.
#[must_use]
pub fn publication_location(root: &Path) -> Option<(StoreLocation, String)> {
    let effective = crate::config_resolver::resolve_effective_config(root);
    location_for(&effective, &|k| std::env::var(k).ok())
}

/// [`publication_location`] for an already-resolved config: `None` when
/// `fleet.repo` is unset, or when `fleet.repo`/`fleet.ref` or `fleet.etaFitRef`
/// is invalid (logged; nothing is fetched or written).
#[must_use]
pub fn location_for(
    effective: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<(StoreLocation, String)> {
    let base = crate::fleet_store::resolve_location(effective, env)
        .map_err(|e| log::warn!("eta fit publish: fleet store misconfigured: {e:#}"))
        .ok()??;
    let eta_ref = resolve_ref(effective);
    if let Err(e) = validate_publication_ref(&eta_ref) {
        log::warn!("eta fit publish: disabled, {e:#}");
        return None;
    }
    Some((
        StoreLocation {
            repo: base.repo,
            reference: eta_ref,
        },
        base.reference,
    ))
}
