//! Body park records for the daemon's own `loom:blocked` writers (#10161).
//!
//! # The defect this closes
//!
//! `loom-daemon park-record apply` (#10152) made the body park record and the
//! label one step for every role prompt, but two writers live in Rust and never
//! went through it:
//!
//! - the insta-crash quarantine
//!   ([`SweepRegistry::apply_quarantine_label`], #3939), and
//! - the PR-less retry hold (`write_prless_hold_label`, #7972 / #9239).
//!
//! Both applied `loom:blocked` with a bare `gh issue edit` and explained the
//! park only in a comment. `star_liveness` reads such a park as
//! `blocked-unnamed` and `check-stale-blocked` as UNDOCUMENTED, because both
//! read the **body**.
//!
//! This module writes the record those readers look for, **before** each
//! writer's existing label edit:
//!
//! ```text
//! <!-- loom:park Blocked by: (unstated) by=daemon at=2026-10-06T12:00:00Z reason="pr-less hold" -->
//! ```
//!
//! A daemon hold has no numbered blocker. It is a deliberate hold, and the
//! `reason` says which one. [`is_daemon_hold`] is the structural test the
//! release tick (#10556) and the undocumented-block queue (#10558) use to
//! recognise one without reading comment prose.
//!
//! # Why only the body goes through [`ParkForge`]
//!
//! `park_record::apply` writes the body, then the label, then removes labels
//! through REST. Neither writer can take that label path unchanged:
//!
//! - The hold's label write is #9239's honest-hold sequence: combined flip,
//!   add-only retry on a rejection, read-back, and no retry on a timeout. A
//!   `remove_labels` failure inside `apply` would report the whole park as
//!   failed, which is the exact defect #9239 fixed.
//! - Both writers run from `reap_once`, on the `ListSweeps` / `GetSweepStatus`
//!   read path, where every `gh` call is bounded by [`reap_gh_timeout`]
//!   (#3973). `operator_decision`'s `GhForge` uses a 60 s deadline and does not
//!   go through the registry's facade (#5401 `GH_CONFIG_DIR`, #10089 counted
//!   ops).
//!
//! So [`RegistryParkForge`] is a bounded `ParkForge` that routes every call
//! through the registry's `gh` facade. Only `view` and `set_body` are used
//! here. Each writer keeps its own label edit, which runs after the record.
//!
//! # When the body write fails (the documented fallback)
//!
//! - **Rejected** (non-zero exit, unreadable answer, unresolvable repo): the
//!   label is **still applied**, and the fallback is logged at `warn`. For the
//!   hold, #9239's rule is that the label is the deliverable: an issue in an
//!   unparked re-claim loop costs far more than a park without a name.
//!   Quarantine is best-effort by contract, and its in-memory pause is
//!   load-bearing either way. So a label-only park happens only after a body
//!   write has been refused, and is visible in the daemon log.
//! - **Timed out**: `gh` is wedged. The writer stops before its label edit,
//!   so a wedged `gh` costs one timeout per writer, not two (#3973). The hold
//!   reports `LabelWriteFailed` (fail closed, #9239), and the next PR-less
//!   release retries it. Quarantine keeps its in-memory pause.
//!
//! # Re-applying a hold
//!
//! A released hold leaves its record in the body. Releases only flip labels,
//! and every reader keys on `loom:blocked` first. When the same hold is
//! applied again, [`compose_hold_body`] **replaces** that writer's earlier
//! record instead of stacking a second one. So `at=` always dates the current
//! park. Records written by any other role, or by the other daemon writer, are
//! left alone.

use std::io::Write as _;

use chrono::{SecondsFormat, Utc};

use crate::operator_decision::cli::IssueState;
use crate::park_record::apply::ParkForge;
use crate::park_record::{self, ParkRecord, MARKER_CLOSE, MARKER_OPEN};
use crate::sweep_registry::reaper::reap_gh_timeout;
use crate::sweep_registry::SweepRegistry;

/// `by=` on every park record the daemon writes for its own holds.
pub const DAEMON_HOLD_BY: &str = "daemon";

/// `reason=` on the insta-crash quarantine's park record (#3939).
pub const QUARANTINE_HOLD_REASON: &str = "insta-crash quarantine";

/// `reason=` on the PR-less retry hold's park record (#7972, #9239).
pub const PRLESS_HOLD_REASON: &str = "pr-less hold";

/// Telemetry name of the body read (#10089). Mapped to `issue.view-state`.
pub(crate) const VIEW_OP: &str = "park_hold.issue_view";
/// Telemetry name of the body write (#10089). Mapped to `issue.edit-body`.
pub(crate) const BODY_OP: &str = "park_hold.issue_body";
/// Telemetry name of the (unused here) label writes. Mapped to
/// `issue.edit-labels`.
pub(crate) const LABELS_OP: &str = "park_hold.issue_labels";

