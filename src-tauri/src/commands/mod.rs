// Command modules organized by domain

pub mod activity;
pub mod config;
pub mod daemon;
pub mod filesystem;
pub mod github;
pub mod project;
pub mod system;
pub mod terminal;
pub mod ui;
pub mod workspace;

// Re-export all command functions for easy registration
pub use activity::*;
pub use config::*;
pub use daemon::*;
pub use filesystem::*;
pub use github::*;
pub use project::*;
pub use system::*;
pub use terminal::*;
pub use ui::*;
pub use workspace::*;
