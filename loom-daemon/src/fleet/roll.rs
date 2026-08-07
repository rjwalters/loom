//! `loom-daemon fleet roll [<host>|--all]` (issue #5504, epic #4340): roll the
//! `loom-daemon` binary across fleet hosts, with a **measured** success
//! verdict rather than a trusted transcript.
//!
//! # The insight this module exists to encode
//!
//! Two obvious signals are NOT trustworthy evidence that a roll took:
//!
//! - `loom-daemon --version` **execs the binary**, so it reports whatever was
//!   last *built* on disk — even when the running process is hours older and
//!   still serving the old code (#5467).
//! - The update script's own transcript/exit code is not conclusive either: a
//!   real rollout printed `RELAUNCH NOT CONFIRMED` yet had, in fact,
//!   succeeded (#5390) — the script's own uncertainty about its supervisor
//!   handoff does not mean the roll failed.
//!
//! The one reliable test is comparing the **process start time** (when the
//! currently-running `loom-daemon` was launched — `systemctl --user show
//! loom-daemon.service -p ActiveEnterTimestamp` on Linux, `launchctl` + `ps -o
//! lstart=` on macOS) against the **binary build time** (the installed
//! binary's mtime): if the process predates the binary, the restart did not
//! take, full stop — regardless of what the transcript or `--version` says.
//! [`compare_process_and_build_time`] is that comparison, and it is the ONLY
//! thing [`execute_roll`] trusts for its final verdict.
//!
//! # Not a new rebuild engine — SSH orchestration over `loom-daemon-update.sh`
//!
//! Mirrors [`super::drain`]'s framing: the rebuild/provision/restart sequence
//! already exists and is well-tested (`defaults/scripts/cli/
//! loom-daemon-update.sh`, invoked the same way `fleet add-worker`'s
//! machine-layout step does, from the canonical `$HOME/.local/share/loom`
//! machine checkout). This module's job is the SSH fanout over that script
//! plus the pre/post measured-verdict wrapper:
//!
//! 1. **`--check`** (cheap, no writes) tells us whether the installed
//!    *binary* already matches source HEAD.
//! 2. A **pre-roll probe** measures the *running process's* start time vs the
//!    installed binary's build time — this is what catches the #5467 case
//!    where the binary is current but the process silently never picked it
//!    up.
//! 3. If the binary is already current AND the process already reflects it,
//!    the host is [`RollOutcome::AlreadyCurrent`] — no further remote calls.
//!    If the binary is current but the PROCESS is stale (the #5467 bug), the
//!    update script is invoked with `--force` to force a restart even though
//!    nothing needs rebuilding. Otherwise it runs with no flag (the normal,
//!    needed rebuild).
//! 4. A **post-roll probe** re-measures the same comparison. This — not the
//!    script's exit code — is the final verdict.
//!
//! # Interrupt safety (#5340's lesson, generalized)
//!
//! Once the update step is launched on the remote host, this module never
//! sends it a signal for any reason, including a local timeout: the binary
//! may already be swapped on disk with the old process still serving
//! requests, and interrupting the sequence there is worse than a slow
//! rollout (the exact mistake the issue's own incident report describes). A
//! per-host [`tokio::time::timeout`] bounds how long the *orchestrator*
//! waits for a host in `--all` fanout mode, mirroring [`super::status`]'s
//! "one hung host cannot stall the report" discipline — but a timeout here
//! only stops **waiting** locally; it never kills the underlying SSH child,
//! and the host's outcome is reported as [`RollOutcome::Unreachable`] (a
//! measured-uncertain state, not a claimed success or failure) rather than
//! folded into either.

use anyhow::Result;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

use super::path_bootstrap;
use super::{CommandOutput, CommandRunner, FleetRegistry};

/// Default per-host bound on how long the orchestrator waits for one host's
/// full roll sequence (check + probe + update + probe) to finish locally.
/// Generous by design (#5340's lesson: a slow roll is not a hang) — a
/// `cargo build --release` fallback (no matching release artifact) can take
/// several minutes; this never signals the remote process on expiry (see the
/// module doc's "Interrupt safety" section).
pub const DEFAULT_ROLL_TIMEOUT_SECS: u64 = 1800;

/// The canonical machine-level Loom checkout `fleet add-worker` clones to and
/// `loom-daemon-update.sh` is invoked from (mirrors
/// `add_worker::render_machine_layout`'s `LOOM_SRC`).
const MACHINE_LOOM_SRC: &str = "$HOME/.local/share/loom";

// ===========================================================================
// The measured verdict (the module's core insight)
// ===========================================================================

/// The result of comparing a running process's start time against its
/// binary's build time — the ONLY signal [`execute_roll`] trusts for its
/// final verdict (never `--version`, never the update script's own exit
/// code/transcript).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeVerdict {
    /// The process started at or after the binary was built — it is running
    /// (at least) the code on disk.
    Rolled,
    /// The process started before the binary was built — a stale process is
    /// still serving requests even though a newer binary is installed.
    NotYetRolled,
    /// Either timestamp could not be measured/parsed. Deliberately its own
    /// variant (never silently folded into `Rolled`): an ambiguous host must
    /// never read as a confirmed success.
    Unchecked,
}

