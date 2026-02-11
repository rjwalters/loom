//! Prompt Template Generation Module
//!
//! Automatically generates reusable prompt templates by analyzing and generalizing
//! successful prompt patterns. This is Phase 5 (Autonomous Learning) of the
//! activity database system.
//!
//! Features:
//! - Cluster similar successful prompts
//! - Extract common structure and identify variable slots
//! - Generate templates with placeholders (e.g., {file}, {`issue_number`})
//! - Track template usage and effectiveness
//! - Retire underperforming templates automatically

use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// ============================================================================
// Types
// ============================================================================

/// A generated prompt template with variable placeholders
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PromptTemplate {
    pub id: Option<i64>,
    /// The template text with placeholders like {file}, {`issue_number`}
    pub template_text: String,
    /// Category of prompts this template applies to
    pub category: String,
    /// List of placeholder names found in the template
    pub placeholders: Vec<String>,
    /// Number of source patterns used to generate this template
    pub source_pattern_count: i32,
    /// Combined success rate of source patterns
    pub source_success_rate: f64,
    /// Number of times this template has been used
    pub times_used: i32,
    /// Success rate when this template is used
    pub success_rate: f64,
    /// Success count for this template
    pub success_count: i32,
    /// Failure count for this template
    pub failure_count: i32,
    /// Whether this template is active (not retired)
    pub active: bool,
    /// Minimum success rate threshold before retirement
    pub retirement_threshold: f64,
    /// When this template was generated
    pub created_at: Option<String>,
    /// When this template was last used
    pub last_used_at: Option<String>,
    /// Human-readable description of what this template does
    pub description: Option<String>,
    /// Example instantiation of the template
    pub example: Option<String>,
}

/// Result of template generation
#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateGenerationResult {
    pub templates_created: i32,
    pub templates_updated: i32,
    pub patterns_analyzed: i32,
    pub clusters_found: i32,
}

/// A cluster of similar patterns
#[derive(Debug)]
struct PatternCluster {
    patterns: Vec<(i64, String, f64, i32)>, // (id, pattern_text, success_rate, times_used)
    #[allow(dead_code)]
    category: String,
}

/// Template usage statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateStats {
    pub total_templates: i32,
    pub active_templates: i32,
    pub retired_templates: i32,
    pub templates_by_category: Vec<TemplateCategoryStats>,
    pub avg_success_rate: f64,
    pub top_templates: Vec<PromptTemplate>,
    pub retirement_candidates: Vec<PromptTemplate>,
}

/// Statistics for a template category
#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateCategoryStats {
    pub category: String,
    pub count: i32,
    pub avg_success_rate: f64,
    pub total_uses: i32,
}

// ============================================================================
// Database Setup
// ============================================================================

/// Open connection to activity database and ensure template schema exists
fn open_template_db(workspace_path: &str) -> SqliteResult<Connection> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let db_path = loom_dir.join("activity.db");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        std::fs::create_dir_all(&loom_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }

    let conn = Connection::open(&db_path)?;

    // Create template-specific tables
    conn.execute_batch(
        r"
        -- Prompt templates table
        CREATE TABLE IF NOT EXISTS prompt_templates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            template_text TEXT NOT NULL UNIQUE,
            category TEXT NOT NULL,
            placeholders TEXT NOT NULL DEFAULT '[]',
            source_pattern_count INTEGER DEFAULT 0,
            source_success_rate REAL DEFAULT 0.0,
            times_used INTEGER DEFAULT 0,
            success_rate REAL DEFAULT 0.0,
            success_count INTEGER DEFAULT 0,
            failure_count INTEGER DEFAULT 0,
            active INTEGER DEFAULT 1,
            retirement_threshold REAL DEFAULT 0.3,
            created_at TEXT DEFAULT (datetime('now')),
            last_used_at TEXT,
            description TEXT,
            example TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_templates_category ON prompt_templates(category);
        CREATE INDEX IF NOT EXISTS idx_templates_active ON prompt_templates(active);
        CREATE INDEX IF NOT EXISTS idx_templates_success_rate ON prompt_templates(success_rate DESC);

        -- Template source patterns (which patterns contributed to a template)
        CREATE TABLE IF NOT EXISTS template_sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            template_id INTEGER NOT NULL,
            pattern_id INTEGER NOT NULL,
            similarity_score REAL DEFAULT 1.0,
            FOREIGN KEY (template_id) REFERENCES prompt_templates(id),
            FOREIGN KEY (pattern_id) REFERENCES prompt_patterns(id),
            UNIQUE(template_id, pattern_id)
        );

        CREATE INDEX IF NOT EXISTS idx_template_sources_template ON template_sources(template_id);
        CREATE INDEX IF NOT EXISTS idx_template_sources_pattern ON template_sources(pattern_id);

        -- Template usage tracking
        CREATE TABLE IF NOT EXISTS template_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            template_id INTEGER NOT NULL,
            activity_id INTEGER,
            instantiated_prompt TEXT NOT NULL,
            was_successful INTEGER,
            used_at TEXT DEFAULT (datetime('now')),
            FOREIGN KEY (template_id) REFERENCES prompt_templates(id),
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        );

        CREATE INDEX IF NOT EXISTS idx_template_usage_template ON template_usage(template_id);
        CREATE INDEX IF NOT EXISTS idx_template_usage_success ON template_usage(was_successful);
        ",
    )?;

    Ok(conn)
}

