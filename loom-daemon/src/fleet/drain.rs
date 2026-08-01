//! `fleet drain <ssh-host>` — retire a worker without losing in-flight work,
//! forge claims, or (when wired) E2E room keys (issue #4343, epic #4340).
//!
//! # Not a new drain engine — SSH orchestration over an existing primitive
//!
//! Steps 1–2 of the body ("stop new dispatch", "let in-flight sweeps finish or
//! checkpoint, bounded wait, then cancel") already exist on the remote daemon:
//! `loom-daemon restart --drain` (#4090) stops admitting new work immediately
//! and waits bounded for in-flight sweeps, refusing (fail-safe) or
//! force-cancelling at the deadline. This module's job is the SSH fanout over
//! that primitive plus the deltas teardown needs:
//!
//! 1. **Drain-then-*exit*, not drain-then-restart.** `restart --drain` ends by
//!    *restarting* the daemon (exit [`crate::ipc::EXIT_RESTART`] under a
//!    supervisor, which relaunches it) — correct for #4090's roll use case,
//!    wrong for teardown: a relaunched daemon could pick up new dispatch
//!    before the box powers off, defeating the whole point of draining.
//!    `restart --drain --then-exit` (the CLI flag this issue adds) speaks the
//!    same `DrainAndRestartDaemon` request with `then_exit: true`, and the
//!    remote daemon exits `EXIT_SHUTDOWN` (143, no supervisor relaunch)
//!    instead once drained.
//! 2. **Immediate, targeted claim reset.** The startup-only
//!    `claim_reconciliation` pass is too late for a drain — this module
//!    captures the in-flight issue numbers from the worker's `status --json`
//!    *before* triggering the remote drain, then (after the remote daemon has
//!    exited) flips any of them still `loom:building` back to `loom:issue`
//!    via `gh` **from the orchestrator, not over SSH** — the forge is global,
//!    so no remote access is needed for this step.
//! 3. **Safehoused flush verification via a supervised stop (#3998).** A
//!    supervised `systemctl --user stop safehoused` IS the flush — see
//!    [`flush_safehouse`]'s doc comment.
//!
//! # Phase state machine (idempotent + resumable, body step 6)
//!
//! [`DrainPhase`] is a fixed, ordered sequence. Each phase's completion is
//! persisted onto the worker's [`super::WorkerRecord`] (`state: "draining"`
//! written **first**, for crash-safety — an interrupted drain must leave the
//! host visible in `fleet status`, not silently gone; the roster entry is
//! removed **last**, only once every earlier phase — including the safehouse
//! flush — has resolved). A re-run reads `drain_phase` and resumes after the
//! last completed one; every phase is individually idempotent (re-marking a
//! draining host draining, re-flipping an already-reset label, re-removing an
//! already-removed workspace are all no-ops).
//!
//! # Exit codes
//!
//! - `0` — fully verified drain: zero stranded claims, host deregistered,
//!   safe to power off.
//! - `1` — a phase failed outright (SSH/launch failure, remote refusal,
//!   unparseable output). The phase marker is preserved; re-run to retry.
//! - `2` — the remote drain timed out **without** `--force-after-timeout`
//!   (fail-safe refusal) — the remote daemon is still running and was never
//!   told to exit.
//! - `3` — every phase completed, but the safehouse flush could not be
//!   verified (see [`flush_safehouse`]) — teardown is **not** certified safe
//!   for room-key continuity even though the roster entry was removed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{
    default_fleet_registry_path, CommandOutput, CommandRunner, FleetRegistry, WorkerRecord,
};
use crate::fleet::add_worker::SshRunner;

/// Default bound on how long the remote drain waits for in-flight sweeps
/// (mirrors [`crate::ipc::DEFAULT_DRAIN_TIMEOUT_SECS`] — kept as an
/// independent constant so this module never has to depend on `ipc`'s tokio
/// runtime types).
pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 1800;

/// Default interval between polls of the remote host while waiting for it to
/// exit after a drain was triggered.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// Grace added on top of `timeout_secs` before the orchestrator gives up
/// waiting for the remote daemon to exit (the remote daemon's own supervisor
/// enforces `timeout_secs`; this buffer absorbs SSH round-trip + poll-interval
/// slack rather than racing it).
const WAIT_EXIT_GRACE_SECS: u64 = 60;

// ===========================================================================
// Phase model
// ===========================================================================

/// One ordered phase of `fleet drain`. Persisted on [`WorkerRecord::drain_phase`]
/// as [`DrainPhase::name`] after it completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DrainPhase {
    /// Mark the roster entry `state: "draining"` (first, for crash-safety).
    MarkDraining,
    /// Capture the worker's in-flight `loom:building` issue numbers before
    /// triggering the remote drain.
    CaptureClaims,
    /// Trigger the remote `restart --drain --then-exit`.
    TriggerRemoteDrain,
    /// Poll until the remote daemon has exited (or the wait deadline passes).
    WaitRemoteExit,
    /// Reset any captured claim still `loom:building` back to `loom:issue`.
    ResetClaims,
    /// Verify (or, absent a seam, honestly not-verify) the safehoused
    /// key-backup flush.
    FlushSafehouse,
    /// Deregister the worker's workspaces on the (now-stopped) remote host's
    /// registry — best-effort, since the remote daemon has already exited.
    DeregisterWorkspace,
    /// Remove the worker from the local fleet registry (last).
    RemoveFromRoster,
}

impl DrainPhase {
    /// The full ordered sequence.
    const ALL: [DrainPhase; 8] = [
        DrainPhase::MarkDraining,
        DrainPhase::CaptureClaims,
        DrainPhase::TriggerRemoteDrain,
        DrainPhase::WaitRemoteExit,
        DrainPhase::ResetClaims,
        DrainPhase::FlushSafehouse,
        DrainPhase::DeregisterWorkspace,
        DrainPhase::RemoveFromRoster,
    ];

    /// Stable machine name, persisted on the registry record.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            DrainPhase::MarkDraining => "mark-draining",
            DrainPhase::CaptureClaims => "capture-claims",
            DrainPhase::TriggerRemoteDrain => "trigger-remote-drain",
            DrainPhase::WaitRemoteExit => "wait-remote-exit",
            DrainPhase::ResetClaims => "reset-claims",
            DrainPhase::FlushSafehouse => "flush-safehouse",
            DrainPhase::DeregisterWorkspace => "deregister-workspace",
            DrainPhase::RemoveFromRoster => "remove-from-roster",
        }
    }

    /// Parse a persisted phase name back (`None` for an unrecognized/absent
    /// value — treated as "start from the beginning", never a hard error, so
    /// a registry written by a future binary still resumes safely).
    #[must_use]
    fn parse(name: Option<&str>) -> Option<DrainPhase> {
        DrainPhase::ALL.into_iter().find(|p| Some(p.name()) == name)
    }

    /// The phase immediately after `self`, or `None` if `self` is the last.
    #[must_use]
    fn next(self) -> Option<DrainPhase> {
        let idx = DrainPhase::ALL.iter().position(|p| *p == self)?;
        DrainPhase::ALL.get(idx + 1).copied()
    }

    /// The first phase to run, given the last-*completed* phase recorded on
    /// the registry (`None` ⇒ nothing completed yet ⇒ start at the first).
    #[must_use]
    fn resume_from(last_completed: Option<&str>) -> DrainPhase {
        match DrainPhase::parse(last_completed) {
            Some(p) => p.next().unwrap_or(DrainPhase::RemoveFromRoster),
            None => DrainPhase::ALL[0],
        }
    }
}

/// A captured `loom:building` claim, persisted so a crash between capture and
/// reset never loses the list (mirrors [`super::VerifyResult`]'s
/// persist-immediately shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedClaim {
    /// The issue number that was `loom:building` on this host at capture
    /// time.
    pub issue: u32,
    /// The forge repo slug (`owner/name`) the issue belongs to.
    pub repo: String,
}

// ===========================================================================
// Config + outcome
// ===========================================================================