/// Whether `record` is a daemon hold: no numbered blocker, `by=daemon`, and
/// one of the two daemon hold reasons.
///
/// The release tick (#10556) and the undocumented-block queue (#10558) key on
/// this rather than on comment prose.
#[must_use]
pub fn is_daemon_hold(record: &ParkRecord) -> bool {
    record.blocker.is_none()
        && record.by.as_deref() == Some(DAEMON_HOLD_BY)
        && matches!(record.reason.as_deref(), Some(QUARANTINE_HOLD_REASON | PRLESS_HOLD_REASON))
}

/// The record line this module writes for `reason` at `at`.
#[must_use]
pub fn render_hold_record(reason: &str, at: &str) -> String {
    park_record::render_park(&[], Some(DAEMON_HOLD_BY), Some(at), Some(reason))
}

/// Whether `line` is exactly one earlier record of this hold: a whole-line
/// marker whose records are all daemon holds with this `reason`.
fn is_own_record_line(line: &str, reason: &str) -> bool {
    let t = line.trim();
    if !(t.starts_with(MARKER_OPEN) && t.ends_with(MARKER_CLOSE)) {
        return false;
    }
    let records = park_record::parse(t);
    !records.is_empty()
        && records
            .iter()
            .all(|r| is_daemon_hold(r) && r.reason.as_deref() == Some(reason))
}

/// `body` with this hold's record appended, after dropping any earlier record
/// of the same hold (see the module docs). Other lines keep their original
/// line endings.
#[must_use]
pub fn compose_hold_body(body: &str, reason: &str, at: &str) -> String {
    let kept: String = body
        .split_inclusive('\n')
        .filter(|line| !is_own_record_line(line, reason))
        .collect();
    let head = kept.trim_end();
    let record = render_hold_record(reason, at);
    if head.is_empty() {
        format!("{record}\n")
    } else {
        format!("{head}\n\n{record}\n")
    }
}

/// A [`ParkForge`] that can say whether one of its calls timed out.
pub(crate) trait BoundedParkForge: ParkForge {
    /// Whether any call so far outlived its deadline.
    fn timed_out(&self) -> bool;
}

/// What the record step did, and therefore whether the label may follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HoldRecord {
    /// The record is in the body (written, or already identical).
    Written,
    /// The read or write was refused. The label still follows (documented
    /// fallback).
    Rejected(String),
    /// A call timed out: `gh` is wedged, so the writer must not spend a second
    /// timeout on its label edit (#3973).
    TimedOut(String),
}

/// Read `issue`'s body and write it back with this hold's record. Every call
/// goes through `forge`, so the order of operations is testable with a fake.
pub(crate) fn record_hold<F: BoundedParkForge + ?Sized>(
    forge: &mut F,
    issue: u64,
    reason: &str,
    at: &str,
) -> HoldRecord {
    let classify = |forge: &F, e: String| {
        if forge.timed_out() {
            HoldRecord::TimedOut(e)
        } else {
            HoldRecord::Rejected(e)
        }
    };
    let state = match forge.view(issue) {
        Ok(s) => s,
        Err(e) => return classify(forge, format!("could not read #{issue}: {e}")),
    };
    let body = compose_hold_body(&state.body, reason, at);
    if body == state.body {
        return HoldRecord::Written;
    }
    match forge.set_body(issue, &body) {
        Ok(()) => HoldRecord::Written,
        Err(e) => classify(forge, format!("body write to #{issue} failed: {e}")),
    }
}

/// The bounded [`ParkForge`] over the registry's `gh` facade.
///
/// Every call is scoped to the registry's workspace (#5401), counted under a
/// `park_hold.*` op (#10089), and bounded by [`reap_gh_timeout`] (#3973). Once
/// one call times out, every later call is refused without spawning `gh`.
pub(crate) struct RegistryParkForge<'a> {
    reg: &'a SweepRegistry,
    timed_out: bool,
}

impl<'a> RegistryParkForge<'a> {
    pub(crate) fn new(reg: &'a SweepRegistry) -> Self {
        Self {
            reg,
            timed_out: false,
        }
    }

    fn issue_path(&self, n: u64) -> Result<String, String> {
        let (owner, repo) = self
            .reg
            .resolve_owner_repo()
            .ok_or_else(|| "could not resolve the repository".to_string())?;
        Ok(format!("repos/{owner}/{repo}/issues/{n}"))
    }

