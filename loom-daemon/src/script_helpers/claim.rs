//! Atomic file-based claiming for parallel agent orchestration — the native
//! port of `loom_tools.claim` (#4275), behind the `loom-claim` PATH shim.
//!
//! `mkdir` is the atomicity primitive (it succeeds or fails atomically on every
//! platform): a claim is the directory `.loom/claims/issue-<N>.lock`, and the
//! metadata lives in `claim.json` inside it.
//!
//! ## One parser, not two
//!
//! The **read** side of this format was already ported to Rust by #4272, as
//! [`crate::worktree_ops::has_valid_claim`], and that stays the single
//! programmatic "is this claim live?" predicate — `recover-orphans` calls it and
//! must not diverge from what this CLI writes. This module therefore imports the
//! shared expiry/abandonment primitives from there rather than forking a second
//! claim-format parser (the same rule #4272's builder was given). What it adds
//! is only the *writer* + operator CLI half that had no Rust equivalent:
//! `claim` / `extend` / `release` / `check` / `list` / `cleanup`.
//!
//! ## Exit codes (load-bearing — `builder-worktree.md` branches on them)
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | success |
//! | 1 | claim already exists (for `claim`), or a general error |
//! | 2 | invalid arguments |
//! | 3 | claim not found (for `extend` / `release` / `check`) |
//! | 4 | agent-ID mismatch (for `extend` / `release`) |

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::worktree_ops::{claim_is_abandoned, claim_is_expired};

/// Default claim TTL: 30 minutes.
pub const DEFAULT_TTL: i64 = 1800;

/// One claim's on-disk metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimInfo {
    pub issue: i64,
    pub agent_id: String,
    pub claimed_at: String,
    pub expires_at: String,
    pub ttl_seconds: i64,
}

impl ClaimInfo {
    #[must_use]
    pub fn from_value(data: &Value) -> Self {
        Self {
            issue: data.get("issue").and_then(Value::as_i64).unwrap_or(0),
            agent_id: data
                .get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            claimed_at: data
                .get("claimed_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            expires_at: data
                .get("expires_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ttl_seconds: data
                .get("ttl_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_TTL),
        }
    }

    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "issue": self.issue,
            "agent_id": self.agent_id,
            "claimed_at": self.claimed_at,
            "expires_at": self.expires_at,
            "ttl_seconds": self.ttl_seconds,
        })
    }
}

/// `.loom/claims/` for a repo root.
#[must_use]
pub fn claims_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("claims")
}

/// `.loom/claims/issue-<N>.lock` for an issue.
#[must_use]
pub fn claim_dir(repo_root: &Path, issue: i64) -> PathBuf {
    claims_dir(repo_root).join(format!("issue-{issue}.lock"))
}

fn claim_file(repo_root: &Path, issue: i64) -> PathBuf {
    claim_dir(repo_root, issue).join("claim.json")
}

/// The default agent ID when the caller does not supply one: `<hostname>-<pid>`.
#[must_use]
pub fn default_agent_id() -> String {
    format!("{}-{}", hostname(), std::process::id())
}

fn hostname() -> String {
    // `gethostname(2)` without a new dependency: the kernel exposes it, and
    // every fallback path still yields a stable-enough discriminator because the
    // pid is appended by the caller.
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string())
}

/// `now + ttl` in the shared `%Y-%m-%dT%H:%M:%SZ` shape.
#[must_use]
pub fn expiration_after(ttl: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(ttl))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn read_claim(repo_root: &Path, issue: i64) -> Option<ClaimInfo> {
    let data = super::read_json_file(&claim_file(repo_root, issue))?;
    if !data.is_object() {
        return None;
    }
    Some(ClaimInfo::from_value(&data))
}

