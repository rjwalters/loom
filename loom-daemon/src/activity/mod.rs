//! Activity tracking module for agent inputs, outputs, and metrics.
//!
//! This module provides functionality for tracking and persisting agent
//! activity data, including:
//!
//! - Agent inputs (commands sent to terminals)
//! - Agent outputs (terminal responses)
//! - Productivity metrics (task tracking, token usage)
//! - Quality metrics (test results, lint/format status)
//! - Activity history for UI display
//!
//! # Module Structure
//!
//! - [`models`]: Type definitions for activity data structures
//! - [`schema`]: Database schema and migrations
//! - [`db`]: Database operations and queries
//! - [`test_parser`]: Parse test/lint output from terminal
//!
//! # Example
//!
//! ```ignore
//! use activity::{ActivityDb, AgentInput, InputType, InputContext};
//!
//! let db = ActivityDb::new("activity.db".into())?;
//!
//! let input = AgentInput {
//!     id: None,
//!     terminal_id: "terminal-1".to_string(),
//!     timestamp: Utc::now(),
//!     input_type: InputType::Manual,
//!     content: "ls -la".to_string(),
//!     agent_role: Some("builder".to_string()),
//!     context: InputContext::default(),
//! };
//!
//! db.record_input(&input)?;
//! ```

mod db;
mod models;
pub mod resource_usage;
mod schema;
pub mod stats;
pub mod test_parser;
pub mod tuning;

// Re-export public types from models
// Only export types that are used by other modules
pub use models::{ActivityEntry, AgentInput, AgentOutput, InputContext, InputType};

// These types are available for future use but not currently imported elsewhere
#[allow(unused_imports)]
pub use models::{
    AgentMetric, LintResults, PrReworkStats, ProductivitySummary, PromptChanges, PromptGitHubEvent,
    PromptGitHubEventType, PromptSuccessStats, QualityMetrics, TestResults, TokenUsage,
};

// Cost analytics types (Issue #1064)
#[allow(unused_imports)]
pub use models::{
    BudgetConfig, BudgetPeriod, BudgetStatus, CostByIssue, CostByPr, CostByRole, CostSummary,
    RunwayProjection,
};

// Re-export the database struct
pub use db::ActivityDb;

// Re-export resource usage parsing and cost calculation
// Used internally by db.rs for terminal output parsing
// Note: db.rs accesses these via super::resource_usage, so these re-exports
// are provided for external crate access (future MCP servers, etc.)
#[allow(unused_imports)]
pub use resource_usage::{detect_provider, parse_resource_usage, ModelPricing, ResourceUsage};

// Re-export stats types and trait for metrics queries
// These types are used for the `loom stats` CLI commands
#[allow(unused_imports)]
pub use stats::{
    AgentEffectiveness, CostPerIssue, DailyVelocity, StatsQueries, StatsSummary, WeeklyVelocity,
};

// Re-export tuning types and functions (Issue #1074)
// These types are used for self-tuning based on effectiveness data
#[allow(unused_imports)]
pub use tuning::{
    create_tuning_schema, EffectivenessSnapshot, ProposalStatus, TunableParameter, TuningConfig,
    TuningHistory, TuningProposal, TuningSummary,
};

// Issue claim registry types (Issue #1159)
// Used for reliable work distribution and crash recovery
pub use models::{ClaimResult, ClaimsSummary, ClaimType, IssueClaim};
