//! `loom-daemon fleet status` (#4342, epic #4340): aggregate sweep/token/health
//! state across every fleet host, side by side, in one command.
//!
//! # Scope: merge/render, not new status IPC
//!
//! `loom-daemon status --json` already exists (#4069) and covers nearly every
//! column an operator needs — `in_flight`, `capacity_bound`, `dynamic_cap`,
//! `token_usage`, the per-repo workspace table, and a machine-classified
//! unreachable-daemon payload with a non-zero exit code. This module does
//! **not** duplicate any of that: it is a roster + concurrent SSH fanout +
//! merge/render layer over the existing per-host `status --json` output.
//!
//! - **Roster**: enumerated from [`super::FleetRegistry`] (written by `fleet
//!   add-worker`, #4341).
//! - **Local host**: collected in-process over the daemon's own Unix socket
//!   (`main.rs`'s `query_daemon_status`), never `ssh localhost` — this module
//!   only knows how to *render* the local [`HostReport`] `main.rs` builds via
//!   [`HostReport::local_up`] / [`HostReport::local_down`].
//! - **Remote hosts**: collected concurrently (bounded by a per-host
//!   [`tokio::time::timeout`], #4342 AC — one hung host cannot stall the
//!   report) via the [`HostStatusSource`] seam (mirrors #4341's
//!   [`super::CommandRunner`] pattern: a real `ssh` implementation
//!   ([`SshStatusSource`]) and a scripted mock drive the unit tests).
//!
//! # Loud, distinct per-host states
//!
//! Silence must never read as idle (the #3979 pilot's monitoring lesson). Each
//! host renders one of six distinct [`HostState`]s: [`HostState::Up`],
//! [`HostState::DaemonDown`] (the host answered, but its daemon reports the
//! #4069 unreachable-daemon payload), [`HostState::Unreachable`] (SSH/connect
//! failure or a per-host timeout), [`HostState::PoweredOff`] (#4697: an
//! `Unreachable` host with a configured idle-shutdown window whose silence has
//! already exceeded that window — likely an EXPECTED power-off, not a
//! failure), [`HostState::ParseError`] (severe version skew — the payload
//! could not be parsed as JSON at all), and [`HostState::Draining`] (registry
//! `state: "draining"`, written by `fleet drain`'s first phase, #4343 —
//! rendered distinctly rather than treated as an unrecognized/parse-error
//! registry state). An empty roster never renders as empty output:
//! [`FleetStatusReport::render_human`] always prints an explicit "no fleet
//! workers registered" notice alongside the local host's row.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use super::path_bootstrap;
use super::{CommandOutput, FleetRegistry, WorkerRecord};

/// Default per-host SSH connect + collection timeout (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 8;

/// The registry `state` sentinel `fleet drain` (#4343) writes as its first
/// phase — recognized here so a mid-drain host renders as
/// [`HostState::Draining`] instead of falling through to a parse-error path
/// (this is a registry-schema check, not a remote-JSON one).
const DRAINING_SENTINEL: &str = "draining";

/// The alias rendered for the local host's own row (AC 1: "including the
/// local host").
pub const LOCAL_HOST_ALIAS: &str = "local";

// ===========================================================================
// Host state
// ===========================================================================

/// A per-host classification. Deliberately loud + distinct (AC 2) — silence
/// must never look like idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    /// The host answered with a well-formed status payload and its daemon is
    /// up.
    Up,
    /// The host answered, but its own daemon is down/unreachable there (the
    /// #4069 unreachable-daemon classification, parsed and carried through
    /// rather than re-derived).
    DaemonDown,
    /// SSH/connect failure, or the per-host collection timed out.
    Unreachable,
    /// An `Unreachable` host with a configured idle-shutdown window
    /// ([`WorkerRecord::idle_shutdown_minutes`]) whose observed silence has
    /// already exceeded that window (#4697) — likely the host's own
    /// idle-shutdown guard powering it off as designed (#3998/#4477), not a
    /// failure. Still not `Up` (the exit-code policy, AC 3, treats it as
    /// unhealthy like every other non-`Up` state), but rendered + labeled
    /// distinctly so an operator does not mistake "expected, by design" for
    /// "outage, investigate".
    PoweredOff,
    /// The host responded, but the payload could not be parsed as JSON at all
    /// (severe version skew) — distinct from `DaemonDown`, whose payload is
    /// still well-formed JSON (#4069).
    ParseError,
    /// Registry `state: "draining"` (#4343): the host is mid-drain and
    /// present-but-loudly-marked, not a parse error.
    Draining,
}

impl HostState {
    /// Fixed label for the human table / `--json` `state` field's prose twin.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            HostState::Up => "UP",
            HostState::DaemonDown => "DAEMON DOWN",
            HostState::Unreachable => "UNREACHABLE",
            // Kept to 11 chars, matching the longest pre-existing label, so
            // it still fits `render_human`'s `{:<12}` STATE column — the
            // "expected, not a failure" qualifier rides on the per-row detail
            // line and the summary tally instead of blowing the table apart.
            HostState::PoweredOff => "POWERED OFF",
            HostState::ParseError => "PARSE ERROR",
            HostState::Draining => "DRAINING",
        }
    }

    /// Whether this state counts as fully healthy for the exit-code policy
    /// (AC 3): a zero exit requires every roster host `Up`.
    #[must_use]
    pub fn is_healthy(self) -> bool {
        matches!(self, HostState::Up)
    }
}

/// Cheap best-effort probe for a `safehoused` process on a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafehousedProbe {
    /// The probe positively found a running `safehoused`.
    Present,
    /// The probe ran and found none.
    Absent,
    /// The probe could not run conclusively (host unreachable, timed out, or
    /// not attempted) — degrade to "unknown" rather than erroring the row.
    Unknown,
}

impl SafehousedProbe {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SafehousedProbe::Present => "present",
            SafehousedProbe::Absent => "absent",
            SafehousedProbe::Unknown => "unknown",
        }
    }
}

/// The local host's own tailnet self-check result (#4952). Mirrors
/// [`SafehousedProbe`]'s three-way shape — a cheap best-effort probe that
/// must never fail the whole report — but is explicitly **local-only**: run
/// once, in-process (never over SSH), only when
/// [`all_tailnet_hosts_unreachable`] flags every tailnet-addressed remote
/// roster host as [`HostState::Unreachable`]. That pattern (every tailnet
/// peer failing simultaneously) is indistinguishable from a genuine
/// multi-host outage without this check — see the module doc's 2026-08-02
/// incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalTailnetState {
    /// The local `tailscale` client reports the tailnet is up/running.
    Up,
    /// The local `tailscale` client reports the tailnet is down/stopped —
    /// the tell this issue exists for: the correct diagnosis (and
    /// one-command fix) is on the local host, not a fleet-wide outage.
    Down,
    /// `tailscale` ran but its output could not be conclusively classified —
    /// degrade to "unknown" rather than misreporting either up or down.
    Unknown,
}

impl LocalTailnetState {
    /// Fixed label for the human banner / `--json` twin.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            LocalTailnetState::Up => "up",
            LocalTailnetState::Down => "down",
            LocalTailnetState::Unknown => "unknown",
        }
    }
}

// ===========================================================================
// Per-host report
// ===========================================================================

