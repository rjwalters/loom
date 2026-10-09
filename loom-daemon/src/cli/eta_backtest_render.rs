//! Text rendering for `loom-daemon eta backtest` (#9325), moved out of
//! `eta_cmd.rs` to keep it under the file-size budget (#10245).

use loom_daemon::eta::backtest::{
    adaptation, BacktestReport, Bucket, Comparison, RegimeAdaptation,
};

fn opt(value: Option<f64>, render: impl Fn(f64) -> String) -> String {
    value.map(render).unwrap_or_else(|| "-".to_string())
}

fn secs(value: Option<f64>) -> String {
    opt(value, |v| format!("{v:.1}"))
}

fn pct(value: Option<f64>) -> String {
    opt(value, |v| format!("{:.1}%", v * 100.0))
}

fn render_bucket(name: &str, b: &Bucket) -> String {
    format!(
        "  {name:<28} n={:<5} scored={:<5} refused={:<5} pinball={:>10} coverage={:>7} bias={:>10}\n",
        b.n,
        b.scored,
        b.refused,
        secs(b.mean_pinball_loss_sec),
        pct(b.coverage),
        secs(b.bias_sec),
    )
}

/// The human `eta backtest` report of one heuristic.
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
    if !r.by_subset.is_empty() {
        out.push_str("by subset (#10524):\n");
        for (name, sb) in &r.by_subset {
            out.push_str(&render_bucket(name, &sb.bucket));
            out.push_str(&format!(
                "  {:<28} late surprise={:>7} over {} case(s)\n",
                "",
                pct(sb.late_rate),
                sb.late_decided
            ));
        }
    }
    let s = &r.stability;
    out.push_str(&format!(
        "stability (predicted landing instant): steps={} median_shift={}s max_shift={}s\n",
        s.steps,
        secs(s.median_shift_sec),
        s.max_shift_sec
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
    ));
    if !r.convergence.is_empty() {
        out.push_str("convergence (by actual lead):\n");
        for (lead, c) in &r.convergence {
            out.push_str(&format!(
                "  {lead:<28} scored={:<5} p25-p75={:>10} with_p90={:<5} p25-p90={:>10}\n",
                c.scored,
                secs(c.median_p25_p75_sec),
                c.with_p90,
                secs(c.median_p25_p90_sec),
            ));
        }
    }
    if let Some(a) = &r.regime_adaptation {
        out.push_str(&render_adaptation(a));
    }
    out
}

fn hours(value: Option<i64>) -> String {
    value.map_or_else(|| "never".to_string(), |h| format!("{h}h"))
}

/// The `--adaptation` lines (#10528).
pub(super) fn render_adaptation(a: &RegimeAdaptation) -> String {
    format!(
        "regime adaptation (x{} shift injected at {} on {}, before={} after={} over {}h):\n  \
         t_p50={} (<= {}h) t_cov={} (<= {}h) t_alarm={} (<= {}h) false_alarm={} \
         pre_coverage={} meets_targets={}\n",
        a.shift_factor,
        a.shift_at.to_rfc3339(),
        a.stage.as_str(),
        a.n_before,
        a.n_after,
        a.horizon_h,
        hours(a.t_p50_h),
        adaptation::TARGET_T_P50_H,
        hours(a.t_cov_h),
        adaptation::TARGET_T_COV_H,
        hours(a.t_alarm_h),
        adaptation::TARGET_T_ALARM_H,
        hours(a.false_alarm_h),
        pct(a.pre_coverage),
        a.meets_targets(),
    )
}

