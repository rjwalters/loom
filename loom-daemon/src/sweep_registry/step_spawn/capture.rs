//! A step's end: its outcome line, and the role tick's capture (#11906).
//!
//! #11877 made a role tick journal the listings its agent read (the pick
//! journal) and export its `role_tick.outcome`. Both fired only inside a role
//! tick, and a step is not one: it runs detached in the sweep registry. So a
//! step carries a journal of its own from the spawn (`pick_journal::
//! attach_step`), and its end emits both through the role tick's own emit
//! (`role_tick_telemetry::emit_for_step`, which drains the journal into the
//! role's `pick.decision`), on a detached thread: the transcript scan behind
//! `role_tick.outcome` must not hold the registry lock. The record is marked
//! `source = "step"` with the step's sweep id, so it never counts as a
//! role-runner tick.
//!
//! The session's own word on its step is its last
//! `LOOM_STEP_OUTCOME role=… target=… class=… reason=…` line
//! (`targeted-role-invocation.md`, #11868), read from the step's log after
//! its own dispatch header. The class rides the record's detail and the end
//! log line; the outcome ledger (#11900) consumes it from
//! [`read_outcome_line`]. The line is advisory: the ledger decides with the
//! forge post-state, which this module does not read. Whoever reads it for
//! the ledger passes `None` when that read fails, never a default (#11962),
//! so a failed probe classifies as `failed`, not as the session's word.

use super::*;
use crate::role_runner::RoleTickOutcome;

/// The four outcome classes of a targeted step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcomeClass {
    Verdict,
    Noop,
    Refused,
    Failed,
}

impl StepOutcomeClass {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "verdict" => Some(Self::Verdict),
            "noop" => Some(Self::Noop),
            "refused" => Some(Self::Refused),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verdict => "verdict",
            Self::Noop => "noop",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// One `LOOM_STEP_OUTCOME` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcomeLine {
    pub role: String,
    pub target: u32,
    pub class: StepOutcomeClass,
    pub reason: String,
}

const OUTCOME_PREFIX: &str = "LOOM_STEP_OUTCOME ";

/// Parse one outcome line. The prompt's own template
/// (`class=<verdict|noop|refused|failed>`) is not one. Pure.
#[must_use]
pub fn parse_outcome_line(line: &str) -> Option<StepOutcomeLine> {
    let rest = &line[line.find(OUTCOME_PREFIX)? + OUTCOME_PREFIX.len()..];
    let field = |key: &str| {
        rest.split_whitespace()
            .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim_matches(|c: char| c == '`' || c == '"'))
    };
    Some(StepOutcomeLine {
        role: field("role")?.to_string(),
        target: field("target")?.parse().ok()?,
        class: StepOutcomeClass::parse(field("class")?)?,
        reason: field("reason").unwrap_or("").to_string(),
    })
}

/// The last outcome line in `log` written after `sweep_id`'s own dispatch
/// header (an earlier run's line in the same per-item log is not this
/// step's). `None` when the session wrote none.
#[must_use]
pub fn read_outcome_line(log: &Path, sweep_id: &str) -> Option<StepOutcomeLine> {
    let text = String::from_utf8_lossy(&std::fs::read(log).ok()?).into_owned();
    let anchor = format!("sweep_id={sweep_id} ");
    let start = text.rfind(&anchor)?;
    text[start..].lines().rev().find_map(parse_outcome_line)
}

/// How a step's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEnd {
    /// It exited (the reaper saw it die), with its status when known.
    Exited(Option<i32>),
    /// Cancelled (by an operator or a roll).
    Cancelled,
    /// Killed by the reaper at its role's timeout (B3).
    TimedOut,
}

