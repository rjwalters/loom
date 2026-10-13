//! J3, the configuration-snapshot journal: **shadow** producers
//! (Epic #9908, Phase 2e, #11349; `docs/design/persistence-inventory.md`
//! §9.4, §10.2, §11).
//!
//! Three record kinds are appended to the `config` stream of the shared
//! journal core ([`crate::journal`]):
//!
//! - `config.resolved` (C8): the effective-config hash
//!   ([`planner_config_hash`], the same function `fleet.state` uses), the
//!   tier path/sha256-or-absent metadata, and the **redacted** body only the
//!   first time a hash is seen.
//! - `config.rendered` (C6): a render (`event: rendered`), and the
//!   pending-restart marker lifecycle (`pending_restart` / `pending_cleared`).
//! - `fleet.sync` (C7): the `FleetSyncStatus` snapshot (`published`) or its
//!   removal (`cleared`).
//!
//! # Contract
//!
//! - **Default off.** Nothing is read, resolved or written unless
//!   [`ENABLE_ENV`] is truthy; disabled mode is behaviour-identical.
//! - **Legacy first, legacy authoritative.** Every hook runs *after* the
//!   legacy write succeeded. No reader consults this journal.
//! - **A shadow failure never fails the operation.** Every entry point
//!   returns `()`; errors (and panics) are logged once at WARN and dropped.
//! - **No credentials.** Bodies and status records are redacted by key name
//!   and by value shape (token shapes, URL userinfo; [`redact`]) before they
//!   reach the journal.
//!
//! Divergence between the journal's projections and the legacy C6/C7 files
//! is reported by [`divergence`] (bounded, normalised) and surfaced by
//! `loom-daemon journal verify --concern config`.

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::journal::{AppendOutcome, Journal, JournalError, Position, Stream};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

/// Env switch for the shadow journal. Unset or falsy: everything is a no-op.
pub const ENABLE_ENV: &str = "LOOM_CONFIG_JOURNAL";
/// Env override of the journal root (shared with `journal verify`).
pub const ROOT_ENV: &str = "LOOM_JOURNAL_ROOT";
/// The J3 stream name; kept apart from execution and telemetry streams.
pub const STREAM: &str = "config";
/// Data schema of the records written here. A reader skips a larger one.
pub const SCHEMA: u64 = 1;
/// Most divergences [`divergence`] itemises.
pub const MAX_DIVERGENCES: usize = 50;

pub const KIND_RESOLVED: &str = "config.resolved";
pub const KIND_RENDERED: &str = "config.rendered";
pub const KIND_SYNC: &str = "fleet.sync";

/// Whether the shadow journal is switched on.
#[must_use]
pub fn enabled() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false)
}

/// The journal root: [`ROOT_ENV`], else `<loom_dir>/journal` (machine scoped,
/// outside every repository).
#[must_use]
pub fn journal_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(root));
    }
    crate::fleet_store::pending_restart::loom_dir().map(|d| d.join("journal"))
}

fn stream() -> Result<Option<Stream>, JournalError> {
    let Some(root) = journal_root() else {
        return Ok(None);
    };
    Journal::open(root).stream(STREAM).map(Some)
}

/// Run a shadow write: skipped when disabled; errors and panics are logged
/// once per `what` and swallowed.
fn shadow(what: &'static str, f: impl FnOnce(&Stream) -> Result<(), JournalError>) {
    if !enabled() {
        return;
    }
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| match stream() {
        Ok(Some(s)) => f(&s),
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }));
    let failure = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(e)) => e.to_string(),
        Err(_) => "panicked".to_string(),
    };
    static WARNED: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .map(|mut s| s.insert(what))
        .unwrap_or(false);
    if first {
        log::warn!(
            "config_journal: shadow {what} write failed (legacy write unaffected): {failure}"
        );
    }
}

// ----------------------------------------------------------------------
// Redaction
// ----------------------------------------------------------------------