/// Compare a process-start epoch against a binary-build epoch (both Unix
/// seconds, as rendered by [`VERDICT_PROBE_SHELL`]). `None` on either side
/// (unparseable/absent — no `loom-daemon` process found, no resolvable
/// binary, or a `date`/`stat` invocation that failed on an unrecognized
/// remote shell) yields [`TimeVerdict::Unchecked`], never a guessed verdict.
#[must_use]
pub fn compare_process_and_build_time(
    process_start_epoch: Option<i64>,
    build_time_epoch: Option<i64>,
) -> TimeVerdict {
    match (process_start_epoch, build_time_epoch) {
        (Some(p), Some(b)) if p >= b => TimeVerdict::Rolled,
        (Some(_), Some(_)) => TimeVerdict::NotYetRolled,
        _ => TimeVerdict::Unchecked,
    }
}

/// Parse the two `LOOM_ROLL_PROCESS_START=<epoch|>` / `LOOM_ROLL_BUILD_TIME=
/// <epoch|>` marker lines [`VERDICT_PROBE_SHELL`] prints, tolerantly: an
/// empty value, a missing marker, or trailing noise on the line all degrade
/// to `None` rather than erroring the whole parse.
#[must_use]
pub fn parse_verdict_probe(stdout: &str) -> (Option<i64>, Option<i64>) {
    let mut process_start = None;
    let mut build_time = None;
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("LOOM_ROLL_PROCESS_START=") {
            process_start = v.trim().parse::<i64>().ok();
        } else if let Some(v) = line.strip_prefix("LOOM_ROLL_BUILD_TIME=") {
            build_time = v.trim().parse::<i64>().ok();
        }
    }
    (process_start, build_time)
}

/// Remote shell probe: prints the running `loom-daemon` process's start time
/// and the resolved binary's build time (mtime), both as Unix epoch seconds
/// (computed remotely via `date`/`stat` so this module never has to parse a
/// locale-dependent timestamp format in Rust). Linux-first (`systemctl
/// --user show … ActiveEnterTimestamp`, per the fleet's Linux-only worker
/// policy, #5395), with a `launchctl`/`ps` fallback for a macOS target (an
/// operator's own box, not necessarily a `fleet add-worker`-provisioned
/// worker) and a `pgrep`/`ps -o lstart=` fallback on either OS when no
/// service manager entry is found. Never fails the whole probe: an
/// unresolvable value renders as an empty marker value, parsed as `None` by
/// [`parse_verdict_probe`].
const VERDICT_PROBE_SHELL: &str = r#"set +e
os="$(uname -s 2>/dev/null)"

binary=""
if command -v loom-daemon >/dev/null 2>&1; then
  binary="$(command -v loom-daemon)"
else
  for c in "$HOME/.cargo/bin/loom-daemon" "$HOME/.local/bin/loom-daemon" "/usr/local/bin/loom-daemon"; do
    if [ -x "$c" ]; then binary="$c"; break; fi
  done
fi

build_epoch=""
if [ -n "$binary" ]; then
  build_epoch="$(stat -c %Y "$binary" 2>/dev/null || stat -f %m "$binary" 2>/dev/null)"
fi

process_epoch=""
if [ "$os" = "Darwin" ]; then
  label="$(launchctl list 2>/dev/null | awk '$3 ~ /loom-daemon/ {print $3; exit}')"
  pid=""
  if [ -n "$label" ]; then
    pid="$(launchctl list "$label" 2>/dev/null | awk -F'= ' '/"PID"/{gsub(";","",$2); print $2}')"
  fi
  if [ -z "$pid" ]; then
    pid="$(pgrep -f loom-daemon 2>/dev/null | head -n1)"
  fi
  if [ -n "$pid" ]; then
    lstart="$(ps -o lstart= -p "$pid" 2>/dev/null)"
    if [ -n "$lstart" ]; then
      process_epoch="$(date -j -f "%a %b %d %T %Y" "$lstart" "+%s" 2>/dev/null)"
    fi
  fi
else
  ts=""
  if systemctl --user show loom-daemon.service >/dev/null 2>&1; then
    ts="$(systemctl --user show loom-daemon.service -p ActiveEnterTimestamp --value 2>/dev/null)"
  fi
  if [ -n "$ts" ] && [ "$ts" != "n/a" ]; then
    process_epoch="$(date -d "$ts" "+%s" 2>/dev/null)"
  fi
  if [ -z "$process_epoch" ]; then
    pid="$(pgrep -f loom-daemon 2>/dev/null | head -n1)"
    if [ -n "$pid" ]; then
      lstart="$(ps -o lstart= -p "$pid" 2>/dev/null)"
      if [ -n "$lstart" ]; then
        process_epoch="$(date -d "$lstart" "+%s" 2>/dev/null)"
      fi
    fi
  fi
fi

echo "LOOM_ROLL_PROCESS_START=${process_epoch}"
echo "LOOM_ROLL_BUILD_TIME=${build_epoch}"
"#;

/// Render an invocation of the machine checkout's `loom-daemon-update.sh`
/// with an optional trailing flag (`--check`, `--force`, or empty for a plain
/// run), prefixed with the canonical PATH export (#4831) and a best-effort
/// cargo env source — the same preamble `add_worker::render_machine_layout`
/// uses to invoke this script.
fn update_script_invocation(flag: &str) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let flag_suffix = if flag.is_empty() {
        String::new()
    } else {
        format!(" {flag}")
    };
    format!(
        "{export_line}. \"$HOME/.cargo/env\" 2>/dev/null || true\n\
         LOOM_SRC=\"{MACHINE_LOOM_SRC}\"\n\
         \"$LOOM_SRC/defaults/scripts/cli/loom-daemon-update.sh\"{flag_suffix}\n"
    )
}

