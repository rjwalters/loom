//! Automatic bad-marking from a **real** failed spawn, by ingesting the
//! launch's own retained log after the fact (#8424 item 1).
//!
//! # The design decision, and why it went this way
//!
//! #8424 item 1 names a genuine fork, and this module is the answer to it.
//!
//! A native harness spawn runs under Unix `exec` (`worker_spawn::exec`): the
//! process that resolved the credential **becomes** the harness, to preserve
//! PID/signal parity for the role runner and the sweep reaper (both of which
//! poll and signal that PID, and would lose a whole generation of liveness and
//! teardown fidelity if Loom inserted a supervisor). So at the moment the child
//! could first say "insufficient balance", no Loom code is running in that
//! process at all. Two ways out:
//!
//! 1. **In-process interception** — stop `exec`ing, `spawn` + pipe instead, and
//!    watch the stream live. Rejected. It buys promptness and costs the
//!    property every dispatch surface already depends on: the PID Loom tracks
//!    would be a Loom wrapper, not the harness, so `kill`-based cancellation,
//!    process-group teardown (#4980), zombie reaping (#3801) and
//!    crash-signal attribution would each need re-deriving through one more
//!    hop. That is a large, risky change to the dispatch core to make a
//!    6-hour cooldown start a few seconds sooner.
//! 2. **Post-hoc ingestion of the retained log** — classify the text the run
//!    already wrote, from whichever process *did* supervise the run and holds
//!    its log path. Chosen.
//!
//! Option 2 is also the shape this tree had already converged on twice, for
//! exactly the same reason:
//!
//! | Precedent | Reads | Feeds |
//! |---|---|---|
//! | [`crate::worker_spawn::launch_outcome`] (#8448) | the launch's native event stream | toolless-launch failure verdict |
//! | `role_runner::provider_health_feedback` (#8443) | the tick's `LOOM_TERMINAL_RESULT` | Codex account health |
//! | `sweep_registry::containment_signal` | the sweep log after its `sweep_id=` anchor | containment mode |
//!
//! This module is the fourth, and deliberately mirrors them: a pure function of
//! already-captured text ([`classify_launch_log`]), a thin filesystem wrapper
//! ([`ingest_launch_log_at`]), region-scoped by the caller's own anchor so one
//! tick can never be attributed another's failure.
//!
//! ## What it costs, stated plainly
//!
//! The mark lands when the supervising process next looks — after the run
//! exits, not during it. A spawn that dies of exhaustion at minute 1 still
//! holds its account until it exits. And a run whose log Loom never retains
//! (no `--log`, output discarded) is never ingested at all. Both are honest
//! limits of the post-hoc choice, not bugs; the alternative was the dispatch
//! core.
//!
//! # Four guards keep this from bad-marking a healthy account
//!
//! Bad-marking wrongly is worse than not marking at all — the pool is small,
//! and a 6h cooldown on a working key can idle a fleet. So:
//!
//! 1. **Pool-selected only.** The mark is applied only when Loom's own
//!    `# LOOM_LAUNCH` record says `credentialSource: "pool"`. A key exported by
//!    an operator for a one-off run (#8363's path) is never marked.
//! 2. **Loom's own record, not the harness's self-report.** Provider and
//!    account come from the line `worker_spawn::run` wrote itself, the same
//!    discipline `role_runner::provider_health_feedback` applies when it
//!    refuses a terminal record that names an account the tick did not use.
//! 3. **Exit-0 runs are never marked.** A run the harness completed is not
//!    evidence its allowance is gone, however many retry banners its log
//!    quotes.
//! 4. **Auth failures are not exhaustion.** A `provider.auth`/401
//!    ([`Classification::CredentialFailure`]) is surfaced and **not** marked —
//!    see [`super::classify`], and #8438 for the launch-configuration bug that
//!    makes a correct key produce one.
//!
//! # Marks are class-scoped when the record names a model
//!
//! The launch record carries the resolved model, so an automatic mark is
//! scoped to that model class (#8424 item 3) and an account-wide mark is used
//! only when there is no usable model. That direction is chosen on purpose:
//! under-marking self-corrects (the next spawn on a genuinely dry account
//! fails once more and marks the second class too), while over-marking idles
//! an allowance that was never exhausted.

use std::path::Path;

use serde_json::Value;

use super::bad_marks::{self, BadMark};
use super::classify::{classify, Classification};

/// Prefix of the line `worker_spawn::run` writes before handing off to a
/// native harness. Everything after it is one JSON object.
pub const LAUNCH_RECORD_PREFIX: &str = "# LOOM_LAUNCH ";

/// The parts of a `# LOOM_LAUNCH` record this module acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRecord {
    /// `credentialSource`: `pool`, `env` or `none`.
    pub credential_source: String,
    /// `credentialProvider` — the **pool namespace** (`zai`), not the
    /// harness-facing provider id (`zai-coding-plan`).
    pub provider: Option<String>,
    /// `credentialAccount` — an account name, never key material.
    pub account: Option<String>,
    /// The resolved model, used to scope a mark to one model class.
    pub model: Option<String>,
}

impl LaunchRecord {
    /// `true` when this launch's credential came from the pool — the only
    /// case in which an account may be marked (guard 1).
    #[must_use]
    pub fn is_pool_selected(&self) -> bool {
        self.credential_source == "pool"
    }
}

/// What an ingested launch log said, and what was done about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFeedback {
    pub provider: String,
    pub account: String,
    /// The model class the mark was scoped to, or `None` for account-wide.
    pub model_class: Option<String>,
    pub classification: Classification,
    /// The mark recorded. `None` when the classification must not bad-mark
    /// (a credential failure) — the feedback is still returned so the caller
    /// can log it.
    pub mark: Option<BadMark>,
    /// One operator-facing line, safe to log: names only the provider, the
    /// account name and the classification.
    pub detail: String,
}

