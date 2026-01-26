// errors.rs - Structured error handling with typed error domains
//
// This module provides a unified error handling strategy that spans the Rust daemon
// and TypeScript frontend. Errors are categorized into domains and include metadata
// that helps consumers decide how to handle them (retry, escalate, log, etc.).
//
// Design Goals:
// 1. Errors serialize cleanly across IPC boundary via serde
// 2. Error domains allow frontend to make smart decisions (circuit breaker integration)
// 3. Recovery hints help agents self-correct without human intervention
// 4. Detailed context preserved for debugging while keeping messages user-friendly

use serde::{Deserialize, Serialize};
use std::fmt;

/// Error domains categorize errors by their source and handling strategy.
///
/// Each domain maps to a specific subsystem and has implications for:
/// - Whether retries make sense
/// - Circuit breaker behavior
/// - User-facing messaging
/// - Escalation paths
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorDomain {
    /// tmux server/session errors (server not running, session missing, pipe failures)
    Tmux,

    /// IPC communication errors (socket unreachable, protocol mismatch, timeout)
    Ipc,

    /// Git/worktree errors (dirty state, conflicts, worktree creation failures)
    Git,

    /// File/directory access errors (permission denied, not found, disk full)
    Filesystem,

    /// Configuration parsing/validation errors (invalid JSON, missing fields)
    Configuration,

    /// Activity database errors (sqlite failures, schema migration issues)
    Activity,

    /// Terminal management errors (invalid ID, terminal not found, state inconsistency)
    Terminal,

    /// Internal errors (logic errors, unexpected state, assertion failures)
    Internal,
}

#[allow(dead_code)]
impl ErrorDomain {
    /// Returns true if errors in this domain are typically recoverable with retry
    #[must_use]
    pub fn is_typically_recoverable(self) -> bool {
        matches!(
            self,
            ErrorDomain::Tmux | ErrorDomain::Ipc | ErrorDomain::Git | ErrorDomain::Filesystem
        )
    }

    /// Returns the default retry delay for this domain in milliseconds
    #[must_use]
    pub fn default_retry_delay_ms(self) -> u64 {
        match self {
            ErrorDomain::Tmux => 2000,
            ErrorDomain::Ipc | ErrorDomain::Filesystem => 1000,
            ErrorDomain::Git | ErrorDomain::Terminal => 500,
            ErrorDomain::Activity => 100,
            ErrorDomain::Configuration | ErrorDomain::Internal => 0,
        }
    }
}

impl fmt::Display for ErrorDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorDomain::Tmux => write!(f, "tmux"),
            ErrorDomain::Ipc => write!(f, "ipc"),
            ErrorDomain::Git => write!(f, "git"),
            ErrorDomain::Filesystem => write!(f, "filesystem"),
            ErrorDomain::Configuration => write!(f, "configuration"),
            ErrorDomain::Activity => write!(f, "activity"),
            ErrorDomain::Terminal => write!(f, "terminal"),
            ErrorDomain::Internal => write!(f, "internal"),
        }
    }
}

/// Error codes provide fine-grained error identification within a domain.
///
/// Format: `DOMAIN_SPECIFIC_ERROR` (e.g., `TMUX_NO_SERVER`, `GIT_DIRTY_WORKTREE`)
/// These codes are stable and can be used for programmatic error handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCode(pub String);

#[allow(dead_code)]
impl ErrorCode {
    // Tmux error codes
    pub const TMUX_NO_SERVER: &'static str = "TMUX_NO_SERVER";
    pub const TMUX_SESSION_NOT_FOUND: &'static str = "TMUX_SESSION_NOT_FOUND";
    pub const TMUX_SESSION_EXISTS: &'static str = "TMUX_SESSION_EXISTS";
    pub const TMUX_PIPE_FAILED: &'static str = "TMUX_PIPE_FAILED";
    pub const TMUX_COMMAND_FAILED: &'static str = "TMUX_COMMAND_FAILED";

