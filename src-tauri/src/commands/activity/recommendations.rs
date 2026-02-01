use super::db::open_activity_db;
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Recommendation types based on what kind of suggestion is being made
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationType {
    /// Prompt suggestions for better results
    Prompt,
    /// Optimal timing suggestions
    Timing,
    /// Role assignment suggestions
    Role,
    /// Anti-pattern warnings
    Warning,
    /// Cost optimization alerts
    Cost,
}

impl std::fmt::Display for RecommendationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Prompt => "prompt",
            Self::Timing => "timing",
            Self::Role => "role",
            Self::Warning => "warning",
            Self::Cost => "cost",
        };
        write!(f, "{s}")
    }
}

/// A generated recommendation
#[allow(dead_code, clippy::struct_field_names)]
#[derive(Debug, Serialize, Deserialize)]
pub struct Recommendation {
    pub id: Option<i64>,
    pub recommendation_type: String,
    pub title: String,
    pub description: Option<String>,
    pub confidence: f64,
    pub evidence: Option<String>,
    pub context_role: Option<String>,
    pub context_task_type: Option<String>,
    pub created_at: Option<String>,
    pub dismissed_at: Option<String>,
    pub acted_on: bool,
}

/// A recommendation rule configuration
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct RecommendationRule {
    pub id: Option<i64>,
    pub name: String,
    pub rule_type: String,
    pub description: Option<String>,
    pub threshold_value: Option<f64>,
    pub threshold_count: Option<i32>,
    pub recommendation_template: String,
    pub priority: i32,
    pub enabled: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Result of recommendation generation
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct RecommendationGenerationResult {
    pub recommendations_created: i32,
    pub rules_evaluated: i32,
    pub patterns_analyzed: i32,
}

/// Get all active (non-dismissed) recommendations
#[allow(dead_code)]
#[tauri::command]
pub fn get_active_recommendations(
    workspace_path: &str,
    limit: Option<i32>,
) -> Result<Vec<Recommendation>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(50);

    let mut stmt = conn
        .prepare(
            "SELECT id, type, title, description, confidence, evidence, context_role, context_task_type, created_at, dismissed_at, acted_on
             FROM recommendations
             WHERE dismissed_at IS NULL
             ORDER BY confidence DESC, created_at DESC
             LIMIT ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let recommendations = stmt
        .query_map([limit], |row| {
            Ok(Recommendation {
                id: row.get(0)?,
                recommendation_type: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                confidence: row.get(4)?,
                evidence: row.get(5)?,
                context_role: row.get(6)?,
                context_task_type: row.get(7)?,
                created_at: row.get(8)?,
                dismissed_at: row.get(9)?,
                acted_on: row.get::<_, i32>(10)? != 0,
            })
        })
        .map_err(|e| format!("Failed to query recommendations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect recommendations: {e}"))?;

    Ok(recommendations)
}

/// Get recommendations filtered by context (role and/or task type)
#[allow(dead_code, clippy::needless_pass_by_value)]
#[tauri::command]
pub fn get_recommendations_for_context(
    workspace_path: &str,
    role: Option<String>,
    task_type: Option<String>,
    limit: Option<i32>,
) -> Result<Vec<Recommendation>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit_val = limit.unwrap_or(20);

    // Build query based on provided context
    let mut stmt = conn
        .prepare(
            "SELECT id, type, title, description, confidence, evidence, context_role, context_task_type, created_at, dismissed_at, acted_on
             FROM recommendations
             WHERE dismissed_at IS NULL
               AND (context_role IS NULL OR context_role = ?1 OR ?1 IS NULL)
               AND (context_task_type IS NULL OR context_task_type = ?2 OR ?2 IS NULL)
             ORDER BY confidence DESC, created_at DESC
             LIMIT ?3",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let recommendations = stmt
        .query_map(params![role, task_type, limit_val], |row| {
            Ok(Recommendation {
                id: row.get(0)?,
                recommendation_type: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                confidence: row.get(4)?,
                evidence: row.get(5)?,
                context_role: row.get(6)?,
                context_task_type: row.get(7)?,
                created_at: row.get(8)?,
                dismissed_at: row.get(9)?,
                acted_on: row.get::<_, i32>(10)? != 0,
            })
        })
        .map_err(|e| format!("Failed to query recommendations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect recommendations: {e}"))?;

    Ok(recommendations)
}

/// Dismiss a recommendation
#[allow(dead_code, clippy::needless_pass_by_value)]
#[tauri::command]
pub fn dismiss_recommendation(
    workspace_path: String,
    recommendation_id: i64,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE recommendations SET dismissed_at = datetime('now') WHERE id = ?1",
        [recommendation_id],
    )
    .map_err(|e| format!("Failed to dismiss recommendation: {e}"))?;

    Ok(())
}

