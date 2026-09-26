//! `loom-daemon fleet-captain <job-name>` — the shell-facing half of the
//! fleet singleton-job captain gate (#8848).
//!
//! # Why a CLI surface exists at all
//!
//! [`loom_daemon::fleet_captain`] is the mechanism, but its callers are not
//! all in-process. The motivating consumer is a **schedule wrapper**: a
//! launchd/systemd timer, or a shell script installed on every fleet host,
//! that must run its periodic check on exactly one of them. Before this, each
//! such wrapper hand-rolled its own fail-closed host gate (2AMLogic/2am's
//! `batch-fleet-reconcile-schedule.sh` and `loom-wake-pull-schedule.sh` do;
//! others simply did not, which is how `loom-worker-2` ended up carrying two
//! singleton timers `loom-worker-1` lacked — 2AMLogic/2am#1125).
//!
//! This subcommand is the one gate they all call instead, so "exactly one
//! host runs this" stops being a per-wrapper reimplementation and becomes a
//! single declared fact (`fleet.captain` in the tracked `.loom/config.json`)
//! that every host reads identically.
//!
//! # Usage
//!
//! ```sh
//! # At the top of a singleton job's wrapper — refuse (quietly, exit 0 from
//! # the wrapper's own perspective) on every host that is not the captain:
//! loom-daemon fleet-captain forge-queue-check || exit 0
//!
//! # …or distinguish "someone else owns this" from "nobody does":
//! loom-daemon fleet-captain forge-queue-check
//! case $? in
//!   0) ;;                                   # this host is the captain — run
//!   3) exit 0 ;;                            # another host is the captain
//!   4) echo "no fleet.captain declared" >&2; exit 1 ;;   # misconfigured
//! esac
//! ```
//!
//! # Exit-code contract
//!
//! | Exit | Meaning | Output |
//! |---|---|---|
//! | `0` | This host **is** the declared captain — the job may run | stderr: the arm message, unless `--quiet` |
//! | [`EX_NOT_CAPTAIN`] (3) | A captain **is** declared and it is not this host | stderr: the refusal, naming the captain |
//! | [`EX_NO_CAPTAIN`] (4) | No `fleet.captain` declared at all | stderr: the refusal, naming the key to set |
//! | [`EX_USAGE`] (2) | The repo root could not be resolved | stderr |
//!
//! `3` and `4` are **separate codes on purpose**. Collapsing them into one
//! non-zero would make a typo'd or never-declared captain indistinguishable
//! from correct "not my turn" behavior — i.e. every singleton silently
//! unarmed fleet-wide with no signal, the exact edge case #8848's own test
//! plan calls out. A wrapper that genuinely does not care can still write
//! `|| exit 0`.
//!
//! `--disarm` (#8901) is a separate mode entirely — it skips gate evaluation,
//! removes any durable shell-arm record for `job_name`, and always exits `0`
//! (idempotent: disarming an already-absent record is not an error).
//!
//! # `evaluate()` is a gate CHECK, not an arm — `run()` is both (#8901)
//!
//! [`loom_daemon::fleet_captain::arm_singleton_job`] maintains a
//! **process-lifetime** armed registry, sampled into
//! `HostHealthRecord::armed_singleton_jobs`. A CLI invocation is its own
//! process that exits immediately, so recording an arm THERE would be
//! written and lost in the same breath — [`evaluate`] therefore stays a pure
//! read ([`loom_daemon::fleet_captain::resolve_gate_for_root`], no registry
//! write of any kind), and the `evaluating_does_not_touch_the_armed_registry`
//! test below pins that.
//!
//! [`FleetCaptainArgs::run`] is the layer above it that closes the gap: on
//! the **armed** path it also calls
//! [`loom_daemon::fleet_captain::record_shell_arm`], the durable,
//! cross-process registry under `.loom/state/fleet-captain/armed.json` that
//! `sample_host_health` merges into `armed_singleton_jobs` alongside the
//! in-daemon one — see [`loom_daemon::fleet_captain`]'s module doc, "Two arm
//! registries" / "Staleness policy", for why this is durable (survives past
//! this process's exit) rather than process-lifetime, and how a stale entry
//! stops being reported without any action from the wrapper.
//!
//! `--disarm` is the optional precision companion: it removes any durable
//! arm for `job_name` on this host immediately, for a wrapper that wants to
//! clear its entry at its own teardown rather than waiting out the TTL. It
//! does not evaluate the gate at all — teardown does not care who the
//! captain is right now.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;

use loom_daemon::fleet_captain::{resolve_gate_for_root, CaptainGate};

