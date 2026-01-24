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
mod schema;
pub mod test_parser;

// Re-export public types from models
// Only export types that are used by other modules
pub use models::{ActivityEntry, AgentInput, AgentOutput, InputContext, InputType};

// These types are available for future use but not currently imported elsewhere
#[allow(unused_imports)]
pub use models::{AgentMetric, LintResults, ProductivitySummary, QualityMetrics, TestResults, TokenUsage};

// Re-export the database struct
pub use db::ActivityDb;
