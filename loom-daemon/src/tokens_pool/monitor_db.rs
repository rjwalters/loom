//! Import **live** account OAuth tokens from claude-monitor's SQLite store.
//!
//! Native Rust port of `loom_tools.tokens.monitor_db` (issue #4106, epic #4081
//! "eliminate Python from Loom", Phase 1, child C). Depends on child B (#4105):
//! it materializes the pool through the *same* writer `bootstrap` uses
//! ([`super::bootstrap::materialize_accounts`] / [`super::bootstrap::write_index`]),
//! so atomic write-then-rename, `0600` file / `0700` directory modes,
//! fingerprint drift detection, and the `index.json` manifest are identical by
//! construction rather than by convention.
//!
//! # Why a live-store import at all
//!
//! Loom treats claude-monitor as its primary account source, but through
//! `~/.claude-monitor/accounts.env` — a **static snapshot** an operator wrote by
//! hand at some point. claude-monitor keeps the *live* credentials in its SQLite
//! store (`usage.db` → `oauth_credentials`) and refreshes them as accounts are
//! re-authenticated. The two surfaces drift silently: after an account roll the
//! snapshot still holds the old (now revoked) tokens, so `bootstrap --force`
//! faithfully rewrites the same dead tokens. This command reads the live store
//! directly so `--force` applies the freshly-rolled tokens.
//!
//! # Read-only, soft dependency
//!
//! The database is opened `file:…?mode=ro` (never written, never migrated — it
//! belongs to another tool) with a bounded busy timeout, and every failure mode
//! is a clean typed [`MonitorImportError::DbUnavailable`] rather than a crash:
//! claude-monitor absent, `usage.db` absent, or a schema without
//! `oauth_credentials` (an older claude-monitor). `LOOM_CLAUDE_MONITOR_DIR`
//! relocates the directory, so tests never touch a real `~/.claude-monitor`.
//!
//! # Secrets
//!
//! `oauth_credentials.access_token` is raw secret material. It is carried in
//! memory only, written solely to `0600` token files, and never logged — logs
//! and the manifest carry email, filename, and the 8-char fingerprint only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};

use super::bootstrap::{
    derive_token_filename, materialize_accounts, write_index, Account, Warning,
};
use super::monitor::claude_monitor_dir;

/// claude-monitor's SQLite store, alongside `ranking.json` / `accounts.env` in
/// the directory resolved by [`claude_monitor_dir`].
const MONITOR_DB_NAME: &str = "usage.db";

/// Provenance tag recorded in `index.json` for accounts sourced from the live
/// DB. Deliberately distinct from `"monitor"` (the `accounts.env` snapshot) so
/// an operator reading the manifest can tell which surface a token came from.
const SOURCE_MONITOR_DB: &str = "monitor-db";

/// Opening the DB should be instant; a bounded timeout means a lock held by
/// claude-monitor surfaces as an error instead of hanging a sweep dispatch.
const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);

