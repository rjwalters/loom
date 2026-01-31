//! A/B Testing Module
//!
//! Provides controlled experiments for comparing different approaches
//! (prompts, roles, configurations) with statistical rigor.
//!
//! Part of Phase 4 (Advanced Analytics) - builds on velocity tracking (#1065)
//! and correlation analysis (#1066).
//!
//! @see Issue #1071 - Add A/B testing framework for approaches

use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ============================================================================
// Types
// ============================================================================

/// Experiment status
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExperimentStatus {
    Draft,
    Active,
    Concluded,
    Cancelled,
}

impl From<&str> for ExperimentStatus {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "active" => ExperimentStatus::Active,
            "concluded" => ExperimentStatus::Concluded,
            "cancelled" => ExperimentStatus::Cancelled,
            _ => ExperimentStatus::Draft,
        }
    }
}

impl std::fmt::Display for ExperimentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExperimentStatus::Draft => write!(f, "draft"),
            ExperimentStatus::Active => write!(f, "active"),
            ExperimentStatus::Concluded => write!(f, "concluded"),
            ExperimentStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Target metric for optimization
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetMetric {
    SuccessRate,
    CycleTime,
    Cost,
}

impl From<&str> for TargetMetric {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "cycle_time" => TargetMetric::CycleTime,
            "cost" => TargetMetric::Cost,
            _ => TargetMetric::SuccessRate,
        }
    }
}

impl std::fmt::Display for TargetMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetMetric::SuccessRate => write!(f, "success_rate"),
            TargetMetric::CycleTime => write!(f, "cycle_time"),
            TargetMetric::Cost => write!(f, "cost"),
        }
    }
}

/// Direction of improvement
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetDirection {
    Higher,
    Lower,
}

impl From<&str> for TargetDirection {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "lower" => TargetDirection::Lower,
            _ => TargetDirection::Higher,
        }
    }
}

impl std::fmt::Display for TargetDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetDirection::Higher => write!(f, "higher"),
            TargetDirection::Lower => write!(f, "lower"),
        }
    }
}

/// Experiment definition
#[derive(Debug, Serialize, Deserialize)]
pub struct Experiment {
    pub id: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub hypothesis: Option<String>,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub concluded_at: Option<String>,
    pub min_sample_size: i32,
    pub target_metric: String,
    pub target_direction: String,
}

/// Variant within an experiment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub id: Option<i64>,
    pub experiment_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub config_json: Option<String>,
    pub weight: f64,
}

/// Assignment of issue/task to variant
#[derive(Debug, Serialize, Deserialize)]
pub struct Assignment {
    pub id: Option<i64>,
    pub experiment_id: i64,
    pub variant_id: i64,
    pub issue_number: Option<i64>,
    pub terminal_id: Option<String>,
    pub assigned_at: String,
}

/// Result for an assignment
#[derive(Debug, Serialize, Deserialize)]
pub struct ExperimentResult {
    pub id: Option<i64>,
    pub assignment_id: i64,
    pub success_factor_id: Option<i64>,
    pub outcome: String,
    pub metric_value: Option<f64>,
    pub recorded_at: String,
}

/// Statistics for a variant
#[derive(Debug, Serialize, Deserialize)]
pub struct VariantStats {
    pub variant_name: String,
    pub variant_id: i64,
    pub sample_size: i32,
    pub success_rate: f64,
    pub avg_metric_value: Option<f64>,
    pub std_dev: Option<f64>,
    pub ci_lower: Option<f64>,
    pub ci_upper: Option<f64>,
}

/// Analysis result for an experiment
#[derive(Debug, Serialize, Deserialize)]
pub struct ExperimentAnalysis {
    pub experiment_id: i64,
    pub winner: Option<String>,
    pub winner_variant_id: Option<i64>,
    pub confidence: f64,
    pub p_value: f64,
    pub effect_size: f64,
    pub stats_per_variant: Vec<VariantStats>,
    pub recommendation: String,
    pub should_conclude: bool,
    pub analysis_date: String,
}

/// Summary of all experiments
#[derive(Debug, Serialize, Deserialize)]
pub struct ExperimentsSummary {
    pub total_experiments: i32,
    pub active_experiments: i32,
    pub concluded_experiments: i32,
    pub total_assignments: i32,
    pub total_results: i32,
}

// ============================================================================
// Database Setup
// ============================================================================