    // IPC error codes
    pub const IPC_CONNECTION_FAILED: &'static str = "IPC_CONNECTION_FAILED";
    pub const IPC_TIMEOUT: &'static str = "IPC_TIMEOUT";
    pub const IPC_PROTOCOL_ERROR: &'static str = "IPC_PROTOCOL_ERROR";
    pub const IPC_SERIALIZATION_FAILED: &'static str = "IPC_SERIALIZATION_FAILED";

    // Git error codes
    pub const GIT_WORKTREE_EXISTS: &'static str = "GIT_WORKTREE_EXISTS";
    pub const GIT_WORKTREE_NOT_FOUND: &'static str = "GIT_WORKTREE_NOT_FOUND";
    pub const GIT_DIRTY_STATE: &'static str = "GIT_DIRTY_STATE";
    pub const GIT_MERGE_CONFLICT: &'static str = "GIT_MERGE_CONFLICT";
    pub const GIT_COMMAND_FAILED: &'static str = "GIT_COMMAND_FAILED";
    pub const GIT_NOT_REPOSITORY: &'static str = "GIT_NOT_REPOSITORY";

    // Filesystem error codes
    pub const FS_NOT_FOUND: &'static str = "FS_NOT_FOUND";
    pub const FS_PERMISSION_DENIED: &'static str = "FS_PERMISSION_DENIED";
    pub const FS_ALREADY_EXISTS: &'static str = "FS_ALREADY_EXISTS";
    pub const FS_IO_ERROR: &'static str = "FS_IO_ERROR";

    // Configuration error codes
    pub const CONFIG_INVALID_JSON: &'static str = "CONFIG_INVALID_JSON";
    pub const CONFIG_MISSING_FIELD: &'static str = "CONFIG_MISSING_FIELD";
    pub const CONFIG_INVALID_VALUE: &'static str = "CONFIG_INVALID_VALUE";
    pub const CONFIG_FILE_NOT_FOUND: &'static str = "CONFIG_FILE_NOT_FOUND";

    // Activity database error codes
    pub const ACTIVITY_DB_LOCKED: &'static str = "ACTIVITY_DB_LOCKED";
    pub const ACTIVITY_DB_CORRUPTED: &'static str = "ACTIVITY_DB_CORRUPTED";
    pub const ACTIVITY_QUERY_FAILED: &'static str = "ACTIVITY_QUERY_FAILED";
    pub const ACTIVITY_SCHEMA_ERROR: &'static str = "ACTIVITY_SCHEMA_ERROR";

    // Terminal error codes
    pub const TERMINAL_NOT_FOUND: &'static str = "TERMINAL_NOT_FOUND";
    pub const TERMINAL_INVALID_ID: &'static str = "TERMINAL_INVALID_ID";
    pub const TERMINAL_ALREADY_EXISTS: &'static str = "TERMINAL_ALREADY_EXISTS";
    pub const TERMINAL_STATE_ERROR: &'static str = "TERMINAL_STATE_ERROR";

    // Internal error codes
    pub const INTERNAL_MUTEX_POISONED: &'static str = "INTERNAL_MUTEX_POISONED";
    pub const INTERNAL_UNEXPECTED_STATE: &'static str = "INTERNAL_UNEXPECTED_STATE";
    pub const INTERNAL_ASSERTION_FAILED: &'static str = "INTERNAL_ASSERTION_FAILED";

    /// Creates a new error code
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for ErrorCode {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Structured daemon error that serializes cleanly across IPC.
///
/// This replaces the simple `Response::Error { message: String }` variant
/// with rich, actionable error information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonError {
    /// The error domain (categorizes the error source)
    pub domain: ErrorDomain,

    /// Stable error code for programmatic handling
    pub code: ErrorCode,

    /// Human-readable error message
    pub message: String,

    /// Whether the error is potentially recoverable with retry
    pub recoverable: bool,

    /// Optional additional context (e.g., file paths, terminal IDs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,

    /// Optional hint for recovery (useful for agents/automation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_hint: Option<String>,
}

#[allow(dead_code)]
impl DaemonError {
    /// Creates a new daemon error with required fields
    #[must_use]
    pub fn new(
        domain: ErrorDomain,
        code: impl Into<ErrorCode>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            domain,
            code: code.into(),
            message: message.into(),
            recoverable: domain.is_typically_recoverable(),
            details: None,
            recovery_hint: None,
        }
    }

