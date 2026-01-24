//! Self-tuning module for automatic parameter adjustment.
//!
//! This module implements self-tuning based on effectiveness data,
//! allowing system parameters to automatically adjust based on
//! measured outcomes.
//!
//! # Safety Rails
//!
//! - All changes are gradual (max 10% adjustment per cycle)
//! - Changes are reversible (full audit trail)
//! - Automatic rollback if metrics degrade significantly
//! - Human approval required for significant changes (>20% cumulative)
//!
//! # Tunable Parameters
//!
//! - `autonomous_interval_ms` - Time between autonomous agent actions
//! - `review_threshold` - Success rate threshold for auto-approval
//! - `escalation_timeout_ms` - Time before escalating to human
//! - `max_doctor_iterations` - Maximum PR fix attempts
//!
//! # Example
//!
//! ```ignore
//! use activity::tuning::{TuningEngine, TuningConfig};
//!
//! let engine = TuningEngine::new(conn, TuningConfig::default());
//!
//! // Analyze current effectiveness and propose adjustments
//! let proposals = engine.analyze_and_propose()?;
//!
//! // Apply approved proposals
//! for proposal in proposals.iter().filter(|p| p.status == ProposalStatus::Approved) {
//!     engine.apply_proposal(proposal.id)?;
//! }
//! ```

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

// ============================================================================
// Types
// ============================================================================

/// Status of a tuning proposal
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Proposal is pending human review
    Pending,
    /// Proposal has been approved (automatically or by human)
    Approved,
    /// Proposal has been rejected
    Rejected,
    /// Proposal has been applied to the system
    Applied,
    /// Proposal was rolled back due to negative impact
    RolledBack,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Applied => "applied",
            Self::RolledBack => "rolled_back",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "applied" => Some(Self::Applied),
            "rolled_back" => Some(Self::RolledBack),
            _ => None,
        }
    }
}

/// A tunable system parameter
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunableParameter {
    /// Unique parameter identifier
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Current value
    pub current_value: f64,
    /// Default value (used for reset)
    pub default_value: f64,
    /// Minimum allowed value
    pub min_value: f64,
    /// Maximum allowed value
    pub max_value: f64,
    /// Unit of measurement (e.g., "ms", "percent", "count")
    pub unit: String,
    /// Whether this parameter can be auto-tuned
    pub auto_tunable: bool,
    /// Last modified timestamp
    pub updated_at: DateTime<Utc>,
}

/// A proposed parameter adjustment
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningProposal {
    /// Unique proposal ID
    pub id: Option<i64>,
    /// Parameter being tuned
    pub parameter_name: String,
    /// Value before proposed change
    pub old_value: f64,
    /// Proposed new value
    pub new_value: f64,
    /// Percentage change
    pub change_percent: f64,
    /// Reason for the proposal
    pub reason: String,
    /// Evidence/metrics supporting the proposal
    pub evidence: String,
    /// Current status
    pub status: ProposalStatus,
    /// Confidence score (0.0-1.0)
    pub confidence: f64,
    /// Whether human approval is required
    pub requires_approval: bool,
    /// Created timestamp
    pub created_at: DateTime<Utc>,
    /// Applied timestamp (if applied)
    pub applied_at: Option<DateTime<Utc>>,
    /// ID of the proposal that rolled back this one (if rolled back)
    pub rollback_proposal_id: Option<i64>,
}

/// Record of a parameter value change
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningHistory {
    /// Unique history entry ID
    pub id: Option<i64>,
    /// Parameter that was changed
    pub parameter_name: String,
    /// Value before change
    pub old_value: f64,
    /// Value after change
    pub new_value: f64,
    /// Associated proposal ID (if any)
    pub proposal_id: Option<i64>,
    /// Who/what made the change
    pub changed_by: String,
    /// Reason for the change
    pub reason: String,
    /// Timestamp of the change
    pub timestamp: DateTime<Utc>,
}

/// Effectiveness metrics snapshot for tuning decisions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivenessSnapshot {
    /// Snapshot timestamp
    pub timestamp: DateTime<Utc>,
    /// Overall success rate (0.0-1.0)
    pub success_rate: f64,
    /// Average cycle time in hours
    pub avg_cycle_time_hours: f64,
    /// Average cost per task in USD
    pub avg_cost_per_task: f64,
    /// Number of tasks completed
    pub tasks_completed: i64,
    /// Number of PRs merged
    pub prs_merged: i64,
    /// Average rework cycles per PR
    pub avg_rework_count: f64,
}

