//! The `loom.failure_class` of a role tick that launched and then failed
//! (#10640), built at the site in [`super::run_role_with_timeout`] that
//! observed the cause and handed to the span through
//! [`crate::observability::lifecycle::note_role_failure`].
//!
//! The vocabulary is the sweep `failure_class`'s wherever the two describe
//! the same shape, because the same classifiers read the same adapter
//! output: a role tick's own region of `role-<role>.log` (everything after
//! its `tick_anchor`) is the counterpart of a sweep's newest dispatch region.
//!
//! | Class | Observed |
//! |---|---|
//! | `launch-failed` | `Command::spawn` returned an error; no child ran |
//! | `wait-failed` | polling the child for its exit returned an error |
//! | `timeout-ceiling` | the role timeout fired and the child was terminated |
//! | `killed-by-signal` | the child was ended by a signal it did not send itself |
//! | `toolless-launch` | exit 0, but the native launch never offered the `loom_*` tools (#8448) |
//! | `sandbox-unavailable` | exit 0, but the runtime sandbox refused every tool call (#10003) |
//! | `preflight-auth-failed` | the wrapper's auth pre-flight refused the session |
//! | `preflight-mcp-failed`, `preflight-token-selection-failed`, `preflight-no-cli-start` | the sweep's pre-flight classifier: the adapter died before its CLI started |
//! | `account-pool-exhausted` | the wrapper's rotation ran out of accounts mid-run |
//! | `credential-expired`, `account-exhausted:<category>`, `session-limit`, `runtime-timeout`, `runtime-fatal`, `cwd-deleted`, `model-refusal` | the runtime adapter's own terminal verdict (`# LOOM_TERMINAL_RESULT`) |
//! | `no-usable-account`, `account-exhausted:<signature>`, `execution-error`, `self-kill:background-wait`, `exit-<code>` | the sweep's crash classifier, ending in the bare exit code |
//!
//! A non-zero exit therefore always gets a class. The explicit
//! `unclassified:after-launch` fallback is reserved for a failure no site
//! noted at all ([`crate::observability::lifecycle::UNCLASSIFIED`]).
//!
//! Every status message is a fixed template plus a code, a signal number, a
//! timeout in seconds, an `io::ErrorKind` or an adapter category. No log
//! text, argv, environment or path reaches the span.

use super::*;
use crate::observability::lifecycle::RoleFailure;
use crate::sweep_registry::PreflightOutcome;
use crate::tokens_pool::TerminalClassification;

/// The wrapper's auth pre-flight sentinel (`failure_sentinel`'s table). The
/// sweep pre-flight table does not carry it, and without it the absence of a
/// CLI start would report the generic `preflight-no-cli-start`.
const AUTH_PREFLIGHT_SENTINEL: &str = "# AUTH_PREFLIGHT_FAILED";
/// The wrapper's mid-run rotation-exhausted sentinel (`failure_sentinel`'s
/// table), which the sweep exhaustion signatures do not match.
const ACCOUNT_POOL_EXHAUSTED_SENTINEL: &str = "# ACCOUNT_POOL_EXHAUSTED";

/// `Command::spawn` failed: no child process exists.
pub(super) fn launch_failed(error: &std::io::Error) -> RoleFailure {
    RoleFailure::new(
        "launch-failed",
        None,
        &format!("role launcher could not be spawned ({:?})", error.kind()),
    )
}

/// `try_wait` failed, so the child's exit was never observed.
pub(super) fn wait_failed(error: &std::io::Error) -> RoleFailure {
    RoleFailure::new(
        "wait-failed",
        None,
        &format!("could not poll the role child for its exit ({:?})", error.kind()),
    )
}

/// The role timeout fired and the child's process group was terminated.
pub(super) fn timed_out(timeout: Duration) -> RoleFailure {
    RoleFailure::new(
        "timeout-ceiling",
        None,
        &format!(
            "role child ran past the {} s role timeout and was terminated",
            timeout.as_secs()
        ),
    )
}

/// Exit 0 from a native launch that never offered the `loom_*` tools.
pub(super) fn toolless_launch() -> RoleFailure {
    RoleFailure::new(
        "toolless-launch",
        Some(0),
        "role child exited 0 but its native launch never offered the loom tools",
    )
}

