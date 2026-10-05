//! `loom-daemon eta fleet signoz …` — the SigNoz in-sweep half of fleet ETA
//! history (#9758).
//!
//! - `refresh` — walk the fleet's exported `sweep.outcome` records for a repo
//!   and publish its SigNoz snapshot. Reads either the telemetry store's
//!   ClickHouse HTTP endpoint (`--endpoint`, with `--credential-file` naming
//!   an owner-only password file — never the secret itself) or a
//!   `JSONEachRow` export of the same query (`--from-file`).
//! - `show` — what is cached: id, window, per-host and per-stage counts.
//! - `query` — print the query, for an operator running it by hand with
//!   `clickhouse-client --param_repo=… --format JSONEachRow`.
//!
//! The daemon runs the same refresh on its fleet refresh cadence when
//! `autonomous.eta.fleetRefresh.signoz.enabled` is set.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

use loom_daemon::eta::fleet_signoz::{self, SignozSnapshot};
use loom_daemon::eta::fleet_signoz_refresh::{
    self, ClickhouseHttp, FetchReport, FileRows, Limits, SignozRead, SignozStop, OUTCOMES_SQL,
};

use super::eta_fleet_cmd::resolve_root;

#[derive(clap::Subcommand)]
pub(crate) enum SignozCommand {
    /// Re-read a repo's fleet `sweep.outcome` records and publish its SigNoz
    /// snapshot (only when every page was read).
    Refresh(SignozRefreshArgs),
    /// Print the cached SigNoz snapshot's census.
    Show(SignozShowArgs),
    /// Print the outcomes query.
    Query,
}

impl SignozCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            SignozCommand::Refresh(args) => args.run(),
            SignozCommand::Show(args) => args.run(),
            SignozCommand::Query => {
                print!("{OUTCOMES_SQL}");
                Ok(())
            }
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct SignozRefreshArgs {
    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory whose `.loom/state/eta/fleet/signoz/` to write. Defaults to
    /// the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Read a `JSONEachRow` export of `eta fleet signoz query` instead of a
    /// live endpoint.
    #[arg(long, value_name = "PATH", conflicts_with = "endpoint")]
    pub from_file: Option<PathBuf>,

    /// ClickHouse HTTP endpoint of the telemetry store. Defaults to
    /// `autonomous.eta.fleetRefresh.signoz.endpoint`.
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,

    /// ClickHouse user. Defaults to the configured one.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// Owner-only file holding the user's password. Defaults to the
    /// configured `credentialFile`.
    #[arg(long, value_name = "PATH")]
    pub credential_file: Option<PathBuf>,

    /// The instant the snapshot describes. Defaults to now; pin it to make two
    /// hosts' snapshots directly comparable.
    #[arg(long, value_name = "RFC3339")]
    pub as_of: Option<String>,

    /// Build and print, but do not write the cache file.
    #[arg(long)]
    pub dry_run: bool,

    /// Print the resulting snapshot as JSON.
    #[arg(long)]
    pub json: bool,
}

impl SignozRefreshArgs {
    fn run(self) -> Result<()> {
        let root = resolve_root(self.repo_root.clone());
        let repo = match &self.repo {
            Some(r) => r.clone(),
            None => super::eta_cmd::resolve_repo(&root)
                .ok_or_else(|| anyhow::anyhow!("could not resolve owner/repo; pass --repo"))?,
        };
        let as_of = match &self.as_of {
            Some(raw) => DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| anyhow::anyhow!("invalid --as-of {raw:?}: {e}"))?,
            None => Utc::now(),
        };
        let configured = loom_daemon::eta::config::read(&root).fleet_refresh.signoz;
        let limits = Limits {
            page_size: configured.page_size,
            max_pages: configured.max_pages,
        };
        let mut reader: Box<dyn SignozRead> = match &self.from_file {
            Some(path) => Box::new(FileRows::read(path).map_err(anyhow::Error::msg)?),
            None => {
                let Some(endpoint) = self.endpoint.clone().or(configured.endpoint) else {
                    bail!(
                        "no SigNoz source: pass --from-file or --endpoint, or configure \
                         autonomous.eta.fleetRefresh.signoz.endpoint"
                    );
                };
                Box::new(ClickhouseHttp {
                    endpoint,
                    user: self.user.clone().or(configured.user),
                    credential_file: self.credential_file.clone().or(configured.credential_file),
                    timeout: std::time::Duration::from_secs(60),
                })
            }
        };
        let path = fleet_signoz::signoz_path(&root, &repo);
        let (report, snapshot) = if self.dry_run {
            match fleet_signoz_refresh::fetch(&repo, reader.as_mut(), as_of, limits) {
                Ok((snapshot, report)) => (report, Some(snapshot)),
                Err(report) => (*report, None),
            }
        } else {
            let report =
                fleet_signoz_refresh::refresh(&root, &repo, reader.as_mut(), as_of, limits);
            let snapshot = report.promoted.then(|| fleet_signoz::read(&path)).flatten();
            (report, snapshot)
        };
        println!("{}", summary(&report));
        if report.stop != SignozStop::Complete {
            eprintln!(
                "[eta fleet signoz] {}: {}; the cached snapshot (if any) is unchanged",
                report.stop.as_str(),
                report.detail.as_deref().unwrap_or("")
            );
            std::process::exit(1);
        }
        if report.promoted {
            println!("[eta fleet signoz] wrote {}", path.display());
        }
        if self.json || self.dry_run {
            if let Some(snapshot) = &snapshot {
                println!("{}", serde_json::to_string_pretty(snapshot)?);
            }
        }
        Ok(())
    }
}

