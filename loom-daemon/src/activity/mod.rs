//! Activity tracking module for agent inputs, outputs, and metrics.
//!
//! This module provides functionality for tracking and persisting agent
//! activity data, including:
//!
//! - Agent inputs (commands sent to terminals)
//! - Agent outputs (terminal responses)
//! - Resource usage and cost analytics
//! - Quality metrics parsed from terminal output
//! - Activity history for UI display
//!
//! Aggregate productivity/effectiveness queries are served by [`stats::StatsQueries`],
//! not by per-metric CRUD methods on [`ActivityDb`] (the older API this module
//! provided was removed as dead code, superseded by `StatsQueries`, see #7567).
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

mod claims;
mod cost_analytics;
mod db;
mod models;
pub mod pricing_card;
mod prompts;
mod quality;
pub mod resource_usage;
mod schema;
pub mod stats;
pub mod test_parser;
pub mod transcript_ingest;
pub mod transcript_parse;
pub mod tuning;
mod usage_report;
pub mod weekly_point_calibration;
pub mod weekly_point_history;

// Re-export public types from models
// Only export types that are used by other modules
pub use models::{ActivityEntry, AgentInput, AgentOutput, InputContext, InputType};

// These types are available for future use but not currently imported elsewhere
#[allow(unused_imports)]
pub use models::{
    AgentMetric, LintResults, PrReworkStats, ProductivitySummary, PromptChanges, PromptForgeEvent,
    PromptForgeEventType, PromptSuccessStats, QualityMetrics, TestResults, TokenUsage,
};

// Cost analytics types (Issue #1064)
#[allow(unused_imports)]
pub use models::{
    BudgetConfig, BudgetPeriod, BudgetStatus, CostByIssue, CostByPr, CostByRole, CostSummary,
    RunwayProjection,
};

// Re-export the database struct
pub use db::ActivityDb;

// Re-export schema initialization so callers that need a raw `rusqlite::Connection`
// (e.g. the GitHub metrics collector) can self-initialize the activity DB schema
// without routing through `ActivityDb`.
pub use schema::init_schema;

// Transcript token-usage ingestion (Issue #8059) — the dispatch-path writer
// for `resource_usage`, which the managed-terminal-only IPC path never reached.
pub use transcript_ingest::{ingest, IngestOptions, IngestStats};
pub use transcript_parse::{attribute_role, parse_transcript, ParsedTranscript, UsageBucket};

// Re-export resource usage parsing and cost calculation
// Used internally by db.rs for terminal output parsing
// Note: db.rs accesses these via super::resource_usage, so these re-exports
// are provided for external crate access (future MCP servers, etc.)
#[allow(unused_imports)]
pub use resource_usage::{detect_provider, parse_resource_usage, ModelPricing, ResourceUsage};

// Re-export the runtime-loadable rate card (#8177) alongside the pricing type
// it feeds, so a consumer can inspect which card is in effect.
#[allow(unused_imports)]
pub use pricing_card::{PricingCard, PricingCardError};

// Re-export stats types and trait for metrics queries
// These types are used for the `loom stats` CLI commands
#[allow(unused_imports)]
pub use stats::{
    AgentEffectiveness, CostPerIssue, CostRow, DailyVelocity, EffectivenessRow, StatsQueries,
    StatsSummary, SummaryMetrics, VelocityRow, WeeklyVelocity,
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
pub use models::{ClaimResult, ClaimType, ClaimsSummary, IssueClaim};

// Token/cost usage report types (Issue #8062) — `loom-daemon usage-report`.
pub use usage_report::{UsageReportGroupBy, UsageReportRow};

// $-eq-per-weekly-limit-point calibration (Issue #8348, part of #8063) — the
// pure join + step-change detector; its `loom-daemon health` wiring is a
// separate sub-issue.
pub use weekly_point_calibration::{
    calibrate, CalibrationSeries, DailyValue, DayCalibration, StepChangeDirection, BASELINE_DAYS,
    STEP_CHANGE_FOLD_THRESHOLD,
};

// Daily weekly-limit-point history (Issue #8347, part of #8063) — the sampling
// side is written best-effort by `loom-daemon tokens check --ranking`; the read
// side is the daily series #8063 joins against `usage-report --by day`.
pub use weekly_point_history::{
    record_daily_sample_best_effort, sum_account_points, WeeklyPointSample,
};
