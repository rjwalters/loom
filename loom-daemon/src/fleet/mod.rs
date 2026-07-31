//! Operator-triggered multi-host worker fanout (`loom-daemon fleet …`, epic
//! #4340). This module owns the **fanout brain**: the boundary decision on
//! #4340 puts "when to expand, what a worker is, dispatch/drain/teardown, fleet
//! status" in `loom-daemon`, not in installed shell scripts — so the bootstrap
//! is modeled as an ordered, idempotent plan the daemon owns, not a monolithic
//! bash script under `defaults/scripts/`.
//!
//! # What this phase delivers (issue #4341 — `fleet add-worker`)
//!
//! `loom-daemon fleet add-worker <ssh-host> --repo <workspace-repo>` takes a
//! reachable, already-provisioned Ubuntu host (an SSH alias from `repo:remote`
//! or operator-supplied) to "daemon running, workspace registered, tokens
//! ranked, dispatch verified" in one command. Generic VM provisioning stays in
//! `repo:remote` (rjwalters/repo) per the epic's boundary decision — this
//! command consumes "a reachable box + an SSH alias", it never wrangles a cloud
//! CLI.
//!
//! ## Architecture — step planner + executor
//!
//! The bootstrap is an ordered [`Plan`] of named [`Step`]s. Each step has three
//! phases:
//!
//! - **check** — shell that exits `0` when the step is already satisfied. This
//!   is what makes a re-run idempotent (AC 2): an already-bootstrapped host
//!   reports every step [`StepStatus::AlreadyDone`] and touches nothing.
//! - **apply** — shell that performs the step (only run when `check` is absent
//!   or fails).
//! - **verify** — optional shell that confirms `apply` succeeded.
//!
//! Rust owns the plan, the ordering, and the per-step checklist report. The
//! shell for each phase is rendered in [`add_worker`] (heredoc-style templates,
//! the way `init::templates` renders scaffolding) and executed over a
//! [`CommandRunner`] — the real one is an `ssh <host> <shell>` runner, but the
//! trait lets the unit tests drive the whole plan through a mock without a live
//! box. A `--dry-run` flag prints the ordered plan without contacting the host,
//! which makes the verify AC testable in CI.
//!
//! ## Fleet registry
//!
//! [`add_worker`] writes one [`WorkerRecord`] per bootstrapped worker into a
//! machine-level [`FleetRegistry`] at `~/.loom/fleet.json`, following the
//! conventions of [`crate::workspace_registry`]. The sibling commands `fleet
//! status` (#4342) and `fleet drain` (#4343) enumerate workers from this
//! inventory — `add-worker` is where it gets written. The schema is kept
//! deliberately minimal; the siblings can extend it.

pub mod add_worker;
pub mod drain;
pub mod status;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ===========================================================================
// Command runner (the SSH / mock seam)
// ===========================================================================

/// Captured result of running one shell command on the target host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process exit code (`-1` when the child was killed by a signal / never
    /// produced a code — treated as a failure by every caller).
    pub code: i32,
    /// Captured stdout (lossily UTF-8 decoded).
    pub stdout: String,
    /// Captured stderr (lossily UTF-8 decoded).
    pub stderr: String,
}

impl CommandOutput {
    /// Whether the command exited cleanly (`code == 0`).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// The seam between the step executor and the host: a `run` that executes one
/// rendered shell command against the target, optionally feeding `stdin`.
///
/// The production implementation ([`add_worker::SshRunner`]) runs `ssh <host>
/// <shell>`; the tests supply a scripted mock so the whole plan can be
/// exercised — including secrets-over-stdin and idempotent re-runs — without a
/// live box or the network.
pub trait CommandRunner {
    /// Run `shell` on the target. When `stdin` is `Some`, its bytes are written
    /// to the command's standard input (this is how secrets reach the host —
    /// never on the command line, where `ps` would expose them).
    ///
    /// Returns the captured [`CommandOutput`]. An `Err` is reserved for a
    /// failure to *launch* the transport at all (e.g. `ssh` missing) — a
    /// non-zero remote exit is a successful launch and is reported via
    /// [`CommandOutput::code`].
    fn run(&self, shell: &str, stdin: Option<&str>) -> Result<CommandOutput>;
}

// ===========================================================================
// Step / plan model
// ===========================================================================

/// A secret or plain payload fed to a step's `apply` command over stdin.
#[derive(Clone)]
pub struct StepStdin {
    /// The bytes written to the command's stdin.
    pub content: String,
    /// When `true`, the content is a secret: it is never printed (dry-run and
    /// logs show `<redacted>`), and it only ever travels over stdin.
    pub secret: bool,
}

impl std::fmt::Debug for StepStdin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render secret bytes through Debug (avoids accidental leakage in
        // a `{:?}` log line).
        if self.secret {
            f.debug_struct("StepStdin")
                .field("secret", &true)
                .field("content", &"<redacted>")
                .finish()
        } else {
            f.debug_struct("StepStdin")
                .field("secret", &false)
                .field("content", &self.content)
                .finish()
        }
    }
}