    /// Sets the recoverable flag
    #[must_use]
    pub fn recoverable(mut self, recoverable: bool) -> Self {
        self.recoverable = recoverable;
        self
    }

    /// Adds additional details to the error
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Adds a recovery hint
    #[must_use]
    pub fn with_recovery_hint(mut self, hint: impl Into<String>) -> Self {
        self.recovery_hint = Some(hint.into());
        self
    }

    // =========================================================================
    // Factory methods for common error types
    // =========================================================================

    // Tmux errors
    /// Creates a "tmux server not running" error
    #[must_use]
    pub fn tmux_no_server() -> Self {
        Self::new(
            ErrorDomain::Tmux,
            ErrorCode::TMUX_NO_SERVER,
            "tmux server is not running",
        )
        .with_recovery_hint("The tmux server may need to be started. Try running `tmux -L loom new-session` or restart the Loom daemon.")
    }

    /// Creates a "tmux session not found" error
    #[must_use]
    pub fn tmux_session_not_found(session_name: &str) -> Self {
        Self::new(
            ErrorDomain::Tmux,
            ErrorCode::TMUX_SESSION_NOT_FOUND,
            format!("tmux session '{session_name}' not found"),
        )
        .with_details(serde_json::json!({ "session_name": session_name }))
        .with_recovery_hint(
            "The terminal session may have been killed. Try restarting the terminal.",
        )
    }

    /// Creates a "tmux pipe failed" error
    #[must_use]
    pub fn tmux_pipe_failed(session_name: &str, details: &str) -> Self {
        Self::new(
            ErrorDomain::Tmux,
            ErrorCode::TMUX_PIPE_FAILED,
            format!("Failed to set up output capture for session '{session_name}'"),
        )
        .with_details(serde_json::json!({
            "session_name": session_name,
            "stderr": details
        }))
    }

    // Terminal errors
    /// Creates a "terminal not found" error
    #[must_use]
    pub fn terminal_not_found(terminal_id: &str) -> Self {
        Self::new(
            ErrorDomain::Terminal,
            ErrorCode::TERMINAL_NOT_FOUND,
            format!("Terminal '{terminal_id}' not found"),
        )
        .recoverable(false)
        .with_details(serde_json::json!({ "terminal_id": terminal_id }))
    }

    /// Creates an "invalid terminal ID" error
    #[must_use]
    pub fn terminal_invalid_id(terminal_id: &str, reason: &str) -> Self {
        Self::new(
            ErrorDomain::Terminal,
            ErrorCode::TERMINAL_INVALID_ID,
            format!("Invalid terminal ID '{terminal_id}': {reason}"),
        )
        .recoverable(false)
        .with_details(serde_json::json!({
            "terminal_id": terminal_id,
            "reason": reason
        }))
    }

    // Activity database errors
    /// Creates a "database lock failed" error
    #[must_use]
    pub fn activity_db_locked() -> Self {
        Self::new(
            ErrorDomain::Activity,
            ErrorCode::ACTIVITY_DB_LOCKED,
            "Activity database is locked",
        )
        .with_recovery_hint("The database may be busy. Wait briefly and retry.")
    }

    /// Creates a "database query failed" error
    #[must_use]
    pub fn activity_query_failed(operation: &str, error: &str) -> Self {
        Self::new(
            ErrorDomain::Activity,
            ErrorCode::ACTIVITY_QUERY_FAILED,
            format!("Failed to {operation}: {error}"),
        )
        .with_details(serde_json::json!({
            "operation": operation,
            "error": error
        }))
    }

    // Git errors
    /// Creates a "not a git repository" error
    #[must_use]
    pub fn git_not_repository(path: &str) -> Self {
        Self::new(
            ErrorDomain::Git,
            ErrorCode::GIT_NOT_REPOSITORY,
            format!("'{path}' is not a git repository"),
        )
        .recoverable(false)
        .with_details(serde_json::json!({ "path": path }))
    }

