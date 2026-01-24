// Command modules organized by domain

pub mod ab_testing;
pub mod activity;
pub mod config;
pub mod correlation;
pub mod daemon;
pub mod filesystem;
pub mod github;
pub mod optimization;
pub mod prediction;
pub mod project;
pub mod system;
pub mod telemetry;
pub mod terminal;
pub mod ui;
pub mod workspace;

// Re-export all command functions for easy registration
pub use ab_testing::*;
pub use activity::*;
pub use config::*;
pub use correlation::*;
pub use daemon::*;
pub use filesystem::*;
pub use github::*;
pub use optimization::*;
pub use prediction::*;
pub use project::*;
pub use system::*;
pub use telemetry::*;
pub use terminal::*;
pub use ui::*;
pub use workspace::*;