/// One named unit of the bootstrap plan.
#[derive(Debug, Clone)]
pub struct Step {
    /// Stable machine name (`base-deps`, `token-pool`, …) used in the checklist
    /// and by callers matching on a specific step.
    pub name: String,
    /// One-line human summary shown in the dry-run plan and the checklist.
    pub summary: String,
    /// Shell that exits `0` when the step is already satisfied. `None` means the
    /// step is always applied (e.g. a `check` + `--ranking` refresh we want to
    /// re-run every time). A present `check` is what classifies a re-run as
    /// [`StepStatus::AlreadyDone`].
    pub check: Option<String>,
    /// Shell that performs the step. Run only when `check` is `None` or fails.
    pub apply: String,
    /// Shell that confirms the step succeeded. `None` trusts `apply`'s exit
    /// code.
    pub verify: Option<String>,
    /// Optional stdin payload for the `apply` command (how secrets reach the
    /// host — see [`StepStdin`]).
    pub stdin: Option<StepStdin>,
}

impl Step {
    /// Convenience constructor for a check → apply step with no stdin/verify.
    #[must_use]
    pub fn new(name: &str, summary: &str, check: Option<String>, apply: String) -> Self {
        Self {
            name: name.to_string(),
            summary: summary.to_string(),
            check,
            apply,
            verify: None,
            stdin: None,
        }
    }

    /// Builder: attach a `verify` phase.
    #[must_use]
    pub fn with_verify(mut self, verify: String) -> Self {
        self.verify = Some(verify);
        self
    }

    /// Builder: attach an stdin payload for `apply`.
    #[must_use]
    pub fn with_stdin(mut self, stdin: StepStdin) -> Self {
        self.stdin = Some(stdin);
        self
    }
}

/// An entry in the ordered plan: either an executable [`Step`] or a
/// deliberately-skipped step (config-gated, e.g. safehouse when it is not
/// enabled or its prerequisite has not landed).
#[derive(Debug, Clone)]
pub enum PlanEntry {
    /// A step to execute (check → apply → verify).
    Step(Step),
    /// A step intentionally not run, with an operator-facing reason. Reported as
    /// [`StepStatus::Skipped`] and shown in the dry-run plan so the omission is
    /// never silent.
    Skip {
        /// Stable machine name.
        name: String,
        /// One-line human summary.
        summary: String,
        /// Why the step is skipped (e.g. `"safehouse.enabled is false"`).
        reason: String,
    },
}

impl PlanEntry {
    /// The step's machine name, regardless of variant.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            PlanEntry::Step(s) => &s.name,
            PlanEntry::Skip { name, .. } => name,
        }
    }

    /// The step's one-line summary, regardless of variant.
    #[must_use]
    pub fn summary(&self) -> &str {
        match self {
            PlanEntry::Step(s) => &s.summary,
            PlanEntry::Skip { summary, .. } => summary,
        }
    }
}

/// An ordered bootstrap plan.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The plan entries, in execution order.
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    /// A new, empty plan.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an executable step.
    pub fn push_step(&mut self, step: Step) {
        self.entries.push(PlanEntry::Step(step));
    }

    /// Append a deliberately-skipped step.
    pub fn push_skip(&mut self, name: &str, summary: &str, reason: &str) {
        self.entries.push(PlanEntry::Skip {
            name: name.to_string(),
            summary: summary.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Render the ordered plan for `--dry-run`: one numbered line per entry with
    /// its status placeholder, plus a `[skip]` reason where applicable. Secrets
    /// are never included (the rendered shell that carries them is not printed).
    #[must_use]
    pub fn render_dry_run(&self, host: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "fleet add-worker plan for {host} ({} steps):\n",
            self.entries.len()
        ));
        for (i, entry) in self.entries.iter().enumerate() {
            let n = i + 1;
            match entry {
                PlanEntry::Step(s) => {
                    let secret_note = if s.stdin.as_ref().is_some_and(|p| p.secret) {
                        "  (feeds a secret via stdin)"
                    } else {
                        ""
                    };
                    out.push_str(&format!("  {n:>2}. [{}] {}{}\n", s.name, s.summary, secret_note));
                }
                PlanEntry::Skip {
                    name,
                    summary,
                    reason,
                } => {
                    out.push_str(&format!("  {n:>2}. [{name}] {summary}  — SKIP: {reason}\n"));
                }
            }
        }
        out
    }
}

