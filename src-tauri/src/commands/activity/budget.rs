use super::db::{calculate_cost, open_activity_db};
use serde::{Deserialize, Serialize};

/// Budget configuration stored in workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetConfig {
    /// Daily budget limit in USD
    pub daily_limit: Option<f64>,
    /// Weekly budget limit in USD
    pub weekly_limit: Option<f64>,
    /// Monthly budget limit in USD
    pub monthly_limit: Option<f64>,
    /// Alert thresholds as percentages (e.g., [50, 75, 90, 100])
    pub alert_thresholds: Vec<i32>,
    /// Whether budget alerts are enabled
    pub alerts_enabled: bool,
    /// Timestamps of last alerts sent (to prevent spam)
    pub last_alerts: BudgetLastAlerts,
}

/// Last alert timestamps by period
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BudgetLastAlerts {
    pub daily: std::collections::HashMap<i32, i64>,
    pub weekly: std::collections::HashMap<i32, i64>,
    pub monthly: std::collections::HashMap<i32, i64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_limit: None,
            weekly_limit: None,
            monthly_limit: None,
            alert_thresholds: vec![50, 75, 90, 100],
            alerts_enabled: true,
            last_alerts: BudgetLastAlerts::default(),
        }
    }
}

/// Get the budget config file path
fn get_budget_config_path(workspace_path: &str) -> std::path::PathBuf {
    std::path::Path::new(workspace_path)
        .join(".loom")
        .join("budget-config.json")
}

/// Get the current budget configuration
#[tauri::command]
pub fn get_budget_config(workspace_path: &str) -> Result<BudgetConfig, String> {
    let config_path = get_budget_config_path(workspace_path);

    if !config_path.exists() {
        return Ok(BudgetConfig::default());
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("Failed to read budget config: {e}"))?;

    serde_json::from_str(&content).map_err(|e| format!("Failed to parse budget config: {e}"))
}

/// Save the budget configuration
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
pub fn save_budget_config(workspace_path: &str, config: BudgetConfig) -> Result<(), String> {
    let config_path = get_budget_config_path(workspace_path);

    // Ensure .loom directory exists
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
    }

    let content = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize budget config: {e}"))?;

    std::fs::write(&config_path, content)
        .map_err(|e| format!("Failed to write budget config: {e}"))?;

    Ok(())
}

/// Cost breakdown by issue
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueCost {
    pub issue_number: i32,
    pub issue_title: String,
    pub total_cost: f64,
    pub total_tokens: i64,
    pub prompt_count: i32,
    pub last_activity: String,
}

/// Get cost breakdown by issue for a time range
#[tauri::command]
pub fn get_costs_by_issue(
    workspace_path: &str,
    time_range: &str,
) -> Result<Vec<IssueCost>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let since_clause = match time_range {
        "today" => "datetime('now', 'start of day')",
        "week" => "datetime('now', '-7 days')",
        "month" => "datetime('now', '-30 days')",
        _ => "datetime('1970-01-01')",
    };

    // Query costs grouped by issue, joining with prompt_github to get issue numbers
    let query = format!(
        "SELECT
            pg.issue_number,
            COALESCE(pg.issue_number, 0) as issue_num,
            COUNT(DISTINCT a.id) as prompt_count,
            COALESCE(SUM(t.prompt_tokens), 0) as total_prompt,
            COALESCE(SUM(t.completion_tokens), 0) as total_completion,
            COALESCE(SUM(t.total_tokens), 0) as total_tokens,
            MAX(a.timestamp) as last_activity
         FROM agent_activity a
         LEFT JOIN token_usage t ON a.id = t.activity_id
         LEFT JOIN prompt_github pg ON a.id = pg.activity_id
         WHERE a.timestamp >= {since_clause}
           AND pg.issue_number IS NOT NULL
         GROUP BY pg.issue_number
         ORDER BY total_tokens DESC
         LIMIT 50"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let costs = stmt
        .query_map([], |row| {
            let issue_number: i32 = row.get(1)?;
            let prompt_count: i32 = row.get(2)?;
            let prompt_tokens: i64 = row.get(3)?;
            let completion_tokens: i64 = row.get(4)?;
            let total_tokens: i64 = row.get(5)?;
            let last_activity: String = row.get(6)?;

            Ok(IssueCost {
                issue_number,
                issue_title: format!("Issue #{issue_number}"), // Title would need GitHub API
                total_cost: calculate_cost(prompt_tokens, completion_tokens),
                total_tokens,
                prompt_count,
                last_activity,
            })
        })
        .map_err(|e| format!("Failed to query issue costs: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect costs: {e}"))?;

    Ok(costs)
}
