//! The on-disk API-key account registry (issue #8401).
//!
//! One account is one file: `<pool>/<provider>/<account>.env`, mode `0600`,
//! holding a single `KEY=value` assignment. Nothing else in Loom stores the
//! value — not `.loom/config.json`, not `model-profiles.json`, not a launch
//! record, not a log line.
//!
//! # The secret never reaches a describable type
//!
//! [`ApiKeyAccount`] is what every listing, health view and error message is
//! built from, and it structurally cannot carry key material: the value is
//! parsed, validated and dropped inside [`describe`], and only
//! [`read_credential`] — which has no `Debug`/`Serialize` on its return type —
//! ever hands one back. That is the mechanism behind the "secret-free `list`
//! and `health`" requirement, rather than a convention reviewers must uphold.

use std::fs;
use std::path::{Path, PathBuf};

use crate::tokens_pool::locking::MkdirLock;

use super::paths::{
    list_account_files, provider_dir, validate_account, validate_provider,
    validate_stored_env_name, PoolReadError, ACCOUNT_FILE_EXT,
};
use super::{bad_marks, inflight, limits};

/// Per-provider file listing the accounts an operator has taken out of
/// selection. One account name per line; `#` starts a comment.
pub const DISABLED_FILE: &str = ".disabled";
/// Per-provider operator pin. When present and non-empty, selection prefers
/// only these accounts. Same format as [`DISABLED_FILE`].
pub const ALLOWLIST_FILE: &str = ".allowlist";

/// Why an account cannot currently be handed to a spawn.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ineligible {
    /// The operator ran `api-keys disable`.
    Disabled,
    /// The file is unreadable, empty, or not a single `KEY=value` assignment.
    Malformed,
    /// A [`bad_marks::mark_bad`] entry is active (has not reached its
    /// `resets_at`, or has none — see `api-keys mark-bad`/`unblock`).
    ///
    /// For a class-scoped mark this means "exhausted **for the model class
    /// this listing asked about**" — the same account is reported selectable
    /// by a listing that asks about another class (#8424 item 3).
    Exhausted,
    /// The account already has [`limits::AccountLimits::max_concurrent`]
    /// spawns in flight (#8424 item 4). Unlike every other variant this is
    /// transient and self-clearing: the moment one of those spawns exits the
    /// account is selectable again. Set only by the selection ladder, which is
    /// the only caller that counts leases.
    AtCapacity,
    /// A pool state file that decides eligibility (`.disabled`,
    /// `.bad_accounts.json`, `.limits.json`) exists but cannot be read or
    /// parsed, so whether this account is disabled, exhausted or capped cannot
    /// be established. The account itself may be perfectly good; it is
    /// withheld because "I could not read the list of disabled accounts" must
    /// never mean "none are".
    Unverifiable,
}

/// Secret-free description of one registered account.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyAccount {
    pub provider: String,
    pub name: String,
    /// The variable name the file assigns — a name, never a value.
    pub env_name: Option<String>,
    pub path: PathBuf,
    /// `false` once an operator has disabled it.
    pub enabled: bool,
    /// `true` when the file is `0600`-or-tighter and owned readably (always
    /// `true` off Unix, where the check does not apply).
    pub permissions_ok: bool,
    /// `None` when the account is selectable right now.
    pub ineligible: Option<Ineligible>,
    /// Operator-facing detail for a malformed entry. Never contains file
    /// contents.
    pub problem: Option<String>,
    /// Declared per-account concurrency cap (#8424 item 4), or `None` for
    /// unbounded — see [`limits`]. A *declaration*, not a live count: the
    /// in-flight tally lives in [`inflight`] and is consulted only by the
    /// selection ladder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

impl ApiKeyAccount {
    #[must_use]
    pub fn selectable(&self) -> bool {
        self.ineligible.is_none()
    }
}

/// A resolved credential. Deliberately has no `Serialize`, and a `Debug` that
/// redacts, so it cannot be logged by accident.
#[derive(Clone)]
pub struct Credential {
    pub env_name: String,
    pub value: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("env_name", &self.env_name)
            .field("value", &"<redacted>")
            .finish()
    }
}

fn account_path(root: &Path, provider: &str, name: &str) -> PathBuf {
    provider_dir(root, provider).join(format!("{name}.{ACCOUNT_FILE_EXT}"))
}