// ===========================================================================
// Execution + checklist report
// ===========================================================================

/// Classification of a single step's outcome after execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// The `check` phase reported the step already satisfied — nothing applied
    /// (the idempotent-re-run case, AC 2).
    AlreadyDone,
    /// The step was applied (and verified, if it had a `verify`) successfully.
    Changed,
    /// The step was deliberately not run; carries the reason.
    Skipped(String),
    /// The step failed; carries a human diagnostic (which phase, exit code, a
    /// trailing slice of stderr).
    Failed(String),
}

impl StepStatus {
    /// A short tag for the checklist column.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            StepStatus::AlreadyDone => "unchanged",
            StepStatus::Changed => "changed",
            StepStatus::Skipped(_) => "skipped",
            StepStatus::Failed(_) => "FAILED",
        }
    }

    /// Whether this status halts the plan (only [`StepStatus::Failed`] does).
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, StepStatus::Failed(_))
    }
}

/// The result of one executed plan entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// The step's machine name.
    pub name: String,
    /// The step's one-line summary.
    pub summary: String,
    /// The classified outcome.
    pub status: StepStatus,
}

/// Execute a single [`Step`] against `runner`, returning its [`StepReport`].
///
/// Phase order: `check` (if present and exit 0 ⇒ [`StepStatus::AlreadyDone`]) →
/// `apply` (feeding `stdin`) → `verify` (if present). Any phase that fails to
/// launch, or exits non-zero on `apply`/`verify`, yields [`StepStatus::Failed`]
/// with a diagnostic that never includes a secret payload.
pub fn execute_step(runner: &dyn CommandRunner, step: &Step) -> StepReport {
    let report = |status| StepReport {
        name: step.name.clone(),
        summary: step.summary.clone(),
        status,
    };

    // 1. check — is it already done?
    if let Some(check) = &step.check {
        match runner.run(check, None) {
            Ok(out) if out.ok() => return report(StepStatus::AlreadyDone),
            Ok(_) => { /* not done — fall through to apply */ }
            Err(e) => return report(StepStatus::Failed(format!("check phase could not run: {e}"))),
        }
    }

    // 2. apply
    let stdin = step.stdin.as_ref().map(|p| p.content.as_str());
    match runner.run(&step.apply, stdin) {
        Ok(out) if out.ok() => { /* applied — verify next */ }
        Ok(out) => {
            return report(StepStatus::Failed(format!(
                "apply exited {}: {}",
                out.code,
                tail_stderr(&out.stderr)
            )))
        }
        Err(e) => return report(StepStatus::Failed(format!("apply phase could not run: {e}"))),
    }

    // 3. verify
    if let Some(verify) = &step.verify {
        match runner.run(verify, None) {
            Ok(out) if out.ok() => {}
            Ok(out) => {
                return report(StepStatus::Failed(format!(
                    "verify exited {}: {}",
                    out.code,
                    tail_stderr(&out.stderr)
                )))
            }
            Err(e) => {
                return report(StepStatus::Failed(format!("verify phase could not run: {e}")))
            }
        }
    }

    report(StepStatus::Changed)
}

/// Execute a whole [`Plan`] against `runner`, halting at the first failed step
/// (so a broken host does not cascade half-applied steps). Returns the reports
/// gathered up to and including the halting step — every remaining entry is left
/// unreported, and the caller surfaces the failure.
pub fn execute_plan(runner: &dyn CommandRunner, plan: &Plan) -> Vec<StepReport> {
    let mut reports = Vec::with_capacity(plan.entries.len());
    for entry in &plan.entries {
        match entry {
            PlanEntry::Skip {
                name,
                summary,
                reason,
            } => {
                reports.push(StepReport {
                    name: name.clone(),
                    summary: summary.clone(),
                    status: StepStatus::Skipped(reason.clone()),
                });
            }
            PlanEntry::Step(step) => {
                let report = execute_step(runner, step);
                let failed = report.status.is_failure();
                reports.push(report);
                if failed {
                    break;
                }
            }
        }
    }
    reports
}

