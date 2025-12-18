//! Telemetry commands for performance monitoring, error tracking, and usage analytics.
//!
//! All telemetry data is stored locally in SQLite databases within the ~/.loom directory.
//! This module provides commands for logging and querying telemetry data.

use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ============================================================================
// Types
// ============================================================================

/// Performance metric entry from frontend
#[derive(Debug, Serialize, Deserialize)]
pub struct PerformanceMetric {
    pub name: String,
    pub duration_ms: f64,
    pub timestamp: String,
    pub category: String,
    pub success: bool,
    pub metadata: Option<serde_json::Value>,
}

/// Error report entry from frontend
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorReport {
    pub message: String,
    pub stack: Option<String>,
    pub timestamp: String,
    pub component: String,
    pub context: Option<serde_json::Value>,
}

/// Usage event entry from frontend
#[derive(Debug, Serialize, Deserialize)]
pub struct UsageEvent {
    pub event_name: String,
    pub category: String,
    pub timestamp: String,
    pub properties: Option<serde_json::Value>,
}

/// Performance statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct PerformanceStats {
    pub category: String,
    pub count: i64,
    pub avg_duration_ms: f64,
    pub max_duration_ms: f64,
    pub min_duration_ms: f64,
    pub success_rate: f64,
}

/// Usage statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct UsageStats {
    pub event_name: String,
    pub category: String,
    pub count: i64,
    pub last_occurrence: String,
}

// ============================================================================
// Database Setup
// ============================================================================

/// Get the path to the telemetry database
fn get_telemetry_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".loom")
        .join("telemetry.db")
}

/// Open connection to telemetry database and ensure schema exists
fn open_telemetry_db() -> SqliteResult<Connection> {
    let db_path = get_telemetry_db_path();

    // Ensure .loom directory exists
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let conn = Connection::open(&db_path)?;

    // Create tables if they don't exist
    conn.execute_batch(
        r"
        -- Performance metrics table
        CREATE TABLE IF NOT EXISTS performance_metrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            duration_ms REAL NOT NULL,
            timestamp TEXT NOT NULL,
            category TEXT NOT NULL,
            success INTEGER NOT NULL DEFAULT 1,
            metadata TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE INDEX IF NOT EXISTS idx_perf_category ON performance_metrics(category);
        CREATE INDEX IF NOT EXISTS idx_perf_timestamp ON performance_metrics(timestamp);
        CREATE INDEX IF NOT EXISTS idx_perf_name ON performance_metrics(name);

        -- Error reports table
        CREATE TABLE IF NOT EXISTS error_reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            message TEXT NOT NULL,
            stack TEXT,
            timestamp TEXT NOT NULL,
            component TEXT NOT NULL,
            context TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE INDEX IF NOT EXISTS idx_error_timestamp ON error_reports(timestamp);
        CREATE INDEX IF NOT EXISTS idx_error_component ON error_reports(component);

        -- Usage events table
        CREATE TABLE IF NOT EXISTS usage_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_name TEXT NOT NULL,
            category TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            properties TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE INDEX IF NOT EXISTS idx_usage_category ON usage_events(category);
        CREATE INDEX IF NOT EXISTS idx_usage_timestamp ON usage_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_usage_event_name ON usage_events(event_name);
        ",
    )?;

    Ok(conn)
}

// ============================================================================
// Performance Metrics Commands
// ============================================================================

/// Log a performance metric
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_performance_metric(metric: PerformanceMetric) -> Result<(), String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;

    let metadata_json = metric
        .metadata
        .map(|m| serde_json::to_string(&m).unwrap_or_default());

    conn.execute(
        "INSERT INTO performance_metrics (name, duration_ms, timestamp, category, success, metadata)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            metric.name,
            metric.duration_ms,
            metric.timestamp,
            metric.category,
            i32::from(metric.success),
            metadata_json,
        ],
    )
    .map_err(|e| format!("Failed to insert performance metric: {e}"))?;

    Ok(())
}