/// Open connection to activity database and ensure A/B testing schema exists
fn open_ab_testing_db(workspace_path: &str) -> SqliteResult<Connection> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let db_path = loom_dir.join("activity.db");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        std::fs::create_dir_all(&loom_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }

    let conn = Connection::open(&db_path)?;

    // Create A/B testing tables
    conn.execute_batch(
        r"
        -- Experiment definitions
        CREATE TABLE IF NOT EXISTS experiments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            description TEXT,
            hypothesis TEXT,
            status TEXT NOT NULL DEFAULT 'draft',
            created_at DATETIME DEFAULT (datetime('now')),
            started_at DATETIME,
            concluded_at DATETIME,
            min_sample_size INTEGER DEFAULT 20,
            target_metric TEXT NOT NULL DEFAULT 'success_rate',
            target_direction TEXT NOT NULL DEFAULT 'higher'
        );

        CREATE INDEX IF NOT EXISTS idx_experiments_status ON experiments(status);
        CREATE INDEX IF NOT EXISTS idx_experiments_name ON experiments(name);

        -- Experiment variants
        CREATE TABLE IF NOT EXISTS experiment_variants (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            experiment_id INTEGER NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            description TEXT,
            config_json TEXT,
            weight REAL DEFAULT 1.0,
            UNIQUE(experiment_id, name)
        );

        CREATE INDEX IF NOT EXISTS idx_variants_experiment ON experiment_variants(experiment_id);

        -- Assignment tracking
        CREATE TABLE IF NOT EXISTS experiment_assignments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            experiment_id INTEGER NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            variant_id INTEGER NOT NULL REFERENCES experiment_variants(id) ON DELETE CASCADE,
            issue_number INTEGER,
            terminal_id TEXT,
            assigned_at DATETIME DEFAULT (datetime('now')),
            UNIQUE(experiment_id, issue_number)
        );

        CREATE INDEX IF NOT EXISTS idx_assignments_experiment ON experiment_assignments(experiment_id);
        CREATE INDEX IF NOT EXISTS idx_assignments_variant ON experiment_assignments(variant_id);
        CREATE INDEX IF NOT EXISTS idx_assignments_issue ON experiment_assignments(issue_number);

        -- Results tracking
        CREATE TABLE IF NOT EXISTS experiment_results (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            assignment_id INTEGER NOT NULL REFERENCES experiment_assignments(id) ON DELETE CASCADE,
            success_factor_id INTEGER,
            outcome TEXT NOT NULL,
            metric_value REAL,
            recorded_at DATETIME DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_results_assignment ON experiment_results(assignment_id);
        CREATE INDEX IF NOT EXISTS idx_results_outcome ON experiment_results(outcome);

        -- Analysis results (cached)
        CREATE TABLE IF NOT EXISTS experiment_analysis (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            experiment_id INTEGER NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            analysis_date DATETIME DEFAULT (datetime('now')),
            winner_variant_id INTEGER REFERENCES experiment_variants(id),
            confidence REAL,
            p_value REAL,
            effect_size REAL,
            sample_size_json TEXT,
            recommendation TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_analysis_experiment ON experiment_analysis(experiment_id);
        ",
    )?;

    Ok(conn)
}

// ============================================================================
// Statistical Functions
// ============================================================================

/// Calculate chi-square test for success rates
fn chi_square_test(successes: &[i32], totals: &[i32]) -> (f64, f64) {
    if successes.len() != totals.len() || successes.len() < 2 {
        return (0.0, 1.0);
    }

    let total_success: i32 = successes.iter().sum();
    let total_n: i32 = totals.iter().sum();

    if total_n == 0 {
        return (0.0, 1.0);
    }

    let expected_rate = f64::from(total_success) / f64::from(total_n);
    let mut chi_sq = 0.0;

    for (i, &n) in totals.iter().enumerate() {
        if n > 0 {
            let expected_success = f64::from(n) * expected_rate;
            let expected_failure = f64::from(n) * (1.0 - expected_rate);

            if expected_success > 0.0 {
                let diff = f64::from(successes[i]) - expected_success;
                chi_sq += (diff * diff) / expected_success;
            }
            if expected_failure > 0.0 {
                let failures = n - successes[i];
                let diff = f64::from(failures) - expected_failure;
                chi_sq += (diff * diff) / expected_failure;
            }
        }
    }

    // Degrees of freedom = (rows - 1) = 1 for 2x2 contingency
    #[allow(clippy::cast_precision_loss)]
    let df = (successes.len() - 1) as f64;
    let p_value = chi_square_p_value(chi_sq, df);

    (chi_sq, p_value)
}

