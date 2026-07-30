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

/// Default cooldown (seconds) after which a non-auth (exhaustion) bad-token
/// entry stops blocking selection (#4122). Weekly/session-limit exhaustion is
/// transient — the account recovers on its own — so those entries expire while
/// auth-reason entries (a broken credential) remain permanent. Mirrors the
/// Python reference default (`bad_tokens.py::cleanup_bad_tokens`, 6h).
pub const DEFAULT_EXHAUSTION_COOLDOWN_SECS: i64 = 6 * 3600;

/// Env override (whole seconds, must parse to `> 0`) for
/// [`DEFAULT_EXHAUSTION_COOLDOWN_SECS`].
pub const EXHAUSTION_COOLDOWN_ENV: &str = "LOOM_TOKEN_EXHAUSTION_COOLDOWN_SECS";

/// On-disk retention for **non-auth** (exhaustion) entries on the routine
/// cleanup path (#4643).
///
/// 24h = 4× the default 6h read-time cooldown: an exhaustion line stays on
/// disk long after it stopped blocking selection (so an operator debugging the
/// previous night's incident still sees it), but `.bad_tokens` can no longer
/// grow without bound — the live shared pool had accumulated entries for days
/// because [`cleanup_bad_tokens`] had **zero callers** before #4643.
pub const DEFAULT_CLEANUP_MAX_AGE_SECS: i64 = 24 * 3600;

/// Floor retention for **auth** entries on the cleanup path (#4643).
///
/// Auth entries are permanent *at read time* ([`is_bad`] never expires them),
/// so pruning one on the routine 24h schedule would silently readmit a broken
/// credential into rotation — exactly the failure mode the auth/exhaustion
/// split exists to prevent. Cleanup therefore never drops an auth entry that
/// is younger than 30 days, regardless of the `max_age_seconds` it was called
/// with; only genuinely ancient auth lines (a credential retired a month ago)
/// are reclaimed. The permanence contract is: **exhaustion entries clear
/// themselves, auth entries clear only via `loom-daemon tokens unblock`** —
/// the 30d floor is garbage collection, not expiry.
pub const AUTH_ENTRY_MIN_RETENTION_SECS: i64 = 30 * 24 * 3600;

/// Resolve the exhaustion-entry cooldown: the [`EXHAUSTION_COOLDOWN_ENV`]
/// override (whole seconds, `> 0`) or [`DEFAULT_EXHAUSTION_COOLDOWN_SECS`].
#[must_use]
pub fn exhaustion_cooldown_secs() -> i64 {
    std::env::var(EXHAUSTION_COOLDOWN_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_EXHAUSTION_COOLDOWN_SECS)
}

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

/// Why a `.bad_tokens` entry blocks selection — the reason class an operator
/// needs in order to know whether the block clears itself (#4643).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadReasonClass {
    /// Auth reason (401 / OAuth / expired / blocked) — permanent at read time,
    /// clears only via `loom-daemon tokens unblock <name>`.
    Auth,
    /// Non-auth ("exhausted" / "rate-limited") — expires by itself once the
    /// entry ages past [`exhaustion_cooldown_secs`] (#4122).
    Exhaustion,
    /// The entry's timestamp did not parse. Fail-closed: treated as permanent,
    /// because we never silently un-block a token on a malformed line.
    MalformedTimestamp,
}

impl BadReasonClass {
    /// Short class label used in operator-facing output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Exhaustion => "exhaustion",
            Self::MalformedTimestamp => "malformed-timestamp",
        }
    }

    /// Whether entries of this class ever clear without operator action.
    #[must_use]
    pub fn permanence(self) -> &'static str {
        match self {
            Self::Auth => "permanent",
            Self::Exhaustion => "TTL",
            Self::MalformedTimestamp => "permanent (fail-closed)",
        }
    }
}

