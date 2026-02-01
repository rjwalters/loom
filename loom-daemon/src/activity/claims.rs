//! Issue claim registry for reliable work distribution.
//!
//! Extracted from `db.rs` (Issue #1159) — provides claim/release operations,
//! heartbeat-based liveness, stale claim detection, and crash recovery.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::models::{ClaimResult, ClaimType, ClaimsSummary, IssueClaim};

// ========================================================================
// Claim Operations
// ========================================================================

/// Attempt to claim an issue or PR for a terminal.
///
/// This uses INSERT OR IGNORE with a unique constraint to prevent race conditions.
/// If the claim already exists, it checks if the claim is stale (based on TTL)
/// and can be reclaimed.
///
/// # Arguments
/// * `number` - The issue or PR number to claim
/// * `claim_type` - Whether this is an issue or PR claim
/// * `terminal_id` - The terminal claiming the work
/// * `label` - Optional GitHub label associated with this claim
/// * `agent_role` - Optional agent role for tracking
/// * `stale_threshold_secs` - How many seconds before a claim is considered stale (default: 3600 = 1 hour)
pub(super) fn claim_issue(
    conn: &Connection,
    number: i32,
    claim_type: ClaimType,
    terminal_id: &str,
    label: Option<&str>,
    agent_role: Option<&str>,
    stale_threshold_secs: Option<i64>,
) -> Result<ClaimResult> {
    let now = Utc::now();
    let claim_type_str = claim_type.as_str();
    let stale_threshold = stale_threshold_secs.unwrap_or(3600); // Default 1 hour

    // First, try to insert a new claim
    let insert_result = conn.execute(
        r"
        INSERT OR IGNORE INTO issue_claims (number, claim_type, terminal_id, claimed_at, last_heartbeat, label, agent_role)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            number,
            claim_type_str,
            terminal_id,
            now.to_rfc3339(),
            now.to_rfc3339(),
            label,
            agent_role,
        ],
    )?;

    // If insert succeeded (rows_affected == 1), we got the claim
    if insert_result == 1 {
        let claim_id = conn.last_insert_rowid();
        return Ok(ClaimResult::Success { claim_id });
    }

    // Insert failed due to unique constraint - check existing claim
    let existing: (i64, String, String) = conn.query_row(
        r"
        SELECT id, terminal_id, last_heartbeat
        FROM issue_claims
        WHERE number = ?1 AND claim_type = ?2
        ",
        params![number, claim_type_str],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    let (existing_id, existing_terminal, heartbeat_str) = existing;
    let heartbeat = DateTime::parse_from_rfc3339(&heartbeat_str)
        .map_err(|e| anyhow::anyhow!("Invalid timestamp: {e}"))?
        .with_timezone(&Utc);

    // Check if the existing claim is stale
    let age_secs = (now - heartbeat).num_seconds();

    if age_secs > stale_threshold {
        // Claim is stale - reclaim it
        conn.execute(
            r"
            UPDATE issue_claims
            SET terminal_id = ?1, claimed_at = ?2, last_heartbeat = ?3, label = ?4, agent_role = ?5
            WHERE id = ?6
            ",
            params![
                terminal_id,
                now.to_rfc3339(),
                now.to_rfc3339(),
                label,
                agent_role,
                existing_id,
            ],
        )?;

        Ok(ClaimResult::Reclaimed {
            claim_id: existing_id,
            previous_terminal: existing_terminal,
        })
    } else {
        // Claim is still active - return the current owner
        let claimed_at_str: String = conn.query_row(
            r"SELECT claimed_at FROM issue_claims WHERE id = ?1",
            params![existing_id],
            |row| row.get(0),
        )?;
        let claimed_at = DateTime::parse_from_rfc3339(&claimed_at_str)
            .map_err(|e| anyhow::anyhow!("Invalid timestamp: {e}"))?
            .with_timezone(&Utc);

        Ok(ClaimResult::AlreadyClaimed {
            terminal_id: existing_terminal,
            claimed_at,
        })
    }
}

