//! Operator-decision signer keys (`fleet/decision-signers.json`, #10827),
//! read from the fleet store.
//!
//! A signed `loom:operator-decision` marker
//! ([`crate::comment_trust::decision`]) counts only when its Ed25519
//! signature verifies under an **active** key listed here. This file holds
//! public keys only; signing private keys never live in the fleet store or in
//! any repository (see `defaults/docs/comment-trust.md`).
//!
//! File contract (version 1), strict:
//!
//! ```json
//! {"version": 1, "keys": [
//!   {"id": "dash-2026-10", "alg": "ed25519",
//!    "public_key": "<standard base64 of the 32-byte key>", "state": "active"}
//! ]}
//! ```
//!
//! - `id`: `[a-z0-9][a-z0-9._-]{0,63}`, unique. A marker names its key by id,
//!   so lookup is one exact match: never a try-every-key search.
//! - `alg`: `ed25519` only.
//! - `public_key`: canonical padded standard base64 of exactly 32 bytes,
//!   unique across entries (a revoked key cannot live on under a second id).
//! - `state`: `active` (verifies) or `revoked` (never verifies). Rotation
//!   adds the new key as `active` beside the old one, moves signers over,
//!   then marks the old one `revoked` (or deletes it).
//! - At most [`MAX_KEYS`] entries. Unknown fields, a wrong version, a
//!   duplicate field or any invalid entry reject the **whole file**.
//!
//! **Fails closed** exactly like [`super::admins`]: store unset, fetch
//! failure, missing or malformed file, or a cached snapshot older than
//! [`super::admins::MAX_AGE`] yield no keys, so every marker stays prose.
//! Read only from the configured store ref, never the judged repository.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use super::fetch;
use super::DECISION_SIGNERS_PATH;

/// Config key for the signer cache TTL, in seconds.
pub const TTL_KEY: &str = "forge.decisionSignersTtlSecs";
/// Default signer cache TTL.
pub const DEFAULT_TTL_SECS: u64 = 300;
/// The most keys a v1 file may list.
pub const MAX_KEYS: usize = 16;

/// One public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerKey {
    /// The key identifier a marker names (`key=`).
    pub id: String,
    /// The raw 32-byte Ed25519 public key.
    pub public_key: [u8; 32],
    /// Whether the key may verify (`state: active`).
    pub active: bool,
}

/// The resolved signer set and how it was obtained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signers {
    /// The keys (empty when unavailable).
    pub keys: Vec<SignerKey>,
    /// Human description (`loaded …` / `unavailable: …`).
    pub state: String,
}

