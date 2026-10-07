//! Distributing the captain's `eta-fit/v2` file to every host (#10508, item 3
//! of #10586).
//!
//! `land-2026-10-06-keen-wren` serves from the newest `eta-fit/v2` file, which
//! lives in its own directory ([`super::v2::fit_dir_v2`]). The v1 distribution
//! ([`super::publish`], #10395) moves only the v1 file, so without this
//! sibling contract a non-captain host would refuse keen-wren's PR stages
//! `no_model`. This module is that contract, beside the v1 one and never
//! mixed with it.
//!
//! # A sibling lane, never a reinterpretation
//!
//! The same dedicated branch (`fleet.etaFitRef`), the same `eta-fit-pub/v1`
//! [`Envelope`], the same checks in the same order, with only the lane
//! changed:
//!
//! - `eta/fit/v2/<fit_id>.json`: the v2 file, byte-for-byte [`coeffs::to_json`];
//! - `eta/fit/v2/latest.json`: its envelope, written **last**;
//! - [`status_path_v2`]: `fit-pub/status-v2.json`, so a v2 outcome never
//!   overwrites v1's `status.json` (which `eta doctor` reads).
//!
//! v1's files, paths, status and checks are untouched: a store with no `v2/`
//! lane, a v1-only captain, or a v2 failure leaves v1 exactly as it was, and
//! a v1 failure does not stop v2. A file is installed only if its `schema` is
//! `eta-fit/v2` and its features are [`FEATURES_V2`]; installs go to
//! `<fit_dir>/v2`, which only [`super::v2::load_latest_v2`] reads. The
//! republish rule ([`same_publication`]), the reviewed-branch refusal, the
//! staleness and as-of bounds and the 304 re-verification are v1's, equally
//! strict.

use super::coeffs::{self, CoefficientFile};
use super::features_v2::{FEATURES_V2, SCHEMA_V2};
use super::publish::{
    blob_sha, contents_path, ensure_branch, parse_envelope, put_file, refuse_reviewed_branch,
    same_publication, sha256_hex, Envelope, FetchKind, PubStatus, Publication, PublishKind,
    Refusal, VerifyCtx, RAW,
};
use super::v2::{fit_dir_v2, load_latest_v2};
use crate::fleet_store::fetch::{write_atomic, Transport};
use crate::fleet_store::propose::WriteTransport;
use crate::fleet_store::StoreLocation;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::path::{Path, PathBuf};

/// Store directory of the v2 lane.
pub const V2_DIR: &str = "eta/fit/v2";
/// Store path of the v2 envelope.
pub const LATEST_PATH_V2: &str = "eta/fit/v2/latest.json";
/// Clock skew tolerated on a published `as_of` (as v1).
const FUTURE_SLACK_MIN: i64 = 5;

/// `<fit_dir>/../fit-pub/status-v2.json`.
#[must_use]
pub fn status_path_v2(root: &Path) -> PathBuf {
    coeffs::fit_dir(root)
        .with_file_name("fit-pub")
        .join("status-v2.json")
}