/// Exit 0 from a session whose sandbox refused every tool call.
pub(super) fn sandbox_unavailable() -> RoleFailure {
    RoleFailure::new(
        "sandbox-unavailable",
        Some(0),
        "role child exited 0 but the runtime sandbox refused every tool call",
    )
}

/// A child that exited unsuccessfully, read against this tick's own region
/// of its role log (`full_log` is the whole append-only file).
pub(super) fn exited(
    status: std::process::ExitStatus,
    full_log: &str,
    tick_anchor: &str,
) -> RoleFailure {
    let Some(code) = status.code() else {
        let message = match exit_signal(status) {
            Some(signal) => format!("role child was killed by signal {signal}"),
            None => "role child was killed by a signal".to_string(),
        };
        return RoleFailure::new("killed-by-signal", None, &message);
    };
    let region = super::super::failure_sentinel::tick_region(full_log, tick_anchor);
    let adapter = sweep_registry::parse_terminal_result_after(full_log, tick_anchor)
        .map(|result| result.category);
    let message = match adapter {
        Some(category) => format!(
            "role child exited with code {code}; runtime adapter reported {}",
            adapter_wire_name(category)
        ),
        None => format!("role child exited with code {code}"),
    };
    RoleFailure::new(classify_exit(region, adapter, code), Some(code), &message)
}

/// The class of a non-zero exit with `code`, most specific first:
///
/// 1. the wrapper's auth pre-flight sentinel;
/// 2. the sweep pre-flight classifier (an explicit pre-flight marker, or no
///    `# LOOM_CLI_START` at all — the adapter died before its CLI ran);
/// 3. the wrapper's mid-run account-rotation exhaustion sentinel;
/// 4. the runtime adapter's own terminal category, unless it is the
///    `RECOVERABLE` catch-all (which says only "unrecognized");
/// 5. the sweep crash classifier, whose last resort is `exit-<code>`.
#[must_use]
pub(super) fn classify_exit(
    region: &str,
    adapter: Option<TerminalClassification>,
    code: i32,
) -> String {
    if region.contains(AUTH_PREFLIGHT_SENTINEL) {
        return "preflight-auth-failed".to_string();
    }
    if let PreflightOutcome::Preflight(label) =
        sweep_registry::classify_preflight_outcome(Some(region))
    {
        return label.to_string();
    }
    if region.contains(ACCOUNT_POOL_EXHAUSTED_SENTINEL) {
        return "account-pool-exhausted".to_string();
    }
    if let Some(class) = adapter.and_then(adapter_class) {
        return class.to_string();
    }
    sweep_registry::classify_crash(region, Some(code)).unwrap_or_else(|| format!("exit-{code}"))
}

/// The class a runtime adapter's terminal category names, or `None` when it
/// names nothing more specific than the exit itself.
fn adapter_class(category: TerminalClassification) -> Option<&'static str> {
    use TerminalClassification as C;
    match category {
        C::TokenExpired => Some("credential-expired"),
        C::TokenExhausted => Some("account-exhausted:token-exhausted"),
        C::ModelCreditsExhausted => Some("account-exhausted:model-credits-exhausted"),
        C::SessionLimit => Some("session-limit"),
        C::Timeout => Some("runtime-timeout"),
        C::Fatal => Some("runtime-fatal"),
        C::CwdDeleted => Some("cwd-deleted"),
        C::ModelRefusal => Some("model-refusal"),
        C::SandboxUnavailable => Some("sandbox-unavailable"),
        C::Recoverable | C::Success => None,
    }
}

/// The category exactly as the adapter wrote it in its terminal record.
fn adapter_wire_name(category: TerminalClassification) -> &'static str {
    use TerminalClassification as C;
    match category {
        C::Success => "SUCCESS",
        C::TokenExpired => "TOKEN_EXPIRED",
        C::TokenExhausted => "TOKEN_EXHAUSTED",
        C::ModelCreditsExhausted => "MODEL_CREDITS_EXHAUSTED",
        C::Recoverable => "RECOVERABLE",
        C::Timeout => "TIMEOUT",
        C::Fatal => "FATAL",
        C::CwdDeleted => "CWD_DELETED",
        C::ModelRefusal => "MODEL_REFUSAL",
        C::SessionLimit => "SESSION_LIMIT",
        C::SandboxUnavailable => "SANDBOX_UNAVAILABLE",
    }
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_: std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests;