/// One host's merged row in the fleet status report.
#[derive(Debug, Clone, Serialize)]
pub struct HostReport {
    /// The SSH alias/host (or [`LOCAL_HOST_ALIAS`] for the local row).
    pub alias: String,
    /// The host's tailnet device name, when the registry has one (#4342's
    /// operator scope-clarification).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tailnet_name: Option<String>,
    /// Cloud provider instance id, when the registry has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
    /// Who/what added this worker, when the registry has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    /// Whether this row is the local host (collected in-process, never over
    /// SSH).
    pub is_local: bool,
    /// The classified state (AC 2).
    pub state: HostState,
    /// The workspace repos the registry says this worker manages.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// The host's own `status --json` payload, leniently parsed as a raw
    /// [`serde_json::Value`] (a remote worker may run an older/newer binary;
    /// missing fields render as "–" rather than failing the row). `None` when
    /// the host could not be reached / timed out / failed to parse / is
    /// draining.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<serde_json::Value>,
    /// A one-line diagnostic for a non-`Up` state (error text, timeout
    /// message, parse error, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Cheap `safehoused` presence probe.
    pub safehoused: SafehousedProbe,
}

impl HostReport {
    /// Build the local host's row from an already-collected `status --json`
    /// value (collected in-process by `main.rs` over the daemon's own Unix
    /// socket — see the module doc).
    #[must_use]
    pub fn local_up(status: serde_json::Value) -> Self {
        Self {
            alias: LOCAL_HOST_ALIAS.to_string(),
            tailnet_name: None,
            provider_instance_id: None,
            added_by: None,
            is_local: true,
            state: HostState::Up,
            workspaces: Vec::new(),
            status: Some(status),
            detail: None,
            safehoused: SafehousedProbe::Unknown,
        }
    }

    /// Build the local host's row when its own daemon could not be reached
    /// (mirrors #4069's unreachable-daemon classification, carried in
    /// `detail`).
    #[must_use]
    pub fn local_down(detail: String) -> Self {
        Self {
            alias: LOCAL_HOST_ALIAS.to_string(),
            tailnet_name: None,
            provider_instance_id: None,
            added_by: None,
            is_local: true,
            state: HostState::DaemonDown,
            workspaces: Vec::new(),
            status: None,
            detail: Some(detail),
            safehoused: SafehousedProbe::Unknown,
        }
    }

    /// A registry entry with `state: "draining"` — rendered without an SSH
    /// probe (the drain state is a control-plane fact, not a liveness one).
    #[must_use]
    fn draining(worker: &WorkerRecord) -> Self {
        Self {
            alias: worker.ssh_host.clone(),
            tailnet_name: worker.tailnet_name.clone(),
            provider_instance_id: worker.provider_instance_id.clone(),
            added_by: worker.added_by.clone(),
            is_local: false,
            state: HostState::Draining,
            workspaces: worker.repos.clone(),
            status: None,
            detail: Some(
                "drain in progress (fleet drain) — host stays visible until removed".to_string(),
            ),
            safehoused: SafehousedProbe::Unknown,
        }
    }
}

// ===========================================================================
// Command runner seam (the SSH / mock seam, mirrors `super::CommandRunner`)
// ===========================================================================

/// The seam between per-host collection and the network: fetch a remote
/// worker's `status --json`, and cheaply probe for `safehoused`. The real impl
/// ([`SshStatusSource`]) shells out to `ssh`; tests supply a scripted mock so
/// the whole merge/render/exit-code path is exercised without a live box.
///
/// Mirrors [`super::CommandRunner`]'s contract: `Err` is reserved for a
/// transport launch failure — a non-zero remote exit or unparseable payload is
/// a successful launch and is classified via [`classify_status_output`].
pub trait HostStatusSource: Send + Sync {
    /// Run `loom-daemon status --json` on `ssh_host` and return its captured
    /// output.
    fn fetch_status(&self, ssh_host: &str) -> Result<CommandOutput>;

    /// Best-effort `safehoused` presence probe on `ssh_host`. Never errors —
    /// an inconclusive probe returns [`SafehousedProbe::Unknown`].
    fn probe_safehoused(&self, ssh_host: &str) -> SafehousedProbe;
}

/// The production [`HostStatusSource`]: `ssh -o BatchMode=yes -o
/// ConnectTimeout=<N> <host> '<shell>'`.
pub struct SshStatusSource {
    /// `ssh -o ConnectTimeout=<N>` — bounds how long a dead/unreachable host
    /// can block the connect phase (the per-host [`tokio::time::timeout`]
    /// wrapping the whole collection is the outer bound; this is the inner
    /// one so a slow DNS/TCP handshake fails fast on its own).
    pub connect_timeout_secs: u64,
}

impl SshStatusSource {
    /// A source with the default connect timeout ([`DEFAULT_TIMEOUT_SECS`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            connect_timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl Default for SshStatusSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Shell probe for `safehoused`: prefers the socket check `resolve_config`
/// documents (`$SAFEHOUSED_SOCKET`, else the XDG-ish default), falls back to
/// `pgrep -x safehoused`.
const SAFEHOUSED_PROBE_SHELL: &str = r#"
if [ -n "${SAFEHOUSED_SOCKET:-}" ] && [ -S "$SAFEHOUSED_SOCKET" ]; then
  echo present
elif [ -S "$HOME/.safehouse/safehoused.sock" ]; then
  echo present
elif pgrep -x safehoused >/dev/null 2>&1; then
  echo present
else
  echo absent
fi
"#;

impl HostStatusSource for SshStatusSource {
    fn fetch_status(&self, ssh_host: &str) -> Result<CommandOutput> {
        run_ssh(ssh_host, "loom-daemon status --json", self.connect_timeout_secs)
    }

