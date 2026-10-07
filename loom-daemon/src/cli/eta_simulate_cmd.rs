//! `loom-daemon eta simulate --planner <config>` (#10528, slice b): preview
//! what a proposed planner config does to the live ready roster's `start` /
//! `land` ETAs before it ships.
//!
//! The roster is the daemon's last work-finder tick (the `DaemonStatus`
//! round-trip `loom-daemon queue` renders), or `--roster PATH`, a saved
//! `loom-daemon queue --json` document. The two columns are labelled with each
//! config's `planner_version` stamp. The pure core is
//! [`loom_daemon::eta::planner_sim`]; this file only loads and renders.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use loom_daemon::eta::planner_sim::{
    preview, Estimators, EtaCell, Preview, PreviewRow, Regime, Roster, RosterRow,
};
use loom_daemon::eta::{Kind, Registry};
use loom_daemon::types::{DispatchPlanContext, ReadyQueueRow};

use super::eta_cmd::{load_history, resolve_repo, resolve_scope};

#[derive(clap::Args)]
pub(crate) struct EtaSimulateArgs {
    /// The proposed planner config: a `.loom/config.json`-shaped document
    /// (its `autonomous.workFinder` / `autonomous.mergeSequencing` blocks are
    /// what is read).
    #[arg(long, value_name = "PATH")]
    pub planner: PathBuf,

    /// Workspace whose current config, journals and history to use. Defaults
    /// to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Read the roster from a saved `loom-daemon queue --json` document
    /// instead of asking the running daemon.
    #[arg(long, value_name = "PATH")]
    pub roster: Option<PathBuf>,

    /// Which history to estimate from: `local`, `augment` or `fleet`.
    /// Defaults to `autonomous.eta.historyScope`.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,

    /// Emit the preview as JSON instead of the table.
    #[arg(long)]
    pub json: bool,
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {} as JSON", path.display()))
}

/// The roster a `loom-daemon queue --json` document describes, its rows'
/// workspace paths mapped through `slug`.
pub(crate) fn roster_from_queue_json(
    doc: &Value,
    mut slug: impl FnMut(&str) -> String,
) -> Result<Roster> {
    let Some(plan) = doc.get("plan").filter(|p| !p.is_null()) else {
        bail!("the roster has no dispatch plan (no work-finder tick yet, or a pre-#9288 daemon)");
    };
    let context: DispatchPlanContext =
        serde_json::from_value(plan.clone()).context("reading the roster's plan block")?;
    let at: DateTime<Utc> =
        serde_json::from_value(doc["tick_at"].clone()).context("reading the roster's tick_at")?;
    let rows: Vec<ReadyQueueRow> =
        serde_json::from_value(doc["queue"].clone()).context("reading the roster's queue rows")?;
    Ok(Roster {
        context,
        at,
        rows: rows
            .into_iter()
            .map(|row| RosterRow {
                repo: slug(&row.repo),
                issue: row.issue,
                plan: row.plan,
            })
            .collect(),
    })
}

/// `owner/repo` for a row's workspace path: `gh repo view` there when it is
/// a directory, else the value itself.
fn slug_resolver() -> impl FnMut(&str) -> String {
    let mut cache: HashMap<String, String> = HashMap::new();
    move |repo: &str| {
        cache
            .entry(repo.to_string())
            .or_insert_with(|| {
                let path = Path::new(repo);
                path.is_dir()
                    .then(|| resolve_repo(path))
                    .flatten()
                    .unwrap_or_else(|| repo.to_string())
            })
            .clone()
    }
}

