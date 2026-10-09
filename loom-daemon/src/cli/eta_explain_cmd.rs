//! `loom-daemon eta explain --file F [--diff F2] [--json]` (#10930, slice 1):
//! replay a logged estimate from its explanation, check parity with the
//! recorded quantiles, and explain it — the per-stage breakdown, each
//! input's marginal contribution, and with `--diff` which inputs moved the
//! p50 between two estimates.
//!
//! Read-only and offline: it reads an explanation export (one
//! `eta-explanation/v1` JSON object per line, the format
//! `fleet_agreement::parse_explanations` reads, or one pretty-printed
//! object) and runs anywhere. The pure core is
//! [`loom_daemon::eta::explain`]; this file only loads and renders.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use loom_daemon::eta::explain::{self, Diff, ExplainReport, Parity};
use loom_daemon::eta::explanation::Explanation;
use loom_daemon::eta::fleet_agreement::parse_explanations;
use loom_daemon::eta::simulate::run_explanation;

#[derive(clap::Args)]
pub(crate) struct EtaExplainArgs {
    /// An explanation export: one `eta-explanation/v1` JSON object per line,
    /// or a single JSON object.
    #[arg(long, value_name = "PATH")]
    pub file: PathBuf,

    /// A second export: explain what moved the p50 from `--file`'s estimate
    /// to this one's.
    #[arg(long, value_name = "PATH")]
    pub diff: Option<PathBuf>,

    /// Pick this estimate id out of `--file` (needed with `--diff` when the
    /// file holds more than one).
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,

    /// Pick this estimate id out of `--diff`.
    #[arg(long, value_name = "ID")]
    pub diff_id: Option<String>,

    /// Emit JSON instead of the text report.
    #[arg(long)]
    pub json: bool,
}

/// The explanations in `text`: a single JSON object, else JSONL. The second
/// value counts unparseable lines.
pub(crate) fn parse(text: &str) -> (Vec<Explanation>, usize) {
    match serde_json::from_str::<Explanation>(text) {
        Ok(one) => (vec![one], 0),
        Err(_) => parse_explanations(text),
    }
}

fn load(path: &Path) -> Result<Vec<Explanation>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (out, skipped) = parse(&text);
    if skipped > 0 {
        eprintln!(
            "[eta explain] skipped {skipped} line(s) of {} that are not eta-explanation/v1",
            path.display()
        );
    }
    if out.is_empty() {
        bail!("{} holds no eta-explanation/v1 record", path.display());
    }
    Ok(out)
}

/// The one explanation `id` names, or the only one there is.
fn pick(all: Vec<Explanation>, id: Option<&str>, what: &str) -> Result<Explanation> {
    if let Some(id) = id {
        return all
            .into_iter()
            .find(|e| e.estimate_id == id)
            .with_context(|| format!("{what} holds no estimate {id}"));
    }
    let n = all.len();
    let mut all = all.into_iter();
    match (all.next(), n) {
        (Some(one), 1) => Ok(one),
        _ => bail!("{what} holds {n} estimates; name one with an id flag"),
    }
}

impl EtaExplainArgs {
    pub(crate) fn run(self) -> Result<()> {
        let a = load(&self.file)?;
        if let Some(path) = &self.diff {
            let a = pick(a, self.id.as_deref(), "--file")?;
            let b = pick(load(path)?, self.diff_id.as_deref(), "--diff")?;
            let Some(d) = explain::diff(&a, &b) else {
                let culprit = if run_explanation(&a).is_none() {
                    &a
                } else {
                    &b
                };
                bail!("cannot diff: estimate {} does not replay", culprit.estimate_id);
            };
            if self.json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print!("{}", render_diff(&d));
            }
            return Ok(());
        }
        let picked = match self.id.as_deref() {
            Some(_) => vec![pick(a, self.id.as_deref(), "--file")?],
            None => a,
        };
        let reports: Vec<ExplainReport> = picked.iter().map(explain::report).collect();
        if self.json {
            let value = match reports.as_slice() {
                [one] => serde_json::to_value(one)?,
                many => serde_json::to_value(many)?,
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            for (i, r) in reports.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                print!("{}", render_report(r));
            }
        }
        let mismatched = reports
            .iter()
            .filter(|r| r.parity == Parity::Mismatch)
            .count();
        if mismatched > 0 {
            bail!("{mismatched} estimate(s) did not replay to their recorded quantiles");
        }
        Ok(())
    }
}

fn quad(q: Option<[i64; 4]>) -> String {
    q.map_or_else(|| "-".to_string(), |[a, b, c, d]| format!("{a}s / {b}s / {c}s / {d}s"))
}