/// Configuration for the tuning engine
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningConfig {
    /// Maximum adjustment percentage per cycle (default: 10%)
    pub max_adjustment_percent: f64,
    /// Cumulative change threshold requiring human approval (default: 20%)
    pub approval_threshold_percent: f64,
    /// Minimum sample size before making adjustments
    pub min_sample_size: i64,
    /// Minimum confidence level for auto-approval (default: 0.8)
    pub min_auto_approval_confidence: f64,
    /// Degradation threshold for automatic rollback (default: 15%)
    pub rollback_threshold_percent: f64,
    /// Observation period after applying changes (hours)
    pub observation_period_hours: i64,
}

impl Default for TuningConfig {
    fn default() -> Self {
        Self {
            max_adjustment_percent: 10.0,
            approval_threshold_percent: 20.0,
            min_sample_size: 10,
            min_auto_approval_confidence: 0.8,
            rollback_threshold_percent: 15.0,
            observation_period_hours: 24,
        }
    }
}

/// Summary of tuning activity
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningSummary {
    /// Total parameters being tracked
    pub total_parameters: i64,
    /// Parameters that are auto-tunable
    pub auto_tunable_count: i64,
    /// Pending proposals awaiting approval
    pub pending_proposals: i64,
    /// Total proposals applied
    pub applied_proposals: i64,
    /// Total rollbacks performed
    pub rollbacks_count: i64,
    /// Average effectiveness improvement (%)
    pub avg_improvement_percent: f64,
    /// Last tuning cycle timestamp
    pub last_tuning_cycle: Option<DateTime<Utc>>,
}

// ============================================================================
// Schema
// ============================================================================

/// Create tuning-related database tables
pub fn create_tuning_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r"
        -- Tunable parameters registry
        CREATE TABLE IF NOT EXISTS tunable_parameters (
            name TEXT PRIMARY KEY,
            description TEXT NOT NULL,
            current_value REAL NOT NULL,
            default_value REAL NOT NULL,
            min_value REAL NOT NULL,
            max_value REAL NOT NULL,
            unit TEXT NOT NULL DEFAULT '',
            auto_tunable BOOLEAN NOT NULL DEFAULT 1,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        -- Tuning proposals (pending, approved, rejected, applied, rolled_back)
        CREATE TABLE IF NOT EXISTS tuning_proposals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            parameter_name TEXT NOT NULL REFERENCES tunable_parameters(name),
            old_value REAL NOT NULL,
            new_value REAL NOT NULL,
            change_percent REAL NOT NULL,
            reason TEXT NOT NULL,
            evidence TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'pending',
            confidence REAL NOT NULL DEFAULT 0.5,
            requires_approval BOOLEAN NOT NULL DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            applied_at DATETIME,
            rollback_proposal_id INTEGER REFERENCES tuning_proposals(id)
        );

        CREATE INDEX IF NOT EXISTS idx_tuning_proposals_parameter ON tuning_proposals(parameter_name);
        CREATE INDEX IF NOT EXISTS idx_tuning_proposals_status ON tuning_proposals(status);
        CREATE INDEX IF NOT EXISTS idx_tuning_proposals_created ON tuning_proposals(created_at);

        -- Tuning history (audit trail of all changes)
        CREATE TABLE IF NOT EXISTS tuning_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            parameter_name TEXT NOT NULL REFERENCES tunable_parameters(name),
            old_value REAL NOT NULL,
            new_value REAL NOT NULL,
            proposal_id INTEGER REFERENCES tuning_proposals(id),
            changed_by TEXT NOT NULL DEFAULT 'system',
            reason TEXT NOT NULL,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE INDEX IF NOT EXISTS idx_tuning_history_parameter ON tuning_history(parameter_name);
        CREATE INDEX IF NOT EXISTS idx_tuning_history_timestamp ON tuning_history(timestamp);

        -- Effectiveness snapshots for trend analysis
        CREATE TABLE IF NOT EXISTS effectiveness_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            success_rate REAL NOT NULL,
            avg_cycle_time_hours REAL NOT NULL,
            avg_cost_per_task REAL NOT NULL,
            tasks_completed INTEGER NOT NULL,
            prs_merged INTEGER NOT NULL,
            avg_rework_count REAL NOT NULL DEFAULT 0.0
        );

        CREATE INDEX IF NOT EXISTS idx_effectiveness_snapshots_timestamp ON effectiveness_snapshots(timestamp);
        ",
    )?;

    // Insert default tunable parameters if they don't exist
    let default_params = [
        (
            "autonomous_interval_ms",
            "Time between autonomous agent actions",
            300000.0,
            300000.0,
            60000.0,
            3600000.0,
            "ms",
            true,
        ),
        (
            "review_threshold",
            "Success rate threshold for auto-approval",
            0.8,
            0.8,
            0.5,
            1.0,
            "ratio",
            true,
        ),
        (
            "escalation_timeout_ms",
            "Time before escalating blocked issues to human",
            1800000.0,
            1800000.0,
            300000.0,
            7200000.0,
            "ms",
            true,
        ),
        (
            "max_doctor_iterations",
            "Maximum PR fix attempts before blocking",
            3.0,
            3.0,
            1.0,
            10.0,
            "count",
            true,
        ),
        (
            "cost_alert_threshold",
            "Budget usage percentage that triggers alerts",
            0.8,
            0.8,
            0.5,
            0.95,
            "ratio",
            true,
        ),
        (
            "min_confidence_for_merge",
            "Minimum Judge confidence for auto-merge",
            0.7,
            0.7,
            0.5,
            0.95,
            "ratio",
            true,
        ),
    ];

    for (name, desc, current, default, min, max, unit, auto) in default_params {
        conn.execute(
            r"INSERT OR IGNORE INTO tunable_parameters
              (name, description, current_value, default_value, min_value, max_value, unit, auto_tunable)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![name, desc, current, default, min, max, unit, auto],
        )?;
    }

    Ok(())
}

