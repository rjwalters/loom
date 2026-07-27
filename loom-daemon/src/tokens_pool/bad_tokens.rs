//! Bad-token tracking, ported from `loom_tools.tokens.bad_tokens`.
//!
//! Tokens that fail with `TOKEN_EXPIRED`, `TOKEN_EXHAUSTED`, or otherwise
//! prove unusable are appended to `.loom/tokens/.bad_tokens`. Subsequent
//! selection calls skip these tokens.
//!
//! File format (one entry per line):
//! `<ISO8601 UTC timestamp> <token_name> <reason words...>`
//!
//! Reads use a word-boundary regex so `agent-1` and `agent-10` do not
//! collide — the exact behavior the Python word-boundary regex provides.

use std::path::{Path, PathBuf};

use chrono::Utc;
use regex::Regex;

use super::locking::MkdirLock;
use super::paths::resolve_tokens_dir;

fn tokens_dir(workspace: &Path) -> PathBuf {
    resolve_tokens_dir(workspace)
}

fn bad_tokens_path(tokens_dir: &Path) -> PathBuf {
    tokens_dir.join(".bad_tokens")
}

fn lock_path(tokens_dir: &Path) -> PathBuf {
    tokens_dir.join(".bad_tokens.lock")
}

fn name_pattern(token_name: &str) -> Regex {
    let escaped = regex::escape(token_name);
    // (?m) matches Python's re.MULTILINE for ^/$ anchors.
    Regex::new(&format!(r"(?m)(^|\s){escaped}(\s|$)")).expect("valid generated regex")
}

/// Reasons treated as "auth" for `unblock` (ported from `cli.py`'s
/// `_AUTH_REASON_RE`). Matched case-insensitively against the free-form
/// reason field. Deliberately does **not** match `TOKEN_EXHAUSTED` — those
/// expire on their own.
fn auth_reason_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(401|oauth|auth(entication)?|unauthorized|token[_\s]?expired|expired|blocked)\b",
        )
        .expect("valid generated regex")
    })
}

/// Append a bad-token entry atomically.
///
/// # Errors
/// Returns an error if the tokens dir does not exist or the lock cannot be
/// acquired.
pub fn mark_bad(workspace: &Path, token_name: &str, reason: &str) -> Result<(), String> {
    let dir = tokens_dir(workspace);
    if !dir.is_dir() {
        return Err(format!("Tokens dir does not exist: {}", dir.display()));
    }

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let safe_reason = reason.replace(['\n', '\r'], " ");
    let safe_reason = safe_reason.trim();
    let line = format!("{timestamp} {token_name} {safe_reason}\n");

    let _lock = MkdirLock::acquire(&lock_path(&dir))?;
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(bad_tokens_path(&dir))
        .map_err(|e| e.to_string())?;
    f.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Return `true` if `token_name` appears in the bad_tokens file.
///
/// Reads are unsynchronized — readers see a consistent file because writers
/// only ever append whole lines.
#[must_use]
pub fn is_bad(workspace: &Path, token_name: &str) -> bool {
    let bad_file = bad_tokens_path(&tokens_dir(workspace));
    let Ok(text) = std::fs::read_to_string(&bad_file) else {
        return false;
    };
    name_pattern(token_name).is_match(&text)
}

/// Drop bad_tokens entries older than `max_age_seconds`. Returns the number
/// of entries retained after pruning.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn cleanup_bad_tokens(workspace: &Path, max_age_seconds: i64) -> Result<usize, String> {
    let dir = tokens_dir(workspace);
    let bad_file = bad_tokens_path(&dir);
    if !bad_file.is_file() {
        return Ok(0);
    }

    let cutoff = Utc::now().timestamp() - max_age_seconds;

    let _lock = MkdirLock::acquire(&lock_path(&dir))?;
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let mut kept: Vec<String> = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let ts_str = stripped.split(' ').next().unwrap_or("");
        match chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%SZ") {
            Ok(naive) => {
                let ts_epoch = naive.and_utc().timestamp();
                if ts_epoch >= cutoff {
                    kept.push(line.to_string());
                }
            }
            Err(_) => {
                // Malformed line — keep it so we don't silently lose data.
                kept.push(line.to_string());
            }
        }
    }

    let tmp = bad_file.with_extension("tmp");
    let body = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &bad_file).map_err(|e| e.to_string())?;

    Ok(kept.len())
}

