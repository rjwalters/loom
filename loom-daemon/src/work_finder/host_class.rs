//! Host-class routing (Issue #9034): let a daemon instance declare what kind
//! of machine it is running on — `local-dev` or `remote-worker` — and refuse
//! to autonomously dispatch a `loom:heavy` sweep on a `local-dev` box.
//!
//! # The problem this fixes
//!
//! `crate::admission_brake`'s own module doc (#4903/#5270) cites the exact
//! motivating incident: an 8-vCPU worker observed at load-average 95 (12x
//! overcommit) from three in-flight heavy simulation sweeps. The admission
//! brake is a **reactive** backstop — it holds *new* admissions once a host is
//! *already* saturated. It cannot refuse a known-heavy sweep *before* it
//! starts on a machine that was never meant to run it. This module is the
//! missing **preventive** half: a host declares its class, and a candidate
//! carrying the new [`LOOM_HEAVY_LABEL`] label is refused on a `local-dev`
//! host before it is ever claimed.
//!
//! # Not `tier:*`
//!
//! `tier:goal-advancing` / `tier:goal-supporting` / `tier:maintenance`
//! (`.github/labels.yml`) are a **priority** axis, not a **resource-weight**
//! one — reusing them here would refuse ordinary prioritized work (a one-line
//! config fix) on every `local-dev` laptop the moment it is prioritized. This
//! module adds exactly one new, explicit, human/Architect/Curator-applied
//! label instead: [`LOOM_HEAVY_LABEL`].
//!
//! # Default: unclassified, inert
//!
//! `host_class` absent (no `hostClass` key in any config tier, no env var) resolves to
//! [`HostClass::Unclassified`], for which [`gate`] never fires — this is the
//! only default that produces zero behavior change for every existing
//! install. Do **not** default an unset host to `local-dev`: that would
//! silently start refusing heavy sweeps on every unconfigured host in the
//! fleet the moment this ships.
//!
//! # A fact about the HOST, resolved once at work-finder startup
//!
//! Reclassifying a machine is a rare, deliberate operator action, so the
//! class is a **startup-capture**: [`resolve_at_startup`] runs exactly once,
//! in `spawn_multi_work_finder_task` *before* the tick loop, against the
//! daemon's primary workspace (`fallback_root`), and the resulting value is
//! threaded into every per-tick `RegistryDispatcher` via
//! [`super::forge::dispatcher_pairs`]. A config edit mid-run changes nothing
//! until the next daemon restart (pinned by
//! `host_class_gate_tests::host_class_is_pinned_at_startup_across_a_mid_run_config_change`).
//! Because it is one value per daemon process, every workspace that daemon
//! serves shares it — a per-repo class would be meaningless.
//!
//! Set it with `LOOM_HOST_CLASS` or a **machine-local** config tier
//! (`~/.local/share/loom/config/defaults.json`, or the ignored
//! `.loom-local/local.json`) — never a repo's committed `.loom/config.json`,
//! which every clone shares and would misclassify each remote worker running
//! that repo as `local-dev`. An unrecognized value (e.g. `"local_dev"`) logs
//! a one-time `WARN` and resolves to unclassified (gate off).
//!
//! [`resolve_allow_heavy_local`] (the override below) is the opposite: read
//! fresh every tick per workspace, like `extra_skip_labels`.
//!
//! # Two independent override paths
//!
//! - **Autonomous work-finder loop**: `autonomous.workFinder.allowHeavyLocal`
//!   / `LOOM_ALLOW_HEAVY_LOCAL` (env > config > default `false`) — un-gates
//!   one `local-dev` box's autonomous loop entirely.
//! - **Explicit CLI dispatch**: `loom-daemon dispatch <issue> --allow-local`
//!   (`loom-daemon/src/cli/dispatch.rs`), mirroring `--ignore-host-constraint`
//!   (#7456) exactly.
//!
//! # Where the gate sits
//!
//! [`gate`] is called from both `tick_with_saturation_brake` (single-workspace)
//! and `tick_multi_with_saturation_brake` (multi-workspace), immediately after
//! the existing host-affinity constraint check (`crate::host_affinity`) — same
//! checkpoint, same **zero-side-effect** refusal contract (no claim flip, no
//! comment, no cooldown/backoff record): a `local-dev` host will never build a
//! refused candidate without an operator action, so there is nothing to
//! "retry" here the way a capacity/ramp/saturation defer means.

use std::path::Path;
use std::sync::Once;

use serde_json::Value;

use super::{read_work_finder_config, WorkFinderConfig, WorkItem};