/// The `.bad_tokens` entry that is currently keeping a token out of rotation,
/// as returned by [`blocking_entry`] (#4643). Everything an operator needs to
/// tell "this clears itself in 12 minutes" apart from "this needs `unblock`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingEntry {
    /// Raw first field of the line (the ISO-8601 UTC timestamp, or whatever
    /// unparseable text stood in its place).
    pub timestamp: String,
    /// Free-form reason text as recorded by [`mark_bad`].
    pub reason: String,
    /// Auth (permanent) vs exhaustion (TTL) vs malformed (fail-closed).
    pub class: BadReasonClass,
    /// Seconds until this entry stops blocking selection. `Some` only for
    /// [`BadReasonClass::Exhaustion`]; `None` means "never, without operator
    /// action".
    pub cooldown_remaining_secs: Option<i64>,
}

/// Return the `.bad_tokens` entry currently blocking `token_name`, or `None`
/// when the token is selectable.
///
/// This is the single scan that decides bad-ness: [`is_bad`] is defined as
/// `blocking_entry(..).is_some()`, so the boolean the selector acts on and the
/// detail the operator reads can never de-sync (#4643 — diagnosing the
/// 2026-07-30 incident required reading Rust source precisely because the
/// selector's decision was unexplained).
///
/// Auth-reason entries (a broken credential — matched by [`auth_reason_regex`])
/// block permanently. Non-auth ("exhaustion") entries block only until they age
/// past [`exhaustion_cooldown_secs`] (#4122): weekly/session-limit exhaustion is
/// transient, so a stale exhaustion line no longer keeps an otherwise-healthy
/// account out of rotation even before [`cleanup_bad_tokens`] prunes it from
/// disk. A line whose timestamp cannot be parsed is treated as permanent
/// (fail-closed). An *expired* entry does not short-circuit the scan — a later
/// line for the same account may still block (this is the common live shape:
/// every retry appends a fresh line, so the newest entry, not the oldest
/// visible one, is the one that matters).
///
/// Reads are unsynchronized — readers see a consistent file because writers
/// only ever append whole lines.
#[must_use]
pub fn blocking_entry(workspace: &Path, token_name: &str) -> Option<BlockingEntry> {
    let bad_file = bad_tokens_path(&tokens_dir(workspace));
    let text = std::fs::read_to_string(&bad_file).ok()?;
    let pattern = name_pattern(token_name);
    let cooldown = exhaustion_cooldown_secs();
    let now = Utc::now().timestamp();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || !pattern.is_match(stripped) {
            continue;
        }
        // Parse `<ts> <name> <reason...>` from this matching line.
        let mut parts = stripped.splitn(3, ' ');
        let ts_str = parts.next().unwrap_or("");
        let _name = parts.next();
        let reason = parts.next().unwrap_or("");
        // Auth entries never expire.
        if auth_reason_regex().is_match(reason) {
            return Some(BlockingEntry {
                timestamp: ts_str.to_string(),
                reason: reason.to_string(),
                class: BadReasonClass::Auth,
                cooldown_remaining_secs: None,
            });
        }
        // Non-auth (exhaustion) entries expire after the cooldown. A
        // malformed/missing timestamp fails closed (permanent).
        match chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%SZ") {
            Ok(naive) => {
                let age = now - naive.and_utc().timestamp();
                if age < cooldown {
                    return Some(BlockingEntry {
                        timestamp: ts_str.to_string(),
                        reason: reason.to_string(),
                        class: BadReasonClass::Exhaustion,
                        cooldown_remaining_secs: Some(cooldown - age),
                    });
                }
            }
            Err(_) => {
                return Some(BlockingEntry {
                    timestamp: ts_str.to_string(),
                    reason: reason.to_string(),
                    class: BadReasonClass::MalformedTimestamp,
                    cooldown_remaining_secs: None,
                })
            }
        }
    }
    None
}

/// Return `true` if `token_name` is currently bad-marked.
///
/// Thin boolean projection of [`blocking_entry`] — see there for the
/// auth-vs-exhaustion permanence rules.
#[must_use]
pub fn is_bad(workspace: &Path, token_name: &str) -> bool {
    blocking_entry(workspace, token_name).is_some()
}