// ============================================================================
// Template Generation Functions
// ============================================================================

/// Generate templates from successful prompt patterns
///
/// This function:
/// 1. Finds clusters of similar successful prompts
/// 2. Extracts common structure from each cluster
/// 3. Identifies variable parts and creates placeholders
/// 4. Generates templates with usage examples
#[tauri::command]
#[allow(clippy::too_many_lines)]
pub fn generate_templates_from_patterns(
    workspace_path: &str,
    min_cluster_size: Option<i32>,
    min_success_rate: Option<f64>,
) -> Result<TemplateGenerationResult, String> {
    let conn =
        open_template_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let min_cluster_size = min_cluster_size.unwrap_or(3);
    let min_success_rate = min_success_rate.unwrap_or(0.6);

    let mut templates_created = 0;
    let mut templates_updated = 0;
    let mut patterns_analyzed = 0;

    // Get successful patterns grouped by category
    let categories: Vec<String> = conn
        .prepare("SELECT DISTINCT category FROM prompt_patterns WHERE category IS NOT NULL")
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Failed to query categories: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect categories: {e}"))?;

    let mut clusters_found = 0;

    for category in &categories {
        // Get successful patterns in this category
        let patterns: Vec<(i64, String, f64, i32)> = conn
            .prepare(
                "SELECT id, pattern_text, success_rate, times_used
                 FROM prompt_patterns
                 WHERE category = ?1
                   AND success_rate >= ?2
                   AND times_used >= 2
                 ORDER BY success_rate DESC, times_used DESC",
            )
            .map_err(|e| format!("Failed to prepare query: {e}"))?
            .query_map(params![category, min_success_rate], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(|e| format!("Failed to query patterns: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect patterns: {e}"))?;

        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            patterns_analyzed += patterns.len() as i32;
        }

        // Cluster similar patterns
        let clusters = cluster_patterns(&patterns, 0.5);
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            clusters_found += clusters.len() as i32;
        }

        // Generate templates from each cluster
        for cluster in clusters {
            #[allow(clippy::cast_sign_loss)]
            if cluster.patterns.len() < min_cluster_size as usize {
                continue;
            }

            // Extract template from cluster
            if let Some((template_text, placeholders, example)) = extract_template(&cluster) {
                // Calculate combined success rate
                let total_successes: f64 = cluster
                    .patterns
                    .iter()
                    .map(|(_, _, rate, uses)| rate * f64::from(*uses))
                    .sum();
                let total_uses: i32 = cluster.patterns.iter().map(|(_, _, _, uses)| uses).sum();
                let combined_success_rate = if total_uses > 0 {
                    total_successes / f64::from(total_uses)
                } else {
                    0.0
                };

                let placeholders_json =
                    serde_json::to_string(&placeholders).unwrap_or_else(|_| "[]".to_string());

                // Generate description
                let description =
                    generate_template_description(&template_text, category, &placeholders);

                // Check if template already exists
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM prompt_templates WHERE template_text = ?1",
                        [&template_text],
                        |row| row.get(0),
                    )
                    .ok();

                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let pattern_count = cluster.patterns.len() as i32;

                let template_id = if let Some(id) = existing {
                    // Update existing template
                    conn.execute(
                        "UPDATE prompt_templates SET
                            source_pattern_count = ?1,
                            source_success_rate = ?2,
                            placeholders = ?3,
                            description = ?4,
                            example = ?5
                         WHERE id = ?6",
                        params![
                            pattern_count,
                            combined_success_rate,
                            placeholders_json,
                            description,
                            example,
                            id
                        ],
                    )
                    .map_err(|e| format!("Failed to update template: {e}"))?;
                    templates_updated += 1;
                    id
                } else {
                    // Create new template
                    conn.execute(
                        "INSERT INTO prompt_templates (
                            template_text, category, placeholders, source_pattern_count,
                            source_success_rate, description, example
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            template_text,
                            category,
                            placeholders_json,
                            pattern_count,
                            combined_success_rate,
                            description,
                            example
                        ],
                    )
                    .map_err(|e| format!("Failed to insert template: {e}"))?;
                    templates_created += 1;
                    conn.last_insert_rowid()
                };

                // Link source patterns to template
                for (pattern_id, _, _, _) in &cluster.patterns {
                    conn.execute(
                        "INSERT OR IGNORE INTO template_sources (template_id, pattern_id)
                         VALUES (?1, ?2)",
                        params![template_id, pattern_id],
                    )
                    .map_err(|e| format!("Failed to link pattern: {e}"))?;
                }
            }
        }
    }

    Ok(TemplateGenerationResult {
        templates_created,
        templates_updated,
        patterns_analyzed,
        clusters_found,
    })
}