fn remove_claim_dir(dir: &Path) {
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Claim an issue atomically.
///
/// Returns `0` on success and `1` when another agent holds a live claim. An
/// **expired** claim, an **abandoned** claim (stale heartbeat within TTL), and
/// an **incomplete** lock dir (no/corrupt `claim.json`) are all cleaned up and
/// the claim is retried once — bounded, so a pathological filesystem cannot
/// recurse forever (the Python original recursed unbounded).
pub fn claim_issue(repo_root: &Path, issue: i64, agent_id: Option<&str>, ttl: i64) -> i32 {
    let agent = agent_id
        .map(str::to_string)
        .unwrap_or_else(default_agent_id);

    // Two attempts: the first may find a stale lock to reap, the second must
    // either win the mkdir race or report a genuine live claim.
    for attempt in 0..2 {
        let dir = claim_dir(repo_root, issue);
        if std::fs::create_dir_all(claims_dir(repo_root)).is_err() {
            super::log_error(&format!(
                "Could not create claims directory {}",
                claims_dir(repo_root).display()
            ));
            return 1;
        }

        // `create_dir` (not `create_dir_all`) is the atomic test-and-set.
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                let info = ClaimInfo {
                    issue,
                    agent_id: agent.clone(),
                    claimed_at: super::now_iso(),
                    expires_at: expiration_after(ttl),
                    ttl_seconds: ttl,
                };
                if let Err(e) =
                    super::write_json_file(&claim_file(repo_root, issue), &info.to_value())
                {
                    super::log_error(&format!("Failed to write claim metadata: {e}"));
                    remove_claim_dir(&dir);
                    return 1;
                }
                super::log_success(&format!("Claimed issue #{issue}"));
                super::log_info(&format!("  Agent: {agent}"));
                super::log_info(&format!("  Expires: {}", info.expires_at));
                return 0;
            }
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                super::log_error(&format!("Failed to create claim directory: {e}"));
                return 1;
            }
            Err(_) => {}
        }

        // The lock dir exists. Decide whether it is reapable.
        let existing = read_claim(repo_root, issue);
        let reap_reason = match &existing {
            None => Some("Found incomplete claim, cleaning up...".to_string()),
            Some(c) if claim_is_expired(&c.expires_at) => {
                Some("Found expired claim, cleaning up...".to_string())
            }
            Some(c) if claim_is_abandoned(&c.claimed_at) => Some(format!(
                "Claim by {} appears abandoned (stale heartbeat), stealing claim...",
                c.agent_id
            )),
            Some(_) => None,
        };

        match reap_reason {
            Some(reason) if attempt == 0 => {
                super::log_warning(&reason);
                remove_claim_dir(&dir);
            }
            _ => {
                // A live claim (or a lock we already tried and failed to reap).
                if let Some(c) = existing {
                    super::log_error(&format!("Issue #{issue} already claimed"));
                    super::log_error(&format!("  By: {}", c.agent_id));
                    super::log_error(&format!("  Expires: {}", c.expires_at));
                } else {
                    super::log_error(&format!(
                        "Issue #{issue} lock directory could not be reclaimed"
                    ));
                }
                return 1;
            }
        }
    }
    1
}

/// Extend a claim's TTL. `3` when no claim exists, `4` on an agent mismatch.
pub fn extend_claim(repo_root: &Path, issue: i64, agent_id: &str, additional: i64) -> i32 {
    if !claim_dir(repo_root, issue).exists() {
        super::log_warning(&format!("No claim found for issue #{issue}"));
        return 3;
    }
    let Some(existing) = read_claim(repo_root, issue) else {
        super::log_warning(&format!("Incomplete claim found for issue #{issue}"));
        return 3;
    };
    if existing.agent_id != agent_id {
        super::log_error("Cannot extend: claim owned by different agent");
        super::log_error(&format!("  Owner: {}", existing.agent_id));
        super::log_error(&format!("  Requested by: {agent_id}"));
        return 4;
    }

    let updated = ClaimInfo {
        expires_at: expiration_after(additional),
        ttl_seconds: additional,
        ..existing
    };
    if let Err(e) = super::write_json_file(&claim_file(repo_root, issue), &updated.to_value()) {
        super::log_error(&format!("Failed to write claim metadata: {e}"));
        return 1;
    }
    super::log_success(&format!("Extended claim for issue #{issue}"));
    super::log_info(&format!("  New expiration: {}", updated.expires_at));
    super::log_info(&format!("  Extended by: {additional} seconds"));
    0
}

/// Release a claim. `3` when no claim exists, `4` on an agent mismatch (only
/// checked when `agent_id` is supplied — an unattributed release is allowed, as
/// in the Python original, so an operator can always break a stuck lock).
pub fn release_claim(repo_root: &Path, issue: i64, agent_id: Option<&str>) -> i32 {
    let dir = claim_dir(repo_root, issue);
    if !dir.exists() {
        super::log_warning(&format!("No claim found for issue #{issue}"));
        return 3;
    }
    if let Some(requested) = agent_id {
        if let Some(existing) = read_claim(repo_root, issue) {
            if existing.agent_id != requested {
                super::log_error("Cannot release: claim owned by different agent");
                super::log_error(&format!("  Owner: {}", existing.agent_id));
                super::log_error(&format!("  Requested by: {requested}"));
                return 4;
            }
        }
    }
    remove_claim_dir(&dir);
    super::log_success(&format!("Released claim for issue #{issue}"));
    0
}

