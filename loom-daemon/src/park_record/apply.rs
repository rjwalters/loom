//! `loom-daemon park-record apply` — the one shared way to apply `loom:blocked`
//! (#10152).
//!
//! # The defect this closes
//!
//! [`super::render`] made the park record *easy* to write, but nothing made it
//! *necessary*: every role still applied the label with a bare
//! `gh issue edit N --add-label "loom:blocked"` and named the blocker in a
//! comment. `star_liveness`, `check-stale-blocked` and Guide's unblock sweep all
//! read the **body**, so those parks read as `blocked-unnamed`, were escalated to
//! the operator, and were never released automatically (#10129, #10125, #10120,
//! 2AMLogic/2am#2062 … on 2026-10-03/04).
//!
//! `apply` makes the record and the label one step:
//!
//! - **No blocker, no label.** A park with neither `--blocked-by` nor an
//!   explicit `--reason` is refused before any forge call, so a missing blocker
//!   is a deliberate, written choice (`--reason operator`), never an omission.
//! - **Body before label.** The record is written first; a failed body write
//!   leaves no label behind, so there is no label-only park.
//! - **Closed blockers are refused** (#9102): a park on an already-closed item is
//!   released by the very next unblock sweep, so it is a mistake, not a park.
//! - **Idempotent.** A blocker the body already declares in a park record is not
//!   declared twice; re-running is a no-op on the body.
//!
//! Every read and write goes through [`ParkForge`], so the ordering rules are
//! tested against a recording fake. The real implementation is
//! `operator_decision`'s REST `GhForge` (#5047: GraphQL exhausts first).

use std::io::Write;

use super::{blockers, parse, render_park, BlockerRef};
use crate::operator_decision::cli::{Forge as DecisionForge, GhForge, IssueState};

/// The label this command exists to apply.
pub const BLOCKED_LABEL: &str = "loom:blocked";

/// Exit codes — the same contract as `operator-decision apply`.
pub mod exit {
    /// Applied (or would apply, under `--dry-run`), or already in place.
    pub const OK: i32 = 0;
    /// Refused; nothing was touched. Reasons on stderr.
    pub const REFUSED: i32 = 1;
    /// A forge read or write failed.
    pub const FORGE: i32 = 4;
}

/// The forge operations `apply` needs.
pub trait ParkForge {
    /// The artifact's body and labels, read fresh.
    fn view(&mut self, number: u64) -> Result<IssueState, String>;
    /// `open` / `closed` for an issue or PR number, in `repo` (`None` = the
    /// forge's own repo) — a cross-repo blocker is read in its own repo (#10443).
    fn state(&mut self, repo: Option<&str>, number: u64) -> Result<String, String>;
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String>;
    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String>;
    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String>;
}

impl ParkForge for GhForge {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        DecisionForge::view(self, number)
    }
    fn state(&mut self, repo: Option<&str>, number: u64) -> Result<String, String> {
        match repo {
            Some(r) => self.issue_state_in(r, number),
            None => self.issue_state(number),
        }
    }
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        DecisionForge::set_body(self, number, body)
    }
    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String> {
        DecisionForge::add_labels(self, number, labels)
    }
    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String> {
        DecisionForge::remove_label(self, number, label)
    }
}

/// One park to apply.
#[derive(Debug, Clone, Default)]
pub struct ApplyRequest {
    /// The issue or PR being parked.
    pub number: u64,
    /// `OWNER/REPO` of the artifact being parked, when known. Lets a qualified
    /// blocker naming this same repo and number be recognised as a self-block.
    pub repo: Option<String>,
    /// The declared blockers. Empty only with an explicit [`Self::reason`].
    pub blocked_by: Vec<BlockerRef>,
    /// Why — required when there is no blocker.
    pub reason: Option<String>,
    /// `by=` provenance.
    pub by: Option<String>,
    /// `at=` provenance, RFC 3339.
    pub at: String,
    /// Labels to drop once parked (e.g. `loom:building`).
    pub remove_labels: Vec<String>,
    pub dry_run: bool,
}

/// Why a request is refused before any forge call, or `None` when it may run.
#[must_use]
pub fn precheck(req: &ApplyRequest) -> Option<String> {
    let has_reason = req.reason.as_deref().is_some_and(|r| !r.trim().is_empty());
    if req.blocked_by.is_empty() && !has_reason {
        return Some(
            "no blocker named: pass --blocked-by N, or state why there is none with \
             --reason (e.g. --reason operator). A park with no named blocker reads as \
             blocked-unnamed and is never released automatically (#10152)"
                .to_string(),
        );
    }
    if req
        .blocked_by
        .iter()
        .any(|b| b.number == req.number && b.is_local(req.repo.as_deref()))
    {
        return Some(format!("#{} cannot block itself", req.number));
    }
    None
}

