//! Loom Analytics API Server
//!
//! Provides REST endpoints for external access to Loom analytics data.
//! Designed for integration with tools like Grafana, custom dashboards, and CI/CD pipelines.
//!
//! # Usage
//!
//! ```bash
//! # Start the API server (default port 9999)
//! loom-api --workspace /path/to/workspace
//!
//! # Custom port
//! loom-api --workspace /path/to/workspace --port 8080
//! ```
//!
//! # Endpoints
//!
//! - `GET /api/v1/health` - Health check
//! - `GET /api/v1/metrics/summary` - Overall agent metrics
//! - `GET /api/v1/metrics/velocity` - Velocity summary with trends
//! - `GET /api/v1/metrics/roles` - Metrics broken down by role
//! - `GET /api/v1/patterns` - Prompt patterns catalog
//! - `GET /api/v1/recommendations` - Active recommendations

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

// ============================================================================
// Application State
// ============================================================================

#[derive(Clone)]
struct AppState {
    workspace_path: PathBuf,
    #[allow(dead_code)]
    db: Arc<Mutex<Option<Connection>>>,
}

impl AppState {
    fn new(workspace_path: PathBuf) -> Self {
        Self {
            workspace_path,
            db: Arc::new(Mutex::new(None)),
        }
    }

    fn get_connection(&self) -> Result<Connection, ApiError> {
        let db_path = self.workspace_path.join(".loom").join("activity.db");
        if !db_path.exists() {
            return Err(ApiError::NotFound(
                "Activity database not found. Ensure Loom has been initialized.".to_string(),
            ));
        }
        Connection::open(&db_path).map_err(|e| ApiError::Internal(format!("Database error: {e}")))
    }
}

