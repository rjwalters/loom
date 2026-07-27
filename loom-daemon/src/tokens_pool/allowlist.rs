//! Operator-managed allowlist for the OAuth token pool, ported from
//! `loom_tools.tokens.allowlist`.
//!
//! The `.allowlist` file at `.loom/tokens/.allowlist` constrains which
//! bootstrapped accounts the spawn-time selector is allowed to choose. When
//! the file is absent, all `.token` files are eligible.
//!
//! File format: one account name (token file stem, no extension) per line.
//! `#` introduces a line comment. Empty lines are ignored.
//!
//! Validation: account names must match an existing `<name>.token` file
//! **exactly** — no fuzzy/substring matching.

use std::path::{Path, PathBuf};

use super::locking::MkdirLock;
use super::paths::resolve_tokens_dir;

/// An operator supplied a name that does not match a bootstrapped account.
#[derive(Debug)]
pub struct UnknownAccountError {
    pub name: String,
    pub available: Vec<String>,
}

impl std::fmt::Display for UnknownAccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let avail = if self.available.is_empty() {
            "(none)".to_string()
        } else {
            self.available.join(", ")
        };
        write!(f, "Unknown account '{}'. Available: {avail}", self.name)
    }
}

impl std::error::Error for UnknownAccountError {}

/// Error type shared by the allowlist mutation functions.
#[derive(Debug)]
pub enum AllowlistError {
    Unknown(UnknownAccountError),
    Io(String),
}

impl std::fmt::Display for AllowlistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AllowlistError {}

fn tokens_dir(workspace: &Path) -> PathBuf {
    resolve_tokens_dir(workspace)
}

fn allowlist_path(workspace: &Path) -> PathBuf {
    tokens_dir(workspace).join(".allowlist")
}

fn lock_path(workspace: &Path) -> PathBuf {
    tokens_dir(workspace).join(".allowlist.lock")
}