/// Operator inputs for one `fleet drain` invocation.
#[derive(Debug, Clone)]
pub struct DrainConfig {
    /// SSH alias/host to drain.
    pub ssh_host: String,
    /// Bound on how long the remote drain waits for in-flight sweeps.
    pub timeout_secs: u64,
    /// On remote timeout, force-cancel stragglers (SIGTERM→grace→SIGKILL) and
    /// exit anyway. Without this, a timeout refuses and the remote daemon
    /// stays running (fail-safe) — this orchestrator then also refuses to
    /// proceed past `wait-remote-exit`.
    pub force_after_timeout: bool,
    /// Interval between polls while waiting for the remote daemon to exit.
    pub poll_interval: Duration,
    /// Hard cap on wait-remote-exit polls (a defensive bound independent of
    /// wall-clock, primarily so a test with a zero `poll_interval` cannot
    /// spin forever against an exhausted mock). Production callers should
    /// size this from `timeout_secs`/`poll_interval`; [`run`] does so.
    pub max_polls: u32,
    /// Whether `safehouse.enabled` resolved `true` for this invocation (via
    /// [`crate::safehouse::resolve_config`]) — threaded in as a plain `bool`
    /// so phase execution stays a pure function or drives off a mocked
    /// runner in tests, with no config-file I/O of its own.
    pub safehouse_enabled: bool,
    /// Emit machine-readable JSON instead of the human-readable report.
    pub json: bool,
}

/// Machine-readable per-phase outcome, mirroring [`super::StepStatus`]'s
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PhaseOutcome {
    /// Already completed on a prior run — skipped this run (resumability).
    AlreadyDone,
    /// Applied successfully this run.
    Changed,
    /// Deliberately skipped (e.g. safehouse disabled) with a reason.
    Skipped { reason: String },
    /// Completed, but with a caveat that prevents a clean "safe to power off"
    /// verdict (the unverified-safehoush-flush case).
    Unverified { reason: String },
    /// Failed; the phase marker is NOT advanced past this phase.
    Failed { reason: String },
}

impl PhaseOutcome {
    #[must_use]
    fn is_failure(&self) -> bool {
        matches!(self, PhaseOutcome::Failed { .. })
    }
}

/// One phase's report.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseReport {
    pub phase: &'static str,
    pub outcome: PhaseOutcome,
}

/// The full drain report: every phase executed this run, plus the terminal
/// verdict.
#[derive(Debug, Clone, Serialize)]
pub struct DrainReport {
    pub host: String,
    pub phases: Vec<PhaseReport>,
    /// `true` only when every phase completed cleanly (no `Failed`,
    /// `Unverified`, or unresolved timeout) — the "safe to power off" gate.
    pub safe_to_power_off: bool,
    /// A human-readable diagnostic when `safe_to_power_off` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caveat: Option<String>,
    /// The exact `repo:remote` teardown command to hand back to the operator
    /// (loom never calls a cloud CLI itself — epic #4340's boundary). Present
    /// once the drain has progressed far enough to be teardown-eligible
    /// (i.e. every phase through `deregister-workspace` completed or was
    /// skipped/already-done).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown_command: Option<String>,
}

impl DrainReport {
    /// Exit code policy (module doc): `0` fully verified, `1` generic
    /// failure, `2` fail-safe timeout refusal, `3` unverified safehouse
    /// flush.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.safe_to_power_off {
            return 0;
        }
        if self
            .phases
            .iter()
            .any(|p| matches!(p.outcome, PhaseOutcome::Unverified { .. }))
        {
            return 3;
        }
        if self.phases.iter().any(|p| {
            p.phase == DrainPhase::WaitRemoteExit.name()
                && matches!(&p.outcome, PhaseOutcome::Failed { reason } if reason.contains("timed out"))
        }) {
            return 2;
        }
        1
    }

    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("fleet drain report for {}:\n", self.host));
        if self.phases.is_empty() {
            out.push_str(
                "  host not found in the fleet registry — nothing to do (already drained/\n  \
                 removed, or never added; see `fleet status` for the current roster).\n",
            );
            return out;
        }
        for p in &self.phases {
            let (mark, detail) = match &p.outcome {
                PhaseOutcome::AlreadyDone => ("=", None),
                PhaseOutcome::Changed => ("+", None),
                PhaseOutcome::Skipped { reason } => ("-", Some(reason.clone())),
                PhaseOutcome::Unverified { reason } => ("!", Some(reason.clone())),
                PhaseOutcome::Failed { reason } => ("x", Some(reason.clone())),
            };
            out.push_str(&format!("  {mark} [{}]", p.phase));
            if let Some(d) = detail {
                out.push_str(&format!(" — {d}"));
            }
            out.push('\n');
        }
        if self.safe_to_power_off {
            out.push_str("\nDRAIN COMPLETE — safe to power off.\n");
        } else if let Some(caveat) = &self.caveat {
            out.push_str(&format!("\nDRAIN NOT VERIFIED SAFE: {caveat}\n"));
        } else {
            out.push_str("\nDRAIN INCOMPLETE.\n");
        }
        if let Some(cmd) = &self.teardown_command {
            out.push_str(&format!(
                "\nTeardown is repo:remote's job — loom never calls a cloud CLI itself. Run:\n  {cmd}\n"
            ));
        }
        out
    }
}

// ===========================================================================
// Claim reset seam (the `gh` / mock seam)
// ===========================================================================

/// The seam between the reset-claims phase and the forge: check whether an
/// issue still carries `loom:building`, and if so flip it back to
/// `loom:issue` with an explanatory comment. Mirrors
/// [`super::CommandRunner`]'s "real ssh vs scripted mock" shape.
pub trait ClaimResetter {
    /// Reset `issue` in `repo` if it still holds `loom:building`. Returns
    /// `Ok(true)` when the claim was actually flipped, `Ok(false)` when the
    /// issue no longer held it (a no-op — the sweep finished normally between
    /// capture and reset). `Err` only on a genuine `gh` failure.
    fn reset_claim(&self, repo: &str, issue: u32, host: &str) -> Result<bool>;
}

/// The production [`ClaimResetter`]: `gh issue view` to check the current
/// label set, then `gh issue edit` + `gh issue comment` to flip it — run
/// **locally** (never over SSH; the forge is global).
///
/// Each `gh` invocation is given an explicit `PATH` env (via
/// [`super::path_bootstrap::local_gh_path_env`]) rather than relying on this
/// process's inherited environment (#4831): a `loom-daemon` launched
/// non-interactively (launchd/systemd) may not have `gh`/Homebrew on its
/// inherited PATH even though an interactive login shell on the same host
/// would.
pub struct GhClaimResetter;