// ============================================================================
// API Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct HealthResponse {
    status: String,
    version: String,
    workspace: String,
    database_available: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentMetrics {
    prompt_count: i64,
    total_tokens: i64,
    total_cost: f64,
    success_rate: f64,
    prs_created: i64,
    issues_closed: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RoleMetrics {
    role: String,
    prompt_count: i64,
    total_tokens: i64,
    total_cost: f64,
    success_rate: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct VelocitySummary {
    issues_closed: i64,
    prs_merged: i64,
    avg_cycle_time_hours: Option<f64>,
    total_prompts: i64,
    total_cost_usd: f64,
    prev_issues_closed: i64,
    prev_prs_merged: i64,
    prev_avg_cycle_time_hours: Option<f64>,
    issues_trend: String,
    prs_trend: String,
    cycle_time_trend: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PromptPattern {
    id: i64,
    pattern_hash: String,
    category: String,
    trigger_type: String,
    role: String,
    occurrence_count: i64,
    avg_tokens: f64,
    avg_duration_ms: f64,
    success_rate: f64,
    avg_cost: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
struct Recommendation {
    id: i64,
    recommendation_type: String,
    title: String,
    description: String,
    priority: i64,
    context_role: Option<String>,
    context_task_type: Option<String>,
    evidence: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct TimeRangeQuery {
    #[serde(default = "default_time_range")]
    time_range: String,
}

fn default_time_range() -> String {
    "week".to_string()
}

#[derive(Debug, Deserialize)]
struct PatternQuery {
    category: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

// ============================================================================
// Error Handling
// ============================================================================

#[derive(Debug)]
enum ApiError {
    NotFound(String),
    Internal(String),
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let body = serde_json::json!({
            "error": message
        });

        (status, Json(body)).into_response()
    }
}

// ============================================================================
// Route Handlers
// ============================================================================

async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let db_path = state.workspace_path.join(".loom").join("activity.db");
    let database_available = db_path.exists();

    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: state.workspace_path.display().to_string(),
        database_available,
    })
}

async fn get_metrics_summary(
    State(state): State<AppState>,
    Query(params): Query<TimeRangeQuery>,
) -> Result<Json<AgentMetrics>, ApiError> {
    let conn = state.get_connection()?;
    let time_range = &params.time_range;

    // Calculate date filter based on time range
    let date_filter = match time_range.as_str() {
        "today" => "date(timestamp) = date('now')",
        "week" => "date(timestamp) >= date('now', '-7 days')",
        "month" => "date(timestamp) >= date('now', '-30 days')",
        _ => "1=1", // all time
    };

    // Query agent activity metrics
    let sql = format!(
        r"
        SELECT
            COUNT(*) as prompt_count,
            COALESCE(SUM(COALESCE(total_tokens, 0)), 0) as total_tokens,
            COALESCE(SUM(COALESCE(total_tokens, 0) * 0.00001), 0) as total_cost,
            CASE WHEN COUNT(*) > 0
                THEN CAST(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
                ELSE 0
            END as success_rate
        FROM agent_activity
        WHERE {date_filter}
        "
    );

    let (prompt_count, total_tokens, total_cost, success_rate): (i64, i64, f64, f64) = conn
        .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .map_err(|e| ApiError::Internal(format!("Query failed: {e}")))?;

    // Query GitHub events for PRs and issues
    let events_sql = format!(
        r"
        SELECT
            COALESCE(SUM(CASE WHEN event_type = 'pr_created' THEN 1 ELSE 0 END), 0) as prs_created,
            COALESCE(SUM(CASE WHEN event_type = 'issue_closed' THEN 1 ELSE 0 END), 0) as issues_closed
        FROM github_events
        WHERE {date_filter}
        "
    );

    let (prs_created, issues_closed): (i64, i64) = conn
        .query_row(&events_sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap_or((0, 0));

    Ok(Json(AgentMetrics {
        prompt_count,
        total_tokens,
        total_cost,
        success_rate,
        prs_created,
        issues_closed,
    }))
}

async fn get_metrics_by_role(
    State(state): State<AppState>,
    Query(params): Query<TimeRangeQuery>,
) -> Result<Json<Vec<RoleMetrics>>, ApiError> {
    let conn = state.get_connection()?;
    let time_range = &params.time_range;

    let date_filter = match time_range.as_str() {
        "today" => "date(timestamp) = date('now')",
        "week" => "date(timestamp) >= date('now', '-7 days')",
        "month" => "date(timestamp) >= date('now', '-30 days')",
        _ => "1=1",
    };

    let sql = format!(
        r"
        SELECT
            role,
            COUNT(*) as prompt_count,
            COALESCE(SUM(COALESCE(total_tokens, 0)), 0) as total_tokens,
            COALESCE(SUM(COALESCE(total_tokens, 0) * 0.00001), 0) as total_cost,
            CASE WHEN COUNT(*) > 0
                THEN CAST(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
                ELSE 0
            END as success_rate
        FROM agent_activity
        WHERE {date_filter}
        GROUP BY role
        ORDER BY prompt_count DESC
        "
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ApiError::Internal(format!("Query prepare failed: {e}")))?;

    let metrics = stmt
        .query_map([], |row| {
            Ok(RoleMetrics {
                role: row.get(0)?,
                prompt_count: row.get(1)?,
                total_tokens: row.get(2)?,
                total_cost: row.get(3)?,
                success_rate: row.get(4)?,
            })
        })
        .map_err(|e| ApiError::Internal(format!("Query failed: {e}")))?
        .filter_map(Result::ok)
        .collect();

    Ok(Json(metrics))
}

async fn get_velocity_summary(
    State(state): State<AppState>,
) -> Result<Json<VelocitySummary>, ApiError> {
    let conn = state.get_connection()?;

    // Check if velocity_snapshots table exists
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='velocity_snapshots'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(Json(VelocitySummary {
            issues_closed: 0,
            prs_merged: 0,
            avg_cycle_time_hours: None,
            total_prompts: 0,
            total_cost_usd: 0.0,
            prev_issues_closed: 0,
            prev_prs_merged: 0,
            prev_avg_cycle_time_hours: None,
            issues_trend: "stable".to_string(),
            prs_trend: "stable".to_string(),
            cycle_time_trend: "stable".to_string(),
        }));
    }

    // Get current week metrics
    let current_sql = r"
        SELECT
            COALESCE(SUM(issues_closed), 0),
            COALESCE(SUM(prs_merged), 0),
            AVG(avg_cycle_time_hours),
            COALESCE(SUM(total_prompts), 0),
            COALESCE(SUM(total_cost_usd), 0)
        FROM velocity_snapshots
        WHERE snapshot_date >= date('now', '-7 days')
    ";

    let (issues_closed, prs_merged, avg_cycle_time_hours, total_prompts, total_cost_usd): (
        i64,
        i64,
        Option<f64>,
        i64,
        f64,
    ) = conn
        .query_row(current_sql, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap_or((0, 0, None, 0, 0.0));

    // Get previous week metrics for comparison
    let prev_sql = r"
        SELECT
            COALESCE(SUM(issues_closed), 0),
            COALESCE(SUM(prs_merged), 0),
            AVG(avg_cycle_time_hours)
        FROM velocity_snapshots
        WHERE snapshot_date >= date('now', '-14 days')
          AND snapshot_date < date('now', '-7 days')
    ";

    let (prev_issues_closed, prev_prs_merged, prev_avg_cycle_time_hours): (i64, i64, Option<f64>) =
        conn.query_row(prev_sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap_or((0, 0, None));

    // Calculate trends
    let issues_trend = calculate_trend(prev_issues_closed, issues_closed);
    let prs_trend = calculate_trend(prev_prs_merged, prs_merged);
    let cycle_time_trend = match (prev_avg_cycle_time_hours, avg_cycle_time_hours) {
        (Some(prev), Some(curr)) => {
            // For cycle time, lower is better
            if curr < prev * 0.9 {
                "improving"
            } else if curr > prev * 1.1 {
                "declining"
            } else {
                "stable"
            }
        }
        _ => "stable",
    }
    .to_string();

    Ok(Json(VelocitySummary {
        issues_closed,
        prs_merged,
        avg_cycle_time_hours,
        total_prompts,
        total_cost_usd,
        prev_issues_closed,
        prev_prs_merged,
        prev_avg_cycle_time_hours,
        issues_trend,
        prs_trend,
        cycle_time_trend,
    }))
}

#[allow(clippy::cast_precision_loss)]
fn calculate_trend(prev: i64, curr: i64) -> String {
    if prev == 0 && curr == 0 {
        "stable".to_string()
    } else if prev == 0 {
        "improving".to_string()
    } else {
        let pct_change = (curr as f64 - prev as f64) / prev as f64;
        if pct_change > 0.1 {
            "improving".to_string()
        } else if pct_change < -0.1 {
            "declining".to_string()
        } else {
            "stable".to_string()
        }
    }
}

async fn get_patterns(
    State(state): State<AppState>,
    Query(params): Query<PatternQuery>,
) -> Result<Json<Vec<PromptPattern>>, ApiError> {
    let conn = state.get_connection()?;

    // Check if prompt_patterns table exists
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='prompt_patterns'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(Json(vec![]));
    }

    let (sql, bindings): (String, Vec<Box<dyn rusqlite::ToSql>>) = match &params.category {
        Some(cat) => (
            format!(
                r"
                SELECT id, pattern_hash, category, trigger_type, role,
                       occurrence_count, avg_tokens, avg_duration_ms, success_rate, avg_cost
                FROM prompt_patterns
                WHERE category = ?1
                ORDER BY occurrence_count DESC
                LIMIT {limit}
                ",
                limit = params.limit
            ),
            vec![Box::new(cat.clone())],
        ),
        None => (
            format!(
                r"
                SELECT id, pattern_hash, category, trigger_type, role,
                       occurrence_count, avg_tokens, avg_duration_ms, success_rate, avg_cost
                FROM prompt_patterns
                ORDER BY occurrence_count DESC
                LIMIT {limit}
                ",
                limit = params.limit
            ),
            vec![],
        ),
    };

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ApiError::Internal(format!("Query prepare failed: {e}")))?;

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        bindings.iter().map(std::convert::AsRef::as_ref).collect();

    let patterns = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_hash: row.get(1)?,
                category: row.get(2)?,
                trigger_type: row.get(3)?,
                role: row.get(4)?,
                occurrence_count: row.get(5)?,
                avg_tokens: row.get(6)?,
                avg_duration_ms: row.get(7)?,
                success_rate: row.get(8)?,
                avg_cost: row.get(9)?,
            })
        })
        .map_err(|e| ApiError::Internal(format!("Query failed: {e}")))?
        .filter_map(Result::ok)
        .collect();

    Ok(Json(patterns))
}