/// Cluster patterns by similarity
fn cluster_patterns(
    patterns: &[(i64, String, f64, i32)],
    similarity_threshold: f64,
) -> Vec<PatternCluster> {
    let mut clusters: Vec<PatternCluster> = Vec::new();
    let mut assigned: Vec<bool> = vec![false; patterns.len()];

    for (i, (id, text, rate, uses)) in patterns.iter().enumerate() {
        if assigned[i] {
            continue;
        }

        let mut cluster = PatternCluster {
            patterns: vec![(*id, text.clone(), *rate, *uses)],
            category: String::new(),
        };
        assigned[i] = true;

        // Find similar patterns
        for (j, (id2, text2, rate2, uses2)) in patterns.iter().enumerate().skip(i + 1) {
            if assigned[j] {
                continue;
            }

            let similarity = calculate_pattern_similarity(text, text2);
            if similarity >= similarity_threshold {
                cluster.patterns.push((*id2, text2.clone(), *rate2, *uses2));
                assigned[j] = true;
            }
        }

        if !cluster.patterns.is_empty() {
            clusters.push(cluster);
        }
    }

    clusters
}

/// Calculate similarity between two patterns (0.0 to 1.0)
fn calculate_pattern_similarity(a: &str, b: &str) -> f64 {
    // Tokenize
    let words_a: Vec<&str> = a.split_whitespace().collect();
    let words_b: Vec<&str> = b.split_whitespace().collect();

    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }

    // Count common words (order-independent)
    let set_a: std::collections::HashSet<&str> = words_a.iter().copied().collect();
    let set_b: std::collections::HashSet<&str> = words_b.iter().copied().collect();

    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();

    if union == 0 {
        return 0.0;
    }

    // Jaccard similarity
    #[allow(clippy::cast_precision_loss)]
    let jaccard = intersection as f64 / union as f64;

    // Also consider structural similarity (same length = bonus)
    #[allow(clippy::cast_precision_loss)]
    let length_similarity = 1.0
        - ((words_a.len() as f64 - words_b.len() as f64).abs()
            / (words_a.len().max(words_b.len()) as f64));

    // Weighted combination
    0.7 * jaccard + 0.3 * length_similarity
}