    /// Fold one facade result into `Ok(stdout)` or an error, noting a timeout.
    fn settle(
        &mut self,
        what: &str,
        r: std::io::Result<Option<std::process::Output>>,
    ) -> Result<Vec<u8>, String> {
        match r {
            Ok(Some(out)) if out.status.success() => Ok(out.stdout),
            Ok(Some(out)) => Err(format!(
                "`{what}` exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(None) => {
                self.timed_out = true;
                Err(format!(
                    "`{what}` exceeded {}s and was killed (#3973)",
                    reap_gh_timeout().as_secs()
                ))
            }
            Err(e) => Err(format!("`{what}` could not be run: {e}")),
        }
    }

    fn refuse_if_wedged(&self) -> Result<(), String> {
        if self.timed_out {
            Err("skipped: an earlier `gh` call timed out (#3973)".to_string())
        } else {
            Ok(())
        }
    }

    fn edit_labels(&mut self, n: u64, flag: &str, value: &str) -> Result<(), String> {
        self.refuse_if_wedged()?;
        let n_arg = n.to_string();
        let mut args = vec!["issue", "edit", &n_arg, flag, value];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        args.extend(repo_flag.iter().map(String::as_str));
        let r = self.reg.gh_write(LABELS_OP, args);
        self.settle("gh issue edit", r).map(|_| ())
    }
}

/// The body read's answer. `body` must be present (it may be `null`): an answer
/// without it is not this issue, and writing a body built from it would erase
/// the real one.
#[derive(serde::Deserialize)]
struct ViewAnswer {
    number: u64,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
}

impl ParkForge for RegistryParkForge<'_> {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        self.refuse_if_wedged()?;
        let path = self.issue_path(number)?;
        // Writer identity: this read is the first half of a read-modify-write,
        // so a lagging reader App must not supply a stale body.
        let r = self.reg.gh_read_own_write(
            VIEW_OP,
            [
                "api",
                &path,
                "--jq",
                "{number, body, labels: [.labels[].name]}",
            ],
        );
        let stdout = self.settle("gh api (issue body)", r)?;
        let raw: serde_json::Value = serde_json::from_slice(&stdout)
            .map_err(|e| format!("unparseable issue answer: {e}"))?;
        if raw.get("body").is_none() {
            return Err("issue answer carries no `body` field".to_string());
        }
        let answer: ViewAnswer =
            serde_json::from_value(raw).map_err(|e| format!("unparseable issue answer: {e}"))?;
        if answer.number != number {
            return Err(format!("asked for #{number}, the forge answered #{}", answer.number));
        }
        Ok(IssueState {
            body: answer.body.unwrap_or_default(),
            labels: answer.labels,
        })
    }

    fn state(&mut self, repo: Option<&str>, number: u64) -> Result<String, String> {
        self.refuse_if_wedged()?;
        // A cross-repo blocker is read in its own repo (#10443).
        let path = match repo {
            Some(r) => format!("repos/{r}/issues/{number}"),
            None => self.issue_path(number)?,
        };
        let r = self.reg.gh_read(VIEW_OP, ["api", &path, "--jq", ".state"]);
        let stdout = self.settle("gh api (issue state)", r)?;
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }

    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        self.refuse_if_wedged()?;
        let path = self.issue_path(number)?;
        // `--input` rather than `-f body=…`: a body near GitHub's 65536-char
        // limit can exceed Linux's per-argument limit.
        let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        file.write_all(serde_json::json!({ "body": body }).to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        let input = file.path().to_string_lossy().to_string();
        let r = self
            .reg
            .gh_write(BODY_OP, ["api", "-X", "PATCH", &path, "--input", &input]);
        self.settle("gh api -X PATCH (issue body)", r).map(|_| ())
    }

    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String> {
        self.edit_labels(number, "--add-label", &labels.join(","))
    }

    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String> {
        self.edit_labels(number, "--remove-label", label)
    }
}

impl BoundedParkForge for RegistryParkForge<'_> {
    fn timed_out(&self) -> bool {
        self.timed_out
    }
}

impl SweepRegistry {
    /// Write the body park record for a daemon hold on `issue`, ahead of the
    /// writer's own `loom:blocked` edit (#10161).
    ///
    /// `Ok(())` means the label edit may run: the record landed, or it was
    /// refused and the documented fallback applies (logged at `warn`). `Err`
    /// means a call timed out, and the writer must stop before its label edit
    /// (#3973). See the module docs.
    pub(crate) fn record_daemon_hold(&self, issue: u32, reason: &str) -> Result<(), String> {
        let at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut forge = RegistryParkForge::new(self);
        let outcome = record_hold(&mut forge, u64::from(issue), reason, &at);
        match &outcome {
            HoldRecord::Written => {
                log::debug!("sweep_registry: wrote the `{reason}` park record on #{issue} (#10161)")
            }
            HoldRecord::Rejected(e) => log::warn!(
                "sweep_registry: `{reason}` park record for #{issue} was not written ({e}); \
                 applying `loom:blocked` without it, so the park reads as unnamed (#10161)"
            ),
            HoldRecord::TimedOut(e) => log::warn!(
                "sweep_registry: `{reason}` park record for #{issue} timed out ({e}); `gh` \
                 looks wedged, so the `loom:blocked` edit is skipped this time (#3973, #10161)"
            ),
        }
        match outcome {
            HoldRecord::TimedOut(e) => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