/// Approximate p-value for chi-square distribution
fn chi_square_p_value(chi_sq: f64, df: f64) -> f64 {
    if chi_sq <= 0.0 {
        return 1.0;
    }

    // Use Wilson-Hilferty approximation for p-value
    let z = ((chi_sq / df).powf(1.0 / 3.0) - (1.0 - 2.0 / (9.0 * df))) / (2.0 / (9.0 * df)).sqrt();

    // Convert z to p-value using normal CDF
    1.0 - normal_cdf(z)
}

/// Normal cumulative distribution function
fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Error function approximation
fn erf(x: f64) -> f64 {
    let a1 = 0.254_829_592;
    let a2 = -0.284_496_736;
    let a3 = 1.421_413_741;
    let a4 = -1.453_152_027;
    let a5 = 1.061_405_429;
    let p = 0.327_591_1;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();

    sign * y
}

/// Calculate two-sample t-test for continuous metrics
fn t_test(means: &[f64], std_devs: &[f64], sizes: &[i32]) -> (f64, f64) {
    if means.len() != 2 || std_devs.len() != 2 || sizes.len() != 2 {
        return (0.0, 1.0);
    }

    let n1 = f64::from(sizes[0]);
    let n2 = f64::from(sizes[1]);

    if n1 < 2.0 || n2 < 2.0 {
        return (0.0, 1.0);
    }

    let mean_diff = means[0] - means[1];
    let var1 = std_devs[0] * std_devs[0];
    let var2 = std_devs[1] * std_devs[1];

    let pooled_se = (var1 / n1 + var2 / n2).sqrt();

    if pooled_se == 0.0 {
        return (0.0, 1.0);
    }

    let t = mean_diff / pooled_se;

    // Welch-Satterthwaite degrees of freedom
    let _df = ((var1 / n1 + var2 / n2).powi(2))
        / ((var1 / n1).powi(2) / (n1 - 1.0) + (var2 / n2).powi(2) / (n2 - 1.0));

    // Approximate p-value using normal distribution for large samples
    let p_value = 2.0 * (1.0 - normal_cdf(t.abs()));

    (t, p_value.clamp(0.0, 1.0))
}

/// Calculate Cohen's d effect size
fn cohens_d(mean1: f64, mean2: f64, std1: f64, std2: f64, n1: i32, n2: i32) -> f64 {
    let pooled_std = ((f64::from(n1 - 1) * std1 * std1 + f64::from(n2 - 1) * std2 * std2)
        / f64::from(n1 + n2 - 2))
    .sqrt();

    if pooled_std == 0.0 {
        return 0.0;
    }

    (mean1 - mean2) / pooled_std
}

/// Calculate 95% confidence interval for a proportion
fn proportion_ci(successes: i32, total: i32) -> (f64, f64) {
    if total == 0 {
        return (0.0, 1.0);
    }

    let p = f64::from(successes) / f64::from(total);
    let z = 1.96; // 95% confidence
    let se = (p * (1.0 - p) / f64::from(total)).sqrt();

    ((p - z * se).max(0.0), (p + z * se).min(1.0))
}

// ============================================================================
// Experiment Lifecycle Commands
// ============================================================================

/// Create a new experiment
#[tauri::command]
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub fn create_experiment(
    workspace_path: &str,
    name: &str,
    description: Option<String>,
    hypothesis: Option<String>,
    status: Option<String>,
    min_sample_size: Option<i32>,
    target_metric: &str,
    target_direction: &str,
) -> Result<i64, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let status = status.unwrap_or_else(|| "draft".to_string());
    let min_sample_size = min_sample_size.unwrap_or(20);

    conn.execute(
        "INSERT INTO experiments (name, description, hypothesis, status, min_sample_size, target_metric, target_direction)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            name,
            description,
            hypothesis,
            status,
            min_sample_size,
            target_metric,
            target_direction
        ],
    )
    .map_err(|e| format!("Failed to create experiment: {e}"))?;

    Ok(conn.last_insert_rowid())
}

/// Add a variant to an experiment
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn add_experiment_variant(
    workspace_path: &str,
    experiment_id: i64,
    name: &str,
    description: Option<String>,
    config_json: Option<String>,
    weight: Option<f64>,
) -> Result<i64, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let weight = weight.unwrap_or(1.0);

    conn.execute(
        "INSERT INTO experiment_variants (experiment_id, name, description, config_json, weight)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![experiment_id, name, description, config_json, weight],
    )
    .map_err(|e| format!("Failed to add variant: {e}"))?;

    Ok(conn.last_insert_rowid())
}