/// Extract a template from a cluster of similar patterns
fn extract_template(cluster: &PatternCluster) -> Option<(String, Vec<String>, String)> {
    if cluster.patterns.is_empty() {
        return None;
    }

    // Tokenize all patterns
    let tokenized: Vec<Vec<&str>> = cluster
        .patterns
        .iter()
        .map(|(_, text, _, _)| text.split_whitespace().collect())
        .collect();

    // Find the median length
    let mut lengths: Vec<usize> = tokenized.iter().map(Vec::len).collect();
    lengths.sort_unstable();
    let median_len = lengths[lengths.len() / 2];

    // Find patterns closest to median length as reference
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let reference_idx = tokenized
        .iter()
        .enumerate()
        .min_by_key(|(_, t)| (t.len() as i32 - median_len as i32).abs())
        .map_or(0, |(i, _)| i);

    let reference = &tokenized[reference_idx];
    let mut template_tokens: Vec<String> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();

    // For each position in the reference, determine if it's constant or variable
    for (pos, &word) in reference.iter().enumerate() {
        // Count how many patterns have this exact word at this position
        let matches = tokenized
            .iter()
            .filter(|t| t.get(pos) == Some(&word))
            .count();

        #[allow(clippy::cast_precision_loss)]
        let match_ratio = matches as f64 / tokenized.len() as f64;

        if match_ratio >= 0.6 {
            // This word is common across patterns, keep it
            template_tokens.push(word.to_string());
        } else {
            // This word varies, create a placeholder
            let placeholder = detect_placeholder_type(word, pos, reference.len());
            if !placeholders.contains(&placeholder) {
                placeholders.push(placeholder.clone());
            }
            template_tokens.push(format!("{{{placeholder}}}"));
        }
    }

    let template_text = template_tokens.join(" ");

    // Generate example by using the most successful pattern
    let best_pattern = cluster
        .patterns
        .iter()
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, text, _, _)| text.clone())
        .unwrap_or_default();

    Some((template_text, placeholders, best_pattern))
}

/// Detect the type of placeholder based on the word content and position
fn detect_placeholder_type(word: &str, _position: usize, _total_length: usize) -> String {
    // Check for common patterns
    if word.starts_with('#') {
        return "issue_number".to_string();
    }

    if word.contains('/') || word.contains('.') {
        return "file_path".to_string();
    }

    if word.starts_with("pr") || word.contains("pull") {
        return "pr_number".to_string();
    }

    // Check for function/method names (camelCase or snake_case)
    if word.contains('_')
        || (word.chars().any(char::is_lowercase) && word.chars().any(char::is_uppercase))
    {
        return "function_name".to_string();
    }

    // Check for error messages
    if word.to_lowercase().contains("error") || word.to_lowercase().contains("fail") {
        return "error_message".to_string();
    }

    // Default to generic placeholder
    "target".to_string()
}