/// Release a claim on an issue or PR.
///
/// This removes the claim from the registry. Can be called when work is complete
/// or if the agent decides to abandon the work.
///
/// # Arguments
/// * `number` - The issue or PR number to release
/// * `claim_type` - Whether this is an issue or PR claim
/// * `terminal_id` - Optional: only release if owned by this terminal
pub(super) fn release_claim(
    conn: &Connection,
    number: i32,
    claim_type: ClaimType,
    terminal_id: Option<&str>,
) -> Result<bool> {
    let claim_type_str = claim_type.as_str();

    let rows = if let Some(tid) = terminal_id {
        conn.execute(
            r"DELETE FROM issue_claims WHERE number = ?1 AND claim_type = ?2 AND terminal_id = ?3",
            params![number, claim_type_str, tid],
        )?
    } else {
        conn.execute(
            r"DELETE FROM issue_claims WHERE number = ?1 AND claim_type = ?2",
            params![number, claim_type_str],
        )?
    };

    Ok(rows > 0)
}

/// Update the heartbeat for an active claim.
///
/// This should be called periodically by agents to indicate they are still
/// working on a claimed issue. Claims without heartbeat updates will be
/// considered stale after the TTL threshold.
pub(super) fn heartbeat_claim(
    conn: &Connection,
    number: i32,
    claim_type: ClaimType,
    terminal_id: &str,
) -> Result<bool> {
    let now = Utc::now();
    let claim_type_str = claim_type.as_str();

    let rows = conn.execute(
        r"
        UPDATE issue_claims
        SET last_heartbeat = ?1
        WHERE number = ?2 AND claim_type = ?3 AND terminal_id = ?4
        ",
        params![now.to_rfc3339(), number, claim_type_str, terminal_id],
    )?;

    Ok(rows > 0)
}

// ========================================================================
// Claim Queries
// ========================================================================