// ============================================================================
// Queries
// ============================================================================

/// Get all tunable parameters
pub fn get_tunable_parameters(conn: &Connection) -> rusqlite::Result<Vec<TunableParameter>> {
    let mut stmt = conn.prepare(
        r"SELECT name, description, current_value, default_value, min_value, max_value, unit, auto_tunable, updated_at
          FROM tunable_parameters
          ORDER BY name",
    )?;

    let rows = stmt.query_map([], |row| {
        let updated_at_str: String = row.get(8)?;
        let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        Ok(TunableParameter {
            name: row.get(0)?,
            description: row.get(1)?,
            current_value: row.get(2)?,
            default_value: row.get(3)?,
            min_value: row.get(4)?,
            max_value: row.get(5)?,
            unit: row.get(6)?,
            auto_tunable: row.get(7)?,
            updated_at,
        })
    })?;

    rows.collect()
}

/// Get a single tunable parameter by name
pub fn get_parameter(conn: &Connection, name: &str) -> rusqlite::Result<Option<TunableParameter>> {
    let result = conn.query_row(
        r"SELECT name, description, current_value, default_value, min_value, max_value, unit, auto_tunable, updated_at
          FROM tunable_parameters
          WHERE name = ?1",
        params![name],
        |row| {
            let updated_at_str: String = row.get(8)?;
            let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            Ok(TunableParameter {
                name: row.get(0)?,
                description: row.get(1)?,
                current_value: row.get(2)?,
                default_value: row.get(3)?,
                min_value: row.get(4)?,
                max_value: row.get(5)?,
                unit: row.get(6)?,
                auto_tunable: row.get(7)?,
                updated_at,
            })
        },
    );

    match result {
        Ok(param) => Ok(Some(param)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Update a parameter value
pub fn update_parameter(
    conn: &Connection,
    name: &str,
    new_value: f64,
    changed_by: &str,
    reason: &str,
    proposal_id: Option<i64>,
) -> rusqlite::Result<()> {
    // Get current value first
    let current: f64 = conn.query_row(
        "SELECT current_value FROM tunable_parameters WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )?;

    // Update the parameter
    conn.execute(
        r"UPDATE tunable_parameters
          SET current_value = ?1, updated_at = ?2
          WHERE name = ?3",
        params![new_value, Utc::now().to_rfc3339(), name],
    )?;

    // Record in history
    conn.execute(
        r"INSERT INTO tuning_history (parameter_name, old_value, new_value, proposal_id, changed_by, reason)
          VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![name, current, new_value, proposal_id, changed_by, reason],
    )?;

    Ok(())
}

/// Reset a parameter to its default value
pub fn reset_parameter(conn: &Connection, name: &str, reason: &str) -> rusqlite::Result<()> {
    let default: f64 = conn.query_row(
        "SELECT default_value FROM tunable_parameters WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )?;

    update_parameter(conn, name, default, "human", reason, None)
}

/// Get pending tuning proposals
pub fn get_pending_proposals(conn: &Connection) -> rusqlite::Result<Vec<TuningProposal>> {
    get_proposals_by_status(conn, ProposalStatus::Pending)
}

/// Get proposals by status
pub fn get_proposals_by_status(
    conn: &Connection,
    status: ProposalStatus,
) -> rusqlite::Result<Vec<TuningProposal>> {
    let mut stmt = conn.prepare(
        r"SELECT id, parameter_name, old_value, new_value, change_percent, reason, evidence,
                 status, confidence, requires_approval, created_at, applied_at, rollback_proposal_id
          FROM tuning_proposals
          WHERE status = ?1
          ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map(params![status.as_str()], map_proposal)?;
    rows.collect()
}

/// Get recent proposals (all statuses)
pub fn get_recent_proposals(
    conn: &Connection,
    limit: i64,
) -> rusqlite::Result<Vec<TuningProposal>> {
    let mut stmt = conn.prepare(
        r"SELECT id, parameter_name, old_value, new_value, change_percent, reason, evidence,
                 status, confidence, requires_approval, created_at, applied_at, rollback_proposal_id
          FROM tuning_proposals
          ORDER BY created_at DESC
          LIMIT ?1",
    )?;

    let rows = stmt.query_map(params![limit], map_proposal)?;
    rows.collect()
}

fn map_proposal(row: &rusqlite::Row<'_>) -> rusqlite::Result<TuningProposal> {
    let created_at_str: String = row.get(10)?;
    let applied_at_str: Option<String> = row.get(11)?;
    let status_str: String = row.get(7)?;

    Ok(TuningProposal {
        id: row.get(0)?,
        parameter_name: row.get(1)?,
        old_value: row.get(2)?,
        new_value: row.get(3)?,
        change_percent: row.get(4)?,
        reason: row.get(5)?,
        evidence: row.get(6)?,
        status: ProposalStatus::from_str(&status_str).unwrap_or(ProposalStatus::Pending),
        confidence: row.get(8)?,
        requires_approval: row.get(9)?,
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        applied_at: applied_at_str.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .ok()
        }),
        rollback_proposal_id: row.get(12)?,
    })
}

/// Create a new tuning proposal
pub fn create_proposal(conn: &Connection, proposal: &TuningProposal) -> rusqlite::Result<i64> {
    conn.execute(
        r"INSERT INTO tuning_proposals
          (parameter_name, old_value, new_value, change_percent, reason, evidence, status, confidence, requires_approval)
          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            proposal.parameter_name,
            proposal.old_value,
            proposal.new_value,
            proposal.change_percent,
            proposal.reason,
            proposal.evidence,
            proposal.status.as_str(),
            proposal.confidence,
            proposal.requires_approval,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Update proposal status
pub fn update_proposal_status(
    conn: &Connection,
    proposal_id: i64,
    status: ProposalStatus,
) -> rusqlite::Result<()> {
    let applied_at = if status == ProposalStatus::Applied {
        Some(Utc::now().to_rfc3339())
    } else {
        None
    };

    conn.execute(
        r"UPDATE tuning_proposals
          SET status = ?1, applied_at = ?2
          WHERE id = ?3",
        params![status.as_str(), applied_at, proposal_id],
    )?;

    Ok(())
}

/// Apply a proposal (updates parameter and marks proposal as applied)
pub fn apply_proposal(conn: &Connection, proposal_id: i64) -> rusqlite::Result<()> {
    // Get proposal details
    let (param_name, new_value): (String, f64) = conn.query_row(
        "SELECT parameter_name, new_value FROM tuning_proposals WHERE id = ?1",
        params![proposal_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // Update the parameter
    update_parameter(
        conn,
        &param_name,
        new_value,
        "tuning_engine",
        &format!("Applied proposal #{proposal_id}"),
        Some(proposal_id),
    )?;

    // Mark proposal as applied
    update_proposal_status(conn, proposal_id, ProposalStatus::Applied)?;

    Ok(())
}

/// Record an effectiveness snapshot
pub fn record_effectiveness_snapshot(
    conn: &Connection,
    snapshot: &EffectivenessSnapshot,
) -> rusqlite::Result<i64> {
    conn.execute(
        r"INSERT INTO effectiveness_snapshots
          (timestamp, success_rate, avg_cycle_time_hours, avg_cost_per_task, tasks_completed, prs_merged, avg_rework_count)
          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot.timestamp.to_rfc3339(),
            snapshot.success_rate,
            snapshot.avg_cycle_time_hours,
            snapshot.avg_cost_per_task,
            snapshot.tasks_completed,
            snapshot.prs_merged,
            snapshot.avg_rework_count,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Get recent effectiveness snapshots
pub fn get_effectiveness_history(
    conn: &Connection,
    days: i64,
) -> rusqlite::Result<Vec<EffectivenessSnapshot>> {
    let mut stmt = conn.prepare(
        r"SELECT timestamp, success_rate, avg_cycle_time_hours, avg_cost_per_task, tasks_completed, prs_merged, avg_rework_count
          FROM effectiveness_snapshots
          WHERE timestamp >= datetime('now', ?1 || ' days')
          ORDER BY timestamp DESC",
    )?;

    let days_str = format!("-{days}");
    let rows = stmt.query_map(params![days_str], |row| {
        let timestamp_str: String = row.get(0)?;
        Ok(EffectivenessSnapshot {
            timestamp: DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            success_rate: row.get(1)?,
            avg_cycle_time_hours: row.get(2)?,
            avg_cost_per_task: row.get(3)?,
            tasks_completed: row.get(4)?,
            prs_merged: row.get(5)?,
            avg_rework_count: row.get(6)?,
        })
    })?;

    rows.collect()
}

/// Get tuning history for a parameter
pub fn get_parameter_history(
    conn: &Connection,
    parameter_name: &str,
    limit: i64,
) -> rusqlite::Result<Vec<TuningHistory>> {
    let mut stmt = conn.prepare(
        r"SELECT id, parameter_name, old_value, new_value, proposal_id, changed_by, reason, timestamp
          FROM tuning_history
          WHERE parameter_name = ?1
          ORDER BY timestamp DESC
          LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![parameter_name, limit], |row| {
        let timestamp_str: String = row.get(7)?;
        Ok(TuningHistory {
            id: row.get(0)?,
            parameter_name: row.get(1)?,
            old_value: row.get(2)?,
            new_value: row.get(3)?,
            proposal_id: row.get(4)?,
            changed_by: row.get(5)?,
            reason: row.get(6)?,
            timestamp: DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    })?;

    rows.collect()
}

/// Get tuning summary statistics
pub fn get_tuning_summary(conn: &Connection) -> rusqlite::Result<TuningSummary> {
    // Get parameter counts
    let (total_parameters, auto_tunable_count): (i64, i64) = conn.query_row(
        r"SELECT COUNT(*), SUM(CASE WHEN auto_tunable = 1 THEN 1 ELSE 0 END)
          FROM tunable_parameters",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // Get proposal counts
    let pending_proposals: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tuning_proposals WHERE status = 'pending'",
        [],
        |row| row.get(0),
    )?;

    let applied_proposals: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tuning_proposals WHERE status = 'applied'",
        [],
        |row| row.get(0),
    )?;

    let rollbacks_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tuning_proposals WHERE status = 'rolled_back'",
        [],
        |row| row.get(0),
    )?;

    // Calculate average improvement (comparing first and last snapshots)
    let avg_improvement_percent: f64 = conn
        .query_row(
            r"SELECT
                CASE
                    WHEN first_rate > 0 THEN ((last_rate - first_rate) / first_rate) * 100
                    ELSE 0
                END as improvement
              FROM (
                SELECT
                    (SELECT success_rate FROM effectiveness_snapshots ORDER BY timestamp ASC LIMIT 1) as first_rate,
                    (SELECT success_rate FROM effectiveness_snapshots ORDER BY timestamp DESC LIMIT 1) as last_rate
              )",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Get last tuning cycle
    let last_tuning_cycle: Option<DateTime<Utc>> = conn
        .query_row(
            "SELECT timestamp FROM effectiveness_snapshots ORDER BY timestamp DESC LIMIT 1",
            [],
            |row| {
                let timestamp_str: String = row.get(0)?;
                Ok(DateTime::parse_from_rfc3339(&timestamp_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .ok())
            },
        )
        .unwrap_or(None);

    Ok(TuningSummary {
        total_parameters,
        auto_tunable_count,
        pending_proposals,
        applied_proposals,
        rollbacks_count,
        avg_improvement_percent,
        last_tuning_cycle,
    })
}

// ============================================================================
// Analysis Functions
// ============================================================================

/// Analyze effectiveness data and propose parameter adjustments
pub fn analyze_and_propose(
    conn: &Connection,
    config: &TuningConfig,
) -> rusqlite::Result<Vec<TuningProposal>> {
    let mut proposals = Vec::new();

    // Get recent effectiveness snapshots
    let snapshots = get_effectiveness_history(conn, 7)?;
    if snapshots.len() < config.min_sample_size as usize {
        log::info!(
            "Insufficient data for tuning analysis: {} samples (need {})",
            snapshots.len(),
            config.min_sample_size
        );
        return Ok(proposals);
    }

    // Calculate trends
    let recent_success_rate = snapshots
        .iter()
        .take(3)
        .map(|s| s.success_rate)
        .sum::<f64>()
        / 3.0_f64.min(snapshots.len() as f64);

    let older_success_rate = snapshots
        .iter()
        .skip(3)
        .map(|s| s.success_rate)
        .sum::<f64>()
        / (snapshots.len() - 3).max(1) as f64;

    let success_trend = recent_success_rate - older_success_rate;

    // Get tunable parameters
    let params = get_tunable_parameters(conn)?;

    for param in params.iter().filter(|p| p.auto_tunable) {
        if let Some(proposal) = analyze_parameter(param, success_trend, &snapshots, config) {
            proposals.push(proposal);
        }
    }

    // Create proposals in database
    for proposal in &mut proposals {
        let id = create_proposal(conn, proposal)?;
        proposal.id = Some(id);
    }

    Ok(proposals)
}

/// Analyze a single parameter and potentially propose an adjustment
fn analyze_parameter(
    param: &TunableParameter,
    success_trend: f64,
    snapshots: &[EffectivenessSnapshot],
    config: &TuningConfig,
) -> Option<TuningProposal> {
    // Calculate adjustment based on parameter type and trend
    let (adjustment_ratio, reason, confidence) = match param.name.as_str() {
        "autonomous_interval_ms" => {
            // If success rate is high and stable, consider reducing interval (more frequent)
            // If success rate is low, increase interval (more time between attempts)
            if success_trend > 0.05 {
                (-0.05, "High success trend - increasing frequency", 0.7)
            } else if success_trend < -0.05 {
                (0.05, "Low success trend - reducing frequency", 0.6)
            } else {
                return None;
            }
        }
        "review_threshold" => {
            // Adjust review threshold based on rework rates
            let avg_rework =
                snapshots.iter().map(|s| s.avg_rework_count).sum::<f64>() / snapshots.len() as f64;
            if avg_rework > 2.0 {
                (0.05, "High rework rate - increasing review threshold", 0.75)
            } else if avg_rework < 0.5 {
                (-0.03, "Low rework rate - relaxing review threshold", 0.65)
            } else {
                return None;
            }
        }
        "max_doctor_iterations" => {
            // Check if PRs frequently hit the max iterations
            let recent_cost = snapshots
                .iter()
                .take(3)
                .map(|s| s.avg_cost_per_task)
                .sum::<f64>()
                / 3.0;
            let older_cost = snapshots
                .iter()
                .skip(3)
                .map(|s| s.avg_cost_per_task)
                .sum::<f64>()
                / (snapshots.len() - 3).max(1) as f64;

            if recent_cost > older_cost * 1.2 {
                // Costs increasing significantly, might need more iterations
                (
                    1.0 / param.current_value,
                    "Increasing costs suggest more fix attempts may help",
                    0.6,
                )
            } else {
                return None;
            }
        }
        _ => return None, // No specific logic for this parameter yet
    };

    // Calculate new value with safety bounds
    let adjustment = param.current_value * adjustment_ratio;
    let clamped_adjustment = adjustment
        .abs()
        .min(param.current_value * config.max_adjustment_percent / 100.0)
        * adjustment.signum();

    let new_value = (param.current_value + clamped_adjustment)
        .max(param.min_value)
        .min(param.max_value);

    // Skip if change is too small
    let change_percent = ((new_value - param.current_value) / param.current_value * 100.0).abs();
    if change_percent < 1.0 {
        return None;
    }

    // Determine if human approval is required
    let cumulative_change = calculate_cumulative_change(param);
    let requires_approval = cumulative_change + change_percent > config.approval_threshold_percent
        || confidence < config.min_auto_approval_confidence;

    // Determine initial status
    let status = if requires_approval {
        ProposalStatus::Pending
    } else {
        ProposalStatus::Approved
    };

    Some(TuningProposal {
        id: None,
        parameter_name: param.name.clone(),
        old_value: param.current_value,
        new_value,
        change_percent,
        reason: reason.to_string(),
        evidence: format!(
            "Success trend: {:.2}%, Recent success rate: {:.2}%",
            success_trend * 100.0,
            snapshots
                .first()
                .map(|s| s.success_rate * 100.0)
                .unwrap_or(0.0)
        ),
        status,
        confidence,
        requires_approval,
        created_at: Utc::now(),
        applied_at: None,
        rollback_proposal_id: None,
    })
}

/// Calculate cumulative change percentage for a parameter (from default)
fn calculate_cumulative_change(param: &TunableParameter) -> f64 {
    if param.default_value == 0.0 {
        0.0
    } else {
        ((param.current_value - param.default_value) / param.default_value * 100.0).abs()
    }
}

/// Check if any applied proposals should be rolled back due to degradation
pub fn check_for_rollbacks(conn: &Connection, config: &TuningConfig) -> rusqlite::Result<Vec<i64>> {
    let mut rollback_ids = Vec::new();

    // Get recently applied proposals within the observation period
    let observation_hours = config.observation_period_hours;
    let mut stmt = conn.prepare(
        r"SELECT id, parameter_name, old_value, applied_at
          FROM tuning_proposals
          WHERE status = 'applied'
            AND applied_at >= datetime('now', ?1 || ' hours')",
    )?;

    let hours_str = format!("-{observation_hours}");
    let recent_applied: Vec<(i64, String, f64, String)> = stmt
        .query_map(params![hours_str], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Get effectiveness before and after each proposal was applied
    for (proposal_id, _param_name, old_value, applied_at_str) in recent_applied {
        let applied_at = DateTime::parse_from_rfc3339(&applied_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        // Get success rate before the change
        let before_rate: Option<f64> = conn
            .query_row(
                r"SELECT AVG(success_rate)
                  FROM effectiveness_snapshots
                  WHERE timestamp < ?1
                  ORDER BY timestamp DESC
                  LIMIT 5",
                params![applied_at.to_rfc3339()],
                |row| row.get(0),
            )
            .ok();

        // Get success rate after the change
        let after_rate: Option<f64> = conn
            .query_row(
                r"SELECT AVG(success_rate)
                  FROM effectiveness_snapshots
                  WHERE timestamp >= ?1
                  ORDER BY timestamp ASC
                  LIMIT 5",
                params![applied_at.to_rfc3339()],
                |row| row.get(0),
            )
            .ok();

        if let (Some(before), Some(after)) = (before_rate, after_rate) {
            let degradation_pct = if before > 0.0 {
                (before - after) / before * 100.0
            } else {
                0.0
            };

            if degradation_pct > config.rollback_threshold_percent {
                log::warn!(
                    "Proposal #{} caused {:.1}% degradation, scheduling rollback",
                    proposal_id,
                    degradation_pct
                );

                // Create rollback proposal
                let rollback = TuningProposal {
                    id: None,
                    parameter_name: _param_name.clone(),
                    old_value, // This is what we're rolling back TO
                    new_value: old_value,
                    change_percent: 0.0, // Rollback to previous value
                    reason: format!(
                        "Automatic rollback due to {:.1}% performance degradation",
                        degradation_pct
                    ),
                    evidence: format!(
                        "Before: {:.2}%, After: {:.2}%",
                        before * 100.0,
                        after * 100.0
                    ),
                    status: ProposalStatus::Approved,
                    confidence: 1.0, // High confidence for rollbacks
                    requires_approval: false,
                    created_at: Utc::now(),
                    applied_at: None,
                    rollback_proposal_id: None,
                };

                let rollback_id = create_proposal(conn, &rollback)?;

                // Mark original proposal as rolled back
                conn.execute(
                    "UPDATE tuning_proposals SET status = 'rolled_back', rollback_proposal_id = ?1 WHERE id = ?2",
                    params![rollback_id, proposal_id],
                )?;

                rollback_ids.push(rollback_id);
            }
        }
    }

    Ok(rollback_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_test_db() -> rusqlite::Result<(Connection, tempfile::TempDir)> {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let conn = Connection::open(&db_path)?;
        create_tuning_schema(&conn)?;
        Ok((conn, temp_dir))
    }

    #[test]
    fn test_create_schema() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        // Verify default parameters exist
        let params = get_tunable_parameters(&conn)?;
        assert!(!params.is_empty());
        assert!(params.iter().any(|p| p.name == "autonomous_interval_ms"));

        Ok(())
    }

    #[test]
    fn test_update_parameter() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        let param = get_parameter(&conn, "autonomous_interval_ms")?.unwrap();
        let old_value = param.current_value;

        update_parameter(
            &conn,
            "autonomous_interval_ms",
            old_value * 1.1,
            "test",
            "Test adjustment",
            None,
        )?;

        let updated = get_parameter(&conn, "autonomous_interval_ms")?.unwrap();
        assert!((updated.current_value - old_value * 1.1).abs() < 0.001);

        // Check history was recorded
        let history = get_parameter_history(&conn, "autonomous_interval_ms", 10)?;
        assert!(!history.is_empty());
        assert_eq!(history[0].changed_by, "test");

        Ok(())
    }

    #[test]
    fn test_proposals() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        let proposal = TuningProposal {
            id: None,
            parameter_name: "autonomous_interval_ms".to_string(),
            old_value: 300000.0,
            new_value: 270000.0,
            change_percent: 10.0,
            reason: "Test proposal".to_string(),
            evidence: "Test evidence".to_string(),
            status: ProposalStatus::Pending,
            confidence: 0.8,
            requires_approval: true,
            created_at: Utc::now(),
            applied_at: None,
            rollback_proposal_id: None,
        };

        let id = create_proposal(&conn, &proposal)?;
        assert!(id > 0);

        let pending = get_pending_proposals(&conn)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].parameter_name, "autonomous_interval_ms");

        // Approve and apply
        update_proposal_status(&conn, id, ProposalStatus::Approved)?;
        apply_proposal(&conn, id)?;

        let pending_after = get_pending_proposals(&conn)?;
        assert!(pending_after.is_empty());

        Ok(())
    }

    #[test]
    fn test_effectiveness_snapshots() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        let snapshot = EffectivenessSnapshot {
            timestamp: Utc::now(),
            success_rate: 0.85,
            avg_cycle_time_hours: 2.5,
            avg_cost_per_task: 0.15,
            tasks_completed: 10,
            prs_merged: 8,
            avg_rework_count: 0.5,
        };

        let id = record_effectiveness_snapshot(&conn, &snapshot)?;
        assert!(id > 0);

        let history = get_effectiveness_history(&conn, 1)?;
        assert_eq!(history.len(), 1);
        assert!((history[0].success_rate - 0.85).abs() < 0.001);

        Ok(())
    }

    #[test]
    fn test_tuning_summary() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        let summary = get_tuning_summary(&conn)?;
        assert!(summary.total_parameters > 0);
        assert!(summary.auto_tunable_count > 0);

        Ok(())
    }
}