const REDACTED: &str = "<redacted>";

fn sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace(['_', '-'], "");
    [
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "credential",
        "authorization",
        "privatekey",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

/// A copy of `value` with credential-keyed values blanked and token-shaped
/// strings replaced. Keys that merely *name* a reference (a `tokenPool` dir
/// path) are blanked too: over-redaction is the safe direction.
#[must_use]
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if sensitive_key(k) && !v.is_null() {
                        (k.clone(), Value::String(REDACTED.to_string()))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(s) => Value::String(redact_userinfo(&crate::secret_scan::redact(s))),
        other => other.clone(),
    }
}

/// Blank URL userinfo (`scheme://user:secret@host` → `scheme://<redacted>@host`)
/// in any string, whatever key it sits under.
fn redact_userinfo(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        #[allow(clippy::expect_used)]
        regex::Regex::new(r"([A-Za-z][A-Za-z0-9+.\-]*://)[^/\s@]+@")
            .expect("static userinfo regex compiles")
    });
    re.replace_all(s, format!("${{1}}{REDACTED}@").as_str())
        .into_owned()
}

// ----------------------------------------------------------------------
// config.resolved
// ----------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `[{tier, path, sha256 | "absent"}]`, highest precedence first.
fn tier_metadata(repo_root: &Path) -> Vec<Value> {
    let paths = crate::config_resolver::tier_paths_by_precedence(repo_root);
    let names = ["local", "project", "repo", "machine"];
    paths
        .iter()
        .zip(names)
        .map(|(path, tier)| {
            let sha = std::fs::read(path)
                .map_or_else(|_| "absent".to_string(), |bytes| sha256_hex(&bytes));
            json!({"tier": tier, "path": path.display().to_string(), "sha256": sha})
        })
        .collect()
}

/// Shadow-record the effective config `repo_root` now resolves to. Cheap to
/// repeat: an unchanged `(hash, tiers)` for this workspace appends nothing.
pub fn record_resolved(repo_root: &Path) {
    if !enabled() {
        return;
    }
    shadow("config.resolved", |s| {
        let effective = crate::config_resolver::resolve_effective_config(repo_root);
        let workspace = repo_root.display().to_string();
        append_resolved(s, &workspace, &effective, tier_metadata(repo_root))
    });
}

/// Two concerns, kept apart:
///
/// - **Transitions** (current state): a record is appended whenever the
///   `(effective_hash, tiers_digest)` of `workspace` differs from that
///   workspace's newest `config.resolved` record — including a *return* to an
///   earlier state (A → B → A, or tiers X → Y → X under one hash). The newest
///   record for a workspace is therefore always its current state.
/// - **Body storage**: the redacted body rides on the first record for a
///   hash, keyed on the hash (`append_if_new`); every later transition to an
///   already-stored hash is appended body-less, unkeyed.
pub(crate) fn append_resolved(
    s: &Stream,
    workspace: &str,
    effective: &Value,
    tiers: Vec<Value>,
) -> Result<(), JournalError> {
    let hash = crate::observability::fleet_state::planner_config_hash(effective);
    let tiers_digest = sha256_hex(Value::Array(tiers.clone()).to_string().as_bytes());
    if latest_state(s, workspace).is_some_and(|(h, t)| h == hash && t == tiers_digest) {
        return Ok(());
    }
    let mut data = json!({
        "schema": SCHEMA,
        "workspace": workspace,
        "effective_hash": hash,
        "tiers_digest": tiers_digest,
        "tiers": tiers,
        "body": redact(effective),
    });
    match s.append_if_new(KIND_RESOLVED, &hash, data.clone()) {
        Ok(AppendOutcome::Appended(_)) => return Ok(()),
        Ok(AppendOutcome::Duplicate { .. }) => {}
        Err(JournalError::EntryTooLarge { bytes }) => {
            data["body"] = json!({"omitted": "too_large", "bytes": bytes});
            if let Ok(AppendOutcome::Appended(_)) =
                s.append_if_new(KIND_RESOLVED, &hash, data.clone())
            {
                return Ok(());
            }
        }
        Err(e) => return Err(e),
    }
    // Body already stored for this hash: record the transition without it.
    data.as_object_mut().map(|m| m.remove("body"));
    s.append(KIND_RESOLVED, None, data).map(|_| ())
}