/// A captain is declared, but it is not this host.
pub(crate) const EX_NOT_CAPTAIN: i32 = 3;
/// No `fleet.captain` declared at all in the effective config.
pub(crate) const EX_NO_CAPTAIN: i32 = 4;
/// The repo root could not be resolved.
pub(crate) const EX_USAGE: i32 = 2;

#[derive(clap::Args)]
pub(crate) struct FleetCaptainArgs {
    /// Name of the declared singleton job being gated. Appears verbatim in
    /// the arm/refusal message, so a wrapper's log says which job was
    /// refused rather than just "refused".
    #[arg(value_name = "JOB_NAME")]
    pub job_name: String,

    /// Repo root to resolve `fleet.captain` from (the same tier-chain config
    /// every other knob resolves through). Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Host identity to compare against, overriding
    /// `sweep_registry::host_identity()`. For testing a placement decision
    /// from another host's point of view without exporting `LOOM_HOST_ID`
    /// into the whole shell.
    #[arg(long, value_name = "HOST_ID")]
    pub host_id: Option<String>,

    /// Suppress the message on the ARMED path only. Refusals always print —
    /// a silent refusal is how a singleton goes missing fleet-wide unnoticed.
    #[arg(long)]
    pub quiet: bool,

    /// Remove any durable shell-arm record for `job_name` on this host and
    /// exit 0 unconditionally (idempotent — removing an absent record is not
    /// an error), instead of evaluating the gate (#8901). The precision
    /// companion to the TTL: a wrapper that calls this at its own teardown
    /// clears its entry immediately rather than waiting out
    /// `fleet.captainArmTtlSecs`. Never required for correctness — the TTL
    /// alone already guarantees no entry survives forever.
    #[arg(long)]
    pub disarm: bool,
}

/// The exit code for `gate`, per the module doc's contract.
#[must_use]
pub(crate) fn exit_code(gate: &CaptainGate) -> i32 {
    match gate {
        CaptainGate::Armed { .. } => 0,
        CaptainGate::Refused { .. } => EX_NOT_CAPTAIN,
        CaptainGate::NoCaptainDeclared => EX_NO_CAPTAIN,
    }
}

/// Resolve the gate for `root`/`host_id` and render it — factored out of
/// [`FleetCaptainArgs::run`] so it is testable without `std::process::exit`.
/// Returns `(exit_code, message)`. **Pure**: never touches either arm
/// registry (see the module doc) — [`evaluate_and_record`] is the layer that
/// does.
pub(crate) fn evaluate(root: &Path, host_id: &str, job_name: &str) -> (i32, String) {
    let gate = resolve_gate_for_root(root, host_id);
    (exit_code(&gate), gate.message(job_name))
}

/// [`evaluate`], plus — on the **armed** path only — a durable shell-arm
/// record via [`loom_daemon::fleet_captain::record_shell_arm`] (#8901).
/// Factored out of [`FleetCaptainArgs::run`] so the recording behavior is
/// testable without `std::process::exit`. A record-write failure is logged
/// to the returned message but never changes the exit code: the gate outcome
/// (is this host the captain) and the observability side-channel are
/// independent facts.
pub(crate) fn evaluate_and_record(root: &Path, host_id: &str, job_name: &str) -> (i32, String) {
    let (code, mut message) = evaluate(root, host_id, job_name);
    if code == 0 {
        if let Err(e) = loom_daemon::fleet_captain::record_shell_arm(root, job_name, Utc::now()) {
            message = format!("{message} (durable arm record failed: {e})");
        }
    }
    (code, message)
}