fn summary(report: &FetchReport) -> String {
    let rejected: Vec<String> = report
        .rejected
        .iter()
        .map(|(reason, n)| format!("{reason}={n}"))
        .collect();
    format!(
        "[eta fleet signoz] {}: {} — pages={} rows={} outcomes={} rejected=[{}] \
         duplicate_records={} duplicate_sweeps={} snapshot={}",
        report.repo,
        report.stop.as_str(),
        report.pages,
        report.rows,
        report.outcomes,
        rejected.join(" "),
        report.stats.duplicate_records,
        report.stats.duplicate_sweeps,
        report.snapshot_id.as_deref().unwrap_or("-"),
    )
}

#[derive(clap::Args)]
pub(crate) struct SignozShowArgs {
    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory whose `.loom/state/eta/fleet/signoz/` to read.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the whole snapshot as JSON.
    #[arg(long)]
    pub json: bool,
}

impl SignozShowArgs {
    fn run(self) -> Result<()> {
        let root = resolve_root(self.repo_root.clone());
        let repo = match &self.repo {
            Some(r) => r.clone(),
            None => super::eta_cmd::resolve_repo(&root)
                .ok_or_else(|| anyhow::anyhow!("could not resolve owner/repo; pass --repo"))?,
        };
        let path = fleet_signoz::signoz_path(&root, &repo);
        let Some(snapshot) = fleet_signoz::read(&path) else {
            bail!(
                "no SigNoz snapshot for {repo} at {} — run `loom-daemon eta fleet signoz refresh --repo {repo}`",
                path.display()
            );
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        } else {
            print!("{}", render(&snapshot, &path));
        }
        Ok(())
    }
}

fn render(snapshot: &SignozSnapshot, path: &Path) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "eta fleet signoz snapshot {} ({})", snapshot.repo, path.display());
    let _ = writeln!(out, "  id:       {}", snapshot.snapshot_id);
    let _ = writeln!(out, "  since:    {}", snapshot.since.to_rfc3339());
    let _ = writeln!(out, "  as_of:    {}", snapshot.as_of.to_rfc3339());
    let _ = writeln!(out, "  outcomes: {}", snapshot.outcomes.len());
    for (host, n) in snapshot.counts_by_host() {
        let _ = writeln!(out, "    host {host:<24} n={n}");
    }
    for (stage, n) in snapshot.counts_by_stage() {
        let _ = writeln!(out, "    {stage:<14} n={n}");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn render_names_the_id_every_host_and_every_stage() {
        let rows = "{\"record_id\":\"r1\",\"host_id\":\"worker-1\",\"repo\":\"acme/app\",\
            \"sweep_id\":\"s1\",\"result\":\"success\",\"total_duration_sec\":\"100\",\
            \"phase_durations\":\"[{\\\"phase\\\":\\\"curator\\\",\\\"duration_sec\\\":30},\
            {\\\"phase\\\":\\\"builder\\\",\\\"duration_sec\\\":70}]\",\
            \"event_time_ns\":\"1790000000000000000\",\"knowable_time_ns\":\"1790000000000000000\"}";
        let mut reader = FileRows::parse(rows).unwrap();
        let as_of = DateTime::from_timestamp(1_790_000_100, 0).unwrap();
        let limits = Limits {
            page_size: 10,
            max_pages: 5,
        };
        let (snapshot, report) =
            fleet_signoz_refresh::fetch("acme/app", &mut reader, as_of, limits).unwrap();
        let text = render(&snapshot, Path::new("/tmp/x.json"));
        assert!(text.contains(&snapshot.snapshot_id), "{text}");
        assert!(text.contains("host worker-1"), "{text}");
        assert!(text.contains("sweep.curator"), "{text}");
        assert!(text.contains("sweep.builder"), "{text}");
        assert!(summary(&report).contains("outcomes=1"));
    }
}