/// claude-monitor labels an account either by bare email or as
/// `"<email>'s Organization"`. This pattern recovers the email from the
/// organization form. Compiled lazily per call site (matching the house style
/// in `bootstrap.rs` / `check.rs`; this runs at most once per import).
fn org_label_re() -> Regex {
    Regex::new(r"^(?P<email>.+?)'s Organization$").unwrap()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error from [`import_from_monitor`]. Mirrors the Python
/// `MonitorDbUnavailable` / `ValueError` split so the CLI arm can map exit
/// codes.
#[derive(Debug)]
pub enum MonitorImportError {
    /// The directory, database file, or `oauth_credentials` table is missing
    /// (or otherwise unreadable). The message names the path that was tried.
    DbUnavailable(String),
    /// Two accounts derive the same token filename (they would clobber each
    /// other on disk).
    DuplicateFile(String),
    /// An IO failure while materializing the pool.
    Io(std::io::Error),
}

impl std::fmt::Display for MonitorImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DbUnavailable(m) | Self::DuplicateFile(m) => write!(f, "{m}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MonitorImportError {}

impl From<std::io::Error> for MonitorImportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// DB path + read-only URI
// ---------------------------------------------------------------------------

/// Return the path to claude-monitor's `usage.db`.
///
/// Honors `LOOM_CLAUDE_MONITOR_DIR` via [`claude_monitor_dir`].
#[must_use]
pub fn monitor_db_path(monitor_dir: Option<&Path>) -> PathBuf {
    let base = match monitor_dir {
        Some(d) => d.to_path_buf(),
        None => claude_monitor_dir(),
    };
    base.join(MONITOR_DB_NAME)
}

/// Percent-encode a path the way Python's `urllib.parse.quote(s)` (default
/// `safe='/'`) does: leave `A-Z a-z 0-9 _ . - ~` and `/` intact, `%XX` the
/// rest (each UTF-8 byte).
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Build the `mode=ro` SQLite URI for `path`.
///
/// The path is percent-encoded before interpolation. Without this, a `?` or
/// `#` anywhere in the path would terminate the URI early and the `mode=ro`
/// query parameter would be silently dropped — the connection could then open
/// **read-write** against claude-monitor's live database (#4095). `%` fares
/// even worse: the unencoded form fails to open a database that is present and
/// readable.
fn read_only_uri(path: &Path) -> String {
    format!("file:{}?mode=ro", percent_encode_path(&path.to_string_lossy()))
}

// ---------------------------------------------------------------------------
// Credential reading
// ---------------------------------------------------------------------------

/// One raw `oauth_credentials` row: `(id, label, access_token, joined_email)`.
/// The three text columns are nullable in the source schema.
type CredentialRow = (i64, Option<String>, Option<String>, Option<String>);

/// One active credential row resolved to an email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorCredential {
    pub email: String,
    pub token: String,
    pub label: String,
}

/// Recover an email from a credential label, or `None` if it isn't one.
///
/// claude-monitor uses either the bare email or `"<email>'s Organization"`.
/// Anything without an `@` is a display name we cannot join on.
fn email_from_label(label: &str) -> Option<String> {
    let text = label.trim();
    if text.is_empty() {
        return None;
    }
    let text = match org_label_re().captures(text) {
        Some(caps) => caps.name("email").map_or(text, |m| m.as_str()).trim(),
        None => text,
    };
    if text.contains('@') {
        Some(text.to_string())
    } else {
        None
    }
}

/// Read active credentials from claude-monitor's store.
///
/// Only `is_active = 1` rows with a non-empty `access_token` are returned:
/// claude-monitor deactivates rows for accounts an operator has removed, and
/// importing those would repopulate the pool with retired accounts.
///
/// `expires_at` is deliberately **not** used as a filter — observed rows carry
/// stale timestamps while the token still authenticates; health is established
/// by probing (`loom-daemon tokens check`), which is authoritative.
///
/// When one email has several active rows, the highest `id` wins (rows are
/// append-ordered, so that is the most recently stored credential). Returns
/// credentials de-duplicated by email, in first-seen order.
fn read_monitor_credentials(
    db_path: &Path,
    warnings: &mut Vec<Warning>,
) -> Result<Vec<MonitorCredential>, MonitorImportError> {
    if !db_path.is_file() {
        return Err(MonitorImportError::DbUnavailable(format!(
            "claude-monitor database not found at {}. Is claude-monitor installed on this \
             host? (Set LOOM_CLAUDE_MONITOR_DIR to point elsewhere.)",
            db_path.display()
        )));
    }

    // Read-only URI: this database belongs to claude-monitor. We never write,
    // migrate, or take a write lock on it. SQLITE_OPEN_READ_ONLY is belt-and-
    // suspenders with the `?mode=ro` in the URI; SQLITE_OPEN_URI makes SQLite
    // parse the string as a URI at all. The path is percent-encoded so a `?`
    // or `#` in it cannot silently void the read-only guarantee (#4095).
    let uri = read_only_uri(db_path);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| {
        MonitorImportError::DbUnavailable(format!("Could not open {}: {e}", db_path.display()))
    })?;
    // A lock held by claude-monitor surfaces as an error after the timeout,
    // rather than hanging a sweep dispatch indefinitely.
    conn.busy_timeout(SQLITE_TIMEOUT).map_err(|e| {
        MonitorImportError::DbUnavailable(format!("Could not open {}: {e}", db_path.display()))
    })?;

    // LEFT JOIN so a credential whose account row is missing still comes back —
    // its email is then recovered from the label.
    let query = "SELECT c.id, c.label, c.access_token, a.email \
                   FROM oauth_credentials c \
                   LEFT JOIN accounts a ON a.id = c.account_id \
                  WHERE c.is_active = 1 \
                  ORDER BY c.id";

    let rows: Vec<CredentialRow> = (|| {
        let mut stmt = conn.prepare(query)?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()
    })()
    .map_err(|e: rusqlite::Error| {
        // A missing oauth_credentials table (older claude-monitor) surfaces as
        // a "no such table" message — only then is the "predates the credential
        // store" hint correct. Any other error (a hot WAL missing its -shm
        // sidecar, a corrupt file) reaches the same branch, and attributing it
        // to a missing table would send the reader down the wrong path.
        let msg = e.to_string();
        let hint = if msg.contains("no such table") {
            " This claude-monitor may predate the credential store."
        } else {
            ""
        };
        MonitorImportError::DbUnavailable(format!(
            "Could not read oauth_credentials from {}: {e}.{hint}",
            db_path.display()
        ))
    })?;

    // De-dup by lowercased email, keeping first-seen order but the latest
    // (highest-id) credential value — mirrors Python's dict-of-email-lower.
    let mut order: Vec<String> = Vec::new();
    let mut by_email: std::collections::BTreeMap<String, MonitorCredential> =
        std::collections::BTreeMap::new();
    for (_id, label, access_token, joined_email) in rows {
        let token = access_token.unwrap_or_default().trim().to_string();
        if token.is_empty() {
            continue;
        }
        let label = label.unwrap_or_default();
        let email = {
            let joined = joined_email.unwrap_or_default();
            let joined = joined.trim();
            if joined.is_empty() {
                match email_from_label(&label) {
                    Some(e) => e,
                    None => {
                        warnings.push(format!(
                            "claude-monitor credential {label:?} has no resolvable email; \
                             skipping (cannot derive a stable token filename)."
                        ));
                        continue;
                    }
                }
            } else {
                joined.to_string()
            }
        };
        let key = email.to_lowercase();
        let display_label = if label.is_empty() {
            email.clone()
        } else {
            label
        };
        if !by_email.contains_key(&key) {
            order.push(key.clone());
        }
        by_email.insert(
            key,
            MonitorCredential {
                email,
                token,
                label: display_label,
            },
        );
    }

    Ok(order.into_iter().map(|k| by_email[&k].clone()).collect())
}