impl ClaimResetter for GhClaimResetter {
    fn reset_claim(&self, repo: &str, issue: u32, host: &str) -> Result<bool> {
        let gh_path = super::path_bootstrap::local_gh_path_env();
        let view = Command::new("gh")
            .env("PATH", &gh_path)
            .args([
                "issue",
                "view",
                &issue.to_string(),
                "--repo",
                repo,
                "--json",
                "labels",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("gh issue view #{issue} in {repo}"))?;
        if !view.status.success() {
            anyhow::bail!(
                "gh issue view #{issue} in {repo} failed: {}",
                String::from_utf8_lossy(&view.stderr).trim()
            );
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&view.stdout).context("parsing gh issue view --json labels")?;
        let has_building = parsed["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|l| l["name"] == "loom:building"));
        if !has_building {
            return Ok(false);
        }

        let edit = Command::new("gh")
            .env("PATH", &gh_path)
            .args([
                "issue",
                "edit",
                &issue.to_string(),
                "--repo",
                repo,
                "--remove-label",
                "loom:building",
                "--add-label",
                "loom:issue",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("gh issue edit #{issue} in {repo}"))?;
        if !edit.status.success() {
            anyhow::bail!(
                "gh issue edit #{issue} in {repo} failed: {}",
                String::from_utf8_lossy(&edit.stderr).trim()
            );
        }

        // Best-effort comment — never fails the reset itself (mirrors the
        // rest of Loom's "a forge comment is advisory" posture).
        let _ = Command::new("gh")
            .env("PATH", &gh_path)
            .args([
                "issue",
                "comment",
                &issue.to_string(),
                "--repo",
                repo,
                "--body",
                &format!(
                    "🔧 **fleet drain**: host `{host}` was drained/retired while this issue was \
                     claimed; `loom:building` reset to `loom:issue` so it is not stranded (see \
                     epic #4340, #4343)."
                ),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();

        Ok(true)
    }
}

// ===========================================================================
// Remote status parsing
// ===========================================================================

/// Whether a remote `status --json` payload (given as raw stdout) reports the
/// daemon still draining — used by `wait-remote-exit` to distinguish "still
/// waiting" from "drain was refused and dispatch resumed". Thin string-level
/// wrapper over [`still_draining`]; the polling path parses once and calls
/// [`classify_remote_exit`] instead.
#[cfg(test)]
#[must_use]
fn parse_still_draining(stdout: &str) -> Option<bool> {
    let value = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    still_draining(&value)
}

/// Read the drain flag out of an already-parsed `status --json` payload.
///
/// The real payload **nests** this under `drain` — `build_status_json_value`
/// in `main.rs` emits `"drain": { "draining": …, "deadline": …, "note": … }`
/// (#4090) — so a top-level-only read always returns `None` against a live
/// daemon and silently disables the refusal branch in [`wait_remote_exit`].
/// The top-level fallback keeps a hypothetical flatter/older payload legible
/// rather than making the parse brittle.
#[must_use]
fn still_draining(value: &serde_json::Value) -> Option<bool> {
    value
        .get("drain")
        .and_then(|drain| drain.get("draining"))
        .or_else(|| value.get("draining"))
        .and_then(serde_json::Value::as_bool)
}

/// One `wait-remote-exit` poll's verdict about the remote daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteExitProbe {
    /// The remote daemon is gone — the expected outcome of a `then_exit`
    /// drain, whether the *host* is still up (the normal case) or also gone.
    Exited,
    /// Still draining, or the payload is not (yet) legible — keep polling.
    StillGoing,
    /// The daemon is reachable and reports `draining: false` — the drain was
    /// refused/aborted and dispatch resumed. Fail loudly.
    Refused,
}

/// Classify one remote `loom-daemon status --json` invocation.
///
/// Three shapes matter, and only the first used to be handled:
///
/// 1. **Empty stdout + non-zero exit** — the SSH transport itself failed
///    (connection refused, exit 255): host gone ⇒ `Exited`.
/// 2. **A payload with a top-level `error` key** — the #4069
///    unreachable-daemon payload (`print_status_unreachable_json` in
///    `main.rs`) `println!`s `{"error": "could not reach loom-daemon at …",
///    "install_state": …}` to **stdout** and exits non-zero
///    (`install_state.exit_code()`). This is the normal post-`then_exit`
///    state (host up, daemon down) and is therefore the primary success
///    signal, not a parse miss. Mirrors
///    [`super::status::classify_status_output`]'s `DaemonDown` arm.
/// 3. **A live status payload** — inspect `drain.draining` to tell "still
///    draining" from "refused and dispatching again".
#[must_use]
fn classify_remote_exit(out: &CommandOutput) -> RemoteExitProbe {
    if out.stdout.trim().is_empty() {
        return if out.ok() {
            // Reachable but silent — nothing to conclude yet.
            RemoteExitProbe::StillGoing
        } else {
            RemoteExitProbe::Exited
        };
    }
    match serde_json::from_str::<serde_json::Value>(&out.stdout) {
        Ok(value) if value.get("error").is_some() => RemoteExitProbe::Exited,
        Ok(value) => match still_draining(&value) {
            Some(false) => RemoteExitProbe::Refused,
            // `Some(true)` (still draining) or `None` (a payload without the
            // field at all) ⇒ keep polling.
            _ => RemoteExitProbe::StillGoing,
        },
        Err(_) => RemoteExitProbe::StillGoing,
    }
}

/// Best-effort mapping from a captured in-flight sweep's workspace-root path
/// (`SweepInfo.repo`) to one of the worker's registered forge slugs, by
/// comparing the root's final path segment to each slug's clone-directory
/// name (`owner/name` -> `name`, the same convention `fleet add-worker` uses).
/// Falls back to the worker's first registered repo when the mapping is
/// ambiguous (absent `repo` field, or no matching segment) — a host with
/// exactly one registered repo (the overwhelmingly common case) is always
/// unambiguous.
#[must_use]
fn resolve_repo_for_sweep(
    sweep_repo_root: Option<&str>,
    worker_repos: &[String],
) -> Option<String> {
    if worker_repos.is_empty() {
        return None;
    }
    if worker_repos.len() == 1 {
        return Some(worker_repos[0].clone());
    }
    if let Some(root) = sweep_repo_root {
        let leaf = root.rsplit('/').next().unwrap_or(root);
        for repo in worker_repos {
            if repo.rsplit('/').next() == Some(leaf) {
                return Some(repo.clone());
            }
        }
    }
    // Ambiguous — best-effort fallback, documented in the module doc.
    Some(worker_repos[0].clone())
}

/// Parse `(issue, repo)` pairs directly from a remote `status --json` payload
/// (used by `capture-claims`). Leniently parsed as a raw [`serde_json::Value`]
/// (mirrors [`super::status::classify_status_output`]'s lenient posture — an
/// older/newer remote binary's reduced/extended field set must never fail this
/// parse, only genuinely non-JSON output does). `SweepKind` serializes as
/// `{"type":"Issue","value":<n>}` / `{"type":"PrSet","value":[..]}` — a
/// `PrSet` sweep contributes every member issue (Mode C sweeps are rare and
/// reserved for future phases, but a drain must not silently drop them if one
/// is ever in flight). Also carries `SweepInfo.repo` (the workspace-root
/// string), when present, into [`resolve_repo_for_sweep`].
#[must_use]
fn parse_captured_claims(stdout: &str, worker_repos: &[String]) -> Vec<CapturedClaim> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return Vec::new();
    };
    let Some(in_flight) = value.get("in_flight").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut claims = Vec::new();
    for sweep in in_flight {
        let sweep_repo_root = sweep.get("repo").and_then(serde_json::Value::as_str);
        let Some(repo) = resolve_repo_for_sweep(sweep_repo_root, worker_repos) else {
            continue;
        };
        let kind = &sweep["kind"];
        match kind["type"].as_str() {
            Some("Issue") => {
                if let Some(n) = kind["value"].as_u64() {
                    claims.push(CapturedClaim {
                        issue: n as u32,
                        repo: repo.clone(),
                    });
                }
            }
            Some("PrSet") => {
                if let Some(arr) = kind["value"].as_array() {
                    for v in arr {
                        if let Some(n) = v.as_u64() {
                            claims.push(CapturedClaim {
                                issue: n as u32,
                                repo: repo.clone(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    claims
}

// ===========================================================================
// Phase execution (pure-ish: driven entirely off the two seams + the record)
// ===========================================================================

/// Derive the `%h`-relative workspace path `fleet add-worker` clones a repo
/// slug into (mirrors `add_worker::workspace_rel`, reimplemented here rather
/// than widening that module's visibility for a one-line helper).
#[must_use]
fn workspace_path_for_repo(repo: &str) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    format!("$HOME/loom-workspaces/{name}")
}

/// Execute every phase from [`DrainPhase::resume_from`] onward against
/// `record`, using `runner` for every remote (SSH) action and `claims` for
/// the local forge claim reset. Mutates `record` in place (phase marker,
/// captured claims) and calls `persist` after **each** phase so a crash mid-run
/// never loses more than the phase in progress. Returns the ordered
/// [`PhaseReport`]s for this run and whether the record should be removed
/// from the registry (only `true` once `RemoveFromRoster` itself completes).
pub fn execute_drain(
    runner: &dyn CommandRunner,
    claims: &dyn ClaimResetter,
    record: &mut WorkerRecord,
    config: &DrainConfig,
    mut persist: impl FnMut(&WorkerRecord) -> Result<()>,
) -> Result<(Vec<PhaseReport>, bool)> {
    let start = DrainPhase::resume_from(record.drain_phase.as_deref());
    let mut reports = Vec::new();
    let mut remove_from_roster = false;

    // Every phase strictly before `start` is implicitly AlreadyDone (resumed
    // past it on a prior run) — reported for a complete, honest checklist.
    for p in DrainPhase::ALL {
        if p < start {
            reports.push(PhaseReport {
                phase: p.name(),
                outcome: PhaseOutcome::AlreadyDone,
            });
        }
    }

    let mut phase = Some(start);
    while let Some(p) = phase {
        let outcome = run_phase(p, runner, claims, record, config);
        let failed = outcome.is_failure();
        let is_remove = p == DrainPhase::RemoveFromRoster && !failed;
        reports.push(PhaseReport {
            phase: p.name(),
            outcome,
        });

        if failed {
            // Do NOT advance drain_phase past a failed phase — the marker
            // (already persisted by the previous successful phase) is the
            // resume point.
            break;
        }

        if is_remove {
            remove_from_roster = true;
            // The record is about to be deleted from the registry by the
            // caller — nothing left to persist onto it.
        } else {
            record.drain_phase = Some(p.name().to_string());
            persist(record)?;
        }

        phase = p.next();
    }

    Ok((reports, remove_from_roster))
}

fn run_phase(
    phase: DrainPhase,
    runner: &dyn CommandRunner,
    claims: &dyn ClaimResetter,
    record: &mut WorkerRecord,
    config: &DrainConfig,
) -> PhaseOutcome {
    match phase {
        DrainPhase::MarkDraining => {
            record.state = Some("draining".to_string());
            PhaseOutcome::Changed
        }

        DrainPhase::CaptureClaims => match runner.run("loom-daemon status --json", None) {
            Ok(out) if out.ok() => {
                let captured = parse_captured_claims(&out.stdout, &record.repos);
                record.drain_captured = captured;
                PhaseOutcome::Changed
            }
            Ok(out) => PhaseOutcome::Failed {
                reason: format!(
                    "remote `status --json` exited {}: {}",
                    out.code,
                    tail(&out.stderr)
                ),
            },
            Err(e) => PhaseOutcome::Failed {
                reason: format!("could not reach {}: {e}", config.ssh_host),
            },
        },

        DrainPhase::TriggerRemoteDrain => {
            let mut shell = format!(
                "loom-daemon restart --drain --then-exit --timeout {}",
                config.timeout_secs
            );
            if config.force_after_timeout {
                shell.push_str(" --force-after-timeout");
            }
            match runner.run(&shell, None) {
                Ok(out) if out.ok() => PhaseOutcome::Changed,
                Ok(out) => PhaseOutcome::Failed {
                    reason: format!(
                        "remote drain trigger refused (exit {}): {}",
                        out.code,
                        tail(&out.stderr)
                    ),
                },
                Err(e) => PhaseOutcome::Failed {
                    reason: format!("could not reach {}: {e}", config.ssh_host),
                },
            }
        }

        DrainPhase::WaitRemoteExit => wait_remote_exit(runner, config),

        DrainPhase::ResetClaims => {
            let mut failures = Vec::new();
            let mut reset_count = 0usize;
            for claim in &record.drain_captured {
                match claims.reset_claim(&claim.repo, claim.issue, &config.ssh_host) {
                    Ok(true) => reset_count += 1,
                    Ok(false) => {}
                    Err(e) => failures.push(format!("#{} in {}: {e}", claim.issue, claim.repo)),
                }
            }
            if failures.is_empty() {
                PhaseOutcome::Changed
            } else {
                PhaseOutcome::Failed {
                    reason: format!(
                        "{} claim(s) reset before failure; could not reset: {}",
                        reset_count,
                        failures.join("; ")
                    ),
                }
            }
        }

        DrainPhase::FlushSafehouse => flush_safehouse(config, runner),

        DrainPhase::DeregisterWorkspace => {
            let mut failures = Vec::new();
            for repo in &record.repos {
                let shell =
                    format!("loom-daemon workspace remove \"{}\"", workspace_path_for_repo(repo));
                match runner.run(&shell, None) {
                    // The remote daemon has already exited by this phase — a
                    // connection failure here is *expected*, not an error:
                    // `workspace remove` is a filesystem-registry edit that
                    // works whether or not the daemon is running, but we
                    // cannot SSH-execute it once the host itself is about to
                    // be torn down. Best-effort: log and move on.
                    Ok(out) if out.ok() => {}
                    Ok(_) | Err(_) => {
                        failures.push(repo.clone());
                    }
                }
            }
            if failures.is_empty() {
                PhaseOutcome::Changed
            } else {
                // Best-effort, never blocks teardown: the host is being
                // retired anyway, so a stale workspace-registry entry on a
                // box about to be powered off is cosmetic, not a correctness
                // issue. Recorded as Skipped (not Failed) so it never blocks
                // the roster-removal phase.
                PhaseOutcome::Skipped {
                    reason: format!(
                        "could not deregister workspace(s) on {} (host likely already \
                         unreachable post-drain, which is expected): {}",
                        config.ssh_host,
                        failures.join(", ")
                    ),
                }
            }
        }

        DrainPhase::RemoveFromRoster => PhaseOutcome::Changed,
    }
}

/// `wait-remote-exit`: poll `loom-daemon status --json` on the target until
/// either (a) the daemon is gone — the connection itself fails, *or* the
/// remote answers with the #4069 unreachable-daemon payload (`{"error": …}`
/// on stdout, non-zero exit), which is the normal "host up, daemon exited"
/// end state of a `then_exit` drain — i.e. success — or (b) the payload
/// parses and reports `drain.draining: false` while still reachable — the
/// remote daemon refused the drain (timed out without
/// `--force-after-timeout`) and is still running; fail loudly rather than
/// silently declaring success — or (c) [`DrainConfig::max_polls`] is
/// exhausted, a defensive bound independent of wall-clock so a test double
/// can never spin forever.
///
/// **Known limitation**: a transient SSH failure mid-wait (network blip,
/// exit 255) is indistinguishable from "the host went away" and is therefore
/// read as a successful exit. This is inherent to SSH-based liveness probing;
/// the cost of a false positive is bounded — the subsequent phases are
/// best-effort/orchestrator-side, and a still-running remote daemon would be
/// caught by the next `fleet status`.
fn wait_remote_exit(runner: &dyn CommandRunner, config: &DrainConfig) -> PhaseOutcome {
    let deadline = Instant::now() + Duration::from_secs(config.timeout_secs + WAIT_EXIT_GRACE_SECS);
    for attempt in 0..config.max_polls.max(1) {
        match runner.run("loom-daemon status --json", None) {
            Err(_) => return PhaseOutcome::Changed, // unreachable ⇒ exited ⇒ success
            Ok(out) => match classify_remote_exit(&out) {
                RemoteExitProbe::Exited => return PhaseOutcome::Changed,
                RemoteExitProbe::StillGoing => { /* keep polling */ }
                RemoteExitProbe::Refused => {
                    return PhaseOutcome::Failed {
                        reason: format!(
                            "remote drain timed out and was refused (no --force-after-timeout, or \
                             the daemon aborted it) — {} is still up and dispatching. It can also \
                             mean version skew: a pre-#4343 remote daemon ignores `then_exit` and \
                             drains-then-restarts. Re-run `fleet drain --force-after-timeout`, \
                             update the remote daemon, or investigate the stragglers.",
                            config.ssh_host
                        ),
                    };
                }
            },
        }
        if Instant::now() >= deadline || attempt + 1 >= config.max_polls {
            break;
        }
        if !config.poll_interval.is_zero() {
            std::thread::sleep(config.poll_interval);
        }
    }
    PhaseOutcome::Failed {
        reason: format!(
            "timed out after {}s waiting for {} to exit post-drain (still reachable)",
            config.timeout_secs + WAIT_EXIT_GRACE_SECS,
            config.ssh_host
        ),
    }
}

/// Safehoused key-backup flush verification (body step 3 / AC 2, #3998).
///
/// `safehoused` has no clean-exit restart primitive of its own — a supervised
/// **stop IS the flush**: safehoused's SIGTERM/ctrl-c shutdown path calls
/// `client.encryption().backups().wait_for_steady_state()` and prints
/// `"safehoused: room-key backup flushed; bye"` before exiting (confirmed on
/// the safehouse repo's `main`, `safehoused/src/main.rs`). So this phase, over
/// `runner`:
///
/// 1. `systemctl --user stop safehoused` (idempotent — a no-op, exit 0, on an
///    already-stopped unit, so a drain of a host where safehoused is already
///    down verifies via unit state rather than hanging).
/// 2. Polls `systemctl --user is-active safehoused` briefly for the stop to
///    settle (bounded — never spins forever).
/// 3. Verifies via the journal's flush line, falling back to the unit's
///    `ExecMainStatus` (ordinarily a stale/rotated journal case) when the
///    line is absent but the unit exited cleanly.
///
/// - `safehouse.enabled == false` (the default): [`PhaseOutcome::Skipped`] —
///   no room keys are in play on this host; "safe to power off" is accurate
///   without touching the host.
/// - `safehouse.enabled == true` and the flush verifies (remote exit 0):
///   [`PhaseOutcome::Changed`] — eligible for "safe to power off" (AC:
///   verified ⇒ exit 0).
/// - `safehouse.enabled == true` and the flush does **not** verify (remote
///   nonzero exit, or the host could not be reached): [`PhaseOutcome::Unverified`]
///   — the drain still completes (workspace deregistration + roster removal
///   proceed; loom never *refuses* to retire a box over this), but the final
///   verdict explicitly withholds "safe to power off" and exits non-zero
///   (`3`) so an operator/monitor treats it as a flag, not a clean success.
#[must_use]
fn flush_safehouse(config: &DrainConfig, runner: &dyn CommandRunner) -> PhaseOutcome {
    if !config.safehouse_enabled {
        return PhaseOutcome::Skipped {
            reason: "safehouse.enabled is false — no room keys in play on this host".to_string(),
        };
    }
    match runner.run(FLUSH_SAFEHOUSE_SHELL, None) {
        Ok(out) if out.ok() => PhaseOutcome::Changed,
        Ok(out) => PhaseOutcome::Unverified {
            reason: format!(
                "safehoused key-backup flush could not be verified on {} (exit {}): {} — \
                 teardown proceeds (workspace/roster cleanup complete), but is NOT certified \
                 safe for room-key continuity.",
                config.ssh_host,
                out.code,
                tail(&out.stderr)
            ),
        },
        Err(e) => PhaseOutcome::Unverified {
            reason: format!(
                "could not reach {} to stop safehoused and verify the key-backup flush: {e} — \
                 teardown proceeds (workspace/roster cleanup complete), but is NOT certified \
                 safe for room-key continuity.",
                config.ssh_host
            ),
        },
    }
}

/// Remote shell for [`flush_safehouse`]: stop the `safehoused` unit (its
/// SIGTERM path IS the flush) and verify. Exits `0` when the flush is
/// verified (journal line, or a clean `ExecMainStatus` when the unit is
/// inactive), non-zero otherwise. Bounded polling (`30` × `1s` max) so an
/// already-stopped unit — or one that never comes down — never hangs the
/// drain.
const FLUSH_SAFEHOUSE_SHELL: &str = r#"set +e
systemctl --user stop safehoused >/dev/null 2>&1
i=0
while [ "$i" -lt 30 ]; do
  state="$(systemctl --user is-active safehoused 2>/dev/null)"
  [ "$state" = "active" ] || break
  sleep 1
  i=$((i + 1))
done
if journalctl --user -u safehoused --no-pager -n 200 2>/dev/null | grep -q "room-key backup flushed"; then
  exit 0
fi
exit_status="$(systemctl --user show safehoused -p ExecMainStatus --value 2>/dev/null)"
state="$(systemctl --user is-active safehoused 2>/dev/null)"
if [ "$exit_status" = "0" ] && [ "$state" != "active" ]; then
  exit 0
fi
echo "safehoused flush not verified (state=$state exec_main_status=$exit_status)" >&2
exit 1
"#;

fn tail(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

// ===========================================================================
// Top-level `run` (real I/O: registry load/save, SSH, `gh`)
// ===========================================================================

/// `loom-daemon fleet drain <ssh-host>` entry point. Loads the fleet registry,
/// resolves the resume point, executes every remaining phase over a real
/// [`SshRunner`] / [`GhClaimResetter`], persisting after each phase, and
/// prints the report.
///
/// A host absent from the registry is a clean no-op (exit 0): either it was
/// never added, or a prior `fleet drain` already completed and removed it —
/// both cases mean "nothing to do", not an error (Test Plan edge case:
/// "drain re-run on an already-drained host").
pub fn run(config: &DrainConfig) -> Result<DrainReport> {
    let path = default_fleet_registry_path()?;
    let registry = FleetRegistry::load(&path)?;

    let Some(idx) = registry
        .workers
        .iter()
        .position(|w| w.ssh_host == config.ssh_host)
    else {
        return Ok(DrainReport {
            host: config.ssh_host.clone(),
            phases: Vec::new(),
            safe_to_power_off: true,
            caveat: None,
            teardown_command: None,
        });
    };

    let mut record = registry.workers[idx].clone();
    let runner = SshRunner::new(&config.ssh_host);
    let claims = GhClaimResetter;

    let registry_path = path.clone();
    let (reports, removed) = execute_drain(&runner, &claims, &mut record, config, |rec| {
        let mut reg = FleetRegistry::load(&registry_path)?;
        reg.upsert(rec.clone());
        reg.save(&registry_path)
    })?;

    if removed {
        let mut reg = FleetRegistry::load(&path)?;
        reg.workers.retain(|w| w.ssh_host != config.ssh_host);
        reg.save(&path)?;
    } else {
        // Persist the final (possibly-failed) state too, so a failed run's
        // `drain_captured`/`drain_phase` are on disk for the next resume even
        // if the last phase's own `persist` call inside `execute_drain` was
        // never reached (a failure never calls it, by design).
        let mut reg = FleetRegistry::load(&path)?;
        reg.upsert(record.clone());
        reg.save(&path)?;
    }

    Ok(build_report(config, reports))
}

/// Turn a phase-execution run into the final [`DrainReport`] verdict.
#[must_use]
fn build_report(config: &DrainConfig, phases: Vec<PhaseReport>) -> DrainReport {
    let any_failed = phases.iter().any(|p| p.outcome.is_failure());
    let any_unverified = phases
        .iter()
        .any(|p| matches!(p.outcome, PhaseOutcome::Unverified { .. }));
    let removed = phases
        .iter()
        .any(|p| p.phase == DrainPhase::RemoveFromRoster.name() && !p.outcome.is_failure());

    let safe_to_power_off = removed && !any_failed && !any_unverified;

    let caveat = if any_unverified {
        phases.iter().find_map(|p| match &p.outcome {
            PhaseOutcome::Unverified { reason } => Some(reason.clone()),
            _ => None,
        })
    } else if any_failed {
        phases.iter().find_map(|p| match &p.outcome {
            PhaseOutcome::Failed { reason } => Some(format!("{}: {reason}", p.phase)),
            _ => None,
        })
    } else {
        None
    };

    let teardown_command = if removed
        || (!any_failed
            && phases
                .iter()
                .any(|p| p.phase == DrainPhase::DeregisterWorkspace.name()))
    {
        Some(format!("repo:remote --down {}", config.ssh_host))
    } else {
        None
    };

    DrainReport {
        host: config.ssh_host.clone(),
        phases,
        safe_to_power_off,
        caveat,
        teardown_command,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::CommandOutput;
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ---- Mocks -------------------------------------------------------

    /// Ordered-queue mock runner (mirrors `add_worker`'s `MockRunner`): each
    /// `run` pops the next canned response, recording the shell it was asked
    /// to run.
    struct MockRunner {
        responses: RefCell<Vec<Result<CommandOutput, String>>>,
        calls: RefCell<Vec<String>>,
    }

    impl MockRunner {
        fn new(responses: Vec<Result<CommandOutput, String>>) -> Self {
            Self {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, shell: &str, _stdin: Option<&str>) -> Result<CommandOutput> {
            self.calls.borrow_mut().push(shell.to_string());
            let mut r = self.responses.borrow_mut();
            if r.is_empty() {
                return Ok(ok_status(false));
            }
            match r.remove(0) {
                Ok(out) => Ok(out),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    /// A live `status --json` payload. `draining` is nested under `drain`,
    /// exactly as `build_status_json_value` (`main.rs`) emits it — a flat
    /// top-level `draining` is *not* a shape any real daemon produces.
    fn ok_status(draining: bool) -> CommandOutput {
        ok_with_in_flight(&[], draining)
    }

    fn ok_with_in_flight(issues: &[u32], draining: bool) -> CommandOutput {
        let sweeps: Vec<String> = issues
            .iter()
            .map(|n| format!(r#"{{"kind": {{"type": "Issue", "value": {n}}}, "repo": null}}"#))
            .collect();
        CommandOutput {
            code: 0,
            stdout: format!(
                r#"{{"in_flight": [{}], "drain": {{"draining": {draining}, "deadline": null, "note": null}}}}"#,
                sweeps.join(",")
            ),
            stderr: String::new(),
        }
    }

    /// The #4069 unreachable-daemon payload: the *host* answered over SSH,
    /// but its `loom-daemon` is gone, so the remote CLI prints this JSON to
    /// **stdout** and exits non-zero (`install_state.exit_code()`). This —
    /// not an SSH error — is the normal post-`then_exit` state.
    fn daemon_down_host_up() -> CommandOutput {
        CommandOutput {
            code: 1,
            stdout: r#"{
  "error": "could not reach loom-daemon at /Users/w/.loom/daemon.sock: Connection refused (os error 61)",
  "install_state": {
    "state": "not_running",
    "started_at": null,
    "pid": null
  }
}"#
            .to_string(),
            stderr: String::new(),
        }
    }

    fn refused() -> CommandOutput {
        CommandOutput {
            code: 1,
            stdout: String::new(),
            stderr: "refusing to drain: no supervisor detected".to_string(),
        }
    }

    /// Records every reset call; `outcomes` scripts the per-`(issue)` result.
    struct MockClaimResetter {
        outcomes: Mutex<HashMap<u32, Result<bool, String>>>,
        calls: Mutex<Vec<(String, u32, String)>>,
    }

    impl MockClaimResetter {
        fn new(outcomes: HashMap<u32, Result<bool, String>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl ClaimResetter for MockClaimResetter {
        fn reset_claim(&self, repo: &str, issue: u32, host: &str) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push((repo.to_string(), issue, host.to_string()));
            match self.outcomes.lock().unwrap().remove(&issue) {
                Some(Ok(b)) => Ok(b),
                Some(Err(e)) => Err(anyhow::anyhow!(e)),
                None => Ok(false),
            }
        }
    }

    fn worker(host: &str) -> WorkerRecord {
        WorkerRecord {
            ssh_host: host.to_string(),
            repos: vec!["rjwalters/anvil".to_string()],
            priority: 100,
            bootstrapped_at: "2026-07-29T00:00:00Z".to_string(),
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

    fn base_config(host: &str) -> DrainConfig {
        DrainConfig {
            ssh_host: host.to_string(),
            timeout_secs: 60,
            force_after_timeout: false,
            poll_interval: Duration::from_millis(0),
            max_polls: 3,
            safehouse_enabled: false,
            json: false,
        }
    }

    // ---- DrainPhase ----------------------------------------------------

    #[test]
    fn resume_from_none_starts_at_first_phase() {
        assert_eq!(DrainPhase::resume_from(None), DrainPhase::MarkDraining);
    }

    #[test]
    fn resume_from_last_completed_starts_at_next() {
        assert_eq!(DrainPhase::resume_from(Some("mark-draining")), DrainPhase::CaptureClaims);
        assert_eq!(
            DrainPhase::resume_from(Some("flush-safehouse")),
            DrainPhase::DeregisterWorkspace
        );
    }

    #[test]
    fn resume_from_unrecognized_starts_over_rather_than_erroring() {
        assert_eq!(DrainPhase::resume_from(Some("some-future-phase")), DrainPhase::MarkDraining);
    }

    #[test]
    fn resume_from_final_phase_stays_at_final() {
        assert_eq!(
            DrainPhase::resume_from(Some("remove-from-roster")),
            DrainPhase::RemoveFromRoster
        );
    }

    // ---- parse helpers ---------------------------------------------------

    #[test]
    fn parse_captured_claims_reads_issue_and_prset_kinds() {
        let json = r#"{"in_flight": [
            {"kind": {"type": "Issue", "value": 42}},
            {"kind": {"type": "PrSet", "value": [7, 9]}}
        ]}"#;
        let repos = vec!["rjwalters/anvil".to_string()];
        let mut got: Vec<u32> = parse_captured_claims(json, &repos)
            .into_iter()
            .map(|c| c.issue)
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec![7, 9, 42]);
    }

    #[test]
    fn parse_captured_claims_tolerates_garbage() {
        let repos = vec!["rjwalters/anvil".to_string()];
        assert!(parse_captured_claims("not json", &repos).is_empty());
        assert!(parse_captured_claims(r#"{"no_in_flight_key": true}"#, &repos).is_empty());
    }

    #[test]
    fn parse_still_draining_reads_the_nested_drain_object() {
        // The real shape emitted by `build_status_json_value`.
        assert_eq!(
            parse_still_draining(r#"{"drain": {"draining": true, "deadline": null}}"#),
            Some(true)
        );
        assert_eq!(
            parse_still_draining(r#"{"drain": {"draining": false, "note": "timeout refusal"}}"#),
            Some(false)
        );
        // Lenient top-level fallback.
        assert_eq!(parse_still_draining(r#"{"draining": true}"#), Some(true));
        assert_eq!(parse_still_draining(r#"{"draining": false}"#), Some(false));
        assert_eq!(parse_still_draining(r#"{"other": 1}"#), None);
        assert_eq!(parse_still_draining(r#"{"drain": {"deadline": null}}"#), None);
        assert_eq!(parse_still_draining("garbage"), None);
    }

    #[test]
    fn parse_still_draining_reads_a_full_status_payload() {
        // Guards against the top-level-read regression: a realistic payload
        // must yield `Some(false)`, not `None`, so the refusal branch fires.
        assert_eq!(parse_still_draining(&ok_status(false).stdout), Some(false));
        assert_eq!(parse_still_draining(&ok_status(true).stdout), Some(true));
    }

    #[test]
    fn classify_remote_exit_maps_every_real_shape() {
        // Daemon down, host still up (#4069 payload on stdout, non-zero) —
        // the primary `then_exit` success signal.
        assert_eq!(classify_remote_exit(&daemon_down_host_up()), RemoteExitProbe::Exited);
        // SSH-level failure (connection refused / host gone).
        assert_eq!(
            classify_remote_exit(&CommandOutput {
                code: 255,
                stdout: String::new(),
                stderr: "ssh: connect to host worker-1 port 22: Connection refused".to_string(),
            }),
            RemoteExitProbe::Exited
        );
        // Still draining.
        assert_eq!(classify_remote_exit(&ok_status(true)), RemoteExitProbe::StillGoing);
        // Reachable and no longer draining ⇒ refused.
        assert_eq!(classify_remote_exit(&ok_status(false)), RemoteExitProbe::Refused);
        // Unparseable / silent output is never a verdict.
        assert_eq!(
            classify_remote_exit(&CommandOutput {
                code: 0,
                stdout: "not json".to_string(),
                stderr: String::new(),
            }),
            RemoteExitProbe::StillGoing
        );
        assert_eq!(
            classify_remote_exit(&CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            RemoteExitProbe::StillGoing
        );
    }

    #[test]
    fn resolve_repo_for_sweep_unambiguous_with_one_repo() {
        let repos = vec!["rjwalters/anvil".to_string()];
        assert_eq!(resolve_repo_for_sweep(None, &repos), Some("rjwalters/anvil".to_string()));
    }

    #[test]
    fn resolve_repo_for_sweep_matches_leaf_segment_with_multiple_repos() {
        let repos = vec!["rjwalters/anvil".to_string(), "rjwalters/loom".to_string()];
        assert_eq!(
            resolve_repo_for_sweep(Some("/home/w/loom-workspaces/loom"), &repos),
            Some("rjwalters/loom".to_string())
        );
        assert_eq!(
            resolve_repo_for_sweep(Some("/home/w/loom-workspaces/anvil"), &repos),
            Some("rjwalters/anvil".to_string())
        );
    }

    #[test]
    fn resolve_repo_for_sweep_falls_back_to_first_when_ambiguous() {
        let repos = vec!["rjwalters/anvil".to_string(), "rjwalters/loom".to_string()];
        assert_eq!(resolve_repo_for_sweep(None, &repos), Some("rjwalters/anvil".to_string()));
    }

    // ---- full happy-path phase sequence -----------------------------------

    #[test]
    fn happy_path_drains_captures_resets_and_removes_from_roster() {
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[42], true)), // capture-claims
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // trigger
            // wait-remote-exit: the *realistic* post-`then_exit` state — the
            // host is still up (it is only powered off later by
            // `repo:remote`), only its daemon is gone.
            Ok(daemon_down_host_up()),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // deregister-workspace
        ]);
        let mut outcomes = HashMap::new();
        outcomes.insert(42, Ok(true));
        let claims = MockClaimResetter::new(outcomes);

        let mut record = worker("worker-1");
        let config = base_config("worker-1");
        let mut saved = Vec::new();
        let (reports, removed) = execute_drain(&runner, &claims, &mut record, &config, |rec| {
            saved.push(rec.clone());
            Ok(())
        })
        .unwrap();

        assert!(removed, "the record should be marked for roster removal");
        assert!(!reports.iter().any(|r| r.outcome.is_failure()), "reports: {reports:?}");
        assert_eq!(reports.len(), DrainPhase::ALL.len());
        // MarkDraining persisted first.
        assert_eq!(saved[0].state.as_deref(), Some("draining"));
        // The captured claim was reset.
        assert_eq!(claims.calls.lock().unwrap().len(), 1);
        assert_eq!(
            claims.calls.lock().unwrap()[0],
            ("rjwalters/anvil".to_string(), 42, "worker-1".to_string())
        );

        let report = build_report(&config, reports);
        assert!(report.safe_to_power_off);
        assert!(report.teardown_command.unwrap().contains("worker-1"));
    }

    /// Secondary "exited" shape: the host itself became unreachable (SSH
    /// connection refused) rather than merely losing its daemon.
    #[test]
    fn happy_path_also_accepts_ssh_level_unreachability_as_exited() {
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)), // capture-claims
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // trigger
            Err("connection refused".to_string()), // wait-remote-exit: host gone ⇒ exited
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // deregister-workspace
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = worker("worker-1");
        let config = base_config("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();

        assert!(removed);
        assert!(!reports.iter().any(|r| r.outcome.is_failure()), "reports: {reports:?}");
        assert!(build_report(&config, reports).safe_to_power_off);
    }

    /// The exit-255-with-stderr shape SSH actually produces when the box is
    /// powered off, exercised through the real classifier rather than the
    /// runner's `Err` path.
    #[test]
    fn wait_remote_exit_treats_ssh_exit_255_as_exited() {
        let runner = MockRunner::new(vec![Ok(CommandOutput {
            code: 255,
            stdout: String::new(),
            stderr: "ssh: connect to host worker-1 port 22: Connection refused".to_string(),
        })]);
        let config = base_config("worker-1");
        assert_eq!(wait_remote_exit(&runner, &config), PhaseOutcome::Changed);
    }

    /// The primary happy-path signal, isolated: host up, daemon gone.
    #[test]
    fn wait_remote_exit_treats_unreachable_daemon_payload_as_exited() {
        let runner = MockRunner::new(vec![Ok(daemon_down_host_up())]);
        let config = base_config("worker-1");
        assert_eq!(wait_remote_exit(&runner, &config), PhaseOutcome::Changed);
    }

    /// A still-draining poll must not be mistaken for either outcome; the
    /// subsequent daemon-down payload ends the wait successfully.
    #[test]
    fn wait_remote_exit_polls_through_still_draining_then_succeeds() {
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[7], true)), // still draining
            Ok(daemon_down_host_up()),         // then gone
        ]);
        let config = base_config("worker-1");
        assert_eq!(wait_remote_exit(&runner, &config), PhaseOutcome::Changed);
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    /// Blocker-2 regression guard: a *real* (nested) payload reporting
    /// `draining: false` while reachable must hit the fail-loud branch.
    #[test]
    fn wait_remote_exit_fails_loudly_on_a_real_refusal_payload() {
        let runner = MockRunner::new(vec![Ok(ok_status(false))]);
        let config = base_config("worker-1");
        match wait_remote_exit(&runner, &config) {
            PhaseOutcome::Failed { reason } => {
                assert!(reason.contains("refused"), "reason: {reason}");
                assert!(reason.contains("worker-1"), "reason: {reason}");
            }
            other => panic!("expected a loud refusal failure, got {other:?}"),
        }
        // One poll is enough — it must not spin to the deadline.
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    // ---- resumability ------------------------------------------------

    #[test]
    fn rerun_after_interruption_resumes_at_recorded_phase() {
        // Simulate a prior run that got through `trigger-remote-drain`.
        let mut record = worker("worker-1");
        record.state = Some("draining".to_string());
        record.drain_phase = Some(DrainPhase::TriggerRemoteDrain.name().to_string());
        record.drain_captured = vec![CapturedClaim {
            issue: 42,
            repo: "rjwalters/anvil".to_string(),
        }];

        let runner = MockRunner::new(vec![
            Err("connection refused".to_string()), // wait-remote-exit
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // deregister
        ]);
        let mut outcomes = HashMap::new();
        outcomes.insert(42, Ok(true));
        let claims = MockClaimResetter::new(outcomes);
        let config = base_config("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();

        assert!(removed);
        // The first two phases must be reported AlreadyDone, not re-run —
        // and critically the mock never received a `capture-claims`/`trigger`
        // call (only 2 responses were scripted and both were consumed by
        // wait-remote-exit + deregister-workspace).
        assert_eq!(reports[0].phase, DrainPhase::MarkDraining.name());
        assert_eq!(reports[0].outcome, PhaseOutcome::AlreadyDone);
        assert_eq!(reports[1].phase, DrainPhase::CaptureClaims.name());
        assert_eq!(reports[1].outcome, PhaseOutcome::AlreadyDone);
        assert_eq!(reports[2].phase, DrainPhase::TriggerRemoteDrain.name());
        assert_eq!(reports[2].outcome, PhaseOutcome::AlreadyDone);
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    #[test]
    fn rerun_on_already_drained_host_is_a_clean_noop() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("fleet.json");
        std::env::set_var(super::super::FLEET_REGISTRY_PATH_ENV, &registry_path);
        // No worker registered at all.
        let config = base_config("gone-host");
        let report = run(&config).unwrap();
        std::env::remove_var(super::super::FLEET_REGISTRY_PATH_ENV);

        assert!(report.phases.is_empty());
        assert!(report.safe_to_power_off);
    }

    // ---- fail-safe timeout without --force-after-timeout -----------------

    #[test]
    fn timeout_without_force_refuses_and_leaves_daemon_running() {
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)), // capture-claims
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // trigger accepted
            Ok(ok_status(false)), // wait-remote-exit: still reachable, draining=false ⇒ refused
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = worker("worker-1");
        let config = base_config("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();

        assert!(!removed, "a refused drain must not remove the roster entry");
        let wait_report = reports
            .iter()
            .find(|r| r.phase == DrainPhase::WaitRemoteExit.name())
            .unwrap();
        assert!(wait_report.outcome.is_failure());
        // Halts — reset-claims / flush / deregister / remove never run.
        assert_eq!(reports.len(), 4);

        let report = build_report(&config, reports);
        assert!(!report.safe_to_power_off);
        assert_eq!(report.exit_code(), 2);
        assert!(report.teardown_command.is_none());
    }

    #[test]
    fn trigger_remote_drain_refusal_halts_before_wait_and_leaves_roster_intact() {
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)), // capture-claims
            Ok(refused()),                    // trigger-remote-drain: remote refuses
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = worker("worker-1");
        let config = base_config("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();

        assert!(!removed);
        let trigger_report = reports
            .iter()
            .find(|r| r.phase == DrainPhase::TriggerRemoteDrain.name())
            .unwrap();
        assert!(trigger_report.outcome.is_failure());
        // wait-remote-exit and everything after never runs.
        assert_eq!(reports.len(), 3);
        assert_eq!(build_report(&config, reports).exit_code(), 1);
    }

    // ---- force-cancel resets every captured claim -------------------------

    #[test]
    fn timeout_with_force_completes_and_resets_every_captured_claim() {
        let mut config = base_config("worker-1");
        config.force_after_timeout = true;

        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[10, 11], true)), // capture-claims: two in-flight
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // trigger (forced)
            Err("connection refused".to_string()),  // wait-remote-exit: exited after force-cancel
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }), // deregister
        ]);
        let mut outcomes = HashMap::new();
        outcomes.insert(10, Ok(true));
        outcomes.insert(11, Ok(true));
        let claims = MockClaimResetter::new(outcomes);
        let mut record = worker("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();

        assert!(removed);
        assert!(!reports.iter().any(|r| r.outcome.is_failure()));
        let mut reset_issues: Vec<u32> = claims
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, i, _)| *i)
            .collect();
        reset_issues.sort_unstable();
        assert_eq!(reset_issues, vec![10, 11]);
    }

    // ---- claim already resolved between capture and reset -----------------

    #[test]
    fn reset_claims_no_ops_an_issue_that_already_left_building() {
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[42], true)),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Err("connection refused".to_string()),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ]);
        // Issue 42 finished normally in the meantime — resetter reports false.
        let mut outcomes = HashMap::new();
        outcomes.insert(42, Ok(false));
        let claims = MockClaimResetter::new(outcomes);
        let mut record = worker("worker-1");
        let config = base_config("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();
        assert!(removed);
        assert!(!reports.iter().any(|r| r.outcome.is_failure()));
        assert_eq!(claims.calls.lock().unwrap().len(), 1);
    }

    // ---- safehouse flush verification (#3998) -----------------------------

    #[test]
    fn unverified_safehouse_flush_yields_nonzero_exit_and_no_safe_line() {
        let mut config = base_config("worker-1");
        config.safehouse_enabled = true;

        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Err("connection refused".to_string()),
            // FlushSafehouse's own stop-and-verify call: the remote script
            // could not verify the flush (state still unclear).
            Ok(CommandOutput {
                code: 1,
                stdout: String::new(),
                stderr: "safehoused flush not verified (state=activating exec_main_status=)"
                    .to_string(),
            }),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = worker("worker-1");

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();
        // Not blocked — deregistration/roster-removal still proceed.
        assert!(removed);

        let flush_report = reports
            .iter()
            .find(|r| r.phase == DrainPhase::FlushSafehouse.name())
            .unwrap();
        match &flush_report.outcome {
            PhaseOutcome::Unverified { reason } => {
                assert!(reason.contains("not verified"), "reason: {reason}")
            }
            other => panic!("expected Unverified, got {other:?}"),
        }

        let report = build_report(&config, reports);
        assert!(!report.safe_to_power_off);
        assert_eq!(report.exit_code(), 3);
        let rendered = report.render_human();
        assert!(!rendered.contains("safe to power off."), "rendered: {rendered}");
        assert!(rendered.contains("DRAIN NOT VERIFIED SAFE"));
    }

    #[test]
    fn unreachable_host_during_flush_yields_unverified_not_failed() {
        // A drain must never let an SSH hiccup during the flush phase halt
        // the run — workspace deregistration + roster removal must still
        // proceed (loom never refuses to retire a box over this).
        let mut config = base_config("worker-1");
        config.safehouse_enabled = true;

        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Err("connection refused".to_string()),
            Err("ssh: connect to host worker-1 port 22: Connection timed out".to_string()),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = worker("worker-1");
        let config2 = config.clone();

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();
        assert!(removed);
        assert!(!reports.iter().any(|r| r.outcome.is_failure()));
        let report = build_report(&config2, reports);
        assert_eq!(report.exit_code(), 3);
    }

    #[test]
    fn verified_safehouse_flush_yields_exit_zero_and_safe_line() {
        let mut config = base_config("worker-1");
        config.safehouse_enabled = true;

        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Err("connection refused".to_string()),
            // FlushSafehouse's stop-and-verify call succeeds (journal showed
            // "room-key backup flushed", or a clean ExecMainStatus).
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = worker("worker-1");
        let config2 = config.clone();

        let (reports, removed) =
            execute_drain(&runner, &claims, &mut record, &config, |_| Ok(())).unwrap();
        assert!(removed);

        let flush_report = reports
            .iter()
            .find(|r| r.phase == DrainPhase::FlushSafehouse.name())
            .unwrap();
        assert_eq!(flush_report.outcome, PhaseOutcome::Changed);

        let report = build_report(&config2, reports);
        assert!(report.safe_to_power_off);
        assert_eq!(report.exit_code(), 0);
        assert!(report.render_human().contains("safe to power off."));
    }

    #[test]
    fn safehouse_disabled_is_skipped_and_stays_safe_without_touching_the_host() {
        let config = base_config("worker-1"); // safehouse_enabled: false
        let runner = MockRunner::new(vec![]);
        let outcome = flush_safehouse(&config, &runner);
        assert!(matches!(outcome, PhaseOutcome::Skipped { .. }));
        assert!(runner.calls.borrow().is_empty(), "disabled flush must never touch the host");
    }

    #[test]
    fn flush_safehouse_stop_command_targets_the_safehoused_unit() {
        // The rendered remote shell must stop (never kill -9) the unit and
        // check both the journal flush line and the unit's exit status —
        // asserted directly on the constant so a future edit cannot silently
        // drop either verification path.
        assert!(FLUSH_SAFEHOUSE_SHELL.contains("systemctl --user stop safehoused"));
        assert!(FLUSH_SAFEHOUSE_SHELL.contains("room-key backup flushed"));
        assert!(FLUSH_SAFEHOUSE_SHELL.contains("ExecMainStatus"));
    }

    // ---- roster entry removed only in the final phase ---------------------

    #[test]
    fn roster_entry_removed_only_after_every_other_phase_completes() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("fleet.json");
        std::env::set_var(super::super::FLEET_REGISTRY_PATH_ENV, &registry_path);

        let mut registry = FleetRegistry::default();
        registry.upsert(worker("worker-1"));
        registry.save(&registry_path).unwrap();

        // Directly exercise `execute_drain` + a `run`-shaped persist, then
        // simulate the same removal `run` performs, to prove ordering
        // without needing a live ssh binary.
        let runner = MockRunner::new(vec![
            Ok(ok_with_in_flight(&[], true)),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Err("connection refused".to_string()),
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        ]);
        let claims = MockClaimResetter::new(HashMap::new());
        let mut record = registry.get("worker-1").unwrap().clone();
        let config = base_config("worker-1");

        let mut still_present_before_final = true;
        let (reports, removed) = execute_drain(&runner, &claims, &mut record, &config, |rec| {
            let mut reg = FleetRegistry::load(&registry_path)?;
            // Every intermediate persist must still find the entry present.
            still_present_before_final = reg.get(&rec.ssh_host).is_some();
            reg.upsert(rec.clone());
            reg.save(&registry_path)
        })
        .unwrap();

        assert!(still_present_before_final);
        assert!(removed);
        assert!(!reports.iter().any(|r| r.outcome.is_failure()));

        std::env::remove_var(super::super::FLEET_REGISTRY_PATH_ENV);
    }

    // ---- GhClaimResetter PATH bootstrap (#4831) -----------------------

    /// Write an executable stub `gh` that dispatches on the subcommand
    /// (`issue view` / `issue edit` / `issue comment`) at
    /// `<dir>/gh`, mirroring just enough of the real `gh` CLI's output shape
    /// for [`GhClaimResetter::reset_claim`] to exercise its full parse/branch
    /// logic against a *resolved-via-PATH* binary rather than a mocked trait.
    #[cfg(unix)]
    fn write_stub_gh(dir: &std::path::Path, has_building_label: bool) {
        use std::os::unix::fs::PermissionsExt;
        let script = format!(
            r#"#!/bin/sh
case "$2" in
  view)
    echo '{{"labels":[{{"name":"{label}"}}]}}'
    ;;
  edit|comment)
    ;;
esac
exit 0
"#,
            label = if has_building_label {
                "loom:building"
            } else {
                "some-other-label"
            }
        );
        let gh_path = dir.join("gh");
        std::fs::write(&gh_path, script).unwrap();
        let mut perms = std::fs::metadata(&gh_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&gh_path, perms).unwrap();
    }

    /// [`GhClaimResetter`] must resolve `gh` via the canonical PATH built by
    /// [`super::super::path_bootstrap::local_gh_path_env`] — specifically
    /// `$HOME/.local/bin` — even when the real `gh` (if any) elsewhere on
    /// this process's inherited PATH is NOT what should win. Proves the
    /// canonical set is prepended (highest precedence), not merely appended,
    /// closing the #4831 gap where `GhClaimResetter` previously ran with
    /// whatever PATH the daemon process happened to inherit.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial(env_home_path)]
    fn gh_claim_resetter_resolves_gh_via_canonical_home_local_bin() {
        let fake_home = tempfile::tempdir().unwrap();
        let local_bin = fake_home.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        write_stub_gh(&local_bin, true);

        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let result = GhClaimResetter.reset_claim("rjwalters/loom", 4831, "worker-stub");

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        // The stub reports the issue still `loom:building`, so a successful
        // resolve-and-run flips it (Ok(true)); a PATH miss would instead
        // surface as an `Err` ("gh issue view ... failed" / launch failure)
        // or (if some unrelated real `gh` on the inherited PATH answered
        // first) a result inconsistent with our stub's scripted labels.
        assert!(
            matches!(result, Ok(true)),
            "expected GhClaimResetter to resolve+run the stub gh via $HOME/.local/bin, got {result:?}"
        );
    }

    /// Same resolution path, but the stub reports the issue as NOT holding
    /// `loom:building` — [`GhClaimResetter::reset_claim`] must short-circuit
    /// to `Ok(false)` without attempting `issue edit`/`issue comment`.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial(env_home_path)]
    fn gh_claim_resetter_no_op_when_label_absent() {
        let fake_home = tempfile::tempdir().unwrap();
        let local_bin = fake_home.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        write_stub_gh(&local_bin, false);

        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let result = GhClaimResetter.reset_claim("rjwalters/loom", 4831, "worker-stub");

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(matches!(result, Ok(false)));
    }
}
