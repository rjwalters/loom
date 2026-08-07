//! Materialize the multi-account OAuth token pool from account sources.
//!
//! Native Rust port of `loom_tools.tokens.bootstrap` (issue #4105, epic #4081
//! "eliminate Python from Loom", Phase 1, child B). Reads numbered
//! `ACCOUNT_EMAIL_N` / `ACCOUNT_KEY_N` / `ACCOUNT_TOKEN_FILE_N` triples and
//! materializes them as per-account `.token` files inside `.loom/tokens/`,
//! writing an `index.json` manifest with sha256 fingerprints (truncated to 8
//! chars) so drift between the source and on-disk state can be detected
//! without storing secret material.
//!
//! # Additive multi-source merge (#3695, #3698)
//!
//! Accounts are read from up to three sources and merged **by account email**
//! (case-insensitive) so a set of Claude accounts can be declared **once** and
//! shared across every workspace. In **precedence order, highest first**:
//!
//! 1. **claude-monitor master (#3698)** — `~/.claude-monitor/accounts.env` when
//!    present (dir via [`super::monitor::claude_monitor_dir`], overridable with
//!    `LOOM_CLAUDE_MONITOR_DIR`). EMAIL+KEY-only entries auto-derive their token
//!    filename (see [`derive_token_filename`]). Soft dependency: pure file
//!    detection, no import of any claude-monitor package.
//! 2. **Home master (#3695)** — **opt-in only** (#3704). Consulted only when
//!    `LOOM_ACCOUNTS_ENV` (or `--home-env`) points at a file; unset means the
//!    home master is not auto-read (no default location).
//! 3. **Repo-local** — `<repo>/.loom/accounts.env` if present, else the legacy
//!    `<repo>/.env` (override with `--env`).
//!
//! An entry whose email appears in a higher-precedence source **overrides** the
//! lower one; an entry with a new email **adds** to the pool. Absent the
//! claude-monitor file, resolution is byte-for-byte identical to the #3695
//! home+repo merge (repo overrides home).
//!
//! # `index.json` manifest (versioned, provider-aware)
//!
//! `index.json` carries no secret material — only email / filename / source /
//! provider / upstream id / materialized flag / 8-char fingerprint — and is
//! written atomically (write-then-rename). The historical claim that this file
//! "must parse identically whether written by Python or Rust" is **stale**:
//! `loom-tools` (the Python implementation this module once had to stay
//! byte-compatible with) was deleted in epic #4081 Phase 4 (#4557,
//! ADR-0013), so there has been no second implementation since then. As of
//! #5607, `INDEX_VERSION` is `3`: rows gain `provider` / `upstream_id` /
//! `materialized` (D3 of `docs/design/token-pool-provider-identity.md`). A
//! `version: 2` manifest already on disk still reads correctly via
//! [`read_manifest_rows`] (backfilled as `provider = "claude"`,
//! `upstream_id = "email:<lowercased email>"`, `materialized = true`), and the
//! next `bootstrap` / `import-from-monitor` run rewrites it as `3` — no
//! migration command, no operator action.
//!
//! # Reuse surface (#4106)
//!
//! [`Account`], [`materialize_accounts`], and [`write_index`] are `pub` with a
//! stable signature so the `import-from-monitor` port (issue #4106) can reuse
//! this writer directly rather than hand-rolling a second one that could drift
//! on the security-relevant details (atomic write-then-rename, `0600`/`0700`
//! modes, fingerprint-based drift detection).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use sha2::{Digest, Sha256};

use super::account_registry::AccountProvider;
use super::monitor::claude_monitor_dir;

/// `index.json` schema version. `3` as of #5607: rows gained `provider` /
/// `upstream_id` / `materialized` (design D3). The writer always emits `3`;
/// [`read_manifest_rows`] accepts `2` and `3`, backfilling a `2` row.
pub const INDEX_VERSION: i64 = 3;
/// Prior schema version, still readable (backfilled) by [`read_manifest_rows`].
const INDEX_VERSION_V2: i64 = 2;
/// Pool directory mode (`0700`).
const DIR_MODE: u32 = 0o700;
/// Per-token file mode (`0600`).
const FILE_MODE: u32 = 0o600;

/// Env var naming the opt-in home master accounts file (#3695/#3704).
pub const HOME_ACCOUNTS_ENV_VAR: &str = "LOOM_ACCOUNTS_ENV";
/// Filename of claude-monitor's master accounts file inside its dir (#3698).
const MONITOR_ACCOUNTS_ENV_NAME: &str = "accounts.env";

// Regexes are compiled lazily per call site. bootstrap runs at most a handful
// of times per invocation, so a `once_cell`/`LazyLock` cache buys nothing and
// would add ceremony; matching `check.rs`'s no-new-machinery house style.

/// `ACCOUNT_<FIELD>_<N>=<value>` — FIELD is one of the triple keys, N is 1+
/// digits, anchored to the start of the (already left-trimmed) line.
fn env_line_re() -> Regex {
    Regex::new(r"^ACCOUNT_(EMAIL|KEY|TOKEN_FILE)_(\d+)\s*=\s*(.*)$").unwrap()
}

/// Filename safety: lowercase/uppercase letters, digits, dot, dash, underscore;
/// must end with `.token`.
fn token_file_re() -> Regex {
    Regex::new(r"^[A-Za-z0-9._-]+\.token$").unwrap()
}

// ---------------------------------------------------------------------------
// Filename derivation (#3697)
// ---------------------------------------------------------------------------

/// Derive a safe `<name>.token` filename from an account email (#3697).
///
/// Convention (stable so account identities don't churn): strip dots and
/// hyphens from the local-part, append `-<first-domain-label>`, lowercase, then
/// drop any remaining unsafe character. Examples:
///
/// * `alice@example.com`     -> `alice-example.token`
/// * `a.b.jones@example.org` -> `abjones-example.token`
/// * `agent-1@example.com`   -> `agent1-example.token`
///
/// The result always matches [`token_file_re`]. Two *distinct* emails can still
/// derive the same stem; that true collision is caught by the duplicate-filename
/// guard in [`bootstrap_tokens`], not silently merged.
#[must_use]
pub fn derive_token_filename(email: &str) -> String {
    let strip_local = Regex::new(r"[.-]").unwrap();
    let strip_unsafe = Regex::new(r"[^A-Za-z0-9._-]").unwrap();

    let email = email.trim();
    let (local, domain) = match email.split_once('@') {
        Some((l, d)) => (l, Some(d)),
        None => (email, None),
    };
    let local_clean = strip_local.replace_all(local, "");
    let domain_label = domain
        .map(|d| d.split('.').next().unwrap_or(""))
        .unwrap_or("");
    let stem = if domain_label.is_empty() {
        local_clean.to_string()
    } else {
        format!("{local_clean}-{domain_label}")
    };
    let stem = strip_unsafe
        .replace_all(&stem.to_lowercase(), "")
        .to_string();
    let stem = if stem.is_empty() {
        "account".to_string()
    } else {
        stem
    };
    format!("{stem}.token")
}