/// Print a claim's metadata. `0` when live, `3` when absent, incomplete, or
/// expired (the expired case still prints the record, for diagnosis).
pub fn check_claim(repo_root: &Path, issue: i64) -> i32 {
    if !claim_dir(repo_root, issue).exists() {
        super::log_info(&format!("Issue #{issue} is not claimed"));
        return 3;
    }
    let Some(existing) = read_claim(repo_root, issue) else {
        super::log_warning(&format!("Incomplete claim found for issue #{issue}"));
        return 3;
    };
    let rendered = serde_json::to_string_pretty(&existing.to_value())
        .unwrap_or_else(|_| existing.to_value().to_string());
    if claim_is_expired(&existing.expires_at) {
        super::log_warning(&format!("Issue #{issue} has an expired claim"));
        println!("{rendered}");
        return 3;
    }
    super::log_success(&format!("Issue #{issue} is claimed"));
    println!("{rendered}");
    0
}

/// Every `issue-*.lock` directory holding a parseable `claim.json`, sorted by
/// directory name (matching the Python's `sorted(glob(...))`).
fn iter_claims(repo_root: &Path) -> Vec<(PathBuf, Option<ClaimInfo>)> {
    let dir = claims_dir(repo_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|n| n.starts_with("issue-") && n.ends_with(".lock"))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|p| {
            let info = super::read_json_file(&p.join("claim.json"))
                .filter(Value::is_object)
                .map(|v| ClaimInfo::from_value(&v));
            (p, info)
        })
        .collect()
}

/// List all claims. Always exit `0`.
pub fn list_claims(repo_root: &Path) -> i32 {
    let _ = std::fs::create_dir_all(claims_dir(repo_root));
    println!("Active claims:\n");
    let mut count = 0usize;
    for (_, info) in iter_claims(repo_root) {
        let Some(c) = info else { continue };
        if claim_is_expired(&c.expires_at) {
            println!("  Issue #{} (EXPIRED)", c.issue);
        } else {
            println!("  Issue #{} - Agent: {}, Expires: {}", c.issue, c.agent_id, c.expires_at);
        }
        count += 1;
    }
    if count == 0 {
        println!("  (none)");
    }
    println!("\nTotal: {count} claim(s)");
    0
}

/// Remove every expired claim (and every incomplete lock dir). Always exit `0`.
pub fn cleanup_claims(repo_root: &Path) -> i32 {
    let _ = std::fs::create_dir_all(claims_dir(repo_root));
    super::log_info("Cleaning up expired claims...");
    let mut cleaned = 0usize;
    for (path, info) in iter_claims(repo_root) {
        match info {
            Some(c) if claim_is_expired(&c.expires_at) => {
                remove_claim_dir(&path);
                super::log_success(&format!("Removed expired claim for issue #{}", c.issue));
                cleaned += 1;
            }
            // No parseable claim file — an incomplete lock, always reapable.
            None => {
                remove_claim_dir(&path);
                cleaned += 1;
            }
            Some(_) => {}
        }
    }
    if cleaned == 0 {
        super::log_info("No expired claims found");
    } else {
        println!("\nCleaned up {cleaned} expired claim(s)");
    }
    0
}

// --------------------------------------------------------------------------
// CLI
// --------------------------------------------------------------------------

const USAGE: &str = "\
usage: loom-claim <command> [args...]

Commands:
  claim <issue-number> [agent-id] [ttl-seconds]
      Atomically claim an issue. Default TTL is 30 minutes (1800 seconds).
      Exits 0 on success, 1 if already claimed.

  extend <issue-number> <agent-id> [additional-seconds]
      Extend an existing claim's TTL. Agent must own the claim.
      Exits 0 on success, 3 if no claim exists, 4 if agent mismatch.

  release <issue-number> [agent-id]
      Release a claim. If agent-id is provided, verifies ownership.
      Exits 0 on success, 3 if no claim exists, 4 if agent mismatch.

  check <issue-number>
      Check if an issue is claimed and print claim metadata.
      Exits 0 if claimed, 3 if not claimed or expired.

  list      List all active claims.
  cleanup   Remove all expired claims.