/// The role tick outcome a step's end maps to. The session's `failed`
/// class, a non-zero exit, a timeout and a cancel are failures; anything
/// else completed. Pure.
#[must_use]
pub fn tick_outcome(end: StepEnd, line: Option<&StepOutcomeLine>) -> RoleTickOutcome {
    let class = line.map(|l| l.class);
    match (end, class) {
        (StepEnd::TimedOut, _) => RoleTickOutcome::Failure("step timed out".to_string()),
        (StepEnd::Cancelled, _) => RoleTickOutcome::Failure("step cancelled".to_string()),
        (_, Some(StepOutcomeClass::Failed)) => RoleTickOutcome::Failure(format!(
            "step outcome failed: {}",
            line.map_or("", |l| l.reason.as_str())
        )),
        (StepEnd::Exited(Some(0)), _) => RoleTickOutcome::Success,
        (StepEnd::Exited(Some(code)), _) => RoleTickOutcome::Failure(format!("step exited {code}")),
        (StepEnd::Exited(None), Some(_)) => RoleTickOutcome::Success,
        (StepEnd::Exited(None), None) => {
            RoleTickOutcome::Failure("step exited, status unknown".to_string())
        }
    }
}

/// Everything the end-of-step emit needs, read out of the registry.
#[derive(Debug, Clone)]
pub(crate) struct StepCapture {
    pub(crate) root: PathBuf,
    pub(crate) role: StepRole,
    pub(crate) sweep_id: SweepId,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) outcome: RoleTickOutcome,
    pub(crate) model: String,
    pub(crate) effort: String,
}

/// Emit the role tick's capture for one finished step: its journal drained
/// into the role's `pick.decision`, and its `role_tick.outcome`. Blocking.
pub(crate) fn emit_step_capture(capture: &StepCapture) {
    crate::observability::pick_journal::adopt(
        &capture.root,
        crate::observability::pick_journal::step_path(&capture.sweep_id),
    );
    crate::role_tick_telemetry::emit_for_step(
        &capture.root,
        capture.role.role_name(),
        capture.started_at,
        &capture.outcome,
        Some(crate::role_tick_telemetry::ResolvedLaunch::new(&capture.model, &capture.effort)),
        &capture.sweep_id,
    );
}

impl SweepRegistry {
    /// A step ended (`release_dead_step`, or found dead at startup with no
    /// entry): reap the leases it still listed, then emit its capture off the
    /// lock. Returns its outcome line.
    pub(crate) fn end_step(
        &self,
        sweep_id: &str,
        item: u32,
        role: StepRole,
        started_at: DateTime<Utc>,
        end: StepEnd,
    ) -> Option<StepOutcomeLine> {
        let end = if super::timeout::take_timed_out(sweep_id) {
            StepEnd::TimedOut
        } else {
            end
        };
        let info = self.entries.get(sweep_id);
        let log_path =
            info.map_or_else(|| self.compute_step_log_path(item, role), |i| i.log_path.clone());
        let line = read_outcome_line(&log_path, sweep_id);
        log::info!(
            "sweep_registry: {role} step {sweep_id} ended ({end:?}), outcome {} (#11906)",
            line.as_ref().map_or("none".to_string(), |l| format!(
                "{} {}",
                l.class.as_str(),
                l.reason
            ))
        );
        if !self.config.skip_label_flip {
            let reason = match end {
                StepEnd::TimedOut => crate::pr_lease_reap::REASON_TIMEOUT,
                StepEnd::Cancelled => "cancelled",
                StepEnd::Exited(_) => "exited",
            };
            self.reap_step_leases(sweep_id, reason);
        }
        let capture = StepCapture {
            root: self.config.workspace_root.clone(),
            role,
            sweep_id: sweep_id.to_string(),
            started_at,
            outcome: tick_outcome(end, line.as_ref()),
            model: info.and_then(|i| i.model.clone()).unwrap_or_default(),
            effort: info.and_then(|i| i.effort.clone()).unwrap_or_default(),
        };
        // Hermetic fixtures (no forge) keep their tests free of the detached
        // transcript scan; the capture itself is tested directly.
        if !self.config.skip_label_flip {
            // #12027: the step's verdict record reads its log at the drain.
            let staged = crate::observability::step_verdict::StagedStep {
                sweep_id: sweep_id.to_string(),
                step: role,
                item,
                log: log_path,
            };
            let spawned = std::thread::Builder::new()
                .name("step-capture".to_string())
                .spawn(move || {
                    crate::observability::step_verdict::stage_step(staged);
                    emit_step_capture(&capture);
                });
            if let Err(e) = spawned {
                log::warn!("sweep_registry: {sweep_id}: no capture thread ({e})");
            }
        }
        line
    }
}