/// Render the per-step checklist from a set of [`StepReport`]s (AC: a re-run
/// "reports unchanged steps"). Human-readable, one line per step.
#[must_use]
pub fn render_checklist(host: &str, reports: &[StepReport]) -> String {
    let mut out = String::new();
    out.push_str(&format!("fleet add-worker checklist for {host}:\n"));
    for (i, r) in reports.iter().enumerate() {
        let n = i + 1;
        let mark = match r.status {
            StepStatus::AlreadyDone => "=",
            StepStatus::Changed => "+",
            StepStatus::Skipped(_) => "-",
            StepStatus::Failed(_) => "x",
        };
        out.push_str(&format!("  {mark} {n:>2}. [{}] {} ({})", r.name, r.summary, r.status.tag()));
        match &r.status {
            StepStatus::Skipped(reason) => out.push_str(&format!(" — {reason}")),
            StepStatus::Failed(diag) => out.push_str(&format!(" — {diag}")),
            _ => {}
        }
        out.push('\n');
    }
    out
}

/// Whether the reports represent a fully-successful run (no failed step, and
/// every non-skip entry was reported — i.e. the plan did not halt early).
#[must_use]
pub fn all_succeeded(reports: &[StepReport], plan: &Plan) -> bool {
    reports.len() == plan.entries.len() && !reports.iter().any(|r| r.status.is_failure())
}

/// Trim a stderr blob to its last non-empty line (bounded), so a diagnostic
/// stays one line and never dumps a multi-KB build log into the checklist.
fn tail_stderr(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.len() > 200 {
        format!("{}…", &line[..200])
    } else {
        line.to_string()
    }
}

// ===========================================================================
// Fleet registry (`~/.loom/fleet.json`)
// ===========================================================================

/// Environment override for the fleet-registry file location (mirrors
/// [`crate::workspace_registry::REGISTRY_PATH_ENV`]). Primarily a test seam.
pub const FLEET_REGISTRY_PATH_ENV: &str = "LOOM_FLEET_PATH";

/// Current on-disk schema version for the fleet registry.
pub const FLEET_REGISTRY_VERSION: u32 = 1;

/// The outcome of the most recent `verify` step for a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyResult {
    /// Whether the verify phase passed.
    pub ok: bool,
    /// RFC3339 timestamp of the verification.
    pub at: String,
    /// One-line human summary (e.g. `"daemon running, 7 tokens ranked"`).
    pub summary: String,
}

