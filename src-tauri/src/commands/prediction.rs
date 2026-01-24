//! Success Prediction Model
//!
//! Provides a machine learning-based prediction model that estimates the likelihood
//! of success for prompts/tasks before execution. Uses logistic regression trained
//! on historical agent activity data.
//!
//! Part of Phase 4 (Advanced Analytics) - Builds on Phase 3 correlation analysis.

use chrono::{Datelike, Timelike};
use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ============================================================================
// Types
// ============================================================================

/// Request for a success prediction
#[derive(Debug, Serialize, Deserialize)]
pub struct PredictionRequest {
    /// The prompt text to analyze
    pub prompt_text: String,
    /// Optional agent role (builder, judge, curator, etc.)
    pub role: Option<String>,
    /// Optional additional context (time of day, recent history, etc.)
    pub context: Option<serde_json::Value>,
}

/// Result of a success prediction
#[derive(Debug, Serialize, Deserialize)]
pub struct PredictionResult {
    /// Predicted probability of success (0.0 - 1.0)
    pub success_probability: f64,
    /// Model confidence in the prediction (0.0 - 1.0)
    pub confidence: f64,
    /// Confidence interval as (lower, upper)
    pub confidence_interval: (f64, f64),
    /// Key factors that influenced the prediction
    pub key_factors: Vec<PredictionFactor>,
    /// Suggested alternative prompts with higher predicted success
    pub suggested_alternatives: Vec<PromptAlternative>,
    /// Warning if prediction may be unreliable
    pub warning: Option<String>,
}

/// A factor that influenced the prediction
#[derive(Debug, Serialize, Deserialize)]
pub struct PredictionFactor {
    /// Name of the factor (e.g., "prompt_length", "role_history")
    pub name: String,
    /// How much this factor contributed to the prediction (-1.0 to 1.0)
    pub contribution: f64,
    /// Whether this factor had a positive or negative effect
    pub direction: String,
    /// Human-readable explanation
    pub explanation: String,
}

/// A suggested alternative prompt
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptAlternative {
    /// The suggested modified prompt
    pub suggestion: String,
    /// Predicted improvement in success probability
    pub predicted_improvement: f64,
    /// Reason for the suggestion
    pub reason: String,
}

/// Features extracted from a prompt for prediction
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct PromptFeatures {
    // Text features
    pub length: i32,
    pub word_count: i32,
    pub has_code_block: bool,
    pub has_specific_file_refs: bool,
    pub question_count: i32,
    pub imperative_verb_count: i32,

    // Intent features
    pub intent_category: String,

    // Context features
    pub hour_of_day: i32,
    pub day_of_week: i32,
    pub role: String,

    // Historical features
    pub similar_pattern_success_rate: Option<f64>,
    pub role_avg_success_rate: f64,
    pub recent_session_success_rate: Option<f64>,
}

/// Model coefficients for logistic regression
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelCoefficients {
    pub intercept: f64,
    pub length_coef: f64,
    pub word_count_coef: f64,
    pub has_code_block_coef: f64,
    pub has_file_refs_coef: f64,
    pub question_count_coef: f64,
    pub imperative_verb_coef: f64,
    pub hour_coef: f64,
    pub day_coef: f64,
    pub role_success_rate_coef: f64,
}

impl Default for ModelCoefficients {
    fn default() -> Self {
        // Default coefficients based on heuristics (will be updated by training)
        Self {
            intercept: 0.0,
            length_coef: 0.001,          // Slightly longer prompts are better
            word_count_coef: 0.005,      // More words = more context
            has_code_block_coef: 0.2,    // Code blocks help
            has_file_refs_coef: 0.3,     // File references help specificity
            question_count_coef: -0.1,   // Too many questions = uncertainty
            imperative_verb_coef: 0.15,  // Clear commands are better
            hour_coef: 0.0,              // No default time bias
            day_coef: 0.0,               // No default day bias
            role_success_rate_coef: 0.8, // Role history is highly predictive
        }
    }
}

/// Training result summary
#[derive(Debug, Serialize, Deserialize)]
pub struct TrainingResult {
    pub samples_used: i32,
    pub accuracy: f64,
    pub precision: f64,
    pub recall: f64,
    pub f1_score: f64,
    pub trained_at: String,
}