async fn get_recommendations(
    State(state): State<AppState>,
) -> Result<Json<Vec<Recommendation>>, ApiError> {
    let conn = state.get_connection()?;

    // Check if recommendations table exists
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='recommendations'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(Json(vec![]));
    }

    let sql = r"
        SELECT id, recommendation_type, title, description, priority,
               context_role, context_task_type, evidence, created_at
        FROM recommendations
        WHERE dismissed_at IS NULL
        ORDER BY priority DESC, created_at DESC
        LIMIT 50
    ";

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| ApiError::Internal(format!("Query prepare failed: {e}")))?;

    let recommendations = stmt
        .query_map([], |row| {
            Ok(Recommendation {
                id: row.get(0)?,
                recommendation_type: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                priority: row.get(4)?,
                context_role: row.get(5)?,
                context_task_type: row.get(6)?,
                evidence: row.get(7)?,
                created_at: row.get(8)?,
            })
        })
        .map_err(|e| ApiError::Internal(format!("Query failed: {e}")))?
        .filter_map(Result::ok)
        .collect();

    Ok(Json(recommendations))
}

// ============================================================================
// Router Setup
// ============================================================================

fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/metrics/summary", get(get_metrics_summary))
        .route("/api/v1/metrics/velocity", get(get_velocity_summary))
        .route("/api/v1/metrics/roles", get(get_metrics_by_role))
        .route("/api/v1/patterns", get(get_patterns))
        .route("/api/v1/recommendations", get(get_recommendations))
        .layer(cors)
        .with_state(state)
}