/// Get a specific claim if it exists.
pub(super) fn get_claim(
    conn: &Connection,
    number: i32,
    claim_type: ClaimType,
) -> Result<Option<IssueClaim>> {
    let claim_type_str = claim_type.as_str();

    let result = conn.query_row(
        r"
        SELECT id, number, claim_type, terminal_id, claimed_at, last_heartbeat, label, agent_role
        FROM issue_claims
        WHERE number = ?1 AND claim_type = ?2
        ",
        params![number, claim_type_str],
        |row| {
            let claimed_at_str: String = row.get(4)?;
            let heartbeat_str: String = row.get(5)?;

            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                claimed_at_str,
                heartbeat_str,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        },
    );

    match result {
        Ok((id, num, ct_str, tid, claimed_str, heartbeat_str, label, role)) => {
            let claimed_at = DateTime::parse_from_rfc3339(&claimed_str)
                .map_err(|e| anyhow::anyhow!("Invalid timestamp: {e}"))?
                .with_timezone(&Utc);
            let last_heartbeat = DateTime::parse_from_rfc3339(&heartbeat_str)
                .map_err(|e| anyhow::anyhow!("Invalid timestamp: {e}"))?
                .with_timezone(&Utc);

            Ok(Some(IssueClaim {
                id: Some(id),
                number: num,
                claim_type: ClaimType::from_str(&ct_str).unwrap_or(ClaimType::Issue),
                terminal_id: tid,
                claimed_at,
                last_heartbeat,
                label,
                agent_role: role,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Get all active claims.
pub(super) fn get_all_claims(conn: &Connection) -> Result<Vec<IssueClaim>> {
    let mut stmt = conn.prepare(
        r"
        SELECT id, number, claim_type, terminal_id, claimed_at, last_heartbeat, label, agent_role
        FROM issue_claims
        ORDER BY claimed_at
        ",
    )?;

    let claims = stmt.query_map([], |row| {
        let claimed_at_str: String = row.get(4)?;
        let heartbeat_str: String = row.get(5)?;
        let ct_str: String = row.get(2)?;

        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i32>(1)?,
            ct_str,
            row.get::<_, String>(3)?,
            claimed_at_str,
            heartbeat_str,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;

    let mut result = Vec::new();
    for claim in claims {
        let (id, num, ct_str, tid, claimed_str, heartbeat_str, label, role) = claim?;

        let claimed_at = DateTime::parse_from_rfc3339(&claimed_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
            .with_timezone(&Utc);
        let last_heartbeat = DateTime::parse_from_rfc3339(&heartbeat_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
            .with_timezone(&Utc);

        result.push(IssueClaim {
            id: Some(id),
            number: num,
            claim_type: ClaimType::from_str(&ct_str).unwrap_or(ClaimType::Issue),
            terminal_id: tid,
            claimed_at,
            last_heartbeat,
            label,
            agent_role: role,
        });
    }

    Ok(result)
}

/// Get claims for a specific terminal.
pub(super) fn get_claims_by_terminal(
    conn: &Connection,
    terminal_id: &str,
) -> Result<Vec<IssueClaim>> {
    let mut stmt = conn.prepare(
        r"
        SELECT id, number, claim_type, terminal_id, claimed_at, last_heartbeat, label, agent_role
        FROM issue_claims
        WHERE terminal_id = ?1
        ORDER BY claimed_at
        ",
    )?;

    let claims = stmt.query_map(params![terminal_id], |row| {
        let claimed_at_str: String = row.get(4)?;
        let heartbeat_str: String = row.get(5)?;
        let ct_str: String = row.get(2)?;

        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i32>(1)?,
            ct_str,
            row.get::<_, String>(3)?,
            claimed_at_str,
            heartbeat_str,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;

    let mut result = Vec::new();
    for claim in claims {
        let (id, num, ct_str, tid, claimed_str, heartbeat_str, label, role) = claim?;

        let claimed_at = DateTime::parse_from_rfc3339(&claimed_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
            .with_timezone(&Utc);
        let last_heartbeat = DateTime::parse_from_rfc3339(&heartbeat_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
            .with_timezone(&Utc);

        result.push(IssueClaim {
            id: Some(id),
            number: num,
            claim_type: ClaimType::from_str(&ct_str).unwrap_or(ClaimType::Issue),
            terminal_id: tid,
            claimed_at,
            last_heartbeat,
            label,
            agent_role: role,
        });
    }

    Ok(result)
}

/// Get stale claims (those without heartbeat for longer than threshold).
///
/// # Arguments
/// * `stale_threshold_secs` - How many seconds before a claim is considered stale
pub(super) fn get_stale_claims(
    conn: &Connection,
    stale_threshold_secs: i64,
) -> Result<Vec<IssueClaim>> {
    let threshold_time = Utc::now() - chrono::Duration::seconds(stale_threshold_secs);

    let mut stmt = conn.prepare(
        r"
        SELECT id, number, claim_type, terminal_id, claimed_at, last_heartbeat, label, agent_role
        FROM issue_claims
        WHERE last_heartbeat < ?1
        ORDER BY last_heartbeat
        ",
    )?;

    let claims = stmt.query_map(params![threshold_time.to_rfc3339()], |row| {
        let claimed_at_str: String = row.get(4)?;
        let heartbeat_str: String = row.get(5)?;
        let ct_str: String = row.get(2)?;

        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i32>(1)?,
            ct_str,
            row.get::<_, String>(3)?,
            claimed_at_str,
            heartbeat_str,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;

    let mut result = Vec::new();
    for claim in claims {
        let (id, num, ct_str, tid, claimed_str, heartbeat_str, label, role) = claim?;

        let claimed_at = DateTime::parse_from_rfc3339(&claimed_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
            .with_timezone(&Utc);
        let last_heartbeat = DateTime::parse_from_rfc3339(&heartbeat_str)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
            .with_timezone(&Utc);

        result.push(IssueClaim {
            id: Some(id),
            number: num,
            claim_type: ClaimType::from_str(&ct_str).unwrap_or(ClaimType::Issue),
            terminal_id: tid,
            claimed_at,
            last_heartbeat,
            label,
            agent_role: role,
        });
    }

    Ok(result)
}

// ========================================================================
// Claim Lifecycle
// ========================================================================

/// Release all stale claims and return the count of claims released.
///
/// This is used during daemon startup for crash recovery.
pub(super) fn release_stale_claims(conn: &Connection, stale_threshold_secs: i64) -> Result<usize> {
    let threshold_time = Utc::now() - chrono::Duration::seconds(stale_threshold_secs);

    let rows = conn.execute(
        r"DELETE FROM issue_claims WHERE last_heartbeat < ?1",
        params![threshold_time.to_rfc3339()],
    )?;

    Ok(rows)
}

/// Release all claims for a specific terminal.
///
/// This is used when a terminal is destroyed or restarted.
pub(super) fn release_terminal_claims(conn: &Connection, terminal_id: &str) -> Result<usize> {
    let rows =
        conn.execute(r"DELETE FROM issue_claims WHERE terminal_id = ?1", params![terminal_id])?;

    Ok(rows)
}

/// Get a summary of all claims for visibility.
pub(super) fn get_claims_summary(
    conn: &Connection,
    stale_threshold_secs: i64,
) -> Result<ClaimsSummary> {
    let all_claims = get_all_claims(conn)?;
    let stale_claims = get_stale_claims(conn, stale_threshold_secs)?;

    let mut by_type = std::collections::HashMap::new();
    let mut by_terminal: std::collections::HashMap<String, Vec<i32>> =
        std::collections::HashMap::new();

    for claim in &all_claims {
        *by_type
            .entry(claim.claim_type.as_str().to_string())
            .or_insert(0) += 1;
        by_terminal
            .entry(claim.terminal_id.clone())
            .or_default()
            .push(claim.number);
    }

    #[allow(clippy::cast_possible_wrap)]
    Ok(ClaimsSummary {
        total_claims: all_claims.len() as i64,
        by_type,
        by_terminal,
        stale_claims,
    })
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::redundant_closure_for_method_calls
)]
mod tests {
    use super::super::db::ActivityDb;
    use super::super::models::{ClaimResult, ClaimType};
    use super::*;
    use rusqlite::params;
    use tempfile::NamedTempFile;

    #[test]
    fn test_claim_issue_success() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        let result = db.claim_issue(
            123,
            ClaimType::Issue,
            "terminal-1",
            Some("loom:building"),
            Some("builder"),
            None,
        )?;

        match result {
            ClaimResult::Success { claim_id } => {
                assert!(claim_id > 0);
            }
            _ => panic!("Expected ClaimResult::Success"),
        }

        let claim = db.get_claim(123, ClaimType::Issue)?;
        assert!(claim.is_some());
        let claim = claim.unwrap();
        assert_eq!(claim.number, 123);
        assert_eq!(claim.terminal_id, "terminal-1");
        assert_eq!(claim.label, Some("loom:building".to_string()));
        assert_eq!(claim.agent_role, Some("builder".to_string()));

        Ok(())
    }

    #[test]
    fn test_claim_issue_already_claimed() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        let _result1 = db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;

        let result2 = db.claim_issue(123, ClaimType::Issue, "terminal-2", None, None, None)?;

        match result2 {
            ClaimResult::AlreadyClaimed { terminal_id, .. } => {
                assert_eq!(terminal_id, "terminal-1");
            }
            _ => panic!("Expected ClaimResult::AlreadyClaimed"),
        }

        Ok(())
    }

    #[test]
    fn test_claim_issue_reclaim_stale() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        let _result1 = db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;

        // Manually make it stale by updating the heartbeat to the past
        let old_time = Utc::now() - chrono::Duration::hours(2);
        db.conn.execute(
            "UPDATE issue_claims SET last_heartbeat = ?1 WHERE number = 123",
            params![old_time.to_rfc3339()],
        )?;

        // Now claim with stale threshold of 1 hour - should reclaim
        let result2 =
            db.claim_issue(123, ClaimType::Issue, "terminal-2", None, None, Some(3600))?;

        match result2 {
            ClaimResult::Reclaimed {
                previous_terminal, ..
            } => {
                assert_eq!(previous_terminal, "terminal-1");
            }
            _ => panic!("Expected ClaimResult::Reclaimed"),
        }

        let claim = db.get_claim(123, ClaimType::Issue)?.unwrap();
        assert_eq!(claim.terminal_id, "terminal-2");

        Ok(())
    }

    #[test]
    fn test_release_claim() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;

        let released = db.release_claim(123, ClaimType::Issue, Some("terminal-1"))?;
        assert!(released);

        let claim = db.get_claim(123, ClaimType::Issue)?;
        assert!(claim.is_none());

        Ok(())
    }

    #[test]
    fn test_release_claim_wrong_terminal() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;

        let released = db.release_claim(123, ClaimType::Issue, Some("terminal-2"))?;
        assert!(!released);

        let claim = db.get_claim(123, ClaimType::Issue)?;
        assert!(claim.is_some());

        Ok(())
    }

    #[test]
    fn test_heartbeat_claim() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;

        let claim_before = db.get_claim(123, ClaimType::Issue)?.unwrap();
        let heartbeat_before = claim_before.last_heartbeat;

        std::thread::sleep(std::time::Duration::from_millis(10));

        let updated = db.heartbeat_claim(123, ClaimType::Issue, "terminal-1")?;
        assert!(updated);

        let claim_after = db.get_claim(123, ClaimType::Issue)?.unwrap();
        assert!(claim_after.last_heartbeat > heartbeat_before);

        Ok(())
    }

    #[test]
    fn test_get_terminal_claims() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(456, ClaimType::Pr, "terminal-1", None, None, None)?;
        db.claim_issue(789, ClaimType::Issue, "terminal-2", None, None, None)?;

        let claims = db.get_claims_by_terminal("terminal-1")?;
        assert_eq!(claims.len(), 2);

        let claims = db.get_claims_by_terminal("terminal-2")?;
        assert_eq!(claims.len(), 1);

        Ok(())
    }

    #[test]
    fn test_get_stale_claims() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(456, ClaimType::Issue, "terminal-2", None, None, None)?;

        let old_time = Utc::now() - chrono::Duration::hours(2);
        db.conn.execute(
            "UPDATE issue_claims SET last_heartbeat = ?1 WHERE number = 123",
            params![old_time.to_rfc3339()],
        )?;

        let stale = db.get_stale_claims(3600)?;
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].number, 123);

        Ok(())
    }

    #[test]
    fn test_release_stale_claims() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(456, ClaimType::Issue, "terminal-2", None, None, None)?;

        let old_time = Utc::now() - chrono::Duration::hours(2);
        db.conn.execute(
            "UPDATE issue_claims SET last_heartbeat = ?1 WHERE number = 123",
            params![old_time.to_rfc3339()],
        )?;

        let count = db.release_stale_claims(3600)?;
        assert_eq!(count, 1);

        let all = db.get_all_claims()?;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].number, 456);

        Ok(())
    }

    #[test]
    fn test_release_terminal_claims() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(456, ClaimType::Pr, "terminal-1", None, None, None)?;
        db.claim_issue(789, ClaimType::Issue, "terminal-2", None, None, None)?;

        let count = db.release_terminal_claims("terminal-1")?;
        assert_eq!(count, 2);

        let all = db.get_all_claims()?;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].terminal_id, "terminal-2");

        Ok(())
    }

    #[test]
    fn test_claims_summary() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(100, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(101, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(200, ClaimType::Pr, "terminal-2", None, None, None)?;

        let old_time = Utc::now() - chrono::Duration::hours(2);
        db.conn.execute(
            "UPDATE issue_claims SET last_heartbeat = ?1 WHERE number = 100",
            params![old_time.to_rfc3339()],
        )?;

        let summary = db.get_claims_summary(3600)?;
        assert_eq!(summary.total_claims, 3);
        assert_eq!(summary.by_type.get("issue"), Some(&2));
        assert_eq!(summary.by_type.get("pr"), Some(&1));
        assert_eq!(summary.by_terminal.get("terminal-1").map(|v| v.len()), Some(2));
        assert_eq!(summary.by_terminal.get("terminal-2").map(|v| v.len()), Some(1));
        assert_eq!(summary.stale_claims.len(), 1);

        Ok(())
    }

    #[test]
    fn test_claim_type_isolation() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        db.claim_issue(123, ClaimType::Issue, "terminal-1", None, None, None)?;
        db.claim_issue(123, ClaimType::Pr, "terminal-2", None, None, None)?;

        let issue_claim = db.get_claim(123, ClaimType::Issue)?.unwrap();
        let pr_claim = db.get_claim(123, ClaimType::Pr)?.unwrap();

        assert_eq!(issue_claim.terminal_id, "terminal-1");
        assert_eq!(pr_claim.terminal_id, "terminal-2");

        Ok(())
    }
}