/// Model statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelStats {
    pub is_trained: bool,
    pub samples_count: i32,
    pub last_trained: Option<String>,
    pub accuracy: Option<f64>,
    pub coefficients: Option<ModelCoefficients>,
}

// ============================================================================
// Database Setup
// ============================================================================

/// Open connection to the prediction database
fn open_prediction_db(workspace_path: &str) -> SqliteResult<Connection> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let db_path = loom_dir.join("activity.db");

    if !loom_dir.exists() {
        std::fs::create_dir_all(&loom_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }

    let conn = Connection::open(&db_path)?;

    // Create prediction-specific tables
    conn.execute_batch(
        r"
        -- Prediction model coefficients storage
        CREATE TABLE IF NOT EXISTS prediction_models (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            model_version TEXT NOT NULL DEFAULT 'v1',
            intercept REAL NOT NULL DEFAULT 0,
            length_coef REAL NOT NULL DEFAULT 0,
            word_count_coef REAL NOT NULL DEFAULT 0,
            has_code_block_coef REAL NOT NULL DEFAULT 0,
            has_file_refs_coef REAL NOT NULL DEFAULT 0,
            question_count_coef REAL NOT NULL DEFAULT 0,
            imperative_verb_coef REAL NOT NULL DEFAULT 0,
            hour_coef REAL NOT NULL DEFAULT 0,
            day_coef REAL NOT NULL DEFAULT 0,
            role_success_rate_coef REAL NOT NULL DEFAULT 0,
            trained_at TEXT DEFAULT (datetime('now')),
            samples_count INTEGER DEFAULT 0,
            accuracy REAL DEFAULT 0,
            precision_score REAL DEFAULT 0,
            recall_score REAL DEFAULT 0,
            f1_score REAL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_prediction_models_version ON prediction_models(model_version);

        -- Prediction history for tracking model performance
        CREATE TABLE IF NOT EXISTS prediction_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            predicted_at TEXT DEFAULT (datetime('now')),
            prompt_hash TEXT NOT NULL,
            prompt_length INTEGER,
            role TEXT,
            predicted_probability REAL NOT NULL,
            actual_outcome TEXT,
            was_correct INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_prediction_history_date ON prediction_history(predicted_at);
        CREATE INDEX IF NOT EXISTS idx_prediction_history_role ON prediction_history(role);
        ",
    )?;

    Ok(conn)
}

// ============================================================================
// Feature Extraction
// ============================================================================

/// Extract features from a prompt text
fn extract_features(
    prompt: &str,
    role: Option<&str>,
    context: Option<&serde_json::Value>,
) -> PromptFeatures {
    let words: Vec<&str> = prompt.split_whitespace().collect();

    // Imperative verbs commonly used in effective prompts
    let imperative_verbs = [
        "add",
        "create",
        "implement",
        "fix",
        "update",
        "remove",
        "refactor",
        "build",
        "write",
        "test",
        "check",
        "verify",
        "ensure",
        "make",
    ];

    let imperative_count = words
        .iter()
        .filter(|w| imperative_verbs.contains(&w.to_lowercase().as_str()))
        .count() as i32;

    // Check for code blocks (triple backticks)
    let has_code_block = prompt.contains("```");

    // Check for file references (paths like src/foo.rs or ./path/to/file)
    let has_file_refs = prompt.contains('/')
        && (prompt.contains(".rs")
            || prompt.contains(".ts")
            || prompt.contains(".js")
            || prompt.contains(".py")
            || prompt.contains(".md")
            || prompt.contains(".json"));

    // Count questions
    let question_count = prompt.matches('?').count() as i32;

    // Detect intent category
    let intent_category = detect_intent_category(prompt);

    // Get time context
    let now = chrono::Local::now();
    let hour_of_day = context
        .and_then(|c| c.get("hour_of_day"))
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| now.hour() as i64) as i32;
    let day_of_week = context
        .and_then(|c| c.get("day_of_week"))
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| now.weekday().num_days_from_sunday() as i64)
        as i32;

    PromptFeatures {
        length: prompt.len() as i32,
        word_count: words.len() as i32,
        has_code_block,
        has_specific_file_refs: has_file_refs,
        question_count,
        imperative_verb_count: imperative_count,
        intent_category,
        hour_of_day,
        day_of_week,
        role: role.unwrap_or("unknown").to_string(),
        similar_pattern_success_rate: None,
        role_avg_success_rate: 0.5, // Will be populated from DB
        recent_session_success_rate: None,
    }
}

