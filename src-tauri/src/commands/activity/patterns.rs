use super::db::{calculate_cost, open_activity_db};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Prompt pattern categories based on agent roles and task types
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PatternCategory {
    /// Building features - issue implementation
    Build,
    /// Fixing bugs or addressing feedback
    Fix,
    /// Code refactoring and cleanup
    Refactor,
    /// Code review and quality checks
    Review,
    /// Issue curation and enhancement
    Curate,
    /// Architecture and design proposals
    Architect,
    /// Code simplification proposals
    Simplify,
    /// General/uncategorized patterns
    General,
}

impl std::fmt::Display for PatternCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Build => "build",
            Self::Fix => "fix",
            Self::Refactor => "refactor",
            Self::Review => "review",
            Self::Curate => "curate",
            Self::Architect => "architect",
            Self::Simplify => "simplify",
            Self::General => "general",
        };
        write!(f, "{s}")
    }
}

impl PatternCategory {
    /// Infer category from role name
    #[allow(dead_code)]
    fn from_role(role: &str) -> Self {
        match role.to_lowercase().as_str() {
            "builder" => Self::Build,
            "doctor" => Self::Fix,
            "judge" => Self::Review,
            "curator" => Self::Curate,
            "architect" => Self::Architect,
            "hermit" => Self::Simplify,
            _ => Self::General,
        }
    }
}