// ---------------------------------------------------------------------------
// .env parsing
// ---------------------------------------------------------------------------

/// Strip surrounding quotes and embedded newlines/quotes from a value.
///
/// Mirrors `bootstrap._strip_value`: drop a single matching pair of surrounding
/// quotes, then remove embedded newlines and stray quotes.
fn strip_value(raw: &str) -> String {
    let s = raw.trim();
    let bytes = s.as_bytes();
    let s = if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'\'' || first == b'"') {
            &s[1..s.len() - 1]
        } else {
            s
        }
    } else {
        s
    };
    s.replace(['\n', '\r', '\'', '"'], "")
}

/// A partially-parsed `ACCOUNT_*_N` triple (any subset of fields may be set).
#[derive(Default, Clone)]
struct ParsedTriple {
    email: Option<String>,
    key: Option<String>,
    file: Option<String>,
}

/// Parse `env_path` and return `{N: ParsedTriple}` keyed by account index.
///
/// Lines that don't match the `ACCOUNT_*_N=` pattern are ignored (this is not a
/// general-purpose `.env` parser). Partial triples are returned as-is so the
/// caller can warn and skip them. Mirrors `bootstrap.parse_env_accounts`.
fn parse_env_accounts(env_path: &Path) -> BTreeMap<u32, ParsedTriple> {
    let mut accounts: BTreeMap<u32, ParsedTriple> = BTreeMap::new();
    let text = match std::fs::read_to_string(env_path) {
        Ok(t) => t,
        Err(_) => return accounts,
    };
    let re = env_line_re();

    for line in text.lines() {
        let stripped = line.trim_start();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let caps = match re.captures(stripped) {
            Some(c) => c,
            None => continue,
        };
        let field = &caps[1];
        let n: u32 = match caps[2].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let value = strip_value(&caps[3]);
        let entry = accounts.entry(n).or_default();
        match field {
            "EMAIL" => entry.email = Some(value),
            "KEY" => entry.key = Some(value),
            "TOKEN_FILE" => entry.file = Some(value),
            _ => {}
        }
    }
    accounts
}

// ---------------------------------------------------------------------------
// Source resolution + merge (#3695, #3698)
// ---------------------------------------------------------------------------

/// A single validated account triple with its provenance.
///
/// `source` is one of `"home"`, `"repo"`, `"repo-override"`, `"monitor"`, or
/// `"monitor-override"` (see `bootstrap.Account` for the full semantics).
///
/// Public and stable so the `import-from-monitor` port (#4106) reuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub email: String,
    pub key: String,
    pub file: String,
    pub source: String,
    /// Source index N, for reporting/ordering.
    pub index: u32,
    /// Every env-triple account is `claude` — this pool is Claude-only for
    /// the `bootstrap` path. `import-from-monitor` (#5607) reuses [`Account`]
    /// for other providers too, so the field lives here rather than being
    /// assumed.
    pub provider: AccountProvider,
    /// Namespaced import-time identity (design D2): `email:<lowercased email>`
    /// for every env-triple source, since those carry no upstream account id
    /// at all. Always `Some` — every writer produces one.
    pub upstream_id: Option<String>,
}

/// Resolve the opt-in home-dir master accounts file (#3695, retired as default
/// #3704). Mirrors `bootstrap.default_home_accounts_env`.
///
/// Returns the resolved path (which may not exist on disk) when
/// `LOOM_ACCOUNTS_ENV` names a non-empty path, else `None` (the master is
/// opt-in only: no default location).
fn default_home_accounts_env() -> Option<PathBuf> {
    match std::env::var(HOME_ACCOUNTS_ENV_VAR) {
        Ok(v) => {
            if v.trim().is_empty() {
                None
            } else {
                Some(expand_tilde(&v))
            }
        }
        Err(_) => None,
    }
}

/// Resolve claude-monitor's master accounts file (#3698):
/// `<claude-monitor-dir>/accounts.env`. The path may not exist on disk.
fn default_claude_monitor_accounts_env() -> PathBuf {
    claude_monitor_dir().join(MONITOR_ACCOUNTS_ENV_NAME)
}

/// Resolve the repo-local accounts source path (#3695). Mirrors
/// `bootstrap.resolve_repo_env`.
///
/// Precedence: explicit `env_path` (`--env`), else `<repo>/.loom/accounts.env`
/// if it exists, else `<repo>/.env` (returned even when absent so the caller can
/// report it).
fn resolve_repo_env(repo_root: &Path, env_path: Option<&Path>) -> PathBuf {
    if let Some(p) = env_path {
        return p.to_path_buf();
    }
    let dedicated = repo_root.join(".loom").join("accounts.env");
    if dedicated.is_file() {
        return dedicated;
    }
    repo_root.join(".env")
}

/// Minimal `~`/`~/` expansion (Python's `Path.expanduser()` equivalent).
fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

/// A warning message surfaced during a bootstrap run (partial triple, unsafe
/// filename, drift, duplicate filename). Collected rather than logged inline so
/// the CLI arm owns presentation and JSON stays clean on stdout.
pub type Warning = String;

/// Validate parsed triples and return complete, safe [`Account`]s. Incomplete
/// triples and unsafe token filenames are recorded as warnings and skipped.
/// Mirrors `bootstrap._assemble_valid_accounts`.
fn assemble_valid_accounts(
    accounts: &BTreeMap<u32, ParsedTriple>,
    source: &str,
    warnings: &mut Vec<Warning>,
) -> Vec<Account> {
    let file_re = token_file_re();
    let mut out: Vec<Account> = Vec::new();
    for (&n, triple) in accounts {
        // Auto-derive the token filename from the email when omitted (#3697).
        let file = match (&triple.file, &triple.email) {
            (Some(f), _) if !f.is_empty() => Some(f.clone()),
            (_, Some(e)) if !e.is_empty() => Some(derive_token_filename(e)),
            (f, _) => f.clone(),
        };

        let email_ok = triple.email.as_deref().is_some_and(|s| !s.is_empty());
        let key_ok = triple.key.as_deref().is_some_and(|s| !s.is_empty());
        let file_ok = file.as_deref().is_some_and(|s| !s.is_empty());

        if !email_ok || !key_ok || !file_ok {
            let mut missing: Vec<&str> = Vec::new();
            if !email_ok {
                missing.push("email");
            }
            if !file_ok {
                missing.push("file");
            }
            if !key_ok {
                missing.push("key");
            }
            missing.sort_unstable();
            warnings.push(format!(
                "[{source}] ACCOUNT_*_{n}: incomplete triple (missing: {}); skipping.",
                missing.join(", ")
            ));
            continue;
        }

        let filename = file.unwrap();
        if !file_re.is_match(&filename) || filename.contains('/') || filename.contains('\\') {
            warnings.push(format!(
                "[{source}] ACCOUNT_TOKEN_FILE_{n}={filename:?}: unsafe filename; skipping."
            ));
            continue;
        }

        let email = triple.email.clone().unwrap();
        let upstream_id = format!("email:{}", email.trim().to_lowercase());
        out.push(Account {
            email,
            key: triple.key.clone().unwrap(),
            file: filename,
            source: source.to_string(),
            index: n,
            provider: AccountProvider::Claude,
            upstream_id: Some(upstream_id),
        });
    }
    out
}