/// Detect the intent category of a prompt
fn detect_intent_category(prompt: &str) -> String {
    let prompt_lower = prompt.to_lowercase();

    if prompt_lower.contains("fix")
        || prompt_lower.contains("bug")
        || prompt_lower.contains("error")
    {
        "fix".to_string()
    } else if prompt_lower.contains("add")
        || prompt_lower.contains("implement")
        || prompt_lower.contains("create")
    {
        "build".to_string()
    } else if prompt_lower.contains("refactor")
        || prompt_lower.contains("clean")
        || prompt_lower.contains("improve")
    {
        "refactor".to_string()
    } else if prompt_lower.contains("test") || prompt_lower.contains("verify") {
        "test".to_string()
    } else if prompt_lower.contains("review") || prompt_lower.contains("check") {
        "review".to_string()
    } else {
        "general".to_string()
    }
}

// ============================================================================
// Prediction Functions
// ============================================================================

/// Get or create default model coefficients
fn get_model_coefficients(conn: &Connection) -> ModelCoefficients {
    conn.query_row(
        "SELECT intercept, length_coef, word_count_coef, has_code_block_coef,
                has_file_refs_coef, question_count_coef, imperative_verb_coef,
                hour_coef, day_coef, role_success_rate_coef
         FROM prediction_models
         ORDER BY trained_at DESC
         LIMIT 1",
        [],
        |row| {
            Ok(ModelCoefficients {
                intercept: row.get(0)?,
                length_coef: row.get(1)?,
                word_count_coef: row.get(2)?,
                has_code_block_coef: row.get(3)?,
                has_file_refs_coef: row.get(4)?,
                question_count_coef: row.get(5)?,
                imperative_verb_coef: row.get(6)?,
                hour_coef: row.get(7)?,
                day_coef: row.get(8)?,
                role_success_rate_coef: row.get(9)?,
            })
        },
    )
    .unwrap_or_default()
}

