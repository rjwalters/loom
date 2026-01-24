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

    // Human rating (1-5 stars, optional)
    pub human_rating: Option<i32>,
}