/// Remove entries for `names` from `.bad_tokens` (operator `unblock`, ported
/// from `cli.py::_cmd_unblock`). By default only entries whose reason field
/// matches [`auth_reason_regex`] are dropped ("TTL entries clear
/// themselves" — non-auth reasons are left for [`cleanup_bad_tokens`] /
/// natural expiry); `all_reasons` also drops non-auth entries for the given
/// names. Malformed lines (fewer than 2 whitespace-separated fields) are
/// always kept so we never silently lose data. Returns `(removed, kept)`.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or the file cannot be
/// read/written.
pub fn unblock(
    workspace: &Path,
    names: &[String],
    all_reasons: bool,
) -> Result<(usize, usize), String> {
    let dir = tokens_dir(workspace);
    let bad_file = bad_tokens_path(&dir);
    if !bad_file.is_file() {
        return Ok((0, 0));
    }

    let target: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();

    let _lock = MkdirLock::acquire(&lock_path(&dir))?;
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let mut removed = 0usize;
    let mut kept: Vec<String> = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.splitn(3, ' ');
        let _timestamp = parts.next();
        let entry_name = parts.next();
        let reason = parts.next().unwrap_or("");
        if let Some(name) = entry_name {
            if target.contains(name) && (all_reasons || auth_reason_regex().is_match(reason)) {
                removed += 1;
                continue;
            }
        }
        kept.push(line.to_string());
    }

    let tmp = bad_file.with_extension("tmp");
    let body = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &bad_file).map_err(|e| e.to_string())?;

    Ok((removed, kept.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn make_pool() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        // resolve_tokens_dir() only picks the per-repo pool when it holds at
        // least one `*.token` file — seed one so these tests deterministically
        // exercise the per-repo pool rather than falling back to this host's
        // real shared pool (~/.loom/tokens) when it is empty.
        fs::write(dir.join("seed.token"), "sk-ant-oat01-fake").unwrap();
        tmp
    }

    fn pool_dir(ws: &Path) -> PathBuf {
        ws.join(".loom").join("tokens")
    }

    #[test]
    fn mark_and_check_bad() {
        let tmp = make_pool();
        assert!(!is_bad(tmp.path(), "agent-1"));
        mark_bad(tmp.path(), "agent-1", "exhausted: 429").unwrap();
        assert!(is_bad(tmp.path(), "agent-1"));
    }

    #[test]
    fn word_boundary_does_not_confuse_agent_1_and_agent_10() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "agent-10", "auth").unwrap();
        assert!(!is_bad(tmp.path(), "agent-1"));
        assert!(is_bad(tmp.path(), "agent-10"));
    }

    #[test]
    fn is_bad_false_when_file_missing() {
        let tmp = make_pool();
        assert!(!is_bad(tmp.path(), "agent-1"));
    }

    #[test]
    fn mark_bad_strips_newlines_from_reason() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "agent-1", "line1\nline2\r\n").unwrap();
        let text = fs::read_to_string(pool_dir(tmp.path()).join(".bad_tokens")).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("line1 line2"));
    }

    #[test]
    #[serial]
    fn mark_bad_errors_when_dir_missing() {
        // Neither the per-repo pool nor the shared pool exists here — disable
        // the shared fallback so this doesn't resolve to this host's real
        // ~/.loom/tokens (see `super::super::paths::SHARED_TOKENS_DIR_ENV`).
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let err = mark_bad(&tmp.path().join("nope"), "agent-1", "x");
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
        assert!(err.is_err());
    }

    #[test]
    fn cleanup_drops_old_entries_keeps_fresh() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        let old = (Utc::now() - chrono::Duration::seconds(1000))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let fresh = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!("{old} agent-old exhausted\n{fresh} agent-new exhausted\n"),
        )
        .unwrap();

        let kept = cleanup_bad_tokens(tmp.path(), 500).unwrap();
        assert_eq!(kept, 1);
        assert!(!is_bad(tmp.path(), "agent-old"));
        assert!(is_bad(tmp.path(), "agent-new"));
    }

    #[test]
    fn cleanup_keeps_malformed_lines() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        fs::write(dir.join(".bad_tokens"), "garbage line with no timestamp\n").unwrap();
        let kept = cleanup_bad_tokens(tmp.path(), 1).unwrap();
        assert_eq!(kept, 1);
    }

    #[test]
    fn cleanup_no_file_is_noop() {
        let tmp = make_pool();
        assert_eq!(cleanup_bad_tokens(tmp.path(), 100).unwrap(), 0);
    }

    #[test]
    fn unblock_no_file_is_noop() {
        let tmp = make_pool();
        assert_eq!(unblock(tmp.path(), &["a".to_string()], false).unwrap(), (0, 0));
    }

    #[test]
    fn unblock_removes_auth_reason_by_default() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "a", "401 unauthorized").unwrap();
        mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();
        let (removed, kept) =
            unblock(tmp.path(), &["a".to_string(), "b".to_string()], false).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(kept, 1);
        assert!(!is_bad(tmp.path(), "a"));
        assert!(is_bad(tmp.path(), "b"));
    }

    #[test]
    fn unblock_all_reasons_drops_non_auth_too() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();
        let (removed, kept) = unblock(tmp.path(), &["b".to_string()], true).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(kept, 0);
        assert!(!is_bad(tmp.path(), "b"));
    }

    #[test]
    fn unblock_ignores_unrelated_names() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "a", "auth failure").unwrap();
        let (removed, kept) = unblock(tmp.path(), &["c".to_string()], false).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(kept, 1);
        assert!(is_bad(tmp.path(), "a"));
    }

    #[test]
    fn unblock_keeps_malformed_lines() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        fs::write(dir.join(".bad_tokens"), "onlyoneword\n").unwrap();
        let (removed, kept) = unblock(tmp.path(), &["onlyoneword".to_string()], true).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(kept, 1);
    }

    #[test]
    fn auth_reason_regex_does_not_match_exhausted() {
        assert!(!auth_reason_regex().is_match("exhausted: weekly limit"));
        assert!(auth_reason_regex().is_match("401 Unauthorized"));
        assert!(auth_reason_regex().is_match("oauth token expired"));
    }
}
