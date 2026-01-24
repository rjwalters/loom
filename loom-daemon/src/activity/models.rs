//! Type definitions for activity tracking.
//!
//! This module contains all the data structures used for tracking agent
//! inputs, outputs, metrics, and activity entries.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Type of input sent to terminal
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Manual,          // User-initiated command (direct keyboard input)
    Autonomous,      // Agent autonomous action (interval prompts)
    System,          // System-initiated (e.g., setup commands)
    UserInstruction, // User-initiated prompts via UI buttons
}

impl InputType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Manual => "manual",
            Self::Autonomous => "autonomous",
            Self::System => "system",
            Self::UserInstruction => "user_instruction",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(Self::Manual),
            "autonomous" => Some(Self::Autonomous),
            "system" => Some(Self::System),
            "user_instruction" => Some(Self::UserInstruction),
            _ => None,
        }
    }
}

/// Context information for an agent input
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i32>,
}

/// Agent input record
#[derive(Debug, Clone)]
pub struct AgentInput {
    #[allow(dead_code)]
    pub id: Option<i64>,
    pub terminal_id: String,
    pub timestamp: DateTime<Utc>,
    pub input_type: InputType,
    pub content: String,
    pub agent_role: Option<String>,
    pub context: InputContext,
}

/// Agent output record (terminal output sample)
#[derive(Debug, Clone)]
pub struct AgentOutput {
    #[allow(dead_code)]
    pub id: Option<i64>,
    pub input_id: Option<i64>,
    pub terminal_id: String,
    pub timestamp: DateTime<Utc>,
    pub content: Option<String>,
    pub content_preview: Option<String>,
    pub exit_code: Option<i32>,
    pub metadata: Option<String>,
}

/// Agent productivity metrics for a task
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentMetric {
    pub id: Option<i64>,
    pub terminal_id: String,
    pub agent_role: String,
    pub agent_system: String,
    pub task_type: Option<String>,
    pub github_issue: Option<i32>,
    pub github_pr: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub wall_time_seconds: Option<i64>,
    pub active_time_seconds: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost_usd: f64,
    pub status: String,
    pub outcome_type: Option<String>,
    pub test_failures: i32,
    pub ci_failures: i32,
    pub commits_count: i32,
    pub lines_changed: i32,
    pub context: Option<String>,
}

/// Token usage record for a single API request
///
/// Enhanced to track cache tokens, duration, and provider for LLM resource usage
/// analytics (Issue #1013).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TokenUsage {
    pub id: Option<i64>,
    pub input_id: Option<i64>,
    pub metric_id: Option<i64>,
    pub timestamp: DateTime<Utc>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub model: Option<String>,
    pub estimated_cost_usd: f64,
    // Enhanced fields for resource tracking (Issue #1013)
    /// Cache read tokens (prompt caching - reduces input cost)
    pub tokens_cache_read: Option<i64>,
    /// Cache write tokens (prompt caching - initial cache creation)
    pub tokens_cache_write: Option<i64>,
    /// API response time in milliseconds
    pub duration_ms: Option<i64>,
    /// LLM provider: 'anthropic', 'openai', 'google', etc.
    pub provider: Option<String>,
}

/// Combined activity entry (input + output)
/// Used for displaying terminal activity history in UI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub input_id: i64,
    pub timestamp: DateTime<Utc>,
    pub input_type: InputType,
    pub prompt: String,
    pub agent_role: Option<String>,
    pub git_branch: Option<String>,

    // Output data (optional, joined from agent_outputs)
    pub output_preview: Option<String>,
    pub exit_code: Option<i32>,
    pub output_timestamp: Option<DateTime<Utc>>,
}

/// Type alias for productivity summary: (`agent_system`, `tasks_completed`, `avg_minutes`, `avg_tokens`, `total_cost`)
pub type ProductivitySummary = Vec<(String, i64, f64, f64, f64)>;

/// Git changes associated with a prompt
/// Links individual prompts to the git commits/changes they caused
#[derive(Debug, Clone)]
pub struct PromptChanges {
    #[allow(dead_code)]
    pub id: Option<i64>,
    pub input_id: i64,
    pub before_commit: Option<String>,
    pub after_commit: Option<String>,
    pub files_changed: i32,
    pub lines_added: i32,
    pub lines_removed: i32,
    pub tests_added: i32,
    pub tests_modified: i32,
}

/// GitHub event types that can be parsed from terminal output
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptGitHubEventType {
    IssueCreated,
    IssueClosed,
    PrCreated,
    PrMerged,
    PrClosed,
    LabelAdded,
    LabelRemoved,
    ReviewSubmitted,
}

impl PromptGitHubEventType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::IssueCreated => "issue_created",
            Self::IssueClosed => "issue_closed",
            Self::PrCreated => "pr_created",
            Self::PrMerged => "pr_merged",
            Self::PrClosed => "pr_closed",
            Self::LabelAdded => "label_added",
            Self::LabelRemoved => "label_removed",
            Self::ReviewSubmitted => "review_submitted",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "issue_created" => Some(Self::IssueCreated),
            "issue_closed" => Some(Self::IssueClosed),
            "pr_created" => Some(Self::PrCreated),
            "pr_merged" => Some(Self::PrMerged),
            "pr_closed" => Some(Self::PrClosed),
            "label_added" => Some(Self::LabelAdded),
            "label_removed" => Some(Self::LabelRemoved),
            "review_submitted" => Some(Self::ReviewSubmitted),
            _ => None,
        }
    }
}