/// The human `eta backtest --compare` report: both reports, then the paired
/// comparison on the union of cases with its walk-forward daily folds.
pub(super) fn render_comparison(c: &Comparison) -> String {
    let (a, b, p) = (&c.a.heuristic, &c.b.heuristic, &c.paired);
    let mut out = String::new();
    out.push_str(&render_report(&c.a));
    out.push_str(&render_report(&c.b));
    out.push_str(&format!("paired on the union of {} case(s):\n", p.cases));
    out.push_str(&format!(
        "  answer rate         {a}={} {b}={}\n",
        pct(p.a_answer_rate),
        pct(p.b_answer_rate)
    ));
    out.push_str(&format!(
        "  pinball4 (common)   {a}={} {b}={} over {} case(s)\n",
        secs(p.a_mean_pinball4_loss_sec),
        secs(p.b_mean_pinball4_loss_sec),
        p.loss4_pairs
    ));
    out.push_str(&format!(
        "  late surprise       {a}={} {b}={} over {} case(s)\n",
        pct(p.a_late_rate),
        pct(p.b_late_rate),
        p.late_pairs
    ));
    for (label, d) in [
        ("pinball", p.delta_pinball_loss_sec),
        ("pinball4", p.delta_pinball4_loss_sec),
    ] {
        if let Some(d) = d {
            out.push_str(&format!(
                "  {:<19} ({b} − {a})={} 95% issue-bootstrap CI [{}, {}] over {} case(s)\n",
                format!("delta {label}"),
                secs(d.value),
                secs(d.lo),
                secs(d.hi),
                d.n
            ));
        }
    }
    for f in &p.folds {
        out.push_str(&format!(
            "  fold {}  cases={:<5} common={:<5} {a}={:>10} {b}={:>10}\n",
            f.day,
            f.cases,
            f.loss4_pairs,
            secs(f.a_mean_pinball4_loss_sec),
            secs(f.b_mean_pinball4_loss_sec),
        ));
    }
    if !c.paired_by_subset.is_empty() {
        out.push_str("  paired by subset (#10508):\n");
        for (name, s) in &c.paired_by_subset {
            let delta = s.delta_pinball4_loss_sec.as_ref();
            out.push_str(&format!(
                "    {name:<18} cases={:<5} pinball4 {a}={} {b}={} delta={} CI [{}, {}] items={} late {a}={} {b}={} over {}\n",
                s.cases,
                secs(s.a_mean_pinball4_loss_sec),
                secs(s.b_mean_pinball4_loss_sec),
                secs(delta.and_then(|d| d.value)),
                secs(delta.and_then(|d| d.lo)),
                secs(delta.and_then(|d| d.hi)),
                s.delta4_items,
                pct(s.a_late_rate),
                pct(s.b_late_rate),
                s.late_pairs,
            ));
        }
    }
    let w = &p.day_wins;
    out.push_str(&format!(
        "  {b} won {}/{} decided day(s) ({} tied), 95% CI [{}, {}]\n",
        w.wins,
        w.days,
        w.ties,
        pct(w.ci_low),
        pct(w.ci_high)
    ));
    out.push_str(&format!("better: {}\n", c.better.as_deref().unwrap_or("neither")));
    out
}

#[cfg(test)]
mod tests {
    use super::render_adaptation;
    use chrono::{TimeZone, Utc};
    use loom_daemon::eta::backtest::RegimeAdaptation;
    use loom_daemon::eta::Stage;

    #[test]
    fn adaptation_lines_name_every_figure_and_its_target() {
        let a = RegimeAdaptation {
            stage: Stage::ReviewWait,
            shift_at: Utc.with_ymd_and_hms(2026, 10, 3, 0, 0, 0).unwrap(),
            shift_factor: 2.0,
            n_before: 40,
            n_after: 30,
            horizon_h: 24,
            t_p50_h: Some(3),
            t_cov_h: Some(9),
            t_alarm_h: None,
            false_alarm_h: None,
            pre_coverage: Some(0.5),
        };
        let text = render_adaptation(&a);
        for part in [
            "x2 shift",
            "review_wait",
            "t_p50=3h (<= 6h)",
            "t_cov=9h (<= 12h)",
            "t_alarm=never (<= 3h)",
            "false_alarm=never",
            "pre_coverage=50.0%",
            "meets_targets=false",
        ] {
            assert!(text.contains(part), "{part:?} in {text}");
        }
    }
}