/// The label naming a CPU/RAM-intensive sweep (Issue #9034) — e.g. a
/// sustained simulation/EDA build — that should not run on a `local-dev`
/// `host_class` machine without an explicit override. Human/Architect/
/// Curator-applied; the work finder never applies or removes it itself.
pub const LOOM_HEAVY_LABEL: &str = "loom:heavy";

/// Env override for [`resolve`] — `LOOM_HOST_CLASS`.
pub const HOST_CLASS_ENV: &str = "LOOM_HOST_CLASS";

/// Env override for [`resolve_allow_heavy_local`] — `LOOM_ALLOW_HEAVY_LOCAL`.
pub const ALLOW_HEAVY_LOCAL_ENV: &str = "LOOM_ALLOW_HEAVY_LOCAL";

/// A daemon host's operator-declared class (Issue #9034). An unrecognized
/// config/env value falls through to [`Self::Unclassified`] rather than
/// erroring — the same soft-fail contract every other
/// `autonomous.workFinder.*` knob in this module uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostClass {
    /// No `host_class` declared anywhere — [`gate`] never fires. The default
    /// for every host that has not opted in.
    #[default]
    Unclassified,
    /// A developer's own machine (a laptop, a workstation) — heavy sweeps are
    /// refused here unless explicitly overridden.
    LocalDev,
    /// A dedicated remote build/sweep worker (AWS, Hetzner, …) — never gated.
    RemoteWorker,
}

impl HostClass {
    /// The config/log string form: `"local-dev"` / `"remote-worker"` /
    /// `"unclassified"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::LocalDev => "local-dev",
            Self::RemoteWorker => "remote-worker",
        }
    }

    /// Parse the two recognized config/env values. Anything else — including
    /// `"unclassified"` itself, which no operator ever writes — is `None`,
    /// and the caller treats that as unclassified.
    #[must_use]
    fn parse(s: &str) -> Option<Self> {
        match s {
            "local-dev" => Some(Self::LocalDev),
            "remote-worker" => Some(Self::RemoteWorker),
            _ => None,
        }
    }
}

/// This host's declared class plus the live "allow heavy local" override,
/// bundled into one [`super::WorkDispatcher`] trait method
/// (`heavy_local_policy`) so `work_finder.rs` — frozen by the file-size
/// ratchet (`.loom/docs/file-size-policy.md`) — needs only ONE new default
/// method rather than two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeavyLocalPolicy {
    /// This host's [`resolve`]d class.
    pub class: HostClass,
    /// This workspace's [`resolve_allow_heavy_local`]d override.
    pub allow_heavy_local: bool,
}

fn env_host_class() -> Option<HostClass> {
    std::env::var(HOST_CLASS_ENV)
        .ok()
        .and_then(|v| HostClass::parse(v.trim()))
}

/// Parse `autonomous.workFinder.hostClass` out of the `workFinder` sub-block,
/// soft-failing to `None` (unclassified) on absent or unrecognized — the same
/// contract [`super::repo_cap::parse_config`] uses.
#[must_use]
pub fn parse_config(wf: Option<&Value>) -> Option<HostClass> {
    wf.and_then(|w| w.get("hostClass"))
        .and_then(Value::as_str)
        .and_then(HostClass::parse)
}

/// Resolve this host's declared class with precedence **env
/// ([`HOST_CLASS_ENV`]) > config (`autonomous.workFinder.hostClass`) >
/// unclassified**. Production callers go through [`resolve_at_startup`],
/// which runs once — never hot-reloaded per tick.
#[must_use]
pub fn resolve(config: &WorkFinderConfig) -> HostClass {
    env_host_class().or(config.host_class).unwrap_or_default()
}

/// Every raw `hostClass` / `LOOM_HOST_CLASS` value that is set but not
/// recognized, labelled by source — pure, so the warn path is testable.
fn unrecognized_values(env: Option<&str>, config: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(raw) = env.map(str::trim).filter(|v| !v.is_empty()) {
        if HostClass::parse(raw).is_none() {
            out.push(format!("{HOST_CLASS_ENV}={raw:?}"));
        }
    }
    if let Some(v) = config.filter(|v| !v.is_null()) {
        if v.as_str().and_then(HostClass::parse).is_none() {
            out.push(format!("autonomous.workFinder.hostClass={v}"));
        }
    }
    out
}