/// Return all bootstrapped account names (token file stems), sorted.
#[must_use]
pub fn list_accounts(workspace: &Path) -> Vec<String> {
    let dir = tokens_dir(workspace);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            name.strip_suffix(".token").map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

fn strip_comment(line: &str) -> String {
    line.split('#').next().unwrap_or("").trim().to_string()
}

/// Return the current allowlist, in file order (duplicates dropped, keeping
/// first occurrence — matches Python's `seen` set).
#[must_use]
pub fn read_allowlist(workspace: &Path) -> Vec<String> {
    let path = allowlist_path(workspace);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let name = strip_comment(raw);
        if !name.is_empty() && seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

fn validate_names(workspace: &Path, names: &[String]) -> Result<Vec<String>, UnknownAccountError> {
    let available = list_accounts(workspace);
    let available_set: std::collections::HashSet<&str> =
        available.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    let mut resolved = Vec::new();
    for raw in names {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        if !available_set.contains(name) {
            return Err(UnknownAccountError {
                name: name.to_string(),
                available,
            });
        }
        if seen.insert(name.to_string()) {
            resolved.push(name.to_string());
        }
    }
    Ok(resolved)
}

/// Atomic same-directory write via a PID-suffixed temp file + rename (the
/// temp file lives beside `path` so the rename stays on one filesystem).
pub(super) fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp = path.with_file_name(format!("{file_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, content).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Replace the allowlist with exactly `names` (validated). Returns the
/// validated, deduplicated list that was written.
///
/// # Errors
/// Returns [`AllowlistError::Unknown`] if any name is not a bootstrapped
/// account, or [`AllowlistError::Io`] if the tokens dir does not exist / the
/// lock cannot be acquired.
pub fn write_allowlist(workspace: &Path, names: &[String]) -> Result<Vec<String>, AllowlistError> {
    let dir = tokens_dir(workspace);
    if !dir.is_dir() {
        return Err(AllowlistError::Io(format!(
            "Tokens dir does not exist: {}. Run `loom-tokens bootstrap` first.",
            dir.display()
        )));
    }
    let resolved = validate_names(workspace, names).map_err(AllowlistError::Unknown)?;
    let _lock = MkdirLock::acquire(&lock_path(workspace)).map_err(AllowlistError::Io)?;
    let path = allowlist_path(workspace);
    if resolved.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let body = format!("{}\n", resolved.join("\n"));
        atomic_write(&path, &body).map_err(AllowlistError::Io)?;
    }
    Ok(resolved)
}

/// Add `names` to the existing allowlist. Returns `(added, skipped_existing)`.
///
/// # Errors
/// See [`write_allowlist`].
pub fn add_to_allowlist(
    workspace: &Path,
    names: &[String],
) -> Result<(Vec<String>, Vec<String>), AllowlistError> {
    let dir = tokens_dir(workspace);
    if !dir.is_dir() {
        return Err(AllowlistError::Io(format!(
            "Tokens dir does not exist: {}. Run `loom-tokens bootstrap` first.",
            dir.display()
        )));
    }
    let resolved = validate_names(workspace, names).map_err(AllowlistError::Unknown)?;
    let _lock = MkdirLock::acquire(&lock_path(workspace)).map_err(AllowlistError::Io)?;
    let mut existing = read_allowlist(workspace);
    let mut existing_set: std::collections::HashSet<String> = existing.iter().cloned().collect();
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for n in resolved {
        if existing_set.contains(&n) {
            skipped.push(n);
        } else {
            existing_set.insert(n.clone());
            existing.push(n.clone());
            added.push(n);
        }
    }
    if !added.is_empty() {
        let body = format!("{}\n", existing.join("\n"));
        atomic_write(&allowlist_path(workspace), &body).map_err(AllowlistError::Io)?;
    }
    Ok((added, skipped))
}

/// Remove `names` from the allowlist. Removing the last account drops the
/// file entirely. Returns `(removed, skipped_missing)`.
///
/// # Errors
/// See [`write_allowlist`].
pub fn remove_from_allowlist(
    workspace: &Path,
    names: &[String],
) -> Result<(Vec<String>, Vec<String>), AllowlistError> {
    let dir = tokens_dir(workspace);
    if !dir.is_dir() {
        return Err(AllowlistError::Io(format!(
            "Tokens dir does not exist: {}. Run `loom-tokens bootstrap` first.",
            dir.display()
        )));
    }
    let resolved = validate_names(workspace, names).map_err(AllowlistError::Unknown)?;
    let to_remove: std::collections::HashSet<&str> = resolved.iter().map(String::as_str).collect();
    let _lock = MkdirLock::acquire(&lock_path(workspace)).map_err(AllowlistError::Io)?;
    let existing = read_allowlist(workspace);
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for n in &existing {
        if to_remove.contains(n.as_str()) {
            removed.push(n.clone());
        } else {
            kept.push(n.clone());
        }
    }
    let present: std::collections::HashSet<&str> = existing.iter().map(String::as_str).collect();
    let mut skipped = Vec::new();
    for n in &resolved {
        if !present.contains(n.as_str()) {
            skipped.push(n.clone());
        }
    }
    let path = allowlist_path(workspace);
    if kept.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let body = format!("{}\n", kept.join("\n"));
        atomic_write(&path, &body).map_err(AllowlistError::Io)?;
    }
    Ok((removed, skipped))
}

/// Remove the allowlist file entirely. Returns `true` if a file was removed.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn clear_allowlist(workspace: &Path) -> Result<bool, String> {
    let _lock = MkdirLock::acquire(&lock_path(workspace))?;
    let path = allowlist_path(workspace);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_pool(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        for n in names {
            fs::write(dir.join(format!("{n}.token")), "sk-ant-oat01-fake").unwrap();
        }
        tmp
    }

    fn ss(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn list_accounts_sorted() {
        let tmp = make_pool(&["b", "a", "c"]);
        assert_eq!(list_accounts(tmp.path()), vec!["a", "b", "c"]);
    }

    #[test]
    fn read_allowlist_empty_when_missing() {
        let tmp = make_pool(&["a"]);
        assert!(read_allowlist(tmp.path()).is_empty());
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = make_pool(&["a", "b", "c"]);
        let written = write_allowlist(tmp.path(), &ss(&["a", "b"])).unwrap();
        assert_eq!(written, vec!["a", "b"]);
        assert_eq!(read_allowlist(tmp.path()), vec!["a", "b"]);
    }

    #[test]
    fn write_unknown_account_errors() {
        let tmp = make_pool(&["a"]);
        let err = write_allowlist(tmp.path(), &ss(&["ghost"])).unwrap_err();
        assert!(matches!(err, AllowlistError::Unknown(_)));
    }

    #[test]
    fn write_empty_removes_file() {
        let tmp = make_pool(&["a"]);
        write_allowlist(tmp.path(), &ss(&["a"])).unwrap();
        assert!(allowlist_path(tmp.path()).exists());
        write_allowlist(tmp.path(), &[]).unwrap();
        assert!(!allowlist_path(tmp.path()).exists());
    }

    #[test]
    fn add_dedups_and_reports_skipped() {
        let tmp = make_pool(&["a", "b", "c"]);
        write_allowlist(tmp.path(), &ss(&["a"])).unwrap();
        let (added, skipped) = add_to_allowlist(tmp.path(), &ss(&["a", "b"])).unwrap();
        assert_eq!(added, vec!["b"]);
        assert_eq!(skipped, vec!["a"]);
        assert_eq!(read_allowlist(tmp.path()), vec!["a", "b"]);
    }

    #[test]
    fn remove_last_drops_file() {
        let tmp = make_pool(&["a"]);
        write_allowlist(tmp.path(), &ss(&["a"])).unwrap();
        let (removed, skipped) = remove_from_allowlist(tmp.path(), &ss(&["a"])).unwrap();
        assert_eq!(removed, vec!["a"]);
        assert!(skipped.is_empty());
        assert!(!allowlist_path(tmp.path()).exists());
    }

    #[test]
    fn remove_reports_names_not_present() {
        let tmp = make_pool(&["a", "b"]);
        write_allowlist(tmp.path(), &ss(&["a"])).unwrap();
        let (removed, skipped) = remove_from_allowlist(tmp.path(), &ss(&["b"])).unwrap();
        assert!(removed.is_empty());
        assert_eq!(skipped, vec!["b"]);
    }

    #[test]
    fn clear_allowlist_reports_whether_file_existed() {
        let tmp = make_pool(&["a"]);
        assert!(!clear_allowlist(tmp.path()).unwrap());
        write_allowlist(tmp.path(), &ss(&["a"])).unwrap();
        assert!(clear_allowlist(tmp.path()).unwrap());
    }

    #[test]
    fn strip_comment_ignores_hash_suffix() {
        assert_eq!(strip_comment("agent-1 # pinned"), "agent-1");
        assert_eq!(strip_comment("# full comment"), "");
        assert_eq!(strip_comment("  "), "");
    }
}
