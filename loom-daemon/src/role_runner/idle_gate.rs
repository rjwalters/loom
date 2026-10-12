//! Pipeline-empty gate for hermit / architect idle generation (#10817,
//! slice 4 of #10630).
//!
//! Today an `onIdle` edge means "a host slot is free". With
//! `autonomous.balance.idleGate` on it additionally means "this repo's
//! pipeline is empty": no review / changes / merge debt (from the demand
//! ledger) and, when observed, no ready or building issues. Only the
//! proposal-generating roles ([`GATED_ROLES`]) are gated; every other idle
//! role, and every call with the flag off, takes the unchanged path.
//!
//! Unobserved inputs never invent a denial and never invent a grant: they
//! defer to today's host-slot rule. The predicate is pure so slice 1's
//! `RepoPipeline` can replace [`PipelineView`] as a one-line input swap.

use std::path::Path;

use super::demand::{self, HostDebt};

/// Env override for `autonomous.balance.idleGate` (env > config > default).
pub const IDLE_GATE_ENV: &str = "LOOM_BALANCE_IDLE_GATE";

/// The roles whose idle generation is keyed off pipeline-empty.
pub const GATED_ROLES: [&str; 2] = ["hermit", "architect"];

/// What the gate decided for one `(repo, role)` idle edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleGateDecision {
    /// Pipeline observed empty: dispatch.
    Grant,
    /// Nothing observed either way: today's host-slot rule applies unchanged.
    Defer,
    /// Pipeline has work; the string names why.
    Deny(String),
}

/// Per-repo pipeline readings; `None` is unobserved.
#[derive(Debug, Clone, Copy, Default)]
pub struct PipelineView {
    /// Review / changes / merge debt from the demand ledger.
    pub debt: HostDebt,
    /// Open `loom:issue` count, if known.
    pub ready: Option<usize>,
    /// Open `loom:building` count, if known.
    pub building: Option<usize>,
}

/// Whether `role` is subject to the gate.
#[must_use]
pub fn is_gated_role(role: &str) -> bool {
    GATED_ROLES.contains(&role)
}

/// Pure predicate: deny on any observed work, grant when something was
/// observed and all of it is zero, else defer.
#[must_use]
pub fn pipeline_empty(view: &PipelineView) -> IdleGateDecision {
    let mut observed = false;
    let axes = [
        ("review-debt", view.debt.review.map(|a| a.total)),
        ("changes-debt", view.debt.changes.map(|a| a.total)),
        ("merge-debt", view.debt.merge.map(|a| a.total)),
        ("ready-issues", view.ready),
        ("building-issues", view.building),
    ];
    for (name, count) in axes {
        if let Some(n) = count {
            observed = true;
            if n > 0 {
                return IdleGateDecision::Deny(format!("{name}={n}"));
            }
        }
    }
    if observed {
        IdleGateDecision::Grant
    } else {
        IdleGateDecision::Defer
    }
}

/// Resolve the flag from a raw env value and the config value. An invalid env
/// value falls back to the config tier (then the default `false`); the bool
/// in the result is `true` when the env value was invalid (caller warns).
#[must_use]
pub fn resolve_flag(env: Option<&str>, config: Option<bool>) -> (bool, bool) {
    if let Some(raw) = env {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return (true, false),
            "0" | "false" | "no" | "off" => return (false, false),
            _ => return (config.unwrap_or(false), true),
        }
    }
    (config.unwrap_or(false), false)
}

/// `autonomous.balance.idleGate` for `root`, env > config > default (`false`).
#[must_use]
pub fn idle_gate_enabled(root: &Path) -> bool {
    let env = std::env::var(IDLE_GATE_ENV).ok();
    let effective = crate::config_resolver::resolve_effective_config(root);
    let config = crate::config_resolver::get_path(&effective, "autonomous.balance.idleGate")
        .and_then(serde_json::Value::as_bool);
    let (on, invalid) = resolve_flag(env.as_deref(), config);
    if invalid {
        log::warn!("role_runner: invalid {IDLE_GATE_ENV} value, falling back to config/default");
    }
    on
}

/// Gate `role` on `root`. `None` means the gate does not apply (flag off or
/// an ungated role) and the caller must take the unchanged path with no ledger
/// read and no log line.
#[must_use]
pub fn gate(root: &Path, role: &str) -> Option<IdleGateDecision> {
    if !is_gated_role(role) || !idle_gate_enabled(root) {
        return None;
    }
    let stale = demand::read_demand_config(root).stale();
    let view = PipelineView {
        debt: demand::global().repo_debt(root, stale),
        ready: None,
        building: None,
    };
    let decision = pipeline_empty(&view);
    match &decision {
        IdleGateDecision::Grant => log::info!(
            "role_runner: idle grant root={} role={role} trigger=idle reason=pipeline-empty",
            root.display()
        ),
        IdleGateDecision::Deny(why) => log::info!(
            "role_runner: idle deny root={} role={role} trigger=idle reason={why}",
            root.display()
        ),
        IdleGateDecision::Defer => {}
    }
    Some(decision)
}