// ============================================================================
// CLI Arguments
// ============================================================================

#[derive(Debug)]
struct Args {
    workspace: PathBuf,
    port: u16,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let mut workspace: Option<PathBuf> = None;
    let mut port: u16 = 9999;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                workspace = args.next().map(PathBuf::from);
            }
            "--port" | "-p" => {
                if let Some(p) = args.next() {
                    port = p.parse().unwrap_or(9999);
                }
            }
            "--help" | "-h" => {
                println!(
                    r"Loom Analytics API Server

USAGE:
    loom-api --workspace <PATH> [OPTIONS]

OPTIONS:
    -w, --workspace <PATH>    Path to Loom workspace (required)
    -p, --port <PORT>         Port to listen on (default: 9999)
    -h, --help                Show this help message

ENDPOINTS:
    GET /api/v1/health              Health check
    GET /api/v1/metrics/summary     Overall agent metrics
    GET /api/v1/metrics/velocity    Velocity summary with trends
    GET /api/v1/metrics/roles       Metrics broken down by role
    GET /api/v1/patterns            Prompt patterns catalog
    GET /api/v1/recommendations     Active recommendations

QUERY PARAMETERS:
    time_range    Filter metrics by time range: today, week, month, all (default: week)
    category      Filter patterns by category
    limit         Limit number of results (default: 50)
"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    Ok(Args {
        workspace: workspace.ok_or_else(|| anyhow::anyhow!("--workspace is required"))?,
        port,
    })
}

// ============================================================================
// Main Entry Point
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;

    // Validate workspace exists
    if !args.workspace.exists() {
        anyhow::bail!("Workspace path does not exist: {}", args.workspace.display());
    }

    let state = AppState::new(args.workspace.clone());
    let app = create_router(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    info!(
        "Starting Loom API server on http://{} for workspace {}",
        addr,
        args.workspace.display()
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn setup_test_db(dir: &TempDir) -> Connection {
        let loom_dir = dir.path().join(".loom");
        std::fs::create_dir_all(&loom_dir).unwrap();
        let db_path = loom_dir.join("activity.db");
        let conn = Connection::open(&db_path).unwrap();

        // Create minimal schema
        conn.execute(
            "CREATE TABLE agent_activity (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                role TEXT NOT NULL,
                outcome TEXT NOT NULL,
                total_tokens INTEGER
            )",
            [],
        )
        .unwrap();

        conn.execute(
            "CREATE TABLE github_events (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                event_type TEXT NOT NULL
            )",
            [],
        )
        .unwrap();

        // Insert test data
        conn.execute(
            "INSERT INTO agent_activity (timestamp, role, outcome, total_tokens)
             VALUES (datetime('now'), 'builder', 'success', 1000)",
            [],
        )
        .unwrap();

        conn
    }

    #[tokio::test]
    async fn test_health_check() {
        let dir = TempDir::new().unwrap();
        setup_test_db(&dir);

        let state = AppState::new(dir.path().to_path_buf());
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_metrics_summary() {
        let dir = TempDir::new().unwrap();
        setup_test_db(&dir);

        let state = AppState::new(dir.path().to_path_buf());
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/metrics/summary?time_range=all")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_missing_database() {
        let dir = TempDir::new().unwrap();
        // Don't create database

        let state = AppState::new(dir.path().to_path_buf());
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/metrics/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