/// Parse an account file body into its single `KEY=value` assignment.
///
/// Errors are shape descriptions only — the body is never echoed.
fn parse_entry(body: &str) -> Result<Credential, String> {
    let mut found: Option<Credential> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err("expected a single KEY=value assignment".to_string());
        };
        let key = key.trim();
        // The strict `UPPER_SNAKE_CASE` form, not `validate_env_name`: this is
        // text from a file, and the competing reading of a mixed-case left-hand
        // side is "a bare key with `=` in it". The error never echoes it.
        validate_stored_env_name(key)?;
        let value = value.trim();
        // Tolerate a quoted value the way a `.env` file conventionally allows.
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        // A right-hand side made only of `=` is base32/base64 padding left over
        // from splitting a bare key, not a credential. `key` is NOT echoed here:
        // in that reading it is the key material.
        if value.bytes().all(|b| b == b'=') {
            return Err(if value.is_empty() {
                "the variable is assigned an empty value".to_string()
            } else {
                "file looks like a bare key, not a KEY=value assignment".to_string()
            });
        }
        if found.is_some() {
            return Err("file holds more than one assignment; one account is one key".to_string());
        }
        found = Some(Credential {
            env_name: key.to_string(),
            value: value.to_string(),
        });
    }
    found.ok_or_else(|| "file holds no KEY=value assignment".to_string())
}

#[cfg(unix)]
fn permissions_ok(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 == 0)
}

#[cfg(not(unix))]
fn permissions_ok(_path: &Path) -> bool {
    true
}

/// Read one account's names/state without retaining its value.
fn describe(root: &Path, provider: &str, path: &Path) -> ApiKeyAccount {
    describe_for_class(root, provider, path, None)
}

/// [`describe`], resolving bad marks for one model class (#8424 item 3).
///
/// `model_class = None` is the account-wide question and is exactly
/// [`describe`]: any active mark makes the account [`Ineligible::Exhausted`].
/// `Some(class)` narrows to the marks that block that class, so a same-account
/// allowance for a different class stays selectable.
fn describe_for_class(
    root: &Path,
    provider: &str,
    path: &Path,
    model_class: Option<&str>,
) -> ApiKeyAccount {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let disabled = read_list(root, provider, DISABLED_FILE);
    let enabled = disabled.as_ref().is_ok_and(|names| !names.contains(&name));
    let permissions_ok = permissions_ok(path);
    let parsed = fs::read_to_string(path)
        .map_err(|e| format!("unreadable: {}", e.kind()))
        .and_then(|body| parse_entry(&body));
    let (env_name, mut problem) = match parsed {
        Ok(credential) => (Some(credential.env_name), None),
        Err(problem) => (None, Some(problem)),
    };
    let active_mark = bad_marks::active_mark_for_class(
        root,
        provider,
        &name,
        model_class,
        bad_marks::epoch_now(),
    );
    let declared_cap = limits::max_concurrent(root, provider, &name);
    let ineligible = if problem.is_some() {
        Some(Ineligible::Malformed)
    } else if let Err(unreadable) = &disabled {
        problem = Some(unreadable.clone());
        Some(Ineligible::Unverifiable)
    } else if !enabled {
        Some(Ineligible::Disabled)
    } else if let Err(unreadable) = &active_mark {
        problem = Some(unreadable.clone());
        Some(Ineligible::Unverifiable)
    } else if let Err(unreadable) = &declared_cap {
        // An unreadable `.limits.json` withholds the account for the same
        // reason an unreadable `.disabled` does: the operator's declared
        // ceiling is unknown, and guessing "unbounded" would breach it.
        problem = Some(unreadable.clone());
        Some(Ineligible::Unverifiable)
    } else if let Ok(Some(mark)) = &active_mark {
        problem = Some(format!(
            "bad-marked — \"{}\"{}{}",
            mark.reason,
            mark.class_suffix(),
            mark.resets_at.map_or(
                " (no reset horizon; needs `api-keys unblock`)".to_string(),
                |resets_at| format!(" (resets at unix {resets_at})")
            )
        ));
        Some(Ineligible::Exhausted)
    } else {
        None
    };
    ApiKeyAccount {
        provider: provider.to_string(),
        name,
        env_name,
        path: path.to_path_buf(),
        enabled,
        permissions_ok,
        ineligible,
        problem,
        max_concurrent: declared_cap.unwrap_or(None),
    }
}