/// Mark a recommendation as acted upon
#[allow(dead_code, clippy::needless_pass_by_value)]
#[tauri::command]
pub fn mark_recommendation_acted_on(
    workspace_path: String,
    recommendation_id: i64,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute("UPDATE recommendations SET acted_on = 1 WHERE id = ?1", [recommendation_id])
        .map_err(|e| format!("Failed to mark recommendation as acted on: {e}"))?;

    Ok(())
}

/// Get all recommendation rules
#[allow(dead_code)]
#[tauri::command]
pub fn get_recommendation_rules(workspace_path: &str) -> Result<Vec<RecommendationRule>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority, enabled, created_at, updated_at
             FROM recommendation_rules
             ORDER BY priority ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let rules = stmt
        .query_map([], |row| {
            Ok(RecommendationRule {
                id: row.get(0)?,
                name: row.get(1)?,
                rule_type: row.get(2)?,
                description: row.get(3)?,
                threshold_value: row.get(4)?,
                threshold_count: row.get(5)?,
                recommendation_template: row.get(6)?,
                priority: row.get(7)?,
                enabled: row.get::<_, i32>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    Ok(rules)
}

/// Update a recommendation rule
#[allow(dead_code, clippy::needless_pass_by_value)]
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn update_recommendation_rule(
    workspace_path: String,
    rule_id: i64,
    threshold_value: Option<f64>,
    threshold_count: Option<i32>,
    priority: Option<i32>,
    enabled: Option<bool>,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Build dynamic update based on provided values
    if let Some(tv) = threshold_value {
        conn.execute(
            "UPDATE recommendation_rules SET threshold_value = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![tv, rule_id],
        )
        .map_err(|e| format!("Failed to update threshold_value: {e}"))?;
    }

    if let Some(tc) = threshold_count {
        conn.execute(
            "UPDATE recommendation_rules SET threshold_count = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![tc, rule_id],
        )
        .map_err(|e| format!("Failed to update threshold_count: {e}"))?;
    }

    if let Some(p) = priority {
        conn.execute(
            "UPDATE recommendation_rules SET priority = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![p, rule_id],
        )
        .map_err(|e| format!("Failed to update priority: {e}"))?;
    }

    if let Some(e) = enabled {
        conn.execute(
            "UPDATE recommendation_rules SET enabled = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![i32::from(e), rule_id],
        )
        .map_err(|e| format!("Failed to update enabled: {e}"))?;
    }

    Ok(())
}

/// Generate recommendations from analytics data
/// Evaluates all enabled rules against current data and creates new recommendations
#[allow(dead_code, clippy::too_many_lines)]
#[tauri::command]
pub fn generate_recommendations(
    workspace_path: &str,
) -> Result<RecommendationGenerationResult, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut recommendations_created = 0;
    let mut rules_evaluated = 0;
    let mut patterns_analyzed = 0;

    // Get all enabled rules
    #[allow(clippy::type_complexity)]
    let rules: Vec<(i64, String, String, Option<f64>, Option<i32>, String)> = conn
        .prepare(
            "SELECT id, name, rule_type, threshold_value, threshold_count, recommendation_template
             FROM recommendation_rules
             WHERE enabled = 1
             ORDER BY priority ASC",
        )
        .map_err(|e| format!("Failed to prepare rules query: {e}"))?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
        })
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    for (_rule_id, rule_name, rule_type, threshold_value, threshold_count, template) in rules {
        rules_evaluated += 1;

        match rule_type.as_str() {
            "warning" => {
                // Low success pattern warning
                let threshold = threshold_value.unwrap_or(0.5);
                let min_uses = threshold_count.unwrap_or(5);

                let low_success_patterns: Vec<(i64, String, f64, i32)> = conn
                    .prepare(
                        "SELECT id, pattern_text, success_rate, times_used
                         FROM prompt_patterns
                         WHERE success_rate < ?1 AND times_used >= ?2
                         ORDER BY success_rate ASC
                         LIMIT 10",
                    )
                    .map_err(|e| format!("Failed to prepare pattern query: {e}"))?
                    .query_map(params![threshold, min_uses], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .map_err(|e| format!("Failed to query patterns: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect patterns: {e}"))?;

                for (pattern_id, pattern_text, success_rate, times_used) in low_success_patterns {
                    patterns_analyzed += 1;

                    // Check if recommendation already exists for this pattern
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM recommendations
                             WHERE type = 'warning' AND evidence LIKE ?1 AND dismissed_at IS NULL",
                            [format!("%\"pattern_id\":{pattern_id}%")],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);

                    if exists {
                        continue;
                    }

                    let title = format!(
                        "Low success pattern: {}",
                        if pattern_text.len() > 50 {
                            format!("{}...", &pattern_text[..50])
                        } else {
                            pattern_text.clone()
                        }
                    );

                    let description = template
                        .replace("{{pattern}}", &pattern_text)
                        .replace("{{success_rate}}", &format!("{:.0}", success_rate * 100.0))
                        .replace("{{uses}}", &times_used.to_string());

                    let evidence = serde_json::json!({
                        "pattern_id": pattern_id,
                        "pattern_text": pattern_text,
                        "success_rate": success_rate,
                        "times_used": times_used,
                        "rule_name": rule_name
                    })
                    .to_string();

                    // Confidence based on number of uses (more data = higher confidence)
                    let confidence = (1.0 - success_rate) * (f64::from(times_used) / 20.0).min(1.0);

                    conn.execute(
                        "INSERT INTO recommendations (type, title, description, confidence, evidence, created_at)
                         VALUES ('warning', ?1, ?2, ?3, ?4, datetime('now'))",
                        params![title, description, confidence, evidence],
                    )
                    .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                    recommendations_created += 1;
                }
            }
            "cost" => {
                // High cost alert
                let cost_multiplier = threshold_value.unwrap_or(2.0);
                let min_occurrences = threshold_count.unwrap_or(3);

                // Get average cost per role
                let avg_costs: Vec<(String, f64, i32)> = conn
                    .prepare(
                        "SELECT a.role,
                                AVG((t.prompt_tokens * 0.003 + t.completion_tokens * 0.015) / 1000.0) as avg_cost,
                                COUNT(*) as count
                         FROM agent_activity a
                         JOIN token_usage t ON a.id = t.activity_id
                         GROUP BY a.role
                         HAVING COUNT(*) >= ?1",
                    )
                    .map_err(|e| format!("Failed to prepare cost query: {e}"))?
                    .query_map([min_occurrences], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(|e| format!("Failed to query costs: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect costs: {e}"))?;

                if avg_costs.is_empty() {
                    continue;
                }

                #[allow(clippy::cast_precision_loss)]
                let overall_avg: f64 =
                    avg_costs.iter().map(|(_, c, _)| c).sum::<f64>() / avg_costs.len() as f64;

                for (role, avg_cost, count) in &avg_costs {
                    if *avg_cost > overall_avg * cost_multiplier {
                        let actual_multiplier = avg_cost / overall_avg;

                        // Check if recommendation already exists
                        let exists: bool = conn
                            .query_row(
                                "SELECT 1 FROM recommendations
                                 WHERE type = 'cost' AND context_role = ?1 AND dismissed_at IS NULL",
                                [role],
                                |_| Ok(true),
                            )
                            .unwrap_or(false);

                        if exists {
                            continue;
                        }

                        let title = format!("High cost alert: {role}");
                        let description = template
                            .replace("{{cost_multiplier}}", &format!("{actual_multiplier:.1}"))
                            .replace("{{actual_cost}}", &format!("${avg_cost:.4}"))
                            .replace("{{avg_cost}}", &format!("${overall_avg:.4}"));

                        let evidence = serde_json::json!({
                            "role": role,
                            "avg_cost": avg_cost,
                            "overall_avg": overall_avg,
                            "multiplier": actual_multiplier,
                            "sample_size": count,
                            "rule_name": rule_name
                        })
                        .to_string();

                        let confidence = ((actual_multiplier - 1.0) / cost_multiplier).min(1.0)
                            * (f64::from(*count) / 50.0).min(1.0);

                        conn.execute(
                            "INSERT INTO recommendations (type, title, description, confidence, evidence, context_role, created_at)
                             VALUES ('cost', ?1, ?2, ?3, ?4, ?5, datetime('now'))",
                            params![title, description, confidence, evidence, role],
                        )
                        .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                        recommendations_created += 1;
                    }
                }
            }
            "prompt" => {
                // Similar successful prompt suggestion
                let min_success_rate = threshold_value.unwrap_or(0.8);
                let min_uses = threshold_count.unwrap_or(3);

                // Find high-success patterns to suggest
                let successful_patterns: Vec<(i64, String, String, f64, i32)> = conn
                    .prepare(
                        "SELECT id, pattern_text, COALESCE(category, 'general'), success_rate, times_used
                         FROM prompt_patterns
                         WHERE success_rate >= ?1 AND times_used >= ?2
                         ORDER BY success_rate DESC, times_used DESC
                         LIMIT 20",
                    )
                    .map_err(|e| format!("Failed to prepare pattern query: {e}"))?
                    .query_map(params![min_success_rate, min_uses], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                    })
                    .map_err(|e| format!("Failed to query patterns: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect patterns: {e}"))?;

                for (pattern_id, pattern_text, category, success_rate, times_used) in
                    successful_patterns
                {
                    patterns_analyzed += 1;

                    // Check if recommendation already exists
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM recommendations
                             WHERE type = 'prompt' AND evidence LIKE ?1 AND dismissed_at IS NULL",
                            [format!("%\"pattern_id\":{pattern_id}%")],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);

                    if exists {
                        continue;
                    }

                    let title = format!(
                        "Successful pattern: {}",
                        if pattern_text.len() > 40 {
                            format!("{}...", &pattern_text[..40])
                        } else {
                            pattern_text.clone()
                        }
                    );

                    let description = template
                        .replace("{{similar_prompt}}", &pattern_text)
                        .replace("{{success_rate}}", &format!("{:.0}", success_rate * 100.0))
                        .replace("{{uses}}", &times_used.to_string());

                    let evidence = serde_json::json!({
                        "pattern_id": pattern_id,
                        "pattern_text": pattern_text,
                        "category": category,
                        "success_rate": success_rate,
                        "times_used": times_used,
                        "rule_name": rule_name
                    })
                    .to_string();

                    let confidence = success_rate * (f64::from(times_used) / 10.0).min(1.0);

                    conn.execute(
                        "INSERT INTO recommendations (type, title, description, confidence, evidence, context_task_type, created_at)
                         VALUES ('prompt', ?1, ?2, ?3, ?4, ?5, datetime('now'))",
                        params![title, description, confidence, evidence, category],
                    )
                    .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                    recommendations_created += 1;
                }
            }
            "role" => {
                // Role effectiveness suggestion
                let min_success_rate = threshold_value.unwrap_or(0.75);
                let min_uses = threshold_count.unwrap_or(5);

                // Get role success rates
                let role_stats: Vec<(String, f64, i32)> = conn
                    .prepare(
                        "SELECT role,
                                CAST(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END) AS REAL) /
                                NULLIF(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 0) as success_rate,
                                COUNT(*) as count
                         FROM agent_activity
                         WHERE work_found = 1
                         GROUP BY role
                         HAVING COUNT(*) >= ?1 AND success_rate IS NOT NULL",
                    )
                    .map_err(|e| format!("Failed to prepare role query: {e}"))?
                    .query_map([min_uses], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(|e| format!("Failed to query roles: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect roles: {e}"))?;

                // Calculate overall success rate
                let (total_success, total_with_work): (i32, i32) = conn
                    .query_row(
                        "SELECT
                            COALESCE(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END), 0),
                            COALESCE(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 1)
                         FROM agent_activity",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap_or((0, 1));

                let overall_rate = if total_with_work > 0 {
                    f64::from(total_success) / f64::from(total_with_work)
                } else {
                    0.0
                };

                for (role, success_rate, count) in role_stats {
                    if success_rate >= min_success_rate && success_rate > overall_rate * 1.1 {
                        // Check if recommendation already exists
                        let exists: bool = conn
                            .query_row(
                                "SELECT 1 FROM recommendations
                                 WHERE type = 'role' AND context_role = ?1 AND dismissed_at IS NULL",
                                [&role],
                                |_| Ok(true),
                            )
                            .unwrap_or(false);

                        if exists {
                            continue;
                        }

                        let title = format!("{role} excels at task completion");
                        let description = template
                            .replace("{{role}}", &role)
                            .replace("{{success_rate}}", &format!("{:.0}", success_rate * 100.0))
                            .replace("{{current_rate}}", &format!("{:.0}", overall_rate * 100.0));

                        let evidence = serde_json::json!({
                            "role": role,
                            "success_rate": success_rate,
                            "overall_rate": overall_rate,
                            "sample_size": count,
                            "rule_name": rule_name
                        })
                        .to_string();

                        let confidence = success_rate * (f64::from(count) / 20.0).min(1.0);

                        conn.execute(
                            "INSERT INTO recommendations (type, title, description, confidence, evidence, context_role, created_at)
                             VALUES ('role', ?1, ?2, ?3, ?4, ?5, datetime('now'))",
                            params![title, description, confidence, evidence, role],
                        )
                        .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                        recommendations_created += 1;
                    }
                }
            }
            "timing" => {
                // Optimal timing suggestions - analyze success by hour of day
                let min_success_rate = threshold_value.unwrap_or(0.7);
                let min_samples = threshold_count.unwrap_or(10);

                // Get success rate by hour
                let hourly_stats: Vec<(i32, f64, i32)> = conn
                    .prepare(
                        "SELECT
                            CAST(strftime('%H', timestamp) AS INTEGER) as hour,
                            CAST(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END) AS REAL) /
                            NULLIF(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 0) as success_rate,
                            COUNT(*) as count
                         FROM agent_activity
                         WHERE work_found = 1
                         GROUP BY hour
                         HAVING COUNT(*) >= ?1 AND success_rate IS NOT NULL
                         ORDER BY success_rate DESC",
                    )
                    .map_err(|e| format!("Failed to prepare timing query: {e}"))?
                    .query_map([min_samples], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(|e| format!("Failed to query timing: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect timing: {e}"))?;

                // Find peak performance hours
                let peak_hours: Vec<_> = hourly_stats
                    .iter()
                    .filter(|(_, sr, _)| *sr >= min_success_rate)
                    .collect();

                if peak_hours.len() >= 2 {
                    // Check if timing recommendation already exists
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM recommendations
                             WHERE type = 'timing' AND dismissed_at IS NULL",
                            [],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);

                    if !exists {
                        let start_hour = peak_hours.iter().map(|(h, _, _)| *h).min().unwrap_or(9);
                        let end_hour = peak_hours.iter().map(|(h, _, _)| *h).max().unwrap_or(17);
                        #[allow(clippy::cast_precision_loss)]
                        let avg_success: f64 = peak_hours.iter().map(|(_, sr, _)| *sr).sum::<f64>()
                            / peak_hours.len() as f64;

                        let title = "Optimal timing identified".to_string();
                        let description = template
                            .replace("{{success_rate}}", &format!("{:.0}", avg_success * 100.0))
                            .replace("{{start_hour}}", &format!("{start_hour}:00"))
                            .replace("{{end_hour}}", &format!("{end_hour}:00"));

                        let evidence = serde_json::json!({
                            "peak_hours": peak_hours.iter().map(|(h, sr, c)| {
                                serde_json::json!({"hour": h, "success_rate": sr, "count": c})
                            }).collect::<Vec<_>>(),
                            "start_hour": start_hour,
                            "end_hour": end_hour,
                            "avg_success_rate": avg_success,
                            "rule_name": rule_name
                        })
                        .to_string();

                        #[allow(clippy::cast_precision_loss)]
                        let confidence = avg_success * (peak_hours.len() as f64 / 8.0).min(1.0);

                        conn.execute(
                            "INSERT INTO recommendations (type, title, description, confidence, evidence, created_at)
                             VALUES ('timing', ?1, ?2, ?3, ?4, datetime('now'))",
                            params![title, description, confidence, evidence],
                        )
                        .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                        recommendations_created += 1;
                    }
                }
            }
            _ => {
                // Unknown rule type, skip
            }
        }
    }

    Ok(RecommendationGenerationResult {
        recommendations_created,
        rules_evaluated,
        patterns_analyzed,
    })
}