Exit codes:
  0 - Success
  1 - Claim already exists (for claim), or general error
  2 - Invalid arguments
  3 - Claim not found (for release/check)
  4 - Agent ID mismatch (for release)

Examples:
  loom-claim claim 123                    # Claim with the default agent ID
  loom-claim claim 123 builder-1 3600     # Claim for 1 hour
  loom-claim extend 123 builder-1 7200    # Extend by 2 hours
  loom-claim release 123 builder-1        # Release with ownership check
  loom-claim check 123                    # Check claim status
  loom-claim list                         # List all claims
  loom-claim cleanup                      # Clean expired claims";

/// Run the claim CLI against an explicit repo root. Split out of [`run`] so the
/// whole command surface is testable without touching the process cwd.
#[must_use]
pub fn run_in(repo_root: &Path, command: Option<&str>, args: &[String]) -> i32 {
    let Some(cmd) = command else {
        println!("{USAGE}");
        return 0;
    };
    if matches!(cmd, "-h" | "--help" | "help") {
        println!("{USAGE}");
        return 0;
    }

    // Every command except list/cleanup takes an issue number first.
    let issue = |idx: usize| -> Result<i64, i32> {
        let Some(raw) = args.get(idx) else {
            super::log_error("Issue number required");
            return Err(2);
        };
        raw.parse::<i64>().map_err(|_| {
            super::log_error(&format!("Invalid issue number: {raw}"));
            2
        })
    };
    let number = |idx: usize, fallback: i64| -> i64 {
        args.get(idx)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(fallback)
    };

    match cmd {
        "claim" => match issue(0) {
            Ok(n) => {
                claim_issue(repo_root, n, args.get(1).map(String::as_str), number(2, DEFAULT_TTL))
            }
            Err(code) => code,
        },
        "extend" => {
            if args.len() < 2 {
                super::log_error("Issue number and agent ID required for extend");
                return 2;
            }
            match issue(0) {
                Ok(n) => extend_claim(repo_root, n, &args[1], number(2, DEFAULT_TTL)),
                Err(code) => code,
            }
        }
        "release" => match issue(0) {
            Ok(n) => release_claim(repo_root, n, args.get(1).map(String::as_str)),
            Err(code) => code,
        },
        "check" => match issue(0) {
            Ok(n) => check_claim(repo_root, n),
            Err(code) => code,
        },
        "list" => list_claims(repo_root),
        "cleanup" => cleanup_claims(repo_root),
        other => {
            super::log_error(&format!("Unknown command '{other}'"));
            println!("{USAGE}");
            2
        }
    }
}