fn signed(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_string(), |v| format!("{v:+}s"))
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// The text report.
pub(crate) fn render_report(r: &ExplainReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "estimate {} {} {} {} as_of {}",
        r.estimate_id,
        r.heuristic,
        r.kind,
        r.subject,
        r.as_of.to_rfc3339()
    );
    let parity = match &r.parity {
        Parity::Exact => "exact".to_string(),
        Parity::Mismatch => "MISMATCH".to_string(),
        Parity::NotReplayable { reason } => format!("not replayable ({reason})"),
        Parity::NoEstimate => "no estimate".to_string(),
    };
    let _ = writeln!(out, "engine   {}", r.engine);
    let _ = writeln!(out, "recorded p25/p50/p75/p90 {}", quad(r.recorded));
    let _ = writeln!(out, "replayed p25/p50/p75/p90 {}  parity: {parity}", quad(r.replayed));
    if !r.truncated.is_empty() {
        let _ = writeln!(out, "truncated {}", r.truncated.join(", "));
    }
    if let Some(source) = &r.breakdown_source {
        let _ = writeln!(out, "\nstages ({source}):");
        for s in &r.stages {
            let _ = writeln!(
                out,
                "  {:<16} entry_p50 {:>9}  dwell_p50 {:>9}  share {:>6}  reach {:>4}",
                s.stage.as_str(),
                s.entry_p50_sec.map_or("-".to_string(), |v| format!("{v}s")),
                s.dwell_p50_sec.map_or("-".to_string(), |v| format!("{v}s")),
                s.p50_share.map_or("-".to_string(), |v| format!("{v:.3}")),
                s.reach_pct.map_or("-".to_string(), |v| format!("{v}%")),
            );
        }
    }
    if !r.inputs.is_empty() {
        let _ = writeln!(out, "\ninputs (one at a time, ranked by |Δp50|):");
        let _ = writeln!(
            out,
            "  {:<44} {:>12} {:>12} {:>10} {:>10}",
            "input", "value", "perturbed", "Δp50", "Δp90"
        );
        for m in &r.inputs {
            let _ = writeln!(
                out,
                "  {:<44} {:>12} {:>12} {:>10} {:>10}",
                m.input.name,
                num(m.input.value),
                num(m.input.perturbed),
                signed(m.dp50_sec),
                signed(m.dp90_sec),
            );
        }
    }
    if !r.context.is_empty() {
        let _ = writeln!(out, "\nrecorded context (not read by the replay):");
        for (name, value) in &r.context {
            let _ = writeln!(out, "  {name} = {value}");
        }
    }
    out
}

/// The text diff.
pub(crate) fn render_diff(d: &Diff) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "diff {} -> {}", d.a, d.b);
    let _ = writeln!(
        out,
        "p50 {}s -> {}s ({:+}s)   p90 {}s -> {}s ({:+}s)",
        d.a_p50_sec,
        d.b_p50_sec,
        d.b_p50_sec - d.a_p50_sec,
        d.a_p90_sec,
        d.b_p90_sec,
        d.b_p90_sec - d.a_p90_sec
    );
    if !d.same_engine {
        let _ = writeln!(out, "the two replay through different engines");
    }
    if d.swaps.is_empty() {
        let _ = writeln!(out, "\nno replay input changed");
    } else {
        let _ = writeln!(out, "\nchanged inputs (each swapped into a alone, ranked by |Δp50|):");
        for s in &d.swaps {
            let _ = writeln!(
                out,
                "  {:<44} {:>12} -> {:<12} Δp50 {:>10}  Δp90 {:>10}",
                s.name,
                num(s.from),
                num(s.to),
                signed(s.dp50_sec),
                signed(s.dp90_sec),
            );
        }
    }
    let _ = writeln!(
        out,
        "residual (interactions, history, seed): Δp50 {:+}s  Δp90 {:+}s",
        d.residual_p50_sec, d.residual_p90_sec
    );
    if !d.only_in_a.is_empty() {
        let _ = writeln!(out, "only in a: {}", d.only_in_a.join(", "));
    }
    if !d.only_in_b.is_empty() {
        let _ = writeln!(out, "only in b: {}", d.only_in_b.join(", "));
    }
    if !d.context_changed.is_empty() {
        let _ = writeln!(out, "\nrecorded context changed (not read by the replay):");
        for c in &d.context_changed {
            let _ = writeln!(out, "  {} {} -> {}", c.name, c.from, c.to);
        }
    }
    out
}

#[cfg(test)]
#[path = "eta_explain_cmd_tests.rs"]
mod tests;