/// `body` with the park record(s) `req` needs appended, or `None` when the body
/// already declares them (idempotence).
///
/// With blockers, only the ones no park record in `body` already names are
/// rendered. Without, one `(unstated)` record is rendered unless one exists.
#[must_use]
pub fn compose_body(
    body: &str,
    blocked_by: &[BlockerRef],
    by: Option<&str>,
    at: &str,
    reason: Option<&str>,
) -> Option<String> {
    let lines = if blocked_by.is_empty() {
        if parse(body).iter().any(|r| r.blocker.is_none()) {
            return None;
        }
        render_park(&[], by, Some(at), reason)
    } else {
        let declared = blockers(body);
        let missing: Vec<BlockerRef> = blocked_by
            .iter()
            .filter(|b| !declared.contains(b))
            .cloned()
            .collect();
        if missing.is_empty() {
            return None;
        }
        render_park(&missing, by, Some(at), reason)
    };
    let head = body.trim_end();
    Some(if head.is_empty() {
        format!("{lines}\n")
    } else {
        format!("{head}\n\n{lines}\n")
    })
}

/// Apply the park: refuse, or write the body record(s), then `loom:blocked`,
/// then drop `remove_labels`.
pub fn apply(
    forge: &mut dyn ParkForge,
    req: &ApplyRequest,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let n = req.number;
    if let Some(why) = precheck(req) {
        let _ = writeln!(err, "park-record apply: REFUSED - {why}; nothing was changed");
        return exit::REFUSED;
    }

    // Fresh read immediately before the write (concurrent body edits).
    let state = match forge.view(n) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(err, "park-record apply: could not read #{n}: {e}");
            return exit::FORGE;
        }
    };

    let mut closed = Vec::new();
    for b in &req.blocked_by {
        match forge.state(b.repo.as_deref(), b.number) {
            Ok(s) if s.eq_ignore_ascii_case("open") => {}
            Ok(_) => closed.push(b.to_string()),
            Err(e) => {
                let _ = writeln!(err, "park-record apply: could not read blocker {b}: {e}");
                return exit::FORGE;
            }
        }
    }
    if !closed.is_empty() {
        let _ = writeln!(
            err,
            "park-record apply: REFUSED - blocker(s) {} already closed (#9102): the unblock \
             sweep would release this park at once. Name an open blocker, or use --reason; \
             nothing was changed",
            closed.join(", ")
        );
        return exit::REFUSED;
    }

    let new_body = compose_body(
        &state.body,
        &req.blocked_by,
        req.by.as_deref(),
        &req.at,
        req.reason.as_deref(),
    );
    let has = |l: &str| state.labels.iter().any(|x| x == l);
    let add: Vec<String> = if has(BLOCKED_LABEL) {
        Vec::new()
    } else {
        vec![BLOCKED_LABEL.to_string()]
    };
    let mut remove: Vec<String> = Vec::new();
    for l in &req.remove_labels {
        let l = l.trim();
        if !l.is_empty() && l != BLOCKED_LABEL && has(l) && !remove.iter().any(|x| x == l) {
            remove.push(l.to_string());
        }
    }
    let body_changed = new_body.is_some();

    let report = |out: &mut dyn Write| {
        let _ = writeln!(out, "NUMBER={n}");
        let _ = writeln!(out, "BODY_CHANGED={body_changed}");
        let _ = writeln!(out, "LABELS_ADD={}", add.join(","));
        let _ = writeln!(out, "LABELS_REMOVE={}", remove.join(","));
    };

    if req.dry_run {
        let _ = writeln!(out, "DRY_RUN=true");
        report(out);
        if let Some(b) = &new_body {
            let _ = writeln!(out, "BODY<<EOF\n{b}EOF");
        }
        return exit::OK;
    }

    if let Some(b) = &new_body {
        if let Err(e) = forge.set_body(n, b) {
            let _ = writeln!(
                err,
                "park-record apply: body write to #{n} failed, no label applied: {e}"
            );
            return exit::FORGE;
        }
    }
    if !add.is_empty() {
        if let Err(e) = forge.add_labels(n, &add) {
            let _ = writeln!(err, "park-record apply: labeling #{n} failed: {e}");
            return exit::FORGE;
        }
    }
    for l in &remove {
        if let Err(e) = forge.remove_label(n, l) {
            let _ = writeln!(err, "park-record apply: removing {l} from #{n} failed: {e}");
            return exit::FORGE;
        }
    }
    let _ = writeln!(out, "APPLIED=true");
    report(out);
    exit::OK
}

#[cfg(test)]
mod tests;