// ===========================================================================
// Per-host outcome + report
// ===========================================================================

/// A single host's roll outcome. Deliberately loud + distinct (mirrors
/// [`super::status::HostState`]'s "silence never reads as idle" discipline) —
/// exactly the four states the issue's acceptance criteria ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum RollOutcome {
    /// The host was rolled and the post-roll measured verdict confirms it:
    /// the running process's start time is at or after the installed
    /// binary's build time.
    Rolled(String),
    /// The binary already matched source HEAD (`--check` exit 0) AND the
    /// pre-roll measured verdict already confirmed the running process
    /// reflects it — no remote mutation was needed.
    AlreadyCurrent(String),
    /// The host was reachable throughout, but the measured verdict never
    /// confirmed a successful roll (the process still predates the binary
    /// after the update ran, or a timestamp could not be measured/parsed).
    /// Never inferred from the update script's exit code/transcript alone.
    Failed(String),
    /// SSH could not reach the host at some point in the sequence, or the
    /// orchestrator's own per-host wait timed out. An in-flight remote
    /// update is NEVER signaled on this path — the host's true state is
    /// simply unknown until re-checked (never folded into success OR
    /// failure).
    Unreachable(String),
}

impl RollOutcome {
    /// Fixed label for the human table / `--json` twin — exactly the four
    /// strings the issue's acceptance criteria specify.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            RollOutcome::Rolled(_) => "ROLLED",
            RollOutcome::AlreadyCurrent(_) => "ALREADY CURRENT",
            RollOutcome::Failed(_) => "FAILED",
            RollOutcome::Unreachable(_) => "UNREACHABLE",
        }
    }

    /// Whether this outcome counts as a healthy roll for the exit-code
    /// policy: `Rolled` and `AlreadyCurrent` both mean "this host is
    /// confirmed running current code" — `Failed`/`Unreachable` do not.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, RollOutcome::Rolled(_) | RollOutcome::AlreadyCurrent(_))
    }

    /// The detail string carried by any variant.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            RollOutcome::Rolled(d)
            | RollOutcome::AlreadyCurrent(d)
            | RollOutcome::Failed(d)
            | RollOutcome::Unreachable(d) => d,
        }
    }
}

/// One host's roll report.
#[derive(Debug, Clone, Serialize)]
pub struct HostRollReport {
    pub host: String,
    pub outcome: RollOutcome,
}

/// Aggregate report across every targeted host.
#[derive(Debug, Clone, Serialize)]
pub struct RollReport {
    pub hosts: Vec<HostRollReport>,
    /// `true` when `--all` was given and the fleet registry had zero
    /// workers — mirrors [`super::status::FleetStatusSummary::empty_roster`]
    /// (#5060): "nothing to roll" must never read as "fleet confirmed
    /// current".
    pub empty_roster: bool,
}

impl RollReport {
    /// Exit-code policy (AC: non-zero when any host failed or could not be
    /// checked): `0` only when every targeted host is `Rolled` or
    /// `AlreadyCurrent`, AND at least one host was actually targeted.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.empty_roster || self.hosts.is_empty() {
            return 1;
        }
        if self.hosts.iter().all(|h| h.outcome.is_success()) {
            0
        } else {
            1
        }
    }

    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str("fleet roll report:\n");
        if self.empty_roster {
            out.push_str(
                "  (no fleet workers registered — nothing to roll; add one with \
                 `loom-daemon fleet add-worker <ssh-host> --repo <owner/name>`, or roll a \
                 specific host with `loom-daemon fleet roll <ssh-host>`)\n",
            );
            return out;
        }
        let mut rolled = 0;
        let mut already_current = 0;
        let mut failed = 0;
        let mut unreachable = 0;
        for h in &self.hosts {
            match &h.outcome {
                RollOutcome::Rolled(_) => rolled += 1,
                RollOutcome::AlreadyCurrent(_) => already_current += 1,
                RollOutcome::Failed(_) => failed += 1,
                RollOutcome::Unreachable(_) => unreachable += 1,
            }
            out.push_str(&format!("  [{}] {}\n", h.outcome.label(), h.host));
            out.push_str(&format!("      -> {}\n", h.outcome.detail()));
        }
        out.push_str(&format!(
            "\n{} host(s): {} rolled, {} already current, {} failed, {} unreachable.\n",
            self.hosts.len(),
            rolled,
            already_current,
            failed,
            unreachable
        ));
        out
    }
}

// ===========================================================================
// Per-host execution (pure-ish: driven entirely off the `CommandRunner` seam)
// ===========================================================================

/// Whether a [`CommandOutput`] from an `ssh <host> …` invocation looks like an
/// ssh-*transport*-level failure (connection refused, DNS failure, auth
/// failure, killed by signal before a remote session was ever established)
/// rather than a legitimate exit code from the remote script.
///
/// `ssh(1)`'s own contract is the basis for this: it "exits with the exit
/// status of the remote command, or with 255 if an error occurred" — and
/// every real exit code `loom-daemon-update.sh` uses is documented and small
/// (`0`/`1`/`3`/`4`/`5`/`6`/`7`/`8`; see the script's own `--help`), so a
/// bare `255` (or a negative code — [`super::CommandOutput::code`]'s
/// signal-death sentinel) is unambiguous. This matters because the script's
/// own "update available" `--check` response is entirely on **stderr**
/// (`warn(...)` before `exit 3`) — unlike `status --json`'s always-populated
/// stdout, an empty-stdout heuristic (à la
/// [`super::status::classify_status_output`]) would misclassify that
/// legitimate response as unreachable.
#[must_use]
fn ssh_transport_failure(out: &CommandOutput) -> bool {
    out.code == 255 || out.code < 0
}