/// Secret-free description of one account by name — what `add`/`limit` print
/// back after mutating it. A name that is not registered still describes
/// (as [`Ineligible::Malformed`], "unreadable"), the same way `list` reports
/// an account file it cannot read.
#[must_use]
pub fn describe_account(root: &Path, provider: &str, name: &str) -> ApiKeyAccount {
    describe(root, provider, &account_path(root, provider, name))
}

/// Every registered account for one provider, sorted by name.
///
/// # Errors
/// [`PoolReadError`] when the provider directory exists but cannot be read —
/// never an empty list, which would read as "this provider is not pooled".
pub fn list_provider(root: &Path, provider: &str) -> Result<Vec<ApiKeyAccount>, PoolReadError> {
    list_provider_for_class(root, provider, None)
}

/// [`list_provider`], resolving bad marks for one model class (#8424 item 3).
/// `None` is exactly [`list_provider`].
///
/// # Errors
/// As [`list_provider`].
pub fn list_provider_for_class(
    root: &Path,
    provider: &str,
    model_class: Option<&str>,
) -> Result<Vec<ApiKeyAccount>, PoolReadError> {
    Ok(list_account_files(&provider_dir(root, provider))?
        .iter()
        .map(|path| describe_for_class(root, provider, path, model_class))
        .collect())
}

/// Every registered account across every provider under `root` (or just
/// `provider` when given), sorted by provider then name.
pub fn list_all(root: &Path, provider: Option<&str>) -> Result<Vec<ApiKeyAccount>, PoolReadError> {
    let providers = match provider {
        Some(p) => vec![p.to_string()],
        None => super::paths::list_providers(root)?,
    };
    let mut accounts = Vec::new();
    for p in &providers {
        accounts.extend(list_provider(root, p)?);
    }
    Ok(accounts)
}

/// `true` when `provider` has at least one registered account file — i.e. the
/// operator has opted this provider into pool management on this host.
///
/// # Errors
/// [`PoolReadError`] when that cannot be determined. Callers on the spawn path
/// must refuse to launch rather than fall back to "unpooled".
pub fn provider_is_pooled(root: &Path, provider: &str) -> Result<bool, PoolReadError> {
    Ok(!list_account_files(&provider_dir(root, provider))?.is_empty())
}

/// Read one account's credential. The only function that returns key material.
pub fn read_credential(root: &Path, provider: &str, name: &str) -> Result<Credential, String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let path = account_path(root, provider, name);
    let body = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read account {provider}/{name}: {}", e.kind()))?;
    parse_entry(&body).map_err(|e| format!("account {provider}/{name}: {e}"))
}

/// Register a new account. `secret` is written to a `0600` file and is never
/// returned, logged, or placed on a command line.
pub fn add(
    root: &Path,
    provider: &str,
    name: &str,
    env_name: &str,
    secret: &str,
    force: bool,
) -> Result<ApiKeyAccount, String> {
    validate_provider(provider)?;
    validate_account(name)?;
    // Same strict form `parse_entry` reads back, so `add` can never write a
    // file that `list` then reports unusable.
    validate_stored_env_name(env_name)?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("refusing to register an empty key".to_string());
    }
    if secret.contains('\n') {
        return Err("key material must be a single line".to_string());
    }
    let dir = provider_dir(root, provider);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    restrict_dir(&dir);
    let path = account_path(root, provider, name);
    if path.exists() && !force {
        return Err(format!(
            "account {provider}/{name} already exists at {}; pass --force to replace it",
            path.display()
        ));
    }
    write_secret(&path, &format!("{env_name}={secret}\n"))?;
    Ok(describe(root, provider, &path))
}

/// Atomically replace `path` with `body`: stage a sibling temp file created
/// `0600` **at open** (so the bytes are never briefly world-readable), write
/// and flush it, then `rename` it over the target.
///
/// `rename` is what makes every reader safe without taking a lock: selection
/// reads account files, `.disabled` and `.bad_accounts.json` lock-free, and
/// with an in-place `O_TRUNC` + `write` it could observe a zero-length or torn
/// file — which for `.bad_accounts.json` used to mean "no marks" and for
/// `.disabled` means "nothing is disabled". A reader now sees the old file or
/// the new one, never a partial one; a crash or `ENOSPC` mid-write strands a
/// dot-prefixed temp file and leaves the target untouched. Mirrors
/// `tokens_pool::bad_tokens` / `account_registry` (temp + `rename`).
///
/// Also used for the non-secret state files: `reason` in the bad-marks sidecar
/// is free text an operator may paste a provider error body into, so the whole
/// provider directory stays uniformly `0600`.
pub(super) fn write_secret(path: &Path, body: &str) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    // Dot-prefixed and not `*.env`, so a stranded temp is never an account.
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        file_name.trim_start_matches('.'),
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let staged = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    staged.map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("cannot write {}: {e}", path.display())
    })
}