/// The persisted v2 status; default when absent or unreadable.
#[must_use]
pub fn read_status_v2(root: &Path) -> PubStatus {
    std::fs::read_to_string(status_path_v2(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_status_v2(root: &Path, status: &PubStatus) {
    let path = status_path_v2(root);
    let text = serde_json::to_string_pretty(status).unwrap_or_default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = write_atomic(&path, text.as_bytes()) {
        log::warn!("eta fit v2 publish: could not write {}: {e:#}", path.display());
    }
}

/// [`super::publish::verify`] for the v2 lane: `eta-fit/v2` schema and
/// [`FEATURES_V2`], every other check and its order the same.
///
/// # Errors
///
/// The first failed check.
pub fn verify_v2(
    env: &Envelope,
    bytes: &[u8],
    ctx: &VerifyCtx<'_>,
) -> Result<CoefficientFile, Refusal> {
    if sha256_hex(bytes) != env.sha256 {
        return Err(Refusal::ShaMismatch);
    }
    let file: CoefficientFile = serde_json::from_slice(bytes).map_err(|_| Refusal::BadFit)?;
    if file.schema != SCHEMA_V2 {
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
    if file.features.iter().map(String::as_str).ne(FEATURES_V2) {
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

/// The envelope for the v2 `file` published as `bytes` by `captain`.
#[must_use]
pub fn envelope_for_v2(
    file: &CoefficientFile,
    bytes: &[u8],
    captain: &str,
    now: DateTime<Utc>,
) -> Envelope {
    super::publish::envelope_for(file, bytes, captain, now)
}

// ---------------------------------------------------------------------------
// Fetch (non-captain)
// ---------------------------------------------------------------------------

enum FetchFail {
    Refused(Refusal, Option<Box<Envelope>>),
    Error(anyhow::Error),
}

impl From<anyhow::Error> for FetchFail {
    fn from(e: anyhow::Error) -> Self {
        FetchFail::Error(e)
    }
}

/// [`super::publish::fetch_and_install`] for the v2 lane: fetch the captain's
/// newest v2 publication, verify it and install it into [`fit_dir_v2`].
/// Never panics, never leaves a half-written file; every failure is a
/// [`FetchKind`] recorded in `status-v2.json`, and the previous v2 file (or
/// none) stays. v1 is not touched.
pub fn fetch_and_install_v2(
    transport: &dyn Transport,
    loc: &StoreLocation,
    root: &Path,
    captain: &str,
    now: DateTime<Utc>,
    max_age: Duration,
) -> FetchKind {
    let mut status = read_status_v2(root);
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
            // v1's `BadFit` text names `eta-fit/v1`; this lane's schema is v2.
            let detail = if r == Refusal::BadFit {
                format!("file is not a readable {SCHEMA_V2} coefficient file")
            } else {
                r.detail()
            };
            log::warn!("eta fit v2 publish: refused the published fit: {detail}");
            (kind, Some(r.code().to_string()))
        }
        Err(FetchFail::Error(e)) => {
            log::warn!("eta fit v2 publish: fetch failed, keeping the local fit: {e:#}");
            (FetchKind::FetchError, Some(format!("{e:#}")))
        }
    };
    if !matches!(kind, FetchKind::Installed | FetchKind::Current | FetchKind::NotModified) {
        status.etag = None;
    }
    status.checked_at = Some(now);
    status.kind = Some(kind);
    status.reason = reason;
    write_status_v2(root, &status);
    kind
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
    let local = load_latest_v2(root, now + Duration::days(36_500));
    // A 304 holds only while the last good fetch's captain is still the
    // declared one and its fit is still the newest local v2 file.
    let still_serving = status.captain_host.as_deref() == Some(captain)
        && status.fit_id.is_some()
        && local.as_ref().map(|f| f.id.as_str()) == status.fit_id.as_deref();
    let etag = status.etag.as_deref().filter(|_| still_serving);
    let reply = transport
        .get(&contents_path(loc, LATEST_PATH_V2), Some(RAW), etag)
        .context("reading the published v2 envelope")?;
    match reply.status {
        304 => {
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
                "forge answered HTTP {s} reading {LATEST_PATH_V2} @ {}",
                loc.reference
            )
            .into())
        }
    }
    let env = parse_envelope(reply.body.as_bytes()).map_err(|r| FetchFail::Refused(r, None))?;
    let file_reply = transport
        .get(&contents_path(loc, &format!("{V2_DIR}/{}", env.file)), Some(RAW), None)
        .context("reading the published v2 fit")?;
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
    let fit = verify_v2(&env, &bytes, &ctx)
        .map_err(|r| FetchFail::Refused(r, Some(Box::new(env.clone()))))?;
    status.fit_id = Some(env.fit_id.clone());
    status.captain_host = Some(env.captain_host.clone());
    status.as_of = Some(env.as_of);
    status.published_at = Some(env.published_at);
    if local.as_ref().is_some_and(|l| l.id == fit.id) {
        status.etag = reply.etag;
        return Ok(FetchKind::Current);
    }
    let dir = fit_dir_v2(root);
    let path = dir.join(coeffs::path_for(fit.as_of));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_atomic(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    super::run::prune_dir(&dir, super::run::RETAIN_FILES);
    status.etag = reply.etag;
    log::info!(
        "eta fit v2 publish: installed the captain's v2 fit {} (as_of {}) into {}",
        fit.id,
        fit.as_of.to_rfc3339(),
        path.display()
    );
    Ok(FetchKind::Installed)
}

// ---------------------------------------------------------------------------
// Publish (captain)
// ---------------------------------------------------------------------------

/// [`super::publish::publish`] for the v2 lane: `fit` first (unless the store
/// already holds these exact bytes), the envelope last, idempotent under
/// [`same_publication`]. Refuses the store's reviewed branch first.
///
/// # Errors
///
/// A refused branch, `fit` not tagged `eta-fit/v2`, or any store failure;
/// nothing partial is ever named by the envelope.
pub fn publish_v2(
    t: &dyn Transport,
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    base_ref: &str,
    fit: &CoefficientFile,
    captain: &str,
    now: DateTime<Utc>,
) -> Result<PublishKind> {
    refuse_reviewed_branch(loc, base_ref)?;
    if fit.schema != SCHEMA_V2 {
        bail!("refusing to publish a `{}` file on the v2 lane", fit.schema);
    }
    let bytes = coeffs::to_json(fit).into_bytes();
    let env = envelope_for_v2(fit, &bytes, captain, now);
    ensure_branch(t, wt, loc, base_ref)?;

    let latest = t.get(&contents_path(loc, LATEST_PATH_V2), Some(RAW), None)?;
    let current = latest.status == 200
        && parse_envelope(latest.body.as_bytes()).is_ok_and(|cur| same_publication(&cur, &env));
    let file_path = format!("{V2_DIR}/{}", env.file);
    let stored = t.get(&contents_path(loc, &file_path), Some(RAW), None)?;
    let file_ok = match stored.status {
        200 => sha256_hex(stored.body.as_bytes()) == env.sha256,
        404 => false,
        s => bail!("HTTP {s} reading {file_path} in {}", loc.repo),
    };
    if current && file_ok {
        return Ok(PublishKind::AlreadyPublished);
    }
    let msg = format!("eta fit v2 {} (as_of {})", fit.id, fit.as_of.to_rfc3339());
    if !file_ok {
        let existing = if stored.status == 404 {
            None
        } else {
            blob_sha(t, loc, &file_path)?
        };
        put_file(wt, loc, &file_path, &bytes, existing, &msg)?;
    }
    let latest_sha = blob_sha(t, loc, LATEST_PATH_V2)?;
    let env_text = serde_json::to_string_pretty(&env).context("encoding the envelope")? + "\n";
    put_file(wt, loc, LATEST_PATH_V2, env_text.as_bytes(), latest_sha, &msg)?;
    Ok(PublishKind::Published)
}

/// [`super::publish::publish_newest`] for the v2 lane: publish the newest
/// local v2 file unless this host already published it, as this captain, to
/// this destination. `None` when there is no local v2 file (nothing is
/// written, v1 unaffected). A failure is logged and recorded in
/// `status-v2.json`, never propagated.
pub fn publish_newest_v2(
    t: &dyn Transport,
    wt: &dyn WriteTransport,
    loc: &StoreLocation,
    base_ref: &str,
    root: &Path,
    captain: &str,
    now: DateTime<Utc>,
) -> Option<PublishKind> {
    let fit = load_latest_v2(root, now + Duration::days(1))?;
    let mut status = read_status_v2(root);
    let publication = Publication::new(&fit.id, captain, loc);
    if status.last_published.as_ref() == Some(&publication) {
        return None;
    }
    match publish_v2(t, wt, loc, base_ref, &fit, captain, now) {
        Ok(kind) => {
            log::info!(
                "eta fit v2 publish: {kind:?} fit {} to {} @ {}",
                fit.id,
                loc.repo,
                loc.reference
            );
            status.last_published = Some(publication);
            status.publish_error = None;
            write_status_v2(root, &status);
            Some(kind)
        }
        Err(e) => {
            log::warn!("eta fit v2 publish: could not publish fit {}: {e:#}", fit.id);
            status.publish_error = Some(format!("{e:#}"));
            write_status_v2(root, &status);
            None
        }
    }
}