/// Get performance metrics with optional filters
#[tauri::command]
pub fn get_performance_metrics(
    category: Option<String>,
    since: Option<String>,
    limit: Option<i32>,
) -> Result<Vec<PerformanceMetric>, String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;
    let limit = limit.unwrap_or(100);

    let query = match (&category, &since) {
        (Some(_), Some(_)) => {
            "SELECT name, duration_ms, timestamp, category, success, metadata
             FROM performance_metrics
             WHERE category = ?1 AND timestamp > ?2
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
        (Some(_), None) => {
            "SELECT name, duration_ms, timestamp, category, success, metadata
             FROM performance_metrics
             WHERE category = ?1
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
        (None, Some(_)) => {
            "SELECT name, duration_ms, timestamp, category, success, metadata
             FROM performance_metrics
             WHERE timestamp > ?2
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
        (None, None) => {
            "SELECT name, duration_ms, timestamp, category, success, metadata
             FROM performance_metrics
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let metrics = match (&category, &since) {
        (Some(cat), Some(since_time)) => stmt
            .query_map(params![cat, since_time, limit], map_performance_row)
            .map_err(|e| format!("Failed to query metrics: {e}"))?,
        (Some(cat), None) => stmt
            .query_map(params![cat, "", limit], map_performance_row)
            .map_err(|e| format!("Failed to query metrics: {e}"))?,
        (None, Some(since_time)) => stmt
            .query_map(params!["", since_time, limit], map_performance_row)
            .map_err(|e| format!("Failed to query metrics: {e}"))?,
        (None, None) => stmt
            .query_map(params!["", "", limit], map_performance_row)
            .map_err(|e| format!("Failed to query metrics: {e}"))?,
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to collect metrics: {e}"))?;

    Ok(metrics)
}

fn map_performance_row(row: &rusqlite::Row) -> rusqlite::Result<PerformanceMetric> {
    let metadata_str: Option<String> = row.get(5)?;
    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());

    Ok(PerformanceMetric {
        name: row.get(0)?,
        duration_ms: row.get(1)?,
        timestamp: row.get(2)?,
        category: row.get(3)?,
        success: row.get::<_, i32>(4)? != 0,
        metadata,
    })
}

/// Get performance statistics grouped by category
#[tauri::command]
pub fn get_performance_stats(since: Option<String>) -> Result<Vec<PerformanceStats>, String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;

    let query = if since.is_some() {
        "SELECT
            category,
            COUNT(*) as count,
            AVG(duration_ms) as avg_duration,
            MAX(duration_ms) as max_duration,
            MIN(duration_ms) as min_duration,
            CAST(SUM(success) AS REAL) / COUNT(*) as success_rate
         FROM performance_metrics
         WHERE timestamp > ?1
         GROUP BY category
         ORDER BY count DESC"
    } else {
        "SELECT
            category,
            COUNT(*) as count,
            AVG(duration_ms) as avg_duration,
            MAX(duration_ms) as max_duration,
            MIN(duration_ms) as min_duration,
            CAST(SUM(success) AS REAL) / COUNT(*) as success_rate
         FROM performance_metrics
         GROUP BY category
         ORDER BY count DESC"
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let stats = if let Some(since_time) = since {
        stmt.query_map([since_time], |row| {
            Ok(PerformanceStats {
                category: row.get(0)?,
                count: row.get(1)?,
                avg_duration_ms: row.get(2)?,
                max_duration_ms: row.get(3)?,
                min_duration_ms: row.get(4)?,
                success_rate: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query stats: {e}"))?
    } else {
        stmt.query_map([], |row| {
            Ok(PerformanceStats {
                category: row.get(0)?,
                count: row.get(1)?,
                avg_duration_ms: row.get(2)?,
                max_duration_ms: row.get(3)?,
                min_duration_ms: row.get(4)?,
                success_rate: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query stats: {e}"))?
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to collect stats: {e}"))?;

    Ok(stats)
}

// ============================================================================
// Error Tracking Commands
// ============================================================================

/// Log an error report
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_error_report(error: ErrorReport) -> Result<(), String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;

    let context_json = error
        .context
        .map(|c| serde_json::to_string(&c).unwrap_or_default());

    conn.execute(
        "INSERT INTO error_reports (message, stack, timestamp, component, context)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            error.message,
            error.stack,
            error.timestamp,
            error.component,
            context_json,
        ],
    )
    .map_err(|e| format!("Failed to insert error report: {e}"))?;

    Ok(())
}

/// Get error reports with optional filters
#[tauri::command]
pub fn get_error_reports(
    since: Option<String>,
    limit: Option<i32>,
) -> Result<Vec<ErrorReport>, String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;
    let limit = limit.unwrap_or(50);

    let query = if since.is_some() {
        "SELECT message, stack, timestamp, component, context
         FROM error_reports
         WHERE timestamp > ?1
         ORDER BY timestamp DESC
         LIMIT ?2"
    } else {
        "SELECT message, stack, timestamp, component, context
         FROM error_reports
         ORDER BY timestamp DESC
         LIMIT ?2"
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let errors = if let Some(since_time) = since {
        stmt.query_map(params![since_time, limit], map_error_row)
            .map_err(|e| format!("Failed to query errors: {e}"))?
    } else {
        stmt.query_map(params!["", limit], map_error_row)
            .map_err(|e| format!("Failed to query errors: {e}"))?
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to collect errors: {e}"))?;

    Ok(errors)
}

fn map_error_row(row: &rusqlite::Row) -> rusqlite::Result<ErrorReport> {
    let context_str: Option<String> = row.get(4)?;
    let context = context_str.and_then(|s| serde_json::from_str(&s).ok());

    Ok(ErrorReport {
        message: row.get(0)?,
        stack: row.get(1)?,
        timestamp: row.get(2)?,
        component: row.get(3)?,
        context,
    })
}

// ============================================================================
// Usage Analytics Commands
// ============================================================================

/// Log a usage event
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_usage_event(event: UsageEvent) -> Result<(), String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;

    let properties_json = event
        .properties
        .map(|p| serde_json::to_string(&p).unwrap_or_default());

    conn.execute(
        "INSERT INTO usage_events (event_name, category, timestamp, properties)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            event.event_name,
            event.category,
            event.timestamp,
            properties_json,
        ],
    )
    .map_err(|e| format!("Failed to insert usage event: {e}"))?;

    Ok(())
}

/// Get usage events with optional filters
#[tauri::command]
pub fn get_usage_events(
    category: Option<String>,
    since: Option<String>,
    limit: Option<i32>,
) -> Result<Vec<UsageEvent>, String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;
    let limit = limit.unwrap_or(100);

    let query = match (&category, &since) {
        (Some(_), Some(_)) => {
            "SELECT event_name, category, timestamp, properties
             FROM usage_events
             WHERE category = ?1 AND timestamp > ?2
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
        (Some(_), None) => {
            "SELECT event_name, category, timestamp, properties
             FROM usage_events
             WHERE category = ?1
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
        (None, Some(_)) => {
            "SELECT event_name, category, timestamp, properties
             FROM usage_events
             WHERE timestamp > ?2
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
        (None, None) => {
            "SELECT event_name, category, timestamp, properties
             FROM usage_events
             ORDER BY timestamp DESC
             LIMIT ?3"
        }
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let events = match (&category, &since) {
        (Some(cat), Some(since_time)) => stmt
            .query_map(params![cat, since_time, limit], map_usage_row)
            .map_err(|e| format!("Failed to query events: {e}"))?,
        (Some(cat), None) => stmt
            .query_map(params![cat, "", limit], map_usage_row)
            .map_err(|e| format!("Failed to query events: {e}"))?,
        (None, Some(since_time)) => stmt
            .query_map(params!["", since_time, limit], map_usage_row)
            .map_err(|e| format!("Failed to query events: {e}"))?,
        (None, None) => stmt
            .query_map(params!["", "", limit], map_usage_row)
            .map_err(|e| format!("Failed to query events: {e}"))?,
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to collect events: {e}"))?;

    Ok(events)
}

fn map_usage_row(row: &rusqlite::Row) -> rusqlite::Result<UsageEvent> {
    let properties_str: Option<String> = row.get(3)?;
    let properties = properties_str.and_then(|s| serde_json::from_str(&s).ok());

    Ok(UsageEvent {
        event_name: row.get(0)?,
        category: row.get(1)?,
        timestamp: row.get(2)?,
        properties,
    })
}

/// Get usage statistics grouped by event name
#[tauri::command]
pub fn get_usage_stats(since: Option<String>) -> Result<Vec<UsageStats>, String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;

    let query = if since.is_some() {
        "SELECT
            event_name,
            category,
            COUNT(*) as count,
            MAX(timestamp) as last_occurrence
         FROM usage_events
         WHERE timestamp > ?1
         GROUP BY event_name, category
         ORDER BY count DESC"
    } else {
        "SELECT
            event_name,
            category,
            COUNT(*) as count,
            MAX(timestamp) as last_occurrence
         FROM usage_events
         GROUP BY event_name, category
         ORDER BY count DESC"
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let stats = if let Some(since_time) = since {
        stmt.query_map([since_time], |row| {
            Ok(UsageStats {
                event_name: row.get(0)?,
                category: row.get(1)?,
                count: row.get(2)?,
                last_occurrence: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to query stats: {e}"))?
    } else {
        stmt.query_map([], |row| {
            Ok(UsageStats {
                event_name: row.get(0)?,
                category: row.get(1)?,
                count: row.get(2)?,
                last_occurrence: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to query stats: {e}"))?
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to collect stats: {e}"))?;

    Ok(stats)
}

// ============================================================================
// Data Management Commands
// ============================================================================

/// Delete all telemetry data
#[tauri::command]
pub fn delete_telemetry_data() -> Result<(), String> {
    let conn =
        open_telemetry_db().map_err(|e| format!("Failed to open telemetry database: {e}"))?;

    conn.execute_batch(
        "DELETE FROM performance_metrics;
         DELETE FROM error_reports;
         DELETE FROM usage_events;",
    )
    .map_err(|e| format!("Failed to delete telemetry data: {e}"))?;

    Ok(())
}