/// Everything from the last occurrence of `anchor` onward — one tick's or
/// sweep's own slice of a shared, append-only log.
///
/// `None` when the anchor is absent, so a previous run's output can never be
/// read as this one's. The same `rfind` anchoring
/// `sweep_registry::parse_terminal_result_after` and
/// `role_runner::toolless_launch` use. An empty anchor means "the whole text"
/// — for a caller holding a log that belongs to exactly one launch.
#[must_use]
pub fn region_after<'a>(contents: &'a str, anchor: &str) -> Option<&'a str> {
    if anchor.is_empty() {
        return Some(contents);
    }
    contents.rfind(anchor).map(|at| &contents[at..])
}

/// The last `# LOOM_LAUNCH` record in `region`, if any.
///
/// The *last*, because a region may hold more than one launch (a re-dispatch
/// inside one sweep) and the failure at the end of the region belongs to the
/// launch that most recently started.
#[must_use]
pub fn parse_launch_record(region: &str) -> Option<LaunchRecord> {
    region.lines().rev().find_map(|line| {
        let json = line.trim().strip_prefix(LAUNCH_RECORD_PREFIX)?;
        let value: Value = serde_json::from_str(json).ok()?;
        let string = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Some(LaunchRecord {
            credential_source: string("credentialSource").unwrap_or_default(),
            provider: string("credentialProvider"),
            account: string("credentialAccount"),
            model: string("model"),
        })
    })
}

/// Pure half: what `contents` says about the launch anchored at `anchor`,
/// with no filesystem writes. `None` means "no opinion" — the ordinary case.
///
/// Applies guards 1-3 from the module docs (pool-selected, Loom's own record,
/// never an exit-0 run). Guard 4 is a property of the returned
/// [`Classification`] and is applied by [`ingest_launch_log`].
#[must_use]
pub fn classify_launch_log(
    contents: &str,
    anchor: &str,
    exit_code: Option<i32>,
) -> Option<(LaunchRecord, Classification)> {
    // Guard 3: a completed run is not evidence about an allowance.
    if exit_code == Some(0) {
        return None;
    }
    let region = region_after(contents, anchor)?;
    // Guard 2 + 1: Loom's own record, and only a pool-selected credential.
    let record = parse_launch_record(region)?;
    if !record.is_pool_selected() || record.provider.is_none() || record.account.is_none() {
        return None;
    }
    let classification = classify(region, exit_code.unwrap_or(1))?;
    Some((record, classification))
}

/// The hook: classify `contents` and, when it names a pool account that ran
/// out, record the bad mark. Returns what happened, or `None` for no opinion.
///
/// `workspace` is the workspace whose effective pool the launch selected from
/// — the same path `select_api_key` was given, resolved here through
/// [`super::paths::resolve_provider_root`] so the mark lands in the pool the
/// spawn actually used (per-repo pool first, then the shared machine pool).
///
/// Errors are swallowed into `mark: None` rather than propagated: this runs on
/// a reaper/role-tick path where nothing can usefully react, and a failed
/// bookkeeping write must never change a run's reported outcome. The caller
/// logs [`LaunchFeedback::detail`].
#[must_use]
pub fn ingest_launch_log(
    workspace: &Path,
    contents: &str,
    anchor: &str,
    exit_code: Option<i32>,
) -> Option<LaunchFeedback> {
    let (record, classification) = classify_launch_log(contents, anchor, exit_code)?;
    let provider = record.provider?;
    let account = record.account?;
    let model_class = record
        .model
        .as_deref()
        .and_then(bad_marks::normalize_model_class);
    let scope = model_class
        .as_deref()
        .map_or_else(|| "account-wide".to_string(), |class| format!("model class {class}"));

    // Guard 4: a credential/config failure gets no exhaustion horizon.
    let Some(cooldown) = classification.default_cooldown_secs() else {
        return Some(LaunchFeedback {
            detail: format!(
                "api-keys pool: {provider}/{account} reported a {} — NOT bad-marked (a \
                 credential/configuration fault is not an exhaustion signal; check the \
                 provider key and the launch flags, e.g. #8438's missing --standalone)",
                classification.label()
            ),
            provider,
            account,
            model_class,
            classification,
            mark: None,
        });
    };

    let reason = format!("{} (classified from the launch log)", classification.label());
    let marked = super::paths::resolve_provider_root(workspace, &provider)
        .map_err(|e| e.to_string())
        .and_then(|root| {
            bad_marks::mark_bad_for_class(
                &root,
                &provider,
                &account,
                &reason,
                Some(cooldown),
                model_class.as_deref(),
            )
        });
    let detail = match &marked {
        Ok(_) => format!(
            "api-keys pool: bad-marked {provider}/{account} ({scope}) as {} for {cooldown}s from \
             the launch log",
            classification.label()
        ),
        Err(error) => format!(
            "api-keys pool: could not bad-mark {provider}/{account} ({scope}) as {}: {error}",
            classification.label()
        ),
    };
    Some(LaunchFeedback {
        provider,
        account,
        model_class,
        classification,
        mark: marked.ok(),
        detail,
    })
}

/// Filesystem wrapper over [`ingest_launch_log`] for a caller that holds a log
/// *path*. An unreadable log is "no opinion", never a failure.
#[must_use]
pub fn ingest_launch_log_at(
    workspace: &Path,
    log_path: &Path,
    anchor: &str,
    exit_code: Option<i32>,
) -> Option<LaunchFeedback> {
    let contents = std::fs::read_to_string(log_path).ok()?;
    ingest_launch_log(workspace, &contents, anchor, exit_code)
}

#[cfg(test)]
#[path = "ingest_tests.rs"]
mod tests;
