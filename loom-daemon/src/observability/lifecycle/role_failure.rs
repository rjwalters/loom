//! Why a launched role tick failed, on its `loom.role_attempt` span (#10640).
//!
//! Before this module a failed tick's root span said only `loom.result =
//! failure` (and `loom.admission.reason = failure`, the same word again):
//! 849 such spans in one day, most under 5 s, none naming a cause. The cause
//! is known — but only at the site in `role_runner::launch` that observed it
//! (the spawn error, the exit status, the timeout, the tick's own log
//! region), while the span is closed later by [`super::role_invocation`] from
//! a `RoleTickOutcome::Failure(String)` whose text is free-form and must
//! never reach a span. So the observing site records a [`RoleFailure`] here,
//! on the same thread-local channel [`super::role_command`] already uses to
//! open the span, and the close reads it back.
//!
//! What lands on the span is bounded by construction:
//!
//! - [`FAILURE_CLASS`] — a closed vocabulary, the role-tick counterpart of the
//!   sweep `failure_class` (same key, same kebab-case style, the same
//!   classifier labels where the shapes coincide). [`UNCLASSIFIED`] is the
//!   explicit fallback when a failure reached the close with no class noted.
//! - [`EXIT_CODE`] — present only when a child process exited with a code.
//! - [`STATUS_MESSAGE`] — one line built from a fixed template plus
//!   machine-derived values (a code, a signal number, a timeout in seconds, an
//!   adapter category). Never log text, argv, environment or a path.

use std::cell::RefCell;

use crate::role_runner::RoleTickOutcome;
use crate::telemetry::trace::{TraceAttributes, STATUS_MESSAGE};

/// `loom.failure_class`: why a failed attempt failed, from a closed vocabulary.
pub const FAILURE_CLASS: &str = "loom.failure_class";
/// `loom.exit_code`: the child's exit code, when it exited with one.
pub const EXIT_CODE: &str = "loom.exit_code";
/// The explicit fallback class: the tick failed after its launch, and no site
/// recorded why. Prefixed like the sweep's synthesized labels so it cannot be
/// mistaken for a classifier verdict.
pub const UNCLASSIFIED: &str = "unclassified:after-launch";
/// The exported span attributes a role failure adds (the status message
/// travels as the OTLP status description instead). Each must survive the
/// daemon allowlist and the gateway's span `keep_keys` (contract-tested).
pub const ROLE_FAILURE_ATTRIBUTE_KEYS: &[&str] = &[FAILURE_CLASS, EXIT_CODE];

/// The span-facing description of one failed role tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleFailure {
    class: String,
    exit_code: Option<i32>,
    message: String,
}

impl RoleFailure {
    /// `class` must come from the closed vocabulary; `message` from a fixed
    /// template. The message is forced onto one line and into the span
    /// attribute bound here, so a template slip cannot drop it at the
    /// allowlist.
    #[must_use]
    pub fn new(class: impl Into<String>, exit_code: Option<i32>, message: &str) -> Self {
        Self {
            class: class.into(),
            exit_code,
            message: one_line(message),
        }
    }

    #[must_use]
    pub fn class(&self) -> &str {
        &self.class
    }

    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The attributes this failure adds to a span close.
    #[must_use]
    pub fn attributes(&self) -> TraceAttributes {
        let mut attrs = TraceAttributes::new();
        attrs.insert(FAILURE_CLASS.to_owned(), self.class.clone());
        if let Some(code) = self.exit_code {
            attrs.insert(EXIT_CODE.to_owned(), code.to_string());
        }
        if !self.message.is_empty() {
            attrs.insert(STATUS_MESSAGE.to_owned(), self.message.clone());
        }
        attrs
    }
}

/// Control characters become spaces and the result is cut to the 256-byte
/// attribute bound on a character boundary.
fn one_line(message: &str) -> String {
    let mut line = String::new();
    for c in message.trim().chars() {
        let c = if c.is_control() { ' ' } else { c };
        if line.len() + c.len_utf8() > 256 {
            break;
        }
        line.push(c);
    }
    line
}

thread_local! {
    static NOTED: RefCell<Option<RoleFailure>> = const { RefCell::new(None) };
}

/// Record why this thread's in-flight role tick failed. Called by the site
/// that observed the cause; a later note replaces an earlier one. Harmless
/// when no tick span is open (a pre-launch failure journals no span).
pub fn note_role_failure(failure: RoleFailure) {
    NOTED.with(|slot| *slot.borrow_mut() = Some(failure));
}

/// Drop any note left behind, so a tick starts clean.
pub(super) fn clear() {
    NOTED.with(|slot| slot.borrow_mut().take());
}

/// Take this thread's note.
pub(super) fn take() -> Option<RoleFailure> {
    NOTED.with(|slot| slot.borrow_mut().take())
}

/// The failure a closing tick reports: the noted one for a `Failure`, the
/// explicit [`UNCLASSIFIED`] fallback when nothing was noted, and `None` for
/// every other outcome (their `loom.admission.reason` already names them).
#[must_use]
pub fn for_outcome(outcome: &RoleTickOutcome, noted: Option<RoleFailure>) -> Option<RoleFailure> {
    match outcome {
        RoleTickOutcome::Failure(_) => Some(noted.unwrap_or_else(|| {
            RoleFailure::new(
                UNCLASSIFIED,
                None,
                "role tick failed after its launch; no cause was recorded",
            )
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
