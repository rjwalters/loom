//! Success Correlation Analysis Engine
//!
//! Analyzes factors that correlate with successful outcomes in agent activity.
//! Provides statistical correlation calculations and success prediction insights.
//!
//! Phase 3 (Intelligence & Learning) - Uses Phase 2 correlation data from activity.rs

use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ============================================================================
// Types
// ============================================================================

/// Correlation result between two factors
#[derive(Debug, Serialize, Deserialize)]
pub struct CorrelationResult {
    pub id: Option<i64>,
    pub factor_a: String,
    pub factor_b: String,
    pub correlation_coefficient: f64,
    pub p_value: f64,
    pub sample_size: i32,
    pub analysis_date: String,
    pub notes: Option<String>,
}

/// Success factor entry for an agent input
#[derive(Debug, Serialize, Deserialize)]
pub struct SuccessFactor {
    pub id: Option<i64>,
    pub input_id: i64,
    pub prompt_length: Option<i32>,
    pub hour_of_day: Option<i32>,
    pub day_of_week: Option<i32>,
    pub has_tests_first: Option<bool>,
    pub review_cycles: Option<i32>,
    pub outcome: String, // 'success', 'failure', 'partial'
}

/// Success rate breakdown by factor
#[derive(Debug, Serialize, Deserialize)]
pub struct SuccessRateByFactor {
    pub factor_name: String,
    pub factor_value: String,
    pub total_count: i32,
    pub success_count: i32,
    pub success_rate: f64,
}

/// Correlation insight derived from analysis
#[derive(Debug, Serialize, Deserialize)]
pub struct CorrelationInsight {
    pub factor: String,
    pub insight: String,
    pub correlation_strength: String, // "strong", "moderate", "weak"
    pub recommendation: String,
}

/// Summary of correlation analysis
#[derive(Debug, Serialize, Deserialize)]
pub struct CorrelationSummary {
    pub total_samples: i32,
    pub success_rate: f64,
    pub significant_correlations: i32,
    pub top_insights: Vec<CorrelationInsight>,
}

// ============================================================================
// Database Setup
// ============================================================================

