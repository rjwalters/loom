//! `loom-daemon park-record` — render and read the `loom:blocked` park record
//! (#8925).
//!
//! # Why a command and not a documented string
//!
//! The park record is written by a **role prompt**, i.e. by an LLM copying a
//! format out of prose. That is exactly the situation in which a format drifts:
//! a comma instead of a second record, a `blocked-by=` attribute instead of the
//! `Blocked by:` phrase every existing parser matches, a reason containing `--`
//! that terminates the comment early. Each of those produces a record that looks
//! right and reads as zero blockers.
//!
//! `render` removes the opportunity: the role supplies numbers and a reason, and
//! gets back the exact line to paste. `parse` is its inverse, for a shell caller
//! (`guide.md`'s unblock sweep) that needs the declared blockers without
//! re-implementing the grammar in `grep`.
//!
//! Both are pure — no forge read, no label write. `apply` (#10152) is the one
//! that parks: it writes the record into the body, then adds `loom:blocked` —
//! the shared label-apply path, so a park cannot be applied without one.

use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;

use crate::operator_decision::cli::WorkflowWriter;
use crate::park_record::{self, BlockerRef, ParkRecord, Withdrawn};

#[derive(clap::Subcommand)]
pub(crate) enum ParkRecordCommand {
    /// Render the park record line(s) to paste into an artifact body. One line
    /// per blocker (see `park-record.md` for why a comma-separated list is
    /// unsafe). Exit 0 always.
    Render(RenderArgs),

    /// Read the park records out of a body. Prints one declared blocker number
    /// per line, or a JSON object with `--json`. Exit 0 when at least one record
    /// was found, 1 when none was — so a shell caller can branch on "is this
    /// park declared?" without parsing output.
    Parse(ParseArgs),

    /// Park an issue or PR: write the park record into its BODY, then add
    /// `loom:blocked` (#10152). Refuses (exit 1, nothing changed) with no
    /// `--blocked-by` and no explicit `--reason`, or with a closed blocker.
    /// Idempotent. Exit 4 on a forge failure; a failed body write adds no label.
    Apply(ApplyArgs),

    /// Journal that a Champion hold was released (#12665): the `hold.transition`
    /// `remove` for a `loom:blocked` removal the caller just made. Writes
    /// nothing to the forge; a no-op outside a role tick. Exit 0 always.
    Released(ReleasedArgs),

    /// Journal a Champion merge-risk decision with its confidence (#12950):
    /// a `hold.transition` of kind `decision`. Writes nothing to the forge; a
    /// no-op outside a role tick. Exit 0 always.
    Decision(DecisionArgs),

    /// Report whether Guide may route a parked conflict to Doctor for this
    /// base. Prints enabled, disabled, or stacked. Unreadable policy fails.
    GuideConflictRoute(GuideConflictRouteArgs),

    /// Withdraw one declared blocker (#13895): wrap every live park marker
    /// naming `--blocked-by` in an inline code span — marker text unchanged,
    /// so `parse`/`blockers` stop reading it — and append one visible
    /// `Park withdrawn:` audit line. Idempotent: no live marker names the ref
    /// → exit 0, nothing written. Refuses (exit 1, nothing changed) a marker
    /// naming several refs (it would silently drop the others) and a blank
    /// `--reason`. Edits the BODY only: it does NOT remove `loom:blocked` —
    /// the caller or Guide's unblock sweep does. Re-parking appends a fresh
    /// record with `apply --blocked-by`.
    Retract(RetractArgs),
}

#[derive(clap::Args)]
pub(crate) struct GuideConflictRouteArgs {
    /// The exact base ref name from a direct PR snapshot.
    #[arg(long)]
    base_ref: String,