/// Execute the full measured-roll sequence against one host: `--check` ->
/// pre-roll probe -> (skip, or update with/without `--force`) -> post-roll
/// probe -> verdict. Every step goes through `runner`, so this is fully unit
/// testable with a scripted mock (mirrors [`super::drain::execute_drain`]'s
/// shape) — no live SSH/network required.
#[must_use]
pub fn execute_roll(runner: &dyn CommandRunner, host: &str) -> RollOutcome {
    // 1. --check: does the installed BINARY already match source HEAD?
    let check_out = match runner.run(&update_script_invocation("--check"), None) {
        Ok(out) if ssh_transport_failure(&out) => {
            return RollOutcome::Unreachable(format!(
                "could not reach {host} to check update status (ssh exited {}): {}",
                out.code,
                tail(&out.stderr)
            ))
        }
        Ok(out) => out,
        Err(e) => {
            return RollOutcome::Unreachable(format!(
                "could not reach {host} to check update status: {e}"
            ))
        }
    };
    // `--check` exits 0 (up to date) or 3 (update available); any other exit
    // (including a transport-level failure surfaced as a nonzero code, e.g.
    // the now-fixed #5457 dirty-config block) is treated conservatively as
    // "not confirmed current" so the sequence still attempts a real roll
    // rather than silently skipping one.
    let binary_up_to_date = check_out.code == 0;

    // 2. Pre-roll measured verdict — this is what catches the #5467 case
    // where the binary already matches HEAD but the running process never
    // picked it up.
    let pre_probe = match runner.run(VERDICT_PROBE_SHELL, None) {
        Ok(out) if ssh_transport_failure(&out) => {
            return RollOutcome::Unreachable(format!(
                "could not reach {host} to measure the pre-roll process/build times (ssh exited \
                 {}): {}",
                out.code,
                tail(&out.stderr)
            ))
        }
        Ok(out) => out,
        Err(e) => {
            return RollOutcome::Unreachable(format!(
                "could not reach {host} to measure the pre-roll process/build times: {e}"
            ))
        }
    };
    let (pre_process, pre_build) = parse_verdict_probe(&pre_probe.stdout);
    let pre_verdict = compare_process_and_build_time(pre_process, pre_build);

    if binary_up_to_date && pre_verdict == TimeVerdict::Rolled {
        return RollOutcome::AlreadyCurrent(
            "binary already matches source HEAD, and the running process's start time is \
             already at or after the binary's build time — nothing to do."
                .to_string(),
        );
    }

    // 3. Run the update. If the binary is already current but the PROCESS is
    // stale (the exact #5467 bug), force a restart even though nothing needs
    // rebuilding; otherwise a plain run performs whatever rebuild is needed.
    let force = binary_up_to_date && pre_verdict != TimeVerdict::Rolled;
    let update_flag = if force { "--force" } else { "" };
    let update_out = match runner.run(&update_script_invocation(update_flag), None) {
        Ok(out) if ssh_transport_failure(&out) => {
            return RollOutcome::Unreachable(format!(
                "{host} became unreachable while the update was running (ssh exited {}) — its \
                 state is now UNKNOWN (the binary may already be swapped without a confirmed \
                 restart); re-run `fleet roll {host}` to verify before assuming failure or \
                 success: {}",
                out.code,
                tail(&out.stderr)
            ))
        }
        Ok(out) => out,
        Err(e) => {
            return RollOutcome::Unreachable(format!(
                "{host} became unreachable while the update was running — its state is now \
                 UNKNOWN (the binary may already be swapped without a confirmed restart); \
                 re-run `fleet roll {host}` to verify before assuming failure or success: {e}"
            ))
        }
    };

    // 4. Post-roll measured verdict — the ONLY thing trusted for the final
    // call, never `update_out`'s exit code or stdout/stderr transcript.
    let post_probe = match runner.run(VERDICT_PROBE_SHELL, None) {
        Ok(out) if ssh_transport_failure(&out) => {
            return RollOutcome::Unreachable(format!(
                "the update step on {host} exited {} but the host became unreachable (ssh exited \
                 {}) before the post-roll verdict could be measured — re-run `fleet roll {host}` \
                 to verify: {}",
                update_out.code,
                out.code,
                tail(&out.stderr)
            ))
        }
        Ok(out) => out,
        Err(e) => {
            return RollOutcome::Unreachable(format!(
                "the update step on {host} exited {} but the host became unreachable before the \
                 post-roll verdict could be measured — re-run `fleet roll {host}` to verify: {e}",
                update_out.code
            ))
        }
    };
    let (post_process, post_build) = parse_verdict_probe(&post_probe.stdout);
    match compare_process_and_build_time(post_process, post_build) {
        TimeVerdict::Rolled => RollOutcome::Rolled(format!(
            "verified: the running process's start time is at or after the installed binary's \
             build time (update script exited {}{}).",
            update_out.code,
            if force {
                " — binary was already current; forced a restart because the running process was \
                 stale (#5467)"
                    .to_string()
            } else {
                String::new()
            }
        )),
        TimeVerdict::NotYetRolled => RollOutcome::Failed(format!(
            "update script exited {} but the running process STILL predates the installed \
             binary — the restart did not take. Never trust the transcript alone: {}",
            update_out.code,
            tail(&update_out.stderr)
        )),
        TimeVerdict::Unchecked => RollOutcome::Failed(format!(
            "could not determine the process-start/binary-build verdict on {host} after running \
             the update (exit {}) — inconclusive, never assumed successful.",
            update_out.code
        )),
    }
}

