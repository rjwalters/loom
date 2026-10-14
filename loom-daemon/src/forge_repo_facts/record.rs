//! The per-repo canonical record: the post-redirect owner/name the forge
//! answers for a configured `owner/repo`, how fresh that answer is, and
//! whether anything since has cast doubt on it.
//!
//! Persisted in the shared ETag store directory (`0700`, atomic writes —
//! [`crate::forge_etag_store::write_private_json`]) as
//! `repofacts-<sha16(host|nwo)>.json`, so the daemon and a CLI `clean` on the
//! same host share one record. A record is only ever **written from a forge
//! answer this process asked for** (a verify or a confirm read); an observed
//! response can refresh its age or cast doubt on it, never rewrite it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge_etag_store::{self as store, ConditionalRead, Target};

use super::base::{base_repo, BaseRepo};
use super::{enabled, state, GhRepoEnv, SUSPECT_BACKOFF_SECS};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The `owner/repo` the checkout names (the URL that was read).
    pub(crate) configured_nwo: String,
    /// `owner.login` of the forge's answer (after any redirect).
    pub(crate) canonical_owner: String,
    /// `name` of the forge's answer.
    pub(crate) canonical_name: String,
    #[serde(default)]
    pub(crate) repo_id: Option<u64>,
    /// When the forge last confirmed this answer (unix seconds).
    pub(crate) verified_at: i64,
    /// When the request behind the newest accepted answer was sent; newer
    /// observations only.
    #[serde(default)]
    pub(crate) response_sent_at: i64,
    /// The validator of the verify read's `200`.
    #[serde(default)]
    pub(crate) etag: Option<String>,
    /// Something contradicted the record: it is re-read before the next use.
    #[serde(default)]
    pub(crate) suspect: bool,
    /// After a FAILED read: no new read before this time (unix seconds).
    #[serde(default)]
    pub(crate) suspect_until: Option<i64>,
}

impl Record {
    pub(crate) fn full_name(&self) -> String {
        format!("{}/{}", self.canonical_owner, self.canonical_name)
    }

    /// A placeholder carries only a backoff (the first read failed).
    fn is_placeholder(&self) -> bool {
        self.canonical_owner.is_empty() || self.canonical_name.is_empty()
    }

    pub(crate) fn usable(&self, now: i64, ttl: i64) -> bool {
        !self.suspect && !self.is_placeholder() && now - self.verified_at <= ttl
    }

    pub(crate) fn in_backoff(&self, now: i64) -> bool {
        self.suspect_until.is_some_and(|u| u > now)
    }
}

/// `host|owner/repo`, lowercased: one record per configured repo per forge.
pub(super) fn record_key(host: &str, nwo: &str) -> String {
    format!("{}|{}", host.to_ascii_lowercase(), nwo.to_ascii_lowercase())
}

fn record_path(key: &str) -> Option<PathBuf> {
    let dir = store::daemon_store_dir()?;
    Some(dir.join(format!("repofacts-{}.json", crate::short_hash::short_sha16(key))))
}

/// The newest record for `key`: the in-memory one, unless the disk holds one
/// verified later (another process on this host re-verified it).
pub(super) fn load(key: &str) -> Option<Record> {
    let mem = state::with(|s| s.records.get(key).cloned());
    let disk: Option<Record> = record_path(key).and_then(|p| store::read_private_json(&p));
    match (mem, disk) {
        (Some(m), Some(d)) if d.verified_at > m.verified_at => {
            state::with(|s| s.records.insert(key.to_string(), d.clone()));
            Some(d)
        }
        (Some(m), _) => Some(m),
        (None, Some(d)) => {
            state::with(|s| s.records.insert(key.to_string(), d.clone()));
            Some(d)
        }
        (None, None) => None,
    }
}

pub(super) fn save(key: &str, rec: &Record) {
    state::with(|s| s.records.insert(key.to_string(), rec.clone()));
    if let Some(path) = record_path(key) {
        store::write_private_json(&path, rec);
    }
}

/// Mark `key`'s record suspect (re-read before its next use).
pub(super) fn mark_suspect(key: &str, why: &str) {
    if let Some(mut rec) = load(key) {
        if !rec.suspect {
            log::info!("forge_repo_facts: {key} is suspect ({why}); it is re-read before next use");
        }
        rec.suspect = true;
        save(key, &rec);
    }
}

/// Why a verify read produced no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VerifyFailure {
    /// 404 / 410: the repo is gone or not visible to this credential.
    Gone,
    /// Anything else: breaker, network, unparseable.
    Failed,
}