/// Start an experiment (change status from draft to active)
#[tauri::command]
pub fn start_experiment(workspace_path: &str, experiment_id: i64) -> Result<(), String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Verify experiment has at least 2 variants
    let variant_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM experiment_variants WHERE experiment_id = ?1",
            [experiment_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count variants: {e}"))?;

    if variant_count < 2 {
        return Err("Experiment must have at least 2 variants before starting".to_string());
    }

    conn.execute(
        "UPDATE experiments SET status = 'active', started_at = datetime('now') WHERE id = ?1 AND status = 'draft'",
        [experiment_id],
    )
    .map_err(|e| format!("Failed to start experiment: {e}"))?;

    Ok(())
}

/// Conclude an experiment
#[tauri::command]
pub fn conclude_experiment(
    workspace_path: &str,
    experiment_id: i64,
    winner_variant_id: Option<i64>,
) -> Result<(), String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE experiments SET status = 'concluded', concluded_at = datetime('now') WHERE id = ?1",
        [experiment_id],
    )
    .map_err(|e| format!("Failed to conclude experiment: {e}"))?;

    // Record final analysis
    if winner_variant_id.is_some() {
        let analysis = analyze_experiment(workspace_path, experiment_id)?;
        conn.execute(
            "INSERT INTO experiment_analysis (experiment_id, winner_variant_id, confidence, p_value, effect_size, recommendation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                experiment_id,
                winner_variant_id,
                analysis.confidence,
                analysis.p_value,
                analysis.effect_size,
                analysis.recommendation
            ],
        )
        .map_err(|e| format!("Failed to record analysis: {e}"))?;
    }

    Ok(())
}

/// Cancel an experiment
#[tauri::command]
pub fn cancel_experiment(workspace_path: &str, experiment_id: i64) -> Result<(), String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE experiments SET status = 'cancelled', concluded_at = datetime('now') WHERE id = ?1",
        [experiment_id],
    )
    .map_err(|e| format!("Failed to cancel experiment: {e}"))?;

    Ok(())
}

// ============================================================================
// Assignment Commands
// ============================================================================

