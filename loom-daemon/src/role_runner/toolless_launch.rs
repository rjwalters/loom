//! Turn an exit-0 **guarded native** role tick that never actually used a
//! `loom_*` tool into a [`RoleTickOutcome::Failure`] (issue #8448).
//!
//! The defect this closes: on OpenCode 2.x the guarded tool binding
//! `native_tools::provision` installs is never loaded, so a role-tagged
//! worker starts with the deny-by-default agent and **no tools at all**. It
//! then has nothing to do, says so in prose, and the CLI **exits 0**. The
//! live canary in #8448 measured this directly — zero `tool_use` events, zero
//! policy denials, no `step_finish`, worker exit `0` in 9s — and the control
//! run on 1.18.31 exited `0` too. Before this module, those two ticks were
//! byte-identical to the role runner: both `RoleTickOutcome::Success`. A role
//! could therefore run all night, do nothing, and report a healthy fleet.
//!
//! Scope, deliberately narrow — this fires only when ALL of the following
//! hold, because each one is what makes "no `loom_*` tool use" *mean*
//! something:
//!
//! 1. The tick was admitted onto a **native harness** runtime
//!    (`crate::worker_spawn::is_native` — `pi`/`opencode`). A Claude or Codex
//!    tick has no `loom_*` tools and no native event stream; zero uses there
//!    is the normal, healthy case.
//! 2. The tick ran under a resolved admission at all. A test/ad-hoc
//!    invocation that opted out of admission (`spawn_bin` override) leaves
//!    `admission` as `None` and is left alone — the same guard
//!    [`super::provider_health_feedback`] applies.
//! 3. This tick's own region of the role log — everything after its unique
//!    `tick_anchor` header, the same anchor scoping the provider-health
//!    bridge uses — could be read.
//! 4. That region contains at least one parseable native event
//!    ([`LaunchOutcome::observed_a_toolless_run`]). A stream Loom cannot
//!    parse is a gap in Loom's own observation, NOT evidence about the
//!    launch: a future harness release that renames its event types must
//!    degrade this check to "no opinion", never to "fail every tick".
//!
//! Why `Failure` and not a new [`RoleTickOutcome`] variant: this *is* a plain
//! invocation failure — the role did not do its job, the remedy is to fix the
//! binding, and it should be counted, logged and escalated exactly like any
//! other failed tick. The distinct variants in `outcome.rs` all exist to keep
//! *non-failures* (pool holds, load skips, pre-spawn config refusals) out of
//! the failure tally; this belongs in it.

use super::*;
use crate::worker_spawn::launch_outcome::{classify_native_stream, LaunchOutcome};

/// The guarded tool set `native_tools::provision` binds, named in the failure
/// detail so an operator reading the daemon log knows what was missing
/// without opening the role log.
const GUARDED_TOOLS: &str = "loom_read/loom_write/loom_edit/loom_bash";

/// Inspect a just-exited-0 role tick and, when it is a guarded native launch
/// that provably never used a `loom_*` tool, return the failure detail that
/// should replace [`RoleTickOutcome::Success`]. `None` means "no opinion" —
/// report success as before.
///
/// Takes the log *contents* rather than a path so it stays a pure function of
/// its inputs; [`detect`] is the thin filesystem wrapper the call site uses.
pub(super) fn detect_in(
    contents: &str,
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    tick_anchor: &str,
) -> Option<String> {
    let admission = admission?;
    if !crate::worker_spawn::is_native(&admission.runtime) {
        return None;
    }
    let region = tick_region(contents, tick_anchor)?;
    let outcome = classify_native_stream(region);
    if !outcome.observed_a_toolless_run() {
        return None;
    }
    Some(describe(admission, &outcome))
}

/// Filesystem wrapper over [`detect_in`]: reads the role's own log file, which
/// the tick's child wrote its native event stream to (`run_role_with_timeout`
/// redirects the child's stdout/stderr straight at it). An unreadable log is
/// "no opinion", never a failure.
pub(super) fn detect(
    log_path: &Path,
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    tick_anchor: &str,
) -> Option<String> {
    // Cheap pre-check before reading a potentially large role log: a
    // non-native tick can never produce this verdict.
    if !admission.is_some_and(|a| crate::worker_spawn::is_native(&a.runtime)) {
        return None;
    }
    detect_in(&read_role_log(log_path), admission, tick_anchor)
}

/// This tick's own slice of the shared, append-only per-role log: everything
/// from the last occurrence of `tick_anchor` (written into the header line
/// immediately before the spawn) onward. `None` when the anchor is absent, so
/// a previous tick's events can never be read as this one's — the same
/// `rfind` anchoring `sweep_registry::parse_terminal_result_after` uses.
fn tick_region<'a>(contents: &'a str, tick_anchor: &str) -> Option<&'a str> {
    if tick_anchor.is_empty() {
        return None;
    }
    contents.rfind(tick_anchor).map(|at| &contents[at..])
}

fn describe(
    admission: &crate::runtime_admission::ResolvedRuntime,
    outcome: &LaunchOutcome,
) -> String {
    format!(
        "toolless launch: the {} guarded binding never offered {GUARDED_TOOLS} \
         (role={}, {} native event(s), 0 loom_* tool uses, {} step_finish) — \
         the CLI exited 0 without doing any work, so this tick is reported as \
         a failed launch, not a success (#8448)",
        admission.runtime, admission.role, outcome.events, outcome.step_finishes,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