/// Get historical success rate for a role
fn get_role_success_rate(conn: &Connection, role: &str) -> f64 {
    conn.query_row(
        "SELECT CAST(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) AS REAL) /
                NULLIF(COUNT(*), 0)
         FROM success_factors sf
         JOIN agent_activity a ON sf.input_id = a.id
         WHERE a.role = ?1 AND sf.outcome IN ('success', 'failure')",
        [role],
        |row| row.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
    .unwrap_or(0.5)
}

/// Sigmoid function for logistic regression
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Calculate prediction using logistic regression
fn calculate_prediction(features: &PromptFeatures, coefficients: &ModelCoefficients) -> f64 {
    // Normalize features
    let length_norm = (features.length as f64).min(5000.0) / 5000.0;
    let word_count_norm = (features.word_count as f64).min(500.0) / 500.0;
    let question_count_norm = (features.question_count as f64).min(10.0) / 10.0;
    let imperative_norm = (features.imperative_verb_count as f64).min(10.0) / 10.0;
    let hour_norm = (features.hour_of_day as f64 - 12.0) / 12.0; // Center around noon
    let day_norm = (features.day_of_week as f64 - 3.0) / 3.0; // Center around Wednesday

    // Calculate linear combination
    let z = coefficients.intercept
        + coefficients.length_coef * length_norm
        + coefficients.word_count_coef * word_count_norm
        + coefficients.has_code_block_coef * if features.has_code_block { 1.0 } else { 0.0 }
        + coefficients.has_file_refs_coef
            * if features.has_specific_file_refs {
                1.0
            } else {
                0.0
            }
        + coefficients.question_count_coef * question_count_norm
        + coefficients.imperative_verb_coef * imperative_norm
        + coefficients.hour_coef * hour_norm
        + coefficients.day_coef * day_norm
        + coefficients.role_success_rate_coef * features.role_avg_success_rate;

    sigmoid(z)
}

/// Calculate confidence interval using Wilson score interval
fn calculate_confidence_interval(probability: f64, sample_size: i32) -> (f64, f64) {
    if sample_size < 10 {
        // Wide interval for small samples
        return (0.1, 0.9);
    }

    let n = sample_size as f64;
    let z = 1.96; // 95% confidence level
    let p = probability;

    // Wilson score interval
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let margin = (z / denominator) * ((p * (1.0 - p) / n) + z * z / (4.0 * n * n)).sqrt();

    ((center - margin).max(0.0), (center + margin).min(1.0))
}

/// Generate suggestions for improving the prompt
fn generate_suggestions(features: &PromptFeatures, probability: f64) -> Vec<PromptAlternative> {
    let mut suggestions = Vec::new();

    // Only suggest if prediction is below threshold
    if probability >= 0.7 {
        return suggestions;
    }

    // Suggest adding specificity
    if features.word_count < 20 {
        suggestions.push(PromptAlternative {
            suggestion: "Add more context and specific requirements to the prompt".to_string(),
            predicted_improvement: 0.1,
            reason: "Short prompts often lack necessary context for success".to_string(),
        });
    }

    // Suggest file references
    if !features.has_specific_file_refs {
        suggestions.push(PromptAlternative {
            suggestion: "Reference specific files that need to be modified".to_string(),
            predicted_improvement: 0.15,
            reason: "Prompts with file references have higher success rates".to_string(),
        });
    }

    // Suggest code examples
    if !features.has_code_block
        && (features.intent_category == "build" || features.intent_category == "fix")
    {
        suggestions.push(PromptAlternative {
            suggestion: "Include code examples or expected output in code blocks".to_string(),
            predicted_improvement: 0.12,
            reason: "Code blocks provide clear expectations and reduce ambiguity".to_string(),
        });
    }

    // Suggest reducing questions
    if features.question_count > 3 {
        suggestions.push(PromptAlternative {
            suggestion: "Convert questions into clear statements or requirements".to_string(),
            predicted_improvement: 0.08,
            reason: "Too many questions can indicate unclear requirements".to_string(),
        });
    }

    // Suggest adding imperative verbs
    if features.imperative_verb_count == 0 {
        suggestions.push(PromptAlternative {
            suggestion: "Start with clear action verbs like 'Add', 'Create', 'Fix', or 'Implement'"
                .to_string(),
            predicted_improvement: 0.1,
            reason: "Clear commands help set expectations for the task".to_string(),
        });
    }

    suggestions
}

/// Build list of key factors that influenced the prediction
fn build_key_factors(
    features: &PromptFeatures,
    coefficients: &ModelCoefficients,
) -> Vec<PredictionFactor> {
    let mut factors = Vec::new();

    // Role history
    if features.role_avg_success_rate != 0.5 {
        let contribution =
            (features.role_avg_success_rate - 0.5) * coefficients.role_success_rate_coef;
        factors.push(PredictionFactor {
            name: "role_history".to_string(),
            contribution,
            direction: if contribution > 0.0 {
                "positive".to_string()
            } else {
                "negative".to_string()
            },
            explanation: format!(
                "{} role has {:.0}% historical success rate",
                features.role,
                features.role_avg_success_rate * 100.0
            ),
        });
    }

    // Code blocks
    if features.has_code_block {
        factors.push(PredictionFactor {
            name: "code_block".to_string(),
            contribution: coefficients.has_code_block_coef,
            direction: "positive".to_string(),
            explanation: "Prompt includes code examples (helpful for clarity)".to_string(),
        });
    }

    // File references
    if features.has_specific_file_refs {
        factors.push(PredictionFactor {
            name: "file_references".to_string(),
            contribution: coefficients.has_file_refs_coef,
            direction: "positive".to_string(),
            explanation: "Prompt references specific files (increases specificity)".to_string(),
        });
    }

    // Prompt length
    if features.word_count < 10 {
        factors.push(PredictionFactor {
            name: "prompt_length".to_string(),
            contribution: -0.1,
            direction: "negative".to_string(),
            explanation: "Prompt may be too short to provide adequate context".to_string(),
        });
    } else if features.word_count > 50 {
        factors.push(PredictionFactor {
            name: "prompt_length".to_string(),
            contribution: 0.05,
            direction: "positive".to_string(),
            explanation: "Detailed prompt provides good context".to_string(),
        });
    }

    // Questions
    if features.question_count > 3 {
        factors.push(PredictionFactor {
            name: "question_count".to_string(),
            contribution: coefficients.question_count_coef
                * (features.question_count as f64 / 10.0),
            direction: "negative".to_string(),
            explanation: "Multiple questions may indicate unclear requirements".to_string(),
        });
    }

    // Intent clarity
    if features.imperative_verb_count > 0 {
        factors.push(PredictionFactor {
            name: "intent_clarity".to_string(),
            contribution: coefficients.imperative_verb_coef
                * (features.imperative_verb_count as f64 / 10.0),
            direction: "positive".to_string(),
            explanation: "Clear action verbs help define the task".to_string(),
        });
    }

    // Sort by absolute contribution
    factors.sort_by(|a, b| {
        b.contribution
            .abs()
            .partial_cmp(&a.contribution.abs())
            .unwrap()
    });

    // Return top 5 factors
    factors.into_iter().take(5).collect()
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// Predict success likelihood for a prompt
#[tauri::command]
pub fn predict_prompt_success(
    workspace_path: String,
    request: PredictionRequest,
) -> Result<PredictionResult, String> {
    let conn =
        open_prediction_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Extract features
    let mut features =
        extract_features(&request.prompt_text, request.role.as_deref(), request.context.as_ref());

    // Get role success rate from history
    features.role_avg_success_rate = get_role_success_rate(&conn, &features.role);

    // Get model coefficients
    let coefficients = get_model_coefficients(&conn);

    // Calculate prediction
    let probability = calculate_prediction(&features, &coefficients);

    // Get sample size for confidence calculation
    let sample_size: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM success_factors WHERE outcome IN ('success', 'failure')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Calculate confidence
    let confidence = if sample_size >= 200 {
        0.9
    } else if sample_size >= 50 {
        0.7
    } else if sample_size >= 10 {
        0.5
    } else {
        0.3
    };

    let confidence_interval = calculate_confidence_interval(probability, sample_size);

    // Build factors and suggestions
    let key_factors = build_key_factors(&features, &coefficients);
    let suggestions = generate_suggestions(&features, probability);

    // Add warning if applicable
    let warning = if sample_size < 50 {
        Some(format!(
            "Prediction based on limited data ({} samples). Accuracy may improve with more usage.",
            sample_size
        ))
    } else {
        None
    };

    // Log the prediction for future training
    let prompt_hash = format!("{:x}", md5::compute(&request.prompt_text));
    let _ = conn.execute(
        "INSERT INTO prediction_history (prompt_hash, prompt_length, role, predicted_probability)
         VALUES (?1, ?2, ?3, ?4)",
        params![prompt_hash, features.length, features.role, probability],
    );

    Ok(PredictionResult {
        success_probability: probability,
        confidence,
        confidence_interval,
        key_factors,
        suggested_alternatives: suggestions,
        warning,
    })
}

/// Train or retrain the prediction model
#[tauri::command]
pub fn train_prediction_model(workspace_path: String) -> Result<TrainingResult, String> {
    let conn =
        open_prediction_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get training data
    let mut stmt = conn
        .prepare(
            "SELECT
                sf.prompt_length,
                sf.hour_of_day,
                sf.day_of_week,
                a.role,
                sf.outcome
             FROM success_factors sf
             JOIN agent_activity a ON sf.input_id = a.id
             WHERE sf.outcome IN ('success', 'failure')
             ORDER BY RANDOM()
             LIMIT 10000",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let training_data: Vec<(i32, i32, i32, String, bool)> = stmt
        .query_map([], |row| {
            let outcome: String = row.get(4)?;
            Ok((
                row.get::<_, Option<i32>>(0)?.unwrap_or(0),
                row.get::<_, Option<i32>>(1)?.unwrap_or(12),
                row.get::<_, Option<i32>>(2)?.unwrap_or(3),
                row.get::<_, String>(3)?,
                outcome == "success",
            ))
        })
        .map_err(|e| format!("Failed to query data: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect data: {e}"))?;

    let samples_used = training_data.len() as i32;

    if samples_used < 10 {
        return Err("Insufficient training data (need at least 10 samples)".to_string());
    }

    // Simple gradient descent for logistic regression
    let mut coefficients = ModelCoefficients::default();
    let learning_rate = 0.01;
    let iterations = 100;

    // Get role success rates for training
    let role_rates: std::collections::HashMap<String, f64> = {
        let mut rates = std::collections::HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT a.role,
                    CAST(SUM(CASE WHEN sf.outcome = 'success' THEN 1 ELSE 0 END) AS REAL) / COUNT(*)
             FROM success_factors sf
             JOIN agent_activity a ON sf.input_id = a.id
             WHERE sf.outcome IN ('success', 'failure')
             GROUP BY a.role",
        ) {
            if let Ok(rows) =
                stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)))
            {
                for row in rows.flatten() {
                    rates.insert(row.0, row.1);
                }
            }
        }
        rates
    };

    // Training loop
    for _ in 0..iterations {
        let mut gradient = ModelCoefficients::default();

        for (length, hour, day, role, success) in &training_data {
            let role_rate = *role_rates.get(role).unwrap_or(&0.5);

            // Normalize features
            let length_norm = (*length as f64).min(5000.0) / 5000.0;
            let hour_norm = (*hour as f64 - 12.0) / 12.0;
            let day_norm = (*day as f64 - 3.0) / 3.0;

            // Calculate prediction
            let z = coefficients.intercept
                + coefficients.length_coef * length_norm
                + coefficients.hour_coef * hour_norm
                + coefficients.day_coef * day_norm
                + coefficients.role_success_rate_coef * role_rate;

            let pred = sigmoid(z);
            let error = pred - if *success { 1.0 } else { 0.0 };

            // Accumulate gradients
            gradient.intercept += error;
            gradient.length_coef += error * length_norm;
            gradient.hour_coef += error * hour_norm;
            gradient.day_coef += error * day_norm;
            gradient.role_success_rate_coef += error * role_rate;
        }

        // Update coefficients
        let n = training_data.len() as f64;
        coefficients.intercept -= learning_rate * gradient.intercept / n;
        coefficients.length_coef -= learning_rate * gradient.length_coef / n;
        coefficients.hour_coef -= learning_rate * gradient.hour_coef / n;
        coefficients.day_coef -= learning_rate * gradient.day_coef / n;
        coefficients.role_success_rate_coef -= learning_rate * gradient.role_success_rate_coef / n;
    }

    // Calculate accuracy metrics
    let mut correct = 0;
    let mut true_positives = 0;
    let mut false_positives = 0;
    let mut false_negatives = 0;

    for (length, hour, day, role, success) in &training_data {
        let role_rate = *role_rates.get(role).unwrap_or(&0.5);
        let length_norm = (*length as f64).min(5000.0) / 5000.0;
        let hour_norm = (*hour as f64 - 12.0) / 12.0;
        let day_norm = (*day as f64 - 3.0) / 3.0;

        let z = coefficients.intercept
            + coefficients.length_coef * length_norm
            + coefficients.hour_coef * hour_norm
            + coefficients.day_coef * day_norm
            + coefficients.role_success_rate_coef * role_rate;

        let pred = sigmoid(z) >= 0.5;

        if pred == *success {
            correct += 1;
        }
        if pred && *success {
            true_positives += 1;
        }
        if pred && !*success {
            false_positives += 1;
        }
        if !pred && *success {
            false_negatives += 1;
        }
    }

    let accuracy = correct as f64 / samples_used as f64;
    let precision = if true_positives + false_positives > 0 {
        true_positives as f64 / (true_positives + false_positives) as f64
    } else {
        0.0
    };
    let recall = if true_positives + false_negatives > 0 {
        true_positives as f64 / (true_positives + false_negatives) as f64
    } else {
        0.0
    };
    let f1_score = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };

    // Save the trained model
    conn.execute(
        "INSERT INTO prediction_models
         (intercept, length_coef, word_count_coef, has_code_block_coef, has_file_refs_coef,
          question_count_coef, imperative_verb_coef, hour_coef, day_coef, role_success_rate_coef,
          samples_count, accuracy, precision_score, recall_score, f1_score)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            coefficients.intercept,
            coefficients.length_coef,
            coefficients.word_count_coef,
            coefficients.has_code_block_coef,
            coefficients.has_file_refs_coef,
            coefficients.question_count_coef,
            coefficients.imperative_verb_coef,
            coefficients.hour_coef,
            coefficients.day_coef,
            coefficients.role_success_rate_coef,
            samples_used,
            accuracy,
            precision,
            recall,
            f1_score
        ],
    )
    .map_err(|e| format!("Failed to save model: {e}"))?;

    Ok(TrainingResult {
        samples_used,
        accuracy,
        precision,
        recall,
        f1_score,
        trained_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Get statistics about the prediction model
#[tauri::command]
pub fn get_prediction_model_stats(workspace_path: String) -> Result<ModelStats, String> {
    let conn =
        open_prediction_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Check if a model exists
    let model_info: Option<(String, i32, f64, ModelCoefficients)> = conn
        .query_row(
            "SELECT trained_at, samples_count, accuracy,
                    intercept, length_coef, word_count_coef, has_code_block_coef,
                    has_file_refs_coef, question_count_coef, imperative_verb_coef,
                    hour_coef, day_coef, role_success_rate_coef
             FROM prediction_models
             ORDER BY trained_at DESC
             LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, f64>(2)?,
                    ModelCoefficients {
                        intercept: row.get(3)?,
                        length_coef: row.get(4)?,
                        word_count_coef: row.get(5)?,
                        has_code_block_coef: row.get(6)?,
                        has_file_refs_coef: row.get(7)?,
                        question_count_coef: row.get(8)?,
                        imperative_verb_coef: row.get(9)?,
                        hour_coef: row.get(10)?,
                        day_coef: row.get(11)?,
                        role_success_rate_coef: row.get(12)?,
                    },
                ))
            },
        )
        .ok();

    // Get total samples
    let samples_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM success_factors WHERE outcome IN ('success', 'failure')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    match model_info {
        Some((trained_at, _, accuracy, coefficients)) => Ok(ModelStats {
            is_trained: true,
            samples_count,
            last_trained: Some(trained_at),
            accuracy: Some(accuracy),
            coefficients: Some(coefficients),
        }),
        None => Ok(ModelStats {
            is_trained: false,
            samples_count,
            last_trained: None,
            accuracy: None,
            coefficients: None,
        }),
    }
}

