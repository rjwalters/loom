//! `loom-daemon eta fit` (#10245): fit the `eta-fit/v1` coefficient file from
//! the cached fleet snapshots at a cutoff and write it to
//! `.loom/state/eta/fit/fit-<T>.json`.
//!
//! The same runner as the daemon's daily refit
//! ([`loom_daemon::eta::fit::run`]), so a file written here and one written by
//! the daemon at the same cutoff from the same snapshots are byte-identical
//! (on one build). No forge call: the snapshots are whatever
//! `eta fleet backfill|refresh` last cached.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use loom_daemon::eta::fit::run::{self, FitReport};

#[derive(clap::Args)]
pub(crate) struct EtaFitArgs {
    /// The cutoff `T`, RFC 3339. Defaults to today 00:00Z (UTC).
    #[arg(long, value_name = "RFC3339")]
    pub as_of: Option<String>,

    /// Directory whose `.loom/state/eta/fleet/` to read and whose
    /// `.loom/state/eta/fit/` to write. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Write the file to this path instead (and prune nothing).
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,

    /// Build the rows and fit, but write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Print the fit report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl EtaFitArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = super::eta_fleet_cmd::resolve_root(self.repo_root.clone());
        let as_of = parse_as_of(self.as_of.as_deref(), Utc::now())?;
        let report = run::fit_and_write(
            &root,
            as_of,
            self.out.as_deref(),
            self.dry_run,
            &run::current_fitter(),
        )?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", render(&report));
        }
        Ok(())
    }
}

/// `--as-of`, or today 00:00Z.
fn parse_as_of(raw: Option<&str>, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match raw {
        Some(raw) => Ok(DateTime::parse_from_rfc3339(raw)
            .with_context(|| format!("--as-of {raw:?} is not an RFC 3339 instant"))?
            .with_timezone(&Utc)),
        None => Ok(run::midnight(now)),
    }
}

fn render(report: &FitReport) -> String {
    let mut out = String::new();
    let verb = if report.written {
        "wrote"
    } else {
        "dry run, not written:"
    };
    let _ = writeln!(out, "eta fit {verb} {}", report.path.display());
    let _ = writeln!(out, "  id:           {}", report.id);
    let _ = writeln!(out, "  as_of:        {}", report.as_of.to_rfc3339());
    let _ = writeln!(out, "  data_through: {}", report.data_through.to_rfc3339());
    for (stage, s) in &report.stages {
        let _ = writeln!(
            out,
            "  {:<12} rows={:<6} exits={:<5} merge_events={:<5} hazard={} aft={}",
            stage.as_str(),
            s.rows,
            s.exits,
            s.merge_events,
            if s.hazard { "fitted" } else { "skipped" },
            if s.aft { "in" } else { "out" },
        );
    }
    let _ = writeln!(
        out,
        "  dwells={} snapshots={} dropped: missing={} no_flags={} pruned={}",
        report.dwells,
        report.snapshots,
        report.rows_dropped_missing,
        report.rows_dropped_no_flags,
        report.pruned
    );
    if report.rows_dropped_no_flags > 0 {
        let _ = writeln!(
            out,
            "  note: rows were dropped for want of a flag timeline; \
             re-run `loom-daemon eta fleet backfill` (not `refresh`) for each repo"
        );
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use loom_daemon::eta::fit::{coeffs, fit_dir};
    use loom_daemon::eta::fleet::{self, FleetSnapshot};
    use loom_daemon::pr_latency::history::{PrEvent, PrHistory, PrState};

    fn t(hours: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::hours(hours)
    }

    fn snapshot_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let events = vec![
            PrEvent::Labeled {
                label: "loom:review-requested".into(),
                at: t(1),
            },
            PrEvent::Unlabeled {
                label: "loom:review-requested".into(),
                at: t(3),
            },
            PrEvent::Labeled {
                label: "loom:pr".into(),
                at: t(3),
            },
            PrEvent::Merged { at: t(5) },
        ];
        let pr = PrHistory::new(1, t(0), PrState::Merged, Some(t(5)), Vec::new(), events, true);
        let mut snapshot = FleetSnapshot::empty("rjwalters/loom");
        snapshot.merge(&[pr], t(24));
        fleet::write(&fleet::snapshot_path(dir.path(), "rjwalters/loom"), &snapshot).unwrap();
        dir
    }

    #[test]
    fn as_of_defaults_to_today_midnight_utc() {
        let now = DateTime::parse_from_rfc3339("2026-10-04T17:45:12Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parse_as_of(None, now).unwrap().to_rfc3339(), "2026-10-04T00:00:00+00:00");
        assert_eq!(
            parse_as_of(Some("2026-10-01T06:00:00+02:00"), now).unwrap(),
            DateTime::parse_from_rfc3339("2026-10-01T04:00:00Z").unwrap()
        );
        assert!(parse_as_of(Some("yesterday"), now).is_err());
    }

    #[test]
    fn dry_run_writes_nothing_and_a_run_writes_the_same_bytes_twice() {
        let root = snapshot_root();
        let args = |dry_run: bool| EtaFitArgs {
            as_of: Some("2026-09-21T00:00:00Z".to_string()),
            repo_root: Some(root.path().to_path_buf()),
            out: None,
            dry_run,
            json: true,
        };
        args(true).run().unwrap();
        assert!(!fit_dir(root.path()).exists(), "a dry run writes nothing");

        args(false).run().unwrap();
        let path = fit_dir(root.path()).join(coeffs::path_for(t(24)));
        let first = std::fs::read(&path).unwrap();
        args(false).run().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first, "byte-identical on a repeat run");
    }

    #[test]
    fn render_names_every_stage_and_the_horizon() {
        let root = snapshot_root();
        let report = run::fit_and_write(
            root.path(),
            t(24),
            None,
            true,
            &coeffs::Fitter {
                version: "0.0.0".into(),
                revision: "x".into(),
            },
        )
        .unwrap();
        let text = render(&report);
        for stage in ["review_wait", "doctor_wait", "merge_wait", "merge_hold"] {
            assert!(text.contains(stage), "{text}");
        }
        assert!(text.contains("data_through: 2026-09-20T23:58:00+00:00"), "{text}");
        assert!(text.starts_with("eta fit dry run"), "{text}");
    }
}