/// Best-effort in-place overwrite for [`remove`]. Deliberately NOT
/// [`write_secret`]: an atomic replace writes the zeros to a *new* inode and
/// would leave the original key bytes untouched.
fn scrub_in_place(path: &Path) {
    use std::io::Write;
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if let Ok(mut file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.write_all(&vec![0u8; metadata.len() as usize]);
        let _ = file.sync_all();
    }
}

pub(super) fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Remove an account file, best-effort-scrubbing the bytes first.
///
/// The overwrite is **best-effort only** and must not be built on as a
/// guarantee: on a journaling or copy-on-write filesystem (ext4
/// `data=ordered`, btrfs, ZFS, APFS) the rewrite commonly lands in a *new*
/// block and leaves the original untouched, and nothing here defeats an
/// SSD's flash translation layer either. It costs one write and removes the
/// key from the obvious place; secure erasure is a property of the disk, not
/// of this function.
pub fn remove(root: &Path, provider: &str, name: &str) -> Result<(), String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let path = account_path(root, provider, name);
    if !path.exists() {
        return Err(format!("no such account {provider}/{name}"));
    }
    scrub_in_place(&path);
    fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    // Drop every piece of per-account state along with the key, so an account
    // later re-registered under the same name does not inherit a stale
    // disable, operator pin, or exhaustion mark. Best-effort: the key is
    // already gone, and leftover state only ever makes an account LESS
    // eligible, never more.
    let _ = set_listed(root, provider, DISABLED_FILE, name, false);
    let _ = set_listed(root, provider, ALLOWLIST_FILE, name, false);
    let _ = bad_marks::forget(root, provider, name);
    let _ = limits::forget(root, provider, name);
    inflight::forget(root, provider, name);
    Ok(())
}

/// Take an account out of (or back into) selection without touching its key.
pub fn set_enabled(
    root: &Path,
    provider: &str,
    name: &str,
    enabled: bool,
) -> Result<ApiKeyAccount, String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let path = account_path(root, provider, name);
    if !path.exists() {
        let available = list_provider(root, provider)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>();
        return Err(format!(
            "no such account {provider}/{name}. Registered: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        ));
    }
    set_listed(root, provider, DISABLED_FILE, name, !enabled)?;
    Ok(describe(root, provider, &path))
}

/// Read a name-per-line control file (`.disabled` / `.allowlist`).
///
/// # Errors
/// A file that does not exist is an empty list. One that exists but cannot be
/// read is an error: an unreadable `.disabled` must never read as "no account
/// is disabled". The message names the path and error kind, never contents.
pub fn read_list(root: &Path, provider: &str, file: &str) -> Result<Vec<String>, String> {
    let path = provider_dir(root, provider).join(file);
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {} ({:?})", path.display(), e.kind())),
    };
    Ok(body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// Read-modify-write a control file under the provider's control lock, with an
/// atomic replace, so neither a concurrent `disable` nor a lock-free reader can
/// observe (or cause) a lost or torn list.
fn set_listed(
    root: &Path,
    provider: &str,
    file: &str,
    name: &str,
    present: bool,
) -> Result<(), String> {
    let dir = provider_dir(root, provider);
    if !dir.exists() {
        if !present {
            return Ok(());
        }
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        restrict_dir(&dir);
    }
    let _lock = MkdirLock::acquire(&dir.join(".control.lock"))
        .map_err(|e| format!("cannot lock {}: {e}", dir.join(file).display()))?;
    // `?`, not a default: rewriting from an unreadable list would silently
    // re-enable every account that was on it.
    let mut names = read_list(root, provider, file)?;
    names.retain(|n| n != name);
    if present {
        names.push(name.to_string());
    }
    names.sort();
    names.dedup();
    let path = dir.join(file);
    if names.is_empty() {
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("cannot update {}: {e}", path.display()))?;
        }
        return Ok(());
    }
    write_secret(&path, &format!("{}\n", names.join("\n")))
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