impl FleetCaptainArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = match self.repo_root {
            Some(r) => r,
            None => match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("fleet-captain: cannot resolve repo root: {e}");
                    std::process::exit(EX_USAGE);
                }
            },
        };

        if self.disarm {
            if let Err(e) = loom_daemon::fleet_captain::forget_shell_arm(&root, &self.job_name) {
                eprintln!("fleet-captain: failed to disarm '{}': {e}", self.job_name);
                std::process::exit(EX_USAGE);
            }
            if !self.quiet {
                eprintln!("fleet-captain: disarmed '{}' on this host (#8901).", self.job_name);
            }
            std::process::exit(0);
        }

        let host_id = self
            .host_id
            .unwrap_or_else(loom_daemon::sweep_registry::host_identity);

        // Durable, cross-process arm on the ARMED path (#8901) — see the
        // module doc's "`evaluate()` is a gate CHECK, not an arm" section.
        let (code, message) = evaluate_and_record(&root, &host_id, &self.job_name);
        if code != 0 || !self.quiet {
            eprintln!("{message}");
        }
        std::process::exit(code);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Write `contents` as the tracked `.loom/config.json` under `root`.
    fn write_config(root: &Path, contents: &str) {
        let path = root.join(loom_daemon::config_resolver::LEGACY_CONFIG_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
    }

    #[test]
    fn armed_on_the_captain_host_exits_zero() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"captain": "loom-worker-1"}}"#);
        let (code, message) = evaluate(dir.path(), "loom-worker-1", "forge-queue-check");
        assert_eq!(code, 0);
        assert!(message.contains("forge-queue-check"), "{message}");
        assert!(message.contains("loom-worker-1"), "{message}");
    }

    #[test]
    fn refused_on_a_non_captain_host_exits_3_and_names_the_captain() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"captain": "loom-worker-1"}}"#);
        let (code, message) = evaluate(dir.path(), "loom-worker-2", "forge-queue-check");
        assert_eq!(code, EX_NOT_CAPTAIN);
        // The captain MUST be named — a refusal that does not say who owns
        // the job leaves an operator with nothing to act on.
        assert!(message.contains("loom-worker-1"), "{message}");
        assert!(message.contains("loom-worker-2"), "{message}");
    }

    #[test]
    fn no_captain_declared_exits_4_not_3() {
        // The distinct code is the whole point: "nobody is the captain" is a
        // misconfiguration a wrapper can surface, not a routine "not my turn".
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"nextAgentNumber": 3}"#);
        let (code, message) = evaluate(dir.path(), "loom-worker-1", "forge-queue-check");
        assert_eq!(code, EX_NO_CAPTAIN);
        assert_ne!(code, EX_NOT_CAPTAIN);
        assert!(message.contains("no fleet.captain declared"), "{message}");
    }

    #[test]
    fn missing_config_file_is_no_captain_declared_not_an_error() {
        let dir = tempdir().unwrap();
        let (code, _) = evaluate(dir.path(), "loom-worker-1", "forge-queue-check");
        assert_eq!(code, EX_NO_CAPTAIN);
    }

    #[test]
    fn evaluating_does_not_touch_the_armed_registry() {
        // A CLI check is stateless by design (see the module doc): arming
        // here would write a process-lifetime entry that dies with the
        // process, publishing a phantom `armed_singleton_jobs` value.
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"captain": "loom-worker-1"}}"#);
        let job = format!("cli-gate-test-{}", std::process::id());
        let (code, _) = evaluate(dir.path(), "loom-worker-1", &job);
        assert_eq!(code, 0);
        assert!(
            !loom_daemon::fleet_captain::armed_singleton_job_names().contains(&job),
            "a CLI gate check must not record an arm"
        );
        // #8901: `evaluate()` must not touch the DURABLE registry either —
        // only `evaluate_and_record` (called from `run()`) does that.
        assert!(
            loom_daemon::fleet_captain::shell_armed_job_names(
                dir.path(),
                chrono::Utc::now(),
                std::time::Duration::from_secs(3600),
            )
            .is_empty(),
            "a pure gate check must not record a durable arm either"
        );
    }

    #[test]
    fn evaluate_and_record_writes_a_durable_arm_on_the_armed_path_only() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"captain": "loom-worker-1"}}"#);
        let job = format!("durable-arm-test-{}", std::process::id());

        // Refused: no durable arm recorded.
        let (code, _) = evaluate_and_record(dir.path(), "loom-worker-2", &job);
        assert_eq!(code, EX_NOT_CAPTAIN);
        assert!(loom_daemon::fleet_captain::shell_armed_job_names(
            dir.path(),
            chrono::Utc::now(),
            std::time::Duration::from_secs(3600),
        )
        .is_empty());

        // Armed: recorded, and readable back immediately (same process here,
        // but the registry is file-backed so a separate process would see it
        // too — see `fleet_captain::tests::shell_arm_written_by_one_call_is_read_back_by_another`).
        let (code, _) = evaluate_and_record(dir.path(), "loom-worker-1", &job);
        assert_eq!(code, 0);
        assert!(loom_daemon::fleet_captain::shell_armed_job_names(
            dir.path(),
            chrono::Utc::now(),
            std::time::Duration::from_secs(3600),
        )
        .contains(&job));

        loom_daemon::fleet_captain::forget_shell_arm(dir.path(), &job).unwrap();
    }

    #[test]
    fn exit_code_mapping_is_exhaustive_and_distinct() {
        assert_eq!(
            exit_code(&CaptainGate::Armed {
                captain: "h".to_string()
            }),
            0
        );
        assert_eq!(
            exit_code(&CaptainGate::Refused {
                captain: "a".to_string(),
                current_host_id: "b".to_string(),
            }),
            EX_NOT_CAPTAIN
        );
        assert_eq!(exit_code(&CaptainGate::NoCaptainDeclared), EX_NO_CAPTAIN);
    }
}