fn tail(s: &str) -> String {
    let line = s
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        "(no stderr output)".to_string()
    } else {
        line.to_string()
    }
}

// ===========================================================================
// Concurrent fanout (`--all`) — mirrors `status::collect_remote_host`
// ===========================================================================

/// Execute [`execute_roll`] against one host, bounded by `timeout`. On
/// expiry, this only stops **waiting** — it never signals the underlying SSH
/// child (see the module doc's "Interrupt safety" section) — and reports
/// [`RollOutcome::Unreachable`], never a guessed success or failure.
pub async fn roll_host(
    runner: Arc<dyn CommandRunner + Send + Sync>,
    host: String,
    timeout: Duration,
) -> HostRollReport {
    let task_host = host.clone();
    let handle = tokio::task::spawn_blocking(move || execute_roll(runner.as_ref(), &task_host));
    let outcome = match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join_err)) => {
            RollOutcome::Unreachable(format!("roll task for {host} did not complete: {join_err}"))
        }
        Err(_elapsed) => RollOutcome::Unreachable(format!(
            "timed out after {}s waiting locally for {host}'s roll to complete — the remote \
             update was never signaled and may still be in progress; re-run `fleet roll {host}` \
             to confirm before assuming failure or success.",
            timeout.as_secs()
        )),
    };
    HostRollReport { host, outcome }
}