/// One conditional `GET repos/<nwo>` for `base` (reader-first through
/// [`store::fetch_conditional`]), accounted under `caller` / `repo.view`.
/// On success the record is saved and returned; on failure the record (or a
/// placeholder) is put into a [`SUSPECT_BACKOFF_SECS`] backoff.
pub(super) fn verify(
    gh: &Path,
    root: &Path,
    base: &BaseRepo,
    prior: Option<&Record>,
    caller: &'static str,
) -> Result<Record, VerifyFailure> {
    let key = record_key(&base.host, &base.nwo);
    let sent_at = state::now();
    let target = Target {
        repo: Some(base.nwo.clone()),
        host: Some(base.host.clone()),
    };
    let url = format!("repos/{}", base.nwo);
    // Revalidate only a real answer: a placeholder has no body to keep.
    let sent = prior.filter(|r| !r.is_placeholder());
    let etag = sent.and_then(|r| r.etag.as_deref());
    let site = ConditionalRead::new(caller, crate::forge_call_stats::ops::REPO_VIEW);
    let fetched = store::fetch_conditional(site, gh, Some(root), &target, &url, etag);
    let outcome = match fetched {
        Ok((_, Some(r), _)) if r.status == 304 && etag.is_some() => sent.map(|p| Record {
            verified_at: sent_at,
            response_sent_at: sent_at,
            suspect: false,
            suspect_until: None,
            ..p.clone()
        }),
        Ok((_, Some(r), _)) if r.status == 200 => {
            parse_repo(&r.body, &base.nwo, sent_at).map(|rec| Record {
                etag: r.etag.clone(),
                ..rec
            })
        }
        Ok((_, Some(r), _)) if matches!(r.status, 404 | 410) => {
            return Err(backoff(&key, base, prior, VerifyFailure::Gone))
        }
        Ok((_, None, stderr)) if stderr.contains("HTTP 404") || stderr.contains("HTTP 410") => {
            return Err(backoff(&key, base, prior, VerifyFailure::Gone))
        }
        _ => None,
    };
    match outcome {
        Some(rec) => {
            save(&key, &rec);
            Ok(rec)
        }
        None => Err(backoff(&key, base, prior, VerifyFailure::Failed)),
    }
}

/// Record a failed read: suspect, and no new read for
/// [`SUSPECT_BACKOFF_SECS`].
fn backoff(
    key: &str,
    base: &BaseRepo,
    prior: Option<&Record>,
    why: VerifyFailure,
) -> VerifyFailure {
    let until = state::now() + SUSPECT_BACKOFF_SECS;
    let mut rec = prior.cloned().unwrap_or_else(|| Record {
        configured_nwo: base.nwo.clone(),
        canonical_owner: String::new(),
        canonical_name: String::new(),
        repo_id: None,
        verified_at: 0,
        response_sent_at: 0,
        etag: None,
        suspect: true,
        suspect_until: None,
    });
    rec.suspect = true;
    rec.suspect_until = Some(until);
    save(key, &rec);
    log::info!(
        "forge_repo_facts: reading {} failed ({why:?}); no retry for {SUSPECT_BACKOFF_SECS}s",
        base.nwo
    );
    why
}

/// Parse a `GET repos/<nwo>` body.
pub(super) fn parse_repo(body: &str, configured: &str, sent_at: i64) -> Option<Record> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let owner = v.pointer("/owner/login")?.as_str()?.to_string();
    let name = v.get("name")?.as_str()?.to_string();
    let full = v.get("full_name").and_then(serde_json::Value::as_str);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    if full.is_some_and(|f| !f.eq_ignore_ascii_case(&format!("{owner}/{name}"))) {
        return None;
    }
    Some(Record {
        configured_nwo: configured.to_string(),
        canonical_owner: owner,
        canonical_name: name,
        repo_id: v.get("id").and_then(serde_json::Value::as_u64),
        verified_at: sent_at,
        response_sent_at: sent_at,
        etag: None,
        suspect: false,
        suspect_until: None,
    })
}

/// A first-hand forge response this process issued for exactly
/// `requested_nwo` on `host` named the repository `full_name` (a listing
/// row's `repository_url`, a pull's `base.repo.full_name`), as of the moment
/// the request was sent.
///
/// - Older than the record's newest accepted answer: ignored (monotonic).
/// - Matching the record: refreshes `verified_at` for free.
/// - Not matching: the record is marked suspect, which forces a confirm read
///   before its next use. The record is never rewritten from the observed
///   body, and a relayed or cross-host snapshot must never be fed here.
pub(crate) fn observe(host: &str, requested_nwo: &str, full_name: &str, response_sent_at: i64) {
    if !enabled() {
        return;
    }
    let key = record_key(host, requested_nwo);
    let Some(mut rec) = load(&key) else { return };
    if rec.is_placeholder() || response_sent_at < rec.response_sent_at {
        return;
    }
    if rec.full_name().eq_ignore_ascii_case(full_name) {
        if rec.suspect {
            return; // only a confirm read clears doubt
        }
        rec.verified_at = rec.verified_at.max(response_sent_at);
        rec.response_sent_at = response_sent_at;
        save(&key, &rec);
    } else {
        log::warn!(
            "forge_repo_facts: a response for {requested_nwo} named {full_name}, not the recorded \
             {}; confirming before the record is used again",
            rec.full_name()
        );
        rec.suspect = true;
        rec.suspect_until = None;
        save(&key, &rec);
    }
}

/// A call that used `root`'s fact got 404, 410 or "Could not resolve to a
/// Repository": the record behind it is re-read before its next use.
pub(crate) fn invalidate(root: &Path, why: &str) {
    if !enabled() {
        return;
    }
    let mut keys: Vec<String> = [GhRepoEnv::Honour, GhRepoEnv::Ignore]
        .into_iter()
        .filter_map(|env| base_repo(root, env))
        .map(|b| record_key(&b.host, &b.nwo))
        .collect();
    keys.dedup();
    for key in keys {
        mark_suspect(&key, why);
    }
}