    // Filesystem errors
    /// Creates a "file not found" error
    #[must_use]
    pub fn fs_not_found(path: &str) -> Self {
        Self::new(
            ErrorDomain::Filesystem,
            ErrorCode::FS_NOT_FOUND,
            format!("File or directory not found: {path}"),
        )
        .recoverable(false)
        .with_details(serde_json::json!({ "path": path }))
    }

    // Internal errors
    /// Creates a "mutex poisoned" error
    #[must_use]
    pub fn internal_mutex_poisoned(resource: &str) -> Self {
        Self::new(
            ErrorDomain::Internal,
            ErrorCode::INTERNAL_MUTEX_POISONED,
            format!("{resource} mutex poisoned - a thread panicked while holding the lock"),
        )
        .recoverable(false)
        .with_recovery_hint("This indicates a serious internal error. Restart the daemon.")
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}:{}] {}", self.domain, self.code, self.message)
    }
}

impl std::error::Error for DaemonError {}

/// Converts an anyhow error into a `DaemonError` by parsing the error message.
///
/// This provides a migration path: existing error sites that return strings
/// can be gradually migrated to use `DaemonError` directly, while this function
/// provides reasonable categorization for legacy errors.
impl From<anyhow::Error> for DaemonError {
    fn from(err: anyhow::Error) -> Self {
        let message = err.to_string();

        // Try to categorize based on error message patterns
        if message.contains("no server running") {
            return Self::tmux_no_server();
        }
        if message.contains("no such session") || message.contains("session not found") {
            return Self::new(ErrorDomain::Tmux, ErrorCode::TMUX_SESSION_NOT_FOUND, message);
        }
        if message.contains("Terminal not found") {
            return Self::new(ErrorDomain::Terminal, ErrorCode::TERMINAL_NOT_FOUND, message);
        }
        if message.contains("Invalid terminal ID") {
            return Self::new(ErrorDomain::Terminal, ErrorCode::TERMINAL_INVALID_ID, message);
        }
        if message.contains("mutex") && message.contains("poison") {
            return Self::internal_mutex_poisoned("Unknown");
        }
        if message.contains("Database lock") || message.contains("database is locked") {
            return Self::activity_db_locked();
        }

        // Default: treat as internal error with the original message
        Self::new(ErrorDomain::Internal, ErrorCode::INTERNAL_UNEXPECTED_STATE, message)
            .recoverable(false)
    }
}

/// Result type alias for daemon operations
#[allow(dead_code)]
pub type DaemonResult<T> = Result<T, DaemonError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_serialization() {
        let error = DaemonError::tmux_no_server();
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("tmux"));
        assert!(json.contains("TMUX_NO_SERVER"));

        // Deserialize back
        let parsed: DaemonError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.domain, ErrorDomain::Tmux);
        assert_eq!(parsed.code.0, ErrorCode::TMUX_NO_SERVER);
    }

    #[test]
    fn test_error_with_details() {
        let error = DaemonError::terminal_not_found("terminal-1");
        assert!(error.details.is_some());
        let details = error.details.unwrap();
        assert_eq!(details["terminal_id"], "terminal-1");
    }

    #[test]
    fn test_domain_recoverability() {
        assert!(ErrorDomain::Tmux.is_typically_recoverable());
        assert!(ErrorDomain::Ipc.is_typically_recoverable());
        assert!(!ErrorDomain::Configuration.is_typically_recoverable());
        assert!(!ErrorDomain::Internal.is_typically_recoverable());
    }

    #[test]
    fn test_anyhow_conversion() {
        let anyhow_err = anyhow::anyhow!("no server running on /tmp/tmux.sock");
        let daemon_err: DaemonError = anyhow_err.into();
        assert_eq!(daemon_err.domain, ErrorDomain::Tmux);
        assert_eq!(daemon_err.code.0, ErrorCode::TMUX_NO_SERVER);
    }
}
