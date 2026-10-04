//! `loom-daemon eta offline` (#10193): build the point-in-time dataset from
//! exported `eta.estimate` / `eta.outcome` lines, run the walk-forward
//! protocol, and score a heuristic's logged estimates against each fitted
//! model family (Kaplan–Meier reference, linear quantile regression) on
//! identical rows. See [`loom_daemon::eta::offline`].
//!
//! Input is JSONL, one [`LoggedLine`] per line:
//! `{"observed_at": RFC3339, "event": "eta.estimate"|"eta.outcome", "body": …}`
//! — the log store's `observed_timestamp`, event name and body. `--export`
//! writes every fold's training and validation rows (one JSON document per
//! fold) for model fitting outside the daemon.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

use loom_daemon::eta::offline::dataset::{Logged, LoggedLine};
use loom_daemon::eta::offline::evaluate::{
    self, Estimate, OfflineReport, PredictorReport, Protocol,
};

/// `loom-daemon eta offline`.
#[derive(clap::Args)]
pub(crate) struct EtaOfflineArgs {
    /// JSONL export of logged ETA records (`-` for stdin).
    #[arg(long, value_name = "PATH")]
    pub input: PathBuf,

    /// The evaluation instant `N` (RFC 3339). Required, so a report is
    /// reproducible from its inputs.
    #[arg(long, value_name = "RFC3339")]
    pub now: String,

    /// Seconds added to each record's `observed_at` to give the instant it
    /// became knowable (the store's arrival lag). Never negative: a negative
    /// margin would make a record knowable before it was logged, letting
    /// future data past the point-in-time checks.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(i64).range(0..))]
    pub margin_secs: i64,

    /// The heuristic whose logged estimates are the baseline.
    #[arg(long, default_value = "land-v2")]
    pub baseline: String,

    /// Training window, days.
    #[arg(long, default_value_t = 14)]
    pub train_days: i64,

    /// Selection folds (days).
    #[arg(long, default_value_t = 2)]
    pub selection_folds: u32,

    /// Reported folds (days), ending at `--now`.
    #[arg(long, default_value_t = 2)]
    pub reported_folds: u32,

    /// Issue-bootstrap resamples.
    #[arg(long, default_value_t = 1000)]
    pub resamples: usize,

    /// Write every fold's rows here as JSONL.
    #[arg(long, value_name = "PATH")]
    pub export: Option<PathBuf>,

    /// Emit the report as JSON instead of text.
    #[arg(long)]
    pub json: bool,
}

impl EtaOfflineArgs {
    pub(crate) fn run(self) -> Result<()> {
        let now: DateTime<Utc> = DateTime::parse_from_rfc3339(&self.now)
            .with_context(|| format!("invalid --now {:?}", self.now))?
            .with_timezone(&Utc);
        let reader: Box<dyn BufRead> = if self.input.as_os_str() == "-" {
            Box::new(BufReader::new(std::io::stdin()))
        } else {
            let file = std::fs::File::open(&self.input)
                .with_context(|| format!("opening {}", self.input.display()))?;
            Box::new(BufReader::new(file))
        };
        let mut lines = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: LoggedLine = serde_json::from_str(&line)
                .with_context(|| format!("line {}: not a logged ETA line", i + 1))?;
            lines.push(parsed);
        }
        let logged = Logged::ingest(&lines, Duration::seconds(self.margin_secs));
        let protocol = Protocol {
            now,
            train_days: self.train_days,
            selection_folds: self.selection_folds,
            reported_folds: self.reported_folds,
        };
        let (report, folds) =
            evaluate::run(&logged, &protocol, &self.baseline, &[], self.resamples)?;
        if let Some(path) = &self.export {
            let mut out = std::io::BufWriter::new(
                std::fs::File::create(path)
                    .with_context(|| format!("creating {}", path.display()))?,
            );
            for fold in &folds {
                serde_json::to_writer(&mut out, fold)?;
                out.write_all(b"\n")?;
            }
            out.flush()?;
        }
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            eprintln!(
                "[eta offline] ingested {} land estimates, {} resolutions; skipped {:?}",
                logged.estimates.len(),
                logged.events.len(),
                logged.skipped
            );
            print!("{}", render(&report));
        }
        Ok(())
    }
}

fn fmt_estimate(e: &Estimate) -> String {
    match (e.value, e.lo, e.hi) {
        (Some(v), Some(lo), Some(hi)) => format!("{v:.3} [{lo:.3}, {hi:.3}] (n={})", e.n),
        (Some(v), _, _) => format!("{v:.3} (n={})", e.n),
        _ => "-".to_string(),
    }
}

fn render_predictor(p: &PredictorReport) -> String {
    format!(
        "  {}\n    answer rate        {}\n    truncated pinball  {}\n    landed pinball     {}\n    landed MAE         {}\n    landed bias        {}\n    25-75 coverage     {}\n",
        p.id,
        p.answer_rate.map_or("-".to_string(), |r| format!("{r:.3}")),
        fmt_estimate(&p.truncated_pinball),
        fmt_estimate(&p.landed_pinball),
        fmt_estimate(&p.landed_mae),
        fmt_estimate(&p.landed_bias),
        fmt_estimate(&p.coverage),
    )
}

/// The human report.
fn render(r: &OfflineReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "eta offline: N={} train={}d selection={} reported={} (95% CIs resample issues)\n",
        r.protocol.now,
        r.protocol.train_days,
        r.protocol.selection_folds,
        r.protocol.reported_folds
    ));
    for f in &r.folds {
        out.push_str(&format!(
            "  fold {:?} cutoff={} observe_until={} train={} validate={}\n",
            f.fold.role, f.fold.cutoff, f.fold.observe_until, f.train_rows, f.validate_rows
        ));
    }
    for (id, score) in &r.selection {
        out.push_str(&format!(
            "  selection {id}: {}\n",
            score.map_or("-".to_string(), |s| format!("{s:.1}"))
        ));
    }
    out.push_str(&format!(
        "reported rows: {} scoreable, baseline {} answer rate {}\n",
        r.scoreable_rows,
        r.baseline_id,
        r.baseline_answer_rate
            .map_or("-".to_string(), |v| format!("{v:.3}"))
    ));
    for c in &r.comparisons {
        out.push_str(&format!(
            "frozen {} ({}): {} paired rows over {} issues\n",
            c.settings.id(),
            c.settings.family(),
            c.paired_rows,
            c.paired_issues
        ));
        out.push_str(&render_predictor(&c.baseline));
        out.push_str(&render_predictor(&c.model));
        out.push_str(&format!(
            "  delta (model - baseline) truncated pinball: {}\n",
            fmt_estimate(&c.delta_truncated_pinball)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::EtaOfflineArgs;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: EtaOfflineArgs,
    }

    fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
        let base = ["eta-offline", "--input", "-", "--now", "2026-10-01T00:00:00Z"];
        Cli::try_parse_from(base.iter().chain(extra))
    }

    #[test]
    fn margin_secs_defaults_to_120_and_accepts_zero() {
        assert_eq!(parse(&[]).unwrap().args.margin_secs, 120);
        assert_eq!(parse(&["--margin-secs", "0"]).unwrap().args.margin_secs, 0);
    }

    #[test]
    fn a_negative_margin_secs_is_rejected() {
        // A negative margin would move knowable-at before observed_at and let
        // records logged after a cutoff pass the point-in-time assertions.
        assert!(parse(&["--margin-secs", "-1"]).is_err());
        assert!(parse(&["--margin-secs=-3600"]).is_err());
    }
}
