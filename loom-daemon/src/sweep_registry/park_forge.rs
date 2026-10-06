//! Bounded [`ParkForge`] adapter for the daemon's own `loom:blocked` writers
//! (Issue #10161).
//!
//! The two daemon-side parks — the insta-crash quarantine
//! ([`SweepRegistry::apply_quarantine_label`]) and the PR-less hold
//! ([`SweepRegistry::write_prless_hold_label`]) — used to apply `loom:blocked`
//! with a bare `gh issue edit` and explain it only in a comment. Star-liveness
//! reads such a park as `blocked-unnamed`, and `check-stale-blocked` as
//! UNDOCUMENTED, because nothing in the body says it is a deliberate hold.
//!
//! They now go through [`crate::park_record::apply::apply`]: body record first,
//! then the label. `GhForge` (REST) is deliberately **not** used: both writers
//! run from `reap_once`, on the `ListSweeps` / `GetSweepStatus` read path whose
//! per-call budget is 5s (#3973). [`BoundedParkForge`] instead routes every
//! call through the registry's `gh_read` / `gh_write` facade — workspace-scoped
//! `GH_CONFIG_DIR` (#5401), counted metrics (#10089), [`reap_gh_timeout`] per
//! call.
//!
//! Wedged `gh`: after the first timeout the adapter fails every later call
//! without spawning, so a wedge costs at most one timeout per writer.
//!
//! A failed body write never yields a label-only park: `apply` stops before the
//! label, so the label is not applied.

use super::*;
use crate::operator_decision::cli::IssueState;
use crate::park_record::apply::{apply, ApplyRequest, ParkForge};

/// `by=` provenance on every daemon park record.
pub(crate) const DAEMON_PARK_BY: &str = "daemon";
/// `reason=` on the insta-crash quarantine's record.
pub(crate) const QUARANTINE_PARK_REASON: &str = "insta-crash quarantine";
/// `reason=` on the PR-less hold's record.
pub(crate) const PRLESS_HOLD_PARK_REASON: &str = "pr-less hold";

/// How a bounded park write failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParkWriteFailure {
    /// A call exceeded [`reap_gh_timeout`] and was killed. Never retry.
    TimedOut(String),
    /// A call exited non-zero or could not be run.
    Rejected(String),
}

impl ParkWriteFailure {
    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::TimedOut(d) | Self::Rejected(d) => d,
        }
    }
}

/// Park `issue` through `forge`: body record (`reason`, `by=daemon`, now) before
/// `loom:blocked`, then `remove_labels`. `Err` carries `apply`'s stderr.
pub(crate) fn park_via(
    forge: &mut dyn ParkForge,
    issue: u32,
    reason: &str,
    remove_labels: &[&str],
) -> Result<(), String> {
    let req = ApplyRequest {
        number: u64::from(issue),
        blocked_by: Vec::new(),
        reason: Some(reason.to_string()),
        by: Some(DAEMON_PARK_BY.to_string()),
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        remove_labels: remove_labels.iter().map(ToString::to_string).collect(),
        dry_run: false,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    if apply(forge, &req, &mut out, &mut err) == crate::park_record::apply::exit::OK {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&err).trim().to_string())
    }
}

/// The quarantine's park (`loom:issue` is dropped alongside).
pub(crate) fn quarantine_park_via(forge: &mut dyn ParkForge, issue: u32) -> Result<(), String> {
    park_via(forge, issue, QUARANTINE_PARK_REASON, &["loom:issue"])
}

/// The PR-less hold's park. `remove_loom_issue = false` is the add-only
/// fallback (#9239).
pub(crate) fn prless_hold_park_via(
    forge: &mut dyn ParkForge,
    issue: u32,
    remove_loom_issue: bool,
) -> Result<(), String> {
    let remove: &[&str] = if remove_loom_issue {
        &["loom:issue"]
    } else {
        &[]
    };
    park_via(forge, issue, PRLESS_HOLD_PARK_REASON, remove)
}