/// Open connection to activity database and ensure correlation schema exists
fn open_correlation_db(workspace_path: &str) -> SqliteResult<Connection> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let db_path = loom_dir.join("activity.db");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        std::fs::create_dir_all(&loom_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }

    let conn = Connection::open(&db_path)?;

    // Ensure base tables exist (agent_activity should already exist from activity.rs)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS agent_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            trigger TEXT NOT NULL,
            work_found INTEGER NOT NULL,
            work_completed INTEGER,
            issue_number INTEGER,
            duration_ms INTEGER,
            outcome TEXT NOT NULL,
            notes TEXT
        )",
        [],
    )?;

    // Create correlation-specific tables
    conn.execute_batch(
        r"
        -- Correlation results table for storing analysis outcomes
        CREATE TABLE IF NOT EXISTS correlation_results (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            factor_a TEXT NOT NULL,
            factor_b TEXT NOT NULL,
            correlation_coefficient REAL NOT NULL,
            p_value REAL NOT NULL,
            sample_size INTEGER NOT NULL,
            analysis_date TEXT DEFAULT (datetime('now')),
            notes TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_correlation_factors ON correlation_results(factor_a, factor_b);
        CREATE INDEX IF NOT EXISTS idx_correlation_date ON correlation_results(analysis_date);
        CREATE INDEX IF NOT EXISTS idx_correlation_significance ON correlation_results(p_value);

        -- Success factors table for tracking input characteristics
        CREATE TABLE IF NOT EXISTS success_factors (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            input_id INTEGER NOT NULL,
            prompt_length INTEGER,
            hour_of_day INTEGER,
            day_of_week INTEGER,
            has_tests_first INTEGER,
            review_cycles INTEGER,
            outcome TEXT NOT NULL DEFAULT 'unknown',
            created_at TEXT DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_success_factors_input ON success_factors(input_id);
        CREATE INDEX IF NOT EXISTS idx_success_factors_outcome ON success_factors(outcome);
        CREATE INDEX IF NOT EXISTS idx_success_factors_hour ON success_factors(hour_of_day);
        CREATE INDEX IF NOT EXISTS idx_success_factors_day ON success_factors(day_of_week);
        ",
    )?;

    Ok(conn)
}

// ============================================================================
// Statistical Calculations
// ============================================================================

/// Calculate Pearson correlation coefficient between two numeric vectors
#[allow(clippy::many_single_char_names)]
fn pearson_correlation(x: &[f64], y: &[f64]) -> (f64, f64) {
    if x.len() != y.len() || x.len() < 3 {
        return (0.0, 1.0); // Not enough data
    }

    #[allow(clippy::cast_precision_loss)]
    let n = x.len() as f64;

    // Calculate means
    let mean_x: f64 = x.iter().sum::<f64>() / n;
    let mean_y: f64 = y.iter().sum::<f64>() / n;

    // Calculate covariance and standard deviations
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;

    for i in 0..x.len() {
        let dx = x[i] - mean_x;
        let dy = y[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }

    if var_x == 0.0 || var_y == 0.0 {
        return (0.0, 1.0); // No variance
    }

    let r = cov / (var_x.sqrt() * var_y.sqrt());

    // Calculate t-statistic for significance test
    let t = r * ((n - 2.0) / (1.0 - r * r)).sqrt();

    // Approximate p-value using t-distribution
    // For simplicity, use a normal approximation for large samples
    let p_value = if n > 30.0 {
        2.0 * (1.0 - normal_cdf(t.abs()))
    } else {
        // For smaller samples, use a more conservative estimate
        2.0 * (1.0 - normal_cdf(t.abs() * 0.9))
    };

    (r, p_value.clamp(0.0, 1.0))
}

/// Approximate normal CDF for p-value calculation
fn normal_cdf(x: f64) -> f64 {
    // Approximation using error function
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Error function approximation
fn erf(x: f64) -> f64 {
    // Horner form approximation
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

/// Calculate chi-square statistic for categorical data
#[allow(dead_code)]
fn chi_square_test(observed: &[[i32; 2]; 2]) -> (f64, f64) {
    let total: i32 = observed.iter().flat_map(|row| row.iter()).sum();

    if total == 0 {
        return (0.0, 1.0);
    }

    let row_totals = [
        observed[0][0] + observed[0][1],
        observed[1][0] + observed[1][1],
    ];
    let col_totals = [
        observed[0][0] + observed[1][0],
        observed[0][1] + observed[1][1],
    ];

    let mut chi_sq = 0.0;

    for i in 0..2 {
        for j in 0..2 {
            let expected = f64::from(row_totals[i] * col_totals[j]) / f64::from(total);
            if expected > 0.0 {
                let diff = f64::from(observed[i][j]) - expected;
                chi_sq += (diff * diff) / expected;
            }
        }
    }

    // Approximate p-value for chi-square with 1 degree of freedom
    let p_value = 1.0 - chi_square_cdf(chi_sq, 1.0);

    (chi_sq, p_value.clamp(0.0, 1.0))
}

/// Chi-square CDF approximation
#[allow(dead_code, clippy::float_cmp)]
fn chi_square_cdf(x: f64, df: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }

    // Use incomplete gamma function approximation
    #[allow(clippy::no_effect_underscore_binding)]
    let _k = df / 2.0;
    #[allow(clippy::no_effect_underscore_binding)]
    let _z = x / 2.0;

    // Simple approximation for df=1
    if df == 1.0 {
        return 2.0 * normal_cdf(x.sqrt()) - 1.0;
    }

    // For other degrees of freedom, use Wilson-Hilferty approximation
    let z_approx =
        ((x / df).powf(1.0 / 3.0) - (1.0 - 2.0 / (9.0 * df))) / (2.0 / (9.0 * df)).sqrt();
    normal_cdf(z_approx)
}

// ============================================================================
// Data Extraction Functions
// ============================================================================

/// Extract success factors from historical activity data
#[tauri::command]
pub fn extract_success_factors(workspace_path: &str) -> Result<i32, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Extract factors from agent_activity table
    let extracted = conn
        .execute(
            "INSERT INTO success_factors (input_id, prompt_length, hour_of_day, day_of_week, outcome)
             SELECT
                 id,
                 COALESCE(duration_ms / 100, 0) as prompt_length,
                 CAST(strftime('%H', timestamp) AS INTEGER) as hour_of_day,
                 CAST(strftime('%w', timestamp) AS INTEGER) as day_of_week,
                 CASE
                     WHEN work_found = 1 AND work_completed = 1 THEN 'success'
                     WHEN work_found = 1 AND work_completed = 0 THEN 'failure'
                     WHEN work_found = 0 THEN 'no_work'
                     ELSE 'partial'
                 END as outcome
             FROM agent_activity
             WHERE id NOT IN (SELECT input_id FROM success_factors)",
            [],
        )
        .map_err(|e| format!("Failed to extract success factors: {e}"))?;

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    Ok(extracted as i32)
}

// ============================================================================
// Correlation Analysis Commands
// ============================================================================

/// Run correlation analysis between hour of day and success rate
#[tauri::command]
pub fn analyze_hour_success_correlation(workspace_path: &str) -> Result<CorrelationResult, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get hour and success data
    let mut stmt = conn
        .prepare(
            "SELECT hour_of_day, CASE WHEN outcome = 'success' THEN 1.0 ELSE 0.0 END as success
             FROM success_factors
             WHERE hour_of_day IS NOT NULL AND outcome IN ('success', 'failure')",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let data: Vec<(f64, f64)> = stmt
        .query_map([], |row| Ok((f64::from(row.get::<_, i32>(0)?), row.get(1)?)))
        .map_err(|e| format!("Failed to query data: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect data: {e}"))?;

    if data.is_empty() {
        return Ok(CorrelationResult {
            id: None,
            factor_a: "hour_of_day".to_string(),
            factor_b: "success".to_string(),
            correlation_coefficient: 0.0,
            p_value: 1.0,
            sample_size: 0,
            analysis_date: chrono::Utc::now().to_rfc3339(),
            notes: Some("Insufficient data for analysis".to_string()),
        });
    }

    let (hours, successes): (Vec<f64>, Vec<f64>) = data.into_iter().unzip();
    let (r, p_value) = pearson_correlation(&hours, &successes);

    let result = CorrelationResult {
        id: None,
        factor_a: "hour_of_day".to_string(),
        factor_b: "success".to_string(),
        correlation_coefficient: r,
        p_value,
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        sample_size: hours.len() as i32,
        analysis_date: chrono::Utc::now().to_rfc3339(),
        notes: Some(
            (if p_value < 0.05 {
                "Statistically significant"
            } else {
                "Not statistically significant"
            })
            .to_string(),
        ),
    };

    // Store the result
    conn.execute(
        "INSERT INTO correlation_results (factor_a, factor_b, correlation_coefficient, p_value, sample_size, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            result.factor_a,
            result.factor_b,
            result.correlation_coefficient,
            result.p_value,
            result.sample_size,
            result.notes
        ],
    )
    .map_err(|e| format!("Failed to store correlation result: {e}"))?;

    Ok(result)
}

/// Run correlation analysis between role and success rate
#[tauri::command]
pub fn analyze_role_success_correlation(
    workspace_path: &str,
) -> Result<Vec<SuccessRateByFactor>, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT
                 a.role,
                 COUNT(*) as total,
                 SUM(CASE WHEN sf.outcome = 'success' THEN 1 ELSE 0 END) as success_count
             FROM agent_activity a
             JOIN success_factors sf ON a.id = sf.input_id
             WHERE sf.outcome IN ('success', 'failure')
             GROUP BY a.role
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results = stmt
        .query_map([], |row| {
            let total: i32 = row.get(1)?;
            let success_count: i32 = row.get(2)?;
            Ok(SuccessRateByFactor {
                factor_name: "role".to_string(),
                factor_value: row.get(0)?,
                total_count: total,
                success_count,
                success_rate: if total > 0 {
                    f64::from(success_count) / f64::from(total)
                } else {
                    0.0
                },
            })
        })
        .map_err(|e| format!("Failed to query role correlations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect results: {e}"))?;

    Ok(results)
}