/// Outcome of a [`cleanup_bad_tokens_in_dir`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupOutcome {
    /// Entries still on disk after the pass.
    pub kept: usize,
    /// Entries dropped by this pass (`0` ⇒ the file was not rewritten).
    pub removed: usize,
}

/// Split `text` into (retained lines, removed count) under the reason-aware
/// max-age policy. Pure — no I/O — so the lock-free pre-check and the
/// under-lock rewrite in [`cleanup_bad_tokens_in_dir`] apply identical rules.
fn partition_by_age(text: &str, max_age_seconds: i64, now: i64) -> (Vec<String>, usize) {
    let cutoff = now - max_age_seconds;
    // Auth entries are permanent at read time, so they get a much longer floor
    // (see [`AUTH_ENTRY_MIN_RETENTION_SECS`]): never dropped before 30 days,
    // no matter how aggressive the requested max age is.
    let auth_cutoff = now - max_age_seconds.max(AUTH_ENTRY_MIN_RETENTION_SECS);
    let mut kept: Vec<String> = Vec::new();
    let mut removed = 0usize;
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.splitn(3, ' ');
        let ts_str = parts.next().unwrap_or("");
        let _name = parts.next();
        let reason = parts.next().unwrap_or("");
        let entry_cutoff = if auth_reason_regex().is_match(reason) {
            auth_cutoff
        } else {
            cutoff
        };
        match chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%SZ") {
            Ok(naive) => {
                if naive.and_utc().timestamp() >= entry_cutoff {
                    kept.push(line.to_string());
                } else {
                    removed += 1;
                }
            }
            Err(_) => {
                // Malformed line — keep it so we don't silently lose data.
                kept.push(line.to_string());
            }
        }
    }
    (kept, removed)
}

/// Drop `.bad_tokens` entries in `dir` older than `max_age_seconds`, with
/// **auth** entries held for at least [`AUTH_ENTRY_MIN_RETENTION_SECS`]
/// (#4643). Malformed lines are always retained.
///
/// The common case (nothing prunable) costs one unsynchronized read and takes
/// **no lock at all** — this runs on the `tokens select` hot path, where a
/// burst of concurrent spawns must not serialize on the `.bad_tokens` lock.
/// The file is re-read and re-partitioned under the lock before any rewrite,
/// so a line appended between the pre-check and the lock is never lost.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or the file cannot be
/// read/written.
pub fn cleanup_bad_tokens_in_dir(
    dir: &Path,
    max_age_seconds: i64,
) -> Result<CleanupOutcome, String> {
    let bad_file = bad_tokens_path(dir);
    if !bad_file.is_file() {
        return Ok(CleanupOutcome {
            kept: 0,
            removed: 0,
        });
    }

    // Lock-free pre-check: bail out without touching the lock when there is
    // nothing to prune (the steady state on a healthy pool).
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let (kept, removed) = partition_by_age(&text, max_age_seconds, Utc::now().timestamp());
    if removed == 0 {
        return Ok(CleanupOutcome {
            kept: kept.len(),
            removed: 0,
        });
    }

    let _lock = MkdirLock::acquire(&lock_path(dir))?;
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let (kept, removed) = partition_by_age(&text, max_age_seconds, Utc::now().timestamp());
    if removed == 0 {
        return Ok(CleanupOutcome {
            kept: kept.len(),
            removed: 0,
        });
    }

    let tmp = bad_file.with_extension("tmp");
    let body = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &bad_file).map_err(|e| e.to_string())?;

    Ok(CleanupOutcome {
        kept: kept.len(),
        removed,
    })
}

/// Workspace-anchored [`cleanup_bad_tokens_in_dir`]. Returns the number of
/// entries retained after pruning.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn cleanup_bad_tokens(workspace: &Path, max_age_seconds: i64) -> Result<usize, String> {
    cleanup_bad_tokens_in_dir(&tokens_dir(workspace), max_age_seconds).map(|o| o.kept)
}