    /// Repository whose effective policy applies.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}

impl GuideConflictRouteArgs {
    fn run(self) -> Result<()> {
        if self.base_ref.trim().is_empty() {
            anyhow::bail!("the direct PR base ref is unknown");
        }
        let disposition =
            if crate::merge_pr::stacked_children::is_stackable_parent_branch(&self.base_ref) {
                "stacked"
            } else if crate::stale_blocked::guide_conflict_route_enabled(&self.workspace)? {
                "enabled"
            } else {
                "disabled"
            };
        println!("{disposition}");
        Ok(())
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum DecisionOutcome {
    Hold,
    SelfRelease,
    Pass,
}

#[derive(clap::Args)]
pub(crate) struct DecisionArgs {
    /// The PR the decision was made on.
    #[arg(long, value_name = "N")]
    pub pr: u64,

    /// What the `merge-risk` criterion decided.
    #[arg(long, value_enum)]
    pub outcome: DecisionOutcome,

    /// Confidence (0-1) that a human would release the PR.
    #[arg(long, value_name = "0-1")]
    pub confidence: Option<f64>,
}

#[derive(clap::Args)]
pub(crate) struct ReleasedArgs {
    /// The PR whose hold was released.
    #[arg(long, value_name = "N")]
    pub pr: u64,

    /// Closed token for what released it (e.g. `release_precheck`).
    #[arg(long, value_name = "TOKEN", default_value = "release_precheck")]
    pub trigger: String,

    /// When the hold began (RFC 3339), if the caller knows it. Omitted: the
    /// record carries a lower-bound flag instead of a guessed entry time.
    #[arg(long = "entered-at", value_name = "TIMESTAMP")]
    pub entered_at: Option<String>,

    /// The removal was attempted and failed.
    #[arg(long)]
    pub failed: bool,
}

#[derive(clap::Args)]
pub(crate) struct ApplyArgs {
    /// The issue to park.
    #[arg(
        long,
        value_name = "N",
        required_unless_present = "pr",
        conflicts_with = "pr"
    )]
    pub issue: Option<u64>,

    /// The PR to park.
    #[arg(long, value_name = "N")]
    pub pr: Option<u64>,

    /// The open blocker(s): `N`, `#N` (this repo) or `OWNER/REPO#N` (another
    /// repo). Repeatable; comma-separated accepted.
    #[arg(long = "blocked-by", value_name = "REF", num_args = 1.., value_delimiter = ',')]
    pub blocked_by: Vec<BlockerRef>,

    /// Why. Required when no `--blocked-by` is given (e.g. `operator`), so a
    /// park with no named blocker is a deliberate choice.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,

    /// Who is parking it — a role name or `human`.
    #[arg(long = "by", value_name = "ROLE")]
    pub by: Option<String>,

    /// Park transiently for a cause that clears by itself, e.g. `capacity:batch`
    /// (#13606). Writes `attempt` and an exponential-backoff `until`; the unblock
    /// sweep releases it once `until` passes. Excludes `--blocked-by`.
    #[arg(long, value_name = "CAUSE")]
    pub transient: Option<String>,

    /// Label to remove once parked (repeatable), e.g. loom:building.
    #[arg(long = "remove-label", value_name = "LABEL")]
    pub remove_label: Vec<String>,

    /// Target repository; defaults to the checkout's remote.
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,

    /// Print the planned body and label changes; mutate nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

#[derive(clap::Args)]
pub(crate) struct RetractArgs {
    /// The issue to withdraw the blocker from.
    #[arg(
        long,
        value_name = "N",
        required_unless_present_any = ["pr", "file"],
        conflicts_with = "pr"
    )]
    pub issue: Option<u64>,

    /// The PR to withdraw the blocker from.
    #[arg(long, value_name = "N")]
    pub pr: Option<u64>,

    /// The blocker to withdraw: `N`, `#N` (this repo) or `OWNER/REPO#N`.
    #[arg(long = "blocked-by", value_name = "REF")]
    pub blocked_by: BlockerRef,

    /// Why the blocker no longer gates this artifact. Required.
    #[arg(long, value_name = "TEXT")]
    pub reason: String,

    /// Who is withdrawing it — a role name or `human`.
    #[arg(long = "by", value_name = "ROLE")]
    pub by: Option<String>,

    /// Target repository; defaults to the checkout's remote.
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,

    /// Compose against this body instead of the forge (`-` or a path; stdin
    /// with `-`). Offline: the forge is never read, the planned body is
    /// printed as for `--dry-run`, nothing is written.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["issue", "pr"])]
    pub file: Option<PathBuf>,