/// Analyze success rate by time of day buckets
#[tauri::command]
pub fn analyze_time_of_day_success(
    workspace_path: &str,
) -> Result<Vec<SuccessRateByFactor>, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT
                 CASE
                     WHEN hour_of_day BETWEEN 6 AND 11 THEN 'morning'
                     WHEN hour_of_day BETWEEN 12 AND 17 THEN 'afternoon'
                     WHEN hour_of_day BETWEEN 18 AND 21 THEN 'evening'
                     ELSE 'night'
                 END as time_bucket,
                 COUNT(*) as total,
                 SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count
             FROM success_factors
             WHERE hour_of_day IS NOT NULL AND outcome IN ('success', 'failure')
             GROUP BY time_bucket
             ORDER BY
                 CASE time_bucket
                     WHEN 'morning' THEN 1
                     WHEN 'afternoon' THEN 2
                     WHEN 'evening' THEN 3
                     WHEN 'night' THEN 4
                 END",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results = stmt
        .query_map([], |row| {
            let total: i32 = row.get(1)?;
            let success_count: i32 = row.get(2)?;
            Ok(SuccessRateByFactor {
                factor_name: "time_of_day".to_string(),
                factor_value: row.get(0)?,
                total_count: total,
                success_count,
                success_rate: if total > 0 {
                    f64::from(success_count) / f64::from(total)
                } else {
                    0.0
                },
            })
        })
        .map_err(|e| format!("Failed to query time correlations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect results: {e}"))?;

    Ok(results)
}