/// Outcome of [`unblock`]. `excluded` names the accounts that still have a
/// non-auth ("exhausted") entry left in place because the default scope (no
/// `all_reasons`) does not drop them (#4212). First-seen file order,
/// de-duplicated, so the daemon CLI and the Python `cli.py::_cmd_unblock`
/// emit a byte-identical `excluded` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnblockOutcome {
    pub removed: usize,
    pub kept: usize,
    pub excluded: Vec<String>,
}

/// Remove entries for `names` from `.bad_tokens` (operator `unblock`, ported
/// from `cli.py::_cmd_unblock`). By default only entries whose reason field
/// matches [`auth_reason_regex`] are dropped ("TTL entries clear
/// themselves" — non-auth reasons are left for [`cleanup_bad_tokens`] /
/// natural expiry); `all_reasons` also drops non-auth entries for the given
/// names. Malformed lines (fewer than 2 whitespace-separated fields) are
/// always kept so we never silently lose data.
///
/// Returns an [`UnblockOutcome`]. `excluded` lists the named accounts whose
/// non-auth entries the default scope left in place — the caller surfaces
/// these and exits non-zero (#4212) so a no-op `unblock` can no longer look
/// like success on a still-poisoned pool.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or the file cannot be
/// read/written.
pub fn unblock(
    workspace: &Path,
    names: &[String],
    all_reasons: bool,
) -> Result<UnblockOutcome, String> {
    let dir = tokens_dir(workspace);
    let bad_file = bad_tokens_path(&dir);
    if !bad_file.is_file() {
        return Ok(UnblockOutcome {
            removed: 0,
            kept: 0,
            excluded: Vec::new(),
        });
    }

    let target: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();

    let _lock = MkdirLock::acquire(&lock_path(&dir))?;
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let mut removed = 0usize;
    let mut kept: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    let mut excluded_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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
            if target.contains(name) {
                if all_reasons || auth_reason_regex().is_match(reason) {
                    removed += 1;
                    continue;
                }
                // A named account with a non-auth entry that the DEFAULT scope
                // leaves behind — record it so the caller can name it and fail.
                if excluded_seen.insert(name.to_string()) {
                    excluded.push(name.to_string());
                }
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

    Ok(UnblockOutcome {
        removed,
        kept: kept.len(),
        excluded,
    })
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

    /// #4122: an exhaustion (non-auth) entry older than the cooldown no longer
    /// reports `is_bad`, while an auth entry of the same age remains permanent.
    #[test]
    fn exhaustion_entry_expires_after_cooldown_auth_stays() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        // Both entries are 7h old — past the 6h default cooldown.
        let old = (Utc::now() - chrono::Duration::seconds(7 * 3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!(
                "{old} agent-exh exhausted: weekly limit\n\
                 {old} agent-auth 401 unauthorized\n"
            ),
        )
        .unwrap();
        // Exhaustion entry has aged out — token is selectable again even though
        // the line is still on disk (cleanup has not run yet).
        assert!(!is_bad(tmp.path(), "agent-exh"));
        assert_eq!(blocking_entry(tmp.path(), "agent-exh"), None);
        // Auth entry is permanent — unaffected by the TTL.
        assert!(is_bad(tmp.path(), "agent-auth"));
        // #4643: the same scan now also explains itself. The auth entry is
        // reported as permanent with no cooldown remaining.
        let entry = blocking_entry(tmp.path(), "agent-auth").expect("auth entry blocks");
        assert_eq!(entry.class, BadReasonClass::Auth);
        assert_eq!(entry.class.permanence(), "permanent");
        assert_eq!(entry.cooldown_remaining_secs, None);
        assert_eq!(entry.reason, "401 unauthorized");
        assert_eq!(entry.timestamp, old);
    }

    /// #4643: a *fresh* exhaustion entry reports its class, its own timestamp,
    /// and how long is left on the cooldown — the detail the empty-pool error
    /// renders per token.
    #[test]
    fn blocking_entry_reports_exhaustion_class_and_cooldown_remaining() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        // 1h old → 5h of the 6h default cooldown remain.
        let ts = (Utc::now() - chrono::Duration::seconds(3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!("{ts} agent-1 exhausted: hit your session limit\n"),
        )
        .unwrap();
        let entry = blocking_entry(tmp.path(), "agent-1").expect("fresh entry blocks");
        assert_eq!(entry.class, BadReasonClass::Exhaustion);
        assert_eq!(entry.class.label(), "exhaustion");
        assert_eq!(entry.class.permanence(), "TTL");
        assert_eq!(entry.timestamp, ts);
        assert_eq!(entry.reason, "exhausted: hit your session limit");
        let remaining = entry
            .cooldown_remaining_secs
            .expect("TTL entry has a remaining");
        assert!(
            (4 * 3600..=5 * 3600).contains(&remaining),
            "expected ~5h remaining, got {remaining}"
        );
    }

    /// #4643: the incident shape — an account whose *oldest* visible entry has
    /// long expired but which was re-marked recently is still blocked, and the
    /// entry reported is the deciding (fresh) one, not the stale one an
    /// operator would see at the top of the file.
    #[test]
    fn blocking_entry_reports_the_deciding_fresh_line_not_the_stale_one() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        let old = (Utc::now() - chrono::Duration::seconds(13 * 3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let fresh = (Utc::now() - chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!(
                "{old} agent-1 exhausted: hit your limit\n\
                 {fresh} agent-1 exhausted: hit your weekly limit\n"
            ),
        )
        .unwrap();
        let entry = blocking_entry(tmp.path(), "agent-1").expect("re-marked account blocks");
        assert_eq!(entry.timestamp, fresh);
        assert_eq!(entry.reason, "exhausted: hit your weekly limit");
    }

    /// #4643: an unparseable timestamp is reported as fail-closed permanent,
    /// matching [`is_bad`]'s long-standing behavior.
    #[test]
    fn blocking_entry_classifies_malformed_timestamp_as_permanent() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        fs::write(dir.join(".bad_tokens"), "not-a-timestamp agent-1 exhausted\n").unwrap();
        let entry = blocking_entry(tmp.path(), "agent-1").expect("malformed line fails closed");
        assert_eq!(entry.class, BadReasonClass::MalformedTimestamp);
        assert_eq!(entry.cooldown_remaining_secs, None);
        assert!(entry.class.permanence().starts_with("permanent"));
    }

    /// #4122: a fresh exhaustion entry still blocks (the cooldown only expires
    /// aged entries).
    #[test]
    fn fresh_exhaustion_entry_still_blocks() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "agent-1", "exhausted: weekly limit").unwrap();
        assert!(is_bad(tmp.path(), "agent-1"));
    }

    /// #4122: a matching line with an unparseable timestamp fails closed
    /// (treated as permanently bad) so a malformed entry never un-blocks a
    /// token.
    #[test]
    fn malformed_timestamp_fails_closed() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        fs::write(dir.join(".bad_tokens"), "not-a-timestamp agent-1 exhausted\n").unwrap();
        assert!(is_bad(tmp.path(), "agent-1"));
    }

    #[test]
    #[serial]
    fn exhaustion_cooldown_env_override() {
        std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "100");
        assert_eq!(exhaustion_cooldown_secs(), 100);
        std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "0");
        assert_eq!(exhaustion_cooldown_secs(), DEFAULT_EXHAUSTION_COOLDOWN_SECS);
        std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "garbage");
        assert_eq!(exhaustion_cooldown_secs(), DEFAULT_EXHAUSTION_COOLDOWN_SECS);
        std::env::remove_var(EXHAUSTION_COOLDOWN_ENV);
        assert_eq!(exhaustion_cooldown_secs(), DEFAULT_EXHAUSTION_COOLDOWN_SECS);
    }

    /// #4643: the *reported* cooldown remaining tracks the
    /// [`EXHAUSTION_COOLDOWN_ENV`] override, not just the default — otherwise
    /// the empty-pool error would tell an operator who shortened the cooldown
    /// to wait hours that do not apply.
    #[test]
    #[serial]
    fn blocking_entry_cooldown_remaining_honors_env_override() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        // 10 minutes old.
        let ts = (Utc::now() - chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!("{ts} agent-1 exhausted: hit your session limit\n"),
        )
        .unwrap();

        // 30-minute cooldown → ~20 minutes left (default 6h would report ~5h50m).
        std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "1800");
        let entry = blocking_entry(tmp.path(), "agent-1").expect("still inside a 30m cooldown");
        let remaining = entry.cooldown_remaining_secs.expect("TTL entry");
        // 15-minute cooldown → already expired, so the token is selectable.
        std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "300");
        let expired = blocking_entry(tmp.path(), "agent-1");
        std::env::remove_var(EXHAUSTION_COOLDOWN_ENV);

        assert!(
            (1100..=1200).contains(&remaining),
            "expected ~20m remaining under the override, got {remaining}s"
        );
        assert_eq!(expired, None, "a 300s cooldown should have expired a 600s-old entry");
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

    /// #4643: the wired path — exactly the call `loom-daemon tokens select`
    /// makes — prunes an over-age exhaustion entry off disk while leaving a
    /// recent one alone. Before #4643 `cleanup_bad_tokens` had zero callers, so
    /// pools accumulated expired entries indefinitely.
    #[test]
    fn wired_cleanup_prunes_over_age_exhaustion_entry() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        let ancient = (Utc::now() - chrono::Duration::seconds(DEFAULT_CLEANUP_MAX_AGE_SECS + 3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let recent = (Utc::now() - chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!(
                "{ancient} agent-old exhausted: hit your session limit\n\
                 {recent} agent-new exhausted: hit your session limit\n"
            ),
        )
        .unwrap();

        let kept = cleanup_bad_tokens(tmp.path(), DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
        assert_eq!(kept, 1);
        let text = fs::read_to_string(dir.join(".bad_tokens")).unwrap();
        assert!(!text.contains("agent-old"), "over-age entry still on disk: {text}");
        assert!(text.contains("agent-new"));
        assert!(is_bad(tmp.path(), "agent-new"));
    }

    /// #4643: auth entries are held for [`AUTH_ENTRY_MIN_RETENTION_SECS`]
    /// regardless of the requested max age — pruning one early would silently
    /// readmit a broken credential, since `is_bad` treats auth as permanent.
    #[test]
    fn cleanup_holds_auth_entries_past_the_requested_max_age() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        // Older than the routine 24h policy, far younger than the 30d floor.
        let old = (Utc::now() - chrono::Duration::seconds(3 * 24 * 3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            dir.join(".bad_tokens"),
            format!(
                "{old} agent-auth 401 unauthorized\n\
                 {old} agent-exh exhausted: hit your session limit\n"
            ),
        )
        .unwrap();

        let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.kept, 1);
        let text = fs::read_to_string(dir.join(".bad_tokens")).unwrap();
        assert!(text.contains("agent-auth"), "auth entry was pruned early: {text}");
        assert!(!text.contains("agent-exh"));
        // Read-time semantics are unchanged: auth still blocks, and the
        // pruned exhaustion entry had already stopped blocking.
        assert!(is_bad(tmp.path(), "agent-auth"));
        assert!(!is_bad(tmp.path(), "agent-exh"));
    }

    /// #4643: an auth entry past the 30d floor is finally reclaimed (garbage
    /// collection of a credential retired a month ago), not held forever.
    #[test]
    fn cleanup_reclaims_auth_entries_past_the_retention_floor() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        let ancient = (Utc::now()
            - chrono::Duration::seconds(AUTH_ENTRY_MIN_RETENTION_SECS + 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
        fs::write(dir.join(".bad_tokens"), format!("{ancient} agent-auth 401 unauthorized\n"))
            .unwrap();
        let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.kept, 0);
    }

    /// #4643: nothing prunable ⇒ no rewrite at all (the `tokens select` hot
    /// path must not serialize a spawn burst on the `.bad_tokens` lock).
    #[test]
    fn cleanup_does_not_rewrite_when_nothing_is_prunable() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        mark_bad(tmp.path(), "agent-1", "exhausted: hit your session limit").unwrap();
        let before = fs::metadata(dir.join(".bad_tokens"))
            .unwrap()
            .modified()
            .unwrap();
        let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.kept, 1);
        let after = fs::metadata(dir.join(".bad_tokens"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "file was rewritten with nothing to prune");
        // No lock directory was left behind either.
        assert!(!dir.join(".bad_tokens.lock").exists());
    }

    #[test]
    fn unblock_no_file_is_noop() {
        let tmp = make_pool();
        let out = unblock(tmp.path(), &["a".to_string()], false).unwrap();
        assert_eq!(out.removed, 0);
        assert_eq!(out.kept, 0);
        assert!(out.excluded.is_empty());
    }

    #[test]
    fn unblock_removes_auth_reason_by_default() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "a", "401 unauthorized").unwrap();
        mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();
        // Only "a" is targeted, so "b" is not excluded (it was never asked for).
        let out = unblock(tmp.path(), &["a".to_string()], false).unwrap();
        assert_eq!(out.removed, 1);
        assert_eq!(out.kept, 1);
        assert!(out.excluded.is_empty());
        assert!(!is_bad(tmp.path(), "a"));
        assert!(is_bad(tmp.path(), "b"));
    }

    /// #4212: a named account whose only entry is non-auth ("exhausted") is
    /// reported as `excluded` under the default scope — the caller fails
    /// instead of silently no-op'ing.
    #[test]
    fn unblock_default_scope_reports_excluded_non_auth() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "a", "401 unauthorized").unwrap();
        mark_bad(tmp.path(), "b", "exhausted: weekly-limit").unwrap();
        let out = unblock(tmp.path(), &["a".to_string(), "b".to_string()], false).unwrap();
        assert_eq!(out.removed, 1); // a's auth entry
        assert_eq!(out.kept, 1); // b's exhausted entry stays
        assert_eq!(out.excluded, vec!["b".to_string()]);
    }

    #[test]
    fn unblock_all_reasons_drops_non_auth_too() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();
        let out = unblock(tmp.path(), &["b".to_string()], true).unwrap();
        assert_eq!(out.removed, 1);
        assert_eq!(out.kept, 0);
        // --all-reasons never excludes anything.
        assert!(out.excluded.is_empty());
        assert!(!is_bad(tmp.path(), "b"));
    }

    #[test]
    fn unblock_ignores_unrelated_names() {
        let tmp = make_pool();
        mark_bad(tmp.path(), "a", "auth failure").unwrap();
        let out = unblock(tmp.path(), &["c".to_string()], false).unwrap();
        assert_eq!(out.removed, 0);
        assert_eq!(out.kept, 1);
        assert!(out.excluded.is_empty());
        assert!(is_bad(tmp.path(), "a"));
    }

    #[test]
    fn unblock_keeps_malformed_lines() {
        let tmp = make_pool();
        let dir = pool_dir(tmp.path());
        fs::write(dir.join(".bad_tokens"), "onlyoneword\n").unwrap();
        let out = unblock(tmp.path(), &["onlyoneword".to_string()], true).unwrap();
        assert_eq!(out.removed, 0);
        assert_eq!(out.kept, 1);
        assert!(out.excluded.is_empty());
    }

    #[test]
    fn auth_reason_regex_does_not_match_exhausted() {
        assert!(!auth_reason_regex().is_match("exhausted: weekly limit"));
        assert!(auth_reason_regex().is_match("401 Unauthorized"));
        assert!(auth_reason_regex().is_match("oauth token expired"));
    }
}