/// One-time `WARN` for an unrecognized class value: a typo such as
/// `"local_dev"` would otherwise silently leave the gate off while the
/// operator believes the machine is protected. Soft-fail semantics are
/// unchanged — the value still resolves to unclassified.
fn warn_unrecognized_once(root: &Path) {
    static WARNED: Once = Once::new();
    let effective = crate::config_resolver::resolve_effective_config(root);
    let bad = unrecognized_values(
        std::env::var(HOST_CLASS_ENV).ok().as_deref(),
        crate::config_resolver::get_path(&effective, "autonomous.workFinder.hostClass"),
    );
    if !bad.is_empty() {
        WARNED.call_once(|| {
            log::warn!(
                "work_finder: unrecognized host class {} — expected `local-dev` or \
                 `remote-worker`; treating this host as unclassified, so the `{LOOM_HEAVY_LABEL}` \
                 gate is OFF (Issue #9034)",
                bad.join(", ")
            );
        });
    }
}

/// Resolve this host's class ONCE, at startup, from `root`'s effective config
/// (env > config > unclassified) — see the module doc's "A fact about the
/// HOST". Called once by the multi-workspace work-finder before its tick loop
/// and once per (one-shot) `loom-daemon dispatch` CLI process; never per tick.
#[must_use]
pub fn resolve_at_startup(root: &Path) -> HostClass {
    warn_unrecognized_once(root);
    resolve(&read_work_finder_config(root))
}

