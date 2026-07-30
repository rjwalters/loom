//! The claim-liveness predicate over `.loom/claims/issue-<N>.lock/claim.json`
//! — originally the read-only half of `loom_tools.claim` ported by #4272.
//!
//! Issue #4275 (family 5) ported the *writer* half too, as
//! [`crate::script_helpers::claim`]. Rather than fork a second parser for the
//! same on-disk format, that CLI reuses this module's expiry/abandonment
//! primitives (re-exported from [`crate::worktree_ops`] as `claim_is_expired` /
//! `claim_is_abandoned`), so `recover-orphans`' notion of a live claim and what
//! `loom-claim` writes cannot drift apart.

use std::path::Path;

use chrono::Utc;

/// Claims are considered abandoned (heartbeat gone stale) after this many
/// seconds since `claimed_at`, even if the TTL-based `expires_at` hasn't
/// lapsed yet. Mirrors `loom_tools.claim.NO_PROGRESS_FILE_THRESHOLD`.
const NO_PROGRESS_FILE_THRESHOLD_SECS: i64 = 600;

#[derive(Debug, serde::Deserialize)]
struct ClaimInfoRaw {
    #[serde(default)]
    claimed_at: String,
    #[serde(default)]
    expires_at: String,
}

/// Whether `issue_number` has a valid (non-expired, non-abandoned) file-based
/// claim under `repo_root/.loom/claims/issue-<N>.lock/claim.json`.
///
/// Mirrors `loom_tools.claim.has_valid_claim` exactly:
/// - Missing claim dir/file → `false`.
/// - Unparseable JSON → `false`.
/// - `expires_at` in the past (lexicographic ISO-8601 `Z` comparison, same as
///   the Python original) → `false`.
/// - `claimed_at` older than [`NO_PROGRESS_FILE_THRESHOLD_SECS`] → `false`
///   ("abandoned", stale heartbeat even within TTL).
/// - Otherwise → `true`.
#[must_use]
pub fn has_valid_claim(repo_root: &Path, issue_number: u32) -> bool {
    let claim_file = repo_root
        .join(".loom")
        .join("claims")
        .join(format!("issue-{issue_number}.lock"))
        .join("claim.json");
    let Ok(text) = std::fs::read_to_string(&claim_file) else {
        return false;
    };
    let Ok(claim) = serde_json::from_str::<ClaimInfoRaw>(&text) else {
        return false;
    };

    if is_expired(&claim.expires_at) {
        return false;
    }
    if is_abandoned(&claim.claimed_at) {
        return false;
    }
    true
}

/// Lexicographic ISO-8601 `%Y-%m-%dT%H:%M:%SZ` comparison against "now",
/// exactly like the Python original (`current_iso > expiration_iso`). The fixed
/// width and literal `Z` are what make the string comparison sound, which is
/// why every writer stamps timestamps through
/// [`crate::script_helpers::now_iso`].
#[must_use]
pub fn is_expired(expires_at: &str) -> bool {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    now.as_str() > expires_at
}

/// Whether `claimed_at` is older than [`NO_PROGRESS_FILE_THRESHOLD_SECS`] — a
/// claim whose owner's heartbeat has gone stale even though its TTL has not
/// lapsed. Unparseable input is treated as NOT abandoned (fail-safe: absent
/// evidence never orphans a claim, per #3651).
#[must_use]
pub fn is_abandoned(claimed_at: &str) -> bool {
    // `%Y-%m-%dT%H:%M:%SZ` carries no UTC-offset item (`Z` is a literal, not
    // `%z`), so this must be parsed as a naive datetime and then assumed UTC
    // — `DateTime::parse_from_str` requires an offset item and would (silently,
    // via the caller's `Ok(..) else return false`) treat every claim as
    // "unparseable" here, which defeats the abandonment check entirely.
    let Ok(claimed) = chrono::NaiveDateTime::parse_from_str(claimed_at, "%Y-%m-%dT%H:%M:%SZ")
    else {
        return false;
    };
    let claimed = claimed.and_utc();
    let age = Utc::now().signed_duration_since(claimed);
    age.num_seconds() > NO_PROGRESS_FILE_THRESHOLD_SECS
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_claim(repo_root: &Path, issue: u32, claimed_at: &str, expires_at: &str) {
        let dir = repo_root
            .join(".loom")
            .join("claims")
            .join(format!("issue-{issue}.lock"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("claim.json"),
            serde_json::json!({
                "issue": issue,
                "agent_id": "test-agent",
                "claimed_at": claimed_at,
                "expires_at": expires_at,
                "ttl_seconds": 1800,
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn no_claim_dir_is_invalid() {
        let dir = tempdir().unwrap();
        assert!(!has_valid_claim(dir.path(), 42));
    }

    #[test]
    fn fresh_claim_is_valid() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let claimed_at = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let expires_at = (now + chrono::Duration::seconds(1800))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        write_claim(dir.path(), 42, &claimed_at, &expires_at);
        assert!(has_valid_claim(dir.path(), 42));
    }

    #[test]
    fn expired_claim_is_invalid() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let claimed_at = (now - chrono::Duration::seconds(3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let expires_at = (now - chrono::Duration::seconds(1800))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        write_claim(dir.path(), 42, &claimed_at, &expires_at);
        assert!(!has_valid_claim(dir.path(), 42));
    }

    #[test]
    fn abandoned_claim_within_ttl_is_invalid() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        // claimed 20 minutes ago (past the 10-minute abandonment threshold)
        // but the TTL (30 min) has not expired yet.
        let claimed_at = (now - chrono::Duration::seconds(1200))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let expires_at = (now + chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        write_claim(dir.path(), 42, &claimed_at, &expires_at);
        assert!(!has_valid_claim(dir.path(), 42));
    }

    #[test]
    fn corrupt_claim_json_is_invalid() {
        let dir = tempdir().unwrap();
        let claim_dir = dir
            .path()
            .join(".loom")
            .join("claims")
            .join("issue-42.lock");
        std::fs::create_dir_all(&claim_dir).unwrap();
        std::fs::write(claim_dir.join("claim.json"), "not json").unwrap();
        assert!(!has_valid_claim(dir.path(), 42));
    }
}