/// Get recommendation statistics
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct RecommendationStats {
    pub total_recommendations: i32,
    pub active_recommendations: i32,
    pub dismissed_recommendations: i32,
    pub acted_on_recommendations: i32,
    pub recommendations_by_type: Vec<TypeCount>,
    pub avg_confidence: f64,
}

/// Type count for statistics
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct TypeCount {
    pub recommendation_type: String,
    pub count: i32,
    pub avg_confidence: f64,
}

/// Get statistics about recommendations
#[allow(dead_code)]
#[tauri::command]
pub fn get_recommendation_stats(workspace_path: &str) -> Result<RecommendationStats, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Total recommendations
    let total_recommendations: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendations", [], |row| row.get(0))
        .unwrap_or(0);

    // Active (non-dismissed) recommendations
    let active_recommendations: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendations WHERE dismissed_at IS NULL", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    // Dismissed recommendations
    let dismissed_recommendations: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM recommendations WHERE dismissed_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Acted on recommendations
    let acted_on_recommendations: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendations WHERE acted_on = 1", [], |row| row.get(0))
        .unwrap_or(0);

    // Recommendations by type
    let mut stmt = conn
        .prepare(
            "SELECT type, COUNT(*), AVG(confidence)
             FROM recommendations
             GROUP BY type
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let recommendations_by_type: Vec<TypeCount> = stmt
        .query_map([], |row| {
            Ok(TypeCount {
                recommendation_type: row.get(0)?,
                count: row.get(1)?,
                avg_confidence: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            })
        })
        .map_err(|e| format!("Failed to query types: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect types: {e}"))?;

    // Average confidence
    let avg_confidence: f64 = conn
        .query_row("SELECT COALESCE(AVG(confidence), 0.0) FROM recommendations", [], |row| {
            row.get(0)
        })
        .unwrap_or(0.0);

    Ok(RecommendationStats {
        total_recommendations,
        active_recommendations,
        dismissed_recommendations,
        acted_on_recommendations,
        recommendations_by_type,
        avg_confidence,
    })
}
