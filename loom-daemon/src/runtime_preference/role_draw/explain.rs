//! `loom-daemon roles draw --explain` (#11726): each stage's effective
//! per-attempt draw table on this host, the probability of each candidate
//! (`w / Σw` before availability), the config tier that set it, what
//! `excludeProviders` and weight 0 took out, and — for a stage with no
//! `roleDraw` — the `rolePreference` list that still decides it.
//!
//! #14027: each candidate of a table is also marked admitted or not **on
//! this host for this workspace**, with the full refusal reason, using the
//! real spawn path's per-candidate question
//! ([`crate::runtime_preference::probe_candidate`] with the real containment
//! preparer). The view prints the effective table after unavailable and
//! unadmitted candidates are dropped and the weights renormalised, and says
//! plainly when no runtime can serve a stage here.

use super::{DrawConfig, Layer};
use crate::runtime_preference::resolve::{SkipReason, Tap};
use serde_json::{json, Value};
use std::path::Path;

/// The stages a table can name, in display order.
pub const STAGES: &[&str] = &[
    "architect",
    "auditor",
    "builder",
    "champion",
    "curator",
    "doctor",
    "driver",
    "guide",
    "hermit",
    "judge",
    "sweep-lifecycle",
];

/// One candidate's admission verdict as the explain view records it: whether
/// it was admitted, the refusal reason when not, and — when the real
/// containment walk admitted it — the account the launch would have run on,
/// so a row names the seat its verdict was measured against (#14578).
fn verdict_fields(
    verdict: &Result<Option<String>, SkipReason>,
) -> (bool, Option<String>, Option<String>) {
    match verdict {
        Ok(account) => (true, None, account.clone()),
        Err(reason) => {
            (false, Some(format!("{}({})", reason.kind(), reason.summary_capped(None))), None)
        }
    }
}

/// The production probe: the real spawn path's per-candidate admission walk
/// ([`crate::runtime_preference::probe_candidate`]) with the real containment
/// preparer, in the view's always-release, never-launch mode (#14027).
///
/// The real preparer has no dry mode: a containment-eligible refusal is
/// retried by genuinely preparing a private selection — leasing a Codex
/// account, validating Docker — exactly as a launch would retry it, which is
/// what makes the view truthful about a runtime that can never be admitted
/// (#13983). The lease side is bounded by the
/// [`crate::runtime_preference::ContainmentPreparer`] contract: this view
/// never takes the prepared selection, and [`probe_candidate`] releases after
/// every candidate, so every selection is dropped — finishing its lease and
/// rolling back its admission record — before the view returns.
///
/// Returns the account the admission selected (#14578), so the row can name
/// the seat its verdict was measured against; `None` when the runtime is
/// admitted without a managed seat.
fn probe_on_this_host(
    root: &Path,
    role: &str,
    tap: &Tap,
    tier: usize,
) -> Result<Option<String>, SkipReason> {
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let mut preparer = admission_preparer(root, role);
    crate::runtime_preference::probe_candidate(
        root,
        role,
        tap,
        tier,
        now,
        crate::runtime_preference::DispatchContext {
            intent: crate::runtime_preference::Intent::Probe,
            role_tick: false,
        },
        &mut preparer,
    )
    .map(|admitted| {
        admitted
            .execution
            .as_ref()
            .map(|execution| execution.account.clone())
    })
}

/// The real spawn path's containment preparer, built for one stage's probe.
///
/// The account is selected for the model the launch would pin (#12748): the
/// candidate's drawn model when it names one, else the role's own resolution
/// — the role runner's precedence without a runner override.
fn admission_preparer<'a>(
    root: &'a Path,
    role: &str,
) -> crate::tokens_pool::private_workspace::containment::Preparer<'a> {
    let hint = crate::role_runner::model_resolution::resolve_role_runner_model(root, role);
    crate::tokens_pool::private_workspace::containment::Preparer::new(
        root,
        crate::tokens_pool::private_workspace::JobKind::Role,
        None,
        format!("explain-{role}"),
        Box::new(move |runtime: &str, drawn: Option<&str>| {
            let (model, source) = match drawn.filter(|d| !d.trim().is_empty()) {
                Some(drawn) => (drawn.to_string(), "role-draw".to_string()),
                None => hint.clone(),
            };
            let (model, _) =
                crate::role_runner::model_resolution::reconcile_unpinned_model_with_runtime(
                    runtime, model, source,
                );
            (!model.is_empty()).then_some(model)
        }),
    )
}