/// Merge the `over` accounts **on top of** the `base` accounts, by email.
/// Mirrors `bootstrap.merge_accounts`.
///
/// An email present only in `base` is inherited; present only in `over` is
/// added; present in both means the `over` entry wins and is retagged
/// `source=override_label` while keeping `base`'s position. Ordering: base
/// accounts first (in base order), then over-only additions (in over order).
fn merge_accounts(base: &[Account], over: &[Account], override_label: &str) -> Vec<Account> {
    fn key(acct: &Account) -> String {
        acct.email.trim().to_lowercase()
    }

    let mut merged: BTreeMap<String, Account> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for acct in base {
        let k = key(acct);
        if !merged.contains_key(&k) {
            order.push(k.clone());
        }
        merged.insert(k, acct.clone()); // last base entry for a dup email wins
    }

    for acct in over {
        let k = key(acct);
        match merged.entry(k.clone()) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                // Override in place, preserving position but recording provenance.
                e.insert(Account {
                    email: acct.email.clone(),
                    key: acct.key.clone(),
                    file: acct.file.clone(),
                    source: override_label.to_string(),
                    index: acct.index,
                    provider: acct.provider,
                    upstream_id: acct.upstream_id.clone(),
                });
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                order.push(k);
                e.insert(acct.clone());
            }
        }
    }

    order.into_iter().map(|k| merged[&k].clone()).collect()
}

// ---------------------------------------------------------------------------
// Fingerprinting + manifest
// ---------------------------------------------------------------------------

/// Return the first 8 hex chars of sha256(secret). Used in `index.json` for
/// drift detection; **never** stores the secret itself. Mirrors
/// `bootstrap.fingerprint`.
#[must_use]
pub fn fingerprint(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let full = hex::encode(hasher.finalize());
    full[..8].to_string()
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Strip the trailing `.token` suffix to derive a logical account name.
fn name_from_file(filename: &str) -> String {
    if filename.to_lowercase().ends_with(".token") {
        filename[..filename.len() - ".token".len()].to_string()
    } else {
        filename.to_string()
    }
}

#[cfg(unix)]
fn chmod_best_effort(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn chmod_best_effort(_path: &Path, _mode: u32) {}

// ---------------------------------------------------------------------------
// Materialize + write
// ---------------------------------------------------------------------------

/// One manifest row for `index.json` (never contains secret material).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRow {
    pub env_index: u32,
    pub name: String,
    /// D3/§5: the account's provider. Reuses [`AccountProvider`] — the
    /// storage layer imports the identity vocabulary rather than defining a
    /// parallel one.
    pub provider: AccountProvider,
    /// Namespaced import-time identity (design D2). Always `Some` on write;
    /// `None` only appears transiently if a caller hand-builds a row without
    /// one (never done by this module's own writers).
    pub upstream_id: Option<String>,
    pub email: String,
    /// `None` for a recorded-but-unmaterialized row (D4): no `.token` file
    /// was ever written for it.
    pub file: Option<String>,
    pub source: String,
    /// D4: `true` when a `.token` file backs this row. `false` for a
    /// recorded-not-materialized non-Claude row imported from claude-monitor
    /// — the credential is visible in the manifest but never written to disk.
    pub materialized: bool,
    /// `None` for an unmaterialized row (no on-disk file to fingerprint).
    pub key_fingerprint: Option<String>,
    /// Set on a drifted row: the source (env) fingerprint that disagreed with
    /// the on-disk one recorded in `key_fingerprint`.
    pub drift: bool,
    pub env_fingerprint: Option<String>,
}

impl ManifestRow {
    fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("env_index".into(), serde_json::json!(self.env_index));
        obj.insert("name".into(), serde_json::json!(self.name));
        obj.insert(
            "provider".into(),
            serde_json::to_value(self.provider)
                .unwrap_or_else(|_| serde_json::Value::String("claude".to_string())),
        );
        obj.insert("upstream_id".into(), serde_json::json!(self.upstream_id));
        obj.insert("email".into(), serde_json::json!(self.email));
        obj.insert("file".into(), serde_json::json!(self.file));
        obj.insert("source".into(), serde_json::json!(self.source));
        obj.insert("materialized".into(), serde_json::json!(self.materialized));
        if let Some(fp) = &self.key_fingerprint {
            obj.insert("key_fingerprint".into(), serde_json::json!(fp));
        }
        if self.drift {
            obj.insert("drift".into(), serde_json::json!(true));
            obj.insert("env_fingerprint".into(), serde_json::json!(self.env_fingerprint));
        }
        serde_json::Value::Object(obj)
    }
}

/// Read `index.json` back into [`ManifestRow`]s, transparently backfilling a
/// `version: 2` manifest (D3): `provider = claude`,
/// `upstream_id = "email:<lowercased email>"`, `materialized = true` (every
/// v2 row always had an on-disk `.token` file). `version: 3` rows are read as
/// written. A missing, unreadable, or malformed file returns an empty `Vec`
/// — mirrors the fail-open style of [`super::monitor::claude_monitor_dir`]'s
/// sibling readers rather than erroring.
#[must_use]
pub fn read_manifest_rows(index_path: &Path) -> Vec<ManifestRow> {
    let raw = match std::fs::read_to_string(index_path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let version = data
        .get("version")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(INDEX_VERSION_V2);
    let is_v2 = version <= INDEX_VERSION_V2;

    let Some(accounts) = data.get("accounts").and_then(|a| a.as_array()) else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(accounts.len());
    for entry in accounts {
        let env_index = entry
            .get("env_index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let email = entry
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let source = entry
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let drift = entry
            .get("drift")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let env_fingerprint = entry
            .get("env_fingerprint")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        if is_v2 {
            let file = entry
                .get("file")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let key_fingerprint = entry
                .get("key_fingerprint")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            out.push(ManifestRow {
                env_index,
                name,
                provider: AccountProvider::Claude,
                upstream_id: Some(format!("email:{}", email.trim().to_lowercase())),
                email,
                file,
                source,
                materialized: true,
                key_fingerprint,
                drift,
                env_fingerprint,
            });
            continue;
        }

        let provider = entry
            .get("provider")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                serde_json::from_value::<AccountProvider>(serde_json::Value::String(s.to_string()))
                    .ok()
            })
            .unwrap_or(AccountProvider::Claude);
        let upstream_id = entry
            .get("upstream_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let file = entry
            .get("file")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let materialized = entry
            .get("materialized")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(file.is_some());
        let key_fingerprint = entry
            .get("key_fingerprint")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        out.push(ManifestRow {
            env_index,
            name,
            provider,
            upstream_id,
            email,
            file,
            source,
            materialized,
            key_fingerprint,
            drift,
            env_fingerprint,
        });
    }
    out
}

/// Per-file disposition from [`materialize_accounts`]. Mirrors
/// `bootstrap.MaterializeOutcome`.
#[derive(Debug, Clone, Default)]
pub struct MaterializeOutcome {
    pub written: Vec<String>,
    pub unchanged: Vec<String>,
    pub drifted: Vec<String>,
    /// Manifest rows for `index.json` (never contains secret material).
    pub manifest_accounts: Vec<ManifestRow>,
}

/// Prefix every legitimate Claude OAuth token/key carries (covers both
/// `sk-ant-oat…` interactive OAuth tokens and `sk-ant-api…` API keys).
const CLAUDE_KEY_PREFIX: &str = "sk-ant-";

/// Validate a Claude credential's shape (design D7) before it is written to
/// disk. A JWT (`eyJ…`) or anything else without the `sk-ant-` prefix is a
/// hard error, never a silently-written file — this is what makes a
/// non-Anthropic credential that slipped past provider filtering
/// unrepresentable in an Anthropic pool slot, whether it arrived via the
/// env-triple `bootstrap` path or `import-from-monitor` (both call
/// [`materialize_accounts`], so one gate covers both, #5604/#5607).
fn validate_claude_credential_shape(secret: &str) -> std::io::Result<()> {
    if secret.starts_with(CLAUDE_KEY_PREFIX) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "credential does not have the shape of a Claude OAuth token/key (expected a \
                 {CLAUDE_KEY_PREFIX:?} prefix); refusing to write it to the pool."
            ),
        ))
    }
}