    /// Print the planned body; mutate nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

#[derive(clap::Args)]
pub(crate) struct RenderArgs {
    /// The blocker(s): `N`, `#N` or `OWNER/REPO#N`. Repeatable, and comma-separated
    /// values are accepted. Omit to render an explicit "blocker unstated"
    /// record, which is still attributable and dated — unlike silence.
    #[arg(long = "blocked-by", value_name = "REF", num_args = 1.., value_delimiter = ',')]
    pub blocked_by: Vec<BlockerRef>,

    /// Who is applying the park — a role name (`doctor`, `champion`, `curator`)
    /// or `human`.
    #[arg(long = "by", value_name = "ROLE")]
    pub by: Option<String>,

    /// When, as RFC 3339. Defaults to now.
    #[arg(long = "at", value_name = "TIMESTAMP")]
    pub at: Option<String>,

    /// One-line free-text reason. Sanitised: flattened to a single line, `"`
    /// downgraded to `'`, and `--` runs collapsed so the reason cannot terminate
    /// the HTML comment early.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct ParseArgs {
    /// Read the body from this file. Use `-` (or omit) for stdin.
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Emit one JSON object instead of one blocker number per line.
    #[arg(long)]
    pub json: bool,
}

/// `loom-tools park-record <action>`: the [`ParkRecordCommand`] tree, unchanged
/// (#14464, epic #12708 Phase 3b). `loom-tools` and `loom-daemon park-record`
/// parse the one clap family and run the one handler in-process, so there is no
/// second implementation to drift. The field is private so the crate-private
/// command type does not leak into the public API.
#[derive(clap::Args)]
pub struct ParkRecordCli {
    #[command(subcommand)]
    command: ParkRecordCommand,
}

impl ParkRecordCli {
    /// Whether this invocation can reach the forge, and so must pass the mode
    /// gate (design §10.3) before it runs. Classified by effect, not by verb:
    /// `render`, `parse`, `retract --file`, `released` and `decision` touch only
    /// argv, stdin, a file or the local role journal; `apply` (a dry-run still
    /// reads the target and any blocker) and a forge `retract` do reach it.
    #[must_use]
    pub fn reaches_forge(&self) -> bool {
        self.command.reaches_forge()
    }
}

/// Run the park-record action through the one shared handler.
///
/// # Errors
/// Whatever the handler returns; most actions exit the process themselves with
/// their documented exit codes.
pub fn handle_park_record_command(cli: ParkRecordCli) -> Result<()> {
    let writer = WorkflowWriter::for_tools(cli.reaches_forge())?;
    cli.command.run(writer)
}

impl ParkRecordCommand {
    /// See [`ParkRecordCli::reaches_forge`]. An exhaustive `match` with no
    /// wildcard: a new action does not compile until its author says whether it
    /// reaches the forge.
    pub(crate) fn reaches_forge(&self) -> bool {
        match self {
            ParkRecordCommand::Render(_)
            | ParkRecordCommand::Parse(_)
            | ParkRecordCommand::Released(_)
            | ParkRecordCommand::Decision(_)
            | ParkRecordCommand::GuideConflictRoute(_) => false,
            ParkRecordCommand::Apply(_) => true,
            // `--file` composes against a local body: the forge is never read.
            ParkRecordCommand::Retract(args) => args.file.is_none(),
        }
    }

    pub(crate) fn run(self, writer: WorkflowWriter) -> Result<()> {
        match self {
            ParkRecordCommand::Render(args) => args.run(),
            ParkRecordCommand::Parse(args) => args.run(),
            ParkRecordCommand::Apply(args) => args.run(writer),
            ParkRecordCommand::Released(args) => {
                args.run();
                Ok(())
            }
            ParkRecordCommand::Decision(args) => {
                args.run();
                Ok(())
            }
            ParkRecordCommand::Retract(args) => args.run(writer),
            ParkRecordCommand::GuideConflictRoute(args) => args.run(),
        }
    }
}

impl RenderArgs {
    fn run(self) -> Result<()> {
        let at = self.at.unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });
        println!(
            "{}",
            park_record::render_park(
                &self.blocked_by,
                self.by.as_deref(),
                Some(&at),
                self.reason.as_deref(),
            )
        );
        Ok(())
    }
}