/// The explain view as JSON (the `--json` form, and the source of the text).
///
/// Asks the real spawn path's admission question for every active candidate
/// of every table — admission with the real containment preparer, credential
/// availability, the backstop ceiling — as a probe: nothing is launched, and
/// any account lease containment preparation takes is released before the
/// view returns (#14027).
#[must_use]
pub fn explain_json(root: &Path) -> Value {
    explain_json_with(root, &mut |role: &str, tap: &Tap, tier: usize| {
        probe_on_this_host(root, role, tap, tier)
    })
}

/// One candidate's admission question: does the spawn walk admit and serve
/// this candidate on this host — with the walk's own skip reason when not,
/// and the account a real admission selected when it does (#14578).
pub(crate) type AdmissionProbe<'a> =
    dyn FnMut(&str, &Tap, usize) -> Result<Option<String>, SkipReason> + 'a;

/// [`explain_json`] with the per-candidate admission question injected
/// (tests): `probe(role, tap, tier)` answers whether the spawn walk would
/// admit and serve that candidate on this host, with the walk's own skip
/// reason when not and the selected account when admitted.
pub(crate) fn explain_json_with(root: &Path, probe: &mut AdmissionProbe<'_>) -> Value {
    let config = DrawConfig::load(root);
    let effective = crate::config_resolver::resolve_effective_config(root);
    let mut stages = Vec::new();
    for stage in STAGES {
        let Some(tables) = config.roles.get(*stage) else {
            let fallback = match super::super::preference_for(&effective, stage) {
                Ok(Some((source, taps))) => json!({
                    "source": source.as_str(),
                    "order": taps.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
                Ok(None) => json!({"source": "static"}),
                Err(error) => json!({"source": "invalid", "error": error}),
            };
            stages.push(json!({"stage": stage, "roleDraw": null, "fallback": fallback}));
            continue;
        };
        let attempts: Vec<Value> = tables
            .values()
            .map(|table| {
                let eff = config.effective(table);
                let total = eff.total();
                let verdicts: Vec<Result<Option<String>, SkipReason>> = eff
                    .active
                    .iter()
                    .enumerate()
                    .map(|(tier, (candidate, _))| probe(stage, &candidate.tap(), tier))
                    .collect();
                let candidates: Vec<Value> = eff
                    .active
                    .iter()
                    .enumerate()
                    .map(|(i, (c, w))| {
                        #[allow(clippy::cast_precision_loss)]
                        let p = *w as f64 / total as f64;
                        let (admitted, reason, account) = verdict_fields(&verdicts[i]);
                        json!({
                            "candidate": c.key,
                            "weight": w,
                            "p": p,
                            "admitted": admitted,
                            "reason": reason,
                            "account": account,
                        })
                    })
                    .collect();
                let off: Vec<Value> = eff
                    .off
                    .iter()
                    .map(|(k, why)| json!({"candidate": k, "reason": why}))
                    .collect();
                let served: Vec<(String, u64)> = eff
                    .active
                    .iter()
                    .zip(verdicts.iter())
                    .filter(|(_, verdict)| verdict.is_ok())
                    .map(|((c, w), _)| (c.key.clone(), *w))
                    .collect();
                let effective_total = served
                    .iter()
                    .fold(0u64, |acc, (_, w)| acc.saturating_add(*w));
                let effective: Vec<Value> = served
                    .iter()
                    .map(|(key, w)| {
                        #[allow(clippy::cast_precision_loss)]
                        let p = *w as f64 / effective_total as f64;
                        json!({"candidate": key, "weight": w, "p": p})
                    })
                    .collect();
                json!({
                    "attempt": table.attempt,
                    "layer": table.layer.as_str(),
                    "total": total,
                    "candidates": candidates,
                    "off": off,
                    "empty": eff.active.is_empty(),
                    "effective": {
                        "total": effective_total,
                        "candidates": effective,
                        "empty": served.is_empty(),
                    },
                })
            })
            .collect();
        stages.push(json!({"stage": stage, "roleDraw": attempts}));
    }
    json!({
        "workspace": root.display().to_string(),
        "excludeProviders": config.exclude,
        "excludeLayer": config.exclude_layer.map(Layer::as_str),
        "warnings": config.warnings,
        "stages": stages,
    })
}

/// The explain view as text.
#[must_use]
pub fn explain_text(root: &Path) -> String {
    explain_text_with(root, &mut |role: &str, tap: &Tap, tier: usize| {
        probe_on_this_host(root, role, tap, tier)
    })
}

/// [`explain_text`] with the per-candidate admission question injected
/// (tests), mirroring [`explain_json_with`].
pub(crate) fn explain_text_with(root: &Path, probe: &mut AdmissionProbe<'_>) -> String {
    let doc = explain_json_with(root, probe);
    let mut out = format!("Role draw tables for {}\n", doc["workspace"].as_str().unwrap_or(""));
    let excluded: Vec<&str> = doc["excludeProviders"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if excluded.is_empty() {
        out.push_str("excludeProviders: (none)\n");
    } else {
        out.push_str(&format!(
            "excludeProviders: {} (set by {})\n",
            excluded.join(", "),
            doc["excludeLayer"].as_str().unwrap_or("?")
        ));
    }
    for stage in doc["stages"].as_array().into_iter().flatten() {
        let name = stage["stage"].as_str().unwrap_or("?");
        let Some(attempts) = stage["roleDraw"].as_array() else {
            let fb = &stage["fallback"];
            let order = fb["order"]
                .as_array()
                .map(|o| {
                    o.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" > ")
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "\n{name}: no roleDraw — {} {}\n",
                fb["source"].as_str().unwrap_or("?"),
                order
            ));
            continue;
        };
        out.push_str(&format!("\n{name}:\n"));
        for attempt in attempts {
            out.push_str(&format!(
                "  attempt {}+ (set by {}, Σw={}){}\n",
                attempt["attempt"],
                attempt["layer"].as_str().unwrap_or("?"),
                attempt["total"],
                if attempt["empty"].as_bool() == Some(true) {
                    "  WARNING: empty after exclusions — nothing can run this stage here"
                } else {
                    ""
                }
            ));
            for c in attempt["candidates"].as_array().into_iter().flatten() {
                let mark = if c["admitted"].as_bool() == Some(true) {
                    match c["account"].as_str() {
                        Some(account) => format!("admitted (on {account})"),
                        None => "admitted".to_string(),
                    }
                } else {
                    format!("not admitted: {}", c["reason"].as_str().unwrap_or("?"))
                };
                out.push_str(&format!(
                    "    {:<24} w={:<5} p={:.1}%  {}\n",
                    c["candidate"].as_str().unwrap_or("?"),
                    c["weight"],
                    c["p"].as_f64().unwrap_or(0.0) * 100.0,
                    mark
                ));
            }
            for c in attempt["off"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "    {:<24} off: {}\n",
                    c["candidate"].as_str().unwrap_or("?"),
                    c["reason"].as_str().unwrap_or("?")
                ));
            }
            let effective = &attempt["effective"];
            if effective["empty"].as_bool() == Some(true) {
                out.push_str("  no runtime can serve this role here\n");
            } else {
                let served: Vec<String> = effective["candidates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| {
                        format!(
                            "{} {:.1}%",
                            c["candidate"].as_str().unwrap_or("?"),
                            c["p"].as_f64().unwrap_or(0.0) * 100.0
                        )
                    })
                    .collect();
                out.push_str(&format!(
                    "  effective on this host: Σw={} — {}\n",
                    effective["total"],
                    served.join(", ")
                ));
            }
        }
    }
    for warning in doc["warnings"].as_array().into_iter().flatten() {
        out.push_str(&format!("\nwarning: {}", warning.as_str().unwrap_or("?")));
    }
    out.push_str(
        "\n\nA candidate's p is its share of the configured table, before availability. \
         \"admitted\" is the real spawn path's per-candidate verdict on this host for this \
         workspace — capability admission (a containment-eligible refusal retried through the \
         real private-clone preparer, as a launch would retry it), then credential \
         availability and the backstop ceiling — and the effective table drops the refused \
         candidates and renormalises. The verdict is a probe: nothing is launched, and any \
         account lease containment preparation takes is released before the view returns. \
         Attempt N uses the highest table <= N.\n",
    );
    out
}