fn env_allow_heavy_local() -> Option<bool> {
    std::env::var(ALLOW_HEAVY_LOCAL_ENV)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Parse `autonomous.workFinder.allowHeavyLocal` out of the `workFinder`
/// sub-block — `None` when absent or not a JSON boolean.
#[must_use]
pub fn parse_config_allow_heavy_local(wf: Option<&Value>) -> Option<bool> {
    wf.and_then(|w| w.get("allowHeavyLocal"))
        .and_then(Value::as_bool)
}

/// Resolve the autonomous-loop override with precedence **env
/// ([`ALLOW_HEAVY_LOCAL_ENV`]) > config
/// (`autonomous.workFinder.allowHeavyLocal`) > default `false`**. Read fresh
/// every tick (unlike [`resolve`] above) so an operator can un-gate one
/// `local-dev` box's autonomous loop without a daemon restart.
#[must_use]
pub fn resolve_allow_heavy_local(config: &WorkFinderConfig) -> bool {
    env_allow_heavy_local()
        .or(config.allow_heavy_local)
        .unwrap_or(false)
}

/// Whether `labels` carries [`LOOM_HEAVY_LABEL`].
#[must_use]
pub fn is_heavy(labels: &[String]) -> bool {
    labels.iter().any(|l| l == LOOM_HEAVY_LABEL)
}

/// Whether `policy` refuses a candidate carrying `labels` (Issue #9034): the
/// host is `local-dev`, the candidate carries `loom:heavy`, and the override
/// is not set.
#[must_use]
fn refuses(policy: HeavyLocalPolicy, labels: &[String]) -> bool {
    policy.class == HostClass::LocalDev && is_heavy(labels) && !policy.allow_heavy_local
}

/// The host-class gate's per-candidate verdict (Issue #9034): `false` when
/// `item` is unaffected; `true` when it must be refused THIS tick. The caller
/// `continue`s past it with **zero** other side effects (no claim flip, no
/// comment, no cooldown/backoff record) — mirroring the host-affinity
/// constraint's own no-side-effect refusal shape (#7456). Bumps `*skipped`
/// and logs internally so both tick loops need only one call each (see the
/// module doc's "Where the gate sits").
pub fn gate(item: &WorkItem, policy: HeavyLocalPolicy, skipped: &mut usize) -> bool {
    if !refuses(policy, &item.labels) {
        return false;
    }
    *skipped += 1;
    log::info!(
        "work_finder: skipping issue #{} — carries `{LOOM_HEAVY_LABEL}`, this host is {} \
         (Issue #9034); pass --allow-local or set autonomous.workFinder.allowHeavyLocal to \
         override",
        item.number,
        policy.class.as_str()
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn item_with_labels(labels: &[&str]) -> WorkItem {
        WorkItem::new(9, labels.iter().map(|s| (*s).to_string()).collect())
    }

    // ---- HostClass parsing / resolution --------------------------------

    #[test]
    fn parse_config_reads_both_recognized_values() {
        assert_eq!(
            parse_config(Some(&serde_json::json!({"hostClass": "local-dev"}))),
            Some(HostClass::LocalDev)
        );
        assert_eq!(
            parse_config(Some(&serde_json::json!({"hostClass": "remote-worker"}))),
            Some(HostClass::RemoteWorker)
        );
    }

    #[test]
    fn parse_config_falls_through_on_absent_or_invalid() {
        assert_eq!(parse_config(None), None);
        assert_eq!(parse_config(Some(&serde_json::json!({}))), None);
        assert_eq!(
            parse_config(Some(&serde_json::json!({"hostClass": "laptop"}))),
            None,
            "an unrecognized value falls through to unclassified, never an error"
        );
    }

    #[test]
    #[serial]
    fn resolve_precedence_is_env_then_config_then_unclassified() {
        std::env::remove_var(HOST_CLASS_ENV);
        assert_eq!(resolve(&WorkFinderConfig::default()), HostClass::Unclassified);

        let cfg = WorkFinderConfig {
            host_class: Some(HostClass::LocalDev),
            ..Default::default()
        };
        assert_eq!(resolve(&cfg), HostClass::LocalDev);

        std::env::set_var(HOST_CLASS_ENV, "remote-worker");
        assert_eq!(resolve(&cfg), HostClass::RemoteWorker, "env wins over config");
        std::env::remove_var(HOST_CLASS_ENV);
    }

    #[test]
    #[serial]
    fn resolve_treats_an_invalid_env_value_as_unset() {
        std::env::set_var(HOST_CLASS_ENV, "laptop");
        let cfg = WorkFinderConfig {
            host_class: Some(HostClass::RemoteWorker),
            ..Default::default()
        };
        assert_eq!(resolve(&cfg), HostClass::RemoteWorker, "invalid env falls through to config");
        std::env::remove_var(HOST_CLASS_ENV);
    }

    #[test]
    fn unrecognized_values_flags_typos_from_either_source() {
        assert!(unrecognized_values(None, None).is_empty());
        assert!(unrecognized_values(Some("local-dev"), None).is_empty());
        assert!(unrecognized_values(Some("  "), None).is_empty(), "blank env is unset");
        assert!(unrecognized_values(None, Some(&serde_json::json!("remote-worker"))).is_empty());
        let bad = unrecognized_values(Some("laptop"), Some(&serde_json::json!("local_dev")));
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad[0].contains("laptop") && bad[1].contains("local_dev"), "{bad:?}");
        assert_eq!(unrecognized_values(None, Some(&serde_json::json!(true))).len(), 1);
    }

    // ---- allowHeavyLocal precedence -------------------------------------

    #[test]
    #[serial]
    fn resolve_allow_heavy_local_precedence_is_env_then_config_then_false() {
        std::env::remove_var(ALLOW_HEAVY_LOCAL_ENV);
        assert!(!resolve_allow_heavy_local(&WorkFinderConfig::default()));

        let cfg = WorkFinderConfig {
            allow_heavy_local: Some(true),
            ..Default::default()
        };
        assert!(resolve_allow_heavy_local(&cfg));

        std::env::set_var(ALLOW_HEAVY_LOCAL_ENV, "0");
        assert!(!resolve_allow_heavy_local(&cfg), "env wins over config, even to turn it off");
        std::env::remove_var(ALLOW_HEAVY_LOCAL_ENV);
    }

    // ---- the gate predicate ----------------------------------------------

    #[test]
    fn heavy_on_local_dev_is_refused() {
        let policy = HeavyLocalPolicy {
            class: HostClass::LocalDev,
            allow_heavy_local: false,
        };
        let mut skipped = 0;
        assert!(gate(&item_with_labels(&["loom:issue", LOOM_HEAVY_LABEL]), policy, &mut skipped));
        assert_eq!(skipped, 1);
    }

    #[test]
    fn heavy_on_remote_worker_dispatches() {
        let policy = HeavyLocalPolicy {
            class: HostClass::RemoteWorker,
            allow_heavy_local: false,
        };
        let mut skipped = 0;
        assert!(!gate(
            &item_with_labels(&["loom:issue", LOOM_HEAVY_LABEL]),
            policy,
            &mut skipped
        ));
        assert_eq!(skipped, 0);
    }

    #[test]
    fn heavy_on_unclassified_dispatches() {
        let mut skipped = 0;
        assert!(!gate(
            &item_with_labels(&["loom:issue", LOOM_HEAVY_LABEL]),
            HeavyLocalPolicy::default(),
            &mut skipped
        ));
    }

    #[test]
    fn non_heavy_on_local_dev_dispatches() {
        let policy = HeavyLocalPolicy {
            class: HostClass::LocalDev,
            allow_heavy_local: false,
        };
        let mut skipped = 0;
        assert!(!gate(&item_with_labels(&["loom:issue"]), policy, &mut skipped));
    }

    #[test]
    fn heavy_on_local_dev_with_override_dispatches() {
        let policy = HeavyLocalPolicy {
            class: HostClass::LocalDev,
            allow_heavy_local: true,
        };
        let mut skipped = 0;
        assert!(!gate(
            &item_with_labels(&["loom:issue", LOOM_HEAVY_LABEL]),
            policy,
            &mut skipped
        ));
    }
}