/// Generate a human-readable description for a template
fn generate_template_description(
    _template: &str,
    category: &str,
    placeholders: &[String],
) -> String {
    let action = match category {
        "build" => "Implements or creates",
        "fix" => "Fixes or resolves",
        "refactor" => "Refactors or restructures",
        "review" => "Reviews or analyzes",
        "curate" => "Enhances or documents",
        _ => "Performs action on",
    };

    let placeholder_desc = if placeholders.is_empty() {
        String::new()
    } else {
        format!(
            " Variables: {}",
            placeholders
                .iter()
                .map(|p| format!("{{{p}}}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    format!("{action} based on template pattern.{placeholder_desc}")
}

// ============================================================================
// Template Query Functions
// ============================================================================

/// Get all templates, optionally filtered by category
#[tauri::command]
pub fn get_templates(
    workspace_path: &str,
    category: Option<&str>,
    active_only: Option<bool>,
    limit: Option<i32>,
) -> Result<Vec<PromptTemplate>, String> {
    let conn =
        open_template_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let active_only = active_only.unwrap_or(true);
    let limit = limit.unwrap_or(50);

    let (query, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(cat) = category {
        let q = "SELECT id, template_text, category, placeholders, source_pattern_count,
                        source_success_rate, times_used, success_rate, success_count,
                        failure_count, active, retirement_threshold, created_at,
                        last_used_at, description, example
                 FROM prompt_templates
                 WHERE category = ?1 AND (?2 = 0 OR active = 1)
                 ORDER BY success_rate DESC, times_used DESC
                 LIMIT ?3"
            .to_string();
        (
            q,
            vec![
                Box::new(cat.to_string()),
                Box::new(i32::from(active_only)),
                Box::new(limit),
            ],
        )
    } else {
        let q = "SELECT id, template_text, category, placeholders, source_pattern_count,
                        source_success_rate, times_used, success_rate, success_count,
                        failure_count, active, retirement_threshold, created_at,
                        last_used_at, description, example
                 FROM prompt_templates
                 WHERE ?1 = 0 OR active = 1
                 ORDER BY success_rate DESC, times_used DESC
                 LIMIT ?2"
            .to_string();
        (q, vec![Box::new(i32::from(active_only)), Box::new(limit)])
    };

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();

    let templates = stmt
        .query_map(params_refs.as_slice(), |row| {
            let placeholders_json: String = row.get(3)?;
            let placeholders: Vec<String> =
                serde_json::from_str(&placeholders_json).unwrap_or_default();

            Ok(PromptTemplate {
                id: row.get(0)?,
                template_text: row.get(1)?,
                category: row.get(2)?,
                placeholders,
                source_pattern_count: row.get(4)?,
                source_success_rate: row.get(5)?,
                times_used: row.get(6)?,
                success_rate: row.get(7)?,
                success_count: row.get(8)?,
                failure_count: row.get(9)?,
                active: row.get::<_, i32>(10)? != 0,
                retirement_threshold: row.get(11)?,
                created_at: row.get(12)?,
                last_used_at: row.get(13)?,
                description: row.get(14)?,
                example: row.get(15)?,
            })
        })
        .map_err(|e| format!("Failed to query templates: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect templates: {e}"))?;

    Ok(templates)
}

/// Get a single template by ID
#[tauri::command]
pub fn get_template(
    workspace_path: &str,
    template_id: i64,
) -> Result<Option<PromptTemplate>, String> {
    let conn =
        open_template_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let template = conn
        .query_row(
            "SELECT id, template_text, category, placeholders, source_pattern_count,
                    source_success_rate, times_used, success_rate, success_count,
                    failure_count, active, retirement_threshold, created_at,
                    last_used_at, description, example
             FROM prompt_templates
             WHERE id = ?1",
            [template_id],
            |row| {
                let placeholders_json: String = row.get(3)?;
                let placeholders: Vec<String> =
                    serde_json::from_str(&placeholders_json).unwrap_or_default();

                Ok(PromptTemplate {
                    id: row.get(0)?,
                    template_text: row.get(1)?,
                    category: row.get(2)?,
                    placeholders,
                    source_pattern_count: row.get(4)?,
                    source_success_rate: row.get(5)?,
                    times_used: row.get(6)?,
                    success_rate: row.get(7)?,
                    success_count: row.get(8)?,
                    failure_count: row.get(9)?,
                    active: row.get::<_, i32>(10)? != 0,
                    retirement_threshold: row.get(11)?,
                    created_at: row.get(12)?,
                    last_used_at: row.get(13)?,
                    description: row.get(14)?,
                    example: row.get(15)?,
                })
            },
        )
        .ok();

    Ok(template)
}

/// Find the best matching template for a prompt intent
#[tauri::command]
pub fn find_matching_template(
    workspace_path: &str,
    prompt: &str,
    category: Option<&str>,
) -> Result<Option<PromptTemplate>, String> {
    let _conn =
        open_template_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get candidate templates
    let templates = get_templates(workspace_path, category, Some(true), Some(20))?;

    if templates.is_empty() {
        return Ok(None);
    }

    // Find best match by comparing prompt to template examples
    let mut best_match: Option<(PromptTemplate, f64)> = None;

    for template in templates {
        // Compare with example
        let similarity = if let Some(ref example) = template.example {
            calculate_pattern_similarity(prompt, example)
        } else {
            0.0
        };

        // Also compare with template text (ignoring placeholders)
        let template_words: String = template
            .template_text
            .split_whitespace()
            .filter(|w| !w.starts_with('{') && !w.ends_with('}'))
            .collect::<Vec<_>>()
            .join(" ");

        let template_similarity = calculate_pattern_similarity(prompt, &template_words);

        let score = 0.6 * similarity + 0.4 * template_similarity;

        if score > 0.3 {
            if let Some((_, best_score)) = &best_match {
                if score > *best_score {
                    best_match = Some((template, score));
                }
            } else {
                best_match = Some((template, score));
            }
        }
    }

    Ok(best_match.map(|(t, _)| t))
}

/// Instantiate a template with values
#[tauri::command]
pub fn instantiate_template(
    workspace_path: &str,
    template_id: i64,
    values: HashMap<String, String>,
) -> Result<String, String> {
    let template = get_template(workspace_path, template_id)?
        .ok_or_else(|| "Template not found".to_string())?;

    let mut result = template.template_text.clone();

    for (key, value) in values {
        let placeholder = format!("{{{key}}}");
        result = result.replace(&placeholder, &value);
    }

    Ok(result)
}

// ============================================================================
// Template Usage Tracking
// ============================================================================

/// Record that a template was used
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn record_template_usage(
    workspace_path: String,
    template_id: i64,
    instantiated_prompt: String,
    activity_id: Option<i64>,
) -> Result<i64, String> {
    let conn =
        open_template_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO template_usage (template_id, activity_id, instantiated_prompt)
         VALUES (?1, ?2, ?3)",
        params![template_id, activity_id, instantiated_prompt],
    )
    .map_err(|e| format!("Failed to record usage: {e}"))?;

    let usage_id = conn.last_insert_rowid();

    // Update template stats
    conn.execute(
        "UPDATE prompt_templates SET
            times_used = times_used + 1,
            last_used_at = datetime('now')
         WHERE id = ?1",
        [template_id],
    )
    .map_err(|e| format!("Failed to update template: {e}"))?;

    Ok(usage_id)
}

/// Record the outcome of a template usage
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn record_template_outcome(
    workspace_path: String,
    usage_id: i64,
    was_successful: bool,
) -> Result<(), String> {
    let conn =
        open_template_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Update usage record
    conn.execute(
        "UPDATE template_usage SET was_successful = ?1 WHERE id = ?2",
        params![was_successful, usage_id],
    )
    .map_err(|e| format!("Failed to update usage: {e}"))?;

    // Get template ID
    let template_id: i64 = conn
        .query_row("SELECT template_id FROM template_usage WHERE id = ?1", [usage_id], |row| {
            row.get(0)
        })
        .map_err(|e| format!("Failed to get template ID: {e}"))?;

    // Update template statistics
    if was_successful {
        conn.execute(
            "UPDATE prompt_templates SET
                success_count = success_count + 1,
                success_rate = CAST(success_count + 1 AS REAL) / times_used
             WHERE id = ?1",
            [template_id],
        )
        .map_err(|e| format!("Failed to update template success: {e}"))?;
    } else {
        conn.execute(
            "UPDATE prompt_templates SET
                failure_count = failure_count + 1,
                success_rate = CAST(success_count AS REAL) / times_used
             WHERE id = ?1",
            [template_id],
        )
        .map_err(|e| format!("Failed to update template failure: {e}"))?;
    }

    Ok(())
}

// ============================================================================
// Template Lifecycle Management
// ============================================================================

/// Retire underperforming templates
#[tauri::command]
pub fn retire_underperforming_templates(
    workspace_path: &str,
    min_uses: Option<i32>,
) -> Result<i32, String> {
    let conn =
        open_template_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let min_uses = min_uses.unwrap_or(10);

    // Retire templates that:
    // 1. Have been used at least min_uses times
    // 2. Have success_rate below their retirement_threshold
    let retired = conn
        .execute(
            "UPDATE prompt_templates SET active = 0
             WHERE active = 1
               AND times_used >= ?1
               AND success_rate < retirement_threshold",
            [min_uses],
        )
        .map_err(|e| format!("Failed to retire templates: {e}"))?;

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    Ok(retired as i32)
}

/// Reactivate a retired template
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn reactivate_template(workspace_path: String, template_id: i64) -> Result<(), String> {
    let conn =
        open_template_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute("UPDATE prompt_templates SET active = 1 WHERE id = ?1", [template_id])
        .map_err(|e| format!("Failed to reactivate template: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- calculate_pattern_similarity tests ----

    #[test]
    fn test_similarity_identical() {
        let score = calculate_pattern_similarity("fix the login bug", "fix the login bug");
        assert!((score - 1.0).abs() < 1e-10, "score = {score}");
    }

    #[test]
    fn test_similarity_completely_different() {
        let score = calculate_pattern_similarity("alpha beta gamma", "delta epsilon zeta");
        assert!(score < 0.1, "score = {score}");
    }

    #[test]
    fn test_similarity_partial_overlap() {
        let score = calculate_pattern_similarity("fix the login bug", "fix the signup bug");
        assert!(score > 0.4 && score < 1.0, "score = {score}");
    }

    #[test]
    fn test_similarity_empty_strings() {
        assert!((calculate_pattern_similarity("", "hello")).abs() < 1e-10);
        assert!((calculate_pattern_similarity("hello", "")).abs() < 1e-10);
        assert!((calculate_pattern_similarity("", "")).abs() < 1e-10);
    }

    #[test]
    fn test_similarity_symmetric() {
        let a = "implement the search feature";
        let b = "implement the filter feature";
        let score_ab = calculate_pattern_similarity(a, b);
        let score_ba = calculate_pattern_similarity(b, a);
        assert!((score_ab - score_ba).abs() < 1e-10);
    }

    // ---- cluster_patterns tests ----

    #[test]
    fn test_cluster_similar_patterns() {
        let patterns = vec![
            (1, "fix the login bug".to_string(), 0.8, 5),
            (2, "fix the signup bug".to_string(), 0.7, 3),
            (3, "implement the search feature".to_string(), 0.9, 10),
            (4, "implement the filter feature".to_string(), 0.85, 8),
        ];
        let clusters = cluster_patterns(&patterns, 0.5);
        // Should cluster "fix" patterns together and "implement" patterns together
        assert!(clusters.len() >= 2, "clusters = {}", clusters.len());
    }

    #[test]
    fn test_cluster_no_similar_patterns() {
        let patterns = vec![
            (1, "alpha".to_string(), 0.8, 5),
            (2, "beta".to_string(), 0.7, 3),
            (3, "gamma".to_string(), 0.9, 10),
        ];
        let clusters = cluster_patterns(&patterns, 0.9);
        // With high threshold, each pattern should be its own cluster
        assert_eq!(clusters.len(), 3);
    }

    #[test]
    fn test_cluster_empty_input() {
        let patterns: Vec<(i64, String, f64, i32)> = vec![];
        let clusters = cluster_patterns(&patterns, 0.5);
        assert!(clusters.is_empty());
    }

    // ---- detect_placeholder_type tests ----

    #[test]
    fn test_placeholder_issue_number() {
        assert_eq!(detect_placeholder_type("#123", 0, 5), "issue_number");
    }

    #[test]
    fn test_placeholder_file_path_slash() {
        assert_eq!(detect_placeholder_type("src/lib.rs", 0, 5), "file_path");
    }

    #[test]
    fn test_placeholder_file_path_dot() {
        assert_eq!(detect_placeholder_type("config.json", 0, 5), "file_path");
    }

    #[test]
    fn test_placeholder_pr_number() {
        assert_eq!(detect_placeholder_type("pr-123", 0, 5), "pr_number");
    }

    #[test]
    fn test_placeholder_function_snake_case() {
        assert_eq!(detect_placeholder_type("my_function", 0, 5), "function_name");
    }

    #[test]
    fn test_placeholder_function_camel_case() {
        assert_eq!(detect_placeholder_type("myFunction", 0, 5), "function_name");
    }

    #[test]
    fn test_placeholder_error_message() {
        assert_eq!(detect_placeholder_type("errorMessage", 0, 5), "function_name");
        // Note: "error" alone would match error_message
        assert_eq!(detect_placeholder_type("error", 0, 5), "error_message");
    }

    #[test]
    fn test_placeholder_default() {
        assert_eq!(detect_placeholder_type("something", 0, 5), "target");
    }

    // ---- extract_template tests ----

    #[test]
    fn test_extract_template_empty_cluster() {
        let cluster = PatternCluster {
            patterns: vec![],
            category: String::new(),
        };
        assert!(extract_template(&cluster).is_none());
    }

    #[test]
    fn test_extract_template_single_pattern() {
        let cluster = PatternCluster {
            patterns: vec![(1, "fix the login bug".to_string(), 0.8, 5)],
            category: "fix".to_string(),
        };
        let result = extract_template(&cluster);
        assert!(result.is_some());
        let (template, _placeholders, example) = result.unwrap();
        assert!(!template.is_empty());
        assert_eq!(example, "fix the login bug");
    }

    // ---- generate_template_description tests ----

    #[test]
    fn test_description_build_category() {
        let desc = generate_template_description("template", "build", &[]);
        assert!(desc.contains("Implements or creates"));
    }

    #[test]
    fn test_description_fix_category() {
        let desc = generate_template_description("template", "fix", &[]);
        assert!(desc.contains("Fixes or resolves"));
    }

    #[test]
    fn test_description_with_placeholders() {
        let desc = generate_template_description(
            "template",
            "build",
            &["file_path".to_string(), "issue_number".to_string()],
        );
        assert!(desc.contains("{file_path}"));
        assert!(desc.contains("{issue_number}"));
    }

    #[test]
    fn test_description_unknown_category() {
        let desc = generate_template_description("template", "unknown", &[]);
        assert!(desc.contains("Performs action on"));
    }
}

/// Get template statistics
#[tauri::command]
pub fn get_template_stats(workspace_path: &str) -> Result<TemplateStats, String> {
    let conn =
        open_template_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Total and active counts
    let total_templates: i32 = conn
        .query_row("SELECT COUNT(*) FROM prompt_templates", [], |row| row.get(0))
        .unwrap_or(0);

    let active_templates: i32 = conn
        .query_row("SELECT COUNT(*) FROM prompt_templates WHERE active = 1", [], |row| row.get(0))
        .unwrap_or(0);

    let retired_templates = total_templates - active_templates;

    // Templates by category
    let templates_by_category: Vec<TemplateCategoryStats> = conn
        .prepare(
            "SELECT category, COUNT(*), AVG(success_rate), SUM(times_used)
             FROM prompt_templates
             WHERE active = 1
             GROUP BY category
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| {
            Ok(TemplateCategoryStats {
                category: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                count: row.get(1)?,
                avg_success_rate: row.get(2)?,
                total_uses: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to query categories: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect categories: {e}"))?;

    // Average success rate
    let avg_success_rate: f64 = conn
        .query_row(
            "SELECT AVG(success_rate) FROM prompt_templates WHERE active = 1 AND times_used >= 5",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Top templates
    let top_templates = get_templates(workspace_path, None, Some(true), Some(5))?;

    // Retirement candidates
    let retirement_candidates = conn
        .prepare(
            "SELECT id, template_text, category, placeholders, source_pattern_count,
                    source_success_rate, times_used, success_rate, success_count,
                    failure_count, active, retirement_threshold, created_at,
                    last_used_at, description, example
             FROM prompt_templates
             WHERE active = 1
               AND times_used >= 5
               AND success_rate < retirement_threshold
             ORDER BY success_rate ASC
             LIMIT 5",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| {
            let placeholders_json: String = row.get(3)?;
            let placeholders: Vec<String> =
                serde_json::from_str(&placeholders_json).unwrap_or_default();

            Ok(PromptTemplate {
                id: row.get(0)?,
                template_text: row.get(1)?,
                category: row.get(2)?,
                placeholders,
                source_pattern_count: row.get(4)?,
                source_success_rate: row.get(5)?,
                times_used: row.get(6)?,
                success_rate: row.get(7)?,
                success_count: row.get(8)?,
                failure_count: row.get(9)?,
                active: row.get::<_, i32>(10)? != 0,
                retirement_threshold: row.get(11)?,
                created_at: row.get(12)?,
                last_used_at: row.get(13)?,
                description: row.get(14)?,
                example: row.get(15)?,
            })
        })
        .map_err(|e| format!("Failed to query candidates: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect candidates: {e}"))?;

    Ok(TemplateStats {
        total_templates,
        active_templates,
        retired_templates,
        templates_by_category,
        avg_success_rate,
        top_templates,
        retirement_candidates,
    })
}