/// `(effective_hash, tiers_digest)` of the newest `config.resolved` record
/// for `workspace`, if any.
fn latest_state(s: &Stream, workspace: &str) -> Option<(String, String)> {
    let field = |d: &Value, k: &str| d.get(k).and_then(Value::as_str).map(str::to_owned);
    let mut found = None;
    for rec in s.reader(Position::START).flatten() {
        let d = &rec.envelope.data;
        if rec.envelope.kind == KIND_RESOLVED
            && d.get("workspace").and_then(Value::as_str) == Some(workspace)
        {
            found = field(d, "effective_hash").zip(field(d, "tiers_digest"));
        }
    }
    found
}

// ----------------------------------------------------------------------
// config.rendered
// ----------------------------------------------------------------------

/// The restart-required keys a render of `target` is about to change; empty
/// (and free) unless the shadow journal is on and a write will happen.
#[must_use]
pub fn restart_keys(target: &crate::fleet_store::render::Target, will_write: bool) -> Vec<String> {
    if !(will_write && enabled()) {
        return Vec::new();
    }
    crate::fleet_store::reload::partition(&crate::fleet_store::render::drifted_paths(target)).1
}

/// Shadow-record a render of `target` that wrote (`wrote`), after the legacy
/// write. `backup` is what `render::write` returned.
pub fn record_rendered(
    wrote: bool,
    target: &crate::fleet_store::render::Target,
    backup: Option<std::path::PathBuf>,
    store_commit: &Option<String>,
    restart_required_keys: &[String],
) {
    if wrote {
        append_rendered(
            target.tier.name(),
            &target.path,
            backup.as_deref(),
            store_commit.as_deref(),
            restart_required_keys,
        );
    }
}

/// [`record_rendered`] with the pieces spelled out (CLI path and tests).
pub fn append_rendered(
    tier: &str,
    target: &Path,
    backup: Option<&Path>,
    store_commit: Option<&str>,
    restart_required_keys: &[String],
) {
    shadow("config.rendered", |s| {
        s.append(
            KIND_RENDERED,
            None,
            json!({
                "schema": SCHEMA,
                "event": "rendered",
                "tier": tier,
                "target": target.display().to_string(),
                "backup": backup.map(|b| b.display().to_string()),
                "store_commit": store_commit,
                "restart_required_keys": restart_required_keys,
                "daemon_pid_at_render": Value::Null,
            }),
        )
        .map(|_| ())
    });
}

/// Shadow-record the pending-restart marker as written (C6).
pub fn record_pending_restart(
    paths: &[String],
    observed_pid: u32,
    rendered_at: chrono::DateTime<chrono::Utc>,
) {
    shadow("config.rendered", |s| {
        s.append(
            KIND_RENDERED,
            None,
            json!({
                "schema": SCHEMA,
                "event": "pending_restart",
                "restart_required_keys": paths,
                "daemon_pid_at_render": observed_pid,
                "rendered_at": rendered_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            }),
        )
        .map(|_| ())
    });
}

/// Shadow-record that the pending-restart marker was cleared.
pub fn record_pending_cleared() {
    shadow("config.rendered", |s| {
        s.append(KIND_RENDERED, None, json!({"schema": SCHEMA, "event": "pending_cleared"}))
            .map(|_| ())
    });
}

// ----------------------------------------------------------------------
// fleet.sync
// ----------------------------------------------------------------------