impl ReleasedArgs {
    fn run(self) {
        use crate::telemetry::kinds::hold_transition::{reason, transition};
        let token = |t: &str| {
            let ok = !t.is_empty()
                && t.len() <= 40
                && t.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if ok {
                t.to_string()
            } else {
                "unknown".to_string()
            }
        };
        let entered_at = self
            .entered_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        crate::observability::pick_journal::record_hold(
            self.pr,
            transition::REMOVE,
            reason::HOLD_RELEASED,
            &token(&self.trigger),
            !self.failed,
            entered_at,
            None,
        );
    }
}

impl DecisionArgs {
    fn run(self) {
        use crate::telemetry::kinds::hold_transition::{reason, transition};
        // A non-finite or out-of-range value is dropped, never exported.
        let confidence_pct = self
            .confidence
            .filter(|c| c.is_finite() && (0.0..=1.0).contains(c))
            .map(|c| (c * 100.0).round() as u8);
        let reason_code = match self.outcome {
            DecisionOutcome::Hold => reason::MERGE_RISK_HOLD,
            DecisionOutcome::SelfRelease => reason::MERGE_RISK_SELF_RELEASE,
            DecisionOutcome::Pass => reason::MERGE_RISK_PASS,
        };
        crate::observability::pick_journal::record_hold(
            self.pr,
            transition::DECISION,
            reason_code,
            "champion_role",
            true,
            None,
            confidence_pct,
        );
    }
}

impl ApplyArgs {
    /// Never returns: exits with the command's own code.
    fn run(self, writer: WorkflowWriter) -> Result<()> {
        use crate::operator_decision::cli::default_repo_root;
        use crate::park_record::apply::{apply, ApplyRequest};
        let is_pr = self.issue.is_none() && self.pr.is_some();
        let req = ApplyRequest {
            number: self.issue.or(self.pr).unwrap_or_default(),
            repo: self.repo.clone(),
            blocked_by: self.blocked_by,
            reason: self.reason,
            by: self.by,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            remove_labels: self.remove_label,
            dry_run: self.dry_run,
            is_pr,
            transient: self.transient,
            jitter: random_unit(),
        };
        let mut forge = writer.forge(default_repo_root(), self.repo);
        let code = apply(&mut forge, &req, &mut std::io::stdout(), &mut std::io::stderr());
        crate::park_record::apply::journal_applied(&req, is_pr, code);
        std::process::exit(code)
    }
}

impl RetractArgs {
    /// Never returns: exits with the command's own code.
    fn run(self, writer: WorkflowWriter) -> Result<()> {
        use crate::operator_decision::cli::default_repo_root;
        use crate::park_record::apply::{retract, RetractRequest};
        let req = RetractRequest {
            number: self.issue.or(self.pr).unwrap_or_default(),
            repo: self.repo.clone(),
            blocked_by: self.blocked_by,
            reason: self.reason,
            by: self.by,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            dry_run: self.dry_run,
        };
        let code = match self.file.as_deref() {
            Some(p) => {
                let body = match p {
                    p if p == std::path::Path::new("-") => {
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf)?;
                        buf
                    }
                    p => std::fs::read_to_string(p)?,
                };
                crate::park_record::apply::retract_offline(
                    &req,
                    &body,
                    &mut std::io::stdout(),
                    &mut std::io::stderr(),
                )
            }
            None => {
                let mut forge = writer.forge(default_repo_root(), self.repo);
                retract(&mut forge, &req, &mut std::io::stdout(), &mut std::io::stderr())
            }
        };
        std::process::exit(code)
    }
}

/// A value in `-1.0..=1.0` from the clock's sub-second nanos: backoff jitter
/// needs spread across hosts, not cryptographic randomness.
fn random_unit() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 2001) / 1000.0 - 1.0
}