/// One bootstrapped worker's inventory record. Kept deliberately minimal at
/// #4341 — the sibling `fleet status`/`fleet drain` commands (#4342/#4343)
/// extend it below with `#[serde(default)]`/`Option` fields so an
/// already-written `~/.loom/fleet.json` from an older binary keeps parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    /// The SSH alias/host used to reach the worker (the canonical key — two
    /// records with the same `ssh_host` are deduplicated on upsert).
    pub ssh_host: String,
    /// The workspace repos registered on the worker (as passed to `--repo`).
    #[serde(default)]
    pub repos: Vec<String>,
    /// The dispatch priority the workspaces were registered at.
    #[serde(default = "crate::workspace_registry::default_priority")]
    pub priority: u32,
    /// RFC3339 timestamp of the most recent successful bootstrap. Doubles as
    /// the roster's "added date" (#4342's operator scope-clarification).
    pub bootstrapped_at: String,
    /// Result of the last `verify` step, if one ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verify: Option<VerifyResult>,
    /// Cloud provider instance id (e.g. an EC2 instance id), when known. Part
    /// of the canonical-registry field list from #4342's operator
    /// scope-clarification — replaces the per-repo `.env` instance pins
    /// (`REPO_REMOTE_INSTANCE_ID`) as the source of truth. Absent on records
    /// written by a pre-#4342 binary or a manually-added worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
    /// The host's tailnet device name (e.g. `loom-worker-1`), when the worker
    /// is on a tailnet. Rendered alongside `ssh_host` in `fleet status`'s
    /// human table (#4342).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet_name: Option<String>,
    /// Who/what added this worker to the registry (operator handle, or an
    /// automation identifier), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    /// Registry-level lifecycle state. `None`/absent means active (the
    /// overwhelmingly common case, including every record written before this
    /// field existed) — only the literal `"draining"` (written by `fleet
    /// drain`'s first phase, #4343) changes rendering, and it must never be
    /// mistaken for a parse error. Modeled as a bare `String` (not an enum) so
    /// an unrecognized future state still round-trips instead of failing to
    /// deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The most recently *completed* `fleet drain` phase (#4343), a stable
    /// machine name (see [`drain::DrainPhase::name`]). `None` means no drain
    /// has ever started (or the record predates #4343). This is the
    /// resumability marker (body step 6): a re-run reads it and skips every
    /// phase up to and including this one, so an interrupted drain (crash,
    /// killed SSH, operator Ctrl-C) picks up where it left off rather than
    /// re-running already-applied phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_phase: Option<String>,
    /// The `loom:building` claims captured by drain's `capture-claims` phase,
    /// before the remote daemon is told to drain — persisted immediately so a
    /// crash between capture and the later `reset-claims` phase does not lose
    /// the list (the exact stranding window the epic's pilot evidence, anvil
    /// #758, cites). Empty on a record with no drain in progress.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drain_captured: Vec<drain::CapturedClaim>,
    /// The idle-shutdown guard's configured window (minutes), when installed
    /// via `fleet add-worker --idle-shutdown-minutes` (the stage-2 cron guard
    /// `add_worker::render_idle_shutdown()` renders). `None` means no guard is
    /// installed — including every record written before this field existed.
    /// `fleet status` (#4697) uses this together with [`Self::last_seen_up_at`]
    /// to distinguish an EXPECTED power-off (the host idle-shut itself down,
    /// by design — #3998/#4477) from a genuinely UNEXPECTED outage when a host
    /// stops answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_shutdown_minutes: Option<u32>,
    /// RFC3339 timestamp of the most recent `fleet status` poll that observed
    /// this host [`crate::fleet::status::HostState::Up`]. Written by `fleet
    /// status` itself (#4697) on every poll that sees the host up; `None`
    /// until the first such observation (a freshly-bootstrapped worker whose
    /// first poll hasn't run yet, or a record predating this field). This is
    /// the reference point the idle-shutdown "expected vs unexpected"
    /// heuristic measures elapsed silence from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_up_at: Option<String>,
}

/// The machine-level set of bootstrapped workers, persisted at
/// `~/.loom/fleet.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRegistry {
    /// On-disk schema version.
    #[serde(default = "default_fleet_version")]
    pub version: u32,
    /// Bootstrapped workers, in insertion order.
    #[serde(default)]
    pub workers: Vec<WorkerRecord>,
}

fn default_fleet_version() -> u32 {
    FLEET_REGISTRY_VERSION
}

impl Default for FleetRegistry {
    fn default() -> Self {
        Self {
            version: FLEET_REGISTRY_VERSION,
            workers: Vec::new(),
        }
    }
}

/// Resolve the fleet-registry file path: honour [`FLEET_REGISTRY_PATH_ENV`]
/// first, else `~/.loom/fleet.json`.
pub fn default_fleet_registry_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(FLEET_REGISTRY_PATH_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".loom").join("fleet.json"))
}