/// `FleetSyncStatus` fields that change on every pass without a state change.
/// [`record_sync`] does not re-journal a snapshot differing only in these, so
/// [`divergence`] must not compare them either (one list, both sides).
const SYNC_VOLATILE: &[&str] = &["at"];

fn last_sync_digest() -> &'static Mutex<Option<(PathBuf, String)>> {
    static CELL: OnceLock<Mutex<Option<(PathBuf, String)>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Shadow-record a published sync snapshot (after the legacy file write). An
/// unchanged snapshot (ignoring `at`) is not re-appended by this process.
pub fn record_sync(status: &crate::fleet_sync::FleetSyncStatus) {
    if !enabled() {
        return;
    }
    shadow("fleet.sync", |s| {
        let mut body = redact(&serde_json::to_value(status).map_err(JournalError::Encode)?);
        let digest = {
            let mut stable = body.clone();
            if let Some(m) = stable.as_object_mut() {
                for k in SYNC_VOLATILE {
                    m.remove(*k);
                }
            }
            sha256_hex(stable.to_string().as_bytes())
        };
        let root = s.dir().to_path_buf();
        if last_sync_digest()
            .lock()
            .is_ok_and(|g| g.as_ref().is_some_and(|(r, d)| *r == root && *d == digest))
        {
            return Ok(());
        }
        let mut data = json!({"schema": SCHEMA, "event": "published", "status": body.clone()});
        match s.append(KIND_SYNC, None, data.clone()) {
            Err(JournalError::EntryTooLarge { .. }) => {
                if let Some(m) = body.as_object_mut() {
                    m.remove("workspaces");
                    m.remove("checkouts");
                }
                data = json!({
                    "schema": SCHEMA, "event": "published", "truncated": true, "status": body,
                });
                s.append(KIND_SYNC, None, data)
            }
            other => other,
        }?;
        if let Ok(mut g) = last_sync_digest().lock() {
            *g = Some((root, digest));
        }
        Ok(())
    });
}

/// Shadow-record that the sync snapshot was removed (`clear_status`).
pub fn record_sync_cleared() {
    shadow("fleet.sync", |s| {
        s.append(KIND_SYNC, None, json!({"schema": SCHEMA, "event": "cleared"}))?;
        if let Ok(mut g) = last_sync_digest().lock() {
            *g = None;
        }
        Ok(())
    });
}

// ----------------------------------------------------------------------
// Projections and divergence (§10.2)
// ----------------------------------------------------------------------

/// One unexplained difference between a legacy store and its projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Divergence {
    /// `pending_restart` (C6) or `fleet_sync_status` (C7).
    pub store: &'static str,
    pub field: String,
    pub legacy: String,
    pub journal: String,
}

/// Result of [`divergence`]; bounded to [`MAX_DIVERGENCES`] items.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DivergenceReport {
    pub total: usize,
    pub truncated: bool,
    pub divergences: Vec<Divergence>,
    /// Records of a newer data schema, skipped (version skew, §11.4).
    pub skipped_newer_schema: u64,
    pub records_read: u64,
}

impl DivergenceReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.total == 0
    }
    fn push(&mut self, d: Divergence) {
        self.total += 1;
        if self.divergences.len() < MAX_DIVERGENCES {
            self.divergences.push(d);
        } else {
            self.truncated = true;
        }
    }
}

/// Drop nulls and round RFC3339 timestamps to whole seconds, so absent ==
/// null and sub-second noise is not a divergence.
fn normalise(value: &Value) -> Value {
    match value {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), normalise(v)))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(normalise).collect()),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_or_else(|_| value.clone(), |t| Value::String(t.timestamp().to_string())),
        other => other.clone(),
    }
}