/// CLI entry point (`loom-daemon claim ...`, behind the `loom-claim` shim).
///
/// Resolves the repo root by walking up from `cwd`; exits `2` outside a Loom
/// repository, matching the Python's `FileNotFoundError` branch.
#[must_use]
pub fn run(cwd: &Path, command: Option<&str>, args: &[String]) -> i32 {
    // `--help` must work outside a repo (the Python parsed args before
    // resolving the root).
    if command.is_none() || matches!(command, Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return 0;
    }
    let Some(repo_root) = super::find_repo_root(cwd) else {
        super::log_error("Not in a git repository with .loom directory");
        return 2;
    };
    run_in(&repo_root, command, args)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn arg(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn ts(offset_secs: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::seconds(offset_secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn write_raw_claim(root: &Path, issue: i64, claimed_at: &str, expires_at: &str, agent: &str) {
        let dir = claim_dir(root, issue);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("claim.json"),
            json!({
                "issue": issue,
                "agent_id": agent,
                "claimed_at": claimed_at,
                "expires_at": expires_at,
                "ttl_seconds": DEFAULT_TTL,
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn claim_creates_the_lock_and_metadata() {
        let dir = tempdir().unwrap();
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 1800), 0);
        let info = read_claim(dir.path(), 42).unwrap();
        assert_eq!(info.issue, 42);
        assert_eq!(info.agent_id, "builder-1");
        assert_eq!(info.ttl_seconds, 1800);
        assert_eq!(info.claimed_at.len(), 20);
        assert!(info.expires_at > info.claimed_at);
    }

    #[test]
    fn a_second_agent_cannot_take_a_live_claim() {
        let dir = tempdir().unwrap();
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 1800), 0);
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-2"), 1800), 1);
        assert_eq!(read_claim(dir.path(), 42).unwrap().agent_id, "builder-1");
    }

    #[test]
    fn an_expired_claim_is_reaped_and_reclaimed() {
        let dir = tempdir().unwrap();
        write_raw_claim(dir.path(), 42, &ts(-3600), &ts(-1800), "ghost");
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-2"), 1800), 0);
        assert_eq!(read_claim(dir.path(), 42).unwrap().agent_id, "builder-2");
    }

    /// A claim still inside its TTL but with a >10-minute-old heartbeat is
    /// stealable — the shared abandonment threshold #4272 landed.
    #[test]
    fn an_abandoned_claim_within_ttl_is_stolen() {
        let dir = tempdir().unwrap();
        write_raw_claim(dir.path(), 42, &ts(-1200), &ts(600), "ghost");
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-2"), 1800), 0);
        assert_eq!(read_claim(dir.path(), 42).unwrap().agent_id, "builder-2");
    }

    #[test]
    fn an_incomplete_lock_dir_is_reaped() {
        let dir = tempdir().unwrap();
        // Lock dir with no claim.json at all…
        std::fs::create_dir_all(claim_dir(dir.path(), 42)).unwrap();
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 1800), 0);

        // …and with a corrupt one.
        std::fs::write(claim_file(dir.path(), 43), "not json").ok();
        std::fs::create_dir_all(claim_dir(dir.path(), 43)).unwrap();
        std::fs::write(claim_file(dir.path(), 43), "not json").unwrap();
        assert_eq!(claim_issue(dir.path(), 43, Some("builder-1"), 1800), 0);
    }

    /// The writer here and the `has_valid_claim` reader #4272 landed must agree:
    /// a freshly written claim is live, an expired one is not.
    #[test]
    fn the_writer_and_the_4272_reader_agree() {
        let dir = tempdir().unwrap();
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 1800), 0);
        assert!(crate::worktree_ops::has_valid_claim(dir.path(), 42));

        write_raw_claim(dir.path(), 43, &ts(-3600), &ts(-1800), "ghost");
        assert!(!crate::worktree_ops::has_valid_claim(dir.path(), 43));
    }

    #[test]
    fn extend_requires_ownership_and_pushes_the_expiry_out() {
        let dir = tempdir().unwrap();
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 60), 0);
        let before = read_claim(dir.path(), 42).unwrap();

        assert_eq!(extend_claim(dir.path(), 42, "builder-2", 7200), 4);
        assert_eq!(extend_claim(dir.path(), 42, "builder-1", 7200), 0);

        let after = read_claim(dir.path(), 42).unwrap();
        assert!(after.expires_at > before.expires_at);
        assert_eq!(after.ttl_seconds, 7200);
        // `claimed_at` is preserved — extend must not reset the heartbeat clock.
        assert_eq!(after.claimed_at, before.claimed_at);
    }

    #[test]
    fn extend_on_a_missing_or_incomplete_claim_exits_3() {
        let dir = tempdir().unwrap();
        assert_eq!(extend_claim(dir.path(), 42, "builder-1", 1800), 3);
        std::fs::create_dir_all(claim_dir(dir.path(), 42)).unwrap();
        assert_eq!(extend_claim(dir.path(), 42, "builder-1", 1800), 3);
    }

    #[test]
    fn release_checks_ownership_only_when_an_agent_is_named() {
        let dir = tempdir().unwrap();
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 1800), 0);
        assert_eq!(release_claim(dir.path(), 42, Some("builder-2")), 4);
        assert_eq!(release_claim(dir.path(), 42, Some("builder-1")), 0);
        assert!(!claim_dir(dir.path(), 42).exists());
        assert_eq!(release_claim(dir.path(), 42, None), 3);

        // An unattributed release breaks any lock (operator escape hatch).
        assert_eq!(claim_issue(dir.path(), 43, Some("builder-1"), 1800), 0);
        assert_eq!(release_claim(dir.path(), 43, None), 0);
    }

    #[test]
    fn check_reports_live_absent_and_expired_claims() {
        let dir = tempdir().unwrap();
        assert_eq!(check_claim(dir.path(), 42), 3);
        assert_eq!(claim_issue(dir.path(), 42, Some("builder-1"), 1800), 0);
        assert_eq!(check_claim(dir.path(), 42), 0);

        write_raw_claim(dir.path(), 43, &ts(-3600), &ts(-1800), "ghost");
        assert_eq!(check_claim(dir.path(), 43), 3);
    }

    #[test]
    fn cleanup_removes_expired_and_incomplete_claims_only() {
        let dir = tempdir().unwrap();
        assert_eq!(claim_issue(dir.path(), 1, Some("live"), 1800), 0);
        write_raw_claim(dir.path(), 2, &ts(-3600), &ts(-1800), "expired");
        std::fs::create_dir_all(claim_dir(dir.path(), 3)).unwrap();

        assert_eq!(cleanup_claims(dir.path()), 0);
        assert!(claim_dir(dir.path(), 1).exists(), "live claim must survive");
        assert!(!claim_dir(dir.path(), 2).exists());
        assert!(!claim_dir(dir.path(), 3).exists());
    }

    #[test]
    fn list_never_fails_even_with_no_claims_dir() {
        let dir = tempdir().unwrap();
        assert_eq!(list_claims(dir.path()), 0);
        assert_eq!(claim_issue(dir.path(), 7, Some("a"), 1800), 0);
        write_raw_claim(dir.path(), 8, &ts(-3600), &ts(-1800), "ghost");
        assert_eq!(list_claims(dir.path()), 0);
    }

    // ===== CLI argument handling / exit codes =====

    #[test]
    fn cli_maps_commands_to_the_documented_exit_codes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        assert_eq!(run_in(root, Some("claim"), &arg(&["42", "builder-1"])), 0);
        assert_eq!(run_in(root, Some("claim"), &arg(&["42", "builder-2"])), 1);
        assert_eq!(run_in(root, Some("check"), &arg(&["42"])), 0);
        assert_eq!(run_in(root, Some("extend"), &arg(&["42", "builder-2"])), 4);
        assert_eq!(run_in(root, Some("extend"), &arg(&["42", "builder-1"])), 0);
        assert_eq!(run_in(root, Some("release"), &arg(&["42", "builder-2"])), 4);
        assert_eq!(run_in(root, Some("release"), &arg(&["42", "builder-1"])), 0);
        assert_eq!(run_in(root, Some("release"), &arg(&["42"])), 3);
        assert_eq!(run_in(root, Some("list"), &[]), 0);
        assert_eq!(run_in(root, Some("cleanup"), &[]), 0);
    }

    #[test]
    fn cli_rejects_bad_arguments_with_exit_2() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        assert_eq!(run_in(root, Some("claim"), &[]), 2);
        assert_eq!(run_in(root, Some("claim"), &arg(&["not-a-number"])), 2);
        assert_eq!(run_in(root, Some("extend"), &arg(&["42"])), 2);
        assert_eq!(run_in(root, Some("check"), &[]), 2);
        assert_eq!(run_in(root, Some("frobnicate"), &[]), 2);
    }

    #[test]
    fn cli_help_and_no_command_print_usage_and_exit_0() {
        let dir = tempdir().unwrap();
        assert_eq!(run_in(dir.path(), None, &[]), 0);
        assert_eq!(run_in(dir.path(), Some("--help"), &[]), 0);
        assert_eq!(run_in(dir.path(), Some("help"), &[]), 0);
        // `run` (which resolves a repo root) must still answer --help outside one.
        assert_eq!(run(dir.path(), Some("--help"), &[]), 0);
    }

    #[test]
    fn cli_ttl_argument_is_honored_and_bad_values_fall_back_to_the_default() {
        let dir = tempdir().unwrap();
        assert_eq!(run_in(dir.path(), Some("claim"), &arg(&["1", "a", "3600"])), 0);
        assert_eq!(read_claim(dir.path(), 1).unwrap().ttl_seconds, 3600);

        assert_eq!(run_in(dir.path(), Some("claim"), &arg(&["2", "a", "junk"])), 0);
        assert_eq!(read_claim(dir.path(), 2).unwrap().ttl_seconds, DEFAULT_TTL);
    }

    #[test]
    fn default_agent_id_is_host_plus_pid() {
        let id = default_agent_id();
        assert!(id.contains('-'));
        assert!(id.ends_with(&std::process::id().to_string()));
    }

    #[test]
    fn run_outside_a_loom_repo_exits_2() {
        let dir = tempdir().unwrap();
        if super::super::find_repo_root(dir.path()).is_some() {
            return; // temp root inside a repo on this host — not a false negative
        }
        assert_eq!(run(dir.path(), Some("list"), &[]), 2);
    }
}