impl ParseArgs {
    fn run(self) -> Result<()> {
        let body = match self.file.as_deref() {
            Some(p) if p != std::path::Path::new("-") => std::fs::read_to_string(p)?,
            _ => {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            }
        };

        let records = park_record::parse(&body);
        let blockers = park_record::blockers(&body);

        if self.json {
            let rows: Vec<_> = records.iter().map(record_json).collect();
            // Retraction audit lines (#13895): a withdrawn blocker is no longer
            // declared, so it appears only here — never in `blockers`.
            let gone: Vec<_> = park_record::withdrawn(&body)
                .iter()
                .map(withdrawn_json)
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "records": rows,
                    // Local blockers stay bare numbers (unchanged); qualified ones are
                    // `OWNER/REPO#N` strings.
                    "blockers": blockers.iter().map(|b| match &b.repo {
                        None => serde_json::json!(b.number),
                        Some(_) => serde_json::json!(b.to_string()),
                    }).collect::<Vec<_>>(),
                    "withdrawn": gone,
                    "declared": !records.is_empty(),
                })
            );
        } else {
            for b in &blockers {
                // Bare number for a local blocker (unchanged), qualified otherwise.
                match &b.repo {
                    None => println!("{}", b.number),
                    Some(_) => println!("{b}"),
                }
            }
        }

        // Exit 1 for "no record", mirroring `forge check-open-pr`'s convention
        // that an answer of "none" is an exit code, not an error.
        if records.is_empty() {
            std::process::exit(1);
        }
        Ok(())
    }
}

fn withdrawn_json(w: &Withdrawn) -> serde_json::Value {
    serde_json::json!({
        "blocker": w.blocker.number,
        "blocker_repo": w.blocker.repo.clone(),
        "by": w.by,
        "at": w.at,
        "reason": w.reason,
    })
}

fn record_json(r: &ParkRecord) -> serde_json::Value {
    let now = chrono::Utc::now().to_rfc3339();
    // `null` for a legacy park; `due` reuses `transient_due` so a shell caller
    // (Guide's unblock sweep, #13606) never re-implements the missing/garbled
    // `until` rule.
    let transient = r.transient.as_ref().map(|t| {
        serde_json::json!({
            "cause": t.cause,
            "attempt": t.attempt,
            "until": t.until,
            "due": park_record::transient_due(t, &now),
            "escalate": t.attempt >= park_record::TRANSIENT_ESCALATE_ATTEMPT,
        })
    });
    serde_json::json!({
        "blocker": r.blocker.as_ref().map(|b| b.number),
        "blocker_repo": r.blocker.as_ref().and_then(|b| b.repo.clone()),
        "by": r.by,
        "at": r.at,
        "reason": r.reason,
        "transient": transient,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_record_json(body: &str) -> serde_json::Value {
        record_json(&park_record::parse(body)[0])
    }

    #[test]
    fn a_withdrawn_audit_line_surfaces_ref_by_at_and_reason_in_json() {
        let w = park_record::parse_withdrawn(
            "Park withdrawn: #5 \u{2014} by=guide at=2026-11-15T12:00:00Z reason=\"not a gate\"",
        )
        .unwrap();
        let v = withdrawn_json(&w);
        assert_eq!(v["blocker"], 5);
        assert!(v["blocker_repo"].is_null());
        assert_eq!(v["by"], "guide");
        assert_eq!(v["at"], "2026-11-15T12:00:00Z");
        assert_eq!(v["reason"], "not a gate");
    }

    #[test]
    fn legacy_park_has_null_transient() {
        let v = first_record_json(
            "<!-- loom:park Blocked by: #5 by=builder at=2026-11-05T12:00:00Z -->",
        );
        assert!(v["transient"].is_null());
    }

    #[test]
    fn expired_transient_is_due_and_future_is_not() {
        let past = first_record_json(
            "<!-- loom:park Blocked by: (unstated) by=builder at=2026-11-05T12:00:00Z \
             transient=capacity:batch attempt=2 until=2026-11-05T12:30:00Z -->",
        );
        assert_eq!(past["transient"]["cause"], "capacity:batch");
        assert_eq!(past["transient"]["attempt"], 2);
        assert_eq!(past["transient"]["due"], true);
        assert_eq!(past["transient"]["escalate"], false);

        let future = first_record_json(
            "<!-- loom:park Blocked by: (unstated) by=builder at=2026-11-05T12:00:00Z \
             transient=capacity:batch attempt=6 until=2999-01-01T00:00:00Z -->",
        );
        assert_eq!(future["transient"]["due"], false);
        assert_eq!(future["transient"]["escalate"], true);
    }

    #[test]
    fn missing_until_is_due() {
        let v = first_record_json(
            "<!-- loom:park Blocked by: (unstated) by=builder at=2026-11-05T12:00:00Z \
             transient=capacity:batch attempt=1 -->",
        );
        assert_eq!(v["transient"]["due"], true);
    }
}
