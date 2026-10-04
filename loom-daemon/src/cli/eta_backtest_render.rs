//! Text rendering for `loom-daemon eta backtest` (#9325), moved out of
//! `eta_cmd.rs` to keep it under the file-size budget (#10245).

use loom_daemon::eta::backtest::{BacktestReport, Bucket, Comparison};

fn render_bucket(name: &str, b: &Bucket) -> String {
    format!(
        "  {name:<28} n={:<5} scored={:<5} refused={:<5} pinball={:>10} coverage={:>7} bias={:>10}\n",
        b.n,
        b.scored,
        b.refused,
        b.mean_pinball_loss_sec
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_string()),
        b.coverage
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "-".to_string()),
        b.bias_sec
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_string()),
    )
}

pub(super) fn render_report(r: &BacktestReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("ETA backtest: {} ({})\n", r.heuristic, r.kind));
    out.push_str(&render_bucket("overall", &r.overall));
    if !r.by_repo.is_empty() {
        out.push_str("by repo:\n");
        for (repo, b) in &r.by_repo {
            out.push_str(&render_bucket(repo, b));
        }
    }
    if !r.by_horizon.is_empty() {
        out.push_str("by horizon:\n");
        for (h, b) in &r.by_horizon {
            out.push_str(&render_bucket(h, b));
        }
    }
    out
}

pub(super) fn render_comparison(c: &Comparison) -> String {
    let mut out = String::new();
    out.push_str(&render_report(&c.a));
    out.push_str(&render_report(&c.b));
    out.push_str(&format!("better: {}\n", c.better.as_deref().unwrap_or("tie / neither scored")));
    out
}
