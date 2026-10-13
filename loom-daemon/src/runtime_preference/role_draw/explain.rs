//! `loom-daemon roles draw --explain` (#11726): each stage's effective
//! per-attempt draw table on this host, the probability of each candidate
//! (`w / Σw` before availability), the config tier that set it, what
//! `excludeProviders` and weight 0 took out, and — for a stage with no
//! `roleDraw` — the `rolePreference` list that still decides it.

use super::{DrawConfig, Layer};
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

/// The explain view as JSON (the `--json` form, and the source of the text).
#[must_use]
pub fn explain_json(root: &Path) -> Value {
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
                let candidates: Vec<Value> = eff
                    .active
                    .iter()
                    .map(|(c, w)| {
                        #[allow(clippy::cast_precision_loss)]
                        let p = *w as f64 / total as f64;
                        json!({"candidate": c.key, "weight": w, "p": p})
                    })
                    .collect();
                let off: Vec<Value> = eff
                    .off
                    .iter()
                    .map(|(k, why)| json!({"candidate": k, "reason": why}))
                    .collect();
                json!({
                    "attempt": table.attempt,
                    "layer": table.layer.as_str(),
                    "total": total,
                    "candidates": candidates,
                    "off": off,
                    "empty": eff.active.is_empty(),
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
    let doc = explain_json(root);
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
                out.push_str(&format!(
                    "    {:<24} w={:<5} p={:.1}%\n",
                    c["candidate"].as_str().unwrap_or("?"),
                    c["weight"],
                    c["p"].as_f64().unwrap_or(0.0) * 100.0
                ));
            }
            for c in attempt["off"].as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "    {:<24} off: {}\n",
                    c["candidate"].as_str().unwrap_or("?"),
                    c["reason"].as_str().unwrap_or("?")
                ));
            }
        }
    }
    for warning in doc["warnings"].as_array().into_iter().flatten() {
        out.push_str(&format!("\nwarning: {}", warning.as_str().unwrap_or("?")));
    }
    out.push_str(
        "\n\nProbabilities are before availability: an unavailable candidate is excluded at \
         draw time and the rest renormalised. Attempt N uses the highest table <= N.\n",
    );
    out
}