impl EtaSimulateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = super::eta_fleet_cmd::resolve_root(self.repo_root.clone());
        let now = Utc::now();
        let doc = match &self.roster {
            Some(path) => read_json(path)?,
            None => {
                let Some(report) = super::fleet_config_reload::fetch_status() else {
                    bail!("could not reach loom-daemon for the live roster; pass --roster PATH");
                };
                super::ready_queue_cmd::queue_json(&report, now)
            }
        };
        let roster = roster_from_queue_json(&doc, slug_resolver())?;

        let current_config = read_json(&root.join(".loom/config.json")).unwrap_or(Value::Null);
        let proposed_config = read_json(&self.planner)?;
        let version = env!("CARGO_PKG_VERSION");
        let before = Regime::of(version, &current_config);
        let after = Regime::of(version, &proposed_config);

        let config = loom_daemon::eta::config::read(&root);
        let registry = Registry::builtin();
        let estimators = Estimators {
            start: registry.current(Kind::Start, config.current(Kind::Start)),
            land: registry.current(Kind::Land, config.current(Kind::Land)),
        };
        let history = load_history(&root, resolve_scope(self.scope.as_deref(), &root)?);
        let result = preview(&roster, &before, &after, &estimators, &history, now);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            print!("{}", render(&result, &self.planner.display().to_string()));
        }
        Ok(())
    }
}

/// `1h05m`, `12m`, `45s`.
fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, 0) => format!("{secs}s"),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h{m:02}m"),
    }
}

fn cell(c: &EtaCell) -> String {
    match (c.p50_sec, c.no_estimate_reason) {
        (Some(p50), _) => duration(p50),
        (None, Some(reason)) => reason.to_string(),
        (None, None) => "-".to_string(),
    }
}

fn delta(before: &EtaCell, after: &EtaCell) -> String {
    match (before.p50_sec, after.p50_sec) {
        (Some(b), Some(a)) if a == b => "=".to_string(),
        (Some(b), Some(a)) if a < b => format!("-{}", duration(b - a)),
        (Some(b), Some(a)) => format!("+{}", duration(a - b)),
        _ => String::new(),
    }
}

fn render_row(row: &PreviewRow) -> String {
    let pos = row
        .before
        .position
        .map_or_else(|| "-".to_string(), |p| format!("#{p}"));
    let (b, a) = (&row.before, &row.after);
    format!(
        "  {:<24} {:>4}  {:>14} {:>14} {:>9}  {:>14} {:>14} {:>9}\n",
        format!("{}#{}", row.repo, row.issue),
        pos,
        cell(&b.start),
        cell(&a.start),
        delta(&b.start, &a.start),
        cell(&b.land),
        cell(&a.land),
        delta(&b.land, &a.land),
    )
}

/// The human-readable before/after table.
pub(crate) fn render(p: &Preview, planner: &str) -> String {
    let mut out = format!(
        "Planner preview as of {} (roster tick {})\n",
        p.as_of.format("%Y-%m-%d %H:%M:%SZ"),
        p.plan_at.format("%Y-%m-%d %H:%M:%SZ"),
    );
    out.push_str(&format!("  before: {} (current config)\n", p.before_version));
    out.push_str(&format!("  after:  {} ({planner})\n", p.after_version));
    out.push_str(&format!(
        "  simulated: {}\n",
        if p.simulated.is_empty() {
            "none (no modelled knob changed)".to_string()
        } else {
            p.simulated.join(", ")
        }
    ));
    if !p.unsimulated.is_empty() {
        out.push_str(&format!(
            "  NOT simulated (changed, shown unchanged): {}\n",
            p.unsimulated.join(", ")
        ));
    }
    out.push_str(&format!(
        "  p50 from now; start: {}, land: {}\n",
        p.start_heuristic, p.land_heuristic
    ));
    if p.rows.is_empty() {
        out.push_str("  (the roster has no ready rows)\n");
        return out;
    }
    out.push_str(&format!(
        "  {:<24} {:>4}  {:>14} {:>14} {:>9}  {:>14} {:>14} {:>9}\n",
        "ISSUE",
        "POS",
        "start before",
        "start after",
        "delta",
        "land before",
        "land after",
        "delta"
    ));
    for row in &p.rows {
        out.push_str(&render_row(row));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "eta_simulate_cmd_tests.rs"]
mod tests;