impl FleetRegistry {
    /// Load the registry from `path`. A missing or empty file yields an empty
    /// registry; a present-but-unparseable file is an error so a corrupted
    /// registry is loud rather than silently reset (mirrors
    /// [`crate::workspace_registry::WorkspaceRegistry::load`]).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                if contents.trim().is_empty() {
                    return Ok(Self::default());
                }
                let registry: Self = serde_json::from_str(&contents)
                    .with_context(|| format!("parsing fleet registry at {}", path.display()))?;
                Ok(registry)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => {
                Err(e).with_context(|| format!("reading fleet registry at {}", path.display()))
            }
        }
    }

    /// Load from the default registry path ([`default_fleet_registry_path`]).
    pub fn load_default() -> Result<Self> {
        Self::load(&default_fleet_registry_path()?)
    }

    /// Persist the registry to `path` atomically (temp file + rename), creating
    /// the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating fleet registry dir {}", parent.display()))?;
        }
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing temp fleet registry {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Persist to the default registry path.
    pub fn save_default(&self) -> Result<()> {
        self.save(&default_fleet_registry_path()?)
    }

    /// Insert or replace the record for `record.ssh_host` (dedup on the SSH
    /// alias — a re-bootstrap updates the existing record in place rather than
    /// duplicating it). Returns `true` when an existing record was replaced,
    /// `false` when a new one was inserted.
    pub fn upsert(&mut self, record: WorkerRecord) -> bool {
        if let Some(existing) = self
            .workers
            .iter_mut()
            .find(|w| w.ssh_host == record.ssh_host)
        {
            *existing = record;
            true
        } else {
            self.workers.push(record);
            false
        }
    }

    /// The record for `ssh_host`, if present.
    #[must_use]
    pub fn get(&self, ssh_host: &str) -> Option<&WorkerRecord> {
        self.workers.iter().find(|w| w.ssh_host == ssh_host)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // ---- Mock command runner -------------------------------------------

    /// A scripted [`CommandRunner`]: each `run` pops the next canned
    /// [`CommandOutput`] and records the `(shell, stdin)` it was asked to run,
    /// so tests can assert on both ordering/idempotency and secret handling.
    struct MockRunner {
        responses: RefCell<Vec<CommandOutput>>,
        calls: RefCell<Vec<(String, Option<String>)>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Option<String>)> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, shell: &str, stdin: Option<&str>) -> Result<CommandOutput> {
            self.calls
                .borrow_mut()
                .push((shell.to_string(), stdin.map(str::to_string)));
            let mut r = self.responses.borrow_mut();
            if r.is_empty() {
                Ok(CommandOutput {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            } else {
                Ok(r.remove(0))
            }
        }
    }

    fn ok() -> CommandOutput {
        CommandOutput {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
    fn nonzero(code: i32, stderr: &str) -> CommandOutput {
        CommandOutput {
            code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    // ---- execute_step --------------------------------------------------

    #[test]
    fn check_pass_reports_already_done_without_applying() {
        // check exits 0 → apply must NOT run.
        let runner = MockRunner::new(vec![ok()]);
        let step = Step::new("s", "summary", Some("check-cmd".into()), "apply-cmd".into());
        let report = execute_step(&runner, &step);
        assert_eq!(report.status, StepStatus::AlreadyDone);
        // Only the check ran.
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "check-cmd");
    }

    #[test]
    fn check_fail_then_apply_reports_changed() {
        // check exits non-zero → apply runs and succeeds.
        let runner = MockRunner::new(vec![nonzero(1, ""), ok()]);
        let step = Step::new("s", "summary", Some("check-cmd".into()), "apply-cmd".into());
        let report = execute_step(&runner, &step);
        assert_eq!(report.status, StepStatus::Changed);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "apply-cmd");
    }

    #[test]
    fn no_check_always_applies() {
        let runner = MockRunner::new(vec![ok()]);
        let step = Step::new("s", "summary", None, "apply-cmd".into());
        let report = execute_step(&runner, &step);
        assert_eq!(report.status, StepStatus::Changed);
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn apply_nonzero_reports_failed_with_stderr_tail() {
        let runner = MockRunner::new(vec![nonzero(2, "line one\nboom: the real error\n")]);
        let step = Step::new("s", "summary", None, "apply-cmd".into());
        let report = execute_step(&runner, &step);
        match report.status {
            StepStatus::Failed(diag) => {
                assert!(diag.contains("exited 2"), "diag: {diag}");
                assert!(diag.contains("boom: the real error"), "diag: {diag}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn verify_failure_reports_failed() {
        // check absent, apply ok, verify non-zero.
        let runner = MockRunner::new(vec![ok(), nonzero(3, "verify said no")]);
        let step = Step::new("s", "summary", None, "apply".into()).with_verify("verify".into());
        let report = execute_step(&runner, &step);
        assert!(report.status.is_failure());
    }

    #[test]
    fn secret_stdin_is_fed_to_apply_only() {
        let runner = MockRunner::new(vec![ok()]);
        let step = Step::new("s", "secret step", None, "cat > dest".into()).with_stdin(StepStdin {
            content: "super-secret-pat".into(),
            secret: true,
        });
        let report = execute_step(&runner, &step);
        assert_eq!(report.status, StepStatus::Changed);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1.as_deref(), Some("super-secret-pat"));
    }

    // ---- execute_plan / idempotency ------------------------------------

    #[test]
    fn plan_ordering_is_preserved() {
        let mut plan = Plan::new();
        plan.push_step(Step::new("first", "1", None, "a".into()));
        plan.push_step(Step::new("second", "2", None, "b".into()));
        plan.push_step(Step::new("third", "3", None, "c".into()));
        let runner = MockRunner::new(vec![ok(), ok(), ok()]);
        let reports = execute_plan(&runner, &plan);
        let names: Vec<_> = reports.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn idempotent_rerun_reports_every_step_unchanged() {
        // Every step has a check that passes → all AlreadyDone, nothing applied.
        let mut plan = Plan::new();
        for i in 0..3 {
            plan.push_step(Step::new(
                &format!("s{i}"),
                "summary",
                Some(format!("check-{i}")),
                format!("apply-{i}"),
            ));
        }
        let runner = MockRunner::new(vec![ok(), ok(), ok()]);
        let reports = execute_plan(&runner, &plan);
        assert!(reports.iter().all(|r| r.status == StepStatus::AlreadyDone));
        assert!(all_succeeded(&reports, &plan));
        // Only the 3 checks ran — zero applies.
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn plan_halts_on_first_failure() {
        let mut plan = Plan::new();
        plan.push_step(Step::new("ok1", "1", None, "a".into()));
        plan.push_step(Step::new("boom", "2", None, "b".into()));
        plan.push_step(Step::new("never", "3", None, "c".into()));
        // first ok, second fails, third should never run.
        let runner = MockRunner::new(vec![ok(), nonzero(1, "nope")]);
        let reports = execute_plan(&runner, &plan);
        assert_eq!(reports.len(), 2, "halts after the failing step");
        assert_eq!(reports[0].status, StepStatus::Changed);
        assert!(reports[1].status.is_failure());
        assert!(!all_succeeded(&reports, &plan));
    }

    #[test]
    fn skip_entries_report_skipped_and_do_not_run() {
        let mut plan = Plan::new();
        plan.push_step(Step::new("real", "do it", None, "a".into()));
        plan.push_skip("safehouse", "wire safehouse", "safehouse.enabled is false");
        let runner = MockRunner::new(vec![ok()]);
        let reports = execute_plan(&runner, &plan);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].status, StepStatus::Changed);
        match &reports[1].status {
            StepStatus::Skipped(reason) => assert_eq!(reason, "safehouse.enabled is false"),
            other => panic!("expected Skipped, got {other:?}"),
        }
        // Only the one real step ran on the host.
        assert_eq!(runner.calls().len(), 1);
    }

    // ---- rendering -----------------------------------------------------

    #[test]
    fn dry_run_render_lists_every_entry_and_redacts_secrets() {
        let mut plan = Plan::new();
        plan.push_step(
            Step::new("token-accounts", "install accounts.env", None, "cat > x".into()).with_stdin(
                StepStdin {
                    content: "SECRET_TOKEN_VALUE".into(),
                    secret: true,
                },
            ),
        );
        plan.push_skip("safehouse", "wire safehouse", "requires #3998");
        let rendered = plan.render_dry_run("worker-1");
        assert!(rendered.contains("worker-1"));
        assert!(rendered.contains("token-accounts"));
        assert!(rendered.contains("feeds a secret via stdin"));
        assert!(rendered.contains("SKIP: requires #3998"));
        // The secret value must never appear in the dry-run output.
        assert!(!rendered.contains("SECRET_TOKEN_VALUE"));
    }

    #[test]
    fn checklist_render_marks_each_status() {
        let reports = vec![
            StepReport {
                name: "a".into(),
                summary: "A".into(),
                status: StepStatus::AlreadyDone,
            },
            StepReport {
                name: "b".into(),
                summary: "B".into(),
                status: StepStatus::Changed,
            },
            StepReport {
                name: "c".into(),
                summary: "C".into(),
                status: StepStatus::Skipped("requires #3998".into()),
            },
            StepReport {
                name: "d".into(),
                summary: "D".into(),
                status: StepStatus::Failed("apply exited 1: boom".into()),
            },
        ];
        let out = render_checklist("worker-1", &reports);
        assert!(out.contains("(unchanged)"));
        assert!(out.contains("(changed)"));
        assert!(out.contains("(skipped) — requires #3998"));
        assert!(out.contains("(FAILED) — apply exited 1: boom"));
    }

    #[test]
    fn secret_stdin_debug_is_redacted() {
        let s = StepStdin {
            content: "hunter2".into(),
            secret: true,
        };
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"));
        assert!(dbg.contains("<redacted>"));
        // A non-secret payload is shown.
        let plain = StepStdin {
            content: "visible".into(),
            secret: false,
        };
        assert!(format!("{plain:?}").contains("visible"));
    }

    // ---- fleet registry ------------------------------------------------

    fn record(host: &str) -> WorkerRecord {
        WorkerRecord {
            ssh_host: host.to_string(),
            repos: vec!["rjwalters/anvil".to_string()],
            priority: 50,
            bootstrapped_at: "2026-07-28T00:00:00Z".to_string(),
            last_verify: None,
            provider_instance_id: None,
            tailnet_name: None,
            added_by: None,
            state: None,
            drain_phase: None,
            drain_captured: Vec::new(),
            idle_shutdown_minutes: None,
            last_seen_up_at: None,
        }
    }

    #[test]
    fn registry_load_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let reg = FleetRegistry::load(&path).unwrap();
        assert!(reg.workers.is_empty());
        assert_eq!(reg.version, FLEET_REGISTRY_VERSION);
    }

    #[test]
    fn registry_load_corrupt_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(FleetRegistry::load(&path).is_err());
    }

    #[test]
    fn registry_upsert_dedups_on_ssh_host() {
        let mut reg = FleetRegistry::default();
        assert!(!reg.upsert(record("worker-1")), "first insert is new");
        assert!(reg.upsert(record("worker-1")), "second is a replace");
        assert_eq!(reg.workers.len(), 1);

        assert!(!reg.upsert(record("worker-2")));
        assert_eq!(reg.workers.len(), 2);
    }

    #[test]
    fn registry_upsert_replaces_in_place() {
        let mut reg = FleetRegistry::default();
        reg.upsert(record("worker-1"));
        let mut updated = record("worker-1");
        updated.priority = 3;
        updated.last_verify = Some(VerifyResult {
            ok: true,
            at: "2026-07-28T01:00:00Z".to_string(),
            summary: "daemon running".to_string(),
        });
        reg.upsert(updated);
        assert_eq!(reg.workers.len(), 1);
        let got = reg.get("worker-1").unwrap();
        assert_eq!(got.priority, 3);
        assert!(got.last_verify.as_ref().unwrap().ok);
    }

    #[test]
    fn registry_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("fleet.json");
        let mut reg = FleetRegistry::default();
        reg.upsert(record("worker-1"));
        reg.save(&path).unwrap();
        assert!(path.exists());

        let loaded = FleetRegistry::load(&path).unwrap();
        assert_eq!(loaded, reg);
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn default_registry_path_honours_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom-fleet.json");
        std::env::set_var(FLEET_REGISTRY_PATH_ENV, &custom);
        let resolved = default_fleet_registry_path().unwrap();
        std::env::remove_var(FLEET_REGISTRY_PATH_ENV);
        assert_eq!(resolved, custom);
    }

    #[test]
    fn registry_legacy_record_without_priority_parses_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::fs::write(
            &path,
            r#"{ "version": 1, "workers": [ { "ssh_host": "w1", "bootstrapped_at": "t" } ] }"#,
        )
        .unwrap();
        let reg = FleetRegistry::load(&path).unwrap();
        assert_eq!(reg.workers.len(), 1);
        assert_eq!(reg.workers[0].priority, crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY);
        assert!(reg.workers[0].repos.is_empty());
        // #4342's extended fields must also be absent-tolerant on a
        // pre-#4342 record.
        assert!(reg.workers[0].provider_instance_id.is_none());
        assert!(reg.workers[0].tailnet_name.is_none());
        assert!(reg.workers[0].added_by.is_none());
        assert!(reg.workers[0].state.is_none());
    }

    #[test]
    fn registry_parses_4342_extended_fields_and_draining_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::fs::write(
            &path,
            r#"{ "version": 1, "workers": [ {
                "ssh_host": "repo-remote-anvil",
                "bootstrapped_at": "2026-07-29T00:00:00Z",
                "provider_instance_id": "i-0f35973f28ed5d97f",
                "tailnet_name": "loom-worker-1",
                "added_by": "operator",
                "state": "draining"
            } ] }"#,
        )
        .unwrap();
        let reg = FleetRegistry::load(&path).unwrap();
        let w = &reg.workers[0];
        assert_eq!(w.provider_instance_id.as_deref(), Some("i-0f35973f28ed5d97f"));
        assert_eq!(w.tailnet_name.as_deref(), Some("loom-worker-1"));
        assert_eq!(w.added_by.as_deref(), Some("operator"));
        assert_eq!(w.state.as_deref(), Some("draining"));
    }
}
