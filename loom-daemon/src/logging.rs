//! Structured logging macros for Loom daemon
//!
//! Provides JSON-formatted logs with consistent structure for easy parsing
//! and debugging across components.
//!
//! # Example
//! ```ignore
//! log_info!("Terminal created", {
//!     component: "terminal",
//!     terminal_id: Some(id.clone()),
//!     working_dir: path
//! });
//! ```

use std::time::SystemTime;

/// Generate ISO 8601 timestamp
///
/// # Panics
/// Panics if system time is before UNIX epoch (should never happen on modern systems)
#[allow(clippy::expect_used)]
#[allow(dead_code)]
pub fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System time before UNIX epoch");
    #[allow(clippy::cast_possible_wrap)]
    let datetime = chrono::DateTime::from_timestamp(now.as_secs() as i64, now.subsec_nanos())
        .expect("Invalid timestamp");
    datetime.to_rfc3339()
}

/// Generate unique error ID for tracking
///
/// # Panics
/// Panics if system time is before UNIX epoch (should never happen on modern systems)
#[allow(clippy::expect_used)]
#[allow(dead_code)]
pub fn generate_error_id() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System time before UNIX epoch");
    format!("ERR-{:x}", now.as_millis())
}

/// Log structured INFO message
#[macro_export]
macro_rules! log_info {
    ($msg:expr, { $($key:ident: $val:expr),* $(,)? }) => {{
        use serde_json::json;
        let context = json!({
            $(stringify!($key): $val,)*
        });
        let entry = json!({
            "timestamp": $crate::logging::timestamp(),
            "level": "INFO",
            "message": $msg,
            "context": context
        });
        eprintln!("{}", entry);
    }};
}

/// Log structured WARN message
#[macro_export]
macro_rules! log_warn {
    ($msg:expr, { $($key:ident: $val:expr),* $(,)? }) => {{
        use serde_json::json;
        let context = json!({
            $(stringify!($key): $val,)*
        });
        let entry = json!({
            "timestamp": $crate::logging::timestamp(),
            "level": "WARN",
            "message": $msg,
            "context": context
        });
        eprintln!("{}", entry);
    }};
}

/// Log structured ERROR message
#[macro_export]
macro_rules! log_error {
    ($msg:expr, $err:expr, { $($key:ident: $val:expr),* $(,)? }) => {{
        use serde_json::json;
        let error_id = $crate::logging::generate_error_id();
        let context = json!({
            "errorId": error_id,
            "errorMessage": format!("{}", $err),
            $(stringify!($key): $val,)*
        });
        let entry = json!({
            "timestamp": $crate::logging::timestamp(),
            "level": "ERROR",
            "message": $msg,
            "context": context
        });
        eprintln!("{}", entry);
    }};
}