/// Analyze success rate by day of week
#[tauri::command]
pub fn analyze_day_of_week_success(
    workspace_path: &str,
) -> Result<Vec<SuccessRateByFactor>, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT
                 CASE day_of_week
                     WHEN 0 THEN 'Sunday'
                     WHEN 1 THEN 'Monday'
                     WHEN 2 THEN 'Tuesday'
                     WHEN 3 THEN 'Wednesday'
                     WHEN 4 THEN 'Thursday'
                     WHEN 5 THEN 'Friday'
                     WHEN 6 THEN 'Saturday'
                 END as day_name,
                 COUNT(*) as total,
                 SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count
             FROM success_factors
             WHERE day_of_week IS NOT NULL AND outcome IN ('success', 'failure')
             GROUP BY day_of_week
             ORDER BY day_of_week",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results = stmt
        .query_map([], |row| {
            let total: i32 = row.get(1)?;
            let success_count: i32 = row.get(2)?;
            Ok(SuccessRateByFactor {
                factor_name: "day_of_week".to_string(),
                factor_value: row.get(0)?,
                total_count: total,
                success_count,
                success_rate: if total > 0 {
                    f64::from(success_count) / f64::from(total)
                } else {
                    0.0
                },
            })
        })
        .map_err(|e| format!("Failed to query day correlations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect results: {e}"))?;

    Ok(results)
}

// ============================================================================
// Query Functions
// ============================================================================

/// Get all stored correlation results with optional significance filter
#[tauri::command]
pub fn get_correlations(
    workspace_path: &str,
    min_significance: Option<f64>,
) -> Result<Vec<CorrelationResult>, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let p_threshold = min_significance.unwrap_or(1.0);

    let mut stmt = conn
        .prepare(
            "SELECT id, factor_a, factor_b, correlation_coefficient, p_value, sample_size, analysis_date, notes
             FROM correlation_results
             WHERE p_value <= ?1
             ORDER BY ABS(correlation_coefficient) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results = stmt
        .query_map([p_threshold], |row| {
            Ok(CorrelationResult {
                id: row.get(0)?,
                factor_a: row.get(1)?,
                factor_b: row.get(2)?,
                correlation_coefficient: row.get(3)?,
                p_value: row.get(4)?,
                sample_size: row.get(5)?,
                analysis_date: row.get(6)?,
                notes: row.get(7)?,
            })
        })
        .map_err(|e| format!("Failed to query correlations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect results: {e}"))?;

    Ok(results)
}