/// A prompt pattern extracted from activity data
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptPattern {
    pub id: Option<i64>,
    pub pattern_text: String,
    pub category: Option<String>,
    pub times_used: i32,
    pub success_count: i32,
    pub failure_count: i32,
    pub success_rate: f64,
    pub avg_cost_usd: f64,
    pub avg_duration_seconds: i32,
    pub avg_tokens: i32,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// A match between a pattern and an activity
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PatternMatch {
    pub id: Option<i64>,
    pub pattern_id: i64,
    pub activity_id: i64,
    pub similarity_score: f64,
    pub matched_at: Option<String>,
}

/// Summary of pattern extraction results
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PatternExtractionResult {
    pub patterns_created: i32,
    pub patterns_updated: i32,
    pub activities_processed: i32,
    pub matches_created: i32,
}

/// Normalize a trigger/prompt text into a pattern
/// - Strips issue/PR numbers (e.g., #123 -> #N)
/// - Normalizes whitespace
/// - Lowercases for comparison
#[allow(dead_code)]
fn normalize_prompt_to_pattern(prompt: &str) -> String {
    // Replace issue/PR numbers with placeholder
    let pattern = regex::Regex::new(r"#\d+")
        .map_or_else(|_| prompt.to_string(), |re| re.replace_all(prompt, "#N").to_string());

    // Normalize whitespace
    let pattern = regex::Regex::new(r"\s+")
        .map(|re| re.replace_all(&pattern, " ").to_string())
        .unwrap_or(pattern);

    // Trim and lowercase
    pattern.trim().to_lowercase()
}

/// Extract patterns from historical activity data
#[allow(dead_code, clippy::too_many_lines)]
#[tauri::command]
pub fn extract_prompt_patterns(workspace_path: &str) -> Result<PatternExtractionResult, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut patterns_created = 0;
    let mut patterns_updated = 0;
    let mut activities_processed = 0;
    let mut matches_created = 0;

    // Get all activities with their outcomes and token usage
    let mut stmt = conn
        .prepare(
            "SELECT a.id, a.trigger, a.role, a.work_found, a.work_completed, a.duration_ms,
                    COALESCE(t.total_tokens, 0), COALESCE(t.prompt_tokens, 0), COALESCE(t.completion_tokens, 0)
             FROM agent_activity a
             LEFT JOIN token_usage t ON a.id = t.activity_id
             WHERE a.trigger IS NOT NULL AND a.trigger != ''
             ORDER BY a.timestamp ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    #[allow(clippy::type_complexity)]
    let activities: Vec<(i64, String, String, bool, Option<bool>, Option<i32>, i64, i64, i64)> =
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i32>(3)? != 0,
                row.get::<_, Option<i32>>(4)?.map(|i| i != 0),
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .map_err(|e| format!("Failed to query activities: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect activities: {e}"))?;

    for (
        activity_id,
        trigger,
        role,
        work_found,
        work_completed,
        duration_ms,
        total_tokens,
        prompt_tokens,
        completion_tokens,
    ) in activities
    {
        activities_processed += 1;

        // Normalize the prompt to a pattern
        let pattern_text = normalize_prompt_to_pattern(&trigger);
        if pattern_text.is_empty() {
            continue;
        }

        // Determine category from role
        let category = PatternCategory::from_role(&role).to_string();

        // Determine success (work_found AND work_completed)
        let is_success = work_found && work_completed.unwrap_or(false);

        // Calculate cost
        let cost = calculate_cost(prompt_tokens, completion_tokens);

        // Try to find existing pattern
        let existing_pattern: Option<(i64, i32, i32, i32, i64, i64, i64)> = conn
            .query_row(
                "SELECT id, times_used, success_count, failure_count,
                        CAST(avg_cost_usd * times_used * 100 AS INTEGER),
                        avg_duration_seconds * times_used,
                        avg_tokens * times_used
                 FROM prompt_patterns
                 WHERE pattern_text = ?1",
                [&pattern_text],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .ok();

        let pattern_id = if let Some((
            id,
            times_used,
            success_count,
            failure_count,
            total_cost_cents,
            total_duration,
            total_tok,
        )) = existing_pattern
        {
            // Update existing pattern
            let new_times_used = times_used + 1;
            let new_success_count = if is_success {
                success_count + 1
            } else {
                success_count
            };
            let new_failure_count = if is_success {
                failure_count
            } else {
                failure_count + 1
            };
            let new_success_rate = f64::from(new_success_count) / f64::from(new_times_used);
            #[allow(clippy::cast_possible_truncation)]
            let new_total_cost_cents = total_cost_cents + (cost * 100.0) as i64;
            #[allow(clippy::cast_precision_loss)]
            let new_avg_cost = (new_total_cost_cents as f64 / 100.0) / f64::from(new_times_used);
            let new_total_duration = total_duration + i64::from(duration_ms.unwrap_or(0) / 1000);
            #[allow(clippy::cast_possible_truncation)]
            let new_avg_duration = (new_total_duration / i64::from(new_times_used)) as i32;
            let new_total_tokens = total_tok + total_tokens;
            #[allow(clippy::cast_possible_truncation)]
            let new_avg_tokens = (new_total_tokens / i64::from(new_times_used)) as i32;

            conn.execute(
                "UPDATE prompt_patterns SET
                    times_used = ?1,
                    success_count = ?2,
                    failure_count = ?3,
                    success_rate = ?4,
                    avg_cost_usd = ?5,
                    avg_duration_seconds = ?6,
                    avg_tokens = ?7,
                    last_used_at = datetime('now')
                 WHERE id = ?8",
                params![
                    new_times_used,
                    new_success_count,
                    new_failure_count,
                    new_success_rate,
                    new_avg_cost,
                    new_avg_duration,
                    new_avg_tokens,
                    id
                ],
            )
            .map_err(|e| format!("Failed to update pattern: {e}"))?;

            patterns_updated += 1;
            id
        } else {
            // Create new pattern
            let success_count = i32::from(is_success);
            let failure_count = i32::from(!is_success);
            let success_rate = if is_success { 1.0 } else { 0.0 };

            #[allow(clippy::cast_possible_truncation)]
            let total_tokens_i32 = total_tokens as i32;
            conn.execute(
                "INSERT INTO prompt_patterns (
                    pattern_text, category, times_used, success_count, failure_count,
                    success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, last_used_at
                 ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
                params![
                    &pattern_text,
                    &category,
                    success_count,
                    failure_count,
                    success_rate,
                    cost,
                    duration_ms.unwrap_or(0) / 1000,
                    total_tokens_i32
                ],
            )
            .map_err(|e| format!("Failed to insert pattern: {e}"))?;

            patterns_created += 1;
            conn.last_insert_rowid()
        };

        // Check if match already exists
        let match_exists: bool = conn
            .query_row(
                "SELECT 1 FROM pattern_matches WHERE pattern_id = ?1 AND activity_id = ?2",
                params![pattern_id, activity_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !match_exists {
            // Create pattern match
            conn.execute(
                "INSERT INTO pattern_matches (pattern_id, activity_id, similarity_score)
                 VALUES (?1, ?2, 1.0)",
                params![pattern_id, activity_id],
            )
            .map_err(|e| format!("Failed to insert pattern match: {e}"))?;

            matches_created += 1;
        }
    }

    Ok(PatternExtractionResult {
        patterns_created,
        patterns_updated,
        activities_processed,
        matches_created,
    })
}

/// Get patterns by category
#[allow(dead_code)]
#[tauri::command]
pub fn get_patterns_by_category(
    workspace_path: &str,
    category: &str,
    limit: Option<i32>,
) -> Result<Vec<PromptPattern>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(50);

    let mut stmt = conn
        .prepare(
            "SELECT id, pattern_text, category, times_used, success_count, failure_count,
                    success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, created_at, last_used_at
             FROM prompt_patterns
             WHERE category = ?1
             ORDER BY success_rate DESC, times_used DESC
             LIMIT ?2",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns = stmt
        .query_map(params![category, limit], |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_text: row.get(1)?,
                category: row.get(2)?,
                times_used: row.get(3)?,
                success_count: row.get(4)?,
                failure_count: row.get(5)?,
                success_rate: row.get(6)?,
                avg_cost_usd: row.get(7)?,
                avg_duration_seconds: row.get(8)?,
                avg_tokens: row.get(9)?,
                created_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("Failed to query patterns: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect patterns: {e}"))?;

    Ok(patterns)
}

/// Get top patterns sorted by a specific metric
#[allow(dead_code)]
#[tauri::command]
pub fn get_top_patterns(
    workspace_path: &str,
    sort_by: &str,
    limit: Option<i32>,
) -> Result<Vec<PromptPattern>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(20);

    #[allow(clippy::match_same_arms)]
    let order_clause = match sort_by {
        "success_rate" => "success_rate DESC, times_used DESC",
        "times_used" => "times_used DESC, success_rate DESC",
        "cost" => "avg_cost_usd ASC, success_rate DESC",
        "tokens" => "avg_tokens ASC, success_rate DESC",
        "recent" => "last_used_at DESC",
        _ => "success_rate DESC, times_used DESC",
    };

    let query = format!(
        "SELECT id, pattern_text, category, times_used, success_count, failure_count,
                success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, created_at, last_used_at
         FROM prompt_patterns
         WHERE times_used >= 2
         ORDER BY {order_clause}
         LIMIT ?1"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns = stmt
        .query_map([limit], |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_text: row.get(1)?,
                category: row.get(2)?,
                times_used: row.get(3)?,
                success_count: row.get(4)?,
                failure_count: row.get(5)?,
                success_rate: row.get(6)?,
                avg_cost_usd: row.get(7)?,
                avg_duration_seconds: row.get(8)?,
                avg_tokens: row.get(9)?,
                created_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("Failed to query patterns: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect patterns: {e}"))?;

    Ok(patterns)
}

/// Find patterns similar to a given prompt text
#[allow(dead_code)]
#[tauri::command]
pub fn find_similar_patterns(
    workspace_path: &str,
    prompt_text: &str,
    limit: Option<i32>,
) -> Result<Vec<PromptPattern>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(10);

    // Normalize the input prompt
    let normalized = normalize_prompt_to_pattern(prompt_text);

    // Extract key words for fuzzy matching (words with 3+ chars)
    let words: Vec<&str> = normalized
        .split_whitespace()
        .filter(|w| w.len() >= 3)
        .collect();

    if words.is_empty() {
        return Ok(Vec::new());
    }

    // Build LIKE conditions for each word
    let like_conditions: Vec<String> = words
        .iter()
        .map(|w| format!("pattern_text LIKE '%{w}%'"))
        .collect();

    let where_clause = like_conditions.join(" OR ");

    let query = format!(
        "SELECT id, pattern_text, category, times_used, success_count, failure_count,
                success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, created_at, last_used_at
         FROM prompt_patterns
         WHERE {where_clause}
         ORDER BY success_rate DESC, times_used DESC
         LIMIT ?1"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns = stmt
        .query_map([limit], |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_text: row.get(1)?,
                category: row.get(2)?,
                times_used: row.get(3)?,
                success_count: row.get(4)?,
                failure_count: row.get(5)?,
                success_rate: row.get(6)?,
                avg_cost_usd: row.get(7)?,
                avg_duration_seconds: row.get(8)?,
                avg_tokens: row.get(9)?,
                created_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("Failed to query patterns: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect patterns: {e}"))?;

    Ok(patterns)
}

/// Get pattern catalog statistics
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PatternCatalogStats {
    pub total_patterns: i32,
    pub patterns_by_category: Vec<CategoryCount>,
    pub avg_success_rate: f64,
    pub most_successful_category: Option<String>,
    pub most_used_pattern: Option<String>,
    pub total_activities_matched: i32,
}

/// Category count for statistics
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct CategoryCount {
    pub category: String,
    pub count: i32,
    pub avg_success_rate: f64,
}

/// Get statistics about the pattern catalog
#[allow(dead_code)]
#[tauri::command]
pub fn get_pattern_catalog_stats(workspace_path: &str) -> Result<PatternCatalogStats, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Total patterns
    let total_patterns: i32 = conn
        .query_row("SELECT COUNT(*) FROM prompt_patterns", [], |row| row.get(0))
        .unwrap_or(0);

    // Patterns by category with avg success rate
    let mut stmt = conn
        .prepare(
            "SELECT category, COUNT(*), AVG(success_rate)
             FROM prompt_patterns
             GROUP BY category
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns_by_category: Vec<CategoryCount> = stmt
        .query_map([], |row| {
            Ok(CategoryCount {
                category: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                count: row.get(1)?,
                avg_success_rate: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to query categories: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect categories: {e}"))?;

    // Average success rate across all patterns
    let avg_success_rate: f64 = conn
        .query_row(
            "SELECT AVG(success_rate) FROM prompt_patterns WHERE times_used >= 2",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Most successful category (by avg success rate, min 5 patterns)
    let most_successful_category: Option<String> = conn
        .query_row(
            "SELECT category FROM prompt_patterns
             GROUP BY category
             HAVING COUNT(*) >= 5
             ORDER BY AVG(success_rate) DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    // Most used pattern
    let most_used_pattern: Option<String> = conn
        .query_row(
            "SELECT pattern_text FROM prompt_patterns ORDER BY times_used DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    // Total activities matched
    let total_activities_matched: i32 = conn
        .query_row("SELECT COUNT(DISTINCT activity_id) FROM pattern_matches", [], |row| row.get(0))
        .unwrap_or(0);

    Ok(PatternCatalogStats {
        total_patterns,
        patterns_by_category,
        avg_success_rate,
        most_successful_category,
        most_used_pattern,
        total_activities_matched,
    })
}

/// Record that a pattern was used (for tracking when patterns are applied)
#[allow(dead_code, clippy::needless_pass_by_value)]
#[tauri::command]
pub fn record_pattern_usage(
    workspace_path: String,
    pattern_id: i64,
    activity_id: i64,
    was_successful: bool,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Update pattern statistics
    if was_successful {
        conn.execute(
            "UPDATE prompt_patterns SET
                times_used = times_used + 1,
                success_count = success_count + 1,
                success_rate = CAST(success_count + 1 AS REAL) / (times_used + 1),
                last_used_at = datetime('now')
             WHERE id = ?1",
            [pattern_id],
        )
        .map_err(|e| format!("Failed to update pattern: {e}"))?;
    } else {
        conn.execute(
            "UPDATE prompt_patterns SET
                times_used = times_used + 1,
                failure_count = failure_count + 1,
                success_rate = CAST(success_count AS REAL) / (times_used + 1),
                last_used_at = datetime('now')
             WHERE id = ?1",
            [pattern_id],
        )
        .map_err(|e| format!("Failed to update pattern: {e}"))?;
    }

    // Create pattern match
    conn.execute(
        "INSERT INTO pattern_matches (pattern_id, activity_id, similarity_score)
         VALUES (?1, ?2, 1.0)",
        params![pattern_id, activity_id],
    )
    .map_err(|e| format!("Failed to insert pattern match: {e}"))?;

    Ok(())
}