/// Write each account's key to `<tokens_dir>/<file>` and build manifest rows.
/// Mirrors `bootstrap.materialize_accounts`.
///
/// Drift (an on-disk token whose fingerprint differs from the source) is
/// reported and **skipped** unless `force`. Atomic write-then-rename with
/// `0600` files inside a `0700` dir.
///
/// Every account passed here is materialized (D4) — non-Claude accounts from
/// claude-monitor are recorded directly into `index.json` by the caller
/// without ever reaching this function (#5607).
///
/// Public and stable so the `import-from-monitor` port (#4106) materializes
/// pools through this same writer.
pub fn materialize_accounts(
    accounts: &[Account],
    tokens_dir: &Path,
    force: bool,
    dry_run: bool,
    warnings: &mut Vec<Warning>,
) -> std::io::Result<MaterializeOutcome> {
    if !dry_run {
        std::fs::create_dir_all(tokens_dir)?;
        chmod_best_effort(tokens_dir, DIR_MODE);
    }

    let mut outcome = MaterializeOutcome::default();

    for acct in accounts {
        let token_path = tokens_dir.join(&acct.file);
        let new_fp = fingerprint(&acct.key);

        let existing_fp: Option<String> = if token_path.exists() {
            match std::fs::read_to_string(&token_path) {
                Ok(existing) => Some(fingerprint(existing.trim_end_matches('\n'))),
                Err(e) => {
                    warnings.push(format!(
                        "Could not read {}: {e}; will rewrite.",
                        token_path.display()
                    ));
                    None
                }
            }
        } else {
            None
        };

        // action: "written" | "unchanged" | "drifted"
        let action = match &existing_fp {
            None => "written",
            Some(fp) if *fp == new_fp => {
                if force {
                    "written"
                } else {
                    "unchanged"
                }
            }
            Some(_) => "drifted",
        };

        if action == "drifted" && !force {
            let disk_fp = existing_fp.clone().unwrap_or_default();
            warnings.push(format!(
                "DRIFT: {} on disk does not match the account source \
                 (disk fp={disk_fp}, source fp={new_fp}); re-run with --force to overwrite.",
                acct.file
            ));
            outcome.drifted.push(acct.file.clone());
            outcome.manifest_accounts.push(ManifestRow {
                env_index: acct.index,
                name: name_from_file(&acct.file),
                provider: acct.provider,
                upstream_id: acct.upstream_id.clone(),
                email: acct.email.clone(),
                file: Some(acct.file.clone()),
                source: acct.source.clone(),
                materialized: true,
                key_fingerprint: Some(disk_fp),
                drift: true,
                env_fingerprint: Some(new_fp),
            });
            continue;
        }

        if action == "unchanged" {
            outcome.unchanged.push(acct.file.clone());
        } else {
            // written (new file or force-overwrite). Shape validation runs
            // right before the write decision is acted on (D7) — even under
            // `dry_run`, so a bad-shaped credential is reported as the same
            // hard error a real run would produce, not silently accepted.
            if acct.provider == AccountProvider::Claude {
                validate_claude_credential_shape(&acct.key)?;
            }
            if !dry_run {
                let tmp = token_path.with_file_name(format!("{}.tmp", acct.file));
                std::fs::write(&tmp, &acct.key)?;
                chmod_best_effort(&tmp, FILE_MODE);
                std::fs::rename(&tmp, &token_path)?;
                chmod_best_effort(&token_path, FILE_MODE);
            }
            outcome.written.push(acct.file.clone());
        }

        outcome.manifest_accounts.push(ManifestRow {
            env_index: acct.index,
            name: name_from_file(&acct.file),
            provider: acct.provider,
            upstream_id: acct.upstream_id.clone(),
            email: acct.email.clone(),
            file: Some(acct.file.clone()),
            source: acct.source.clone(),
            materialized: true,
            key_fingerprint: Some(new_fp),
            drift: false,
            env_fingerprint: None,
        });
    }

    Ok(outcome)
}