/// [`ParkForge`] over a [`SweepRegistry`]'s counted, bounded `gh` facade.
pub(crate) struct BoundedParkForge<'a> {
    reg: &'a SweepRegistry,
    /// Telemetry op for the label edit (`quarantine.label` / `prless.hold_label`).
    label_op: &'static str,
    /// Fold `--remove-label` into the one label edit when `apply` asks for the
    /// removal, preserving the single combined `gh issue edit` flip these
    /// writers always made (and #9239's add-only fallback on its rejection).
    fold_remove: Option<String>,
    failure: Option<ParkWriteFailure>,
}

impl<'a> BoundedParkForge<'a> {
    pub(crate) fn new(
        reg: &'a SweepRegistry,
        label_op: &'static str,
        fold_remove: Option<&str>,
    ) -> Self {
        Self {
            reg,
            label_op,
            fold_remove: fold_remove.map(str::to_string),
            failure: None,
        }
    }

    /// The failure of the call that stopped the park, if any.
    pub(crate) fn failure(&self) -> Option<&ParkWriteFailure> {
        self.failure.as_ref()
    }

    fn run(
        &mut self,
        write: bool,
        op: &'static str,
        mut args: Vec<String>,
    ) -> Result<std::process::Output, String> {
        if let Some(ParkWriteFailure::TimedOut(d)) = &self.failure {
            return Err(d.clone());
        }
        let timeout = reap_gh_timeout();
        args.extend(crate::claim_reconciliation::gh_call::loom_repo_flag());
        let res = if write {
            self.reg.gh_write(op, args)
        } else {
            self.reg.gh_read(op, args)
        };
        let fail = match res {
            Ok(Some(out)) if out.status.success() => return Ok(out),
            Ok(Some(out)) => ParkWriteFailure::Rejected(format!(
                "`gh {op}` exited {}: {}",
                out.status
                    .code()
                    .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(None) => ParkWriteFailure::TimedOut(format!(
                "`gh {op}` exceeded {}s and was killed (#3973)",
                timeout.as_secs()
            )),
            Err(e) => ParkWriteFailure::Rejected(format!("`gh {op}` could not be run: {e}")),
        };
        let msg = fail.detail().to_string();
        self.failure = Some(fail);
        Err(msg)
    }
}

impl ParkForge for BoundedParkForge<'_> {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            labels: Vec<RawLabel>,
        }
        #[derive(serde::Deserialize)]
        struct RawLabel {
            name: String,
        }
        let out = self.run(
            false,
            "park.view",
            [
                "issue",
                "view",
                &number.to_string(),
                "--json",
                "body,labels",
            ]
            .map(String::from)
            .to_vec(),
        )?;
        let raw: Raw = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("unparseable `gh issue view` output: {e}"))?;
        Ok(IssueState {
            body: raw.body.unwrap_or_default(),
            labels: raw.labels.into_iter().map(|l| l.name).collect(),
        })
    }

    fn state(&mut self, number: u64) -> Result<String, String> {
        // Daemon parks name no blocker, so `apply` never asks; answer anyway.
        let out = self.run(
            false,
            "park.view",
            [
                "issue",
                "view",
                &number.to_string(),
                "--json",
                "state",
                "--jq",
                ".state",
            ]
            .map(String::from)
            .to_vec(),
        )?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_lowercase())
    }

    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        self.run(
            true,
            "park.body",
            ["issue", "edit", &number.to_string(), "--body", body]
                .map(String::from)
                .to_vec(),
        )
        .map(|_| ())
    }

    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String> {
        let mut args = vec!["issue".to_string(), "edit".into(), number.to_string()];
        for l in labels {
            args.extend(["--add-label".to_string(), l.clone()]);
        }
        if let Some(r) = &self.fold_remove {
            args.extend(["--remove-label".to_string(), r.clone()]);
        }
        self.run(true, self.label_op, args).map(|_| ())
    }

    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String> {
        if self.fold_remove.as_deref() == Some(label) {
            return Ok(()); // already removed by the folded label edit
        }
        self.run(
            true,
            self.label_op,
            [
                "issue",
                "edit",
                &number.to_string(),
                "--remove-label",
                label,
            ]
            .map(String::from)
            .to_vec(),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests;