impl Signers {
    /// An unavailable signer set: verifies nothing, says why.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            keys: Vec::new(),
            state: format!("unavailable: {}", reason.into()),
        }
    }

    /// Whether the file loaded.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        !self.state.starts_with("unavailable")
    }

    /// The public key of the **active** key `id`; `None` for an unknown or
    /// revoked id.
    #[must_use]
    pub fn active_key(&self, id: &str) -> Option<&[u8; 32]> {
        self.keys
            .iter()
            .find(|k| k.id == id)
            .filter(|k| k.active)
            .map(|k| &k.public_key)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileV1 {
    version: u64,
    keys: Vec<EntryV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryV1 {
    id: String,
    alg: String,
    public_key: String,
    state: String,
}

/// Whether `id` is a valid key identifier.
#[must_use]
pub fn valid_key_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

/// Decode canonical padded standard base64 into exactly `N` bytes: any other
/// spelling of the same bytes (missing padding, URL alphabet, whitespace) is
/// rejected rather than normalised.
#[must_use]
pub fn decode_canonical<const N: usize>(text: &str) -> Option<[u8; N]> {
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = engine.decode(text).ok()?;
    let out: [u8; N] = bytes.try_into().ok()?;
    (engine.encode(out) == text).then_some(out)
}

/// Parse `fleet/decision-signers.json`. Fails closed on any problem.
pub fn parse(text: &str) -> Result<Vec<SignerKey>, String> {
    let file: FileV1 = serde_json::from_str(text).map_err(|e| format!("malformed: {e}"))?;
    if file.version != 1 {
        return Err(format!("unsupported version {}", file.version));
    }
    if file.keys.len() > MAX_KEYS {
        return Err(format!("{} keys listed (max {MAX_KEYS})", file.keys.len()));
    }
    let mut ids = HashSet::new();
    let mut pubs = HashSet::new();
    let mut out = Vec::with_capacity(file.keys.len());
    for e in file.keys {
        if !valid_key_id(&e.id) {
            return Err("a key id is not [a-z0-9][a-z0-9._-]{0,63}".to_string());
        }
        if e.alg != "ed25519" {
            return Err(format!("key {}: alg must be ed25519", e.id));
        }
        let active = match e.state.as_str() {
            "active" => true,
            "revoked" => false,
            _ => return Err(format!("key {}: state must be active or revoked", e.id)),
        };
        let Some(public_key) = decode_canonical::<32>(&e.public_key) else {
            return Err(format!("key {}: public_key is not canonical base64 of 32 bytes", e.id));
        };
        if !ids.insert(e.id.clone()) {
            return Err(format!("duplicate key id {}", e.id));
        }
        if !pubs.insert(public_key) {
            return Err(format!("key {}: public_key listed twice", e.id));
        }
        out.push(SignerKey {
            id: e.id,
            public_key,
            active,
        });
    }
    Ok(out)
}

/// Turn a loaded snapshot into a signer set (fail closed).
#[must_use]
pub fn from_loaded(loaded: &fetch::Loaded, now: chrono::DateTime<chrono::Utc>) -> Signers {
    let m = &loaded.snapshot.manifest;
    if let Some(why) = super::admins::over_age(loaded, now) {
        return Signers::unavailable(why);
    }
    let text = match loaded.snapshot.text(DECISION_SIGNERS_PATH) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Signers::unavailable(format!("{} has no {DECISION_SIGNERS_PATH}", m.repo))
        }
        Err(e) => return Signers::unavailable(format!("{e:#}")),
    };
    match parse(&text) {
        Ok(keys) => Signers {
            state: format!(
                "loaded {} key(s) from {} @ {}",
                keys.len(),
                m.repo,
                loaded.snapshot.short_commit()
            ),
            keys,
        },
        Err(e) => Signers::unavailable(format!("{DECISION_SIGNERS_PATH}: {e}")),
    }
}

/// A process-wide TTL cache of resolved signer sets, keyed by store.
#[derive(Default)]
pub struct Cache {
    map: Mutex<HashMap<String, (Instant, Signers)>>,
}

impl Cache {
    /// The cached set for `key` if younger than `ttl`, else `load()`'s.
    pub fn get_or_load(&self, key: &str, ttl: Duration, load: impl FnOnce() -> Signers) -> Signers {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, s)) = map.get(key) {
            if at.elapsed() < ttl {
                return s.clone();
            }
        }
        let s = load();
        map.insert(key.to_string(), (Instant::now(), s.clone()));
        s
    }
}

fn global() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Cache::default)
}

fn warn_once(cause: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Mutex::default);
    if seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(cause.to_string())
    {
        log::warn!(
            "comment_trust: decision signer keys {cause}; no signed decision counts (#10827)"
        );
    }
}

/// The signer set for the workspace at `root`, cached for the configured TTL.
#[must_use]
pub fn resolve(root: &std::path::Path) -> Signers {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let location = match super::resolve_location(&effective, &|k| std::env::var(k).ok()) {
        Ok(Some(l)) => l,
        Ok(None) => return Signers::unavailable("no fleet store configured (fleet.repo)"),
        Err(e) => {
            let reason = format!("{e:#}");
            warn_once(&reason);
            return Signers::unavailable(reason);
        }
    };
    let ttl = crate::config_resolver::get_path(&effective, TTL_KEY)
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TTL_SECS);
    let key = format!("{}@{}", location.repo, location.reference);
    global().get_or_load(&key, Duration::from_secs(ttl), || {
        let now = chrono::Utc::now();
        let signers = match super::admins::load_store(root, &location, now) {
            Ok(loaded) => from_loaded(&loaded, now),
            Err(e) => Signers::unavailable(e),
        };
        if !signers.is_loaded() {
            warn_once(&signers.state);
        }
        signers
    })
}

#[cfg(test)]
#[path = "tests/decision_signers_tests.rs"]
mod tests;