/// Convert credentials to [`Account`]s with derived token filenames.
///
/// Filenames come from [`derive_token_filename`], the same derivation
/// `bootstrap` uses, so an account keeps one stable identity across both import
/// paths and re-importing overwrites in place instead of creating a parallel
/// file.
fn credentials_to_accounts(creds: &[MonitorCredential]) -> Vec<Account> {
    creds
        .iter()
        .enumerate()
        .map(|(i, c)| Account {
            email: c.email.clone(),
            key: c.token.clone(),
            file: derive_token_filename(&c.email),
            source: SOURCE_MONITOR_DB.to_string(),
            index: (i + 1) as u32,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// One entry in the imported account set (no secrets). Mirrors the dicts in
/// `MonitorImportResult.effective`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAccount {
    pub email: String,
    pub name: String,
    pub file: String,
    pub source: String,
}

/// Outcome of a single [`import_from_monitor`] call. Mirrors
/// `monitor_db.MonitorImportResult`.
#[derive(Debug, Clone, Default)]
pub struct MonitorImportResult {
    pub written: Vec<String>,
    pub unchanged: Vec<String>,
    pub drifted: Vec<String>,
    pub pruned: Vec<String>,
    pub dry_run: bool,
    pub tokens_dir: Option<PathBuf>,
    pub index_path: Option<PathBuf>,
    pub db_path: Option<PathBuf>,
    pub effective: Vec<EffectiveAccount>,
    /// Non-fatal warnings accumulated during the run (no active credentials,
    /// unresolvable emails, prune failures, drift). The CLI arm decides how to
    /// surface them; JSON stdout stays clean.
    pub warnings: Vec<Warning>,
}

impl MonitorImportResult {
    /// JSON shape matching `monitor_db.MonitorImportResult.to_dict` (warnings
    /// are intentionally excluded — Python surfaces them via the logger).
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
            "pruned": self.pruned,
            "dry_run": self.dry_run,
            "tokens_dir": path_str(&self.tokens_dir),
            "index_path": path_str(&self.index_path),
            "db_path": path_str(&self.db_path),
            "effective": self.effective.iter().map(|a| serde_json::json!({
                "email": a.email,
                "name": a.name,
                "file": a.file,
                "source": a.source,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Options for [`import_from_monitor`].
pub struct ImportOptions<'a> {
    /// Destination pool (per-repo `<repo>/.loom/tokens` or the shared
    /// machine-level pool — the caller resolves which).
    pub tokens_dir: &'a Path,
    /// Override the database location (`--db`, tests). When `None`, resolved
    /// from `monitor_dir`.
    pub db_path: Option<&'a Path>,
    /// Override the claude-monitor directory (tests). Ignored when `db_path`
    /// is set.
    pub monitor_dir: Option<&'a Path>,
    /// Overwrite tokens that differ from the store.
    pub force: bool,
    /// Report what would change; write nothing.
    pub dry_run: bool,
    /// Delete `*.token` files for accounts the store no longer reports active.
    pub prune: bool,
}

/// Strip a trailing `.token` suffix to derive a logical account name.
fn name_from_file(filename: &str) -> String {
    filename
        .strip_suffix(".token")
        .map_or_else(|| filename.to_string(), str::to_string)
}

/// Materialize `tokens_dir` from claude-monitor's live credential store.
///
/// Idempotent: a token whose on-disk fingerprint already matches the store is
/// left untouched and reported `unchanged`. A token that differs is reported
/// `drifted` and skipped unless `force` — so a hand-pinned token is never
/// silently replaced. **After an account roll, `force=true` is the flag that
/// actually updates the pool**, since every rolled token legitimately differs
/// from what is on disk.
///
/// Mirrors `monitor_db.import_from_monitor`.
pub fn import_from_monitor(
    opts: &ImportOptions,
) -> Result<MonitorImportResult, MonitorImportError> {
    let resolved_db = match opts.db_path {
        Some(p) => p.to_path_buf(),
        None => monitor_db_path(opts.monitor_dir),
    };

    let mut result = MonitorImportResult {
        dry_run: opts.dry_run,
        tokens_dir: Some(opts.tokens_dir.to_path_buf()),
        index_path: Some(opts.tokens_dir.join("index.json")),
        db_path: Some(resolved_db.clone()),
        ..Default::default()
    };

    let creds = read_monitor_credentials(&resolved_db, &mut result.warnings)?;
    let accounts = credentials_to_accounts(&creds);

    result.effective = accounts
        .iter()
        .map(|a| EffectiveAccount {
            email: a.email.clone(),
            name: name_from_file(&a.file),
            file: a.file.clone(),
            source: a.source.clone(),
        })
        .collect();

    if accounts.is_empty() {
        result.warnings.push(format!(
            "No active credentials in {}; nothing to import. Check that claude-monitor has \
             accounts configured.",
            resolved_db.display()
        ));
        return Ok(result);
    }

    // Two distinct emails can derive the same stem (e.g. a.jones@x.com and
    // ajones@x.com). Fail loudly rather than let one clobber the other.
    let mut seen: std::collections::BTreeMap<String, &Account> = std::collections::BTreeMap::new();
    for acct in &accounts {
        if let Some(prior) = seen.get(&acct.file) {
            return Err(MonitorImportError::DuplicateFile(format!(
                "duplicate token filename: {} maps to both {:?} and {:?}",
                acct.file, prior.email, acct.email
            )));
        }
        seen.insert(acct.file.clone(), acct);
    }

    let outcome = materialize_accounts(
        &accounts,
        opts.tokens_dir,
        opts.force,
        opts.dry_run,
        &mut result.warnings,
    )?;
    result.written = outcome.written.clone();
    result.unchanged = outcome.unchanged.clone();
    result.drifted = outcome.drifted.clone();

    if opts.prune {
        let keep: BTreeSet<String> = accounts.iter().map(|a| a.file.clone()).collect();
        result.pruned =
            prune_stale_tokens(opts.tokens_dir, &keep, opts.dry_run, &mut result.warnings);
    }

    write_index(&outcome.manifest_accounts, result.index_path.as_ref().unwrap(), opts.dry_run)?;

    if !result.drifted.is_empty() && !opts.force {
        result.warnings.push(format!(
            "{} token(s) differ from claude-monitor's store and were left as-is; re-run with \
             --force to overwrite. After rolling accounts this is expected — --force is what \
             applies the new tokens.",
            result.drifted.len()
        ));
    }

    Ok(result)
}

/// Delete `*.token` files not in `keep`; return the filenames removed.
///
/// Globbing `*.token` deliberately excludes the pool's state files
/// (`.ranking`, `.bad_tokens`, `.failure_counts`, `.allowlist`) and
/// `index.json`, none of which carry that suffix — pruning never disturbs
/// rotation state.
fn prune_stale_tokens(
    tokens_dir: &Path,
    keep: &BTreeSet<String>,
    dry_run: bool,
    warnings: &mut Vec<Warning>,
) -> Vec<String> {
    if !tokens_dir.is_dir() {
        return Vec::new();
    }
    // Collect + sort for deterministic ordering (Python sorts the glob).
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let entries = match std::fs::read_dir(tokens_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".token") && !keep.contains(&name) {
            candidates.push((name, entry.path()));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut pruned: Vec<String> = Vec::new();
    for (name, path) in candidates {
        if dry_run {
            pruned.push(name);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => pruned.push(name),
            Err(e) => warnings.push(format!("Could not prune {}: {e}", path.display())),
        }
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Seed a fixture `usage.db` with the `accounts` + `oauth_credentials`
    /// schema the reader queries. `creds` rows are
    /// `(label, access_token, account_email, is_active)`.
    fn seed_usage_db(db_path: &Path, creds: &[(&str, &str, Option<&str>, i64)]) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT);
             CREATE TABLE oauth_credentials (
                 id INTEGER PRIMARY KEY,
                 label TEXT,
                 access_token TEXT,
                 account_id INTEGER,
                 is_active INTEGER
             );",
        )
        .unwrap();
        let mut next_account_id = 1i64;
        for (label, token, account_email, is_active) in creds {
            let account_id = match account_email {
                Some(email) => {
                    let id = next_account_id;
                    next_account_id += 1;
                    conn.execute(
                        "INSERT INTO accounts (id, email) VALUES (?1, ?2)",
                        rusqlite::params![id, email],
                    )
                    .unwrap();
                    Some(id)
                }
                None => None,
            };
            conn.execute(
                "INSERT INTO oauth_credentials (label, access_token, account_id, is_active) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![label, token, account_id, is_active],
            )
            .unwrap();
        }
    }

    // ---- email_from_label / org regex ---------------------------------

    #[test]
    fn email_from_label_bare_email() {
        assert_eq!(email_from_label("alice@example.com").as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn email_from_label_org_form() {
        assert_eq!(
            email_from_label("bob@example.com's Organization").as_deref(),
            Some("bob@example.com")
        );
    }

    #[test]
    fn email_from_label_non_email_is_none() {
        assert!(email_from_label("Acme Inc").is_none());
        assert!(email_from_label("").is_none());
        assert!(email_from_label("   ").is_none());
    }

    // ---- percent-encoding / read-only URI -----------------------------

    #[test]
    fn read_only_uri_percent_encodes_special_chars() {
        let uri = read_only_uri(Path::new("/tmp/we?rd#dir/usage.db"));
        assert_eq!(uri, "file:/tmp/we%3Frd%23dir/usage.db?mode=ro");
        // The literal ?mode=ro delimiter is the only unencoded '?'.
        assert!(uri.ends_with("?mode=ro"));
    }

    // ---- read_monitor_credentials -------------------------------------

    #[test]
    fn reads_active_credentials_and_filters_inactive() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(
            &db,
            &[
                ("alice@example.com", "sk-live-alice", Some("alice@example.com"), 1),
                ("bob@example.com", "sk-live-bob", Some("bob@example.com"), 1),
                // Inactive -> filtered out.
                ("carol@example.com", "sk-live-carol", Some("carol@example.com"), 0),
            ],
        );
        let mut warnings = Vec::new();
        let creds = read_monitor_credentials(&db, &mut warnings).unwrap();
        let emails: Vec<&str> = creds.iter().map(|c| c.email.as_str()).collect();
        assert_eq!(emails, ["alice@example.com", "bob@example.com"]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn recovers_email_from_org_label_when_join_is_null() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        // No accounts row (account_email = None) -> email recovered from label.
        seed_usage_db(&db, &[("dave@example.com's Organization", "sk-dave", None, 1)]);
        let mut warnings = Vec::new();
        let creds = read_monitor_credentials(&db, &mut warnings).unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].email, "dave@example.com");
    }

    #[test]
    fn skips_rows_with_unresolvable_email_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("Some Display Name", "sk-x", None, 1)]);
        let mut warnings = Vec::new();
        let creds = read_monitor_credentials(&db, &mut warnings).unwrap();
        assert!(creds.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no resolvable email"));
    }

    #[test]
    fn skips_rows_with_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "", Some("a@x.com"), 1)]);
        let mut warnings = Vec::new();
        let creds = read_monitor_credentials(&db, &mut warnings).unwrap();
        assert!(creds.is_empty());
    }

    #[test]
    fn highest_id_wins_for_duplicate_email() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(
            &db,
            &[
                ("a@x.com", "old-token", Some("a@x.com"), 1),
                ("a@x.com", "new-token", Some("a@x.com"), 1),
            ],
        );
        let mut warnings = Vec::new();
        let creds = read_monitor_credentials(&db, &mut warnings).unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].token, "new-token");
    }

    #[test]
    fn db_absent_is_db_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("does-not-exist.db");
        let mut warnings = Vec::new();
        match read_monitor_credentials(&db, &mut warnings) {
            Err(MonitorImportError::DbUnavailable(m)) => assert!(m.contains("not found")),
            other => panic!("expected DbUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn missing_oauth_credentials_table_hints_old_monitor() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        // A db that exists but has no oauth_credentials table.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id INTEGER);")
            .unwrap();
        drop(conn);
        let mut warnings = Vec::new();
        match read_monitor_credentials(&db, &mut warnings) {
            Err(MonitorImportError::DbUnavailable(m)) => {
                assert!(m.contains("no such table") || m.contains("oauth_credentials"));
                assert!(m.contains("predate the credential store"));
            }
            other => panic!("expected DbUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn read_survives_question_mark_in_path() {
        // Regression guard for #4095: a '?' in the DB path must not void the
        // read-only URI. Create the db in a directory whose name contains '?'.
        let tmp = tempfile::tempdir().unwrap();
        let weird = tmp.path().join("we?rd#dir");
        fs::create_dir_all(&weird).unwrap();
        let db = weird.join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 1)]);
        let mut warnings = Vec::new();
        let creds = read_monitor_credentials(&db, &mut warnings).unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].email, "a@x.com");
    }

    // ---- import_from_monitor end-to-end -------------------------------

    fn import_opts<'a>(tokens_dir: &'a Path, db: &'a Path) -> ImportOptions<'a> {
        ImportOptions {
            tokens_dir,
            db_path: Some(db),
            monitor_dir: None,
            force: false,
            dry_run: false,
            prune: false,
        }
    }

    #[test]
    fn import_writes_pool_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(
            &db,
            &[
                ("alice@example.com", "sk-alice", Some("alice@example.com"), 1),
                ("bob@example.com", "sk-bob", Some("bob@example.com"), 1),
            ],
        );
        let pool = tmp.path().join("tokens");
        let result = import_from_monitor(&import_opts(&pool, &db)).unwrap();
        assert_eq!(result.written.len(), 2);
        assert_eq!(result.effective.len(), 2);
        assert!(pool.join("index.json").is_file());
        assert!(pool.join("alice-example.token").is_file());
        assert_eq!(fs::read_to_string(pool.join("alice-example.token")).unwrap(), "sk-alice");
        // Source provenance is monitor-db, not monitor.
        assert!(result.effective.iter().all(|a| a.source == "monitor-db"));
    }

    #[test]
    fn import_dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 1)]);
        let pool = tmp.path().join("tokens");
        let mut opts = import_opts(&pool, &db);
        opts.dry_run = true;
        let result = import_from_monitor(&opts).unwrap();
        assert_eq!(result.written, vec!["a-x.token"]);
        assert!(!pool.exists());
    }

    #[test]
    fn import_idempotent_then_drift_needs_force() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 1)]);
        let pool = tmp.path().join("tokens");

        // First import writes.
        import_from_monitor(&import_opts(&pool, &db)).unwrap();
        // Second import is unchanged (fingerprint matches).
        let again = import_from_monitor(&import_opts(&pool, &db)).unwrap();
        assert_eq!(again.unchanged, vec!["a-x.token"]);
        assert!(again.written.is_empty());

        // Simulate a rolled token in the store.
        let db2 = tmp.path().join("usage2.db");
        seed_usage_db(&db2, &[("a@x.com", "sk-a-ROLLED", Some("a@x.com"), 1)]);

        // Without force: reported drifted, on-disk token untouched.
        let drift = import_from_monitor(&import_opts(&pool, &db2)).unwrap();
        assert_eq!(drift.drifted, vec!["a-x.token"]);
        assert_eq!(fs::read_to_string(pool.join("a-x.token")).unwrap(), "sk-a");
        assert!(drift.warnings.iter().any(|w| w.contains("--force")));

        // With force: the rolled token is applied.
        let mut forced = import_opts(&pool, &db2);
        forced.force = true;
        let applied = import_from_monitor(&forced).unwrap();
        assert_eq!(applied.written, vec!["a-x.token"]);
        assert_eq!(fs::read_to_string(pool.join("a-x.token")).unwrap(), "sk-a-ROLLED");
    }

    #[test]
    fn import_no_active_credentials_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 0)]); // inactive
        let pool = tmp.path().join("tokens");
        let result = import_from_monitor(&import_opts(&pool, &db)).unwrap();
        assert!(result.effective.is_empty());
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("No active credentials")));
        assert!(!pool.exists());
    }

    #[test]
    fn prune_removes_stale_tokens_but_leaves_state_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 1)]);
        let pool = tmp.path().join("tokens");
        fs::create_dir_all(&pool).unwrap();

        // A stale token for an account no longer active, plus every pool state
        // file — none of which must be pruned.
        fs::write(pool.join("stale-old.token"), "sk-stale").unwrap();
        for state in [".ranking", ".bad_tokens", ".allowlist", ".failure_counts"] {
            fs::write(pool.join(state), "sentinel").unwrap();
        }

        let mut opts = import_opts(&pool, &db);
        opts.prune = true;
        let result = import_from_monitor(&opts).unwrap();

        assert_eq!(result.pruned, vec!["stale-old.token"]);
        assert!(!pool.join("stale-old.token").exists());
        // The active account's token is present.
        assert!(pool.join("a-x.token").is_file());
        // State files are untouched.
        for state in [".ranking", ".bad_tokens", ".allowlist", ".failure_counts"] {
            assert_eq!(fs::read_to_string(pool.join(state)).unwrap(), "sentinel");
        }
        // index.json is not pruned either.
        assert!(pool.join("index.json").is_file());
    }

    #[test]
    fn prune_dry_run_reports_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 1)]);
        let pool = tmp.path().join("tokens");
        fs::create_dir_all(&pool).unwrap();
        fs::write(pool.join("stale.token"), "sk-stale").unwrap();

        let mut opts = import_opts(&pool, &db);
        opts.prune = true;
        opts.dry_run = true;
        let result = import_from_monitor(&opts).unwrap();
        assert_eq!(result.pruned, vec!["stale.token"]);
        // Dry-run leaves the file in place.
        assert!(pool.join("stale.token").exists());
    }

    #[test]
    fn duplicate_derived_filename_aborts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        // "a.b@x.com" and "ab@x.com" both derive "ab-x.token".
        seed_usage_db(
            &db,
            &[
                ("a.b@x.com", "sk-1", Some("a.b@x.com"), 1),
                ("ab@x.com", "sk-2", Some("ab@x.com"), 1),
            ],
        );
        let pool = tmp.path().join("tokens");
        match import_from_monitor(&import_opts(&pool, &db)) {
            Err(MonitorImportError::DuplicateFile(m)) => {
                assert!(m.contains("duplicate token filename"))
            }
            other => panic!("expected DuplicateFile, got {other:?}"),
        }
    }

    #[test]
    fn to_json_shape_matches_python_to_dict() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("usage.db");
        seed_usage_db(&db, &[("a@x.com", "sk-a", Some("a@x.com"), 1)]);
        let pool = tmp.path().join("tokens");
        let result = import_from_monitor(&import_opts(&pool, &db)).unwrap();
        let json = result.to_json();
        for key in [
            "written",
            "unchanged",
            "drifted",
            "pruned",
            "dry_run",
            "tokens_dir",
            "index_path",
            "db_path",
            "effective",
        ] {
            assert!(json.get(key).is_some(), "missing key {key}");
        }
        // No secret material in the JSON.
        assert!(!json.to_string().contains("sk-a"));
        let eff = &json["effective"][0];
        assert_eq!(eff["source"], "monitor-db");
        assert_eq!(eff["name"], "a-x");
        assert_eq!(eff["file"], "a-x.token");
    }
}