/// Get success factors for a specific role
#[tauri::command]
pub fn get_success_factors_for_role(
    workspace_path: &str,
    role: &str,
) -> Result<Vec<SuccessFactor>, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT sf.id, sf.input_id, sf.prompt_length, sf.hour_of_day, sf.day_of_week,
                    sf.has_tests_first, sf.review_cycles, sf.outcome
             FROM success_factors sf
             JOIN agent_activity a ON sf.input_id = a.id
             WHERE a.role = ?1
             ORDER BY sf.id DESC
             LIMIT 100",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let results = stmt
        .query_map([role], |row| {
            Ok(SuccessFactor {
                id: row.get(0)?,
                input_id: row.get(1)?,
                prompt_length: row.get(2)?,
                hour_of_day: row.get(3)?,
                day_of_week: row.get(4)?,
                has_tests_first: row.get::<_, Option<i32>>(5)?.map(|v| v != 0),
                review_cycles: row.get(6)?,
                outcome: row.get(7)?,
            })
        })
        .map_err(|e| format!("Failed to query success factors: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect results: {e}"))?;

    Ok(results)
}

/// Run a full correlation analysis and return a summary with insights
#[tauri::command]
#[allow(clippy::too_many_lines)]
pub fn run_correlation_analysis(workspace_path: &str) -> Result<CorrelationSummary, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // First, extract success factors if needed
    let _ = extract_success_factors(workspace_path);

    // Get total samples and success rate
    let (total_samples, success_count): (i32, i32) = conn
        .query_row(
            "SELECT COUNT(*), SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END)
             FROM success_factors
             WHERE outcome IN ('success', 'failure')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((0, 0));

    let success_rate = if total_samples > 0 {
        f64::from(success_count) / f64::from(total_samples)
    } else {
        0.0
    };

    // Run individual correlation analyses
    let _ = analyze_hour_success_correlation(workspace_path);

    // Count significant correlations
    let significant_correlations: i32 = conn
        .query_row("SELECT COUNT(*) FROM correlation_results WHERE p_value < 0.05", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    // Generate insights
    let mut insights = Vec::new();

    // Time of day insight
    if let Ok(time_results) = analyze_time_of_day_success(workspace_path) {
        if let Some(best) = time_results
            .iter()
            .filter(|r| r.total_count >= 5)
            .max_by(|a, b| {
                a.success_rate
                    .partial_cmp(&b.success_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            insights.push(CorrelationInsight {
                factor: "time_of_day".to_string(),
                insight: format!(
                    "{} has the highest success rate at {:.1}%",
                    best.factor_value,
                    best.success_rate * 100.0
                ),
                correlation_strength: if best.success_rate > 0.8 {
                    "strong"
                } else if best.success_rate > 0.6 {
                    "moderate"
                } else {
                    "weak"
                }
                .to_string(),
                recommendation: format!(
                    "Consider scheduling complex tasks during {} hours",
                    best.factor_value.to_lowercase()
                ),
            });
        }
    }

    // Role effectiveness insight
    if let Ok(role_results) = analyze_role_success_correlation(workspace_path) {
        if let Some(best) = role_results
            .iter()
            .filter(|r| r.total_count >= 5)
            .max_by(|a, b| {
                a.success_rate
                    .partial_cmp(&b.success_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            insights.push(CorrelationInsight {
                factor: "role".to_string(),
                insight: format!(
                    "{} role has the highest success rate at {:.1}%",
                    best.factor_value,
                    best.success_rate * 100.0
                ),
                correlation_strength: if best.success_rate > 0.8 {
                    "strong"
                } else if best.success_rate > 0.6 {
                    "moderate"
                } else {
                    "weak"
                }
                .to_string(),
                recommendation: format!(
                    "The {} role workflow appears most effective",
                    best.factor_value.to_lowercase()
                ),
            });
        }
    }

    // Day of week insight
    if let Ok(day_results) = analyze_day_of_week_success(workspace_path) {
        if let Some(best) = day_results
            .iter()
            .filter(|r| r.total_count >= 3)
            .max_by(|a, b| {
                a.success_rate
                    .partial_cmp(&b.success_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            insights.push(CorrelationInsight {
                factor: "day_of_week".to_string(),
                insight: format!(
                    "{}s show the highest success rate at {:.1}%",
                    best.factor_value,
                    best.success_rate * 100.0
                ),
                correlation_strength: if best.success_rate > 0.8 {
                    "strong"
                } else if best.success_rate > 0.6 {
                    "moderate"
                } else {
                    "weak"
                }
                .to_string(),
                recommendation: format!(
                    "Consider prioritizing complex work on {}s",
                    best.factor_value
                ),
            });
        }
    }

    Ok(CorrelationSummary {
        total_samples,
        success_rate,
        significant_correlations,
        top_insights: insights,
    })
}

/// Predict success likelihood based on input features
#[tauri::command]
pub fn predict_success(
    workspace_path: &str,
    hour_of_day: Option<i32>,
    day_of_week: Option<i32>,
    role: Option<String>,
) -> Result<f64, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get baseline success rate
    let baseline: f64 = conn
        .query_row(
            "SELECT CAST(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
             FROM success_factors
             WHERE outcome IN ('success', 'failure')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.5);

    let mut adjustment = 0.0;
    let mut factors_used = 0;

    // Adjust based on hour of day
    if let Some(hour) = hour_of_day {
        let hour_rate: Option<f64> = conn
            .query_row(
                "SELECT CAST(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
                 FROM success_factors
                 WHERE hour_of_day = ?1 AND outcome IN ('success', 'failure')",
                [hour],
                |row| row.get(0),
            )
            .ok();

        if let Some(rate) = hour_rate {
            adjustment += (rate - baseline) * 0.3; // Weight factor
            factors_used += 1;
        }
    }

    // Adjust based on day of week
    if let Some(day) = day_of_week {
        let day_rate: Option<f64> = conn
            .query_row(
                "SELECT CAST(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
                 FROM success_factors
                 WHERE day_of_week = ?1 AND outcome IN ('success', 'failure')",
                [day],
                |row| row.get(0),
            )
            .ok();

        if let Some(rate) = day_rate {
            adjustment += (rate - baseline) * 0.2; // Weight factor
            factors_used += 1;
        }
    }

    // Adjust based on role
    if let Some(role_name) = role {
        let role_rate: Option<f64> = conn
            .query_row(
                "SELECT CAST(SUM(CASE WHEN sf.outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
                 FROM success_factors sf
                 JOIN agent_activity a ON sf.input_id = a.id
                 WHERE a.role = ?1 AND sf.outcome IN ('success', 'failure')",
                [role_name],
                |row| row.get(0),
            )
            .ok();

        if let Some(rate) = role_rate {
            adjustment += (rate - baseline) * 0.4; // Higher weight for role
            factors_used += 1;
        }
    }

    // Normalize adjustment if multiple factors used
    if factors_used > 1 {
        adjustment /= f64::from(factors_used);
    }

    // Return predicted success rate, clamped to valid probability range
    Ok((baseline + adjustment).clamp(0.0, 1.0))
}

/// Log a success factor entry manually
#[tauri::command]
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub fn log_success_factor(
    workspace_path: String,
    input_id: i64,
    prompt_length: Option<i32>,
    hour_of_day: Option<i32>,
    day_of_week: Option<i32>,
    has_tests_first: Option<bool>,
    review_cycles: Option<i32>,
    outcome: String,
) -> Result<i64, String> {
    let conn = open_correlation_db(&workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO success_factors (input_id, prompt_length, hour_of_day, day_of_week, has_tests_first, review_cycles, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            input_id,
            prompt_length,
            hour_of_day,
            day_of_week,
            has_tests_first.map(i32::from),
            review_cycles,
            outcome
        ],
    )
    .map_err(|e| format!("Failed to log success factor: {e}"))?;

    Ok(conn.last_insert_rowid())
}

/// Delete old correlation results
#[tauri::command]
pub fn clear_correlation_results(workspace_path: &str) -> Result<i32, String> {
    let conn =
        open_correlation_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let deleted = conn
        .execute("DELETE FROM correlation_results", [])
        .map_err(|e| format!("Failed to clear correlation results: {e}"))?;

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    Ok(deleted as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- erf tests ----

    #[test]
    fn test_erf_zero() {
        assert!((erf(0.0)).abs() < 1e-10);
    }

    #[test]
    fn test_erf_positive() {
        // erf(1) ≈ 0.8427
        let result = erf(1.0);
        assert!((result - 0.8427).abs() < 0.01, "erf(1) = {result}");
    }

    #[test]
    fn test_erf_negative_symmetry() {
        // erf(-x) == -erf(x)
        let x = 0.5;
        assert!((erf(-x) + erf(x)).abs() < 1e-10);
    }

    #[test]
    fn test_erf_large_input() {
        // erf(3) should be very close to 1.0
        assert!((erf(3.0) - 1.0).abs() < 0.001);
    }

    // ---- normal_cdf tests ----

    #[test]
    fn test_normal_cdf_at_zero() {
        // CDF at 0 should be 0.5
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_normal_cdf_large_positive() {
        // CDF at large positive should approach 1.0
        assert!(normal_cdf(5.0) > 0.999);
    }

    #[test]
    fn test_normal_cdf_large_negative() {
        // CDF at large negative should approach 0.0
        assert!(normal_cdf(-5.0) < 0.001);
    }

    #[test]
    fn test_normal_cdf_monotonic() {
        assert!(normal_cdf(-1.0) < normal_cdf(0.0));
        assert!(normal_cdf(0.0) < normal_cdf(1.0));
    }

    // ---- pearson_correlation tests ----

    #[test]
    fn test_pearson_perfect_positive() {
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![2.0, 4.0, 6.0, 8.0, 10.0];
        let (r, _p) = pearson_correlation(&x, &y);
        assert!((r - 1.0).abs() < 1e-10, "r = {r}");
    }

    #[test]
    fn test_pearson_perfect_negative() {
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![10.0, 8.0, 6.0, 4.0, 2.0];
        let (r, _p) = pearson_correlation(&x, &y);
        assert!((r + 1.0).abs() < 1e-10, "r = {r}");
    }

    #[test]
    fn test_pearson_no_correlation() {
        // Constant y -> zero variance -> returns (0, 1)
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![5.0, 5.0, 5.0, 5.0, 5.0];
        let (r, p) = pearson_correlation(&x, &y);
        assert!((r).abs() < 1e-10);
        assert!((p - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_pearson_too_few_samples() {
        let x = vec![1.0, 2.0];
        let y = vec![3.0, 4.0];
        let (r, p) = pearson_correlation(&x, &y);
        assert!((r).abs() < 1e-10);
        assert!((p - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_pearson_mismatched_lengths() {
        let x = vec![1.0, 2.0, 3.0];
        let y = vec![1.0, 2.0];
        let (r, p) = pearson_correlation(&x, &y);
        assert!((r).abs() < 1e-10);
        assert!((p - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_pearson_p_value_significant() {
        // Strong correlation with many points should have low p-value
        let n = 50;
        let x: Vec<f64> = (0..n).map(f64::from).collect();
        let y: Vec<f64> = x.iter().map(|&v| v * 2.0 + 1.0).collect();
        let (r, p) = pearson_correlation(&x, &y);
        assert!((r - 1.0).abs() < 1e-10);
        assert!(p < 0.05, "p = {p}");
    }

    // ---- chi_square tests ----

    #[test]
    fn test_chi_square_all_zeros() {
        let observed = [[0, 0], [0, 0]];
        let (chi, p) = chi_square_test(&observed);
        assert!((chi).abs() < 1e-10);
        assert!((p - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_chi_square_independent() {
        // When proportions are equal, chi-square should be ~0
        let observed = [[50, 50], [50, 50]];
        let (chi, _p) = chi_square_test(&observed);
        assert!(chi.abs() < 1e-10, "chi = {chi}");
    }

    #[test]
    fn test_chi_square_dependent() {
        // Highly dependent data should produce large chi-square
        let observed = [[100, 0], [0, 100]];
        let (chi, p) = chi_square_test(&observed);
        assert!(chi > 10.0, "chi = {chi}");
        assert!(p < 0.05, "p = {p}");
    }

    // ---- chi_square_cdf tests ----

    #[test]
    fn test_chi_square_cdf_zero() {
        assert!((chi_square_cdf(0.0, 1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_chi_square_cdf_negative() {
        assert!((chi_square_cdf(-1.0, 1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_chi_square_cdf_df1_known() {
        // For df=1, chi-square CDF at 3.84 ≈ 0.95
        let cdf = chi_square_cdf(3.84, 1.0);
        assert!((cdf - 0.95).abs() < 0.05, "cdf = {cdf}");
    }
}