fn read_legacy(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn diff_objects(
    report: &mut DivergenceReport,
    store: &'static str,
    legacy: &Value,
    journal: &Value,
    ignore: &[&str],
) {
    let (l, j) = (normalise(legacy), normalise(journal));
    let empty = Map::new();
    let (lm, jm) = (l.as_object().unwrap_or(&empty), j.as_object().unwrap_or(&empty));
    let keys: BTreeSet<&String> = lm.keys().chain(jm.keys()).collect();
    for k in keys {
        if ignore.contains(&k.as_str()) {
            continue;
        }
        if lm.get(k) != jm.get(k) {
            let show = |v: Option<&Value>| {
                v.map_or_else(
                    || "absent".to_string(),
                    |v| {
                        let s = v.to_string();
                        if s.len() > 120 {
                            format!("{}…", s.chars().take(117).collect::<String>())
                        } else {
                            s
                        }
                    },
                )
            };
            report.push(Divergence {
                store,
                field: k.clone(),
                legacy: show(lm.get(k)),
                journal: show(jm.get(k)),
            });
        }
    }
}

/// Rebuild the C6/C7 projections from the `config` stream under `root` and
/// diff them against the legacy files in `loom_dir`. Read-only.
#[must_use]
pub fn divergence(root: &Path, loom_dir: &Path) -> DivergenceReport {
    let mut report = DivergenceReport::default();
    let Ok(stream) = Journal::open(root).stream(STREAM) else {
        return report;
    };
    // `None` = no record seen; `Some(None)` = latest record says "absent".
    let mut pending: Option<Option<Value>> = None;
    let mut sync: Option<Option<(Value, bool)>> = None;
    let mut reader = stream.reader(Position::START);
    for item in &mut reader {
        let Ok(rec) = item else { break };
        report.records_read += 1;
        let data = &rec.envelope.data;
        if data.get("schema").and_then(Value::as_u64).unwrap_or(0) > SCHEMA {
            report.skipped_newer_schema += 1;
            continue;
        }
        let event = data.get("event").and_then(Value::as_str).unwrap_or("");
        match (rec.envelope.kind.as_str(), event) {
            (KIND_RENDERED, "pending_restart") => {
                pending = Some(Some(json!({
                    "paths": data.get("restart_required_keys"),
                    "observedPid": data.get("daemon_pid_at_render"),
                    "renderedAt": data.get("rendered_at"),
                })));
            }
            (KIND_RENDERED, "pending_cleared") => pending = Some(None),
            (KIND_SYNC, "published") => {
                let truncated = data
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                sync = Some(data.get("status").cloned().map(|s| (s, truncated)));
            }
            (KIND_SYNC, "cleared") => sync = Some(None),
            _ => {}
        }
    }
    if let Some(expected) = pending {
        let legacy = read_legacy(&loom_dir.join(crate::fleet_store::pending_restart::FILENAME));
        match (legacy, expected) {
            (None, None) => {}
            (Some(l), Some(j)) => diff_objects(&mut report, "pending_restart", &l, &j, &[]),
            (l, j) => report.push(Divergence {
                store: "pending_restart",
                field: "presence".into(),
                legacy: if l.is_some() { "present" } else { "absent" }.into(),
                journal: if j.is_some() { "present" } else { "absent" }.into(),
            }),
        }
    }
    if let Some(expected) = sync {
        let legacy = read_legacy(&loom_dir.join(crate::fleet_sync::STATUS_FILENAME));
        match (legacy, expected) {
            (None, None) => {}
            (Some(l), Some((j, truncated))) => {
                // The legacy file is not redacted; compare it redacted. Fields
                // the writer deliberately does not re-journal are not compared.
                let mut ignore: Vec<&str> = SYNC_VOLATILE.to_vec();
                if truncated {
                    ignore.extend(["workspaces", "checkouts"]);
                }
                diff_objects(&mut report, "fleet_sync_status", &redact(&l), &j, &ignore);
            }
            (l, j) => report.push(Divergence {
                store: "fleet_sync_status",
                field: "presence".into(),
                legacy: if l.is_some() { "present" } else { "absent" }.into(),
                journal: if j.is_some() { "present" } else { "absent" }.into(),
            }),
        }
    }
    report
}