/// Record actual outcome for a previous prediction (for model improvement)
#[tauri::command]
pub fn record_prediction_outcome(
    workspace_path: String,
    prompt_text: String,
    actual_outcome: String,
) -> Result<(), String> {
    let conn =
        open_prediction_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let prompt_hash = format!("{:x}", md5::compute(&prompt_text));
    let was_success = actual_outcome == "success";

    // Find the most recent prediction for this prompt
    let prediction: Option<(i64, f64)> = conn
        .query_row(
            "SELECT id, predicted_probability FROM prediction_history
             WHERE prompt_hash = ?1 AND actual_outcome IS NULL
             ORDER BY predicted_at DESC
             LIMIT 1",
            [&prompt_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    if let Some((id, predicted_prob)) = prediction {
        let predicted_success = predicted_prob >= 0.5;
        let was_correct = predicted_success == was_success;

        conn.execute(
            "UPDATE prediction_history
             SET actual_outcome = ?1, was_correct = ?2
             WHERE id = ?3",
            params![actual_outcome, was_correct as i32, id],
        )
        .map_err(|e| format!("Failed to record outcome: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_features() {
        let prompt = "Add a new function to src/lib/utils.ts that handles error logging";
        let features = extract_features(prompt, Some("builder"), None);

        assert!(features.length > 0);
        assert!(features.word_count > 5);
        assert!(features.has_specific_file_refs);
        assert!(features.imperative_verb_count > 0);
        assert_eq!(features.intent_category, "build");
    }

    #[test]
    fn test_detect_intent() {
        assert_eq!(detect_intent_category("Fix the bug in login"), "fix");
        assert_eq!(detect_intent_category("Add new feature"), "build");
        assert_eq!(detect_intent_category("Refactor the code"), "refactor");
        assert_eq!(detect_intent_category("Test the API"), "test");
        assert_eq!(detect_intent_category("Do something"), "general");
    }

    #[test]
    fn test_sigmoid() {
        assert!((sigmoid(0.0) - 0.5).abs() < 0.001);
        assert!(sigmoid(10.0) > 0.999);
        assert!(sigmoid(-10.0) < 0.001);
    }

    #[test]
    fn test_confidence_interval() {
        let (low, high) = calculate_confidence_interval(0.5, 100);
        assert!(low < 0.5);
        assert!(high > 0.5);
        assert!(low >= 0.0);
        assert!(high <= 1.0);
    }
}