/// Resolve the roll target(s) given `host`/`all`, run them (concurrently for
/// `--all`, mirroring [`super::status::collect_fleet_report`]'s fanout),
/// bounded per-host by `timeout`, and return the aggregate [`RollReport`].
pub async fn run(
    runner_factory: impl Fn(&str) -> Arc<dyn CommandRunner + Send + Sync>,
    host: Option<String>,
    all: bool,
    timeout: Duration,
) -> Result<RollReport> {
    let targets: Vec<String> = if all {
        let registry = FleetRegistry::load_default()?;
        // #5576: mirror `super::status::collect_fleet_report`'s local-host
        // dedup — a `--all` fanout enumerates the whole roster automatically
        // (unlike a single explicit `fleet roll <host>`, which is the
        // operator's own choice even if it happens to name this machine), so
        // an operator-supplied roster (`LOOM_FLEET_PATH`) that lists this
        // host's own SSH address must not turn into a redundant `ssh <self>`
        // roll attempt.
        let (local, remote): (Vec<String>, Vec<String>) = registry
            .workers
            .into_iter()
            .map(|w| w.ssh_host)
            .partition(|h| super::is_local_ssh_host(h));
        if !local.is_empty() {
            eprintln!(
                "note: skipping {} registered host(s) that resolve to this machine itself \
                 (already running whatever binary is on disk here, not reachable over SSH to \
                 itself): {}",
                local.len(),
                local.join(", "),
            );
        }
        remote
    } else if let Some(h) = host {
        vec![h]
    } else {
        Vec::new()
    };

    let empty_roster = all && targets.is_empty();

    let handles: Vec<_> = targets
        .into_iter()
        .map(|host| {
            let runner = runner_factory(&host);
            tokio::spawn(roll_host(runner, host, timeout))
        })
        .collect();

    let mut hosts = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(report) => hosts.push(report),
            Err(join_err) => hosts.push(HostRollReport {
                host: "<unknown>".to_string(),
                outcome: RollOutcome::Unreachable(format!("roll task panicked: {join_err}")),
            }),
        }
    }

    Ok(RollReport {
        hosts,
        empty_roster,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- compare_process_and_build_time / parse_verdict_probe -------------

    #[test]
    fn rolled_when_process_started_at_or_after_build() {
        assert_eq!(compare_process_and_build_time(Some(200), Some(100)), TimeVerdict::Rolled);
        // Exactly equal counts as rolled too (>=), not a boundary failure.
        assert_eq!(compare_process_and_build_time(Some(100), Some(100)), TimeVerdict::Rolled);
    }

    #[test]
    fn not_yet_rolled_when_process_predates_build() {
        assert_eq!(compare_process_and_build_time(Some(50), Some(100)), TimeVerdict::NotYetRolled);
    }

    #[test]
    fn unchecked_on_missing_or_unparseable_either_side() {
        assert_eq!(compare_process_and_build_time(None, Some(100)), TimeVerdict::Unchecked);
        assert_eq!(compare_process_and_build_time(Some(100), None), TimeVerdict::Unchecked);
        assert_eq!(compare_process_and_build_time(None, None), TimeVerdict::Unchecked);
    }

    #[test]
    fn parse_verdict_probe_reads_both_markers() {
        let stdout = "some noise\nLOOM_ROLL_PROCESS_START=1000\nLOOM_ROLL_BUILD_TIME=900\n";
        assert_eq!(parse_verdict_probe(stdout), (Some(1000), Some(900)));
    }

    #[test]
    fn parse_verdict_probe_tolerates_empty_values() {
        let stdout = "LOOM_ROLL_PROCESS_START=\nLOOM_ROLL_BUILD_TIME=\n";
        assert_eq!(parse_verdict_probe(stdout), (None, None));
    }

    #[test]
    fn parse_verdict_probe_tolerates_missing_markers() {
        assert_eq!(parse_verdict_probe("nothing useful here"), (None, None));
        assert_eq!(parse_verdict_probe(""), (None, None));
    }

    #[test]
    fn parse_verdict_probe_rejects_garbage_values() {
        let stdout = "LOOM_ROLL_PROCESS_START=not-a-number\nLOOM_ROLL_BUILD_TIME=900\n";
        assert_eq!(parse_verdict_probe(stdout), (None, Some(900)));
    }

    // ---- Mock command runner (mirrors `add_worker`/`drain`'s MockRunner) --

    // `Mutex`, not `RefCell` (unlike `add_worker`/`drain`'s sync-only mocks):
    // this module's `--all` fanout drives the runner through
    // `Arc<dyn CommandRunner + Send + Sync>` across a `tokio::spawn_blocking`
    // boundary, so the test double must be `Sync` too.
    struct MockRunner {
        responses: Mutex<Vec<Result<CommandOutput, String>>>,
        calls: Mutex<Vec<String>>,
    }

    impl MockRunner {
        fn new(responses: Vec<Result<CommandOutput, String>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, shell: &str, _stdin: Option<&str>) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push(shell.to_string());
            let mut r = self.responses.lock().unwrap();
            if r.is_empty() {
                return Ok(out(0, "", ""));
            }
            match r.remove(0) {
                Ok(o) => Ok(o),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    fn out(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn probe(process: i64, build: i64) -> CommandOutput {
        out(
            0,
            &format!("LOOM_ROLL_PROCESS_START={process}\nLOOM_ROLL_BUILD_TIME={build}\n"),
            "",
        )
    }

    // ---- execute_roll: the four outcomes -----------------------------------

    #[test]
    fn already_current_when_check_ok_and_process_already_reflects_it() {
        let runner = MockRunner::new(vec![
            Ok(out(0, "up to date", "")), // --check: exit 0
            Ok(probe(500, 400)),          // pre-probe: process newer than build -> Rolled
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "ALREADY CURRENT");
        assert!(outcome.is_success());
        // No update or post-probe call was made — only 2 remote calls total.
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn rolled_when_update_runs_and_post_probe_confirms() {
        let runner = MockRunner::new(vec![
            Ok(out(3, "update available", "")),      // --check: update needed
            Ok(probe(100, 200)), // pre-probe: stale (irrelevant, binary was stale)
            Ok(out(0, "rebuilt and restarted", "")), // update run (no --force expected)
            Ok(probe(500, 300)), // post-probe: now rolled
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "ROLLED");
        assert!(outcome.is_success());
        let calls = runner.calls();
        assert_eq!(calls.len(), 4);
        // The plain (non-forced) update path — binary was genuinely stale.
        assert!(!calls[2].contains("--force"), "calls[2]: {}", calls[2]);
    }

    #[test]
    fn forces_restart_when_binary_current_but_process_stale() {
        // The exact #5467 bug: --check says up to date, but the pre-probe
        // shows the running process predates the (already-current) binary.
        let runner = MockRunner::new(vec![
            Ok(out(0, "up to date", "")),     // --check: exit 0
            Ok(probe(100, 200)),              // pre-probe: process < build -> NotYetRolled
            Ok(out(0, "forced restart", "")), // update run, forced
            Ok(probe(500, 200)),              // post-probe: now rolled
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "ROLLED");
        let calls = runner.calls();
        assert!(calls[2].contains("--force"), "calls[2]: {}", calls[2]);
        assert!(outcome.detail().contains("forced a restart"), "detail: {}", outcome.detail());
    }

    #[test]
    fn failed_when_post_probe_still_shows_stale_process() {
        // The update script can claim success while the restart did not
        // actually take (#5390's "RELAUNCH NOT CONFIRMED" style ambiguity,
        // generalized): the measured verdict must override a green exit code.
        let runner = MockRunner::new(vec![
            Ok(out(3, "update available", "")),
            Ok(probe(100, 200)),
            Ok(out(0, "claims success", "")), // exit 0 — but...
            Ok(probe(150, 400)),              // ...process STILL predates the build.
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "FAILED");
        assert!(!outcome.is_success());
        assert!(outcome.detail().contains("did not take"), "detail: {}", outcome.detail());
    }

    #[test]
    fn failed_when_post_probe_is_unparseable() {
        let runner = MockRunner::new(vec![
            Ok(out(3, "", "")),
            Ok(probe(100, 200)),
            Ok(out(0, "", "")),
            Ok(out(0, "no markers at all", "")), // post-probe: unparseable
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "FAILED");
        assert!(outcome.detail().contains("inconclusive"), "detail: {}", outcome.detail());
    }

    #[test]
    fn unreachable_when_check_step_fails_to_launch() {
        let runner = MockRunner::new(vec![Err("ssh: connection refused".to_string())]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "UNREACHABLE");
        assert!(!outcome.is_success());
    }

    #[test]
    fn unreachable_when_ssh_itself_exits_255_not_folded_into_failed() {
        // #5504 AC: an ssh-transport-level failure (DNS/connect refused/auth)
        // surfaces as `Ok(CommandOutput{code: 255, ..})`, not `Err` — per
        // `CommandRunner::run`'s own documented contract, a non-zero remote
        // exit is a "successful launch". This must still classify as
        // UNREACHABLE, never FAILED (which would wrongly imply the script
        // itself ran and produced a bad verdict) and never a guessed success.
        let runner = MockRunner::new(vec![Ok(out(
            255,
            "",
            "ssh: Could not resolve hostname bogus-host: Name or service not known",
        ))]);
        let outcome = execute_roll(&runner, "bogus-host");
        assert_eq!(outcome.label(), "UNREACHABLE");
        assert!(!outcome.is_success());
        assert!(outcome.detail().contains("ssh exited 255"), "detail: {}", outcome.detail());
    }

    #[test]
    fn check_step_update_available_via_stderr_only_is_not_misread_as_unreachable() {
        // The real script's `--check` "update available" branch writes ONLY
        // to stderr (`warn(...)` before `exit 3`) — empty stdout here is
        // legitimate, not a transport failure. Must proceed normally, not
        // short-circuit to UNREACHABLE.
        let runner = MockRunner::new(vec![
            Ok(out(3, "", "Update available (installed=abc123, source=def456).")),
            Ok(probe(100, 200)),
            Ok(out(0, "rebuilt and restarted", "")),
            Ok(probe(500, 300)),
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "ROLLED");
    }

    #[test]
    fn unreachable_when_host_drops_mid_update() {
        let runner = MockRunner::new(vec![
            Ok(out(3, "", "")),
            Ok(probe(100, 200)),
            Err("ssh: broken pipe".to_string()),
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "UNREACHABLE");
        assert!(outcome.detail().contains("UNKNOWN"), "detail: {}", outcome.detail());
    }

    #[test]
    fn unreachable_when_host_drops_after_update_before_post_probe() {
        let runner = MockRunner::new(vec![
            Ok(out(3, "", "")),
            Ok(probe(100, 200)),
            Ok(out(0, "", "")),
            Err("ssh: connection reset".to_string()),
        ]);
        let outcome = execute_roll(&runner, "worker-1");
        assert_eq!(outcome.label(), "UNREACHABLE");
    }

    // ---- RollReport: exit code + rendering ---------------------------------

    #[test]
    fn exit_code_zero_only_when_every_host_succeeded() {
        let report = RollReport {
            hosts: vec![
                HostRollReport {
                    host: "a".into(),
                    outcome: RollOutcome::Rolled("ok".into()),
                },
                HostRollReport {
                    host: "b".into(),
                    outcome: RollOutcome::AlreadyCurrent("ok".into()),
                },
            ],
            empty_roster: false,
        };
        assert_eq!(report.exit_code(), 0);

        let failing = RollReport {
            hosts: vec![HostRollReport {
                host: "c".into(),
                outcome: RollOutcome::Failed("nope".into()),
            }],
            empty_roster: false,
        };
        assert_ne!(failing.exit_code(), 0);

        let unreachable = RollReport {
            hosts: vec![HostRollReport {
                host: "d".into(),
                outcome: RollOutcome::Unreachable("gone".into()),
            }],
            empty_roster: false,
        };
        assert_ne!(unreachable.exit_code(), 0);
    }

    #[test]
    fn empty_roster_never_exits_zero() {
        let report = RollReport {
            hosts: Vec::new(),
            empty_roster: true,
        };
        assert_ne!(report.exit_code(), 0);
        assert!(report
            .render_human()
            .contains("no fleet workers registered"));
    }

    #[test]
    fn render_human_lists_every_host_and_tallies_outcomes() {
        let report = RollReport {
            hosts: vec![
                HostRollReport {
                    host: "a".into(),
                    outcome: RollOutcome::Rolled("verified".into()),
                },
                HostRollReport {
                    host: "b".into(),
                    outcome: RollOutcome::Failed("stale".into()),
                },
            ],
            empty_roster: false,
        };
        let rendered = report.render_human();
        assert!(rendered.contains("[ROLLED] a"));
        assert!(rendered.contains("[FAILED] b"));
        assert!(rendered.contains("1 rolled, 0 already current, 1 failed, 0 unreachable"));
    }

    // ---- roll_host: per-host timeout never signals the remote ------------

    #[tokio::test]
    async fn per_host_timeout_reports_unreachable_without_hanging_the_fanout() {
        struct HangingRunner;
        impl CommandRunner for HangingRunner {
            fn run(&self, _shell: &str, _stdin: Option<&str>) -> Result<CommandOutput> {
                std::thread::sleep(Duration::from_millis(300));
                Ok(out(0, "", ""))
            }
        }
        let runner: Arc<dyn CommandRunner + Send + Sync> = Arc::new(HangingRunner);
        let start = std::time::Instant::now();
        let report = roll_host(runner, "slow-host".to_string(), Duration::from_millis(20)).await;
        assert!(start.elapsed() < Duration::from_millis(250), "must not wait for the hang");
        assert_eq!(report.outcome.label(), "UNREACHABLE");
        assert!(
            report.outcome.detail().contains("timed out"),
            "detail: {}",
            report.outcome.detail()
        );
    }

    #[tokio::test]
    async fn roll_host_reports_success_within_timeout() {
        let runner: Arc<dyn CommandRunner + Send + Sync> =
            Arc::new(MockRunner::new(vec![Ok(out(0, "up to date", "")), Ok(probe(500, 400))]));
        let report = roll_host(runner, "fast-host".to_string(), Duration::from_secs(5)).await;
        assert_eq!(report.outcome.label(), "ALREADY CURRENT");
    }

    // ---- run(): target resolution ------------------------------------------

    #[tokio::test]
    async fn run_targets_single_host_when_all_is_false() {
        let calls: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_clone = calls.clone();
        let report = run(
            move |host| {
                calls_clone.lock().unwrap().push(host.to_string());
                let r: Arc<dyn CommandRunner + Send + Sync> =
                    Arc::new(MockRunner::new(vec![Ok(out(0, "", "")), Ok(probe(500, 400))]));
                r
            },
            Some("worker-1".to_string()),
            false,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].host, "worker-1");
        assert_eq!(*calls.lock().unwrap(), vec!["worker-1".to_string()]);
    }

    #[tokio::test]
    async fn run_with_neither_host_nor_all_targets_nothing() {
        let report = run(
            |_host| {
                let r: Arc<dyn CommandRunner + Send + Sync> = Arc::new(MockRunner::new(vec![]));
                r
            },
            None,
            false,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(report.hosts.is_empty());
        assert!(!report.empty_roster); // only `--all` sets this flag
        assert_ne!(report.exit_code(), 0);
    }

    #[tokio::test]
    #[serial_test::serial(fleet_registry_path_env)]
    async fn run_all_targets_every_registered_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::env::set_var(super::super::FLEET_REGISTRY_PATH_ENV, &path);

        let mut registry = FleetRegistry::default();
        registry.upsert(super::super::WorkerRecord {
            ssh_host: "worker-1".to_string(),
            repos: Vec::new(),
            priority: 100,
            bootstrapped_at: "2026-08-01T00:00:00Z".to_string(),
            last_verify: None,
            provider_instance_id: None,
            tailnet_name: None,
            added_by: None,
            state: None,
            drain_phase: None,
            drain_captured: Vec::new(),
            idle_shutdown_minutes: None,
            last_seen_up_at: None,
        });
        registry.upsert(super::super::WorkerRecord {
            ssh_host: "worker-2".to_string(),
            repos: Vec::new(),
            priority: 100,
            bootstrapped_at: "2026-08-01T00:00:00Z".to_string(),
            last_verify: None,
            provider_instance_id: None,
            tailnet_name: None,
            added_by: None,
            state: None,
            drain_phase: None,
            drain_captured: Vec::new(),
            idle_shutdown_minutes: None,
            last_seen_up_at: None,
        });
        registry.save(&path).unwrap();

        let report = run(
            |_host| {
                let r: Arc<dyn CommandRunner + Send + Sync> =
                    Arc::new(MockRunner::new(vec![Ok(out(0, "", "")), Ok(probe(500, 400))]));
                r
            },
            None,
            true,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        std::env::remove_var(super::super::FLEET_REGISTRY_PATH_ENV);

        assert_eq!(report.hosts.len(), 2);
        assert!(!report.empty_roster);
        assert_eq!(report.exit_code(), 0);
    }

    /// #5576: `--all` must skip a registered host that resolves to the local
    /// machine itself (e.g. an operator-supplied `LOOM_FLEET_PATH` roster
    /// that names this host's own SSH address) rather than dialing it — a
    /// single explicit `fleet roll <host>` naming the same host is untouched
    /// (that is an explicit operator choice, not an auto-enumerated one).
    #[tokio::test]
    #[serial_test::serial(fleet_registry_path_env)]
    async fn run_all_skips_registered_host_that_resolves_to_local_machine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::env::set_var(super::super::FLEET_REGISTRY_PATH_ENV, &path);

        let mut registry = FleetRegistry::default();
        registry.upsert(super::super::WorkerRecord {
            ssh_host: "worker-1".to_string(),
            repos: Vec::new(),
            priority: 100,
            bootstrapped_at: "2026-08-01T00:00:00Z".to_string(),
            last_verify: None,
            provider_instance_id: None,
            tailnet_name: None,
            added_by: None,
            state: None,
            drain_phase: None,
            drain_captured: Vec::new(),
            idle_shutdown_minutes: None,
            last_seen_up_at: None,
        });
        registry.upsert(super::super::WorkerRecord {
            ssh_host: "localhost".to_string(),
            repos: Vec::new(),
            priority: 100,
            bootstrapped_at: "2026-08-01T00:00:00Z".to_string(),
            last_verify: None,
            provider_instance_id: None,
            tailnet_name: None,
            added_by: None,
            state: None,
            drain_phase: None,
            drain_captured: Vec::new(),
            idle_shutdown_minutes: None,
            last_seen_up_at: None,
        });
        registry.save(&path).unwrap();

        let factory_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_for_factory = factory_calls.clone();
        let report = run(
            move |host| {
                calls_for_factory.lock().unwrap().push(host.to_string());
                let r: Arc<dyn CommandRunner + Send + Sync> =
                    Arc::new(MockRunner::new(vec![Ok(out(0, "", "")), Ok(probe(500, 400))]));
                r
            },
            None,
            true,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        std::env::remove_var(super::super::FLEET_REGISTRY_PATH_ENV);

        // Only the real remote host was ever dialed — the local duplicate
        // never got as far as constructing a runner for it.
        assert_eq!(*factory_calls.lock().unwrap(), vec!["worker-1".to_string()]);
        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].host, "worker-1");
        assert!(!report.empty_roster);
    }

    #[tokio::test]
    #[serial_test::serial(fleet_registry_path_env)]
    async fn run_all_on_empty_registry_reports_empty_roster() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::env::set_var(super::super::FLEET_REGISTRY_PATH_ENV, &path);

        let report = run(
            |_host| {
                let r: Arc<dyn CommandRunner + Send + Sync> = Arc::new(MockRunner::new(vec![]));
                r
            },
            None,
            true,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        std::env::remove_var(super::super::FLEET_REGISTRY_PATH_ENV);

        assert!(report.hosts.is_empty());
        assert!(report.empty_roster);
        assert_ne!(report.exit_code(), 0);
    }
}