/// Prompt-GitHub correlation record for tracking which prompts triggered GitHub actions
#[derive(Debug, Clone)]
pub struct PromptGitHubEvent {
    #[allow(dead_code)]
    pub id: Option<i64>,
    pub input_id: Option<i64>,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub label_before: Option<Vec<String>>,
    pub label_after: Option<Vec<String>>,
    pub event_type: PromptGitHubEventType,
}

/// Test results parsed from terminal output
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestResults {
    pub passed: i32,
    pub failed: i32,
    pub skipped: i32,
    pub runner: Option<String>,
}

/// Lint/format results parsed from terminal output
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LintResults {
    pub lint_errors: i32,
    pub format_errors: i32,
}

/// PR rework statistics for tracking review cycles
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrReworkStats {
    /// Average number of rework cycles for PRs that needed changes
    pub avg_rework_count: f64,
    /// Maximum rework count seen for any PR
    pub max_rework_count: i32,
    /// Number of PRs that required at least one rework cycle
    pub prs_with_rework: i64,
    /// Total number of PRs with review tracking
    pub total_prs_tracked: i64,
}

/// Prompt-to-test success correlation statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptSuccessStats {
    /// Total number of prompts that have test results
    pub total_prompts_with_tests: i64,
    /// Number of prompts where all tests passed (failed = 0, passed > 0)
    pub prompts_with_all_passing: i64,
    /// Average test pass rate across all prompts (passed / (passed + failed))
    pub avg_test_pass_rate: f64,
}

/// Quality metrics record for tracking test outcomes and code quality
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct QualityMetrics {
    pub id: Option<i64>,
    pub input_id: Option<i64>,
    pub timestamp: DateTime<Utc>,

    // Test results
    pub tests_passed: Option<i32>,
    pub tests_failed: Option<i32>,
    pub tests_skipped: Option<i32>,
    pub test_runner: Option<String>,

    // Lint/format results
    pub lint_errors: Option<i32>,
    pub format_errors: Option<i32>,

    // Build status
    pub build_success: Option<bool>,

    // PR review outcomes
    pub pr_approved: Option<bool>,
    pub pr_changes_requested: Option<bool>,

    // Rework tracking (Issue #1054)
    // Counts how many review cycles a PR goes through
    pub rework_count: Option<i32>,

    // Human rating (1-5 stars, optional)
    pub human_rating: Option<i32>,
}

// ============================================================================
// Cost Analytics Models (Issue #1064)
// ============================================================================

/// Budget period type for budget configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    Monthly,
}

impl BudgetPeriod {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }
}

/// Budget configuration record for tracking spending limits
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetConfig {
    pub id: Option<i64>,
    pub period: BudgetPeriod,
    pub limit_usd: f64,
    /// Alert threshold as a fraction (0.0-1.0), default 0.8 (80%)
    pub alert_threshold: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 100.0,
            alert_threshold: 0.8,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        }
    }
}

/// Cost summary for a specific time period
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSummary {
    /// Start of the period
    pub start_date: DateTime<Utc>,
    /// End of the period
    pub end_date: DateTime<Utc>,
    /// Total cost in USD
    pub total_cost: f64,
    /// Number of API requests
    pub request_count: i64,
    /// Total input tokens consumed
    pub total_input_tokens: i64,
    /// Total output tokens consumed
    pub total_output_tokens: i64,
    /// Average cost per request
    pub avg_cost_per_request: f64,
}

/// Cost breakdown by agent role
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostByRole {
    pub agent_role: String,
    pub total_cost: f64,
    pub request_count: i64,
    pub avg_cost: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub first_usage: Option<DateTime<Utc>>,
    pub last_usage: Option<DateTime<Utc>>,
}

/// Cost breakdown by GitHub issue
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostByIssue {
    pub issue_number: i32,
    pub total_cost: f64,
    pub prompt_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub first_usage: Option<DateTime<Utc>>,
    pub last_usage: Option<DateTime<Utc>>,
}

/// Cost breakdown by pull request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostByPr {
    pub pr_number: i32,
    pub total_cost: f64,
    pub prompt_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub first_usage: Option<DateTime<Utc>>,
    pub last_usage: Option<DateTime<Utc>>,
}

/// Budget status showing current spend against limits
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatus {
    /// The budget configuration
    pub config: BudgetConfig,
    /// Amount spent in the current period
    pub spent: f64,
    /// Remaining budget
    pub remaining: f64,
    /// Usage as a percentage (0-100)
    pub usage_percent: f64,
    /// Whether the alert threshold has been crossed
    pub alert_triggered: bool,
    /// Whether the budget has been exceeded
    pub budget_exceeded: bool,
    /// Current period start
    pub period_start: DateTime<Utc>,
    /// Current period end
    pub period_end: DateTime<Utc>,
}

/// Runway projection based on burn rate
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunwayProjection {
    /// Current remaining budget
    pub remaining_budget: f64,
    /// Average daily cost over the lookback period
    pub avg_daily_cost: f64,
    /// Estimated days until budget exhaustion
    pub days_remaining: f64,
    /// Projected exhaustion date
    pub exhaustion_date: Option<DateTime<Utc>>,
    /// Number of days used for the calculation
    pub lookback_days: i32,
}