/// Assign a variant to an issue (or get existing assignment)
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn assign_experiment_variant(
    workspace_path: &str,
    experiment_name: &str,
    issue_number: Option<i64>,
    terminal_id: Option<String>,
) -> Result<Variant, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get experiment ID
    let experiment: Experiment = conn
        .query_row(
            "SELECT id, name, description, hypothesis, status, created_at, started_at, concluded_at,
                    min_sample_size, target_metric, target_direction
             FROM experiments WHERE name = ?1",
            [experiment_name],
            |row| {
                Ok(Experiment {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    hypothesis: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                    started_at: row.get(6)?,
                    concluded_at: row.get(7)?,
                    min_sample_size: row.get(8)?,
                    target_metric: row.get(9)?,
                    target_direction: row.get(10)?,
                })
            },
        )
        .map_err(|_| format!("Experiment '{experiment_name}' not found"))?;

    if experiment.status != "active" {
        return Err(format!(
            "Experiment '{experiment_name}' is not active (status: {})",
            experiment.status
        ));
    }

    let experiment_id = experiment.id.ok_or("Experiment has no ID")?;

    // Check for existing assignment
    if let Some(issue) = issue_number {
        if let Ok(existing) = conn.query_row(
            "SELECT v.id, v.experiment_id, v.name, v.description, v.config_json, v.weight
             FROM experiment_variants v
             JOIN experiment_assignments a ON v.id = a.variant_id
             WHERE a.experiment_id = ?1 AND a.issue_number = ?2",
            params![experiment_id, issue],
            |row| {
                Ok(Variant {
                    id: row.get(0)?,
                    experiment_id: row.get(1)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    config_json: row.get(4)?,
                    weight: row.get(5)?,
                })
            },
        ) {
            return Ok(existing);
        }
    }

    // Get all variants with weights
    let mut stmt = conn
        .prepare(
            "SELECT id, experiment_id, name, description, config_json, weight
             FROM experiment_variants WHERE experiment_id = ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let variants: Vec<Variant> = stmt
        .query_map([experiment_id], |row| {
            Ok(Variant {
                id: row.get(0)?,
                experiment_id: row.get(1)?,
                name: row.get(2)?,
                description: row.get(3)?,
                config_json: row.get(4)?,
                weight: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query variants: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect variants: {e}"))?;

    if variants.is_empty() {
        return Err("No variants defined for experiment".to_string());
    }

    // Weighted random selection
    let total_weight: f64 = variants.iter().map(|v| v.weight).sum();
    let rng_value = rand_simple();
    let mut cumulative = 0.0;

    let selected = variants
        .iter()
        .find(|v| {
            cumulative += v.weight / total_weight;
            rng_value < cumulative
        })
        .unwrap_or(&variants[0]);

    // Record assignment
    let variant_id = selected.id.ok_or("Variant has no ID")?;
    conn.execute(
        "INSERT INTO experiment_assignments (experiment_id, variant_id, issue_number, terminal_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![experiment_id, variant_id, issue_number, terminal_id],
    )
    .map_err(|e| format!("Failed to record assignment: {e}"))?;

    Ok(selected.clone())
}

/// Simple pseudo-random number generator (0.0 to 1.0)
#[allow(clippy::unwrap_used)]
fn rand_simple() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    (f64::from(nanos) / f64::from(u32::MAX)).fract()
}

/// Get assignment for an issue
#[tauri::command]
pub fn get_experiment_assignment(
    workspace_path: &str,
    experiment_name: &str,
    issue_number: i64,
) -> Result<Option<Assignment>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let result = conn.query_row(
        "SELECT a.id, a.experiment_id, a.variant_id, a.issue_number, a.terminal_id, a.assigned_at
         FROM experiment_assignments a
         JOIN experiments e ON a.experiment_id = e.id
         WHERE e.name = ?1 AND a.issue_number = ?2",
        params![experiment_name, issue_number],
        |row| {
            Ok(Assignment {
                id: row.get(0)?,
                experiment_id: row.get(1)?,
                variant_id: row.get(2)?,
                issue_number: row.get(3)?,
                terminal_id: row.get(4)?,
                assigned_at: row.get(5)?,
            })
        },
    );

    match result {
        Ok(assignment) => Ok(Some(assignment)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(format!("Failed to get assignment: {e}")),
    }
}

// ============================================================================
// Result Recording Commands
// ============================================================================

/// Record a result for an experiment assignment
#[tauri::command]
pub fn record_experiment_result(
    workspace_path: &str,
    experiment_name: &str,
    issue_number: i64,
    outcome: &str,
    metric_value: Option<f64>,
) -> Result<(), String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get assignment ID
    let assignment_id: i64 = conn
        .query_row(
            "SELECT a.id FROM experiment_assignments a
             JOIN experiments e ON a.experiment_id = e.id
             WHERE e.name = ?1 AND a.issue_number = ?2",
            params![experiment_name, issue_number],
            |row| row.get(0),
        )
        .map_err(|_| {
            format!(
                "No assignment found for issue {issue_number} in experiment '{experiment_name}'"
            )
        })?;

    conn.execute(
        "INSERT INTO experiment_results (assignment_id, outcome, metric_value)
         VALUES (?1, ?2, ?3)",
        params![assignment_id, outcome, metric_value],
    )
    .map_err(|e| format!("Failed to record result: {e}"))?;

    Ok(())
}

/// Record a result with a linked success factor
#[tauri::command]
pub fn record_experiment_result_with_factor(
    workspace_path: &str,
    experiment_name: &str,
    issue_number: i64,
    outcome: &str,
    success_factor_id: i64,
    metric_value: Option<f64>,
) -> Result<(), String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let assignment_id: i64 = conn
        .query_row(
            "SELECT a.id FROM experiment_assignments a
             JOIN experiments e ON a.experiment_id = e.id
             WHERE e.name = ?1 AND a.issue_number = ?2",
            params![experiment_name, issue_number],
            |row| row.get(0),
        )
        .map_err(|_| {
            format!(
                "No assignment found for issue {issue_number} in experiment '{experiment_name}'"
            )
        })?;

    conn.execute(
        "INSERT INTO experiment_results (assignment_id, success_factor_id, outcome, metric_value)
         VALUES (?1, ?2, ?3, ?4)",
        params![assignment_id, success_factor_id, outcome, metric_value],
    )
    .map_err(|e| format!("Failed to record result: {e}"))?;

    Ok(())
}

// ============================================================================
// Analysis Commands
// ============================================================================

/// Analyze an experiment and calculate statistical results
#[tauri::command]
#[allow(clippy::too_many_lines)]
pub fn analyze_experiment(
    workspace_path: &str,
    experiment_id: i64,
) -> Result<ExperimentAnalysis, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get experiment details
    let experiment: Experiment = conn
        .query_row(
            "SELECT id, name, description, hypothesis, status, created_at, started_at, concluded_at,
                    min_sample_size, target_metric, target_direction
             FROM experiments WHERE id = ?1",
            [experiment_id],
            |row| {
                Ok(Experiment {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    hypothesis: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                    started_at: row.get(6)?,
                    concluded_at: row.get(7)?,
                    min_sample_size: row.get(8)?,
                    target_metric: row.get(9)?,
                    target_direction: row.get(10)?,
                })
            },
        )
        .map_err(|e| format!("Failed to get experiment: {e}"))?;

    // Get stats per variant
    let mut stmt = conn
        .prepare(
            "SELECT
                v.id,
                v.name,
                COUNT(r.id) as sample_size,
                SUM(CASE WHEN r.outcome = 'success' THEN 1 ELSE 0 END) as successes,
                AVG(r.metric_value) as avg_metric,
                -- Calculate standard deviation manually
                SQRT(AVG(r.metric_value * r.metric_value) - AVG(r.metric_value) * AVG(r.metric_value)) as std_dev
             FROM experiment_variants v
             LEFT JOIN experiment_assignments a ON v.id = a.variant_id
             LEFT JOIN experiment_results r ON a.id = r.assignment_id
             WHERE v.experiment_id = ?1
             GROUP BY v.id, v.name",
        )
        .map_err(|e| format!("Failed to prepare stats query: {e}"))?;

    #[allow(clippy::type_complexity)]
    let stats: Vec<(i64, String, i32, i32, Option<f64>, Option<f64>)> = stmt
        .query_map([experiment_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
        })
        .map_err(|e| format!("Failed to query stats: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect stats: {e}"))?;

    // Build variant stats
    let mut variant_stats: Vec<VariantStats> = Vec::new();
    let mut successes_vec: Vec<i32> = Vec::new();
    let mut totals_vec: Vec<i32> = Vec::new();
    let mut means_vec: Vec<f64> = Vec::new();
    let mut std_devs_vec: Vec<f64> = Vec::new();
    let mut sizes_vec: Vec<i32> = Vec::new();

    for (id, name, sample_size, successes, avg_metric, std_dev) in &stats {
        let success_rate = if *sample_size > 0 {
            f64::from(*successes) / f64::from(*sample_size)
        } else {
            0.0
        };

        let (ci_lower, ci_upper) = proportion_ci(*successes, *sample_size);

        variant_stats.push(VariantStats {
            variant_name: name.clone(),
            variant_id: *id,
            sample_size: *sample_size,
            success_rate,
            avg_metric_value: *avg_metric,
            std_dev: *std_dev,
            ci_lower: Some(ci_lower),
            ci_upper: Some(ci_upper),
        });

        successes_vec.push(*successes);
        totals_vec.push(*sample_size);
        if let Some(mean) = avg_metric {
            means_vec.push(*mean);
        }
        if let Some(sd) = std_dev {
            std_devs_vec.push(*sd);
        }
        sizes_vec.push(*sample_size);
    }

    // Calculate p-value and effect size based on target metric
    let (p_value, effect_size) = if experiment.target_metric == "success_rate" {
        // Chi-square test for success rates
        let (_, p) = chi_square_test(&successes_vec, &totals_vec);

        // Effect size: odds ratio for binary outcomes
        let effect = if variant_stats.len() >= 2 {
            let rate1 = variant_stats[0].success_rate;
            let rate2 = variant_stats[1].success_rate;
            if rate2 > 0.0 && rate2 < 1.0 {
                let odds1 = rate1 / (1.0 - rate1).max(0.001);
                let odds2 = rate2 / (1.0 - rate2).max(0.001);
                (odds1 / odds2).ln()
            } else {
                0.0
            }
        } else {
            0.0
        };
        (p, effect)
    } else {
        // T-test for continuous metrics
        if means_vec.len() >= 2 && std_devs_vec.len() >= 2 {
            let (_, p) = t_test(&means_vec, &std_devs_vec, &sizes_vec);
            let effect = cohens_d(
                means_vec[0],
                means_vec[1],
                std_devs_vec[0],
                std_devs_vec[1],
                sizes_vec[0],
                sizes_vec[1],
            );
            (p, effect)
        } else {
            (1.0, 0.0)
        }
    };

    // Determine winner
    let (winner, winner_variant_id) = if p_value < 0.05 && !variant_stats.is_empty() {
        let best = if experiment.target_metric == "success_rate" {
            variant_stats.iter().max_by(|a, b| {
                a.success_rate
                    .partial_cmp(&b.success_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        } else if experiment.target_direction == "higher" {
            variant_stats.iter().max_by(|a, b| {
                a.avg_metric_value
                    .partial_cmp(&b.avg_metric_value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        } else {
            variant_stats.iter().min_by(|a, b| {
                a.avg_metric_value
                    .partial_cmp(&b.avg_metric_value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        };

        best.map_or((None, None), |v| (Some(v.variant_name.clone()), Some(v.variant_id)))
    } else {
        (None, None)
    };

    // Calculate confidence
    let confidence = 1.0 - p_value;

    // Determine if we should conclude
    let min_samples_met = variant_stats
        .iter()
        .all(|v| v.sample_size >= experiment.min_sample_size);
    let statistically_significant = p_value < 0.05;
    let should_conclude = min_samples_met && statistically_significant;

    // Generate recommendation
    let recommendation = if !min_samples_met {
        let min_needed = experiment.min_sample_size;
        let current_min = variant_stats
            .iter()
            .map(|v| v.sample_size)
            .min()
            .unwrap_or(0);
        format!(
            "Continue experiment: {} more samples needed per variant (current minimum: {})",
            min_needed - current_min,
            current_min
        )
    } else if statistically_significant {
        if let Some(ref w) = winner {
            format!(
                "Winner detected: {} (p={:.4}, confidence={:.1}%). Consider concluding the experiment.",
                w, p_value, confidence * 100.0
            )
        } else {
            "Results are significant but no clear winner. Review data manually.".to_string()
        }
    } else {
        format!(
            "No significant difference detected (p={p_value:.4}). Consider continuing or concluding as inconclusive."
        )
    };

    Ok(ExperimentAnalysis {
        experiment_id,
        winner,
        winner_variant_id,
        confidence,
        p_value,
        effect_size,
        stats_per_variant: variant_stats,
        recommendation,
        should_conclude,
        analysis_date: chrono::Utc::now().to_rfc3339(),
    })
}

/// Analyze experiment by name
#[tauri::command]
pub fn analyze_experiment_by_name(
    workspace_path: &str,
    experiment_name: &str,
) -> Result<ExperimentAnalysis, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let experiment_id: i64 = conn
        .query_row("SELECT id FROM experiments WHERE name = ?1", [experiment_name], |row| {
            row.get(0)
        })
        .map_err(|_| format!("Experiment '{experiment_name}' not found"))?;

    analyze_experiment(workspace_path, experiment_id)
}

/// Check which experiments should be concluded
#[tauri::command]
pub fn check_experiments_for_conclusion(workspace_path: &str) -> Result<Vec<i64>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare("SELECT id FROM experiments WHERE status = 'active'")
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let experiment_ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Failed to query experiments: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect IDs: {e}"))?;

    let mut should_conclude = Vec::new();

    for id in experiment_ids {
        if let Ok(analysis) = analyze_experiment(workspace_path, id) {
            if analysis.should_conclude {
                should_conclude.push(id);
            }
        }
    }

    Ok(should_conclude)
}

// ============================================================================
// Query Commands
// ============================================================================

/// Get experiments, optionally filtered by status
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_experiments(
    workspace_path: &str,
    status: Option<String>,
) -> Result<Vec<Experiment>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let query = if status.is_some() {
        "SELECT id, name, description, hypothesis, status, created_at, started_at, concluded_at,
                min_sample_size, target_metric, target_direction
         FROM experiments WHERE status = ?1 ORDER BY created_at DESC"
    } else {
        "SELECT id, name, description, hypothesis, status, created_at, started_at, concluded_at,
                min_sample_size, target_metric, target_direction
         FROM experiments ORDER BY created_at DESC"
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<Experiment> {
        Ok(Experiment {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            hypothesis: row.get(3)?,
            status: row.get(4)?,
            created_at: row.get(5)?,
            started_at: row.get(6)?,
            concluded_at: row.get(7)?,
            min_sample_size: row.get(8)?,
            target_metric: row.get(9)?,
            target_direction: row.get(10)?,
        })
    };

    if let Some(ref s) = status {
        stmt.query_map([s], map_row)
            .map_err(|e| format!("Failed to query experiments: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect experiments: {e}"))
    } else {
        stmt.query_map([], map_row)
            .map_err(|e| format!("Failed to query experiments: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect experiments: {e}"))
    }
}

/// Get a single experiment by ID
#[tauri::command]
pub fn get_experiment(
    workspace_path: &str,
    experiment_id: i64,
) -> Result<Option<Experiment>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let result = conn.query_row(
        "SELECT id, name, description, hypothesis, status, created_at, started_at, concluded_at,
                min_sample_size, target_metric, target_direction
         FROM experiments WHERE id = ?1",
        [experiment_id],
        |row| {
            Ok(Experiment {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                hypothesis: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                started_at: row.get(6)?,
                concluded_at: row.get(7)?,
                min_sample_size: row.get(8)?,
                target_metric: row.get(9)?,
                target_direction: row.get(10)?,
            })
        },
    );

    match result {
        Ok(exp) => Ok(Some(exp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(format!("Failed to get experiment: {e}")),
    }
}

/// Get experiment by name
#[tauri::command]
pub fn get_experiment_by_name(
    workspace_path: &str,
    name: &str,
) -> Result<Option<Experiment>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let result = conn.query_row(
        "SELECT id, name, description, hypothesis, status, created_at, started_at, concluded_at,
                min_sample_size, target_metric, target_direction
         FROM experiments WHERE name = ?1",
        [name],
        |row| {
            Ok(Experiment {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                hypothesis: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                started_at: row.get(6)?,
                concluded_at: row.get(7)?,
                min_sample_size: row.get(8)?,
                target_metric: row.get(9)?,
                target_direction: row.get(10)?,
            })
        },
    );

    match result {
        Ok(exp) => Ok(Some(exp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(format!("Failed to get experiment: {e}")),
    }
}

/// Get variants for an experiment
#[tauri::command]
pub fn get_experiment_variants(
    workspace_path: &str,
    experiment_id: i64,
) -> Result<Vec<Variant>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, experiment_id, name, description, config_json, weight
             FROM experiment_variants WHERE experiment_id = ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results: Vec<Variant> = stmt
        .query_map([experiment_id], |row| {
            Ok(Variant {
                id: row.get(0)?,
                experiment_id: row.get(1)?,
                name: row.get(2)?,
                description: row.get(3)?,
                config_json: row.get(4)?,
                weight: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query variants: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect variants: {e}"))?;

    Ok(results)
}

/// Get summary of all experiments
#[tauri::command]
pub fn get_experiments_summary(workspace_path: &str) -> Result<ExperimentsSummary, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let total_experiments: i32 = conn
        .query_row("SELECT COUNT(*) FROM experiments", [], |row| row.get(0))
        .unwrap_or(0);

    let active_experiments: i32 = conn
        .query_row("SELECT COUNT(*) FROM experiments WHERE status = 'active'", [], |row| row.get(0))
        .unwrap_or(0);

    let concluded_experiments: i32 = conn
        .query_row("SELECT COUNT(*) FROM experiments WHERE status = 'concluded'", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    let total_assignments: i32 = conn
        .query_row("SELECT COUNT(*) FROM experiment_assignments", [], |row| row.get(0))
        .unwrap_or(0);

    let total_results: i32 = conn
        .query_row("SELECT COUNT(*) FROM experiment_results", [], |row| row.get(0))
        .unwrap_or(0);

    Ok(ExperimentsSummary {
        total_experiments,
        active_experiments,
        concluded_experiments,
        total_assignments,
        total_results,
    })
}

/// Get results for an experiment
#[tauri::command]
pub fn get_experiment_results(
    workspace_path: &str,
    experiment_id: i64,
) -> Result<Vec<ExperimentResult>, String> {
    let conn =
        open_ab_testing_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT r.id, r.assignment_id, r.success_factor_id, r.outcome, r.metric_value, r.recorded_at
             FROM experiment_results r
             JOIN experiment_assignments a ON r.assignment_id = a.id
             WHERE a.experiment_id = ?1
             ORDER BY r.recorded_at DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results: Vec<ExperimentResult> = stmt
        .query_map([experiment_id], |row| {
            Ok(ExperimentResult {
                id: row.get(0)?,
                assignment_id: row.get(1)?,
                success_factor_id: row.get(2)?,
                outcome: row.get(3)?,
                metric_value: row.get(4)?,
                recorded_at: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query results: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect results: {e}"))?;

    Ok(results)
}