/// Atomically write the `index.json` manifest beside the pool. Mirrors
/// `bootstrap.write_index`. Carries only email / filename / source / 8-char
/// fingerprint — [`fingerprint`] guarantees no secret material lands here.
///
/// Public and stable for reuse by the `import-from-monitor` port (#4106).
pub fn write_index(
    manifest_accounts: &[ManifestRow],
    index_path: &Path,
    dry_run: bool,
) -> std::io::Result<()> {
    let manifest = serde_json::json!({
        "version": INDEX_VERSION,
        "generated_at": now_iso(),
        "accounts": manifest_accounts.iter().map(ManifestRow::to_json).collect::<Vec<_>>(),
    });
    if dry_run {
        return Ok(());
    }
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&manifest)? + "\n";
    let tmp = index_path.with_file_name(format!(
        "{}.tmp",
        index_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("index.json")
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, index_path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Options for [`bootstrap_tokens`].
pub struct BootstrapOptions {
    /// Repo root (must contain `.loom/`).
    pub repo_root: PathBuf,
    /// Optional override for the repo-local source file (`--env`).
    pub env_path: Option<PathBuf>,
    /// Home master override (`--home-env`): `Some(Some(path))` points elsewhere,
    /// `Some(None)` disables it (`--no-home`), `None` uses default resolution
    /// (honoring `LOOM_ACCOUNTS_ENV`).
    pub home_env_path: Option<Option<PathBuf>>,
    /// Overwrite token files even when the fingerprint matches.
    pub force: bool,
    /// Compute dispositions without writing anything.
    pub dry_run: bool,
    /// Destination pool dir override (`--shared`). `None` = `<repo>/.loom/tokens`.
    pub tokens_dir: Option<PathBuf>,
}

/// One entry in the effective merged account set (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAccount {
    pub email: String,
    pub name: String,
    pub file: String,
    pub source: String,
}

/// Outcome of a single [`bootstrap_tokens`] call. Mirrors
/// `bootstrap.BootstrapResult`.
#[derive(Debug, Clone, Default)]
pub struct BootstrapResult {
    pub written: Vec<String>,
    pub unchanged: Vec<String>,
    pub drifted: Vec<String>,
    pub skipped: Vec<String>,
    pub dry_run: bool,
    pub tokens_dir: Option<PathBuf>,
    pub index_path: Option<PathBuf>,
    pub monitor_env: Option<PathBuf>,
    pub home_env: Option<PathBuf>,
    pub repo_env: Option<PathBuf>,
    pub effective: Vec<EffectiveAccount>,
    /// Non-fatal warnings accumulated during the run (partial triples, drift,
    /// unreadable tokens). The CLI arm decides how to surface them.
    pub warnings: Vec<Warning>,
}

impl BootstrapResult {
    /// JSON shape matching `bootstrap.BootstrapResult.to_dict`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let path_str = |p: &Option<PathBuf>| match p {
            Some(p) => serde_json::Value::String(p.display().to_string()),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "written": self.written,
            "unchanged": self.unchanged,
            "drifted": self.drifted,
            "skipped": self.skipped,
            "dry_run": self.dry_run,
            "tokens_dir": path_str(&self.tokens_dir),
            "index_path": path_str(&self.index_path),
            "monitor_env": path_str(&self.monitor_env),
            "home_env": path_str(&self.home_env),
            "repo_env": path_str(&self.repo_env),
            "effective": self.effective.iter().map(|a| serde_json::json!({
                "email": a.email,
                "name": a.name,
                "file": a.file,
                "source": a.source,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Error from [`bootstrap_tokens`]. Mirrors the Python `FileNotFoundError` /
/// `ValueError` split so the CLI arm can map exit codes.
#[derive(Debug)]
pub enum BootstrapError {
    /// Neither the monitor, home, nor repo-local source exists on disk.
    NoSource(String),
    /// Two effective accounts resolve to the same token filename.
    DuplicateFile(String),
    /// An IO failure while materializing the pool.
    Io(std::io::Error),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSource(m) | Self::DuplicateFile(m) => write!(f, "{m}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

impl From<std::io::Error> for BootstrapError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Bootstrap `.loom/tokens/` from the merged account sources (#3695, #3698).
/// Mirrors `bootstrap.bootstrap_tokens`.
pub fn bootstrap_tokens(opts: &BootstrapOptions) -> Result<BootstrapResult, BootstrapError> {
    // Destination pool dir: the repo-local pool by default, or an explicit
    // override (e.g. the shared machine-level pool for `--shared`, #3938).
    let tokens_dir = opts
        .tokens_dir
        .clone()
        .unwrap_or_else(|| opts.repo_root.join(".loom").join("tokens"));
    let index_path = tokens_dir.join("index.json");

    // Resolve the sources. `home_env_path` uses a nested Option so an explicit
    // disable (`Some(None)`) is distinguishable from an omitted arg (`None`).
    let home_file: Option<PathBuf> = match &opts.home_env_path {
        None => default_home_accounts_env(),
        Some(explicit) => explicit.clone(),
    };
    let repo_file = resolve_repo_env(&opts.repo_root, opts.env_path.as_deref());
    let monitor_file = default_claude_monitor_accounts_env();

    let mut result = BootstrapResult {
        dry_run: opts.dry_run,
        tokens_dir: Some(tokens_dir.clone()),
        index_path: Some(index_path.clone()),
        ..Default::default()
    };

    let monitor_present = monitor_file.is_file();
    let home_present = home_file.as_deref().is_some_and(Path::is_file);
    let repo_present = repo_file.is_file();

    result.monitor_env = monitor_present.then(|| monitor_file.clone());
    result.home_env = if home_present {
        home_file.clone()
    } else {
        None
    };
    result.repo_env = repo_present.then(|| repo_file.clone());

    if !monitor_present && !home_present && !repo_present {
        let home_disp = home_file
            .as_ref()
            .map_or_else(|| "(disabled)".to_string(), |p| p.display().to_string());
        return Err(BootstrapError::NoSource(format!(
            "No account source found. Looked for a repo-local source at {}, a home master at \
             {home_disp}, and a claude-monitor master at {}. Declare accounts in one of them \
             (see `loom-daemon tokens bootstrap --help`).",
            repo_file.display(),
            monitor_file.display(),
        )));
    }

    let mut warnings: Vec<Warning> = Vec::new();

    let monitor_accounts = if monitor_present {
        assemble_valid_accounts(&parse_env_accounts(&monitor_file), "monitor", &mut warnings)
    } else {
        Vec::new()
    };
    let home_accounts = if home_present {
        assemble_valid_accounts(
            &parse_env_accounts(home_file.as_deref().unwrap()),
            "home",
            &mut warnings,
        )
    } else {
        Vec::new()
    };
    let repo_accounts = if repo_present {
        assemble_valid_accounts(&parse_env_accounts(&repo_file), "repo", &mut warnings)
    } else {
        Vec::new()
    };

    // Three-source merge, precedence claude-monitor > repo > home. First layer
    // repo over home (repo-override), then claude-monitor on top
    // (monitor-override). Absent the monitor source the second merge is a no-op.
    let home_repo = merge_accounts(&home_accounts, &repo_accounts, "repo-override");
    let valid = merge_accounts(&home_repo, &monitor_accounts, "monitor-override");

    result.effective = valid
        .iter()
        .map(|a| EffectiveAccount {
            email: a.email.clone(),
            name: name_from_file(&a.file),
            file: a.file.clone(),
            source: a.source.clone(),
        })
        .collect();

    if valid.is_empty() {
        let srcs: Vec<String> = [&result.monitor_env, &result.home_env, &result.repo_env]
            .iter()
            .filter_map(|p| p.as_ref().map(|p| p.display().to_string()))
            .collect();
        let where_ = if srcs.is_empty() {
            "the account source(s)".to_string()
        } else {
            srcs.join(", ")
        };
        warnings
            .push(format!("No complete ACCOUNT_*_N triples in {where_}; nothing to bootstrap."));
        result.warnings = warnings;
        return Ok(result);
    }

    // Detect duplicate filenames (would otherwise clobber each other) on the
    // *merged* set.
    let mut seen_files: BTreeMap<String, Account> = BTreeMap::new();
    for acct in &valid {
        if let Some(prior) = seen_files.get(&acct.file) {
            let msg = format!(
                "Duplicate ACCOUNT_TOKEN_FILE: {:?} maps to both {:?} ({}) and {:?} ({}); aborting.",
                acct.file, prior.email, prior.source, acct.email, acct.source
            );
            warnings.push(msg);
            result.warnings = warnings;
            return Err(BootstrapError::DuplicateFile(format!(
                "duplicate token filename: {}",
                acct.file
            )));
        }
        seen_files.insert(acct.file.clone(), acct.clone());
    }

    let outcome =
        materialize_accounts(&valid, &tokens_dir, opts.force, opts.dry_run, &mut warnings)?;
    result.written.extend(outcome.written.iter().cloned());
    result.unchanged.extend(outcome.unchanged.iter().cloned());
    result.drifted.extend(outcome.drifted.iter().cloned());

    write_index(&outcome.manifest_accounts, &index_path, opts.dry_run)?;

    if !result.drifted.is_empty() && !opts.force {
        warnings.push(format!(
            "{} token(s) drifted from the account source; re-run with --force to overwrite \
             on-disk values.",
            result.drifted.len()
        ));
    }

    result.warnings = warnings;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- derive_token_filename ----------------------------------------

    #[test]
    fn derive_filename_examples() {
        assert_eq!(derive_token_filename("alice@example.com"), "alice-example.token");
        assert_eq!(derive_token_filename("a.b.jones@example.org"), "abjones-example.token");
        assert_eq!(derive_token_filename("agent-1@example.com"), "agent1-example.token");
    }

    #[test]
    fn derive_filename_no_domain() {
        assert_eq!(derive_token_filename("solo"), "solo.token");
    }

    #[test]
    fn derive_filename_empty_falls_back_to_account() {
        assert_eq!(derive_token_filename("@"), "account.token");
    }

    #[test]
    fn derive_filename_always_matches_safe_re() {
        let re = token_file_re();
        for e in ["Weird Name!@Example.Com", "x+y@z.io", "员工@公司.cn"] {
            assert!(re.is_match(&derive_token_filename(e)), "unsafe for {e}");
        }
    }

    // ---- strip_value --------------------------------------------------

    #[test]
    fn strip_value_removes_surrounding_quotes() {
        assert_eq!(strip_value("  'sk-ant-oat01-x'  "), "sk-ant-oat01-x");
        assert_eq!(strip_value("\"quoted\""), "quoted");
    }

    #[test]
    fn strip_value_removes_embedded_newlines_and_quotes() {
        assert_eq!(strip_value("ab\ncd\"ef"), "abcdef");
    }

    // ---- parse_env_accounts + triple parsing --------------------------

    #[test]
    fn parse_full_and_partial_triples() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        fs::write(
            &env,
            "# comment\n\
             ACCOUNT_EMAIL_1=alice@example.com\n\
             ACCOUNT_KEY_1=sk-ant-oat01-a\n\
             ACCOUNT_TOKEN_FILE_1=alice.token\n\
             ACCOUNT_EMAIL_2=bob@example.com\n\
             ACCOUNT_KEY_3=orphan-key\n\
             NOT_AN_ACCOUNT=ignored\n",
        )
        .unwrap();
        let parsed = parse_env_accounts(&env);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&1].email.as_deref(), Some("alice@example.com"));
        assert_eq!(parsed[&1].file.as_deref(), Some("alice.token"));
        // Partial triples preserved as-is.
        assert_eq!(parsed[&2].email.as_deref(), Some("bob@example.com"));
        assert!(parsed[&2].key.is_none());
        assert!(parsed[&3].email.is_none());
    }

    #[test]
    fn parse_gap_tolerant_indices() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        fs::write(
            &env,
            "ACCOUNT_EMAIL_1=a@x.com\nACCOUNT_KEY_1=k1\n\
             ACCOUNT_EMAIL_5=b@x.com\nACCOUNT_KEY_5=k5\n",
        )
        .unwrap();
        let parsed = parse_env_accounts(&env);
        let keys: Vec<u32> = parsed.keys().copied().collect();
        assert_eq!(keys, vec![1, 5]);
    }

    #[test]
    fn assemble_warns_on_partial_triple() {
        let mut accounts: BTreeMap<u32, ParsedTriple> = BTreeMap::new();
        accounts.insert(
            1,
            ParsedTriple {
                email: Some("a@x.com".into()),
                key: None,
                file: Some("a.token".into()),
            },
        );
        let mut warnings = Vec::new();
        let out = assemble_valid_accounts(&accounts, "repo", &mut warnings);
        assert!(out.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("incomplete triple"));
        assert!(warnings[0].contains("missing: key"));
    }

    #[test]
    fn assemble_auto_derives_filename_from_email() {
        let mut accounts: BTreeMap<u32, ParsedTriple> = BTreeMap::new();
        accounts.insert(
            1,
            ParsedTriple {
                email: Some("carol@example.com".into()),
                key: Some("sk-ant-oat01-c".into()),
                file: None,
            },
        );
        let mut warnings = Vec::new();
        let out = assemble_valid_accounts(&accounts, "monitor", &mut warnings);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "carol-example.token");
        assert!(warnings.is_empty());
    }

    #[test]
    fn assemble_rejects_unsafe_filename() {
        let mut accounts: BTreeMap<u32, ParsedTriple> = BTreeMap::new();
        accounts.insert(
            1,
            ParsedTriple {
                email: Some("a@x.com".into()),
                key: Some("k".into()),
                file: Some("../evil.token".into()),
            },
        );
        let mut warnings = Vec::new();
        let out = assemble_valid_accounts(&accounts, "repo", &mut warnings);
        assert!(out.is_empty());
        assert!(warnings[0].contains("unsafe filename"));
    }

    // ---- merge precedence ---------------------------------------------

    fn acct(email: &str, key: &str, file: &str, source: &str, index: u32) -> Account {
        let upstream_id = format!("email:{}", email.trim().to_lowercase());
        Account {
            email: email.into(),
            key: key.into(),
            file: file.into(),
            source: source.into(),
            index,
            provider: AccountProvider::Claude,
            upstream_id: Some(upstream_id),
        }
    }

    #[test]
    fn merge_repo_overrides_home() {
        let home = vec![acct("a@x.com", "home-key", "a.token", "home", 1)];
        let repo = vec![acct("a@x.com", "repo-key", "a.token", "repo", 1)];
        let merged = merge_accounts(&home, &repo, "repo-override");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].key, "repo-key");
        assert_eq!(merged[0].source, "repo-override");
    }

    #[test]
    fn merge_three_source_precedence_monitor_wins() {
        let home = vec![acct("a@x.com", "hk", "a.token", "home", 1)];
        let repo = vec![acct("a@x.com", "rk", "a.token", "repo", 1)];
        let monitor = vec![acct("a@x.com", "mk", "a.token", "monitor", 1)];
        let home_repo = merge_accounts(&home, &repo, "repo-override");
        let valid = merge_accounts(&home_repo, &monitor, "monitor-override");
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].key, "mk");
        assert_eq!(valid[0].source, "monitor-override");
    }

    #[test]
    fn merge_adds_new_emails_and_is_case_insensitive() {
        let home = vec![acct("A@x.com", "hk", "a.token", "home", 1)];
        let repo = vec![
            acct("a@x.com", "rk", "a.token", "repo", 1),
            acct("b@x.com", "rk2", "b.token", "repo", 2),
        ];
        let merged = merge_accounts(&home, &repo, "repo-override");
        // "A@x.com" and "a@x.com" collapse; b is added.
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, "repo-override");
        assert_eq!(merged[1].email, "b@x.com");
    }

    // ---- fingerprint --------------------------------------------------

    #[test]
    fn fingerprint_is_8_hex_chars() {
        let fp = fingerprint("sk-ant-oat01-secret");
        assert_eq!(fp.len(), 8);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // Stable value (sha256("x")[:8]).
        assert_eq!(fingerprint("x"), "2d711642");
    }

    // ---- materialize + index.json -------------------------------------

    #[cfg(unix)]
    #[test]
    fn materialize_writes_0600_files_in_0700_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        let accts = vec![acct("a@x.com", "sk-ant-oat01-a", "a.token", "repo", 1)];
        let mut warnings = Vec::new();
        let outcome = materialize_accounts(&accts, &dir, false, false, &mut warnings).unwrap();
        assert_eq!(outcome.written, vec!["a.token"]);
        let token = dir.join("a.token");
        assert_eq!(fs::read_to_string(&token).unwrap(), "sk-ant-oat01-a");
        let fmode = fs::metadata(&token).unwrap().permissions().mode() & 0o777;
        let dmode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600);
        assert_eq!(dmode, 0o700);
        // No leftover temp file.
        assert!(!dir.join("a.token.tmp").exists());
    }

    #[test]
    fn materialize_dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        let accts = vec![acct("a@x.com", "sk-ant-oat01-k", "a.token", "repo", 1)];
        let mut warnings = Vec::new();
        let outcome = materialize_accounts(&accts, &dir, false, true, &mut warnings).unwrap();
        assert_eq!(outcome.written, vec!["a.token"]);
        assert!(!dir.exists());
    }

    #[test]
    fn materialize_unchanged_on_fingerprint_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        let accts = vec![acct("a@x.com", "sk-ant-oat01-k", "a.token", "repo", 1)];
        let mut warnings = Vec::new();
        materialize_accounts(&accts, &dir, false, false, &mut warnings).unwrap();
        let outcome = materialize_accounts(&accts, &dir, false, false, &mut warnings).unwrap();
        assert_eq!(outcome.unchanged, vec!["a.token"]);
        assert!(outcome.written.is_empty());
    }

    #[test]
    fn materialize_drift_skipped_without_force_reported_with_force() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.token"), "hand-edited-different").unwrap();
        let accts = vec![acct("a@x.com", "sk-ant-oat01-source", "a.token", "repo", 1)];

        let mut w1 = Vec::new();
        let no_force = materialize_accounts(&accts, &dir, false, false, &mut w1).unwrap();
        assert_eq!(no_force.drifted, vec!["a.token"]);
        assert!(no_force.written.is_empty());
        assert!(no_force.manifest_accounts[0].drift);
        // File untouched.
        assert_eq!(fs::read_to_string(dir.join("a.token")).unwrap(), "hand-edited-different");

        let mut w2 = Vec::new();
        let forced = materialize_accounts(&accts, &dir, true, false, &mut w2).unwrap();
        assert_eq!(forced.written, vec!["a.token"]);
        assert_eq!(fs::read_to_string(dir.join("a.token")).unwrap(), "sk-ant-oat01-source");
    }

    #[test]
    fn materialize_rejects_jwt_shaped_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        let accts = vec![acct(
            "a@x.com",
            "eyJhbGciOiJSUzI1NiJ9.jwt.body",
            "a.token",
            "repo",
            1,
        )];
        let mut warnings = Vec::new();
        let err = materialize_accounts(&accts, &dir, false, false, &mut warnings).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("sk-ant-"));
        // Never written.
        assert!(!dir.join("a.token").exists());
    }

    #[test]
    fn materialize_accepts_oat_and_api_key_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        let accts = vec![
            acct("a@x.com", "sk-ant-oat01-oauth-token", "a.token", "repo", 1),
            acct("b@x.com", "sk-ant-api03-api-key", "b.token", "repo", 2),
        ];
        let mut warnings = Vec::new();
        let outcome = materialize_accounts(&accts, &dir, false, false, &mut warnings).unwrap();
        assert_eq!(outcome.written.len(), 2);
    }

    #[test]
    fn write_index_v3_shape_no_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        fs::create_dir_all(&dir).unwrap();
        let rows = vec![
            ManifestRow {
                env_index: 1,
                name: "alice-example".into(),
                provider: AccountProvider::Claude,
                upstream_id: Some("email:alice@example.com".into()),
                email: "alice@example.com".into(),
                file: Some("alice-example.token".into()),
                source: "repo".into(),
                materialized: true,
                key_fingerprint: Some(fingerprint("sk-ant-oat01-secret")),
                drift: false,
                env_fingerprint: None,
            },
            ManifestRow {
                env_index: 2,
                name: "alice-example-codex".into(),
                provider: AccountProvider::Codex,
                upstream_id: Some("monitor-pk:9".into()),
                email: "alice@example.com".into(),
                file: None,
                source: "monitor-db".into(),
                materialized: false,
                key_fingerprint: None,
                drift: false,
                env_fingerprint: None,
            },
        ];
        let index_path = dir.join("index.json");
        write_index(&rows, &index_path, false).unwrap();
        let raw = fs::read_to_string(&index_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["version"], 3);
        assert!(parsed["generated_at"].is_string());

        let claude_row = &parsed["accounts"][0];
        assert_eq!(claude_row["provider"], "claude");
        assert_eq!(claude_row["upstream_id"], "email:alice@example.com");
        assert_eq!(claude_row["materialized"], true);
        assert_eq!(claude_row["key_fingerprint"].as_str().unwrap().len(), 8);
        assert_eq!(claude_row["source"], "repo");

        let codex_row = &parsed["accounts"][1];
        assert_eq!(codex_row["provider"], "codex");
        assert_eq!(codex_row["materialized"], false);
        assert!(codex_row["file"].is_null());
        assert!(codex_row.get("key_fingerprint").is_none());

        // No secret material anywhere in the file.
        assert!(!raw.contains("sk-ant-oat01-secret"));
        assert!(!index_path.with_file_name("index.json.tmp").exists());
    }

    #[test]
    fn read_manifest_rows_backfills_v2_and_next_write_is_v3() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        fs::create_dir_all(&dir).unwrap();
        let index_path = dir.join("index.json");

        // Hand-write a pre-#5607 v2 manifest (no provider/upstream_id/materialized).
        fs::write(
            &index_path,
            serde_json::json!({
                "version": 2,
                "generated_at": "2026-01-01T00:00:00Z",
                "accounts": [{
                    "env_index": 1,
                    "name": "alice-example",
                    "email": "Alice@Example.com",
                    "file": "alice-example.token",
                    "source": "repo",
                    "key_fingerprint": "deadbeef",
                }],
            })
            .to_string(),
        )
        .unwrap();

        let rows = read_manifest_rows(&index_path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, AccountProvider::Claude);
        assert_eq!(rows[0].upstream_id.as_deref(), Some("email:alice@example.com"));
        assert!(rows[0].materialized);
        assert_eq!(rows[0].file.as_deref(), Some("alice-example.token"));
        assert_eq!(rows[0].key_fingerprint.as_deref(), Some("deadbeef"));

        // The next write rewrites it as v3.
        write_index(&rows, &index_path, false).unwrap();
        let raw = fs::read_to_string(&index_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["version"], 3);
        assert_eq!(parsed["accounts"][0]["provider"], "claude");
        assert_eq!(parsed["accounts"][0]["upstream_id"], "email:alice@example.com");
    }

    #[test]
    fn read_manifest_rows_reads_v3_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tokens");
        fs::create_dir_all(&dir).unwrap();
        let index_path = dir.join("index.json");
        let rows = vec![ManifestRow {
            env_index: 1,
            name: "alice-example-codex".into(),
            provider: AccountProvider::Codex,
            upstream_id: Some("monitor-pk:9".into()),
            email: "alice@example.com".into(),
            file: None,
            source: "monitor-db".into(),
            materialized: false,
            key_fingerprint: None,
            drift: false,
            env_fingerprint: None,
        }];
        write_index(&rows, &index_path, false).unwrap();

        let read_back = read_manifest_rows(&index_path);
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].provider, AccountProvider::Codex);
        assert_eq!(read_back[0].upstream_id.as_deref(), Some("monitor-pk:9"));
        assert!(!read_back[0].materialized);
        assert!(read_back[0].file.is_none());
    }

    #[test]
    fn read_manifest_rows_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let rows = read_manifest_rows(&tmp.path().join("does-not-exist.json"));
        assert!(rows.is_empty());
    }

    // ---- bootstrap_tokens end-to-end ----------------------------------

    fn scratch_repo(env_body: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        fs::write(tmp.path().join(".env"), env_body).unwrap();
        tmp
    }

    fn isolate_env() {
        // Disable the home + monitor sources so tests never touch a real
        // ~/.loom or ~/.claude-monitor.
        std::env::set_var(HOME_ACCOUNTS_ENV_VAR, "");
        std::env::set_var("LOOM_CLAUDE_MONITOR_DIR", "/nonexistent-monitor-xyz-4105");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_repo_only_writes_pool_and_index() {
        isolate_env();
        let repo = scratch_repo(
            "ACCOUNT_EMAIL_1=alice@example.com\n\
             ACCOUNT_KEY_1=sk-ant-oat01-a\n\
             ACCOUNT_EMAIL_2=bob@example.com\n\
             ACCOUNT_KEY_2=sk-ant-oat01-b\n",
        );
        let opts = BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: None,
            force: false,
            dry_run: false,
            tokens_dir: None,
        };
        let result = bootstrap_tokens(&opts).unwrap();
        assert_eq!(result.written.len(), 2);
        assert_eq!(result.effective.len(), 2);
        // Filenames auto-derived from emails.
        let files: Vec<&str> = result.effective.iter().map(|a| a.file.as_str()).collect();
        assert!(files.contains(&"alice-example.token"));
        assert!(files.contains(&"bob-example.token"));
        let pool = repo.path().join(".loom").join("tokens");
        assert!(pool.join("index.json").is_file());
        assert!(pool.join("alice-example.token").is_file());
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_dry_run_writes_no_files() {
        isolate_env();
        let repo = scratch_repo("ACCOUNT_EMAIL_1=a@x.com\nACCOUNT_KEY_1=sk-ant-oat01-k\n");
        let opts = BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: None,
            force: false,
            dry_run: true,
            tokens_dir: None,
        };
        let result = bootstrap_tokens(&opts).unwrap();
        assert_eq!(result.effective.len(), 1);
        assert!(!repo
            .path()
            .join(".loom")
            .join("tokens")
            .join("index.json")
            .exists());
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_no_source_errors() {
        isolate_env();
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        let opts = BootstrapOptions {
            repo_root: tmp.path().to_path_buf(),
            env_path: None,
            home_env_path: None,
            force: false,
            dry_run: false,
            tokens_dir: None,
        };
        match bootstrap_tokens(&opts) {
            Err(BootstrapError::NoSource(_)) => {}
            other => panic!("expected NoSource, got {other:?}"),
        }
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_no_home_ignores_home_master() {
        isolate_env();
        let repo = scratch_repo("ACCOUNT_EMAIL_1=a@x.com\nACCOUNT_KEY_1=sk-ant-oat01-repo-key\n");
        // Point a home master at a file that WOULD add an account, then disable
        // it with home_env_path = Some(None).
        let home = tempfile::tempdir().unwrap();
        let home_env = home.path().join("accounts.env");
        fs::write(&home_env, "ACCOUNT_EMAIL_1=h@x.com\nACCOUNT_KEY_1=sk-ant-oat01-home-key\n")
            .unwrap();
        std::env::set_var(HOME_ACCOUNTS_ENV_VAR, &home_env);

        let opts = BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: Some(None), // --no-home
            force: false,
            dry_run: true,
            tokens_dir: None,
        };
        let result = bootstrap_tokens(&opts).unwrap();
        // Only the repo account survives.
        assert_eq!(result.effective.len(), 1);
        assert_eq!(result.effective[0].email, "a@x.com");
        assert!(result.home_env.is_none());
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_partial_triple_warns_without_aborting() {
        isolate_env();
        let repo = scratch_repo(
            "ACCOUNT_EMAIL_1=a@x.com\nACCOUNT_KEY_1=sk-ant-oat01-k\n\
             ACCOUNT_EMAIL_2=orphan@x.com\n",
        );
        let opts = BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: None,
            force: false,
            dry_run: true,
            tokens_dir: None,
        };
        let result = bootstrap_tokens(&opts).unwrap();
        assert_eq!(result.effective.len(), 1);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("incomplete triple")));
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_duplicate_filename_aborts() {
        isolate_env();
        // Two distinct emails forced onto the same token filename.
        let repo = scratch_repo(
            "ACCOUNT_EMAIL_1=a@x.com\nACCOUNT_KEY_1=k1\nACCOUNT_TOKEN_FILE_1=dup.token\n\
             ACCOUNT_EMAIL_2=b@x.com\nACCOUNT_KEY_2=k2\nACCOUNT_TOKEN_FILE_2=dup.token\n",
        );
        let opts = BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: None,
            force: false,
            dry_run: false,
            tokens_dir: None,
        };
        match bootstrap_tokens(&opts) {
            Err(BootstrapError::DuplicateFile(_)) => {}
            other => panic!("expected DuplicateFile, got {other:?}"),
        }
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn bootstrap_shared_tokens_dir_override() {
        isolate_env();
        let repo = scratch_repo("ACCOUNT_EMAIL_1=a@x.com\nACCOUNT_KEY_1=sk-ant-oat01-k\n");
        let shared = tempfile::tempdir().unwrap();
        let shared_pool = shared.path().join("pool");
        let opts = BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: None,
            force: false,
            dry_run: false,
            tokens_dir: Some(shared_pool.clone()),
        };
        let result = bootstrap_tokens(&opts).unwrap();
        assert_eq!(result.tokens_dir.as_deref(), Some(shared_pool.as_path()));
        assert!(shared_pool.join("index.json").is_file());
        // The repo-local pool was NOT written.
        assert!(!repo.path().join(".loom").join("tokens").exists());
        std::env::remove_var(HOME_ACCOUNTS_ENV_VAR);
        std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    }
}