    fn probe_safehoused(&self, ssh_host: &str) -> SafehousedProbe {
        match run_ssh(ssh_host, SAFEHOUSED_PROBE_SHELL, self.connect_timeout_secs) {
            Ok(out) if out.ok() && out.stdout.trim() == "present" => SafehousedProbe::Present,
            Ok(out) if out.ok() => SafehousedProbe::Absent,
            _ => SafehousedProbe::Unknown,
        }
    }
}

/// Run one `shell` on `host` over `ssh`, with a bounded connect timeout and no
/// stdin (status collection never feeds a payload).
///
/// `ssh host '<cmd>'` runs `<cmd>` under the remote user's default shell in
/// **non-interactive, non-login** mode — it does NOT source `~/.profile` (the
/// canonical PATH #4831 wires into `~/.profile` on a provisioned worker, see
/// `add_worker.rs::render_machine_layout`), so a bare `loom-daemon` on the
/// remote command line is only found if the remote sshd's own default PATH
/// (`/etc/environment`, `sshd_config`'s `PATH=` in `AcceptEnv`/`SetEnv`, or
/// the shell's non-interactive startup file) happens to include
/// `$HOME/.local/bin`. Rather than depend on that, every remote command this
/// module runs is prefixed with the same canonical PATH export line
/// [`super::add_worker`]'s provisioning steps use, so `loom-daemon` (and, for
/// the safehouse probe, `pgrep`) resolve deterministically regardless of the
/// remote shell's non-interactive PATH.
fn run_ssh(host: &str, shell: &str, connect_timeout_secs: u64) -> Result<CommandOutput> {
    let remote_command = format!("{}{shell}", path_bootstrap::canonical_path_export_line());
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o")
        .arg(format!("ConnectTimeout={connect_timeout_secs}"));
    cmd.arg(host);
    cmd.arg(&remote_command);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to launch `ssh {host}` (is ssh installed?)"))?;
    let out = child
        .wait_with_output()
        .with_context(|| format!("waiting on `ssh {host}`"))?;
    Ok(CommandOutput {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Leniently classify a remote `status --json` [`CommandOutput`] (#4342's
/// lenient-parsing requirement): parsed as a raw [`serde_json::Value`] rather
/// than a strict struct, so an older/newer remote binary's reduced/extended
/// field set never fails the row — only genuinely non-JSON output does.
#[must_use]
fn classify_status_output(
    out: &CommandOutput,
) -> (HostState, Option<serde_json::Value>, Option<String>) {
    if out.stdout.trim().is_empty() {
        return (
            HostState::Unreachable,
            None,
            Some(tail_stderr_or(
                &out.stderr,
                "empty response from remote `loom-daemon status --json`",
            )),
        );
    }
    match serde_json::from_str::<serde_json::Value>(&out.stdout) {
        // #4069: an unreachable-daemon payload is still well-formed JSON but
        // carries a top-level `error` key — classify as DaemonDown (the host
        // itself answered; its daemon did not) rather than Up.
        Ok(value) if value.get("error").is_some() => (HostState::DaemonDown, Some(value), None),
        Ok(value) => (HostState::Up, Some(value), None),
        Err(e) => (
            HostState::ParseError,
            None,
            Some(format!("could not parse remote status JSON: {e}")),
        ),
    }
}

/// Attempt to reclassify an already-`Unreachable` host as [`HostState::PoweredOff`]
/// (#4697): an EXPECTED power-off from the host's own idle-shutdown guard
/// (design correct, #3998/#4477 — out of scope here), rather than a genuinely
/// unexpected outage. Only ever called on a host `classify_status_output`/the
/// timeout path already resolved to `Unreachable` — idle-shutdown powers the
/// WHOLE host off (SSH included), so anything that still answers SSH (even a
/// `DaemonDown`/`ParseError` payload) is definitionally not this case.
///
/// Deliberately conservative (a false "expected, don't worry" reading on a
/// genuinely dead host is worse than an occasional false alarm, per #4697's
/// acceptance criteria): reclassifies ONLY when both
/// [`WorkerRecord::idle_shutdown_minutes`] is configured AND
/// [`WorkerRecord::last_seen_up_at`] is a parseable past timestamp whose
/// elapsed silence has already reached that configured window. Either input
/// missing (no guard configured, or the host has never been observed `Up` by
/// `fleet status` yet — a freshly-added record, or one predating these
/// fields) leaves the host `Unreachable`, the pre-#4697,
/// unexpected-until-proven-otherwise default. Returns `None` to mean "leave
/// the state alone".
#[must_use]
fn classify_expected_power_off(
    worker: &WorkerRecord,
    now: DateTime<Utc>,
) -> Option<(HostState, String)> {
    let minutes = worker.idle_shutdown_minutes?;
    let last_seen_raw = worker.last_seen_up_at.as_deref()?;
    let last_seen_at = DateTime::parse_from_rfc3339(last_seen_raw)
        .ok()?
        .with_timezone(&Utc);
    let elapsed_minutes = now.signed_duration_since(last_seen_at).num_minutes();
    if elapsed_minutes < i64::from(minutes) {
        // Silent, but not yet past the configured window — too early to call
        // this "expected"; stay Unreachable (unexpected-until-proven).
        return None;
    }
    Some((
        HostState::PoweredOff,
        format!(
            "unreachable for {elapsed_minutes}m — past the configured idle-shutdown window \
             ({minutes}m, last confirmed up at {last_seen_raw}). Likely an EXPECTED power-off \
             (the host's own idle-shutdown guard), not a failure. Wake it via the provider \
             console/CLI (Loom never calls a cloud CLI itself — see daemon-reference.md's \
             `fleet add-worker` idle-shutdown wake-path notes for re-registration steps)."
        ),
    ))
}

fn tail_stderr_or(stderr: &str, fallback: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(fallback)
        .trim();
    line.to_string()
}

// ===========================================================================
// Concurrent per-host collection
// ===========================================================================

/// Collect one remote worker's row, bounded by `timeout` (#4342 AC: one hung
/// host cannot stall the whole report). A registry `state: "draining"` entry
/// short-circuits straight to [`HostReport::draining`] — no SSH probe at all,
/// since drain is a control-plane fact rather than a liveness one.
pub async fn collect_remote_host(
    source: Arc<dyn HostStatusSource>,
    worker: WorkerRecord,
    timeout: Duration,
) -> HostReport {
    if worker.state.as_deref() == Some(DRAINING_SENTINEL) {
        return HostReport::draining(&worker);
    }

    let alias = worker.ssh_host.clone();

    let (state, status, detail) = {
        let src = source.clone();
        let host = alias.clone();
        let fetch = tokio::task::spawn_blocking(move || src.fetch_status(&host));
        match tokio::time::timeout(timeout, fetch).await {
            Err(_elapsed) => (
                HostState::Unreachable,
                None,
                Some(format!("timed out after {}s with no response", timeout.as_secs())),
            ),
            Ok(Err(join_err)) => (
                HostState::Unreachable,
                None,
                Some(format!("collection task did not complete: {join_err}")),
            ),
            Ok(Ok(Err(e))) => (HostState::Unreachable, None, Some(e.to_string())),
            Ok(Ok(Ok(out))) => classify_status_output(&out),
        }
    };

    // #4697: an Unreachable host may actually be an EXPECTED power-off from
    // its own idle-shutdown guard rather than a genuine outage — see
    // classify_expected_power_off's doc comment for the conservative
    // conditions this requires.
    let (state, detail) = if state == HostState::Unreachable {
        match classify_expected_power_off(&worker, Utc::now()) {
            Some((new_state, new_detail)) => (new_state, Some(new_detail)),
            None => (state, detail),
        }
    } else {
        (state, detail)
    };

    let safehoused = {
        let src = source.clone();
        let host = alias.clone();
        let probe = tokio::task::spawn_blocking(move || src.probe_safehoused(&host));
        match tokio::time::timeout(timeout, probe).await {
            Ok(Ok(p)) => p,
            _ => SafehousedProbe::Unknown,
        }
    };

    HostReport {
        alias,
        tailnet_name: worker.tailnet_name,
        provider_instance_id: worker.provider_instance_id,
        added_by: worker.added_by,
        is_local: false,
        state,
        workspaces: worker.repos,
        status,
        detail,
        safehoused,
    }
}

// ===========================================================================
// Merge, render, exit-code policy
// ===========================================================================

/// The full merged fleet report: every host's row, plus a summary the
/// exit-code policy (AC 3) and `--json` consumers both use.
#[derive(Debug, Clone, Serialize)]
pub struct FleetStatusReport {
    pub hosts: Vec<HostReport>,
    pub summary: FleetStatusSummary,
    /// #4952: the local tailnet self-check result. `None` when the check was
    /// never run — either [`all_tailnet_hosts_unreachable`] was `false` (no
    /// reason to suspect a local tailnet outage) or the `tailscale` CLI is
    /// not installed on this host (AC 4 — a missing CLI must not become a
    /// new hard dependency, so the caller skips the check silently rather
    /// than erroring). Populated by the caller
    /// (`handle_fleet_status_command`) after [`collect_fleet_report`]
    /// returns, since the check itself is a local shell-out, not part of the
    /// SSH fanout this module owns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_tailnet: Option<LocalTailnetState>,
}

/// Roll-up counts across [`FleetStatusReport::hosts`].
#[derive(Debug, Clone, Serialize)]
pub struct FleetStatusSummary {
    pub total: usize,
    pub up: usize,
    pub daemon_down: usize,
    pub unreachable: usize,
    /// #4697: `Unreachable` hosts reclassified as a likely EXPECTED
    /// power-off (see [`HostState::PoweredOff`]) — counted separately from
    /// `unreachable` so a monitor can tell "N unexpected outages" from "N
    /// hosts probably just idle-shut-down as designed" at a glance.
    pub powered_off: usize,
    pub parse_error: usize,
    pub draining: usize,
    /// `true` when the fleet registry itself had zero remote workers
    /// registered (the local host row is still always present) — surfaced so
    /// a scripted consumer distinguishes "no fleet yet" from "fleet all
    /// healthy".
    pub empty_roster: bool,
}

/// Collect the whole fleet report: the already-built `local` row (see the
/// module doc — collected by the caller over the in-process socket path),
/// merged with every registry worker collected concurrently over `source`,
/// each bounded by `timeout`.
pub async fn collect_fleet_report(
    source: Arc<dyn HostStatusSource>,
    registry: FleetRegistry,
    local: HostReport,
    timeout: Duration,
) -> FleetStatusReport {
    let empty_roster = registry.workers.is_empty();

    // Spawn every remote collection up front (this is what makes the fanout
    // concurrent), then await each in registry order — a hung host is bounded
    // by its own `timeout` and cannot delay the others' already-in-flight
    // collection.
    let handles: Vec<_> = registry
        .workers
        .into_iter()
        .map(|worker| tokio::spawn(collect_remote_host(source.clone(), worker, timeout)))
        .collect();

    let mut hosts = vec![local];
    for handle in handles {
        match handle.await {
            Ok(report) => hosts.push(report),
            Err(join_err) => hosts.push(HostReport {
                alias: "<unknown>".to_string(),
                tailnet_name: None,
                provider_instance_id: None,
                added_by: None,
                is_local: false,
                state: HostState::Unreachable,
                workspaces: Vec::new(),
                status: None,
                detail: Some(format!("collection task panicked: {join_err}")),
                safehoused: SafehousedProbe::Unknown,
            }),
        }
    }

    let summary = summarize(&hosts, empty_roster);
    FleetStatusReport {
        hosts,
        summary,
        local_tailnet: None,
    }
}

/// #4952 AC 1 + AC 3: true when every remote roster host that has a tailnet
/// address ([`HostReport::tailnet_name`] is `Some`) is
/// [`HostState::Unreachable`] — the trigger condition for the local tailnet
/// self-check. The local host row is always excluded (it is never collected
/// over the tailnet-dependent SSH path). A host with no tailnet address
/// (plain LAN/SSH) is also excluded from the inference (AC 3), so a mixed
/// roster can still surface a genuine partial outage through those hosts'
/// own states.
///
/// Vacuously `false` when there are no tailnet-addressed remote hosts at all
/// — with nothing to infer from, there is no reason to suspect a local
/// tailnet outage.
#[must_use]
pub fn all_tailnet_hosts_unreachable(hosts: &[HostReport]) -> bool {
    let mut saw_tailnet_host = false;
    for host in hosts {
        if host.is_local || host.tailnet_name.is_none() {
            continue;
        }
        saw_tailnet_host = true;
        if host.state != HostState::Unreachable {
            return false;
        }
    }
    saw_tailnet_host
}

/// After a `fleet status` poll, refresh [`WorkerRecord::last_seen_up_at`] for
/// every REMOTE host (the local host has no registry record) this poll
/// observed [`HostState::Up`]. Returns `true` when at least one record
/// changed, so the caller only re-saves the registry file when something
/// actually moved.
///
/// This is the write side of the #4697 "expected power-off" heuristic
/// ([`classify_expected_power_off`]): a LATER poll that finds the host
/// `Unreachable` needs some persisted "last known up" reference point to
/// measure elapsed silence against, and `fleet status` is the only thing that
/// ever observes a host's liveness — so it is the one that must persist it
/// (there is no separate continuous heartbeat/monitor in this design).
pub fn update_last_seen_up_at(
    registry: &mut FleetRegistry,
    report: &FleetStatusReport,
    now: DateTime<Utc>,
) -> bool {
    let now_str = now.to_rfc3339();
    let mut changed = false;
    for host in &report.hosts {
        if host.is_local || host.state != HostState::Up {
            continue;
        }
        if let Some(worker) = registry
            .workers
            .iter_mut()
            .find(|w| w.ssh_host == host.alias)
        {
            if worker.last_seen_up_at.as_deref() != Some(now_str.as_str()) {
                worker.last_seen_up_at = Some(now_str.clone());
                changed = true;
            }
        }
    }
    changed
}

fn summarize(hosts: &[HostReport], empty_roster: bool) -> FleetStatusSummary {
    let mut summary = FleetStatusSummary {
        total: hosts.len(),
        up: 0,
        daemon_down: 0,
        unreachable: 0,
        powered_off: 0,
        parse_error: 0,
        draining: 0,
        empty_roster,
    };
    for host in hosts {
        match host.state {
            HostState::Up => summary.up += 1,
            HostState::DaemonDown => summary.daemon_down += 1,
            HostState::Unreachable => summary.unreachable += 1,
            HostState::PoweredOff => summary.powered_off += 1,
            HostState::ParseError => summary.parse_error += 1,
            HostState::Draining => summary.draining += 1,
        }
    }
    summary
}

impl FleetStatusReport {
    /// Exit-code policy (AC 3): `0` only when every host is [`HostState::Up`];
    /// otherwise non-zero so a monitor/CI check fails loudly rather than
    /// silently reading an unreachable/down/draining host as fine.
    ///
    /// #5060: an EMPTY roster is also non-zero, even though the only row
    /// present (the local host) is `Up`. A 1-of-1 all-up report from an empty
    /// registry is indistinguishable from a genuinely healthy fleet, so an
    /// unconfigured/lost/mis-pathed registry would otherwise read as "all
    /// clear" to every scripted consumer — the exact fleet-visibility blind
    /// spot this policy exists to prevent. `empty_roster` means "this command
    /// saw no fleet", which is a state to investigate, not to pass.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.summary.empty_roster {
            return 1;
        }
        if self.summary.up == self.summary.total {
            0
        } else {
            1
        }
    }

    /// Render the human-readable table (AC 1: one command, every host side by
    /// side). Never empty output (AC 2): an empty roster prints an explicit
    /// notice in addition to the local host's row, AND (#5060) a qualified
    /// roster-coverage summary line rather than an unqualified count.
    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str("Fleet status:\n");
        if self.summary.empty_roster {
            out.push_str(
                "  (no fleet workers registered — showing the local host only; \
                 add one with `loom-daemon fleet add-worker <ssh-host> --repo <owner/name>`)\n",
            );
        }
        if self.local_tailnet == Some(LocalTailnetState::Down) {
            out.push_str(concat!(
                "  !!! LOCAL TAILNET DOWN — remote states below are UNKNOWN, not a confirmed ",
                "fleet outage !!!\n",
                "      every tailnet-addressed roster host below reports UNREACHABLE because ",
                "the tailnet client on THIS host is down, not because those hosts are down.\n",
                "      restart it, then re-run `fleet status`: `tailscale up` (or, on macOS, ",
                "`open -a Tailscale`).\n",
            ));
        }
        out.push_str(&format!(
            "  {:<20} {:<12} {:<16} {:<8} {:<10} {:<8}\n",
            "HOST", "STATE", "TAILNET", "IN-FLT", "CAP", "SAFEHSD"
        ));
        out.push_str(&format!("  {:-<80}\n", ""));
        for host in &self.hosts {
            let tailnet = host.tailnet_name.as_deref().unwrap_or("-");
            let in_flight = field_str(&host.status, &["in_flight_count"]);
            let cap = field_str(&host.status, &["dynamic_cap", "effective"]);
            out.push_str(&format!(
                "  {:<20} {:<12} {:<16} {:<8} {:<10} {:<8}\n",
                host.alias,
                host.state.label(),
                tailnet,
                in_flight,
                cap,
                host.safehoused.label(),
            ));
            if let Some(detail) = &host.detail {
                out.push_str(&format!("      -> {detail}\n"));
            }
            if !host.workspaces.is_empty() {
                out.push_str(&format!("      workspaces: {}\n", host.workspaces.join(", ")));
            }
        }
        let breakdown = format!(
            "{} daemon-down, {} unreachable, {} powered-off (expected), {} parse-error, {} draining",
            self.summary.daemon_down,
            self.summary.unreachable,
            self.summary.powered_off,
            self.summary.parse_error,
            self.summary.draining,
        );
        // #5060: the SUMMARY LINE ITSELF — not just the notice above the table
        // — has to read as "not a healthy fleet" for an empty roster. An
        // operator scanning only the last line of `fleet status` (or a log
        // scraper matching it) otherwise sees an unqualified "1 host(s): 1 up"
        // that is byte-for-byte the shape of a real, healthy single-host
        // fleet. Qualifying it as ROSTER COVERAGE ("N of M KNOWN hosts") makes
        // the denominator's provenance explicit.
        if self.summary.empty_roster {
            out.push_str(&format!(
                "\n{} of {} known host(s) up — ROSTER IS EMPTY, showing the local host only; \
                 this is NOT a healthy fleet ({}).\n",
                self.summary.up, self.summary.total, breakdown,
            ));
        } else {
            out.push_str(&format!(
                "\n{} host(s): {} up, {}.\n",
                self.summary.total, self.summary.up, breakdown,
            ));
        }
        out
    }
}

/// Walk `path` through an optional [`serde_json::Value`], rendering "–" when
/// the value, or any segment of the path, is absent/null (the lenient-parsing
/// requirement: an older/newer remote binary's reduced/extended field set
/// never fails the row).
fn field_str(value: &Option<serde_json::Value>, path: &[&str]) -> String {
    let Some(mut cur) = value.as_ref() else {
        return "-".to_string();
    };
    for segment in path {
        match cur.get(segment) {
            Some(next) => cur = next,
            None => return "-".to_string(),
        }
    }
    match cur {
        serde_json::Value::Null => "-".to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ---- Mock source -----------------------------------------------------

    #[derive(Clone)]
    enum MockOutcome {
        Output(CommandOutput),
        LaunchErr,
        /// Sleeps longer than the caller's timeout, to exercise the per-host
        /// timeout path without stalling the rest of the fanout.
        Hang(Duration),
    }

    struct MockSource {
        responses: Mutex<HashMap<String, MockOutcome>>,
    }

    impl MockSource {
        fn new(responses: HashMap<String, MockOutcome>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl HostStatusSource for MockSource {
        fn fetch_status(&self, ssh_host: &str) -> Result<CommandOutput> {
            let outcome = self
                .responses
                .lock()
                .unwrap()
                .get(ssh_host)
                .cloned()
                .unwrap_or(MockOutcome::LaunchErr);
            match outcome {
                MockOutcome::Output(out) => Ok(out),
                MockOutcome::LaunchErr => {
                    Err(anyhow::anyhow!("mock: ssh launch failed for {ssh_host}"))
                }
                MockOutcome::Hang(dur) => {
                    std::thread::sleep(dur);
                    Ok(CommandOutput {
                        code: 0,
                        stdout: "{}".to_string(),
                        stderr: String::new(),
                    })
                }
            }
        }

        fn probe_safehoused(&self, _ssh_host: &str) -> SafehousedProbe {
            SafehousedProbe::Unknown
        }
    }

    fn up_output(json: &str) -> CommandOutput {
        CommandOutput {
            code: 0,
            stdout: json.to_string(),
            stderr: String::new(),
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
            tailnet_name: Some("loom-worker-1".to_string()),
            added_by: None,
            state: None,
            drain_phase: None,
            drain_captured: Vec::new(),
            idle_shutdown_minutes: None,
            last_seen_up_at: None,
        }
    }

    // ---- classify_status_output -------------------------------------------

    #[test]
    fn classify_up_on_well_formed_json() {
        let out = up_output(r#"{"in_flight_count": 2}"#);
        let (state, value, detail) = classify_status_output(&out);
        assert_eq!(state, HostState::Up);
        assert!(value.is_some());
        assert!(detail.is_none());
    }

    #[test]
    fn classify_daemon_down_on_error_payload() {
        // #4069's unreachable-daemon JSON: well-formed, but carries `error`.
        let out =
            up_output(r#"{"error": "could not reach loom-daemon at /tmp/x.sock: connect failed"}"#);
        let (state, value, _detail) = classify_status_output(&out);
        assert_eq!(state, HostState::DaemonDown);
        assert!(value.is_some());
    }

    #[test]
    fn classify_parse_error_on_garbage() {
        let out = up_output("not json at all { garbled");
        let (state, value, detail) = classify_status_output(&out);
        assert_eq!(state, HostState::ParseError);
        assert!(value.is_none());
        assert!(detail.unwrap().contains("could not parse"));
    }

    #[test]
    fn classify_lenient_on_field_reduced_older_payload() {
        // An older remote binary's status --json with only a subset of keys
        // must still classify as Up, not ParseError.
        let out = up_output(r#"{"in_flight": []}"#);
        let (state, value, _detail) = classify_status_output(&out);
        assert_eq!(state, HostState::Up);
        assert_eq!(field_str(&value, &["in_flight_count"]), "-");
    }

    #[test]
    fn classify_unreachable_on_empty_stdout() {
        let out = CommandOutput {
            code: 255,
            stdout: String::new(),
            stderr: "ssh: connect to host worker-1 port 22: Connection refused".to_string(),
        };
        let (state, value, detail) = classify_status_output(&out);
        assert_eq!(state, HostState::Unreachable);
        assert!(value.is_none());
        assert!(detail.unwrap().contains("Connection refused"));
    }

    // ---- classify_expected_power_off (#4697) ------------------------------

    #[test]
    fn expected_power_off_when_silence_exceeds_idle_shutdown_window() {
        let mut w = worker("worker-1");
        w.idle_shutdown_minutes = Some(30);
        let last_seen = Utc::now() - chrono::Duration::minutes(45);
        w.last_seen_up_at = Some(last_seen.to_rfc3339());

        let result = classify_expected_power_off(&w, Utc::now());
        let (state, detail) = result.expect("45m silence should exceed a 30m window");
        assert_eq!(state, HostState::PoweredOff);
        assert!(detail.contains("30m"));
        assert!(detail.contains("EXPECTED power-off"));
    }

    #[test]
    fn stays_unreachable_when_silence_well_within_idle_shutdown_window() {
        let mut w = worker("worker-1");
        w.idle_shutdown_minutes = Some(30);
        let last_seen = Utc::now() - chrono::Duration::minutes(5);
        w.last_seen_up_at = Some(last_seen.to_rfc3339());

        // Silent for only 5m against a 30m window — too early to call this
        // "expected"; the conservative default (None => leave state alone).
        assert!(classify_expected_power_off(&w, Utc::now()).is_none());
    }

    #[test]
    fn stays_unreachable_when_no_idle_shutdown_configured() {
        let mut w = worker("worker-1");
        w.idle_shutdown_minutes = None;
        w.last_seen_up_at = Some((Utc::now() - chrono::Duration::minutes(999)).to_rfc3339());
        assert!(classify_expected_power_off(&w, Utc::now()).is_none());
    }

    #[test]
    fn stays_unreachable_when_last_seen_up_at_unknown() {
        let mut w = worker("worker-1");
        w.idle_shutdown_minutes = Some(30);
        w.last_seen_up_at = None;
        assert!(classify_expected_power_off(&w, Utc::now()).is_none());
    }

    // ---- update_last_seen_up_at (#4697) ------------------------------------

    #[test]
    fn update_last_seen_up_at_touches_only_up_remote_hosts() {
        let mut registry = FleetRegistry::default();
        registry.upsert(worker("up-host"));
        registry.upsert(worker("down-host"));

        let report = FleetStatusReport {
            hosts: vec![
                HostReport::local_up(serde_json::json!({})),
                HostReport {
                    alias: "up-host".to_string(),
                    tailnet_name: None,
                    provider_instance_id: None,
                    added_by: None,
                    is_local: false,
                    state: HostState::Up,
                    workspaces: Vec::new(),
                    status: None,
                    detail: None,
                    safehoused: SafehousedProbe::Unknown,
                },
                HostReport {
                    alias: "down-host".to_string(),
                    tailnet_name: None,
                    provider_instance_id: None,
                    added_by: None,
                    is_local: false,
                    state: HostState::Unreachable,
                    workspaces: Vec::new(),
                    status: None,
                    detail: None,
                    safehoused: SafehousedProbe::Unknown,
                },
            ],
            summary: summarize(&[], false),
            local_tailnet: None,
        };

        let now = Utc::now();
        let changed = update_last_seen_up_at(&mut registry, &report, now);
        assert!(changed);
        assert_eq!(registry.get("up-host").unwrap().last_seen_up_at, Some(now.to_rfc3339()));
        assert_eq!(registry.get("down-host").unwrap().last_seen_up_at, None);

        // A second call at the exact same timestamp is a no-op (nothing new
        // to persist).
        assert!(!update_last_seen_up_at(&mut registry, &report, now));
    }

    // ---- collect_remote_host / collect_fleet_report -----------------------

    #[tokio::test]
    async fn merges_up_daemon_down_unreachable_and_parse_error_hosts() {
        let mut responses = HashMap::new();
        responses.insert(
            "up-host".to_string(),
            MockOutcome::Output(up_output(r#"{"in_flight_count": 1}"#)),
        );
        responses.insert(
            "down-host".to_string(),
            MockOutcome::Output(up_output(r#"{"error": "could not reach loom-daemon"}"#)),
        );
        responses.insert("unreachable-host".to_string(), MockOutcome::LaunchErr);
        responses.insert("garbled-host".to_string(), MockOutcome::Output(up_output("garbage{{")));
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(responses));

        let mut registry = FleetRegistry::default();
        for host in ["up-host", "down-host", "unreachable-host", "garbled-host"] {
            registry.upsert(worker(host));
        }

        let local = HostReport::local_up(serde_json::json!({"in_flight_count": 0}));
        let report = collect_fleet_report(source, registry, local, Duration::from_secs(5)).await;

        assert_eq!(report.hosts.len(), 5); // local + 4 registered
        let by_alias = |alias: &str| report.hosts.iter().find(|h| h.alias == alias).unwrap();
        assert_eq!(by_alias(LOCAL_HOST_ALIAS).state, HostState::Up);
        assert_eq!(by_alias("up-host").state, HostState::Up);
        assert_eq!(by_alias("down-host").state, HostState::DaemonDown);
        assert_eq!(by_alias("unreachable-host").state, HostState::Unreachable);
        assert_eq!(by_alias("garbled-host").state, HostState::ParseError);

        assert_eq!(report.summary.total, 5);
        assert_eq!(report.summary.up, 2);
        assert_eq!(report.summary.daemon_down, 1);
        assert_eq!(report.summary.unreachable, 1);
        assert_eq!(report.summary.parse_error, 1);
        assert!(!report.summary.empty_roster);
    }

    // ---- all_tailnet_hosts_unreachable / LOCAL TAILNET DOWN banner (#4952) --

    fn host_report(alias: &str, tailnet_name: Option<&str>, state: HostState) -> HostReport {
        HostReport {
            alias: alias.to_string(),
            tailnet_name: tailnet_name.map(str::to_string),
            provider_instance_id: None,
            added_by: None,
            is_local: false,
            state,
            workspaces: Vec::new(),
            status: None,
            detail: None,
            safehoused: SafehousedProbe::Unknown,
        }
    }

    #[test]
    fn all_tailnet_hosts_unreachable_true_when_every_tailnet_host_is_unreachable() {
        let hosts = vec![
            HostReport::local_up(serde_json::json!({})),
            host_report("worker-1", Some("loom-worker-1"), HostState::Unreachable),
            host_report("worker-2", Some("loom-worker-2"), HostState::Unreachable),
        ];
        assert!(all_tailnet_hosts_unreachable(&hosts));
    }

    #[test]
    fn all_tailnet_hosts_unreachable_false_when_one_tailnet_host_is_up() {
        let hosts = vec![
            host_report("worker-1", Some("loom-worker-1"), HostState::Unreachable),
            host_report("worker-2", Some("loom-worker-2"), HostState::Up),
        ];
        assert!(!all_tailnet_hosts_unreachable(&hosts));
    }

    /// AC 3: a mixed roster where a plain LAN/SSH host (no tailnet address)
    /// is also unreachable must NOT be swallowed into the tailnet inference —
    /// its own unreachable state is excluded from (not required by) the
    /// trigger condition, so a genuine partial outage on that host still
    /// surfaces on its own merits elsewhere in the report.
    #[test]
    fn all_tailnet_hosts_unreachable_excludes_hosts_without_tailnet_address() {
        let hosts = vec![
            host_report("worker-1", Some("loom-worker-1"), HostState::Unreachable),
            host_report("lan-host", None, HostState::Unreachable),
        ];
        assert!(all_tailnet_hosts_unreachable(&hosts));

        // And a non-tailnet host that is merely Up doesn't change the
        // outcome either — it plays no part in the inference either way.
        let hosts2 = vec![
            host_report("worker-1", Some("loom-worker-1"), HostState::Unreachable),
            host_report("lan-host", None, HostState::Up),
        ];
        assert!(all_tailnet_hosts_unreachable(&hosts2));
    }

    #[test]
    fn all_tailnet_hosts_unreachable_false_when_no_tailnet_hosts_registered() {
        // Vacuous case: nothing tailnet-addressed to infer from at all —
        // must not spuriously trigger the self-check.
        let hosts = vec![
            HostReport::local_up(serde_json::json!({})),
            host_report("lan-host", None, HostState::Unreachable),
        ];
        assert!(!all_tailnet_hosts_unreachable(&hosts));
    }

    #[test]
    fn render_human_shows_local_tailnet_down_banner_and_names_restart_action() {
        let hosts = vec![
            HostReport::local_up(serde_json::json!({})),
            host_report("worker-1", Some("loom-worker-1"), HostState::Unreachable),
        ];
        let summary = summarize(&hosts, false);
        let report = FleetStatusReport {
            hosts,
            summary,
            local_tailnet: Some(LocalTailnetState::Down),
        };

        let rendered = report.render_human();
        assert!(rendered.contains("LOCAL TAILNET DOWN"));
        assert!(rendered.contains("tailscale up"));
        // The per-host UNREACHABLE row is still present underneath the
        // banner (AC 2 reframes it, it does not hide it).
        assert!(rendered.contains("UNREACHABLE"));
    }

    /// Regression guard: when the local tailnet check reports `Up` (or was
    /// never run, i.e. `None`), rendering is unchanged from before #4952 —
    /// no banner, plain UNREACHABLE reporting.
    #[test]
    fn render_human_omits_banner_when_local_tailnet_is_up_or_unchecked() {
        let hosts = vec![host_report(
            "worker-1",
            Some("loom-worker-1"),
            HostState::Unreachable,
        )];
        let base_summary = summarize(&hosts, false);

        let up_report = FleetStatusReport {
            hosts: hosts.clone(),
            summary: base_summary.clone(),
            local_tailnet: Some(LocalTailnetState::Up),
        };
        assert!(!up_report.render_human().contains("LOCAL TAILNET DOWN"));

        let unchecked_report = FleetStatusReport {
            hosts,
            summary: base_summary,
            local_tailnet: None,
        };
        assert!(!unchecked_report
            .render_human()
            .contains("LOCAL TAILNET DOWN"));
    }

    /// #4952 AC 4: empty roster + no tailnet hosts at all must not panic and
    /// must not spuriously trigger the self-check condition.
    #[test]
    fn empty_roster_never_triggers_tailnet_inference() {
        assert!(!all_tailnet_hosts_unreachable(&[HostReport::local_up(serde_json::json!({}))]));
        assert!(!all_tailnet_hosts_unreachable(&[]));
    }

    /// #4697: end-to-end through `collect_fleet_report`, an idle-shutdown-
    /// configured host silent past its window classifies as `PoweredOff`, and
    /// one silent well within its window stays `Unreachable`.
    #[tokio::test]
    async fn idle_shutdown_configured_host_past_window_is_powered_off_not_unreachable() {
        // Both hosts fail to answer SSH at all (a genuinely powered-off host
        // answers no differently than a genuinely dead one — the ONLY signal
        // available is elapsed silence vs. the configured window).
        let responses = HashMap::new();
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(responses));

        let mut registry = FleetRegistry::default();

        let mut past_window = worker("past-window-host");
        past_window.idle_shutdown_minutes = Some(30);
        past_window.last_seen_up_at =
            Some((Utc::now() - chrono::Duration::minutes(45)).to_rfc3339());
        registry.upsert(past_window);

        let mut within_window = worker("within-window-host");
        within_window.idle_shutdown_minutes = Some(30);
        within_window.last_seen_up_at =
            Some((Utc::now() - chrono::Duration::minutes(5)).to_rfc3339());
        registry.upsert(within_window);

        let local = HostReport::local_up(serde_json::json!({}));
        let report = collect_fleet_report(source, registry, local, Duration::from_secs(5)).await;

        let by_alias = |alias: &str| report.hosts.iter().find(|h| h.alias == alias).unwrap();
        let past = by_alias("past-window-host");
        assert_eq!(past.state, HostState::PoweredOff);
        assert!(past.detail.as_ref().unwrap().contains("EXPECTED power-off"));

        let within = by_alias("within-window-host");
        assert_eq!(within.state, HostState::Unreachable);

        assert_eq!(report.summary.powered_off, 1);
        assert_eq!(report.summary.unreachable, 1);
        // Still not a healthy fleet — an expected power-off is not "up".
        assert_ne!(report.exit_code(), 0);

        let rendered = report.render_human();
        assert!(rendered.contains("POWERED OFF"));
        assert!(rendered.contains("1 powered-off (expected)"));
        // The STATE label must not overflow render_human's {:<12} column, or
        // every remaining cell on that row shifts right and the table stops
        // being scannable.
        assert!(HostState::PoweredOff.label().len() <= 11);
    }

    #[tokio::test]
    async fn exit_code_zero_only_when_every_host_up() {
        let mut responses = HashMap::new();
        responses.insert("up-host".to_string(), MockOutcome::Output(up_output(r#"{}"#)));
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(responses));
        let mut registry = FleetRegistry::default();
        registry.upsert(worker("up-host"));
        let local = HostReport::local_up(serde_json::json!({}));
        let report =
            collect_fleet_report(source.clone(), registry, local, Duration::from_secs(5)).await;
        assert_eq!(report.exit_code(), 0);

        let mut responses2 = HashMap::new();
        responses2.insert("down-host".to_string(), MockOutcome::LaunchErr);
        let source2: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(responses2));
        let mut registry2 = FleetRegistry::default();
        registry2.upsert(worker("down-host"));
        let local2 = HostReport::local_up(serde_json::json!({}));
        let report2 =
            collect_fleet_report(source2, registry2, local2, Duration::from_secs(5)).await;
        assert_ne!(report2.exit_code(), 0);
    }

    #[tokio::test]
    async fn empty_roster_yields_local_only_row_and_explicit_notice() {
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(HashMap::new()));
        let registry = FleetRegistry::default();
        let local = HostReport::local_up(serde_json::json!({}));
        let report = collect_fleet_report(source, registry, local, Duration::from_secs(5)).await;

        assert_eq!(report.hosts.len(), 1);
        assert!(report.hosts[0].is_local);
        assert!(report.summary.empty_roster);

        // #5060: an empty roster must NEVER read as "all clear", even though
        // the only row present (the local host) is Up. A 1-of-1 all-up report
        // is indistinguishable from a genuinely healthy single-host fleet.
        assert_eq!(report.summary.up, report.summary.total);
        assert_ne!(report.exit_code(), 0);

        let rendered = report.render_human();
        assert!(rendered.contains("no fleet workers registered"));
        assert!(rendered.contains(LOCAL_HOST_ALIAS));
        // #5060 AC 2: the summary LINE ITSELF (not just the notice above the
        // table) is qualified as roster coverage, so it cannot be mistaken for
        // a healthy fleet by an operator/scraper reading only the last line.
        assert!(
            rendered.contains("1 of 1 known host(s) up"),
            "summary line not qualified as roster coverage: {rendered}"
        );
        assert!(rendered.contains("ROSTER IS EMPTY"));
        assert!(rendered.contains("NOT a healthy fleet"));
        assert!(
            !rendered.contains("1 host(s): 1 up"),
            "empty roster must not render the unqualified healthy-fleet summary line: {rendered}"
        );
    }

    /// #5060 non-regression: the empty-roster carve-out above must not make a
    /// genuinely populated, fully-up roster report unhealthy — that would
    /// invert the whole exit-code contract for every real fleet.
    #[tokio::test]
    async fn non_empty_all_up_roster_still_exits_zero_with_unqualified_summary() {
        let mut responses = HashMap::new();
        responses.insert("up-host".to_string(), MockOutcome::Output(up_output(r#"{}"#)));
        responses.insert("other-up-host".to_string(), MockOutcome::Output(up_output(r#"{}"#)));
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(responses));
        let mut registry = FleetRegistry::default();
        registry.upsert(worker("up-host"));
        registry.upsert(worker("other-up-host"));
        let local = HostReport::local_up(serde_json::json!({}));
        let report = collect_fleet_report(source, registry, local, Duration::from_secs(5)).await;

        assert!(!report.summary.empty_roster);
        assert_eq!(report.summary.total, 3);
        assert_eq!(report.summary.up, 3);
        assert_eq!(report.exit_code(), 0);

        let rendered = report.render_human();
        assert!(
            rendered.contains("3 host(s): 3 up"),
            "healthy fleet lost its unqualified summary line: {rendered}"
        );
        assert!(!rendered.contains("ROSTER IS EMPTY"));
        assert!(!rendered.contains("no fleet workers registered"));
    }

    #[tokio::test]
    async fn per_host_timeout_classifies_unreachable_without_stalling() {
        let mut responses = HashMap::new();
        responses.insert("slow-host".to_string(), MockOutcome::Hang(Duration::from_millis(500)));
        responses.insert("fast-host".to_string(), MockOutcome::Output(up_output(r#"{}"#)));
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(responses));

        let mut registry = FleetRegistry::default();
        registry.upsert(worker("slow-host"));
        registry.upsert(worker("fast-host"));

        let local = HostReport::local_up(serde_json::json!({}));
        let start = std::time::Instant::now();
        let report = collect_fleet_report(source, registry, local, Duration::from_millis(50)).await;
        let elapsed = start.elapsed();

        // Bounded by the per-host timeout, not the hang duration — the fast
        // host is not stalled by the slow one.
        assert!(elapsed < Duration::from_millis(400), "elapsed: {elapsed:?}");

        let slow = report
            .hosts
            .iter()
            .find(|h| h.alias == "slow-host")
            .unwrap();
        assert_eq!(slow.state, HostState::Unreachable);
        let fast = report
            .hosts
            .iter()
            .find(|h| h.alias == "fast-host")
            .unwrap();
        assert_eq!(fast.state, HostState::Up);
    }

    #[tokio::test]
    async fn draining_registry_state_renders_distinctly_without_ssh_probe() {
        let source: Arc<dyn HostStatusSource> = Arc::new(MockSource::new(HashMap::new()));
        let mut registry = FleetRegistry::default();
        let mut draining_worker = worker("draining-host");
        draining_worker.state = Some("draining".to_string());
        registry.upsert(draining_worker);

        let local = HostReport::local_up(serde_json::json!({}));
        let report = collect_fleet_report(source, registry, local, Duration::from_secs(5)).await;

        let host = report
            .hosts
            .iter()
            .find(|h| h.alias == "draining-host")
            .unwrap();
        assert_eq!(host.state, HostState::Draining);
        assert!(host.detail.as_ref().unwrap().contains("drain"));
        assert_eq!(report.summary.draining, 1);
        // A draining host still fails the exit-code policy (AC 3) — it is
        // not "up".
        assert_ne!(report.exit_code(), 0);

        let rendered = report.render_human();
        assert!(rendered.contains("DRAINING"));
    }

    #[test]
    fn field_str_renders_dash_for_missing_or_null() {
        assert_eq!(field_str(&None, &["x"]), "-");
        let v = Some(serde_json::json!({"a": {"b": 3}}));
        assert_eq!(field_str(&v, &["a", "b"]), "3");
        assert_eq!(field_str(&v, &["a", "missing"]), "-");
        let v2 = Some(serde_json::json!({"a": null}));
        assert_eq!(field_str(&v2, &["a"]), "-");
    }

    #[test]
    fn local_down_row_is_daemon_down_not_unreachable() {
        // The local host is never "unreachable" (it is local) — a down daemon
        // there is DaemonDown.
        let report = HostReport::local_down("daemon not running".to_string());
        assert_eq!(report.state, HostState::DaemonDown);
        assert!(report.is_local);
    }

    // ---- run_ssh PATH bootstrap (#4831) --------------------------------

    /// [`run_ssh`] must prefix EVERY remote command with the canonical PATH
    /// export line (#4831) — `ssh host '<cmd>'` runs `<cmd>` non-interactively
    /// and non-login, so a bare `loom-daemon status --json` is not guaranteed
    /// to resolve on a clean remote PATH. Verified end-to-end against a stub
    /// `ssh` on PATH (rather than asserting on private string-building logic)
    /// so this fails if the prefix is ever dropped from the actual spawned
    /// command, not just from an internal helper.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial(status_rs_stub_ssh_path)]
    fn run_ssh_prefixes_canonical_path_export() {
        use std::os::unix::fs::PermissionsExt;

        let stub_dir = tempfile::tempdir().unwrap();
        let stub_ssh = stub_dir.path().join("ssh");
        // Echo the LAST argv (the remote command ssh would have run) to
        // stdout, verbatim -- this is exactly what `run_ssh` constructs and
        // passes to the real `ssh` binary.
        std::fs::write(&stub_ssh, "#!/bin/sh\neval \"last=\\$$#\"\necho \"$last\"\n").unwrap();
        let mut perms = std::fs::metadata(&stub_ssh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub_ssh, perms).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", stub_dir.path().display()));

        let result = run_ssh("worker-stub", "loom-daemon status --json", 5);

        std::env::set_var("PATH", old_path);

        let out = result.unwrap();
        assert!(out.ok(), "stub ssh must succeed: {out:?}");
        assert!(
            out.stdout.contains("export PATH=\"${HOME}/.local/bin:${HOME}/.cargo/bin"),
            "run_ssh must prefix the remote command with the canonical PATH export (#4831), got: {}",
            out.stdout
        );
        assert!(
            out.stdout.contains("loom-daemon status --json"),
            "run_ssh must still send the original remote command after the PATH prefix, got: {}",
            out.stdout
        );
    }
}
